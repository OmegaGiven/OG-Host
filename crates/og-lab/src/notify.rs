//! Signal notifications (through signal-cli's JSON-RPC on another machine, reached over ssh,
//! as OmegaEnv does). Policy: critical after 2 min, warnings after 15 min, approvals right
//! away; repeat at most every 6 h (crit) / 24 h; say when it clears; report outages that
//! ended before a message could go out.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::{now_ms, Lab};

pub struct Notifier {
    dir: PathBuf,
    state: Mutex<Map<String, Value>>,
}

fn delay(sev: &str) -> u64 {
    match sev {
        "crit" => 2 * 60_000,
        "warn" => 15 * 60_000,
        _ => 0,
    }
}

fn repeat(sev: &str) -> u64 {
    match sev {
        "crit" => 6 * 3_600_000,
        _ => 24 * 3_600_000,
    }
}

fn hhmm(ms: u64) -> String {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn minutes(ms: u64) -> String {
    format!("{} min", ((ms as f64) / 60_000.0).round().max(1.0))
}

impl Notifier {
    pub fn load(dir: &Path) -> Notifier {
        Notifier { dir: dir.to_path_buf(), state: Mutex::new(crate::store::load(dir, "notify.json").unwrap_or_default()) }
    }
}

/// Send one Signal message; Ok(()) when signal-cli reports SUCCESS.
pub async fn send_signal(lab: &Lab, text: &str) -> Result<(), String> {
    let c = &lab.cfg;
    if c.signal_ssh.is_empty() || c.signal_to.is_empty() {
        return Err("Signal not configured".into());
    }
    let url = if c.signal_url.is_empty() { "http://127.0.0.1:8088/api/v1/rpc" } else { c.signal_url.as_str() };
    if url.contains(['\'', '"', ' ', ';', '`', '$', '\\']) || c.signal_ssh.starts_with('-') {
        return Err("bad Signal configuration".into());
    }
    let body = json!({"jsonrpc": "2.0", "method": "send", "params": {"recipient": [c.signal_to], "message": text}, "id": now_ms()}).to_string();
    let mut child = tokio::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", &c.signal_ssh, &format!("curl -s -m 30 -X POST '{url}' -H 'Content-Type: application/json' --data-binary @-")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(body.as_bytes()).await;
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output()).await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && text.contains("\"SUCCESS\"") {
        Ok(())
    } else {
        let e: String = format!("{}{}", text, String::from_utf8_lossy(&out.stderr)).chars().take(200).collect();
        eprintln!("og-lab: signal send failed: {e}");
        Err(e)
    }
}

async fn tick(lab: &Arc<Lab>) {
    if lab.cfg.notify == Some(false) {
        return;
    }
    let now = now_ms();
    let current: Vec<Value> = crate::fleet::alerts(lab).into_iter().filter(|a| a["quiet"] != true).collect();
    let mut fire: Vec<Value> = Vec::new();
    let mut cleared: Vec<Value> = Vec::new();
    {
        let mut st = lab.notify.state.lock().unwrap();
        for a in &current {
            let key = a["key"].as_str().unwrap_or("").to_string();
            let sev = a["sev"].as_str().unwrap_or("info").to_string();
            let e = st.entry(key.clone()).or_insert_with(|| json!({"firstSeen": now, "sev": sev}));
            e["title"] = a["title"].clone();
            e["detail"] = a["detail"].clone();
            e["sev"] = json!(sev);
            e["goneSince"] = Value::Null;
            let first = e["firstSeen"].as_u64().unwrap_or(now);
            let sent = e["sentAt"].as_u64();
            if now - first >= delay(&sev) && sent.is_none_or(|s| now - s > repeat(&sev)) {
                fire.push(a.clone());
            }
        }
        let keys: Vec<String> = current.iter().filter_map(|a| a["key"].as_str().map(String::from)).collect();
        let mut gone = Vec::new();
        for (key, e) in st.iter_mut() {
            if keys.contains(key) {
                continue;
            }
            let gs = *e.get("goneSince").and_then(|v| v.as_u64()).get_or_insert(now);
            e["goneSince"] = json!(gs);
            if now - gs < 2 * 60_000 {
                continue; // flapping guard
            }
            let first = e["firstSeen"].as_u64().unwrap_or(gs);
            let lasted = gs - first;
            let sev = e["sev"].as_str().unwrap_or("info");
            let sent = e["sentAt"].as_u64().is_some();
            if sev != "info" && (sent || (sev == "crit" && lasted >= delay("crit") + 3 * 60_000)) {
                let mut c = e.clone();
                c["lasted"] = json!(lasted);
                c["missed"] = json!(!sent);
                cleared.push(c);
            }
            gone.push(key.clone());
        }
        for k in gone {
            st.remove(&k);
        }
    }
    if !fire.is_empty() || !cleared.is_empty() {
        let icon = |s: &str| match s {
            "crit" => "🔴",
            "warn" => "🟠",
            _ => "🟣",
        };
        let st = lab.notify.state.lock().unwrap().clone();
        let mut lines: Vec<String> = fire
            .iter()
            .map(|a| {
                let key = a["key"].as_str().unwrap_or("");
                let e = &st[key];
                let first = e["firstSeen"].as_u64().unwrap_or(now);
                let sev = a["sev"].as_str().unwrap_or("info");
                let late = now - first > delay(sev) + 10 * 60_000 && e["sentAt"].is_null();
                let detail = a["detail"].as_str().filter(|d| !d.is_empty()).map(|d| format!("\n   {d}")).unwrap_or_default();
                format!("{} {}{}{}", icon(sev), a["title"].as_str().unwrap_or(""), detail, if late { format!("\n   (since {})", hhmm(first)) } else { String::new() })
            })
            .collect();
        for c in &cleared {
            let t = c["title"].as_str().unwrap_or("");
            if c["missed"] == true {
                lines.push(format!("⚪ Earlier: {t} for {} ({}–{}), now back", minutes(c["lasted"].as_u64().unwrap_or(0)), hhmm(c["firstSeen"].as_u64().unwrap_or(0)), hhmm(c["goneSince"].as_u64().unwrap_or(0))));
            } else {
                lines.push(format!("✅ Resolved: {t}"));
            }
        }
        let link = if fire.iter().any(|a| a.get("stackId").is_some()) { lab.link("#apps") } else { lab.link("#monitor") };
        if send_signal(lab, &format!("{}\n\n{link}", lines.join("\n"))).await.is_ok() {
            let mut st = lab.notify.state.lock().unwrap();
            for a in &fire {
                if let Some(e) = st.get_mut(a["key"].as_str().unwrap_or("")) {
                    e["sentAt"] = json!(now);
                }
            }
        }
    }
    crate::store::save(&lab.notify.dir, "notify.json", &*lab.notify.state.lock().unwrap());
}

pub fn start(lab: Arc<Lab>) {
    tokio::spawn(async move {
        // let agents report first after a restart
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        loop {
            tick(&lab).await;
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}
