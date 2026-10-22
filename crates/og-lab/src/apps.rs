//! Home screen app tiles: groups and layout, health checks every 30 s, known issues,
//! demo-mode config, Homepage import, and the cached icon proxy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{now_ms, Lab};

pub const UNGROUPED: &str = "Ungrouped";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct App {
    pub id: String,
    pub name: String,
    pub group: String,
    pub href: String,
    pub ping: String,
    pub icon: String,
    pub description: String,
    pub host: String,
    pub hidden: bool,
    pub pinned: bool,
    pub order: f64,
    pub source: String,
    pub on_demand: bool,
    pub sensitive: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub checked_at: u64,
    pub fails: u32,
}

pub struct Apps {
    dir: PathBuf,
    pub list: RwLock<Vec<App>>,
    pub groups: RwLock<Option<Vec<String>>>,
    pub removed: RwLock<Vec<String>>,
    pub health: RwLock<HashMap<String, Health>>,
    /// "project:<host>/<name>" | "app:<id>" -> { note, since }
    pub known: RwLock<Map<String, Value>>,
    /// { exitCode, groups, keywords }
    pub demo: RwLock<Value>,
    activity: Mutex<Vec<Value>>,
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Machine name from a tile URL: the tailnet IP map OmegaEnv used, else the first host label.
pub fn host_from_url(u: &str) -> String {
    let Some(rest) = u.split("://").nth(1) else { return String::new() };
    let host = rest.split(['/', ':', '?', '#']).next().unwrap_or("");
    let map = [("100.118.27.80", "station"), ("100.117.29.92", "go"), ("100.118.208.6", "download"), ("100.66.193.126", "vm-3070"), ("100.112.223.44", "vm-2060"), ("100.93.123.117", "station-sway")];
    if let Some((_, n)) = map.iter().find(|(ip, _)| *ip == host) {
        return n.to_string();
    }
    host.split('.').next().unwrap_or("").to_string()
}

impl Apps {
    pub fn load(dir: &Path) -> Apps {
        Apps {
            dir: dir.to_path_buf(),
            list: RwLock::new(crate::store::load(dir, "apps.json").unwrap_or_default()),
            groups: RwLock::new(crate::store::load(dir, "groups.json")),
            removed: RwLock::new(crate::store::load(dir, "removed-apps.json").unwrap_or_default()),
            health: RwLock::new(HashMap::new()),
            known: RwLock::new(crate::store::load(dir, "known.json").unwrap_or_default()),
            // OmegaEnv's defaults when there is no demo.json
            demo: RwLock::new(crate::store::load(dir, "demo.json").unwrap_or_else(|| json!({
                "groups": ["Download Stack", "Finance"],
                "keywords": ["sonarr", "radarr", "lidarr", "prowlarr", "qbittorrent", "torrent", "slskd", "soulseek", "libation", "libby", "arr-stack", "proton", "firefly", "finance", "vaultwarden", "nextcloud-db"],
            }))),
            activity: Mutex::new(crate::store::load(dir, "activity.json").unwrap_or_default()),
        }
    }

    pub fn save(&self) {
        crate::store::save(&self.dir, "apps.json", &*self.list.read().unwrap());
    }

    pub fn save_known(&self) {
        crate::store::save(&self.dir, "known.json", &*self.known.read().unwrap());
    }

    pub fn save_demo(&self) {
        crate::store::save(&self.dir, "demo.json", &*self.demo.read().unwrap());
    }

    pub fn log_activity(&self, mut entry: Value) {
        entry["at"] = json!(now_ms());
        let mut a = self.activity.lock().unwrap();
        a.push(entry);
        let n = a.len();
        if n > 500 {
            a.drain(0..n - 500);
        }
        crate::store::save(&self.dir, "activity.json", &*a);
    }

    pub fn activity(&self) -> Vec<Value> {
        self.activity.lock().unwrap().iter().rev().take(200).cloned().collect()
    }

    /// Ordered group names; groups created by tile edits are appended.
    pub fn group_list(&self) -> Vec<String> {
        let mut g = self.groups.write().unwrap();
        let apps = self.list.read().unwrap();
        let list = g.get_or_insert_with(|| {
            let mut sorted: Vec<&App> = apps.iter().collect();
            sorted.sort_by(|a, b| a.order.total_cmp(&b.order));
            let mut names: Vec<String> = Vec::new();
            for a in sorted {
                if !names.contains(&a.group) {
                    names.push(a.group.clone());
                }
            }
            names
        });
        for a in apps.iter() {
            if !list.contains(&a.group) {
                list.push(a.group.clone());
            }
        }
        list.clone()
    }

    pub fn set_groups(&self, names: Vec<String>) {
        *self.groups.write().unwrap() = Some(names);
        let list = self.group_list();
        crate::store::save(&self.dir, "groups.json", &list);
    }

    pub fn find(&self, r: &str) -> Option<App> {
        let r = r.to_lowercase();
        let l = self.list.read().unwrap();
        l.iter().find(|a| a.id == r).or_else(|| l.iter().find(|a| a.name.to_lowercase() == r)).cloned()
    }

    pub fn patch(&self, id: &str, b: &Value, fields: &[&str]) -> Result<App> {
        let mut l = self.list.write().unwrap();
        let r = id.to_lowercase();
        let Some(a) = l.iter_mut().find(|a| a.id == r || a.name.to_lowercase() == r) else { bail!("no tile \"{id}\". Use list_apps to see ids.") };
        let mut v = serde_json::to_value(&*a)?;
        for k in fields {
            if let Some(x) = b.get(*k) {
                v[*k] = x.clone();
            }
        }
        *a = serde_json::from_value(v)?;
        let out = a.clone();
        drop(l);
        self.save();
        if b.get("group").is_some() {
            let g = self.group_list();
            self.set_groups(g);
        }
        Ok(out)
    }

    pub fn remove(&self, r: &str) -> Result<Value> {
        let a = self.find(r).ok_or_else(|| anyhow::anyhow!("no tile \"{r}\". Use list_apps to see ids."))?;
        self.list.write().unwrap().retain(|x| x.id != a.id);
        {
            let mut rm = self.removed.write().unwrap();
            if !rm.contains(&a.id) {
                rm.push(a.id.clone());
            }
            crate::store::save(&self.dir, "removed-apps.json", &*rm);
        }
        self.save();
        Ok(json!({"removed": a.id, "name": a.name, "note": "Tile removed from the dashboard. The app/container itself was not touched."}))
    }

    pub fn upsert(&self, b: &Value) -> App {
        let id = slug(b["id"].as_str().or(b["name"].as_str()).unwrap_or("app"));
        {
            let mut rm = self.removed.write().unwrap();
            if rm.contains(&id) {
                rm.retain(|x| *x != id);
                crate::store::save(&self.dir, "removed-apps.json", &*rm);
            }
        }
        let mut l = self.list.write().unwrap();
        let prev = l.iter().find(|a| a.id == id).cloned();
        let mut v = serde_json::to_value(prev.clone().unwrap_or(App {
            order: 9999.0,
            source: "manual".into(),
            group: "Installed".into(),
            ping: b["href"].as_str().unwrap_or("").into(),
            ..Default::default()
        }))
        .unwrap();
        for (k, x) in b.as_object().into_iter().flatten() {
            v[k] = x.clone();
        }
        v["id"] = json!(id);
        let mut a: App = serde_json::from_value(v).unwrap_or_default();
        if a.host.is_empty() {
            a.host = prev.map(|p| p.host).filter(|h| !h.is_empty()).unwrap_or_else(|| host_from_url(&a.href));
        }
        l.retain(|x| x.id != id);
        l.push(a.clone());
        drop(l);
        self.save();
        a
    }

    /// Which compose project serves a tile: same host + a container publishing the tile's port.
    pub fn app_project(&self, lab: &Lab, a: &App) -> Option<(String, String)> {
        let rest = a.href.split("://").nth(1)?;
        let hostport = rest.split('/').next()?;
        let port: u16 = hostport.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(if a.href.starts_with("https:") { 443 } else { 80 });
        let snap = lab.fleet.latest(&a.host)?;
        let needle = format!(":{port}->");
        snap["containers"].as_array()?.iter().find(|c| c["project"].as_str().is_some() && c["ports"].as_str().unwrap_or("").contains(&needle)).map(|c| (a.host.clone(), c["project"].as_str().unwrap().to_string()))
    }

    /// The known-issue entry covering an app, a project or a container, if any.
    pub fn known_for(&self, lab: &Lab, app_id: Option<&str>, host: Option<&str>, project: Option<&str>, container: Option<&str>) -> Option<Value> {
        let known = self.known.read().unwrap();
        if known.is_empty() {
            return None;
        }
        let with_key = |k: &str| known.get(k).map(|v| {
            let mut v = v.clone();
            v["key"] = json!(k);
            v
        });
        if let Some(id) = app_id {
            if let Some(v) = with_key(&format!("app:{id}")) {
                return Some(v);
            }
        }
        let mut host = host.map(String::from);
        let mut project = project.map(String::from);
        if let (Some(h), Some(c), None) = (&host, container, &project) {
            project = lab.fleet.latest(h).and_then(|s| s["containers"].as_array().and_then(|cs| cs.iter().find(|x| x["name"] == c).and_then(|x| x["project"].as_str().map(String::from))));
        }
        if let (Some(id), None) = (app_id, &project) {
            if let Some(a) = self.list.read().unwrap().iter().find(|a| a.id == id) {
                if let Some((h, p)) = self.app_project(lab, a) {
                    host = Some(h);
                    project = Some(p);
                }
            }
        }
        match (host, project) {
            (Some(h), Some(p)) => with_key(&format!("project:{h}/{p}")),
            _ => None,
        }
    }

    pub fn import_homepage(&self, path: &str) -> Result<usize> {
        if path.is_empty() || !Path::new(path).exists() {
            return Ok(0);
        }
        let doc: Value = serde_yaml::from_str(&std::fs::read_to_string(path)?)?;
        let removed = self.removed.read().unwrap().clone();
        let mut existing: HashMap<String, App> = self.list.read().unwrap().iter().map(|a| (a.id.clone(), a.clone())).collect();
        let mut next = Vec::new();
        let mut order = 0.0;
        for group_obj in doc.as_array().into_iter().flatten() {
            let Some((group, items)) = group_obj.as_object().and_then(|o| o.iter().next()) else { continue };
            if group == "Station GPU" {
                continue;
            }
            for item in items.as_array().into_iter().flatten() {
                let Some((name, cfg)) = item.as_object().and_then(|o| o.iter().next()) else { continue };
                let Some(href) = cfg["href"].as_str() else { continue };
                let id = slug(name);
                if removed.contains(&id) {
                    existing.remove(&id);
                    continue;
                }
                let prev = existing.remove(&id);
                let desc = cfg["description"].as_str().unwrap_or("").to_string();
                next.push(App {
                    id,
                    name: name.clone(),
                    group: group.clone(),
                    href: href.into(),
                    ping: cfg["ping"].as_str().unwrap_or("").into(),
                    icon: cfg["icon"].as_str().unwrap_or("").into(),
                    host: host_from_url(href),
                    hidden: prev.as_ref().map(|p| p.hidden).unwrap_or_else(|| desc.to_uppercase().contains("DEACTIVATED")),
                    pinned: prev.as_ref().map(|p| p.pinned).unwrap_or(false),
                    description: desc,
                    order,
                    source: "homepage".into(),
                    ..Default::default()
                });
                order += 1.0;
            }
        }
        next.extend(existing.into_values().filter(|a| a.source != "homepage"));
        let n = next.len();
        *self.list.write().unwrap() = next;
        self.save();
        Ok(n)
    }

    pub fn health_of(&self, id: &str) -> Value {
        self.health.read().unwrap().get(id).map(|h| serde_json::to_value(h).unwrap()).unwrap_or_else(|| json!({"state": "pending"}))
    }

    pub fn state_of(&self, id: &str) -> &'static str {
        self.health.read().unwrap().get(id).map(|h| h.state).unwrap_or("pending")
    }
}

pub async fn check_app(lab: &Lab, a: &App) {
    let now = now_ms();
    if a.ping.is_empty() {
        lab.apps.health.write().unwrap().insert(a.id.clone(), Health { state: "unknown", checked_at: now, ..Default::default() });
        return;
    }
    let prev_fails = lab.apps.health.read().unwrap().get(&a.id).map(|h| h.fails).unwrap_or(0);
    let t0 = std::time::Instant::now();
    let client = &lab.http_noredirect();
    let r = client.get(&a.ping).timeout(std::time::Duration::from_secs(6)).send().await;
    let h = match r {
        Ok(resp) => {
            let ms = t0.elapsed().as_millis() as u64;
            let code = resp.status().as_u16();
            let ok = code < 500;
            lab.metrics.record("apps", "app_up", &a.id, now as i64, Some(if ok { 1.0 } else { 0.0 }));
            lab.metrics.record("apps", "app_ms", &a.id, now as i64, Some(ms as f64));
            Health { state: if ok { if ms > 3000 { "degraded" } else { "up" } } else if a.on_demand { "sleeping" } else { "down" }, ms: Some(ms), code: Some(code), error: None, checked_at: now, fails: if ok { 0 } else { prev_fails + 1 } }
        }
        Err(e) => {
            lab.metrics.record("apps", "app_up", &a.id, now as i64, Some(0.0));
            let err = if e.is_timeout() { "TimeoutError".to_string() } else if e.is_connect() { "ECONNREFUSED".to_string() } else { "FetchError".to_string() };
            Health { state: if a.on_demand { "sleeping" } else { "down" }, ms: None, code: None, error: Some(err), checked_at: now, fails: prev_fails + 1 }
        }
    };
    lab.apps.health.write().unwrap().insert(a.id.clone(), h);
}

async fn check_all(lab: &Lab) {
    let list: Vec<App> = lab.apps.list.read().unwrap().iter().filter(|a| !a.hidden).cloned().collect();
    for chunk in list.chunks(8) {
        futures_util::future::join_all(chunk.iter().map(|a| check_app(lab, a))).await;
    }
}

/// Tile-health alerts (mass outage = OG-Host can't reach them, not N separate outages).
pub fn app_alerts(lab: &Lab, hosts: &[Value]) -> Vec<Value> {
    let mut list = Vec::new();
    let offline: Vec<&str> = hosts.iter().filter(|h| h["online"] == false).filter_map(|h| h["name"].as_str()).collect();
    let apps = lab.apps.list.read().unwrap().clone();
    let health = lab.apps.health.read().unwrap();
    let checked: Vec<&App> = apps.iter().filter(|a| !a.hidden && !a.ping.is_empty() && health.get(&a.id).is_some_and(|h| h.state != "pending")).collect();
    let failing = checked.iter().filter(|a| !a.on_demand && health.get(&a.id).is_some_and(|h| h.state == "down")).count();
    let mass = checked.len() >= 8 && failing as f64 / checked.len() as f64 >= 0.5;
    if mass {
        list.push(json!({"sev": "crit", "key": "checks:mass", "title": format!("OG-Host can't reach {failing} of {} apps", checked.len()), "detail": "Almost certainly a network/DNS problem on this server rather than real outages"}));
    }
    for a in &apps {
        if a.hidden || a.on_demand || mass || offline.contains(&a.host.as_str()) {
            continue;
        }
        if let Some(h) = health.get(&a.id) {
            if h.state == "down" && h.fails >= 2 && lab.apps.known_for(lab, Some(&a.id), None, None, None).is_none() {
                let detail = h.code.map(|c| format!("HTTP {c}")).or_else(|| h.error.clone()).unwrap_or_else(|| "no response".into());
                list.push(json!({"sev": "crit", "key": format!("app:{}", a.id), "title": format!("{} is not responding", a.name), "detail": detail, "appId": a.id}));
            }
        }
    }
    list
}

// ---- icon proxy: tile icons and catalog logos, fetched once, served same-origin ----

fn icon_url(src: &str) -> Option<String> {
    if let Some(u) = src.strip_prefix("https://") {
        let host = u.split('/').next().unwrap_or("");
        let private = host.parse::<std::net::IpAddr>().is_ok() || host == "localhost" || host.ends_with(".local") || host.ends_with(".ts.net") || !host.contains('.');
        return (!private).then(|| src.to_string());
    }
    let name = src.trim();
    if name.is_empty() || name.len() > 80 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return None;
    }
    // Material Design Icons by name (OmegaEnv tiles use "mdi-<name>")
    if let Some(m) = name.strip_prefix("mdi-") {
        return Some(format!("https://cdn.jsdelivr.net/npm/@mdi/svg@7.4.47/svg/{m}.svg"));
    }
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) if ["png", "svg", "webp"].contains(&e) => (b, e),
        _ => (name, "png"),
    };
    Some(format!("https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons/{ext}/{base}.{ext}"))
}

/// (content type, bytes) for an icon name or https URL; cached in `<lab>/icons`.
fn public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified() || v.is_broadcast() || v.is_multicast() || (o[0] == 100 && (64..128).contains(&o[1])) || o[0] == 0)
        }
        std::net::IpAddr::V6(v) => !(v.is_loopback() || v.is_unspecified() || v.is_multicast() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80),
    }
}

/// The host of an https URL resolves only to public addresses (no fetching into the LAN or tailnet).
async fn resolves_public(url: &str) -> bool {
    let Some(host) = url.strip_prefix("https://").and_then(|u| u.split('/').next()) else { return false };
    match tokio::net::lookup_host((host, 443)).await {
        Ok(addrs) => {
            let a: Vec<_> = addrs.collect();
            !a.is_empty() && a.iter().all(|x| public_ip(x.ip()))
        }
        Err(_) => false,
    }
}

pub async fn icon(lab: &Lab, src: &str) -> Option<(String, Vec<u8>)> {
    use sha2::Digest;
    let url = icon_url(src)?;
    let key: String = sha2::Sha256::digest(url.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect();
    let dir = lab.dir.join("icons");
    let path = dir.join(&key);
    if let (Ok(b), Ok(t)) = (std::fs::read(&path), std::fs::read_to_string(path.with_extension("type"))) {
        return (!b.is_empty()).then_some((t, b));
    }
    if !resolves_public(&url).await {
        return None;
    }
    let resp = lab.http_noredirect().get(&url).timeout(std::time::Duration::from_secs(10)).send().await.ok()?;
    let ok = resp.status().is_success();
    let ct = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let _ = std::fs::create_dir_all(&dir);
    if !ok || !ct.starts_with("image/") || resp.content_length().unwrap_or(0) > 1_000_000 {
        let _ = std::fs::write(&path, b""); // remember the miss
        let _ = std::fs::write(path.with_extension("type"), "");
        return None;
    }
    let body = resp.bytes().await.ok()?;
    if body.len() > 1_000_000 {
        return None;
    }
    // SVGs are served with a strict CSP by the caller, so scripts inside can't run
    let _ = std::fs::write(&path, &body);
    let _ = std::fs::write(path.with_extension("type"), &ct);
    Some((ct, body.to_vec()))
}

pub fn start(lab: Arc<Lab>) {
    tokio::spawn(async move {
        if lab.apps.list.read().unwrap().is_empty() {
            let _ = lab.apps.import_homepage(&lab.cfg.homepage_services);
        }
        loop {
            check_all(&lab).await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_hosts_icons() {
        assert_eq!(slug("Home Assistant (Prod)"), "home-assistant-prod");
        assert_eq!(host_from_url("http://100.117.29.92:8080/x"), "go");
        assert_eq!(host_from_url("https://jellyfin.tail6c6721.ts.net"), "jellyfin");
        assert_eq!(icon_url("jellyfin.png").unwrap(), "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons/png/jellyfin.png");
        assert_eq!(icon_url("immich").unwrap(), "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons/png/immich.png");
        assert!(icon_url("../../etc/passwd").is_none());
        assert!(icon_url("https://192.168.0.1/x.png").is_none());
        assert!(icon_url("https://go.tail6c6721.ts.net/x.png").is_none());
        assert!(icon_url("http://example.com/x.png").is_none());
        assert!(icon_url("https://raw.githubusercontent.com/x/y.png").is_some());
        assert_eq!(icon_url("mdi-pokeball").unwrap(), "https://cdn.jsdelivr.net/npm/@mdi/svg@7.4.47/svg/pokeball.svg");
    }
}
