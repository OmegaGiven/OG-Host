//! This OG-Host node as a member of its own lab: an omega-agent-format snapshot every 10 s,
//! compose jobs run directly (no agent), and container logs shipped into the lab log store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::serve::Daemon;

pub struct LabNode {
    pub d: Arc<Daemon>,
}

fn read(p: &str) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn ago(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s} seconds"),
        s if s < 3600 => format!("{} minutes", s / 60),
        s if s < 86400 => format!("{} hours", s / 3600),
        s => format!("{} days", s / 86400),
    }
}

/// Listening TCP ports (IPv4 + IPv6).
fn listening() -> Vec<u16> {
    let mut v: Vec<u16> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .flat_map(|f| read(f).lines().skip(1).map(String::from).collect::<Vec<_>>())
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.get(3) == Some(&"0A")).then(|| f.get(1).and_then(|a| a.rsplit(':').next()).and_then(|p| u16::from_str_radix(p, 16).ok())).flatten()
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

fn cpu_temp() -> Option<f64> {
    let mut best: Option<f64> = None;
    for e in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let p = e.path();
        let name = std::fs::read_to_string(p.join("name")).unwrap_or_default();
        if !["k10temp", "coretemp", "zenpower", "cpu_thermal", "acpitz"].contains(&name.trim()) {
            continue;
        }
        for f in std::fs::read_dir(&p).into_iter().flatten().flatten() {
            let n = f.file_name().to_string_lossy().to_string();
            if n.starts_with("temp") && n.ends_with("_input") {
                if let Some(v) = std::fs::read_to_string(f.path()).ok().and_then(|s| s.trim().parse::<f64>().ok()) {
                    best = Some(best.map_or(v / 1000.0, |b: f64| b.max(v / 1000.0)));
                }
            }
        }
    }
    best.map(|b| (b * 10.0).round() / 10.0)
}

impl LabNode {
    fn containers(&self) -> Vec<Value> {
        let stats: Value = self.d.hub.latest.read().unwrap().get("stats").and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
        let now = crate::containers::unix_now();
        self.d
            .node
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|(m, st)| {
                let s = &stats["stats"]["containers"][&m.id];
                let health = self.d.container_json(&m.id).and_then(|c| c["health"]["status"].as_str().map(String::from));
                let status = if st.state == "running" {
                    format!("Up {}{}", ago(now.saturating_sub(st.started_at.unwrap_or(now))), health.map(|h| format!(" ({h})")).unwrap_or_default())
                } else if st.state == "exited" {
                    format!("Exited ({}) {} ago", st.exit_code.unwrap_or(0), ago(now.saturating_sub(st.finished_at.unwrap_or(now))))
                } else {
                    st.state.clone()
                };
                let ports: Vec<String> = m.net.as_ref().map(|a| a.ports.iter().map(|p| format!("{}:{}->{}/{}", p.host_ip.map(|i| i.to_string()).unwrap_or("0.0.0.0".into()), p.host_port, p.container_port, format!("{:?}", p.proto).to_lowercase())).collect()).unwrap_or_default();
                json!({
                    "name": m.name, "image": m.image, "state": st.state, "status": status, "ports": ports.join(", "),
                    "project": m.project, "service": m.service,
                    "workingDir": m.labels.get("com.docker.compose.project.working_dir"),
                    "configFiles": m.labels.get("com.docker.compose.project.config_files"),
                    "pterodactyl": false, "cpu": s["cpu"], "mem": s["mem"],
                })
            })
            .collect()
    }

    fn projects(&self) -> Vec<Value> {
        crate::stacks::list(&self.d.node, &self.d.root)
            .into_iter()
            .map(|s| {
                let services: Vec<Value> = s.services.iter().map(|v| json!({"service": v.name, "container": v.container_name, "state": v.state, "status": v.state, "image": v.image})).collect();
                json!({"name": s.name, "workingDir": s.file.as_ref().and_then(|f| f.parent()).map(|p| p.display().to_string()), "configFiles": s.file.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(), "services": services, "running": s.running, "protected": false})
            })
            .collect()
    }

    fn services(&self, wanted: &[String]) -> Value {
        let mut out = serde_json::Map::new();
        for n in wanted.iter().filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || "@._-".contains(c))) {
            let st = std::process::Command::new("systemctl").args(["is-active", n]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            if !st.is_empty() && st != "inactive" || std::path::Path::new(&format!("/etc/systemd/system/{n}.service")).exists() || std::path::Path::new(&format!("/lib/systemd/system/{n}.service")).exists() {
                out.insert(n.clone(), json!(st));
            }
        }
        Value::Object(out)
    }
}

impl og_lab::LocalNode for LabNode {
    fn name(&self) -> String {
        crate::stats::node_info(&self.d.root)["hostname"].as_str().unwrap_or("og-host").split('.').next().unwrap_or("og-host").to_lowercase()
    }

    fn snapshot(&self) -> Value {
        let info = crate::stats::node_info(&self.d.root);
        let stats: Value = self.d.hub.latest.read().unwrap().get("stats").and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
        let n = &stats["stats"]["node"];
        let mut swap_total = 0u64;
        let mut swap_free = 0u64;
        for l in read("/proc/meminfo").lines() {
            let v = l.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024;
            if l.starts_with("SwapTotal:") {
                swap_total = v;
            } else if l.starts_with("SwapFree:") {
                swap_free = v;
            }
        }
        let gpus: Vec<Value> = self
            .d
            .brain
            .lock()
            .unwrap()
            .last_gpus
            .iter()
            .map(|g| json!({"name": g.name, "util": g.util_pct, "memUsed": g.mem_used_mib * 1048576, "memTotal": g.mem_total_mib * 1048576, "temp": g.temp_c, "power": g.power_w, "encoderSessions": g.encoder_sessions}))
            .collect();
        let host = og_lab::LocalNode::name(self);
        let wanted: Vec<String> = self.d.lab.as_ref().map(|l| l.fleet.checks.read().unwrap()["services"][&host].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()).unwrap_or_default();
        json!({
            "host": host, "os": info["os"], "kernel": info["kernel"], "agent": format!("og-hostd {}", env!("CARGO_PKG_VERSION")), "ogHost": true,
            "uptime": n["uptime"],
            "cpu": {"pct": n["cpu"].as_f64().map(|c| (c * 10.0).round() / 10.0), "cores": info["cores"], "load": n["load"].as_array().into_iter().flatten().map(|l| l.as_f64().map(|x| (x * 100.0).round() / 100.0)).collect::<Vec<_>>(), "model": info["cpu_model"]},
            "mem": {"total": n["mem"]["total"], "available": n["mem"]["available"], "swapTotal": swap_total, "swapFree": swap_free},
            "disks": n["fs"].as_array().into_iter().flatten().map(|f| json!({"mount": f["mount"], "device": "", "fs": "", "total": f["total"], "used": f["used"], "avail": f["total"].as_u64().unwrap_or(0).saturating_sub(f["used"].as_u64().unwrap_or(0))})).collect::<Vec<_>>(),
            "net": {"iface": "", "rxBps": n["net"]["rx"], "txBps": n["net"]["tx"]},
            "temp": cpu_temp(), "gpus": gpus, "docker": true,
            "containers": self.containers(), "projects": self.projects(), "guests": [],
            "services": self.services(&wanted), "listening": listening(),
            "slowAt": og_lab::now_ms(),
        })
    }

    fn run_job(&self, job: Value) -> futures_util::future::BoxFuture<'static, (bool, String, Option<Value>)> {
        let d = self.d.clone();
        Box::pin(async move { run_job(d, job).await })
    }
}

async fn run_job(d: Arc<Daemon>, job: Value) -> (bool, String, Option<Value>) {
    let name = job["name"].as_str().unwrap_or("").to_string();
    let r: anyhow::Result<(String, Option<Value>)> = async {
        match job["type"].as_str().unwrap_or("") {
            "deploy" => {
                crate::stacks::write_file(&d.node, &d.root, &name, job["compose"].as_str().unwrap_or(""))?;
                let p = crate::stacks::load(&d.node, &d.root, &name)?;
                crate::compose::up(&d.node, &d.root, &d.node.store, &p, false).await?;
                Ok((format!("deployed {name} ({} services)", p.services.len()), None))
            }
            "remove" => {
                let p = crate::stacks::load(&d.node, &d.root, &name)?;
                let d2 = d.clone();
                let p2 = p.clone();
                tokio::task::spawn_blocking(move || crate::compose::down(&d2.node, &d2.root, &p2, false)).await??;
                crate::stacks::delete_managed(&d.root, &name)?;
                Ok((format!("removed {name}"), None))
            }
            "read" => {
                let (path, content) = crate::stacks::read_file(&d.node, &d.root, &name)?;
                Ok((String::new(), Some(json!({"path": path, "content": content, "files": [path]}))))
            }
            "write" => {
                let f = crate::stacks::write_file(&d.node, &d.root, &name, job["content"].as_str().unwrap_or(""))?;
                let mut log = format!("saved {} (previous version: {}.bak)", f.display(), f.display());
                if job["apply"] == true {
                    let p = crate::stacks::load(&d.node, &d.root, &name)?;
                    let (d2, p2) = (d.clone(), p.clone());
                    tokio::task::spawn_blocking(move || crate::compose::down(&d2.node, &d2.root, &p2, false)).await??;
                    crate::compose::up(&d.node, &d.root, &d.node.store, &p, false).await?;
                    log.push_str("\napplied (recreated the stack)");
                }
                Ok((log, None))
            }
            "compose" => {
                let op = job["op"].as_str().unwrap_or("");
                let ids: Vec<String> = d.node.list()?.into_iter().filter(|(m, _)| m.project.as_deref() == Some(name.as_str())).map(|(m, _)| m.id).collect();
                match op {
                    "start" => {
                        let p = crate::stacks::load(&d.node, &d.root, &name)?;
                        crate::compose::up(&d.node, &d.root, &d.node.store, &p, false).await?;
                    }
                    "stop" | "restart" => {
                        let d2 = d.clone();
                        let restart = op == "restart";
                        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                            for id in &ids {
                                d2.node.stop(id, 10)?;
                                if restart {
                                    d2.node.start(id)?;
                                }
                            }
                            Ok(())
                        })
                        .await??;
                    }
                    "update" => {
                        let p = crate::stacks::load(&d.node, &d.root, &name)?;
                        for svc in p.services.iter().filter(|s| s.build.is_none()) {
                            if let Some(img) = &svc.image {
                                if let Ok(r) = og_oci::Reference::parse(img) {
                                    let _ = std::fs::remove_file(d.root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_")));
                                }
                                crate::ensure_pulled(&d.root, &d.node.store, img, true).await?;
                            }
                        }
                        let (d2, p2) = (d.clone(), p.clone());
                        tokio::task::spawn_blocking(move || crate::compose::down(&d2.node, &d2.root, &p2, false)).await??;
                        crate::compose::up(&d.node, &d.root, &d.node.store, &p, false).await?;
                    }
                    _ => anyhow::bail!("unknown op {op}"),
                }
                let st: Vec<String> = d.node.list()?.into_iter().filter(|(m, _)| m.project.as_deref() == Some(name.as_str())).map(|(m, s)| format!("{}\t{}", m.service.unwrap_or(m.name), s.state)).collect();
                Ok((format!("{op} {name}: done\n{}", st.join("\n")), None))
            }
            t => anyhow::bail!("unknown job type {t}"),
        }
    }
    .await;
    match r {
        Ok((log, data)) => (true, log, data),
        Err(e) => (false, format!("{e:#}"), None),
    }
}

/// Container output since last time -> the lab log store (called every 2 s).
#[derive(Default)]
pub struct LogShipper {
    offsets: Mutex<HashMap<String, u64>>,
}

impl LogShipper {
    pub fn ship(&self, d: &Daemon, lab: &og_lab::Lab, host: &str) {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(list) = d.node.list() else { return };
        let now = og_lab::now_ms();
        let mut lines: Vec<Value> = Vec::new();
        let mut offs = self.offsets.lock().unwrap();
        let ids: Vec<String> = list.iter().map(|(m, _)| m.id.clone()).collect();
        offs.retain(|k, _| ids.contains(k));
        for (m, _) in &list {
            let path = d.node.log_path(&m.id);
            let len = std::fs::metadata(&path).map(|x| x.len()).unwrap_or(0);
            // a container seen for the first time starts at its last 64 KB
            let off = offs.entry(m.id.clone()).or_insert(len.saturating_sub(64 * 1024));
            if len < *off {
                *off = 0;
            }
            if len == *off {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(&path) else { continue };
            let _ = f.seek(SeekFrom::Start(*off));
            let mut buf = Vec::new();
            let _ = f.take(4 * 1024 * 1024).read_to_end(&mut buf);
            // only whole lines; the rest waits for the next round
            let cut = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
            *off += cut as u64;
            for l in String::from_utf8_lossy(&buf[..cut]).lines().filter(|l| !l.trim().is_empty()) {
                lines.push(json!([now, "container", m.name, og_lab::logs::guess_level(l), l.chars().take(8000).collect::<String>()]));
            }
        }
        drop(offs);
        if !lines.is_empty() {
            lab.logs.ingest(host, &lines);
            lab.fleet.count_log_errors(&lab.metrics, host, &lines);
        }
    }
}
