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
use axum::http::StatusCode;
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
    pub brain: Mutex<crate::brain::Brain>,
    pub hub: crate::stats::Hub,
    pub sessions: crate::auth::Sessions,
    pub access: og_access::Access,
    /// The lab control plane, when enabled (--lab).
    pub lab: Option<Arc<og_lab::Lab>>,
    log_shipper: crate::labnode::LogShipper,
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

#[allow(clippy::too_many_arguments)]
pub async fn serve(node: Node, root: PathBuf, listen: Option<String>, docker_scopes: Vec<String>, scope_labels: Vec<String>, tls: Option<(String, crate::tls::CertFiles)>, lab: bool) -> Result<()> {
    let lab = if lab { Some(og_lab::Lab::open(&root.join("lab")).await.context("starting the lab control plane")?) } else { None };
    let token = if listen.is_some() || tls.is_some() { Some(load_or_create_token(&root)?) } else { None };
    let app: App = Arc::new(Daemon { node, root: root.clone(), token, restarts: Mutex::new(HashMap::new()), health: Mutex::new(HashMap::new()), brain: Mutex::new(Default::default()), hub: Default::default(), sessions: crate::auth::Sessions::load(&root), access: og_access::Access::open(&root), lab: lab.clone(), log_shipper: Default::default() });
    if let Some(l) = &lab {
        l.set_local(Arc::new(crate::labnode::LabNode { d: app.clone() }));
        eprintln!("og-hostd: lab control plane on (data in {})", root.join("lab").display());
    }

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

    // brain: resource policy every 5 s
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let a = app.clone();
                let _ = tokio::task::spawn_blocking(move || a.think()).await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    // live stats for the UI every 2 s
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let a = app.clone();
                let _ = tokio::task::spawn_blocking(move || a.sample()).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    let router = Router::new()
        .route("/v1/version", get(version))
        .route("/v1/info", get(info))
        .route("/v1/events", get(events))
        .route("/v1/metrics", get(metrics))
        .route("/v1/containers/{id}/restart", post(restart))
        .route("/v1/containers/{id}/kill", post(kill))
        .route("/v1/containers/{id}/logs/stream", get(logs_stream))
        .route("/v1/stacks", get(stacks_list))
        .route("/v1/stacks/{name}", get(stack_get).put(stack_put).delete(stack_delete))
        .route("/v1/stacks/{name}/{action}", post(stack_action))
        .route("/v1/ui/login", post(crate::auth::login))
        .route("/v1/ui/tfa", post(crate::auth::login_tfa))
        .route("/v1/ui/logout", post(crate::auth::logout))
        .route("/v1/ui/me", get(crate::auth::me))
        .route("/v1/brain", get(brain_status))
        .route("/v1/containers/{id}/wake", post(start))
        .route("/v1/containers/{id}/resources", get(resources_get).post(resources_set))
        .route("/v1/node", get(node_info))
        .route("/v1/containers", get(list).post(create))
        .route("/v1/containers/{id}", get(inspect).delete(remove))
        .route("/v1/containers/{id}/start", post(start))
        .route("/v1/containers/{id}/stop", post(stop))
        .route("/v1/containers/{id}/logs", get(logs))
        .route("/v1/stacks/up", post(stack_up))
        .route("/v1/stacks/down", post(stack_down))
        .with_state(app.clone())
        .merge(crate::access_api::router(app.clone()))
        // the Docker Engine API too (images, networks, volumes, exec, ...), for the UI
        .nest("/v1/docker", crate::dockerapi::router(crate::dockerapi::Ctx { d: app.clone(), scope: None }));
    // lab API (OmegaEnv paths): who's calling goes into the request for its handlers; root on
    // the unix socket is admin
    let (router, unix_router) = match &lab {
        Some(l) => (
            router.clone().merge(og_lab::http::router(l.clone())),
            router.merge(og_lab::http::router(l.clone()).layer(axum::Extension(og_lab::Caller::Admin))),
        ),
        None => (router.clone(), router),
    };
    let router = router.fallback(crate::ui::asset);
    let unix_router = unix_router.fallback(crate::ui::asset).layer(axum::Extension(og_access::Who::System));

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
        let mut labels = Vec::new();
        for sl in &scope_labels {
            let (n, kv) = sl.split_once(':').context("--scope-label wants name:key=value")?;
            let (k, v) = kv.split_once('=').context("--scope-label wants name:key=value")?;
            if !k.starts_with("og.") {
                anyhow::bail!("--scope-label {sl:?}: only og.* labels");
            }
            if n == name {
                labels.push((k.to_string(), v.to_string()));
            }
        }
        let scope = crate::dockerapi::Scope { name: name.into(), key: key.into(), value: value.into(), labels };
        spawn_docker_socket(app.clone(), PathBuf::from(format!("/run/og-host/docker-{name}.sock")), Some(scope))?;
    }

    let unix_task = tokio::spawn(async move { axum::serve(unix, unix_router).await });

    // gzip JSON responses (event streams and pre-compressed assets are left alone)
    let authed = router.layer(axum::middleware::from_fn_with_state(app.clone(), crate::auth::guard)).layer(tower_http::compression::CompressionLayer::new());
    if let Some((addr, files)) = tls {
        let r = authed.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::tls::serve(&addr, files, r).await {
                eprintln!("og-hostd: https stopped: {e:#}");
            }
        });
    }
    if let Some(addr) = listen {
        let tcp = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
        eprintln!("og-hostd: web UI and API on http://{addr} (bearer token in {}; UI accounts: og-hostd passwd <user>)", root.join("api-token").display());
        axum::serve(tcp, authed.into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    }
    unix_task.await??;
    Ok(())
}

impl Daemon {
    /// Restart policy enforcement.
    /// Stats sample for the UI; also republishes the container list when it changed.
    fn sample(&self) {
        let list = self.node.list().unwrap_or_default();
        let running: Vec<(String, String, Option<String>)> = list
            .iter()
            .filter(|(_, st)| st.state == "running")
            .map(|(m, _)| (m.id.clone(), m.name.clone(), m.net.as_ref().and_then(|a| a.host_veth.clone())))
            .collect();
        let snap = self.hub.sample(&running);
        let gpus = self.brain.lock().unwrap().last_gpus.clone();
        self.hub.publish("stats", json!({ "stats": snap, "gpus": gpus }).to_string());
        let containers: Vec<serde_json::Value> = list.iter().filter_map(|(m, _)| self.container_json(&m.id)).collect();
        self.hub.publish_if_changed("containers", serde_json::to_string(&containers).unwrap_or_default());
        if let Some(lab) = &self.lab {
            if let Some(host) = lab.local_name() {
                self.log_shipper.ship(self, lab, &host);
            }
        }
    }

    fn think(&self) {
        use crate::brain;
        let node = brain::read_node();
        let cs = brain::snapshot(&self.node);
        let gpus = brain::read_gpus();
        let labels: HashMap<String, std::collections::BTreeMap<String, String>> =
            self.node.list().unwrap_or_default().into_iter().map(|(m, _)| (m.id, m.labels)).collect();
        brain::apply_class(&cs, &labels);
        let actions = {
            let mut b = self.brain.lock().unwrap();
            let a = b.decide(&node, &cs, &gpus, Instant::now());
            b.last_node = node;
            b.last_containers = cs;
            b.last_gpus = gpus;
            a
        };
        brain::apply(&self.node, &self.brain, actions);
        self.hub.publish_if_changed("brain", brain_json(self).to_string());
    }

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

    pub(crate) fn container_json(&self, id: &str) -> Option<serde_json::Value> {
        let (m, st) = self.node.list().ok()?.into_iter().find(|(m, _)| m.id == id)?;
        let health = self.health.lock().unwrap().get(id).cloned();
        Some(json!({
            "id": m.id, "name": m.name, "image": m.image, "state": st.state, "exit_code": st.exit_code,
            "pid": st.pid, "error": st.error, "created_at": m.created_at, "project": m.project, "service": m.service,
            "restart": m.restart, "desired": m.desired, "network_mode": m.network_mode,
            "ip": m.net.as_ref().and_then(|a| a.ip), "ports": m.net.as_ref().map(|a| a.ports.clone()).unwrap_or_default(),
            "health": health, "started_at": st.started_at, "finished_at": st.finished_at, "oom_killed": st.oom_killed,
            "labels": m.labels, "class": crate::brain::Class::from_labels(&m.labels),
            "desktop": crate::desktop::is_desktop(&m.labels), "gpus": m.gpus, "minimized": m.saved_resources.is_some(),
        }))
    }
}

async fn resources_get(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<crate::resources::Resources>> {
    Ok(Json(crate::resources::current(&app.node, &app.node.find(&id)?.id)?))
}

/// Body: a `Resources` change, or `{"preset": "minimal" | "restore"}`.
async fn resources_set(State(app): State<App>, UrlPath(id): UrlPath<String>, Json(b): Json<serde_json::Value>) -> ApiResult<Json<crate::resources::Resources>> {
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<crate::resources::Resources> {
        let m = app.node.find(&id)?;
        match b.get("preset").and_then(|p| p.as_str()) {
            Some(p) => crate::resources::preset(&app.node, &m.id, p),
            None => crate::resources::update(&app.node, &m.id, &serde_json::from_value(b)?),
        }
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(r))
}

async fn brain_status(State(app): State<App>) -> Json<serde_json::Value> {
    Json(brain_json(&app))
}

fn brain_json(app: &Daemon) -> serde_json::Value {
    let b = app.brain.lock().unwrap();
    let n = &b.last_node;
    let containers: Vec<_> = b
        .last_containers
        .iter()
        .map(|c| {
            json!({
                "id": c.id, "name": c.name, "class": c.class, "gpu_class": c.gpu_class,
                "running": c.running, "desired": c.desired,
                "memory": { "current": c.mem_current, "max": c.mem_max, "low": b.low(&c.id), "squeezed_high": b.squeezed_high(&c.id), "psi": c.psi },
                "preempted": b.is_preempted(&c.id),
            })
        })
        .collect();
    let sessions: u32 = b.last_gpus.iter().map(|g| g.encoder_sessions).sum();
    json!({
        "node": { "mem_total": n.mem_total, "mem_available": n.mem_available, "psi_memory": n.psi, "psi_memory_host_services": n.host_psi },
        "containers": containers,
        "gpus": b.last_gpus,
        "nvenc": { "sessions": sessions, "limit": crate::brain::NVENC_LIMIT },
        "log": b.log.iter().rev().take(50).map(|(t, m)| json!({ "at": t, "message": m })).collect::<Vec<_>>(),
    })
}

// ---- UI support ---------------------------------------------------------------------------

async fn info(State(app): State<App>) -> Json<serde_json::Value> {
    let mut v = crate::stats::node_info(&app.root);
    v["lab"] = json!(app.lab.is_some());
    v["lab_host"] = json!(app.lab.as_ref().and_then(|l| l.local_name()));
    Json(v)
}

fn sse_event(name: &str, data: &str) -> axum::response::sse::Event {
    axum::response::sse::Event::default().event(name).data(data)
}

/// Server-Sent Events: node info, recent history and the latest of everything first, then
/// `stats` every 2 s and `containers` / `brain` whenever they change.
async fn events(State(app): State<App>) -> axum::response::sse::Sse<impl futures_util::Stream<Item = std::result::Result<axum::response::sse::Event, std::convert::Infallible>>> {
    let rx = app.hub.tx.subscribe();
    let mut first = vec![sse_event("info", &crate::stats::node_info(&app.root).to_string())];
    {
        let h = app.hub.history.lock().unwrap();
        first.push(sse_event("history", &json!({ "node": h.node(3600), "containers": h.containers() }).to_string()));
    }
    for (k, v) in app.hub.latest.read().unwrap().iter() {
        first.push(sse_event(k, v));
    }
    use futures_util::StreamExt;
    let live = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok((name, data)) => return Some((Ok(sse_event(name, &data)), rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });
    let stream = futures_util::stream::iter(first.into_iter().map(Ok)).chain(live);
    axum::response::sse::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

#[derive(Deserialize)]
struct MetricsQ {
    range: Option<u64>,
}

/// Node history, column-wise: {t: [...], cpu: [...], mem, rx, tx, dr, dw}.
async fn metrics(State(app): State<App>, Query(q): Query<MetricsQ>) -> Json<serde_json::Value> {
    let pts = app.hub.history.lock().unwrap().node(q.range.unwrap_or(3600).clamp(60, 86400));
    Json(json!({
        "t": pts.iter().map(|p| p.t).collect::<Vec<_>>(), "cpu": pts.iter().map(|p| p.cpu).collect::<Vec<_>>(),
        "mem": pts.iter().map(|p| p.mem).collect::<Vec<_>>(), "rx": pts.iter().map(|p| p.rx).collect::<Vec<_>>(),
        "tx": pts.iter().map(|p| p.tx).collect::<Vec<_>>(), "dr": pts.iter().map(|p| p.dr).collect::<Vec<_>>(),
        "dw": pts.iter().map(|p| p.dw).collect::<Vec<_>>(),
    }))
}

async fn restart(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<serde_json::Value>> {
    let m = app.node.find(&id)?;
    let a = app.clone();
    let mid = m.id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        a.node.stop(&mid, 10)?;
        a.node.start(&mid)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    app.restarts.lock().unwrap().remove(&m.id);
    Ok(Json(app.container_json(&m.id).unwrap_or_default()))
}

async fn kill(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult<StatusCode> {
    let m = app.node.find(&id)?;
    let _ = app.node.set_desired(&m.id, "stopped");
    app.node.kill(&m.id, libc::SIGKILL)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct StreamQ {
    tail: Option<usize>,
}

/// A container's log: the last `tail` lines (default 300), then new output as it comes.
async fn logs_stream(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<StreamQ>,
) -> ApiResult<axum::response::sse::Sse<impl futures_util::Stream<Item = std::result::Result<axum::response::sse::Event, std::convert::Infallible>>>> {
    let m = app.node.find(&id)?;
    let path = app.node.log_path(&m.id);
    let text = std::fs::read(&path).unwrap_or_default();
    let offset = text.len() as u64;
    let tail = q.tail.unwrap_or(300).min(5000);
    let text = String::from_utf8_lossy(&text);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(tail);
    let first = lines[start..].join("\n");
    let stream = futures_util::stream::unfold((Some(first), offset, path), |(first, mut offset, path)| async move {
        if let Some(f) = first {
            return Some((Ok(sse_event("log", &serde_json::to_string(&f).unwrap_or_default())), (None, offset, path)));
        }
        loop {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if len < offset {
                offset = 0; // truncated
            }
            if len > offset {
                use std::io::{Read, Seek, SeekFrom};
                let mut buf = Vec::new();
                if let Ok(mut f) = std::fs::File::open(&path) {
                    let _ = f.seek(SeekFrom::Start(offset));
                    let _ = f.take(1 << 20).read_to_end(&mut buf);
                }
                offset += buf.len() as u64;
                let chunk = String::from_utf8_lossy(&buf).into_owned();
                return Some((Ok(sse_event("log", &serde_json::to_string(&chunk).unwrap_or_default())), (None, offset, path)));
            }
        }
    });
    Ok(axum::response::sse::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

async fn stacks_list(State(app): State<App>) -> Json<serde_json::Value> {
    let a = app.clone();
    let v = tokio::task::spawn_blocking(move || crate::stacks::list(&a.node, &a.root)).await.unwrap_or_default();
    Json(json!(v))
}

async fn stack_get(State(app): State<App>, UrlPath(name): UrlPath<String>) -> ApiResult<Json<serde_json::Value>> {
    let (file, content) = crate::stacks::read_file(&app.node, &app.root, &name)?;
    Ok(Json(json!({ "name": name, "file": file, "content": content })))
}

#[derive(Deserialize)]
struct StackPut {
    content: String,
}

async fn stack_put(State(app): State<App>, UrlPath(name): UrlPath<String>, Json(b): Json<StackPut>) -> ApiResult<Json<serde_json::Value>> {
    let file = crate::stacks::write_file(&app.node, &app.root, &name, &b.content)?;
    Ok(Json(json!({ "name": name, "file": file })))
}

#[derive(Deserialize)]
struct StackDeleteQ {
    volumes: Option<bool>,
}

async fn stack_delete(State(app): State<App>, UrlPath(name): UrlPath<String>, Query(q): Query<StackDeleteQ>) -> ApiResult<StatusCode> {
    let p = crate::stacks::load(&app.node, &app.root, &name)?;
    let a = app.clone();
    let volumes = q.volumes.unwrap_or(false);
    tokio::task::spawn_blocking(move || -> Result<()> {
        crate::compose::down(&a.node, &a.root, &p, volumes)?;
        crate::stacks::delete_managed(&a.root, &p.name)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

/// up | down | restart | pull (fresh images, then up) | build (rebuild, then up).
async fn stack_action(State(app): State<App>, UrlPath((name, action)): UrlPath<(String, String)>) -> ApiResult<Json<serde_json::Value>> {
    let p = crate::stacks::load(&app.node, &app.root, &name)?;
    let warnings = p.warnings.clone();
    let t0 = std::time::Instant::now();
    match action.as_str() {
        "up" => crate::compose::up(&app.node, &app.root, &app.node.store, &p, false).await?,
        "build" => crate::compose::up(&app.node, &app.root, &app.node.store, &p, true).await?,
        "pull" => {
            for svc in p.services.iter().filter(|s| s.build.is_none()) {
                let Some(img) = &svc.image else { continue };
                if let Ok(r) = og_oci::Reference::parse(img) {
                    let _ = std::fs::remove_file(app.root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_")));
                }
                crate::ensure_pulled(&app.root, &app.node.store, img, true).await?;
            }
            crate::compose::up(&app.node, &app.root, &app.node.store, &p, false).await?
        }
        "down" | "restart" => {
            let a = app.clone();
            let p2 = p.clone();
            tokio::task::spawn_blocking(move || crate::compose::down(&a.node, &a.root, &p2, false)).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
            if action == "restart" {
                crate::compose::up(&app.node, &app.root, &app.node.store, &p, false).await?;
            }
        }
        _ => return Err(ApiError(StatusCode::NOT_FOUND, format!("unknown action {action:?} (up, down, restart, pull, build)"))),
    }
    Ok(Json(json!({ "name": p.name, "action": action, "seconds": t0.elapsed().as_secs_f64(), "warnings": warnings })))
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
