//! The OG-Host web UI: static assets compiled into the binary, and browser sign-in.
//!
//! Assets (crates/og-hostd/ui) are embedded, gzipped once at startup and served with ETags,
//! so the UI loads from the daemon itself with no CDN or internet access.
//!
//! Sign-in: accounts in `<root>/ui-users.json` (argon2id hashes; set with `og-hostd passwd`).
//! A session is a random 256-bit token in an HttpOnly, SameSite=Strict cookie, kept in memory
//! (12 h sliding). Requests that change something with a cookie must carry `X-OG-UI: 1` (a
//! cross-site page can't add that header without a CORS preflight we never allow) and, when
//! the browser sends one, an Origin matching the Host. Failed sign-ins are slowed and limited
//! per client.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::serve::Daemon;

const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const COOKIE: &str = "og_session";

// ---- assets -------------------------------------------------------------------------------

struct Asset {
    mime: &'static str,
    raw: &'static [u8],
    gz: Vec<u8>,
    etag: String,
}

macro_rules! asset {
    ($path:literal, $mime:literal) => {
        ($path, $mime, include_bytes!(concat!("../ui/", $path)) as &'static [u8])
    };
}

static ASSETS: LazyLock<HashMap<&'static str, Asset>> = LazyLock::new(|| {
    use std::io::Write;
    [
        asset!("index.html", "text/html; charset=utf-8"),
        asset!("app.css", "text/css; charset=utf-8"),
        asset!("app.js", "text/javascript; charset=utf-8"),
        asset!("lab.js", "text/javascript; charset=utf-8"),
        asset!("favicon.svg", "image/svg+xml"),
    ]
    .into_iter()
    .map(|(name, mime, raw)| {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let _ = enc.write_all(raw);
        let gz = enc.finish().unwrap_or_default();
        // FNV-1a: good enough to tell versions apart
        let h = raw.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3));
        (name, Asset { mime, raw, gz, etag: format!("\"{h:016x}\"") })
    })
    .collect()
});

fn security_headers(r: &mut Response) {
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
}

pub async fn asset(req: Request<Body>) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let name = if path.is_empty() || !path.contains('.') { "index.html" } else { path };
    // development: OG_HOST_UI_DIR serves the files from disk (set by root in the unit)
    if let Ok(dir) = std::env::var("OG_HOST_UI_DIR") {
        if let Some(a) = ASSETS.get(name) {
            if let Ok(body) = std::fs::read(Path::new(&dir).join(name)) {
                let mut r = Response::new(Body::from(body));
                r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(a.mime));
                r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                security_headers(&mut r);
                return r;
            }
        }
    }
    let Some(a) = ASSETS.get(name) else { return StatusCode::NOT_FOUND.into_response() };
    let inm = req.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    let mut r = if inm == Some(a.etag.as_str()) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let gzip = req.headers().get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("gzip"));
        let mut r = Response::new(Body::from(if gzip { a.gz.clone() } else { a.raw.to_vec() }));
        if gzip {
            r.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(a.mime));
        r
    };
    r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&a.etag).unwrap());
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r.headers_mut().insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    security_headers(&mut r);
    r
}

// ---- accounts -----------------------------------------------------------------------------

#[derive(Default, Serialize, Deserialize)]
struct Users {
    users: HashMap<String, User>,
}

#[derive(Serialize, Deserialize)]
struct User {
    hash: String,
}

fn users_path(root: &Path) -> PathBuf {
    root.join("ui-users.json")
}

fn load_users(root: &Path) -> Users {
    std::fs::read(users_path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn valid_user(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// Create or change an account (CLI: `og-hostd passwd <user>`).
pub fn set_password(root: &Path, user: &str, password: &str) -> Result<()> {
    use argon2::password_hash::PasswordHasher;
    if !valid_user(user) {
        bail!("user names are 1-32 letters, digits, _ . -");
    }
    if password.chars().count() < 10 {
        bail!("use at least 10 characters");
    }
    let hash = argon2::Argon2::default().hash_password(password.as_bytes(), &salt()?).map_err(|e| anyhow::anyhow!("{e}"))?.to_string();
    let mut u = load_users(root);
    u.users.insert(user.to_string(), User { hash });
    let p = users_path(root);
    let tmp = p.with_extension("tmp");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&u)?)?;
    }
    std::fs::rename(&tmp, &p).context("saving accounts")?;
    Sessions::end_user(root, user);
    Ok(())
}

/// 16 random bytes from the kernel.
fn salt() -> Result<argon2::password_hash::SaltString> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    argon2::password_hash::SaltString::encode_b64(&b).map_err(|e| anyhow::anyhow!("{e}"))
}

pub fn remove_user(root: &Path, user: &str) -> Result<bool> {
    let mut u = load_users(root);
    let had = u.users.remove(user).is_some();
    std::fs::write(users_path(root), serde_json::to_vec_pretty(&u)?)?;
    Sessions::end_user(root, user);
    Ok(had)
}

fn verify(root: &Path, user: &str, password: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let users = load_users(root);
    // unknown users still cost a hash, so timing doesn't tell which names exist
    static DUMMY: LazyLock<String> = LazyLock::new(|| {
        use argon2::password_hash::PasswordHasher;
        salt().ok().and_then(|s| argon2::Argon2::default().hash_password(b"not-a-password", &s).ok().map(|h| h.to_string())).unwrap_or_default()
    });
    let hash = users.users.get(user).map(|u| u.hash.as_str()).unwrap_or(DUMMY.as_str());
    let ok = PasswordHash::new(hash).is_ok_and(|h| argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok());
    ok && users.users.contains_key(user)
}

// ---- sessions -----------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    user: String,
    /// Unix seconds.
    expires: u64,
    /// Fingerprint of the password hash at sign-in: a new password ends older sessions.
    #[serde(default)]
    pw: String,
}

fn pw_fingerprint(root: &Path, user: &str) -> Option<String> {
    load_users(root).users.get(user).map(|u| token_hash(&u.hash)[..16].to_string())
}

/// Sessions survive daemon restarts (updates shouldn't sign everyone out): kept in
/// `<root>/ui-sessions.json` (0600) by SHA-256 of the token, so the file can't be replayed as
/// cookies. Sliding expiry is tracked in memory and written back at most every 10 minutes.
#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Session>>,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    file: Mutex<Option<PathBuf>>,
}

fn token_hash(t: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(t.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

impl Sessions {
    pub fn load(root: &Path) -> Self {
        let file = root.join("ui-sessions.json");
        let mut map: HashMap<String, Session> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let users = load_users(root);
        let now = crate::containers::unix_now();
        map.retain(|_, s| s.expires > now && users.users.contains_key(&s.user));
        Sessions { map: Mutex::new(map), failures: Default::default(), file: Mutex::new(Some(file)) }
    }

    fn save(&self, map: &HashMap<String, Session>) {
        let Some(file) = self.file.lock().unwrap().clone() else { return };
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = file.with_extension("tmp");
        let ok = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(&serde_json::to_vec(map).unwrap_or_default()));
        if ok.is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }

    fn create(&self, root: &Path, user: &str) -> String {
        use std::io::Read;
        let mut b = [0u8; 32];
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
        let now = crate::containers::unix_now();
        let mut m = self.map.lock().unwrap();
        m.retain(|_, s| s.expires > now);
        m.insert(token_hash(&t), Session { user: user.to_string(), expires: now + SESSION_TTL.as_secs(), pw: pw_fingerprint(root, user).unwrap_or_default() });
        self.save(&m);
        t
    }

    /// The signed-in user, sliding the expiry forward.
    fn check(&self, root: &Path, token: &str) -> Option<String> {
        let h = token_hash(token);
        let now = crate::containers::unix_now();
        let mut m = self.map.lock().unwrap();
        let s = m.get_mut(&h)?;
        // account removed or password changed since sign-in
        if s.expires <= now || pw_fingerprint(root, &s.user).is_none_or(|f| f != s.pw) {
            m.remove(&h);
            self.save(&m);
            return None;
        }
        let user = s.user.clone();
        let renew = s.expires < now + SESSION_TTL.as_secs() - 600;
        s.expires = now + SESSION_TTL.as_secs();
        if renew {
            self.save(&m);
        }
        Some(user)
    }

    fn end(&self, token: &str) {
        let mut m = self.map.lock().unwrap();
        m.remove(&token_hash(token));
        self.save(&m);
    }

    /// Sign out every session of `user` (password changed or account removed).
    pub fn end_user(root: &Path, user: &str) {
        let s = Sessions::load(root);
        let mut m = s.map.lock().unwrap();
        m.retain(|_, x| x.user != user);
        s.save(&m);
    }

    /// Locked out after 5 failures within 10 minutes, for the rest of those 10 minutes.
    fn locked(&self, ip: IpAddr) -> bool {
        self.failures.lock().unwrap().get(&ip).is_some_and(|(n, since)| *n >= 5 && since.elapsed() < Duration::from_secs(600))
    }

    fn failed(&self, ip: IpAddr) {
        let mut f = self.failures.lock().unwrap();
        let e = f.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() >= Duration::from_secs(600) {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(String::from))
        .filter(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Client address; behind a local reverse proxy (tailscale serve) the forwarded one.
fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if peer.ip().is_loopback() {
        if let Some(ip) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).and_then(|v| v.trim().parse().ok()) {
            return ip;
        }
    }
    peer.ip()
}

fn https(headers: &HeaderMap) -> bool {
    headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
}

#[derive(Deserialize)]
pub struct Login {
    user: String,
    password: String,
}

pub async fn login(
    State(d): State<Arc<Daemon>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<axum::Extension<crate::tls::Secure>>,
    headers: HeaderMap,
    Json(b): Json<Login>,
) -> Response {
    let ip = client_ip(peer, &headers);
    if d.sessions.locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": "too many failed sign-ins; try again in a few minutes"}))).into_response();
    }
    let root = d.root.clone();
    let (user, pass) = (b.user.clone(), b.password);
    let ok = tokio::task::spawn_blocking(move || verify(&root, &user, &pass)).await.unwrap_or(false);
    if !ok {
        d.sessions.failed(ip);
        tokio::time::sleep(Duration::from_millis(600)).await;
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "wrong user name or password"}))).into_response();
    }
    let token = d.sessions.create(&d.root, &b.user);
    let secure = if tls.is_some() || https(&headers) { "; Secure" } else { "" };
    let mut r = Json(json!({"user": b.user})).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure}", SESSION_TTL.as_secs())).unwrap(),
    );
    r
}

pub async fn logout(State(d): State<Arc<Daemon>>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie_token(&headers) {
        d.sessions.end(&t);
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_static("og_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"));
    r
}

pub async fn me(State(d): State<Arc<Daemon>>, headers: HeaderMap) -> Response {
    match cookie_token(&headers).and_then(|t| d.sessions.check(&d.root, &t)) {
        Some(u) => Json(json!({"user": u})).into_response(),
        None => {
            let setup = load_users(&d.root).users.is_empty();
            (StatusCode::UNAUTHORIZED, Json(json!({"error": "not signed in", "setup_needed": setup}))).into_response()
        }
    }
}

fn bearer_ok(d: &Daemon, headers: &HeaderMap) -> bool {
    let want = d.token.as_deref().unwrap_or("");
    let got = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    !want.is_empty() && got.len() == want.len() && got.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// A change made with a session cookie must carry X-OG-UI: 1 and, when the browser sends an
/// Origin, come from this site.
fn csrf_ok(headers: &HeaderMap) -> bool {
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

/// Who is calling the lab API: its own tokens (admin key, API token, agent token), this
/// server's bearer token, or a signed-in session (changes need the CSRF header).
pub async fn lab_caller(State(d): State<Arc<Daemon>>, mut req: Request<Body>, next: axum::middleware::Next) -> Response {
    let headers = req.headers();
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-omega-key").and_then(|v| v.to_str().ok()))
        .unwrap_or("")
        .to_string();
    let mut caller = match &d.lab {
        Some(l) if !token.is_empty() => l.caller_for_token(&token),
        _ => og_lab::Caller::Anonymous,
    };
    if caller == og_lab::Caller::Anonymous && bearer_ok(&d, headers) {
        caller = og_lab::Caller::Admin;
    }
    if caller == og_lab::Caller::Anonymous && cookie_token(headers).and_then(|t| d.sessions.check(&d.root, &t)).is_some() {
        let safe = matches!(*req.method(), Method::GET | Method::HEAD);
        if safe || csrf_ok(headers) {
            caller = og_lab::Caller::Admin;
        }
    }
    req.extensions_mut().insert(caller);
    next.run(req).await
}

/// TCP listener auth: the UI's own assets and sign-in are public; everything else needs the
/// bearer token or a session (plus the CSRF header for changes).
pub async fn guard(State(d): State<Arc<Daemon>>, req: Request<Body>, next: axum::middleware::Next) -> Response {
    let path = req.uri().path();
    let public = !path.starts_with("/v1/") || path == "/v1/ui/login" || path == "/v1/ui/me" || path == "/v1/ui/logout";
    if public {
        return next.run(req).await;
    }
    let headers = req.headers();
    if bearer_ok(&d, headers) {
        return next.run(req).await;
    }
    let Some(_user) = cookie_token(headers).and_then(|t| d.sessions.check(&d.root, &t)) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "sign in first"}))).into_response();
    };
    let safe = matches!(*req.method(), Method::GET | Method::HEAD);
    if !safe && !csrf_ok(headers) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "missing X-OG-UI header or cross-origin request"}))).into_response();
    }
    next.run(req).await
}
