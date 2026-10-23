//! Docker Engine API (subset) on OG-Host, so docker / docker compose / Wolf / Pterodactyl Wings
//! can drive OG-Host unchanged.
//!
//! Served on `/run/og-host/docker.sock` (full access, root only) and on scoped sockets
//! (`/run/og-host/docker-<name>.sock`): a scoped client only sees and touches containers that
//! carry its label, every container it creates gets that label, and privileged containers are
//! refused. That's least privilege for tools that today get the all-powerful docker.sock.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, head, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::containers::{Extra, Meta, NetChoice, Status};
use crate::serve::Daemon;

pub const API_VERSION: &str = "1.47";

#[derive(Clone, Debug)]
pub struct Scope {
    pub name: String,
    pub key: String,
    pub value: String,
    /// og.* labels put on every container this scope creates (e.g. og.profile=desktop for Wolf).
    pub labels: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct Ctx {
    pub d: Arc<Daemon>,
    pub scope: Option<Scope>,
}

struct DErr(StatusCode, String);
impl IntoResponse for DErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "message": self.1 }))).into_response()
    }
}
impl From<anyhow::Error> for DErr {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{e:#}");
        let code = if msg.starts_with("no container") { StatusCode::NOT_FOUND } else { StatusCode::INTERNAL_SERVER_ERROR };
        DErr(code, msg)
    }
}
type R<T> = std::result::Result<T, DErr>;

fn not_found(what: &str) -> DErr {
    DErr(StatusCode::NOT_FOUND, format!("No such container: {what}"))
}

/// Strip a leading `/v1.xx` so every route works with and without a version prefix.
pub fn strip_version(mut req: Request<Body>) -> Request<Body> {
    if std::env::var_os("OG_HOST_DEBUG").is_some() {
        eprintln!("docker-api: {} {}", req.method(), req.uri());
    }
    let path = req.uri().path();
    if let Some(rest) = path.strip_prefix("/v") {
        if let Some(i) = rest.find('/') {
            if rest[..i].chars().all(|c| c.is_ascii_digit() || c == '.') {
                let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
                let new = pq[2 + i..].to_string();
                if let Ok(uri) = new.parse() {
                    *req.uri_mut() = uri;
                }
            }
        }
    }
    req
}

pub fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/_ping", get(ping).head(ping))
        .route("/version", get(version))
        .route("/info", get(info))
        .route("/events", get(events))
        .route("/containers/json", get(containers_list))
        .route("/containers/create", post(container_create))
        .route("/containers/{id}/json", get(container_inspect))
        .route("/containers/{id}/start", post(container_start))
        .route("/containers/{id}/stop", post(container_stop))
        .route("/containers/{id}/restart", post(container_restart))
        .route("/containers/{id}/kill", post(container_kill))
        .route("/containers/{id}/wait", post(container_wait))
        .route("/containers/{id}/attach", post(container_attach))
        .route("/containers/{id}/logs", get(container_logs))
        .route("/containers/{id}/stats", get(container_stats))
        .route("/containers/{id}/update", post(container_update))
        .route("/containers/{id}/exec", post(exec_create))
        .route("/exec/{id}/start", post(exec_start))
        .route("/exec/{id}/json", get(exec_inspect))
        .route("/exec/{id}/resize", post(exec_resize))
        .route("/containers/{id}/resize", post(exec_resize))
        .route("/containers/{id}", delete(container_delete))
        .route("/build", post(image_build))
        .route("/images/json", get(images_list))
        .route("/images/create", post(images_create))
        .route("/images/load", post(images_load).layer(axum::extract::DefaultBodyLimit::disable()))
        .route("/images/{*name}", get(image_inspect_or).delete(image_delete))
        .route("/networks", get(networks_list))
        .route("/networks/create", post(network_create))
        .route("/networks/{id}", get(network_inspect).delete(network_delete))
        .route("/volumes", get(volumes_list))
        .route("/volumes/create", post(volume_create))
        .route("/volumes/{name}", get(volume_inspect).delete(volume_delete))
        .route("/session", head(ping).post(ping))
        .fallback(|req: Request<Body>| async move { DErr(StatusCode::NOT_FOUND, format!("page not found: {} {}", req.method(), req.uri().path())) })
        .with_state(ctx)
}

fn with_api_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert("Api-Version", HeaderValue::from_static(API_VERSION));
    h.insert("OSType", HeaderValue::from_static("linux"));
    h.insert("Docker-Experimental", HeaderValue::from_static("false"));
    h.insert("Server", HeaderValue::from_static("OG-Host"));
    r
}

async fn ping() -> Response {
    with_api_headers((StatusCode::OK, [(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], "OK").into_response())
}

fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string()
}

async fn version() -> Response {
    with_api_headers(
        Json(json!({
            "Platform": {"Name": "OG-Host"},
            "Components": [{"Name": "OG-Host", "Version": env!("CARGO_PKG_VERSION"), "Details": {"ApiVersion": API_VERSION}}],
            "Version": format!("27.0.0-og-host-{}", env!("CARGO_PKG_VERSION")),
            "ApiVersion": API_VERSION, "MinAPIVersion": "1.24",
            "GitCommit": "og-host", "GoVersion": "n/a (Rust)", "Os": "linux",
            "Arch": og_oci::registry::host_platform().architecture, "KernelVersion": kernel(), "BuildTime": "",
        }))
        .into_response(),
    )
}

// ---------- helpers ----------

fn visible(ctx: &Ctx, m: &Meta) -> bool {
    match &ctx.scope {
        None => true,
        Some(s) => m.labels.get(&s.key).is_some_and(|v| s.value.is_empty() || *v == s.value),
    }
}

fn find(ctx: &Ctx, id: &str) -> R<Meta> {
    let id = id.trim_start_matches('/');
    let m = ctx.d.node.find(id).map_err(|_| not_found(id))?;
    if !visible(ctx, &m) {
        return Err(not_found(id)); // invisible to this scope, as if it didn't exist
    }
    Ok(m)
}

fn rfc3339(secs: u64) -> String {
    // civil-from-days (Howard Hinnant), enough for timestamps in API output
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn human_ago(t: u64) -> String {
    let s = now().saturating_sub(t);
    match s {
        0..=1 => "Less than a second".into(),
        2..=59 => format!("{s} seconds"),
        60..=119 => "About a minute".into(),
        120..=3599 => format!("{} minutes", s / 60),
        3600..=7199 => "About an hour".into(),
        7200..=86399 => format!("{} hours", s / 3600),
        _ => format!("{} days", s / 86400),
    }
}

fn docker_state(st: &Status) -> &'static str {
    match st.state.as_str() {
        "running" => "running",
        "created" | "" => "created",
        "dead" => "dead",
        _ => "exited",
    }
}

fn status_text(st: &Status, created: u64) -> String {
    match st.state.as_str() {
        "running" => format!("Up {}", human_ago(st.started_at.unwrap_or(created))),
        "created" | "" => "Created".into(),
        _ => format!("Exited ({}) {} ago", st.exit_code.unwrap_or(0), human_ago(st.finished_at.unwrap_or(created))),
    }
}

/// Bind mounts and named volumes (a bind of `<root>/volumes/<name>/_data` is volume `<name>`).
fn mounts_json(ctx: &Ctx, m: &Meta) -> Vec<Value> {
    let vols = ctx.d.root.join("volumes");
    m.binds
        .iter()
        .map(|(src, dst, ro)| {
            let name = src.strip_prefix(&vols).ok().and_then(|r| {
                let mut c = r.components();
                let n = c.next()?.as_os_str().to_string_lossy().to_string();
                (c.next()?.as_os_str() == "_data").then_some(n)
            });
            match name {
                Some(n) => json!({"Type": "volume", "Name": n, "Source": src, "Destination": dst, "Driver": "local", "Mode": "z", "RW": !ro, "Propagation": ""}),
                None => json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}),
            }
        })
        .chain(m.volumes.iter().map(|(dir, dst, ro)| {
            let n = dir.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            json!({"Type": "volume", "Name": n, "Source": dir.join("_data"), "Destination": dst, "Driver": "local", "Mode": "z", "RW": !ro, "Propagation": ""})
        }))
        .collect()
}

fn net_display(m: &Meta) -> String {
    match m.network_mode.as_str() {
        "default" => "bridge".into(),
        other => other.to_string(),
    }
}

fn ports_json(m: &Meta) -> Vec<Value> {
    m.net
        .as_ref()
        .map(|a| {
            a.ports
                .iter()
                .map(|p| {
                    json!({"IP": p.host_ip.map(|i| i.to_string()).unwrap_or_else(|| "0.0.0.0".into()), "PrivatePort": p.container_port,
                           "PublicPort": p.host_port, "Type": if matches!(p.proto, og_net::Proto::Udp) {"udp"} else {"tcp"}})
                })
                .collect()
        })
        .unwrap_or_default()
}

fn networks_json(m: &Meta) -> Value {
    let mut nets = serde_json::Map::new();
    if let Some(a) = &m.net {
        if let (Some(n), Some(ip)) = (&a.network, a.ip) {
            let name = if n == "default" { "bridge".to_string() } else { n.clone() };
            nets.insert(
                name,
                json!({"NetworkID": n, "EndpointID": a.host_veth, "Gateway": a.gateway, "IPAddress": ip,
                       "IPPrefixLen": a.prefix, "MacAddress": mac_for(ip), "Aliases": m.service.as_ref().map(|s| vec![s.clone()])}),
            );
        }
    }
    Value::Object(nets)
}

fn mac_for(ip: std::net::Ipv4Addr) -> String {
    let o = ip.octets();
    format!("02:42:{:02x}:{:02x}:{:02x}:{:02x}", o[0], o[1], o[2], o[3])
}

fn image_id(ctx: &Ctx, m: &Meta) -> String {
    ctx.d.node.store.read_sync(&m.manifest_digest).ok().and_then(|b| serde_json::from_slice::<og_oci::registry::Manifest>(&b).ok()).map(|mf| mf.config.digest).unwrap_or_else(|| m.manifest_digest.clone())
}

// ---------- system ----------

async fn info(State(ctx): State<Ctx>) -> R<Json<Value>> {
    let list = ctx.d.node.list()?;
    let mine: Vec<_> = list.iter().filter(|(m, _)| visible(&ctx, m)).collect();
    let running = mine.iter().filter(|(_, s)| s.state == "running").count();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mem_total = meminfo.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
    let images = list_images(&ctx).len();
    Ok(Json(json!({
        "ID": "og-host", "Containers": mine.len(), "ContainersRunning": running,
        "ContainersPaused": 0, "ContainersStopped": mine.len() - running, "Images": images,
        "Driver": "og-host", "DockerRootDir": ctx.d.root, "SystemTime": rfc3339(now()),
        "Name": std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim(), "ServerVersion": format!("27.0.0-og-host-{}", env!("CARGO_PKG_VERSION")),
        "OperatingSystem": "OG-Host", "OSType": "linux", "Architecture": std::env::consts::ARCH, "KernelVersion": kernel(),
        "NCPU": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), "MemTotal": mem_total,
        "CgroupDriver": "cgroupfs", "CgroupVersion": "2", "DefaultRuntime": "og-shim", "Runtimes": {"og-shim": {"path": "og-shim"}},
        "SecurityOptions": ["name=seccomp,profile=builtin", "name=userns", "name=cgroupns", "name=no-new-privileges"],
        "Labels": ctx.scope.as_ref().map(|s| vec![format!("og-host.scope={}", s.name)]).unwrap_or_default(),
        "Warnings": [],
    })))
}

/// /events: keep the stream open (clients subscribe while they work); events come later.
async fn events() -> Response {
    let stream = futures_util::stream::pending::<std::result::Result<axum::body::Bytes, std::io::Error>>();
    Response::builder().header(header::CONTENT_TYPE, "application/json").body(Body::from_stream(stream)).unwrap()
}

// ---------- containers ----------

#[derive(Deserialize)]
struct ListQ {
    all: Option<String>,
    filters: Option<String>,
}

fn truthy(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|x| x == "1" || x.eq_ignore_ascii_case("true"))
}

fn parse_filters(f: &Option<String>) -> BTreeMap<String, Vec<String>> {
    let Some(text) = f else { return BTreeMap::new() };
    let v: Value = serde_json::from_str(text).unwrap_or_default();
    let mut out = BTreeMap::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            // both {"label":["a=b"]} and {"label":{"a=b":true}} are used in the wild
            let items: Vec<String> = match val {
                Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                Value::Object(o) => o.keys().cloned().collect(),
                _ => vec![],
            };
            out.insert(k.clone(), items);
        }
    }
    out
}

fn container_matches(m: &Meta, st: &Status, filters: &BTreeMap<String, Vec<String>>) -> bool {
    for (k, vals) in filters {
        let ok = match k.as_str() {
            "label" => vals.iter().all(|l| match l.split_once('=') {
                Some((key, val)) => m.labels.get(key).is_some_and(|v| v == val),
                None => m.labels.contains_key(l),
            }),
            "name" => vals.iter().any(|n| m.name.contains(n.trim_start_matches('/'))),
            "id" => vals.iter().any(|i| m.id.starts_with(i)),
            "status" => vals.iter().any(|s| s == docker_state(st)),
            "ancestor" => vals.iter().any(|a| m.image == *a || m.image.starts_with(&format!("{a}:"))),
            "network" => vals.iter().any(|n| m.network_mode == *n || (n == "bridge" && m.network_mode == "default")),
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

async fn containers_list(State(ctx): State<Ctx>, Query(q): Query<ListQ>) -> R<Json<Vec<Value>>> {
    let all = truthy(&q.all);
    let filters = parse_filters(&q.filters);
    let mut out = Vec::new();
    for (m, st) in ctx.d.node.list()? {
        if !visible(&ctx, &m) || (!all && st.state != "running") || !container_matches(&m, &st, &filters) {
            continue;
        }
        out.push(json!({
            "Id": m.id, "Names": [format!("/{}", m.name)], "Image": m.image, "ImageID": image_id(&ctx, &m),
            "Command": m.args.join(" "), "Created": m.created_at, "Ports": ports_json(&m), "Labels": m.labels,
            "State": docker_state(&st), "Status": status_text(&st, m.created_at),
            "HostConfig": {"NetworkMode": net_display(&m)}, "NetworkSettings": {"Networks": networks_json(&m)}, "Mounts": mounts_json(&ctx, &m),
        }));
    }
    Ok(Json(out))
}

async fn container_inspect(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>) -> R<Json<Value>> {
    let m = find(&ctx, &id)?;
    let st = ctx.d.node.status(&m.id);
    let state = ctx.d.node.list()?.into_iter().find(|(x, _)| x.id == m.id).map(|(_, s)| s).unwrap_or(st);
    let health = std::fs::read(ctx.d.root.join("containers").join(&m.id).join("health.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let cfg: Value = std::fs::read(ctx.d.root.join("containers").join(&m.id).join("bundle/config.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let env = cfg["process"]["env"].clone();
    Ok(Json(json!({
        "Id": m.id, "Created": rfc3339(m.created_at), "Path": m.args.first(), "Args": m.args.iter().skip(1).collect::<Vec<_>>(),
        "State": {
            "Status": docker_state(&state), "Running": state.state == "running", "Paused": false, "Restarting": false,
            "OOMKilled": state.oom_killed, "Dead": state.state == "dead", "Pid": state.pid.unwrap_or(0), "ExitCode": state.exit_code.unwrap_or(0),
            "Error": state.error.clone().unwrap_or_default(),
            "StartedAt": state.started_at.map(rfc3339).unwrap_or_else(|| "0001-01-01T00:00:00Z".into()),
            "FinishedAt": state.finished_at.map(rfc3339).unwrap_or_else(|| "0001-01-01T00:00:00Z".into()),
            "Health": m.healthcheck.as_ref().filter(|h| !h.disabled).map(|_| match &health {
                Some(h) => json!({"Status": h["status"], "FailingStreak": h["failing_streak"],
                                  "Log": [{"Start": rfc3339(h["checked_at"].as_u64().unwrap_or(0)), "End": rfc3339(h["checked_at"].as_u64().unwrap_or(0)), "ExitCode": if h["status"] == "healthy" {0} else {1}, "Output": h["last_output"]}]}),
                None => json!({"Status": "starting", "FailingStreak": 0, "Log": []}),
            }),
        },
        "Image": image_id(&ctx, &m), "Name": format!("/{}", m.name), "RestartCount": 0, "Driver": "og-host", "Platform": "linux",
        "HostConfig": {
            "NetworkMode": net_display(&m), "AutoRemove": m.auto_remove,
            "RestartPolicy": {"Name": m.restart.clone().unwrap_or_else(|| "no".into()), "MaximumRetryCount": 0},
            "Binds": m.binds.iter().map(|(s, t, ro)| format!("{}:{}{}", s.display(), t.display(), if *ro {":ro"} else {""})).collect::<Vec<_>>(),
        },
        "Config": {
            "Hostname": cfg["hostname"], "Image": m.image, "Env": env, "Cmd": m.args, "Labels": m.labels,
            "WorkingDir": cfg["process"]["cwd"], "User": format!("{}:{}", cfg["process"]["user"]["uid"], cfg["process"]["user"]["gid"]),
            "Tty": false, "OpenStdin": false, "AttachStdout": false, "AttachStderr": false,
            "Healthcheck": m.healthcheck.as_ref().map(|h| json!({
                "Test": h.test, "Interval": h.interval_s * 1_000_000_000, "Timeout": h.timeout_s * 1_000_000_000,
                "Retries": h.retries, "StartPeriod": h.start_period_s * 1_000_000_000,
            })),
        },
        "NetworkSettings": {
            "IPAddress": m.net.as_ref().and_then(|a| a.ip).map(|i| i.to_string()).unwrap_or_default(),
            "Gateway": m.net.as_ref().and_then(|a| a.gateway).map(|i| i.to_string()).unwrap_or_default(),
            "Ports": m.net.as_ref().map(|a| {
                let mut o = serde_json::Map::new();
                for p in &a.ports {
                    let k = format!("{}/{}", p.container_port, if matches!(p.proto, og_net::Proto::Udp) {"udp"} else {"tcp"});
                    o.insert(k, json!([{"HostIp": p.host_ip.map(|i| i.to_string()).unwrap_or_else(|| "0.0.0.0".into()), "HostPort": p.host_port.to_string()}]));
                }
                Value::Object(o)
            }).unwrap_or(json!({})),
            "Networks": networks_json(&m),
        },
        "Mounts": mounts_json(&ctx, &m),
    })))
}

#[derive(Deserialize)]
struct NameQ {
    name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct CreateBody {
    image: String,
    #[serde(default)]
    cmd: Option<Vec<String>>,
    #[serde(default)]
    entrypoint: Option<Value>,
    #[serde(default)]
    env: Option<Vec<String>>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    tty: Option<bool>,
    #[serde(default)]
    host_config: Option<Value>,
    #[serde(default)]
    networking_config: Option<Value>,
    #[serde(default)]
    healthcheck: Option<Value>,
}

/// NanoCpus, or CpuQuota/CpuPeriod -> (quota, period) in microseconds.
fn cpu_quota_of(hc: &Value) -> Option<(i64, u64)> {
    if let Some(n) = hc["NanoCpus"].as_i64().filter(|n| *n > 0) {
        return Some((n / 10_000, 100_000)); // 1e9 nano-CPUs = one core = 100000/100000
    }
    let q = hc["CpuQuota"].as_i64().filter(|q| *q > 0)?;
    Some((q, hc["CpuPeriod"].as_u64().filter(|p| *p > 0).unwrap_or(100_000)))
}

/// BlkioDevice{Read,Write}{Bps,IOps}: [{Path, Rate}] -> one IoLimit per device.
fn io_limits_of(hc: &Value) -> Vec<og_runtime::IoLimit> {
    let mut by_dev: BTreeMap<String, og_runtime::IoLimit> = BTreeMap::new();
    for (key, which) in [("BlkioDeviceReadBps", 0), ("BlkioDeviceWriteBps", 1), ("BlkioDeviceReadIOps", 2), ("BlkioDeviceWriteIOps", 3)] {
        for d in hc[key].as_array().into_iter().flatten() {
            let (Some(path), Some(rate)) = (d["Path"].as_str(), d["Rate"].as_u64()) else { continue };
            let l = by_dev.entry(path.to_string()).or_insert_with(|| og_runtime::IoLimit { device: path.into(), read_bps: None, write_bps: None, read_iops: None, write_iops: None });
            match which {
                0 => l.read_bps = Some(rate),
                1 => l.write_bps = Some(rate),
                2 => l.read_iops = Some(rate),
                _ => l.write_iops = Some(rate),
            }
        }
    }
    by_dev.into_values().collect()
}

fn str_vec(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

async fn container_create(State(ctx): State<Ctx>, Query(q): Query<NameQ>, Json(b): Json<CreateBody>) -> R<(StatusCode, Json<Value>)> {
    let hc = b.host_config.clone().unwrap_or_default();
    // the image must already be local; clients pull on 404 and retry, like with Docker
    let r = og_oci::Reference::parse(&b.image).map_err(|e| DErr(StatusCode::BAD_REQUEST, e.to_string()))?;
    let ref_file = ctx.d.root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_"));
    let digest = std::fs::read_to_string(&ref_file).map_err(|_| DErr(StatusCode::NOT_FOUND, format!("No such image: {}", b.image)))?.trim().to_string();

    let mut warnings = Vec::new();
    if hc["Privileged"].as_bool() == Some(true) {
        if ctx.scope.is_some() {
            return Err(DErr(StatusCode::FORBIDDEN, "privileged containers are not allowed on this socket".into()));
        }
        warnings.push("privileged mode is not supported; the container runs with the default capability set".to_string());
    }
    let seccomp_unconfined = str_vec(&hc["SecurityOpt"]).iter().any(|o| crate::containers::is_seccomp_unconfined(o));
    if seccomp_unconfined && ctx.scope.is_some() {
        return Err(DErr(StatusCode::FORBIDDEN, "seccomp=unconfined is not allowed on this socket".into()));
    }
    if b.tty == Some(true) {
        warnings.push("TTY is not supported yet; output is captured as plain logs".to_string());
    }

    // network
    let mode = hc["NetworkMode"].as_str().unwrap_or("default").to_string();
    let endpoint = b.networking_config.as_ref().and_then(|n| n["EndpointsConfig"].as_object()).and_then(|o| o.keys().next().cloned());
    let aliases: Vec<String> = b
        .networking_config
        .as_ref()
        .and_then(|n| n["EndpointsConfig"].as_object())
        .map(|o| o.values().flat_map(|e| str_vec(&e["Aliases"])).collect())
        .unwrap_or_default();
    let choice = match mode.as_str() {
        "" | "default" | "bridge" => match endpoint.filter(|e| e != "bridge" && e != "default") {
            Some(e) => NetChoice::Network(e),
            None => NetChoice::Network("default".into()),
        },
        "host" => NetChoice::Host,
        "none" => NetChoice::None,
        m if m.starts_with("container:") => NetChoice::Container(m["container:".len()..].to_string()),
        other => NetChoice::Network(other.to_string()),
    };
    if let NetChoice::Container(other) = &choice {
        find(&ctx, other)?; // scoped clients can only share networks with their own containers
    }

    // ports
    let mut ports = Vec::new();
    if let Some(pb) = hc["PortBindings"].as_object() {
        for (k, binds) in pb {
            let (cport, proto) = k.split_once('/').unwrap_or((k.as_str(), "tcp"));
            for bnd in binds.as_array().into_iter().flatten() {
                let hp = bnd["HostPort"].as_str().unwrap_or("");
                if hp.is_empty() {
                    warnings.push(format!("port {k}: random host ports are not supported yet; not published"));
                    continue;
                }
                let ip = bnd["HostIp"].as_str().filter(|s| !s.is_empty() && *s != "0.0.0.0" && *s != "::").map(|s| format!("{s}:")).unwrap_or_default();
                ports.push(og_net::PortMap::parse(&format!("{ip}{hp}:{cport}/{proto}")).map_err(|e| DErr(StatusCode::BAD_REQUEST, e.to_string()))?);
            }
        }
    }

    // mounts
    let mut binds = Vec::new();
    let mut volumes = Vec::new();
    let mut tmpfs = Vec::new();
    for spec in str_vec(&hc["Binds"]) {
        let parts: Vec<&str> = spec.split(':').collect();
        let (src, dst, opts) = match parts.as_slice() {
            [a, b] => (*a, *b, ""),
            [a, b, o] => (*a, *b, *o),
            _ => return Err(DErr(StatusCode::BAD_REQUEST, format!("invalid bind {spec:?}"))),
        };
        let ro = opts.split(',').any(|o| o == "ro");
        if src.starts_with('/') {
            let p = PathBuf::from(src);
            if !p.exists() {
                std::fs::create_dir_all(&p).map_err(|e| DErr(StatusCode::BAD_REQUEST, format!("bind source {src}: {e}")))?;
            }
            binds.push(og_runtime::BindMount { source: p.canonicalize().map_err(|e| DErr(StatusCode::BAD_REQUEST, e.to_string()))?, target: PathBuf::from(dst), read_only: ro });
        } else {
            volumes.push((ctx.d.root.join("volumes").join(src), dst.to_string(), ro));
        }
    }
    for mnt in hc["Mounts"].as_array().into_iter().flatten() {
        let (typ, src, dst, ro) = (mnt["Type"].as_str().unwrap_or("volume"), mnt["Source"].as_str().unwrap_or(""), mnt["Target"].as_str().unwrap_or(""), mnt["ReadOnly"].as_bool().unwrap_or(false));
        match typ {
            "bind" => binds.push(og_runtime::BindMount { source: PathBuf::from(src), target: PathBuf::from(dst), read_only: ro }),
            "volume" => volumes.push((ctx.d.root.join("volumes").join(if src.is_empty() { "anon" } else { src }), dst.to_string(), ro)),
            "tmpfs" => tmpfs.push((PathBuf::from(dst), mnt["TmpfsOptions"]["SizeBytes"].as_u64())),
            other => warnings.push(format!("mount type {other} not supported")),
        }
    }
    if let Some(t) = hc["Tmpfs"].as_object() {
        for k in t.keys() {
            tmpfs.push((PathBuf::from(k), None));
        }
    }

    let devices: Vec<PathBuf> = hc["Devices"].as_array().into_iter().flatten().filter_map(|d| d["PathOnHost"].as_str().map(PathBuf::from)).collect();
    // --gpus / DeviceRequests -> CDI names
    let mut cdi_devices = Vec::new();
    for req in hc["DeviceRequests"].as_array().into_iter().flatten() {
        let driver = req["Driver"].as_str().unwrap_or("");
        let gpu_caps = req["Capabilities"].as_array().is_some_and(|c| c.iter().flat_map(|x| x.as_array().cloned().unwrap_or_default()).any(|v| v == "gpu"));
        if driver == "cdi" {
            cdi_devices.extend(str_vec(&req["DeviceIDs"]));
        } else if driver == "nvidia" || gpu_caps {
            let ids = str_vec(&req["DeviceIDs"]);
            if !ids.is_empty() {
                cdi_devices.extend(ids.iter().map(|i| format!("nvidia.com/gpu={i}")));
            } else if req["Count"].as_i64().unwrap_or(-1) == -1 {
                cdi_devices.push("nvidia.com/gpu=all".into());
            } else {
                let n = req["Count"].as_i64().unwrap_or(1).max(1);
                cdi_devices.extend((0..n).map(|i| format!("nvidia.com/gpu={i}")));
            }
        } else {
            warnings.push(format!("device request with driver {driver:?} not supported"));
        }
    }
    let caps = |k: &str| -> std::result::Result<Vec<og_runtime::Capability>, DErr> {
        str_vec(&hc[k])
            .iter()
            .filter(|c| !c.eq_ignore_ascii_case("all"))
            .map(|c| {
                let n = c.to_ascii_uppercase();
                let n = if n.starts_with("CAP_") { n } else { format!("CAP_{n}") };
                serde_json::from_value(Value::String(n.clone())).map_err(|_| DErr(StatusCode::BAD_REQUEST, format!("unknown capability {c}")))
            })
            .collect()
    };
    let cap_add = caps("CapAdd")?;
    let cap_drop = caps("CapDrop")?;
    let restart = hc["RestartPolicy"]["Name"].as_str().filter(|n| !n.is_empty() && *n != "no").map(|n| {
        let max = hc["RestartPolicy"]["MaximumRetryCount"].as_u64().unwrap_or(0);
        if n == "on-failure" && max > 0 { format!("on-failure:{max}") } else { n.to_string() }
    });
    let extra_hosts: Vec<(String, String)> = str_vec(&hc["ExtraHosts"]).iter().filter_map(|e| e.split_once(':').map(|(h, i)| (h.to_string(), i.to_string()))).collect();

    let mut labels = b.labels.clone().unwrap_or_default();
    if let Some(s) = &ctx.scope {
        // OG-Host policy labels (priority, profile, ...) come from the scope's configuration,
        // never from the client: a scoped client can't promote itself or loosen its sandbox
        labels.retain(|k, _| !k.starts_with("og."));
        for (k, v) in &s.labels {
            labels.insert(k.clone(), v.clone());
        }
        labels.insert(s.key.clone(), if s.value.is_empty() { "1".into() } else { s.value.clone() });
    }
    let healthcheck = b.healthcheck.as_ref().and_then(|h| {
        let test = str_vec(&h["Test"]);
        (!test.is_empty() && test[0] != "NONE").then(|| og_compose::Healthcheck {
            test,
            interval_s: h["Interval"].as_u64().map(|n| n / 1_000_000_000).unwrap_or(30).max(1),
            timeout_s: h["Timeout"].as_u64().map(|n| n / 1_000_000_000).unwrap_or(30).max(1),
            retries: h["Retries"].as_u64().unwrap_or(3) as u32,
            start_period_s: h["StartPeriod"].as_u64().map(|n| n / 1_000_000_000).unwrap_or(0),
            disabled: false,
        })
    });
    let extra = Extra {
        restart,
        volumes,
        extra_hosts,
        labels,
        auto_remove: hc["AutoRemove"].as_bool().unwrap_or(false),
        no_start: true,
        healthcheck,
        aliases,
        ..Default::default()
    };
    let entrypoint = match &b.entrypoint {
        Some(Value::String(s)) if !s.is_empty() => Some(vec![s.clone()]),
        Some(Value::Array(_)) => Some(str_vec(b.entrypoint.as_ref().unwrap())).filter(|v| !v.is_empty()),
        _ => None,
    };
    let memory = hc["Memory"].as_i64().filter(|m| *m > 0);
    let memory_low = hc["MemoryReservation"].as_i64().filter(|m| *m > 0);
    let shm = hc["ShmSize"].as_u64().filter(|m| *m > 0);
    let pids = hc["PidsLimit"].as_i64().filter(|p| *p > 0);
    let cpu_shares = hc["CpuShares"].as_u64().filter(|c| *c > 0);
    let cpu_quota = cpu_quota_of(&hc);
    let cpuset_cpus = hc["CpusetCpus"].as_str().filter(|s| !s.is_empty()).map(String::from);
    let cpuset_mems = hc["CpusetMems"].as_str().filter(|s| !s.is_empty()).map(String::from);
    let io_weight = hc["BlkioWeight"].as_u64().filter(|w| *w > 0).map(|w| (w as u16).saturating_mul(10).min(10000));
    let io_limits = io_limits_of(&hc);
    let read_only = hc["ReadonlyRootfs"].as_bool().unwrap_or(false);
    let (image, name) = (b.image.clone(), q.name.clone());
    let d = ctx.d.clone();
    let meta = tokio::task::spawn_blocking(move || {
        d.node.run(&image, &digest, name.as_deref().map(|n| n.trim_start_matches('/')), choice, ports, extra, |o| {
            o.env = b.env.unwrap_or_default();
            o.command = b.cmd.filter(|c| !c.is_empty());
            o.entrypoint = entrypoint;
            o.user = b.user.filter(|u| !u.is_empty());
            o.working_dir = b.working_dir.filter(|w| !w.is_empty());
            if let Some(h) = b.hostname.filter(|h| !h.is_empty()) {
                o.hostname = h;
            }
            o.mounts = binds;
            o.tmpfs = tmpfs;
            o.devices = devices;
            o.cdi_devices = cdi_devices;
            o.seccomp_unconfined = seccomp_unconfined;
            o.cap_add = cap_add;
            o.cap_drop = cap_drop;
            o.memory_max = memory;
            o.memory_low = memory_low;
            o.shm_size = shm;
            o.cpu_shares = cpu_shares;
            o.cpu_quota = cpu_quota;
            o.cpuset_cpus = cpuset_cpus;
            o.cpuset_mems = cpuset_mems;
            o.io_weight = io_weight;
            o.io_limits = io_limits;
            o.read_only_root = read_only;
            if let Some(p) = pids {
                o.pids_max = p;
            }
        })
    })
    .await
    .map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        let msg = format!("{e:#}");
        DErr(if msg.contains("already exists") { StatusCode::CONFLICT } else { StatusCode::INTERNAL_SERVER_ERROR }, msg)
    })?;
    Ok((StatusCode::CREATED, Json(json!({"Id": meta.id, "Warnings": warnings}))))
}

async fn container_start(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>) -> R<StatusCode> {
    let m = find(&ctx, &id)?;
    if ctx.d.node.status(&m.id).state == "running" {
        return Ok(StatusCode::NOT_MODIFIED);
    }
    let d = ctx.d.clone();
    tokio::task::spawn_blocking(move || d.node.start(&m.id)).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct StopQ {
    t: Option<u64>,
    signal: Option<String>,
}

async fn container_stop(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, Query(q): Query<StopQ>) -> R<StatusCode> {
    let m = find(&ctx, &id)?;
    if ctx.d.node.status(&m.id).state != "running" {
        return Ok(StatusCode::NOT_MODIFIED);
    }
    let d = ctx.d.clone();
    let t = q.t.unwrap_or(10);
    tokio::task::spawn_blocking(move || d.node.stop(&m.id, t)).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

async fn container_restart(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, Query(q): Query<StopQ>) -> R<StatusCode> {
    let m = find(&ctx, &id)?;
    let d = ctx.d.clone();
    let t = q.t.unwrap_or(10);
    tokio::task::spawn_blocking(move || {
        d.node.stop(&m.id, t)?;
        d.node.start(&m.id)
    })
    .await
    .map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

fn signal_number(s: &str) -> Option<i32> {
    let s = s.trim().trim_start_matches("SIG").to_ascii_uppercase();
    if let Ok(n) = s.parse() {
        return Some(n);
    }
    Some(match s.as_str() {
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        "WINCH" => libc::SIGWINCH,
        _ => return None,
    })
}

async fn container_kill(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, Query(q): Query<StopQ>) -> R<StatusCode> {
    let m = find(&ctx, &id)?;
    let sig = signal_number(q.signal.as_deref().unwrap_or("KILL")).ok_or_else(|| DErr(StatusCode::BAD_REQUEST, "unknown signal".into()))?;
    if ctx.d.node.status(&m.id).state != "running" {
        return Err(DErr(StatusCode::CONFLICT, format!("Container {} is not running", m.id)));
    }
    ctx.d.node.kill(&m.id, sig)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct WaitQ {
    condition: Option<String>,
}

/// Headers go out immediately (clients start the container only after `wait` has been
/// accepted); the body carries the exit status once the container exits.
async fn container_wait(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, Query(q): Query<WaitQ>) -> R<Response> {
    let m = find(&ctx, &id)?;
    let next_exit = matches!(q.condition.as_deref(), Some("next-exit") | Some("removed"));
    let initial = ctx.d.node.status(&m.id);
    let d = ctx.d.clone();
    let fut = async move {
        let mut saw_running = initial.state == "running";
        loop {
            let st = d.node.status(&m.id);
            if st.state == "running" {
                saw_running = true;
            }
            let fresh_exit = matches!(st.state.as_str(), "exited" | "failed" | "dead")
                && (!next_exit || saw_running || st.finished_at != initial.finished_at);
            if fresh_exit {
                let code = st.exit_code.unwrap_or(if st.state == "failed" { 127 } else { 0 });
                let err = st.error.map(|e| json!({"Message": e})).unwrap_or(Value::Null);
                return Ok::<_, std::io::Error>(axum::body::Bytes::from(json!({"StatusCode": code, "Error": err}).to_string()));
            }
            if !d.root.join("containers").join(&m.id).exists() {
                return Ok(axum::body::Bytes::from(json!({"StatusCode": 0, "Error": null}).to_string())); // auto-removed
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    Ok(Response::builder().header(header::CONTENT_TYPE, "application/json").body(Body::from_stream(futures_util::stream::once(fut))).unwrap())
}

/// Attach (output only): upgrade the connection to a raw stream and relay the container's
/// output, in Docker's multiplexed framing, until it exits. Stdin isn't supported yet.
async fn container_attach(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, req: Request<Body>) -> R<Response> {
    let m = find(&ctx, &id)?;
    let path = ctx.d.node.log_path(&m.id);
    let mut offset = std::fs::metadata(&path).map(|x| x.len()).unwrap_or(0);
    let d = ctx.d.clone();
    let id = m.id.clone();
    tokio::spawn(async move {
        let Ok(upgraded) = hyper::upgrade::on(req).await else { return };
        let mut io = hyper_util::rt::TokioIo::new(upgraded);
        use tokio::io::AsyncWriteExt;
        let mut seen_running = false;
        loop {
            let len = std::fs::metadata(&path).map(|x| x.len()).unwrap_or(offset);
            if len > offset {
                use std::io::{Read, Seek, SeekFrom};
                if let Ok(mut f) = std::fs::File::open(&path) {
                    let _ = f.seek(SeekFrom::Start(offset));
                    let mut buf = Vec::new();
                    let _ = f.read_to_end(&mut buf);
                    offset += buf.len() as u64;
                    if io.write_all(&frame(1, &buf)).await.is_err() {
                        return;
                    }
                }
                continue;
            }
            let state = d.node.status(&id).state;
            if state == "running" {
                seen_running = true;
            } else if seen_running || matches!(state.as_str(), "exited" | "failed" | "dead") && d.node.status(&id).started_at.is_some() {
                // drain anything written right before exit, then close
                tokio::time::sleep(Duration::from_millis(100)).await;
                if std::fs::metadata(&path).map(|x| x.len()).unwrap_or(offset) > offset {
                    continue;
                }
                let _ = io.shutdown().await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONTENT_TYPE, "application/vnd.docker.multiplexed-stream")
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "tcp")
        .body(Body::empty())
        .unwrap())
}

/// docker update: live resource changes.
async fn container_update(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, Json(b): Json<Value>) -> R<Json<Value>> {
    let m = find(&ctx, &id)?;
    let mut r = crate::resources::Resources::default();
    let mut warnings = Vec::new();
    if let Some(v) = b["Memory"].as_i64().filter(|v| *v != 0) {
        r.memory_max = Some(v.max(0));
    }
    if let Some(v) = b["MemoryReservation"].as_i64().filter(|v| *v != 0) {
        r.memory_low = Some(v.max(0));
    }
    if let Some((q, p)) = cpu_quota_of(&b) {
        r.cpus = Some(q as f64 / p as f64);
    }
    if let Some(sh) = b["CpuShares"].as_u64().filter(|v| *v > 0) {
        r.cpu_weight = Some(1 + (sh.clamp(2, 262144) - 2) * 9999 / 262142);
    }
    if let Some(c) = b["CpusetCpus"].as_str().filter(|c| !c.is_empty()) {
        r.cpuset_cpus = Some(c.to_string());
    }
    if let Some(p) = b["PidsLimit"].as_i64().filter(|v| *v != 0) {
        r.pids_max = Some(p.max(0));
    }
    if let Some(w) = b["BlkioWeight"].as_u64().filter(|w| *w > 0) {
        r.io_weight = Some((w as u16).saturating_mul(10).min(10000));
    }
    if b["RestartPolicy"]["Name"].as_str().is_some_and(|n| !n.is_empty()) {
        warnings.push("restart policy changes are not supported by update yet".to_string());
    }
    let d = ctx.d.clone();
    tokio::task::spawn_blocking(move || crate::resources::update(&d.node, &m.id, &r)).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({ "Warnings": warnings })))
}

// ---- exec ----------------------------------------------------------------------------------

#[derive(Clone)]
struct ExecRec {
    container: String,
    argv: Vec<String>,
    env: Vec<String>,
    cwd: Option<String>,
    user: Option<String>,
    tty: bool,
    stdin: bool,
    running: bool,
    started: bool,
    exit_code: Option<i32>,
    pid: Option<u32>,
    created: std::time::Instant,
}

static EXECS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, ExecRec>>> = std::sync::LazyLock::new(Default::default);

fn exec_get(ctx: &Ctx, id: &str) -> R<ExecRec> {
    let rec = EXECS.lock().unwrap().get(id).cloned().ok_or_else(|| not_found(&format!("exec instance {id}")))?;
    find(ctx, &rec.container)?; // scope check: the container must be visible to this client
    Ok(rec)
}

fn exec_update(id: &str, f: impl FnOnce(&mut ExecRec)) {
    if let Some(r) = EXECS.lock().unwrap().get_mut(id) {
        f(r);
    }
}

async fn exec_create(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, Json(b): Json<Value>) -> R<(StatusCode, Json<Value>)> {
    let m = find(&ctx, &id)?;
    if ctx.d.node.status(&m.id).state != "running" {
        return Err(DErr(StatusCode::CONFLICT, format!("container {} is not running", m.id)));
    }
    if b["Privileged"].as_bool() == Some(true) {
        return Err(DErr(StatusCode::FORBIDDEN, "privileged exec is not supported".into()));
    }
    let argv = str_vec(&b["Cmd"]);
    if argv.is_empty() {
        return Err(DErr(StatusCode::BAD_REQUEST, "no exec command specified".into()));
    }
    let eid = crate::containers::new_id().map_err(DErr::from)?;
    {
        let mut all = EXECS.lock().unwrap();
        // forget finished execs after an hour
        all.retain(|_, r| r.running || r.created.elapsed() < Duration::from_secs(3600));
        all.insert(
            eid.clone(),
            ExecRec {
                container: m.id.clone(),
                argv,
                env: str_vec(&b["Env"]),
                cwd: b["WorkingDir"].as_str().map(String::from),
                user: b["User"].as_str().map(String::from),
                tty: b["Tty"].as_bool().unwrap_or(false),
                stdin: b["AttachStdin"].as_bool().unwrap_or(false),
                running: false,
                started: false,
                exit_code: None,
                pid: None,
                created: std::time::Instant::now(),
            },
        );
    }
    Ok((StatusCode::CREATED, Json(json!({ "Id": eid }))))
}

async fn exec_start(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, req: Request<Body>) -> R<Response> {
    let rec = exec_get(&ctx, &id)?;
    if rec.started {
        return Err(DErr(StatusCode::CONFLICT, "exec instance already started".into()));
    }
    let upgrade = req.headers().get(header::UPGRADE).is_some();
    let (parts, body) = req.into_parts();
    let b: Value = axum::body::to_bytes(body, 1 << 20).await.ok().and_then(|x| serde_json::from_slice(&x).ok()).unwrap_or(Value::Null);
    let detach = b["Detach"].as_bool().unwrap_or(false);
    let tty = rec.tty || b["Tty"].as_bool().unwrap_or(false);
    let std_cmd = ctx.d.node.exec_command(&rec.container, &rec.argv, rec.user.as_deref(), &rec.env, rec.cwd.as_deref()).map_err(DErr::from)?;
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(false);
    if detach {
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    } else {
        cmd.stdin(if rec.stdin && upgrade { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, format!("starting exec: {e}")))?;
    let pid = child.id();
    exec_update(&id, |r| {
        r.started = true;
        r.running = true;
        r.pid = pid;
    });
    let finish = |id: String, code: Option<i32>| exec_update(&id, |r| {
        r.running = false;
        r.exit_code = Some(code.unwrap_or(-1));
    });
    if detach {
        let id2 = id.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code());
            finish(id2, code);
        });
        return Ok(StatusCode::OK.into_response());
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let pump = |mut r: Box<dyn tokio::io::AsyncRead + Unpin + Send>, stream: u8, tx: tokio::sync::mpsc::Sender<Vec<u8>>| async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = if tty { buf[..n].to_vec() } else { frame(stream, &buf[..n]) };
                    if tx.send(chunk).await.is_err() {
                        break;
                    }
                }
            }
        }
    };
    let out = tokio::spawn(pump(Box::new(child.stdout.take().unwrap()), 1, tx.clone()));
    let err = tokio::spawn(pump(Box::new(child.stderr.take().unwrap()), 2, tx));
    let child_stdin = child.stdin.take();
    let id2 = id.clone();
    let waiter = tokio::spawn(async move {
        let _ = out.await;
        let _ = err.await;
        let code = child.wait().await.ok().and_then(|s| s.code());
        finish(id2, code);
    });

    if !upgrade {
        // plain HTTP client: stream the output as the body, no stdin
        let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|c| (Ok::<_, std::io::Error>(axum::body::Bytes::from(c)), rx)) });
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" })
            .body(Body::from_stream(stream))
            .unwrap());
    }
    let req = Request::from_parts(parts, Body::empty());
    tokio::spawn(async move {
        let Ok(upgraded) = hyper::upgrade::on(req).await else { return };
        let io = hyper_util::rt::TokioIo::new(upgraded);
        let (mut rd, mut wr) = tokio::io::split(io);
        if let Some(mut sin) = child_stdin {
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut rd, &mut sin).await;
                let _ = sin.shutdown().await; // client closed its side: EOF to the process
            });
        }
        while let Some(chunk) = rx.recv().await {
            if wr.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = waiter.await;
        let _ = wr.shutdown().await;
    });
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONTENT_TYPE, if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" })
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "tcp")
        .body(Body::empty())
        .unwrap())
}

async fn exec_inspect(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>) -> R<Json<Value>> {
    let r = exec_get(&ctx, &id)?;
    Ok(Json(json!({
        "ID": id,
        "ContainerID": r.container,
        "Running": r.running,
        "ExitCode": r.exit_code,
        "Pid": r.pid.unwrap_or(0),
        "OpenStdin": r.stdin,
        "OpenStdout": true,
        "OpenStderr": true,
        "CanRemove": !r.running && r.started,
        "ProcessConfig": {
            "tty": r.tty,
            "entrypoint": r.argv.first(),
            "arguments": r.argv.get(1..).unwrap_or_default(),
            "privileged": false,
            "user": r.user.clone().unwrap_or_default(),
        },
    })))
}

/// No real terminals yet (exec -t streams raw without a pty); accept resizes so clients don't fail.
async fn exec_resize() -> StatusCode {
    StatusCode::OK
}

#[derive(Deserialize)]
struct DeleteQ {
    force: Option<String>,
}

async fn container_delete(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, Query(q): Query<DeleteQ>) -> R<StatusCode> {
    let m = find(&ctx, &id)?;
    if ctx.d.node.status(&m.id).state == "running" {
        if !truthy(&q.force) {
            return Err(DErr(StatusCode::CONFLICT, format!("cannot remove container {}: container is running: stop the container before removing or force remove", m.name)));
        }
        let d = ctx.d.clone();
        let mid = m.id.clone();
        tokio::task::spawn_blocking(move || d.node.stop(&mid, 2)).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    }
    ctx.d.node.remove(&m.id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LogsQ {
    follow: Option<String>,
    tail: Option<String>,
    timestamps: Option<String>,
}

/// Docker's multiplexed stream framing: [stream, 0, 0, 0, len(u32 BE)] + payload.
fn frame(stream: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len());
    v.extend_from_slice(&[stream, 0, 0, 0]);
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    v
}

async fn container_logs(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>, Query(q): Query<LogsQ>) -> R<Response> {
    let m = find(&ctx, &id)?;
    let path = ctx.d.node.log_path(&m.id);
    let text = std::fs::read(&path).unwrap_or_default();
    let lines: Vec<&[u8]> = text.split_inclusive(|b| *b == b'\n').collect();
    let start = match q.tail.as_deref() {
        Some("all") | None => 0,
        Some(n) => lines.len().saturating_sub(n.parse().unwrap_or(lines.len())),
    };
    let _ = truthy(&q.timestamps); // timestamps aren't recorded per line yet
    let mut first = Vec::new();
    for l in &lines[start..] {
        first.extend(frame(1, l));
    }
    let follow = truthy(&q.follow);
    let offset = text.len() as u64;
    let d = ctx.d.clone();
    let mid = m.id.clone();
    let stream = async_stream(first, follow, path, offset, d, mid);
    Ok(Response::builder().header(header::CONTENT_TYPE, "application/vnd.docker.multiplexed-stream").body(Body::from_stream(stream)).unwrap())
}

/// First the existing log, then (if following) new bytes as they arrive until the container stops.
fn async_stream(
    first: Vec<u8>,
    follow: bool,
    path: PathBuf,
    mut offset: u64,
    d: Arc<Daemon>,
    id: String,
) -> impl futures_util::Stream<Item = std::result::Result<axum::body::Bytes, std::io::Error>> {
    futures_util::stream::unfold((Some(first), follow), move |(pending, follow)| {
        let path = path.clone();
        let d = d.clone();
        let id = id.clone();
        async move {
            if let Some(p) = pending {
                return Some((Ok(axum::body::Bytes::from(p)), (None, follow)));
            }
            if !follow {
                return None;
            }
            loop {
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if len > offset {
                    use std::io::{Read, Seek, SeekFrom};
                    let mut f = std::fs::File::open(&path).ok()?;
                    f.seek(SeekFrom::Start(offset)).ok()?;
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf).ok()?;
                    offset += buf.len() as u64;
                    return Some((Ok(axum::body::Bytes::from(frame(1, &buf))), (None, true)));
                }
                if d.node.status(&id).state != "running" {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    })
}

async fn container_stats(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>) -> R<Json<Value>> {
    let m = find(&ctx, &id)?;
    let cg = PathBuf::from("/sys/fs/cgroup/og-host").join(&m.name);
    let read = |f: &str| std::fs::read_to_string(cg.join(f)).unwrap_or_default();
    let num = |s: &str| s.trim().parse::<u64>().unwrap_or(0);
    let usage_usec = read("cpu.stat").lines().find(|l| l.starts_with("usage_usec")).and_then(|l| l.split_whitespace().nth(1)).map(num).unwrap_or(0);
    let mem = num(&read("memory.current"));
    let limit = read("memory.max").trim().parse::<u64>().unwrap_or(u64::MAX);
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let system = std::fs::read_to_string("/proc/stat").unwrap_or_default().lines().next().map(|l| l.split_whitespace().skip(1).filter_map(|x| x.parse::<u64>().ok()).sum::<u64>() * 10_000_000).unwrap_or(0);
    Ok(Json(json!({
        "read": rfc3339(now()), "name": format!("/{}", m.name), "id": m.id,
        "pids_stats": {"current": num(&read("pids.current"))},
        "cpu_stats": {"cpu_usage": {"total_usage": usage_usec * 1000}, "system_cpu_usage": system, "online_cpus": ncpu},
        "precpu_stats": {"cpu_usage": {"total_usage": 0}, "system_cpu_usage": 0, "online_cpus": ncpu},
        "memory_stats": {"usage": mem, "limit": if limit == u64::MAX { 0 } else { limit }},
        "networks": {},
    })))
}

// ---------- builds ----------

#[derive(Deserialize)]
struct BuildQ {
    t: Option<String>,
    dockerfile: Option<String>,
    buildargs: Option<String>,
    target: Option<String>,
}

/// `docker build` (classic API): the body is a tar of the build context.
async fn image_build(State(ctx): State<Ctx>, Query(q): Query<BuildQ>, body: Body) -> R<Response> {
    if ctx.scope.is_some() {
        return Err(DErr(StatusCode::FORBIDDEN, "builds are not allowed on this socket".into()));
    }
    let tag = q.t.clone().unwrap_or_else(|| format!("og-build-{}", now()));
    let dir = ctx.d.root.join("tmp").join(format!("ctx-{}-{}", std::process::id(), now()));
    std::fs::create_dir_all(&dir).map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let tar_path = dir.with_extension("tar");
    // stream the upload to disk (contexts can be large), then unpack it safely
    {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::File::create(&tar_path).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| DErr(StatusCode::BAD_REQUEST, e.to_string()))?;
            f.write_all(&chunk).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        f.flush().await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let args: BTreeMap<String, String> = q
        .buildargs
        .as_deref()
        .and_then(|a| serde_json::from_str::<BTreeMap<String, Option<String>>>(a).ok())
        .map(|m| m.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect())
        .unwrap_or_default();
    let root = ctx.d.root.clone();
    let (dir2, tar2) = (dir.clone(), tar_path.clone());
    let spec = crate::build::BuildSpec { context: dir.clone(), dockerfile: q.dockerfile.clone(), tag: tag.clone(), args, target: q.target.clone() };
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<(String, String)> {
        // the same escape-proof unpacker used for image layers: a context can't write outside `dir`
        og_oci::unpack::apply_layer(&dir2, og_oci::unpack::open_layer(&tar2)?, &og_oci::unpack::UnpackOptions::default())?;
        crate::build::build_with(&root, &spec, true)
    })
    .await
    .map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&tar_path);
    let mut out = String::new();
    match result {
        Ok((digest, log)) => {
            for l in log.lines() {
                out.push_str(&json!({"stream": format!("{l}\n")}).to_string());
                out.push('\n');
            }
            let id = ctx.d.node.store.read_sync(&digest).ok().and_then(|b| serde_json::from_slice::<og_oci::registry::Manifest>(&b).ok()).map(|m| m.config.digest).unwrap_or(digest);
            out.push_str(&json!({"aux": {"ID": id}}).to_string());
            out.push('\n');
            out.push_str(&json!({"stream": format!("Successfully built {}\nSuccessfully tagged {tag}\n", &id[7..19])}).to_string());
            out.push('\n');
        }
        Err(e) => {
            out.push_str(&json!({"errorDetail": {"message": format!("{e:#}")}, "error": format!("{e:#}")}).to_string());
            out.push('\n');
        }
    }
    Ok(Response::builder().header(header::CONTENT_TYPE, "application/json").body(Body::from(out)).unwrap())
}

// ---------- images ----------

/// (reference, manifest digest) for every pulled image.
fn list_images(ctx: &Ctx) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let refs = ctx.d.root.join("refs");
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else if let Ok(d) = std::fs::read_to_string(&p) {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
                if let Some((repo, tag)) = rel.rsplit_once('/') {
                    let tag = tag.replacen('_', ":", 1);
                    let display = repo.strip_prefix("docker.io/library/").or_else(|| repo.strip_prefix("docker.io/")).unwrap_or(repo).to_string();
                    let sep = if tag.starts_with("sha256:") { "@" } else { ":" };
                    out.push((format!("{display}{sep}{tag}"), d.trim().to_string()));
                }
            }
        }
    }
    walk(&refs, &refs, &mut out);
    out
}

fn image_config(ctx: &Ctx, manifest_digest: &str) -> Option<(String, og_oci::registry::ImageConfig, u64)> {
    let mf: og_oci::registry::Manifest = serde_json::from_slice(&ctx.d.node.store.read_sync(manifest_digest).ok()?).ok()?;
    let cfg: og_oci::registry::ImageConfig = serde_json::from_slice(&ctx.d.node.store.read_sync(&mf.config.digest).ok()?).ok()?;
    Some((mf.config.digest.clone(), cfg, mf.layers.iter().map(|l| l.size).sum()))
}

async fn images_list(State(ctx): State<Ctx>) -> R<Json<Vec<Value>>> {
    let mut by_id: BTreeMap<String, (Vec<String>, u64, Value)> = BTreeMap::new();
    for (r, d) in list_images(&ctx) {
        if let Some((id, cfg, size)) = image_config(&ctx, &d) {
            let e = by_id.entry(id).or_insert((vec![], size, json!(cfg.config.labels)));
            e.0.push(r);
        }
    }
    Ok(Json(
        by_id
            .into_iter()
            .map(|(id, (tags, size, labels))| json!({"Id": id, "RepoTags": tags, "RepoDigests": [], "Created": 0, "Size": size, "SharedSize": -1, "Labels": labels, "Containers": -1, "ParentId": ""}))
            .collect(),
    ))
}

#[derive(Deserialize)]
struct PullQ {
    #[serde(rename = "fromImage")]
    from_image: Option<String>,
    tag: Option<String>,
}

async fn images_create(State(ctx): State<Ctx>, Query(q): Query<PullQ>) -> R<Response> {
    let img = q.from_image.ok_or_else(|| DErr(StatusCode::BAD_REQUEST, "fromImage is required (image import isn't supported)".into()))?;
    let full = match q.tag.as_deref().filter(|t| !t.is_empty()) {
        Some(t) if t.starts_with("sha256:") => format!("{img}@{t}"),
        Some(t) => format!("{img}:{t}"),
        None => img.clone(),
    };
    let r = og_oci::Reference::parse(&full).map_err(|e| DErr(StatusCode::BAD_REQUEST, e.to_string()))?;
    let _ = std::fs::remove_file(ctx.d.root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_")));
    let res = crate::ensure_pulled(&ctx.d.root, &ctx.d.node.store, &full, true).await;
    let mut body = String::new();
    body.push_str(&format!("{}\n", json!({"status": format!("Pulling from {}", r.repository), "id": r.tag.clone().unwrap_or_default()})));
    match res {
        Ok(d) => {
            body.push_str(&format!("{}\n", json!({"status": format!("Digest: {d}")})));
            body.push_str(&format!("{}\n", json!({"status": format!("Status: Downloaded newer image for {full}")})));
        }
        Err(e) => body.push_str(&format!("{}\n", json!({"errorDetail": {"message": format!("{e:#}")}, "error": format!("{e:#}")}))),
    }
    Ok(Response::builder().header(header::CONTENT_TYPE, "application/json").body(Body::from(body)).unwrap())
}

async fn image_inspect_or(State(ctx): State<Ctx>, UrlPath(name): UrlPath<String>) -> R<Json<Value>> {
    let name = name.strip_suffix("/json").unwrap_or(&name).to_string();
    let r = og_oci::Reference::parse(&name).map_err(|_| DErr(StatusCode::NOT_FOUND, format!("No such image: {name}")))?;
    let d = std::fs::read_to_string(ctx.d.root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_")))
        .map_err(|_| DErr(StatusCode::NOT_FOUND, format!("No such image: {name}")))?;
    let (id, cfg, size) = image_config(&ctx, d.trim()).ok_or_else(|| DErr(StatusCode::NOT_FOUND, format!("No such image: {name}")))?;
    Ok(Json(json!({
        "Id": id, "RepoTags": [name], "RepoDigests": [format!("{}@{}", r.repository, d.trim())], "Size": size,
        "Architecture": cfg.architecture, "Os": cfg.os,
        "Config": {"Env": cfg.config.env, "Cmd": cfg.config.cmd, "Entrypoint": cfg.config.entrypoint, "WorkingDir": cfg.config.working_dir,
                   "User": cfg.config.user, "ExposedPorts": cfg.config.exposed_ports, "Volumes": cfg.config.volumes, "Labels": cfg.config.labels},
    })))
}

/// docker load: the body is a `docker save` / OCI archive.
async fn images_load(State(ctx): State<Ctx>, body: axum::body::Body) -> R<Json<Value>> {
    if ctx.scope.is_some() {
        return Err(DErr(StatusCode::FORBIDDEN, "loading images is not allowed on this socket".into()));
    }
    let tmpdir = ctx.d.root.join("content/tmp");
    let _ = std::fs::create_dir_all(&tmpdir);
    let tmp = tmpdir.join(format!("load-{}.tar", crate::containers::unix_now()));
    {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut st = body.into_data_stream();
        while let Some(chunk) = st.next().await {
            let c = chunk.map_err(|e| DErr(StatusCode::BAD_REQUEST, e.to_string()))?;
            f.write_all(&c).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        f.flush().await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let root = ctx.d.root.clone();
    let t2 = tmp.clone();
    let r = tokio::task::spawn_blocking(move || crate::load_archive(&root, &t2, None)).await.map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = std::fs::remove_file(&tmp);
    let loaded = r.map_err(|e| DErr(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({"stream": loaded.iter().map(|(n, _)| format!("Loaded image: {n}\n")).collect::<String>()})))
}

async fn image_delete(State(ctx): State<Ctx>, UrlPath(name): UrlPath<String>) -> R<Json<Value>> {
    if ctx.scope.is_some() {
        return Err(DErr(StatusCode::FORBIDDEN, "removing images is not allowed on this socket".into()));
    }
    let r = og_oci::Reference::parse(&name).map_err(|_| DErr(StatusCode::NOT_FOUND, format!("No such image: {name}")))?;
    let p = ctx.d.root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_"));
    std::fs::remove_file(&p).map_err(|_| DErr(StatusCode::NOT_FOUND, format!("No such image: {name}")))?;
    let d = ctx.d.clone();
    let _ = tokio::task::spawn_blocking(move || d.node.prune_layers()).await;
    Ok(Json(json!([{"Untagged": name}])))
}

// ---------- networks ----------

fn network_json(n: &og_net::Network) -> Value {
    let name = if n.name == "default" { "bridge".to_string() } else { n.name.clone() };
    json!({"Name": name, "Id": n.name, "Created": rfc3339(0), "Scope": "local", "Driver": "bridge", "EnableIPv6": false,
           "IPAM": {"Driver": "default", "Config": [{"Subnet": n.subnet.to_string(), "Gateway": n.subnet.gateway().to_string()}]},
           "Internal": false, "Attachable": true, "Options": {"og-host.bridge": n.bridge}, "Labels": {}})
}

fn labels_path(ctx: &Ctx, kind: &str, name: &str) -> PathBuf {
    ctx.d.root.join("labels").join(kind).join(format!("{name}.json"))
}

fn read_labels(ctx: &Ctx, kind: &str, name: &str) -> BTreeMap<String, String> {
    std::fs::read(labels_path(ctx, kind, name)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_labels(ctx: &Ctx, kind: &str, name: &str, labels: &BTreeMap<String, String>) {
    let p = labels_path(ctx, kind, name);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, serde_json::to_vec(labels).unwrap_or_default());
}

/// label / name / id filters on a JSON object with Name, Id and Labels.
fn object_matches(v: &Value, filters: &BTreeMap<String, Vec<String>>) -> bool {
    filters.iter().all(|(k, vals)| match k.as_str() {
        "label" => vals.iter().all(|l| match l.split_once('=') {
            Some((key, val)) => v["Labels"][key].as_str() == Some(val),
            None => !v["Labels"][l.as_str()].is_null(),
        }),
        "name" => vals.iter().any(|n| v["Name"].as_str().is_some_and(|x| x.contains(n.as_str()))),
        "id" => vals.iter().any(|i| v["Id"].as_str().is_some_and(|x| x.starts_with(i.as_str()))),
        "driver" => vals.iter().any(|d| v["Driver"].as_str() == Some(d.as_str())),
        _ => true,
    })
}

#[derive(Deserialize)]
struct FilterQ {
    filters: Option<String>,
}

async fn networks_list(State(ctx): State<Ctx>, Query(q): Query<FilterQ>) -> R<Json<Vec<Value>>> {
    let _ = ctx.d.node.net.ensure("default", None);
    let filters = parse_filters(&q.filters);
    let mut out: Vec<Value> = ctx
        .d
        .node
        .net
        .list()
        .map_err(anyhow::Error::from)?
        .iter()
        .map(|n| {
            let mut v = network_json(n);
            v["Labels"] = json!(read_labels(&ctx, "networks", &n.name));
            v
        })
        .collect();
    out.push(json!({"Name": "host", "Id": "host", "Scope": "local", "Driver": "host", "IPAM": {"Config": []}, "Labels": {}}));
    out.push(json!({"Name": "none", "Id": "none", "Scope": "local", "Driver": "null", "IPAM": {"Config": []}, "Labels": {}}));
    Ok(Json(out.into_iter().filter(|v| object_matches(v, &filters)).collect()))
}

fn net_name(id: &str) -> String {
    if id == "bridge" { "default".into() } else { id.to_string() }
}

async fn network_inspect(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>) -> R<Json<Value>> {
    if id == "host" || id == "none" {
        return Ok(Json(json!({"Name": id, "Id": id, "Scope": "local", "Driver": if id == "host" {"host"} else {"null"}, "IPAM": {"Config": []}, "Labels": {}})));
    }
    let n = ctx.d.node.net.get(&net_name(&id)).map_err(anyhow::Error::from)?.ok_or_else(|| DErr(StatusCode::NOT_FOUND, format!("network {id} not found")))?;
    let mut v = network_json(&n);
    v["Labels"] = json!(read_labels(&ctx, "networks", &n.name));
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NetCreate {
    name: String,
    #[serde(default)]
    ipam: Option<Value>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

async fn network_create(State(ctx): State<Ctx>, Json(b): Json<NetCreate>) -> R<(StatusCode, Json<Value>)> {
    if ctx.d.node.net.get(&b.name).map_err(anyhow::Error::from)?.is_some() {
        return Err(DErr(StatusCode::CONFLICT, format!("network with name {} already exists", b.name)));
    }
    let subnet = b.ipam.as_ref().and_then(|i| i["Config"][0]["Subnet"].as_str().map(String::from));
    let n = ctx.d.node.net.ensure(&b.name, subnet.as_deref()).map_err(|e| DErr(StatusCode::BAD_REQUEST, e.to_string()))?;
    write_labels(&ctx, "networks", &n.name, &b.labels.unwrap_or_default());
    Ok((StatusCode::CREATED, Json(json!({"Id": n.name, "Warning": ""}))))
}

async fn network_delete(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>) -> R<StatusCode> {
    if ctx.scope.is_some() {
        return Err(DErr(StatusCode::FORBIDDEN, "removing networks is not allowed on this socket".into()));
    }
    let name = net_name(&id);
    if name == "default" || name == "host" || name == "none" {
        return Err(DErr(StatusCode::FORBIDDEN, format!("{id} is a pre-defined network and cannot be removed")));
    }
    ctx.d.node.net.remove(&name).map_err(|e| DErr(StatusCode::CONFLICT, e.to_string()))?;
    let _ = std::fs::remove_file(labels_path(&ctx, "networks", &name));
    Ok(StatusCode::NO_CONTENT)
}

// ---------- volumes ----------

fn volume_json(ctx: &Ctx, name: &str) -> Value {
    json!({"Name": name, "Driver": "local", "Mountpoint": ctx.d.root.join("volumes").join(name).join("_data"),
           "Labels": read_labels(ctx, "volumes", name), "Scope": "local", "Options": {}})
}

async fn volumes_list(State(ctx): State<Ctx>, Query(q): Query<FilterQ>) -> R<Json<Value>> {
    let filters = parse_filters(&q.filters);
    let mut vols = Vec::new();
    if let Ok(rd) = std::fs::read_dir(ctx.d.root.join("volumes")) {
        for e in rd.flatten() {
            let v = volume_json(&ctx, &e.file_name().to_string_lossy());
            if object_matches(&v, &filters) {
                vols.push(v);
            }
        }
    }
    Ok(Json(json!({"Volumes": vols, "Warnings": []})))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolCreate {
    name: Option<String>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

async fn volume_create(State(ctx): State<Ctx>, Json(b): Json<VolCreate>) -> R<(StatusCode, Json<Value>)> {
    let name = b.name.filter(|n| !n.is_empty()).unwrap_or_else(|| format!("vol-{}", now()));
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
        return Err(DErr(StatusCode::BAD_REQUEST, format!("invalid volume name {name:?}")));
    }
    let data = ctx.d.root.join("volumes").join(&name).join("_data");
    let fresh = !data.exists();
    std::fs::create_dir_all(&data).map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if fresh {
        let _ = std::fs::remove_dir(&data); // let the first container seed it from its image (copy-up)
        let _ = std::fs::create_dir_all(data.parent().unwrap());
        write_labels(&ctx, "volumes", &name, &b.labels.unwrap_or_default());
    }
    Ok((StatusCode::CREATED, Json(volume_json(&ctx, &name))))
}

async fn volume_inspect(State(ctx): State<Ctx>, UrlPath(name): UrlPath<String>) -> R<Json<Value>> {
    if !ctx.d.root.join("volumes").join(&name).exists() {
        return Err(DErr(StatusCode::NOT_FOUND, format!("get {name}: no such volume")));
    }
    Ok(Json(volume_json(&ctx, &name)))
}

async fn volume_delete(State(ctx): State<Ctx>, UrlPath(name): UrlPath<String>) -> R<StatusCode> {
    if ctx.scope.is_some() {
        return Err(DErr(StatusCode::FORBIDDEN, "removing volumes is not allowed on this socket".into()));
    }
    let d = ctx.d.root.join("volumes").join(&name);
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) || !d.exists() {
        return Err(DErr(StatusCode::NOT_FOUND, format!("get {name}: no such volume")));
    }
    std::fs::remove_dir_all(&d).map_err(|e| DErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = std::fs::remove_file(labels_path(&ctx, "volumes", &name));
    Ok(StatusCode::NO_CONTENT)
}
