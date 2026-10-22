//! Machines: snapshots pushed by omega-agents (and the local node), the host tree, and alerts.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};

use crate::{fmt_bytes, now_ms, Lab};

const HISTORY_POINTS: usize = 360; // 1 h at 10 s
const HOST_STALE_MS: u64 = 45_000;

pub struct HostState {
    pub latest: Value,
    pub history: VecDeque<Value>,
    pub seen_at: u64,
}

pub struct Fleet {
    dir: PathBuf,
    pub hosts: RwLock<HashMap<String, HostState>>,
    /// hosts.json: display overrides { name: { parent, label, order, console, guest } }
    pub meta: RwLock<Value>,
    /// checks.json: expected services/containers, exit node, egress country, quiet logs
    pub checks: RwLock<Value>,
    /// per host|source: (minute, error count) for the log_errors metric
    log_err: Mutex<HashMap<(String, String), (i64, u32)>>,
}

impl Fleet {
    pub fn load(dir: &Path) -> Fleet {
        Fleet {
            dir: dir.to_path_buf(),
            hosts: RwLock::new(HashMap::new()),
            meta: RwLock::new(crate::store::load(dir, "hosts.json").unwrap_or_else(|| json!({}))),
            checks: RwLock::new(crate::store::load(dir, "checks.json").unwrap_or_else(|| json!({"services": {}, "containers": {}, "exitNode": [], "egress": {}}))),
            log_err: Mutex::new(HashMap::new()),
        }
    }

    pub fn save_checks(&self, v: Value) {
        crate::store::save(&self.dir, "checks.json", &v);
        *self.checks.write().unwrap() = v;
    }

    pub fn ingest(&self, metrics: &crate::metrics::Metrics, snap: Value, slow_fresh_hint: bool) {
        let Some(name) = snap["host"].as_str().map(String::from) else { return };
        let mut hosts = self.hosts.write().unwrap();
        let slow_fresh = match snap.get("slowAt") {
            Some(v) if !v.is_null() => hosts.get(&name).is_none_or(|h| h.latest.get("slowAt") != Some(v)),
            _ => slow_fresh_hint,
        };
        metrics.ingest_snapshot(&snap, slow_fresh);
        let h = hosts.entry(name).or_insert_with(|| HostState { latest: Value::Null, history: VecDeque::new(), seen_at: 0 });
        let f = |v: &Value| v.as_f64();
        let mem = match (f(&snap["mem"]["total"]), f(&snap["mem"]["available"])) {
            (Some(t), Some(a)) if t > 0.0 => Some(((1000.0 * (t - a) / t).round()) / 10.0),
            _ => None,
        };
        let g0 = &snap["gpus"][0];
        let vram = match (f(&g0["memUsed"]), f(&g0["memTotal"])) {
            (Some(u), Some(t)) if t > 0.0 => Some(((1000.0 * u / t).round()) / 10.0),
            _ => None,
        };
        h.history.push_back(json!({"t": now_ms(), "cpu": snap["cpu"]["pct"], "mem": mem, "gpu": g0["util"], "vram": vram, "rx": snap["net"]["rxBps"], "tx": snap["net"]["txBps"]}));
        while h.history.len() > HISTORY_POINTS {
            h.history.pop_front();
        }
        h.latest = snap;
        h.seen_at = now_ms();
    }

    pub fn count_log_errors(&self, metrics: &crate::metrics::Metrics, host: &str, lines: &[Value]) {
        let mut m = self.log_err.lock().unwrap();
        for l in lines {
            if l[3].as_str() != Some("error") {
                continue;
            }
            let (Some(ts), Some(source)) = (l[0].as_f64(), l[2].as_str()) else { continue };
            let minute = (ts as i64) / 60_000 * 60_000;
            let k = (host.to_string(), source.to_string());
            if let Some((mm, n)) = m.get(&k).copied() {
                if mm != minute {
                    metrics.record(host, "log_errors", source, mm, Some(n as f64));
                    m.remove(&k);
                }
            }
            let e = m.entry(k).or_insert((minute, 0));
            e.1 += 1;
        }
    }

    fn flush_log_errors(&self, metrics: &crate::metrics::Metrics) {
        let now = now_ms() as i64 / 60_000 * 60_000;
        let mut m = self.log_err.lock().unwrap();
        m.retain(|(h, s), (minute, n)| {
            if *minute < now {
                metrics.record(h, "log_errors", s, *minute, Some(*n as f64));
                false
            } else {
                true
            }
        });
    }

    /// Every host as the UI and alerts see it: latest snapshot + name, label, console, parent, online, history.
    pub fn view(&self) -> Vec<Value> {
        let hosts = self.hosts.read().unwrap();
        let meta = self.meta.read().unwrap();
        // proxmox agents report their guests; a guest name that matches an agent is that host's parent
        let mut parents: HashMap<String, String> = HashMap::new();
        for (name, h) in hosts.iter() {
            for g in h.latest["guests"].as_array().into_iter().flatten() {
                if let Some(gn) = g["name"].as_str() {
                    parents.insert(gn.to_string(), name.clone());
                }
            }
        }
        let guest_to_host: HashMap<String, String> = meta.as_object().into_iter().flatten().filter_map(|(n, m)| m["guest"].as_str().map(|g| (g.to_string(), n.clone()))).collect();
        let mut out: Vec<Value> = hosts
            .iter()
            .map(|(name, h)| {
                let mut v = if h.latest.is_object() { h.latest.clone() } else { json!({}) };
                let m = &meta[name];
                let online = now_ms().saturating_sub(h.seen_at) < HOST_STALE_MS;
                let guests: Vec<Value> = h.latest["guests"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|g| {
                        let mut g = g.clone();
                        let gn = g["name"].as_str().unwrap_or("").to_string();
                        g["host"] = guest_to_host.get(&gn).cloned().map(Value::String).unwrap_or_else(|| if hosts.contains_key(&gn) { Value::String(gn) } else { Value::Null });
                        g
                    })
                    .collect();
                let parent = m["parent"].as_str().map(String::from).or_else(|| parents.get(name).cloned()).or_else(|| h.latest["parent"].as_str().map(String::from));
                let o = v.as_object_mut().unwrap();
                o.insert("name".into(), json!(name));
                o.insert("label".into(), json!(m["label"].as_str().unwrap_or(name)));
                o.insert("console".into(), m["console"].clone());
                o.insert("parent".into(), json!(parent));
                o.insert("online".into(), json!(online));
                o.insert("seenAt".into(), json!(h.seen_at));
                o.insert("guests".into(), json!(guests));
                o.insert("history".into(), json!(h.history));
                v
            })
            .collect();
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out
    }

    pub fn latest(&self, host: &str) -> Option<Value> {
        self.hosts.read().unwrap().get(host).map(|h| h.latest.clone())
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.hosts.read().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
}

fn alert(sev: &str, key: String, title: String, detail: String) -> Value {
    json!({"sev": sev, "key": key, "title": title, "detail": detail})
}

/// Everything that needs attention, most severe first (OmegaEnv's rules).
pub fn alerts(lab: &Lab) -> Vec<Value> {
    let mut list: Vec<Value> = Vec::new();
    let hosts = lab.fleet.view();
    let checks = lab.fleet.checks.read().unwrap().clone();
    let now = now_ms();
    for h in &hosts {
        let name = h["name"].as_str().unwrap_or("");
        if !h["online"].as_bool().unwrap_or(false) {
            let age = now.saturating_sub(h["seenAt"].as_u64().unwrap_or(0)) / 1000;
            list.push(alert("crit", format!("host:{name}"), format!("{name} is offline"), format!("No report for {age} s")));
            continue;
        }
        for d in h["disks"].as_array().into_iter().flatten() {
            let (Some(used), Some(total)) = (d["used"].as_f64(), d["total"].as_f64()) else { continue };
            let free = d["avail"].as_f64().unwrap_or(total - used);
            let pct = 100.0 * used / (used + free).max(1.0);
            let mount = d["mount"].as_str().unwrap_or("");
            if pct >= 95.0 {
                list.push(alert("crit", format!("disk:{name}:{mount}"), format!("{mount} on {name} is almost full"), format!("{pct:.0}% used, {} left", fmt_bytes(free))));
            } else if pct >= 85.0 {
                list.push(alert("warn", format!("disk:{name}:{mount}"), format!("{mount} on {name} is filling up"), format!("{pct:.0}% used, {} left", fmt_bytes(free))));
            }
        }
        let mem = &h["mem"];
        if let (Some(st), Some(sf)) = (mem["swapTotal"].as_f64(), mem["swapFree"].as_f64()) {
            if st > 0.0 && (st - sf) / st > 0.9 {
                list.push(alert("warn", format!("swap:{name}"), format!("{name} has run out of swap"), format!("{} of {} swap used — it needs more RAM or fewer apps", fmt_bytes(st - sf), fmt_bytes(st))));
            }
        }
        if let (Some(t), Some(a)) = (mem["total"].as_f64(), mem["available"].as_f64()) {
            if t > 0.0 && (t - a) / t > 0.92 {
                list.push(alert("warn", format!("mem:{name}"), format!("{name} is low on memory"), format!("{} available of {}", fmt_bytes(a), fmt_bytes(t))));
            }
        }
        if let Some(e) = h["gpuError"].as_str() {
            list.push(alert("crit", format!("gpu-missing:{name}"), format!("The GPU on {name} is not responding"), format!("{e} — usually needs a full power-off of the host")));
        }
        for g in h["gpus"].as_array().into_iter().flatten() {
            if g["temp"].as_f64().unwrap_or(0.0) >= 83.0 {
                list.push(alert("warn", format!("gpu:{name}"), format!("{} on {name} is hot", g["name"].as_str().unwrap_or("GPU")), format!("{} °C, {} W of {} W", g["temp"], g["power"].as_f64().unwrap_or(0.0).round(), g["powerLimit"].as_f64().unwrap_or(0.0).round())));
            }
        }
        if let Some(x) = lab.logs.recent_matching(name, "kernel", "NVRM: Xid", 15) {
            list.push(alert("crit", format!("xid:{name}"), format!("GPU error on {name}"), x.chars().take(160).collect()));
        }
        let gaming = h["gpuMode"].as_str() == Some("gaming");
        let gaming_stops: Vec<String> = checks["gamingStops"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_else(|| vec!["llama-server".into()]);
        if let Some(svc) = h["services"].as_object() {
            for n in checks["services"][name].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
                if gaming && gaming_stops.iter().any(|g| g == n) {
                    continue;
                }
                let st = svc.get(n).or_else(|| svc.get(&format!("user:{n}"))).and_then(|v| v.as_str());
                if st != Some("active") {
                    list.push(alert("crit", format!("svc:{name}:{n}"), format!("{n} on {name} is not running"), st.map(|s| format!("service state: {s}")).unwrap_or_else(|| "service not found on this machine".into())));
                }
            }
        }
        let ctrs = h["containers"].as_array().cloned().unwrap_or_default();
        if h.get("containers").is_some() {
            for n in checks["containers"][name].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
                let c = ctrs.iter().find(|c| c["name"] == n);
                if c.is_some_and(|c| c["state"] == "running") || lab.apps.known_for(lab, None, Some(name), None, Some(n)).is_some() {
                    continue;
                }
                list.push(alert("crit", format!("ctr-down:{name}:{n}"), format!("{n} on {name} is not running"), c.map(|c| format!("{} · {}", c["state"].as_str().unwrap_or(""), c["status"].as_str().unwrap_or(""))).unwrap_or_else(|| "container is missing".into())));
            }
        }
        let net = &h["network"];
        if checks["exitNode"].as_array().is_some_and(|a| a.iter().any(|x| x == name)) && net["exitNode"] == false {
            list.push(alert("warn", format!("exitnode:{name}"), format!("{name} is not routing through the exit node"), "Tailscale reports no active exit node".into()));
        }
        if let Some(want) = checks["egress"][name].as_str() {
            if net.get("egressCountry").is_some() {
                match net["egressCountry"].as_str() {
                    Some(c) if c != want => list.push(alert("crit", format!("egress:{name}"), format!("VPN leak on {name}: traffic exits in {c}"), format!("expected {want} — the VPN is being bypassed"))),
                    None => list.push(alert("warn", format!("egress:{name}"), format!("Could not confirm the VPN exit country on {name}"), "the country lookup failed twice — inconclusive, not a confirmed leak".into())),
                    _ => {}
                }
            }
        }
        for c in &ctrs {
            let cn = c["name"].as_str().unwrap_or("");
            if lab.apps.known_for(lab, None, Some(name), None, Some(cn)).is_some() {
                continue;
            }
            let status = c["status"].as_str().unwrap_or("").to_lowercase();
            if status.contains("unhealthy") {
                list.push(alert("warn", format!("ctr:{name}:{cn}"), format!("{cn} is unhealthy"), format!("on {name} · {}", c["status"].as_str().unwrap_or(""))));
            }
            if c["state"].as_str().is_some_and(|s| s.to_lowercase().contains("restarting")) {
                list.push(alert("crit", format!("ctr:{name}:{cn}"), format!("{cn} keeps restarting"), format!("on {name}")));
            }
        }
    }
    list.extend(crate::apps::app_alerts(lab, &hosts));
    let quiet: Vec<String> = checks["quietLogs"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_else(|| vec!["^Wolf[A-Za-z-]+_".into()]);
    for n in lab.logs.noisy(15, 10) {
        let (host, kind, source) = (n["host"].as_str().unwrap_or(""), n["kind"].as_str().unwrap_or(""), n["source"].as_str().unwrap_or(""));
        if kind == "container" && lab.apps.known_for(lab, None, Some(host), None, Some(source)).is_some() {
            continue;
        }
        if quiet.iter().any(|q| simple_match(q, source)) {
            continue;
        }
        let last: String = n["lastError"]["msg"].as_str().unwrap_or("").chars().take(90).collect();
        let mut a = alert("warn", format!("log:{host}:{source}"), format!("{source} on {host} is logging errors"), format!("{} errors in 15 min — {last}", n["errors"]));
        a["quiet"] = json!(true);
        a["log"] = json!({"host": host, "kind": kind, "source": source});
        list.push(a);
    }
    for s in lab.jobs.queue.lock().unwrap().iter().filter(|s| s.status == "pending") {
        let what = match s.action.as_str() {
            "op" => s.op.clone().unwrap_or_default(),
            "edit" => "edit compose of".into(),
            "remove" => "remove".into(),
            _ => "install".into(),
        };
        let mut a = alert("info", format!("stack:{}", s.id), format!("Waiting for your approval: {what} {}", s.name), format!("on {} — review in Apps", s.host));
        a["stackId"] = json!(s.id);
        list.push(a);
    }
    let rank = |s: &str| match s {
        "crit" => 0,
        "warn" => 1,
        _ => 2,
    };
    list.sort_by_key(|a| rank(a["sev"].as_str().unwrap_or("")));
    list
}

/// The few regex forms used in checks.quietLogs: `^prefix`, `suffix$`, `[A-Za-z-]+` classes.
/// Anything fancier falls back to a substring match of its literal characters.
pub fn simple_match(pat: &str, s: &str) -> bool {
    fn m(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return true;
        }
        if p[0] == b'$' && p.len() == 1 {
            return s.is_empty();
        }
        if p[0] == b'[' {
            if let Some(end) = p.iter().position(|c| *c == b']') {
                let class = &p[1..end];
                let ok = |c: u8| {
                    let mut i = 0;
                    while i < class.len() {
                        if i + 2 < class.len() && class[i + 1] == b'-' {
                            if c >= class[i] && c <= class[i + 2] {
                                return true;
                            }
                            i += 3;
                        } else {
                            if class[i] == c {
                                return true;
                            }
                            i += 1;
                        }
                    }
                    false
                };
                let rest = &p[end + 1..];
                if rest.first() == Some(&b'+') {
                    let mut i = 0;
                    while i < s.len() && ok(s[i]) {
                        i += 1;
                        if m(&rest[1..], &s[i..]) {
                            return true;
                        }
                    }
                    return false;
                }
                return !s.is_empty() && ok(s[0]) && m(rest, &s[1..]);
            }
        }
        !s.is_empty() && (p[0] == b'.' || p[0] == s[0]) && m(&p[1..], &s[1..])
    }
    let (anchored, p) = match pat.strip_prefix('^') {
        Some(rest) => (true, rest),
        None => (false, pat),
    };
    if anchored {
        return m(p.as_bytes(), s.as_bytes());
    }
    (0..=s.len()).any(|i| m(p.as_bytes(), &s.as_bytes()[i..]))
}

pub fn start(lab: Arc<Lab>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            lab.fleet.flush_log_errors(&lab.metrics);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::simple_match;

    #[test]
    fn quiet_log_patterns() {
        assert!(simple_match("^Wolf[A-Za-z-]+_", "WolfSteam_1234"));
        assert!(simple_match("^Wolf[A-Za-z-]+_", "WolfPulseAudio-x_9"));
        assert!(!simple_match("^Wolf[A-Za-z-]+_", "MyWolfSteam_1"));
        assert!(simple_match("Steam", "WolfSteam"));
        assert!(!simple_match("steam", "WolfSteam"));
        assert!(!simple_match("steam$", "steamy"));
    }
}
