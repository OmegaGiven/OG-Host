//! `og-hostd serve`: the long-running node daemon.
//!
//! - REST API on a root-only unix socket (`/run/og-host/api.sock`), optionally also on TCP with
//!   a bearer token (`/var/lib/og-host/api-token`, created on first start, mode 0600).
//! - Supervisor: every 2 s, bring containers towards their desired state using their restart
//!   policy (always / unless-stopped / on-failure[:N]), with exponential backoff. Containers
//!   whose processes vanished (host reboot) are rebuilt and restarted when their policy says so.
//! - Health monitor: runs each container's healthcheck on its interval; results in health.json.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::containers::{Extra, NetChoice, Node};

pub struct Daemon {
    pub node: Node,
    pub root: PathBuf,
    pub token: Option<String>,
    restarts: Mutex<HashMap<String, Backoff>>,
    health: Mutex<HashMap<String, Health>>,
}

#[derive(Debug, Clone)]
struct Backoff {
    attempts: u32,
    next: Instant,
}

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub failing_streak: u32,
    pub last_output: String,
    pub checked_at: u64,
    #[serde(skip)]
    next: Option<Instant>,
    #[serde(skip)]
    started: Option<Instant>,
}

type App = Arc<Daemon>;

struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}
type ApiResult<T> = std::result::Result<T, ApiError>;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_or_create_token(root: &Path) -> Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let p = root.join("api-token");
    if let Ok(t) = std::fs::read_to_string(&p) {
        return Ok(t.trim().to_string());
    }
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p)?.write_all(t.as_bytes())?;
    Ok(t)
}

/// Serve the Docker Engine API on a unix socket (optionally scoped to one label).
fn spawn_docker_socket(app: App, path: PathBuf, scope: Option<crate::dockerapi::Scope>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    listener.set_nonblocking(true)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let router = crate::dockerapi::router(crate::dockerapi::Ctx { d: app, scope: scope.clone() });
    let svc = tower::ServiceExt::map_request(router, crate::dockerapi::strip_version);
    eprintln!("og-hostd: Docker API on {}{}", path.display(), scope.map(|s| format!(" (scope {}: label {}={})", s.name, s.key, s.value)).unwrap_or_default());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, tower::make::Shared::new(svc)).await {
            eprintln!("og-hostd: docker socket stopped: {e}");
        }
    });
    Ok(())
}

pub async fn serve(node: Node, root: PathBuf, listen: Option<String>, docker_scopes: Vec<String>) -> Result<()> {
    let token = if listen.is_some() { Some(load_or_create_token(&root)?) } else { None };
    let app: App = Arc::new(Daemon { node, root: root.clone(), token, restarts: Mutex::new(HashMap::new()), health: Mutex::new(HashMap::new()) });

    // supervisor + health loop
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let a = app.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    a.supervise();
                    a.check_health();
                })
                .await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    let router = Router::new()
        .route("/v1/version", get(version))
        .route("/v1/node", get(node_info))
        .route("/v1/containers", get(list).post(create))
        .route("/v1/containers/{id}", get(inspect).delete(remove))
        .route("/v1/containers/{id}/start", post(start))
        .route("/v1/containers/{id}/stop", post(stop))
        .route("/v1/containers/{id}/logs", get(logs))
        .route("/v1/stacks/up", post(stack_up))
        .route("/v1/stacks/down", post(stack_down))
        .with_state(app.clone());

    // unix socket, root-only
    let sock = PathBuf::from("/run/og-host/api.sock");
    std::fs::create_dir_all(sock.parent().unwrap())?;
    let _ = std::fs::remove_file(&sock);
    let unix = tokio::net::UnixListener::bind(&sock).context("binding api socket")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("og-hostd: API on {}", sock.display());
    spawn_docker_socket(app.clone(), PathBuf::from("/run/og-host/docker.sock"), None)?;
    for spec in docker_scopes {
        let (name, label) = spec.split_once(':').context("--docker-scope wants name:label=value")?;
        let (key, value) = label.split_once('=').unwrap_or((label, ""));
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            anyhow::bail!("bad scope name {name:?}");
        }
        let scope = crate::dockerapi::Scope { name: name.into(), key: key.into(), value: value.into() };
        spawn_docker_socket(app.clone(), PathBuf::from(format!("/run/og-host/docker-{name}.sock")), Some(scope))?;
    }
    let unix_router = router.clone();
    let unix_task = tokio::spawn(async move { axum::serve(unix, unix_router).await });

    if let Some(addr) = listen {
        let tcp = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
        eprintln!("og-hostd: API on http://{addr} (bearer token in {})", root.join("api-token").display());
        let authed = router.layer(axum::middleware::from_fn_with_state(app.clone(), require_token));
        axum::serve(tcp, authed).await?;
    }
    unix_task.await??;
    Ok(())
}

async fn require_token(State(app): State<App>, headers: HeaderMap, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let want = app.token.as_deref().unwrap_or("");
    let got = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    // constant-time compare
    let ok = !want.is_empty() && got.len() == want.len() && got.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if !ok {
        return ApiError(StatusCode::UNAUTHORIZED, "missing or wrong bearer token".into()).into_response();
    }
    next.run(req).await
}

impl Daemon {
    /// Restart policy enforcement.
    fn supervise(&self) {
        let Ok(list) = self.node.list() else { return };
        for (m, st) in list {
            if m.auto_remove && matches!(st.state.as_str(), "exited" | "dead" | "failed") {
                let _ = self.node.remove(&m.id);
                continue;
            }
            let wants_running = m.desired.is_empty() || m.desired == "running";
            if !wants_running || st.state == "running" || st.state == "created" {
                continue;
            }
            let policy = m.restart.clone().unwrap_or_else(|| "no".into());
            let max_retries: Option<u32> = policy.strip_prefix("on-failure:").and_then(|n| n.parse().ok());
            let should = match policy.as_str() {
                "always" | "unless-stopped" => true,
                p if p.starts_with("on-failure") => st.exit_code != Some(0) || st.state == "dead",
                _ => false,
            };
            if !should {
                continue;
            }
            let mut restarts = self.restarts.lock().unwrap();
            let b = restarts.entry(m.id.clone()).or_insert(Backoff { attempts: 0, next: Instant::now() });
            // a run longer than 30 s resets the backoff
            if let (Some(s), Some(f)) = (st.started_at, st.finished_at) {
                if f.saturating_sub(s) > 30 {
                    b.attempts = 0;
                }
            }
            if Instant::now() < b.next || max_retries.is_some_and(|n| b.attempts >= n) {
                continue;
            }
            b.attempts += 1;
            b.next = Instant::now() + Duration::from_secs(1u64 << b.attempts.min(6)).min(Duration::from_secs(60));
            let attempt = b.attempts;
            drop(restarts);
            match self.node.start(&m.id) {
                Ok(()) => eprintln!("og-hostd: restarted {} (policy {policy}, attempt {attempt})", m.name),
                Err(e) => eprintln!("og-hostd: restarting {} failed: {e:#}", m.name),
            }
        }
    }

    fn check_health(&self) {
        let Ok(list) = self.node.list() else { return };
        for (m, st) in list {
            let Some(hc) = m.healthcheck.as_ref().filter(|h| !h.disabled && !h.test.is_empty()) else { continue };
            let mut health = self.health.lock().unwrap();
            if st.state != "running" {
                health.remove(&m.id);
                continue;
            }
            let h = health.entry(m.id.clone()).or_insert(Health {
                status: "starting",
                failing_streak: 0,
                last_output: String::new(),
                checked_at: 0,
                next: Some(Instant::now() + Duration::from_secs(1)),
                started: Some(Instant::now()),
            });
            if h.next.is_some_and(|n| Instant::now() < n) {
                continue;
            }
            h.next = Some(Instant::now() + Duration::from_secs(hc.interval_s.clamp(1, 3600)));
            let in_start_period = h.started.is_some_and(|s| s.elapsed().as_secs() < hc.start_period_s);
            drop(health);
            let argv: Vec<String> = match hc.test.first().map(String::as_str) {
                Some("CMD-SHELL") => vec!["/bin/sh".into(), "-c".into(), hc.test[1..].join(" ")],
                Some("CMD") => hc.test[1..].to_vec(),
                _ => vec!["/bin/sh".into(), "-c".into(), hc.test.join(" ")],
            };
            let res = self.node.exec(&m.id, &argv, self.node.process_user(&m.id), hc.timeout_s.clamp(1, 300));
            let mut health = self.health.lock().unwrap();
            let Some(h) = health.get_mut(&m.id) else { continue };
            h.checked_at = now();
            match res {
                Ok((0, out)) => {
                    h.status = "healthy";
                    h.failing_streak = 0;
                    h.last_output = out.chars().take(500).collect();
                }
                // failures during start_period don't count (Docker semantics)
                _ if in_start_period => {}
                Ok((code, out)) => {
                    h.failing_streak += 1;
                    h.last_output = format!("exit {code}: {}", out.chars().take(500).collect::<String>());
                    if h.failing_streak >= hc.retries.max(1) {
                        h.status = "unhealthy";
                    }
                }
                Err(e) => {
                    h.failing_streak += 1;
                    h.last_output = format!("{e:#}");
                    if h.failing_streak >= hc.retries.max(1) {
                        h.status = "unhealthy";
                    }
                }
            }
            let _ = std::fs::write(self.root.join("containers").join(&m.id).join("health.json"), serde_json::to_vec(&*h).unwrap_or_default());
        }
    }

    fn container_json(&self, id: &str) -> Option<serde_json::Value> {
        let (m, st) = self.node.list().ok()?.into_iter().find(|(m, _)| m.id == id)?;
        let health = self.health.lock().unwrap().get(id).cloned();
        Some(json!({
            "id": m.id, "name": m.name, "image": m.image, "state": st.state, "exit_code": st.exit_code,
            "pid": st.pid, "error": st.error, "created_at": m.created_at, "project": m.project, "service": m.service,
            "restart": m.restart, "desired": m.desired, "network_mode": m.network_mode,
            "ip": m.net.as_ref().and_then(|a| a.ip), "ports": m.net.as_ref().map(|a| a.ports.clone()).unwrap_or_default(),
            "health": health,
        }))
    }
}

async fn version() -> Json<serde_json::Value> {
    Json(json!({ "name": "og-hostd", "version": env!("CARGO_PKG_VERSION"), "api": "v1" }))
}

async fn node_info(State(app): State<App>) -> ApiResult<Json<serde_json::Value>> {
    let list = app.node.list()?;
    let running = list.iter().filter(|(_, s)| s.state == "running").count();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |k: &str| meminfo.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    Ok(Json(json!({
        "containers": list.len(), "running": running,
        "mem_total": kb("MemTotal:"), "mem_available": kb("MemAvailable:"),
        "load": load.split_whitespace().take(3).collect::<Vec<_>>(),
        "networks": app.node.net.list().map_err(anyhow::Error::from)?.iter().map(|n| json!({"name": n.name, "bridge": n.bridge, "subnet": n.subnet.to_string()})).collect::<Vec<_>>(),
    })))
}

async fn list(State(app): State<App>) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let ids: Vec<String> = app.node.list()?.into_iter().map(|(m, _)| m.id).collect();
    Ok(Json(ids.iter().filter_map(|id| app.container_json(id)).collect()))
}

async fn inspect(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<serde_json::Value>> {
    let m = app.node.find(&id)?;
    app.container_json(&m.id).map(Json).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such container".into()))
}

#[derive(Deserialize)]
struct CreateReq {
    image: String,
    name: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    volumes: Vec<String>,
    network: Option<String>,
    command: Option<Vec<String>>,
    memory: Option<String>,
    restart: Option<String>,
}

async fn create(State(app): State<App>, Json(req): Json<CreateReq>) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let digest = crate::ensure_pulled(&app.root, &app.node.store, &req.image, true).await?;
    let ports = req.ports.iter().map(|p| og_net::PortMap::parse(p)).collect::<std::result::Result<Vec<_>, _>>().map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
    let binds = req.volumes.iter().map(|v| crate::containers::parse_bind(v)).collect::<Result<Vec<_>>>()?;
    let memory = req.memory.as_deref().map(crate::containers::parse_size).transpose()?;
    let choice = NetChoice::parse(req.network.as_deref().unwrap_or("default"));
    let a = app.clone();
    let meta = tokio::task::spawn_blocking(move || {
        a.node.run(&req.image, &digest, req.name.as_deref(), choice, ports, Extra { restart: req.restart.clone(), ..Default::default() }, |o| {
            o.env = req.env;
            o.mounts = binds;
            o.command = req.command;
            o.memory_max = memory;
        })
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok((StatusCode::CREATED, Json(app.container_json(&meta.id).unwrap_or(json!({"id": meta.id})))))
}

async fn start(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<serde_json::Value>> {
    let m = app.node.find(&id)?;
    let a = app.clone();
    let mid = m.id.clone();
    tokio::task::spawn_blocking(move || a.node.start(&mid)).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    app.restarts.lock().unwrap().remove(&m.id);
    Ok(Json(app.container_json(&m.id).unwrap_or_default()))
}

async fn stop(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<serde_json::Value>> {
    let m = app.node.find(&id)?;
    let a = app.clone();
    let mid = m.id.clone();
    tokio::task::spawn_blocking(move || a.node.stop(&mid, 10)).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(app.container_json(&m.id).unwrap_or_default()))
}

async fn remove(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult<StatusCode> {
    let m = app.node.find(&id)?;
    app.node.remove(&m.id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LogsQuery {
    tail: Option<usize>,
}

async fn logs(State(app): State<App>, UrlPath(id): UrlPath<String>, Query(q): Query<LogsQuery>) -> ApiResult<String> {
    let m = app.node.find(&id)?;
    let text = String::from_utf8_lossy(&std::fs::read(app.node.log_path(&m.id)).unwrap_or_default()).into_owned();
    Ok(match q.tail {
        Some(n) => {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(n)..].join("\n")
        }
        None => text,
    })
}

#[derive(Deserialize)]
struct StackReq {
    file: PathBuf,
    project: Option<String>,
    #[serde(default)]
    volumes: bool,
}

async fn stack_up(State(app): State<App>, Json(req): Json<StackReq>) -> ApiResult<Json<serde_json::Value>> {
    let env: og_compose::interp::Vars = std::env::vars().collect();
    let p = og_compose::load(&req.file, req.project.as_deref(), &env).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
    crate::compose::up(&app.node, &app.root, &app.node.store, &p, false).await?;
    Ok(Json(json!({ "project": p.name, "services": p.services.len(), "warnings": p.warnings })))
}

async fn stack_down(State(app): State<App>, Json(req): Json<StackReq>) -> ApiResult<Json<serde_json::Value>> {
    let env: og_compose::interp::Vars = std::env::vars().collect();
    let p = og_compose::load(&req.file, req.project.as_deref(), &env).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
    let a = app.clone();
    let name = p.name.clone();
    tokio::task::spawn_blocking(move || crate::compose::down(&a.node, &a.root, &p, req.volumes)).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({ "project": name, "down": true })))
}
