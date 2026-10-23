//! Report to an OmegaEnv core (homelab dashboard) natively, instead of running its Python
//! agent on the node: the same snapshot every 10 s (the one the built-in lab uses), the jobs it
//! hands back (deploy/remove/compose on stacks), container output from the logs og-hostd keeps
//! anyway, and the systemd journal.
//!
//! Configured by `<root>/omegaenv.env` or the agent's `/etc/omegaenv/agent.env`
//! (OMEGA_URL, OMEGA_TOKEN, optional OMEGA_HOST). Without either it does nothing.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::serve::Daemon;

struct Conf {
    url: String,
    token: String,
    host: Option<String>,
}

fn conf(root: &Path) -> Option<Conf> {
    let text = std::fs::read_to_string(root.join("omegaenv.env")).or_else(|_| std::fs::read_to_string("/etc/omegaenv/agent.env")).ok()?;
    let mut kv = HashMap::new();
    for l in text.lines() {
        if let Some((k, v)) = l.split_once('=') {
            kv.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
        }
    }
    Some(Conf { url: kv.get("OMEGA_URL")?.trim_end_matches('/').to_string(), token: kv.get("OMEGA_TOKEN")?.clone(), host: kv.get("OMEGA_HOST").cloned() })
}

/// Whether this node reports natively (then the Python agent isn't needed).
pub fn configured(root: &Path) -> bool {
    std::env::var("OG_OMEGAENV").as_deref() != Ok("0") && conf(root).is_some()
}

pub async fn run(d: Arc<Daemon>) {
    let Some(c) = conf(&d.root).filter(|_| configured(&d.root)) else { return };
    let http = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
        Ok(h) => h,
        Err(_) => return,
    };
    let node = crate::labnode::LabNode { d: d.clone() };
    let host = c.host.clone().unwrap_or_else(|| og_lab::LocalNode::name(&node));
    eprintln!("og-hostd: reporting to OmegaEnv at {} as {host}", c.url);
    let c = Arc::new(c);
    // logs: every 2 s
    {
        let (d, c, http, host) = (d.clone(), c.clone(), http.clone(), host.clone());
        tokio::spawn(async move {
            let mut offsets: HashMap<String, u64> = HashMap::new();
            let mut cursor: Option<String> = None;
            let mut tick = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                tick += 1;
                let (d2, mut offs, cur) = (d.clone(), std::mem::take(&mut offsets), cursor.clone());
                let (lines, offs, cur) = tokio::task::spawn_blocking(move || {
                    let mut lines = container_lines(&d2, &mut offs);
                    let cur = if tick % 5 == 0 { journal_lines(cur, &mut lines) } else { cur };
                    (lines, offs, cur)
                })
                .await
                .unwrap_or_default();
                offsets = offs;
                cursor = cur;
                if lines.is_empty() {
                    continue;
                }
                let _ = http.post(format!("{}/api/logs/ingest", c.url)).bearer_auth(&c.token).json(&json!({"host": host, "lines": lines})).send().await;
            }
        });
    }
    // snapshot + jobs: every 10 s
    loop {
        let mut snap = og_lab::LocalNode::snapshot(&node);
        snap["host"] = json!(host);
        snap["ts"] = json!(og_lab::now_ms());
        snap["guests"] = guests(&d);
        match http.post(format!("{}/api/ingest", c.url)).bearer_auth(&c.token).json(&snap).send().await {
            Ok(r) => {
                if let Ok(v) = r.json::<Value>().await {
                    for job in v["jobs"].as_array().cloned().unwrap_or_default() {
                        let (node, http, c) = (crate::labnode::LabNode { d: d.clone() }, http.clone(), c.clone());
                        tokio::spawn(async move {
                            let id = job["id"].clone();
                            let (ok, log, _) = og_lab::LocalNode::run_job(&node, job).await;
                            let _ = http.post(format!("{}/api/jobs/result", c.url)).bearer_auth(&c.token).json(&json!({"id": id, "ok": ok, "log": log})).send().await;
                        });
                    }
                }
            }
            Err(e) => eprintln!("og-hostd: OmegaEnv push failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// VMs in the agent's guest format.
fn guests(d: &Daemon) -> Value {
    json!(d
        .vms
        .list()
        .into_iter()
        .filter(|v| !v.template)
        .map(|v| {
            let st = d.vms.status(&v.name);
            let rss = st.pid.and_then(crate::vmrun::usage).map(|u| u.1);
            json!({"vmid": null, "name": v.name, "kind": "qemu", "status": st.state, "cpu": null, "cpus": v.cores, "mem": rss, "maxmem": v.memory as u64 * 1048576, "uptime": null})
        })
        .collect::<Vec<_>>())
}

fn container_lines(d: &Daemon, offs: &mut HashMap<String, u64>) -> Vec<Value> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(list) = d.node.list() else { return vec![] };
    let mut lines = Vec::new();
    let ids: Vec<String> = list.iter().map(|(m, _)| m.id.clone()).collect();
    offs.retain(|k, _| ids.contains(k));
    for (m, _) in &list {
        let path = d.node.log_path(&m.id);
        let len = std::fs::metadata(&path).map(|x| x.len()).unwrap_or(0);
        // first sight (or after a restart of og-hostd): only what comes from now on
        let off = offs.entry(m.id.clone()).or_insert(len);
        if len < *off {
            *off = 0;
        }
        if len == *off {
            continue;
        }
        let Ok(mut f) = std::fs::File::open(&path) else { continue };
        let _ = f.seek(SeekFrom::Start(*off));
        let mut buf = Vec::new();
        let _ = f.take(1 << 20).read_to_end(&mut buf);
        let cut = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
        *off += cut as u64;
        let now = og_lab::now_ms();
        for (n, l) in String::from_utf8_lossy(&buf[..cut]).lines().filter(|l| !l.trim().is_empty()).enumerate() {
            if n >= 400 {
                lines.push(json!([now, "container", m.name, "warn", "[og-hostd] more than 400 lines in 2 s; the rest is in og-hostd logs"]));
                break;
            }
            lines.push(json!([now, "container", m.name, og_lab::logs::guess_level(l), l.chars().take(4000).collect::<String>()]));
        }
    }
    lines
}

/// Journal entries (priority info and up) since `cursor`; returns the new cursor.
fn journal_lines(cursor: Option<String>, out: &mut Vec<Value>) -> Option<String> {
    let mut cmd = std::process::Command::new("journalctl");
    cmd.args(["-o", "json", "--no-pager", "-p", "6", "-n", "2000"]);
    match &cursor {
        Some(c) => cmd.arg(format!("--after-cursor={c}")),
        None => cmd.arg("--since=-10s"),
    };
    let o = cmd.output().ok()?;
    let mut last = cursor;
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let Ok(e) = serde_json::from_str::<Value>(l) else { continue };
        last = e["__CURSOR"].as_str().map(String::from).or(last);
        let msg = match &e["MESSAGE"] {
            Value::String(s) => s.clone(),
            Value::Array(b) => String::from_utf8_lossy(&b.iter().filter_map(|x| x.as_u64().map(|v| v as u8)).collect::<Vec<_>>()).into_owned(),
            _ => continue,
        };
        let unit = e["_SYSTEMD_UNIT"].as_str().or(e["SYSLOG_IDENTIFIER"].as_str()).unwrap_or("system").trim_end_matches(".service").to_string();
        // our own lines (container output is shipped from the logs above) and ssh sessions
        if unit == "og-hostd" && msg.contains("container") || unit.starts_with("session-") || unit == "init.scope" {
            continue;
        }
        let prio: u8 = e["PRIORITY"].as_str().and_then(|p| p.parse().ok()).unwrap_or(6);
        let level = if prio <= 3 { "error" } else if prio == 4 { "warn" } else { og_lab::logs::guess_level(&msg) };
        let ts = e["__REALTIME_TIMESTAMP"].as_str().and_then(|t| t.parse::<u64>().ok()).map(|us| us / 1000).unwrap_or_else(og_lab::now_ms);
        for part in msg.lines().filter(|p| !p.trim().is_empty()) {
            out.push(json!([ts, "unit", unit, level, part.chars().take(4000).collect::<String>()]));
        }
    }
    last
}
