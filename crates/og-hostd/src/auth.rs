//! Sign-in, sessions and per-request authorization for the TCP listeners.
//!
//! Who is calling: the server's bearer token or root on the unix socket (system), an API
//! token (`Authorization: OGAPIToken user@realm!name=secret`, or as a Bearer), or a browser
//! session. Sessions are 256-bit random tokens in an HttpOnly, SameSite=Strict cookie, stored
//! hashed, 12 h sliding, bound to the account's credentials (a password, two-factor or
//! enable change ends them). Changes made with a cookie must carry `X-OG-UI: 1` and come
//! from this site. Sign-in: password (local or PAM realm), then a TOTP or recovery code when
//! two-factor is on. Failed attempts are slowed and limited per client and per account.
//!
//! Every /v1 route needs a privilege on an ACL path (see [`rule`]); unknown routes need
//! Sys.Modify. Changes and sign-ins are written to the audit log.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use og_access::{Login, Who};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::serve::Daemon;

const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const COOKIE: &str = "og_session";

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    user: String,
    expires: u64,
    #[serde(default)]
    fp: String,
    #[serde(default)]
    ip: String,
    #[serde(default)]
    created: u64,
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Session>>,
    /// two-factor tickets: hash -> (user, ip, deadline)
    pending: Mutex<HashMap<String, (String, String, Instant)>>,
    ip_fail: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    user_fail: Mutex<HashMap<String, (u32, Instant)>>,
    file: PathBuf,
}

fn hash(t: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(t.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut b = vec![0u8; n];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn now() -> u64 {
    og_access::now()
}

impl Sessions {
    pub fn load(root: &Path) -> Self {
        let file = root.join("access").join("sessions.json");
        let mut map: HashMap<String, Session> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        map.retain(|_, s| s.expires > now());
        // sessions from before accounts had realms: "admin" -> "admin@local"
        for s in map.values_mut() {
            if !s.user.contains('@') {
                s.user = format!("{}@local", s.user);
                s.fp.clear();
            }
        }
        Sessions { map: Mutex::new(map), file, ..Default::default() }
    }

    fn save(&self, map: &HashMap<String, Session>) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = self.file.with_extension("tmp");
        if std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(&serde_json::to_vec(map).unwrap_or_default())).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }

    fn create(&self, d: &Daemon, user: &str, ip: &str) -> String {
        let t = random_hex(32);
        let mut m = self.map.lock().unwrap();
        m.retain(|_, s| s.expires > now());
        m.insert(hash(&t), Session { user: user.into(), expires: now() + SESSION_TTL.as_secs(), fp: d.access.fingerprint(user).unwrap_or_default(), ip: ip.into(), created: now() });
        self.save(&m);
        t
    }

    fn check(&self, d: &Daemon, token: &str) -> Option<String> {
        let h = hash(token);
        let mut m = self.map.lock().unwrap();
        let s = m.get_mut(&h)?;
        let fp = d.access.fingerprint(&s.user);
        // migrated sessions adopt the current fingerprint once
        if s.fp.is_empty() {
            s.fp = fp.clone().unwrap_or_default();
        }
        if s.expires <= now() || fp.as_deref() != Some(s.fp.as_str()) {
            m.remove(&h);
            self.save(&m);
            return None;
        }
        let user = s.user.clone();
        let renew = s.expires < now() + SESSION_TTL.as_secs() - 600;
        s.expires = now() + SESSION_TTL.as_secs();
        if renew {
            self.save(&m);
        }
        Some(user)
    }

    fn end(&self, token: &str) {
        let mut m = self.map.lock().unwrap();
        m.remove(&hash(token));
        self.save(&m);
    }

    /// The caller's sessions (for "sign out other devices").
    pub fn list(&self, d: &Daemon, user: &str, current: Option<&str>) -> Vec<serde_json::Value> {
        let cur = current.map(hash);
        let fp = d.access.fingerprint(user).unwrap_or_default();
        self.map.lock().unwrap().iter().filter(|(_, s)| s.user == user && s.expires > now() && (s.fp.is_empty() || s.fp == fp)).map(|(h, s)| json!({"id": &h[..16], "ip": s.ip, "created": s.created, "expires": s.expires, "current": cur.as_deref() == Some(h.as_str())})).collect()
    }

    pub fn revoke(&self, user: &str, id_prefix: &str) -> bool {
        let mut m = self.map.lock().unwrap();
        let n = m.len();
        m.retain(|h, s| !(s.user == user && h.starts_with(id_prefix) && id_prefix.len() >= 8));
        let changed = m.len() != n;
        self.save(&m);
        changed
    }

    fn locked(&self, ip: IpAddr, user: &str) -> bool {
        let win = Duration::from_secs(600);
        let ip_l = self.ip_fail.lock().unwrap().get(&ip).is_some_and(|(n, t)| *n >= 5 && t.elapsed() < win);
        let user_l = self.user_fail.lock().unwrap().get(user).is_some_and(|(n, t)| *n >= 10 && t.elapsed() < win);
        ip_l || user_l
    }

    fn failed(&self, ip: IpAddr, user: &str) {
        let bump = |e: &mut (u32, Instant)| {
            if e.1.elapsed() >= Duration::from_secs(600) {
                *e = (0, Instant::now());
            }
            e.0 += 1;
        };
        bump(self.ip_fail.lock().unwrap().entry(ip).or_insert((0, Instant::now())));
        bump(self.user_fail.lock().unwrap().entry(user.to_string()).or_insert((0, Instant::now())));
    }

    fn cleared(&self, ip: IpAddr, user: &str) {
        self.ip_fail.lock().unwrap().remove(&ip);
        self.user_fail.lock().unwrap().remove(user);
    }
}

pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(String::from))
        .filter(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Client address; behind a local reverse proxy (tailscale serve) the forwarded one.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if peer.ip().is_loopback() {
        if let Some(ip) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).and_then(|v| v.trim().parse().ok()) {
            return ip;
        }
    }
    peer.ip()
}

fn peer_of(req: &Request<Body>) -> String {
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| client_ip(c.0, req.headers()).to_string()).unwrap_or_else(|| "local".into())
}

/// A change made with a session cookie must carry X-OG-UI: 1 and come from this site.
pub fn csrf_ok(headers: &HeaderMap) -> bool {
    if headers.get("x-og-ui").and_then(|v| v.to_str().ok()) != Some("1") {
        return false;
    }
    if let (Some(origin), Some(host)) = (headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()), headers.get(header::HOST).and_then(|v| v.to_str().ok())) {
        let o = origin.split("://").nth(1).unwrap_or(origin);
        if o != host && headers.get("x-forwarded-host").and_then(|v| v.to_str().ok()) != Some(o) {
            return false;
        }
    }
    true
}

fn server_token_ok(d: &Daemon, presented: &str) -> bool {
    let want = d.token.as_deref().unwrap_or("");
    !want.is_empty() && presented.len() == want.len() && presented.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Caller from the headers. `Err` = credentials given but wrong / CSRF check failed.
pub fn who(d: &Daemon, headers: &HeaderMap, method: &Method) -> Result<Option<Who>, &'static str> {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    let cred = auth.strip_prefix("OGAPIToken ").or_else(|| auth.strip_prefix("Bearer ")).map(str::trim);
    if let Some(c) = cred {
        if server_token_ok(d, c) {
            return Ok(Some(Who::System));
        }
        if c.contains('!') {
            return d.access.check_token(c).map(Some).ok_or("bad API token");
        }
        // other bearer tokens (lab agent/API tokens) are for the lab layer to judge
        return Ok(None);
    }
    if let Some(t) = cookie_token(headers) {
        if let Some(u) = d.sessions.check(d, &t) {
            if !matches!(*method, Method::GET | Method::HEAD) && !csrf_ok(headers) {
                return Err("missing X-OG-UI header or cross-origin request");
            }
            return Ok(Some(Who::User(u)));
        }
    }
    Ok(None)
}

// ---- sign-in endpoints ----

#[derive(Deserialize)]
pub struct LoginReq {
    user: String,
    password: String,
    #[serde(default)]
    realm: Option<String>,
}

fn userid(user: &str, realm: Option<&str>) -> String {
    let u = user.trim();
    if u.contains('@') {
        u.to_string()
    } else if u == "root" && realm.is_none() {
        og_access::SUPERUSER.into()
    } else {
        format!("{u}@{}", realm.unwrap_or("local"))
    }
}

fn session_response(d: &Daemon, user: &str, ip: &str, secure: bool) -> Response {
    let token = d.sessions.create(d, user, ip);
    let mut r = Json(json!({"user": user})).into_response();
    let sec = if secure { "; Secure" } else { "" };
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{sec}", SESSION_TTL.as_secs())).unwrap());
    r
}

fn is_secure(tls: &Option<axum::Extension<crate::tls::Secure>>, headers: &HeaderMap) -> bool {
    tls.is_some() || headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
}

pub async fn login(State(d): State<Arc<Daemon>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, tls: Option<axum::Extension<crate::tls::Secure>>, headers: HeaderMap, Json(b): Json<LoginReq>) -> Response {
    let ip = client_ip(peer, &headers);
    let id = userid(&b.user, b.realm.as_deref());
    if d.sessions.locked(ip, &id) {
        d.access.audit(&id, &ip.to_string(), "login", "locked out", false);
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": "too many failed sign-ins; try again in a few minutes"}))).into_response();
    }
    let d2 = d.clone();
    let (id2, pw) = (id.clone(), b.password);
    let r = tokio::task::spawn_blocking(move || d2.access.check_password(&id2, &pw)).await.unwrap_or(Login::Denied);
    match r {
        Login::Denied => {
            d.sessions.failed(ip, &id);
            d.access.audit(&id, &ip.to_string(), "login", "wrong user or password", false);
            tokio::time::sleep(Duration::from_millis(600)).await;
            (StatusCode::UNAUTHORIZED, Json(json!({"error": "wrong user name or password"}))).into_response()
        }
        Login::NeedTfa => {
            let ticket = random_hex(24);
            let mut p = d.sessions.pending.lock().unwrap();
            p.retain(|_, x| x.2 > Instant::now());
            p.insert(hash(&ticket), (id.clone(), ip.to_string(), Instant::now() + Duration::from_secs(300)));
            Json(json!({"tfa_required": true, "ticket": ticket})).into_response()
        }
        Login::Ok => {
            d.sessions.cleared(ip, &id);
            d.access.audit(&id, &ip.to_string(), "login", "", true);
            session_response(&d, &id, &ip.to_string(), is_secure(&tls, &headers))
        }
    }
}

#[derive(Deserialize)]
pub struct TfaReq {
    ticket: String,
    code: String,
}

pub async fn login_tfa(State(d): State<Arc<Daemon>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, tls: Option<axum::Extension<crate::tls::Secure>>, headers: HeaderMap, Json(b): Json<TfaReq>) -> Response {
    let ip = client_ip(peer, &headers);
    let entry = d.sessions.pending.lock().unwrap().get(&hash(&b.ticket)).cloned();
    let Some((user, _, _)) = entry.filter(|e| e.2 > Instant::now()) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "that sign-in expired; start again"}))).into_response();
    };
    if d.sessions.locked(ip, &user) {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": "too many failed attempts; try again in a few minutes"}))).into_response();
    }
    if !d.access.check_tfa(&user, &b.code) {
        d.sessions.failed(ip, &user);
        d.access.audit(&user, &ip.to_string(), "login", "wrong two-factor code", false);
        tokio::time::sleep(Duration::from_millis(600)).await;
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "that code isn't right"}))).into_response();
    }
    d.sessions.pending.lock().unwrap().remove(&hash(&b.ticket));
    d.sessions.cleared(ip, &user);
    d.access.audit(&user, &ip.to_string(), "login", "with two-factor", true);
    session_response(&d, &user, &ip.to_string(), is_secure(&tls, &headers))
}

pub async fn logout(State(d): State<Arc<Daemon>>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie_token(&headers) {
        d.sessions.end(&t);
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_static("og_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"));
    r
}

/// Areas the UI asks about to decide what to show.
pub const AREAS: &[&str] = &["/", "/containers", "/stacks", "/vms", "/storage", "/lab", "/access", "/nodes", "/system"];

pub async fn me(State(d): State<Arc<Daemon>>, headers: HeaderMap) -> Response {
    match who(&d, &headers, &Method::GET) {
        Ok(Some(w)) => {
            let privs: serde_json::Map<String, serde_json::Value> = AREAS.iter().map(|a| (a.to_string(), json!(d.access.privileges(&w, a)))).collect();
            let anywhere: Vec<&str> = og_access::PRIVILEGES.iter().copied().filter(|p| d.access.allowed_anywhere(&w, "/", p)).collect();
            Json(json!({"user": w.label(), "userid": w.user(), "privileges": privs, "anywhere": anywhere, "tfa": d.access.tfa_status(w.user())})).into_response()
        }
        _ => (StatusCode::UNAUTHORIZED, Json(json!({"error": "not signed in", "setup_needed": !d.access.has_users()}))).into_response(),
    }
}

// ---- authorization ----

/// Container id or name -> its name (ACL paths use names).
fn ctr_name(d: &Daemon, key: &str) -> String {
    d.node.find(key).map(|m| m.name).unwrap_or_else(|_| key.to_string())
}

/// (ACL path, privilege) a /v1 request needs; None = any signed-in caller.
pub fn rule(d: &Daemon, m: &Method, path: &str) -> Option<(String, &'static str)> {
    let get = matches!(*m, Method::GET | Method::HEAD);
    let seg: Vec<&str> = path.trim_start_matches("/v1/").split('/').collect();
    let r = |p: String, privilege: &'static str| Some((p, privilege));
    match seg.as_slice() {
        ["version" | "info" | "node" | "brain" | "metrics" | "events"] => r("/".into(), "Sys.Audit"),
        ["containers"] => r("/containers".into(), if get { "Container.Audit" } else { "Container.Allocate" }),
        ["containers", id] => r(format!("/containers/{}", ctr_name(d, id)), if get { "Container.Audit" } else { "Container.Allocate" }),
        ["containers", id, "start" | "stop" | "restart" | "kill" | "wake"] => r(format!("/containers/{}", ctr_name(d, id)), "Container.PowerMgmt"),
        ["containers", id, "logs", ..] => r(format!("/containers/{}", ctr_name(d, id)), "Container.Console"),
        ["containers", id, "resources"] => r(format!("/containers/{}", ctr_name(d, id)), if get { "Container.Audit" } else { "Container.Config" }),
        ["stacks"] => r("/stacks".into(), "Stack.Audit"),
        // a compose file anywhere on disk: root-equivalent
        ["stacks", "up" | "down"] => r("/".into(), "Sys.Modify"),
        ["stacks", name] => r(format!("/stacks/{name}"), if get { "Stack.Audit" } else { "Stack.Modify" }),
        ["stacks", name, _] => r(format!("/stacks/{name}"), "Stack.PowerMgmt"),
        // the Docker API can mount any host path: admin only for changes
        ["docker", ..] => r("/".into(), if get { "Sys.Audit" } else { "Sys.Modify" }),
        ["access", "me", ..] => None,
        ["access", ..] => r("/access".into(), if get { "Access.Audit" } else { "Access.Modify" }),
        ["firewall", ..] => r("/system".into(), if get { "Sys.Audit" } else { "Sys.Modify" }),
        ["storage"] => r("/storage".into(), if get { "Datastore.Audit" } else { "Datastore.Allocate" }),
        ["storage", id] => r(format!("/storage/{id}"), if get { "Datastore.Audit" } else { "Datastore.Allocate" }),
        // keys: the handler also wants Sys.Modify on /
        ["storage", id, "key"] => r(format!("/storage/{id}"), "Datastore.Allocate"),
        ["storage", id, "backups"] => r(format!("/storage/{id}"), if get { "Datastore.Audit" } else { "Datastore.Allocate" }),
        ["storage", id, "backups", "files"] => r(format!("/storage/{id}"), "Datastore.Audit"),
        ["storage", id, "backups", "download"] => r(format!("/storage/{id}"), "Backup.Restore"),
        ["storage", id, "backups", "notes"] => r(format!("/storage/{id}"), "Backup.Create"),
        ["storage", id, "prune" | "gc"] => r(format!("/storage/{id}"), "Datastore.Allocate"),
        ["storage", id, "verify"] => r(format!("/storage/{id}"), "Datastore.Audit"),
        // per target (or on the datastore), checked in the handler
        ["backup"] => None,
        ["backup", "jobs", ..] => r("/storage".into(), if get { "Datastore.Audit" } else { "Datastore.Allocate" }),
        ["restore"] => r("/storage".into(), "Backup.Restore"),
        ["tasks", ..] => r("/".into(), if get { "Sys.Audit" } else { "Sys.Modify" }),
        // chat / approve check AI.Use / AI.Approve themselves; the settings are an admin's
        ["ai", "config"] if !get => r("/".into(), "Sys.Modify"),
        ["ai", "models"] => r("/".into(), "Sys.Modify"),
        ["ai", ..] => None,
        ["update", ..] => r("/system".into(), if get { "Sys.Audit" } else { "Sys.Update" }),
        ["cluster", ..] => r("/nodes".into(), if get { "Cluster.Audit" } else { "Cluster.Modify" }),
        // the node that does the work checks the actual route
        ["nodes", ..] => None,
        ["vms"] | ["vms", "host"] => r("/vms".into(), if get { "VM.Audit" } else { "VM.Allocate" }),
        ["vms", n] => r(format!("/vms/{n}"), match *m { Method::GET | Method::HEAD => "VM.Audit", Method::DELETE => "VM.Allocate", _ => "VM.Config" }),
        ["vms", n, "vnc" | "console"] => r(format!("/vms/{n}"), "VM.Console"),
        ["vms", n, "disks", ..] | ["vms", n, "cdrom"] => r(format!("/vms/{n}"), "VM.Config"),
        ["vms", n, "snapshots", ..] => r(format!("/vms/{n}"), if get { "VM.Audit" } else { "VM.Snapshot" }),
        ["vms", n, _] => r(format!("/vms/{n}"), "VM.PowerMgmt"),
        ["iso", ..] => r("/storage".into(), if get { "Datastore.Audit" } else { "Datastore.Allocate" }),
        _ => r("/".into(), "Sys.Modify"),
    }
}

fn deny(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({"error": msg}))).into_response()
}

/// TCP listener auth for everything under /v1 (and the lab under /api, /mcp, /message).
pub async fn guard(State(d): State<Arc<Daemon>>, mut req: Request<Body>, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    let is_lab = path.starts_with("/api/") || path == "/mcp" || path == "/message";
    let public = (!path.starts_with("/v1/") && !is_lab) || matches!(path.as_str(), "/v1/ui/login" | "/v1/ui/tfa" | "/v1/ui/me" | "/v1/ui/logout");
    if public {
        return next.run(req).await;
    }
    let method = req.method().clone();
    // from another cluster node (it checked the user, and its certificate was checked here)
    let via_cluster = req.extensions().get::<crate::cluster::ClusterUser>().map(|c| c.0.clone());
    let w = match via_cluster.map(|w| Ok(Some(w))).unwrap_or_else(|| who(&d, req.headers(), &method)) {
        Ok(w) => w,
        Err(msg) => return deny(if msg.starts_with("bad") { StatusCode::UNAUTHORIZED } else { StatusCode::FORBIDDEN }, msg),
    };
    let ip = peer_of(&req);
    if is_lab {
        // lab tokens (agents, the AI's API token) are judged by the lab itself
        let (caller, needs) = lab_caller(&d, w.as_ref(), &method, &path, req.headers());
        if let (Some(w), og_lab::Caller::Anonymous) = (&w, &caller) {
            if !matches!(method, Method::GET | Method::HEAD) {
                d.access.audit(&w.label(), &ip, &format!("{method} {path}"), "permission denied", false);
            }
            return deny(StatusCode::FORBIDDEN, &format!("permission denied: {needs} on /lab"));
        }
        req.extensions_mut().insert(caller);
        let changes = !matches!(method, Method::GET | Method::HEAD) && !path.starts_with("/api/ingest") && !path.starts_with("/api/logs/ingest") && !path.starts_with("/api/jobs/result") && path != "/mcp";
        let label = w.as_ref().map(|w| w.label());
        let resp = next.run(req).await;
        if changes {
            if let Some(l) = label {
                d.access.audit(&l, &ip, &format!("{method} {path}"), "", resp.status().is_success());
            }
        }
        return resp;
    }
    let Some(w) = w else { return deny(StatusCode::UNAUTHORIZED, "sign in first") };
    if let Some((acl, privilege)) = rule(&d, &method, &path) {
        if !d.access.allowed(&w, &acl, privilege) {
            if !matches!(method, Method::GET | Method::HEAD) {
                d.access.audit(&w.label(), &ip, &format!("{method} {path}"), "permission denied", false);
            }
            return deny(StatusCode::FORBIDDEN, &format!("permission denied: {privilege} on {acl}"));
        }
    }
    req.extensions_mut().insert(w.clone());
    let changes = !matches!(method, Method::GET | Method::HEAD);
    let resp = next.run(req).await;
    if changes {
        d.access.audit(&w.label(), &ip, &format!("{method} {path}"), "", resp.status().is_success());
    }
    resp
}

/// The lab's coarse caller: its own tokens, or a signed-in caller with the matching privilege.
fn lab_caller(d: &Daemon, w: Option<&Who>, method: &Method, path: &str, headers: &HeaderMap) -> (og_lab::Caller, &'static str) {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-omega-key").and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    if let (Some(l), false) = (&d.lab, token.is_empty() || token.contains('!')) {
        let c = l.caller_for_token(token);
        if c != og_lab::Caller::Anonymous {
            return (c, "");
        }
    }
    let Some(w) = w else { return (og_lab::Caller::Anonymous, "") };
    let privilege = if path.starts_with("/api/assistant/") {
        "AI.Use"
    } else if path.contains("/approve") || path.contains("/reject") {
        "AI.Approve"
    } else if matches!(*method, Method::GET | Method::HEAD) {
        "Lab.Audit"
    } else {
        "Lab.Modify"
    };
    let ok = d.access.allowed(w, "/lab", privilege) && (privilege == "Lab.Audit" || privilege == "AI.Use" || d.access.allowed(w, "/lab", "Lab.Audit"));
    (if ok { og_lab::Caller::Admin } else { og_lab::Caller::Anonymous }, privilege)
}
