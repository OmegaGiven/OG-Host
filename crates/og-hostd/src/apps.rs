//! The node's app homepage and go-links aliases.
//!
//! Apps are found, not configured: one tile per compose project, its address taken from
//! `tailscale serve` (https on the tailnet name) or a published port. `og.*` labels on any of
//! the project's containers adjust a tile (`og.title`, `og.url`, `og.icon`, `og.group`,
//! `og.description`, `og.alias`, `og.hidden=1`), and so do overrides in `<root>/aliases.json`
//! (no container recreate needed).
//!
//! Aliases (`<root>/aliases.json` `links`, plus every app's own name) answer on a separate
//! plain-HTTP listener (`"listen"` in the file, e.g. `127.0.0.1:8412`), meant to sit behind a
//! small tailnet node such as `go2`: `go2/host2/x?y` redirects to the alias's URL + `/x?y`.
//! The file is re-read when it changes, so `og-hostd alias …` edits apply at once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::serve::Daemon;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Link {
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Also show it as a tile on the homepage.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tile: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub icon: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// Per-project tile changes, same meaning as the `og.*` labels (and winning over them).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Extra alias names (comma-separated), besides the project's own name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AliasFile {
    /// The tailnet name the aliases are typed with (`go2` -> go2/host2), shown in the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Address of the alias redirect listener (plain HTTP); none = no listener.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    pub links: BTreeMap<String, Link>,
    pub apps: BTreeMap<String, AppOverride>,
}

fn file(root: &Path) -> PathBuf {
    root.join("aliases.json")
}

pub fn valid_alias(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.')
}

fn valid_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://")) && u.len() > 8 && !u.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn load(root: &Path) -> AliasFile {
    std::fs::read(file(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Atomic: temp file, fsync, rename (og-host2 has lost unsynced files before).
pub fn save(root: &Path, f: &AliasFile) -> Result<()> {
    use std::io::Write;
    let p = file(root);
    let tmp = p.with_extension("json.tmp");
    let mut out = std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    out.write_all(&serde_json::to_vec_pretty(f)?)?;
    out.sync_all()?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

// ---- what tailscale knows -----------------------------------------------------------------

#[derive(Clone, Default)]
struct Tailnet {
    /// `og-host2.tail6c6721.ts.net`
    name: Option<String>,
    /// local backend port -> https URL `tailscale serve` publishes it at
    served: BTreeMap<u16, String>,
}

static TAILNET: Mutex<Option<(Instant, Tailnet)>> = Mutex::new(None);

fn tailnet() -> Tailnet {
    let mut c = TAILNET.lock().unwrap();
    if let Some((t, v)) = c.as_ref() {
        if t.elapsed() < Duration::from_secs(30) {
            return v.clone();
        }
    }
    let run = |args: &[&str]| -> Option<Value> {
        let out = std::process::Command::new("tailscale").args(args).output().ok()?;
        serde_json::from_slice(&out.stdout).ok()
    };
    let mut v = Tailnet::default();
    if let Some(st) = run(&["status", "--json"]) {
        v.name = st["Self"]["DNSName"].as_str().map(|n| n.trim_end_matches('.').to_string()).filter(|n| !n.is_empty());
    }
    if let Some(sv) = run(&["serve", "status", "--json"]) {
        for (hostport, web) in sv["Web"].as_object().into_iter().flatten() {
            let Some(port) = hostport.rsplit_once(':').map(|(_, p)| p) else { continue };
            let Some(proxy) = web["Handlers"]["/"]["Proxy"].as_str() else { continue };
            if let Some(backend) = proxy.rsplit_once(':').and_then(|(_, p)| p.trim_end_matches('/').parse::<u16>().ok()) {
                let host = hostport.rsplit_once(':').map(|(h, _)| h).unwrap_or(hostport);
                let url = if port == "443" { format!("https://{host}") } else { format!("https://{host}:{port}") };
                v.served.entry(backend).or_insert(url);
            }
        }
    }
    *c = Some((Instant::now(), v.clone()));
    v
}

// ---- tiles ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Tile {
    /// Project name, or `link:<alias>` for a manual link tile.
    pub id: String,
    pub title: String,
    pub url: Option<String>,
    /// Other addresses the project publishes.
    pub more: Vec<String>,
    pub group: String,
    pub icon: String,
    pub description: String,
    /// running | sleeping | stopped | partial | link
    pub state: String,
    pub containers: Vec<String>,
    pub aliases: Vec<String>,
    pub hidden: bool,
}

fn title_case(s: &str) -> String {
    s.split(['-', '_']).filter(|w| !w.is_empty()).map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" ")
}

pub fn tiles(d: &Daemon) -> Vec<Tile> {
    let f = load(&d.root);
    let tn = tailnet();
    let mut projects: BTreeMap<String, Vec<(crate::containers::Meta, crate::containers::Status)>> = BTreeMap::new();
    for (m, st) in d.node.list().unwrap_or_default() {
        let key = m.project.clone().unwrap_or_else(|| m.name.clone());
        projects.entry(key).or_default().push((m, st));
    }
    let mut out = Vec::new();
    for (name, cs) in projects {
        let label = |k: &str| cs.iter().find_map(|(m, _)| m.labels.get(k).filter(|v| !v.is_empty()).cloned());
        let o = f.apps.get(&name).cloned().unwrap_or_default();
        // addresses: served (https) first, then ports reachable beyond loopback
        let mut served = Vec::new();
        let mut plain = Vec::new();
        for (m, _) in &cs {
            for p in m.net.as_ref().map(|a| a.ports.as_slice()).unwrap_or_default() {
                if !matches!(p.proto, og_net::Proto::Tcp) {
                    continue;
                }
                if let Some(u) = tn.served.get(&p.host_port) {
                    served.push(u.clone());
                } else if p.host_ip.is_none_or(|ip| !ip.is_loopback()) {
                    let host = match (p.host_ip, &tn.name) {
                        (Some(ip), _) if !ip.is_unspecified() => ip.to_string(),
                        (_, Some(n)) => n.clone(),
                        _ => continue,
                    };
                    plain.push(format!("http://{host}:{}", p.host_port));
                }
            }
        }
        let mut urls: Vec<String> = served.into_iter().chain(plain).collect();
        urls.dedup();
        let url = o.url.clone().or_else(|| label("og.url")).or_else(|| urls.first().cloned());
        let more = urls.into_iter().filter(|u| Some(u) != url.as_ref()).collect();
        let running = cs.iter().filter(|(_, st)| st.state == "running").count();
        let sleeping = cs.iter().any(|(m, _)| m.desired == "sleeping");
        let state = if running == cs.len() { "running" } else if sleeping { "sleeping" } else if running == 0 { "stopped" } else { "partial" };
        let mut aliases = vec![name.to_lowercase()];
        for a in o.alias.clone().or_else(|| label("og.alias")).unwrap_or_default().split(',') {
            let a = a.trim().to_lowercase();
            if valid_alias(&a) && !aliases.contains(&a) {
                aliases.push(a);
            }
        }
        let hidden = o.hidden.unwrap_or_else(|| label("og.hidden").is_some_and(|v| v == "1" || v == "true"));
        out.push(Tile {
            title: o.title.clone().or_else(|| label("og.title")).unwrap_or_else(|| title_case(&name)),
            url,
            more,
            group: o.group.clone().or_else(|| label("og.group")).unwrap_or_default(),
            icon: o.icon.clone().or_else(|| label("og.icon")).unwrap_or_default(),
            description: o.description.clone().or_else(|| label("og.description")).unwrap_or_default(),
            state: state.into(),
            containers: cs.iter().map(|(m, _)| m.name.clone()).collect(),
            aliases,
            hidden,
            id: name,
        });
    }
    for (name, l) in &f.links {
        if l.tile {
            out.push(Tile {
                id: format!("link:{name}"),
                title: if l.title.is_empty() { title_case(name) } else { l.title.clone() },
                url: Some(l.url.clone()),
                more: vec![],
                group: l.group.clone(),
                icon: l.icon.clone(),
                description: l.description.clone(),
                state: "link".into(),
                containers: vec![],
                aliases: vec![name.clone()],
                hidden: false,
            });
        }
    }
    out
}

/// Where an alias points: manual links first, then apps (their name or extra aliases).
pub fn resolve(d: &Daemon, alias: &str) -> Option<String> {
    let alias = alias.to_lowercase();
    if let Some(l) = load(&d.root).links.get(&alias) {
        return Some(l.url.clone());
    }
    tiles(d).into_iter().find(|t| t.aliases.contains(&alias)).and_then(|t| t.url)
}

// ---- API (/v1/apps, /v1/aliases) -----------------------------------------------------------

type App = Arc<Daemon>;

fn bad(e: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response()
}

pub fn router(d: App) -> Router {
    Router::new()
        .route("/v1/apps", get(apps_get))
        .route("/v1/apps/{name}", put(app_put))
        .route("/v1/aliases", get(aliases_get))
        .route("/v1/aliases/{name}", put(alias_put).delete(alias_delete))
        .with_state(d)
}

async fn apps_get(State(d): State<App>) -> Json<Value> {
    let tn = tokio::task::spawn_blocking({
        let d = d.clone();
        move || { let f = load(&d.root); (tiles(&d), tailnet().name, f.listen, f.name) }
    })
    .await
    .unwrap_or_default();
    Json(json!({"tiles": tn.0, "node": tn.1, "alias_listen": tn.2, "alias_name": tn.3}))
}

async fn app_put(State(d): State<App>, UrlPath(name): UrlPath<String>, Json(o): Json<AppOverride>) -> Response {
    if let Some(u) = o.url.as_deref().filter(|u| !u.is_empty()) {
        if !valid_url(u) {
            return bad("url must start with http:// or https://");
        }
    }
    let mut f = load(&d.root);
    let empty = serde_json::to_value(&o).map(|v| v.as_object().is_none_or(|m| m.is_empty())).unwrap_or(true);
    if empty { f.apps.remove(&name); } else { f.apps.insert(name, o); }
    match save(&d.root, &f) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

async fn aliases_get(State(d): State<App>) -> Json<AliasFile> {
    Json(load(&d.root))
}

async fn alias_put(State(d): State<App>, UrlPath(name): UrlPath<String>, Json(l): Json<Link>) -> Response {
    let name = name.to_lowercase();
    if !valid_alias(&name) {
        return bad("alias names: a-z, 0-9, - _ . (up to 64)");
    }
    if !valid_url(&l.url) {
        return bad("url must start with http:// or https://");
    }
    let mut f = load(&d.root);
    f.links.insert(name, l);
    match save(&d.root, &f) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

async fn alias_delete(State(d): State<App>, UrlPath(name): UrlPath<String>) -> Response {
    let mut f = load(&d.root);
    if f.links.remove(&name.to_lowercase()).is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such alias"}))).into_response();
    }
    match save(&d.root, &f) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

// ---- the redirect listener -----------------------------------------------------------------

/// Start the alias listener when aliases.json asks for one (read once at startup).
pub async fn start(d: App) {
    let Some(addr) = load(&d.root).listen else { return };
    let l = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => return eprintln!("og-hostd: alias listener {addr}: {e}"),
    };
    eprintln!("og-hostd: aliases on http://{addr}");
    let r = Router::new().fallback(redirect).with_state(d);
    if let Err(e) = axum::serve(l, r).await {
        eprintln!("og-hostd: alias listener: {e}");
    }
}

async fn redirect(State(d): State<App>, req: Request<Body>) -> Response {
    let path = req.uri().path().trim_start_matches('/').to_string();
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let (alias, rest) = path.split_once('/').map(|(a, r)| (a.to_string(), format!("/{r}"))).unwrap_or((path.clone(), String::new()));
    let alias = percent_decode(&alias);
    let d2 = d.clone();
    let a2 = alias.clone();
    let target = if alias.is_empty() {
        // the bare name: this node's homepage
        tokio::task::spawn_blocking(move || tailnet().name.map(|n| format!("https://{n}:8006/#myapps"))).await.ok().flatten()
    } else {
        tokio::task::spawn_blocking(move || resolve(&d2, &a2)).await.ok().flatten()
    };
    match target {
        Some(t) => {
            let to = if rest.is_empty() && query.is_empty() { t } else { format!("{}{rest}{query}", t.trim_end_matches('/')) };
            let mut r = StatusCode::FOUND.into_response();
            if let Ok(v) = HeaderValue::from_str(&to) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        None => not_found(&d, &alias).await,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Unknown alias: a small page listing what exists (closest first).
async fn not_found(d: &App, alias: &str) -> Response {
    let d = d.clone();
    let (links, apps, node) = tokio::task::spawn_blocking(move || (load(&d.root).links, tiles(&d), tailnet().name)).await.unwrap_or_default();
    let mut rows: Vec<(String, String)> = links.into_iter().map(|(k, l)| (k, l.url)).collect();
    for t in apps {
        if let Some(u) = t.url.filter(|_| !t.hidden) {
            for a in t.aliases {
                rows.push((a, u.clone()));
            }
        }
    }
    rows.sort_by_key(|(k, _)| (!k.contains(alias) || alias.is_empty(), k.clone()));
    rows.dedup_by(|a, b| a.0 == b.0);
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let list: String = rows.iter().map(|(k, u)| format!("<li><a href=\"/{0}\"><b>{0}</b></a> <span>{1}</span></li>", esc(k), esc(u))).collect();
    let manage = node.map(|n| format!("<p><a href=\"https://{n}:8006/#links\">Manage aliases</a></p>")).unwrap_or_default();
    let head = if alias.is_empty() { "Aliases".to_string() } else { format!("No alias “{}”", esc(alias)) };
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{head}</title>\
<style>body{{font:15px system-ui,sans-serif;margin:0 auto;max-width:720px;padding:24px 16px;background:#0b0d12;color:#e6e8ee}}a{{color:#7aa2ff}}span{{color:#8a90a0;font-size:13px;word-break:break-all}}li{{margin:6px 0}}ul{{padding-left:18px}}</style>\
<h1>{head}</h1>{manage}<ul>{list}</ul>"
    );
    let mut r = (StatusCode::NOT_FOUND, html).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    crate::ui::security_headers(&mut r);
    r
}

// ---- CLI -----------------------------------------------------------------------------------

/// `og-hostd alias ls | add NAME URL | rm NAME | name go2 | listen ADDR | import shortcuts.json`
pub fn cli(root: &Path, args: &[String]) -> Result<()> {
    let mut f = load(root);
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["ls"] => {
            if let Some(l) = &f.listen {
                println!("listening on http://{l}{}", f.name.as_ref().map(|n| format!(" (typed as {n}/…)")).unwrap_or_default());
            }
            for (k, l) in &f.links {
                println!("{k:20} {}{}", l.url, if l.tile { "  (tile)" } else { "" });
            }
            return Ok(());
        }
        ["add", name, url, flags @ ..] => {
            let name = name.to_lowercase();
            if !valid_alias(&name) {
                bail!("alias names: a-z, 0-9, - _ . (up to 64)");
            }
            if !valid_url(url) {
                bail!("url must start with http:// or https://");
            }
            let mut l = f.links.remove(&name).unwrap_or_default();
            l.url = url.to_string();
            let mut it = flags.iter();
            while let Some(fl) = it.next() {
                match *fl {
                    "--tile" => l.tile = true,
                    "--no-tile" => l.tile = false,
                    "--title" => l.title = it.next().context("--title wants a value")?.to_string(),
                    "--group" => l.group = it.next().context("--group wants a value")?.to_string(),
                    "--icon" => l.icon = it.next().context("--icon wants a value")?.to_string(),
                    other => bail!("unknown flag {other:?} (--tile, --no-tile, --title T, --group G, --icon I)"),
                }
            }
            f.links.insert(name, l);
        }
        ["rm", name] => {
            if f.links.remove(&name.to_lowercase()).is_none() {
                bail!("no alias {name:?}");
            }
        }
        ["name", n] => f.name = (*n != "off").then(|| n.to_string()),
        ["listen", addr] => {
            f.listen = if *addr == "off" { None } else { Some(addr.parse::<std::net::SocketAddr>().context("listen wants ip:port or off")?.to_string()) };
            println!("restart og-hostd to apply (systemctl restart og-hostd)");
        }
        ["import", path] => {
            // go-alias-rust's shortcuts.json: { "name": "url", ... }; existing names are kept
            let m: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(path)?).context("expected {\"name\": \"url\", …}")?;
            let mut n = 0;
            for (k, u) in m {
                let k = k.to_lowercase();
                if valid_alias(&k) && valid_url(&u) && !f.links.contains_key(&k) {
                    f.links.insert(k, Link { url: u, ..Default::default() });
                    n += 1;
                }
            }
            println!("imported {n} aliases");
        }
        _ => bail!("usage: og-hostd alias [ls | add NAME URL [--tile] [--title T] [--group G] [--icon I] | rm NAME | name SHORTNAME | listen ADDR|off | import FILE]"),
    }
    save(root, &f)
}
