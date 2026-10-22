//! Moving VMs between cluster nodes, with their disks (no shared storage needed).
//!
//! The target creates the VM (same settings, disks as new volumes in a pool of its own) and
//! starts its QEMU paused, waiting for a migration, with its disks served over NBD. Offline
//! (VM off): the source copies each disk into those exports with `qemu-img convert`, then the
//! target QEMU quits. Live: the source mirrors the disks there while the VM runs, then moves
//! its memory and state (QEMU migration); the VM carries on on the target. Disk copies and
//! memory travel over TLS with the cluster's CA and node certificates.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::serve::Daemon;
use crate::vms::Vm;

/// QEMU's x509 credential directory, made from the cluster's CA and this node's certificate.
fn tls_dir(d: &Daemon) -> Result<PathBuf> {
    let src = d.root.join("cluster");
    let dir = src.join("qemu-tls");
    std::fs::create_dir_all(&dir)?;
    let cert = std::fs::read(src.join("node.pem")).context("not in a cluster")?;
    let key = std::fs::read(src.join("node.key"))?;
    std::fs::write(dir.join("ca-cert.pem"), std::fs::read(src.join("ca.pem"))?)?;
    for (n, b) in [("server-cert.pem", &cert), ("client-cert.pem", &cert)] {
        std::fs::write(dir.join(n), b)?;
    }
    for n in ["server-key.pem", "client-key.pem"] {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(dir.join(n))?.write_all(&key)?;
    }
    Ok(dir)
}

fn free_port() -> Result<u16> {
    for p in 60000..60100u16 {
        if std::net::TcpListener::bind(("0.0.0.0", p)).is_ok() {
            return Ok(p);
        }
    }
    bail!("no free port in 60000-60099")
}

fn tar_b64(dir: &std::path::Path, files: &[&str]) -> Result<String> {
    let mut b = tar::Builder::new(Vec::new());
    for f in files {
        let p = dir.join(f);
        if p.is_dir() {
            b.append_dir_all(f, &p)?;
        } else if p.is_file() {
            b.append_path_with_name(&p, f)?;
        }
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(b.into_inner()?))
}

// ---- target side (cluster port) ----

#[derive(Deserialize)]
pub struct Prepare {
    vm: Vm,
    /// tar (base64) of efivars.fd and tpm/
    files: String,
    /// disk id -> virtual size in bytes
    sizes: std::collections::HashMap<String, u64>,
    pool: Option<String>,
    live: bool,
}

pub async fn prepare(State(d): State<Arc<Daemon>>, Json(b): Json<Prepare>) -> Response {
    let d2 = d.clone();
    match tokio::task::spawn_blocking(move || prepare_inner(&d2, b)).await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}"))) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": format!("{e:#}")}))).into_response(),
    }
}

fn prepare_inner(d: &Daemon, b: Prepare) -> Result<Value> {
    let mut vm = b.vm;
    let name = vm.name.clone();
    if !crate::vms::valid_name(&name) {
        bail!("bad VM name");
    }
    if d.vms.dir(&name).exists() {
        bail!("a VM named {name} exists on this node already");
    }
    let tls = tls_dir(d)?;
    let dir = d.vms.dir(&name);
    std::fs::create_dir_all(&dir)?;
    let mut made = vec![];
    let r = (|| -> Result<Value> {
        // settings, UEFI variables, TPM
        let tarb = base64::engine::general_purpose::STANDARD.decode(&b.files)?;
        let mut a = tar::Archive::new(&tarb[..]);
        for e in a.entries()? {
            let mut e = e?;
            if !e.unpack_in(&dir)? {
                bail!("bad path in the VM's files");
            }
        }
        // new volumes here
        for x in &mut vm.disks {
            let want = b.pool.clone().or_else(|| x.volume.split(':').next().map(String::from)).unwrap_or_else(|| "local".into());
            let pool = d.vms.pools.get(&want).or_else(|_| d.vms.pools.get("local"))?;
            let vol = format!("{}:vm-{name}-{}{}", pool.id, x.id, if pool.kind == "dir" { ".qcow2" } else { "" });
            let size = *b.sizes.get(&x.id).context("missing disk size")?;
            d.vms.pools.create(&vol, size.div_ceil(1 << 30).max(1))?;
            if size % (1 << 30) != 0 {
                // exact size (the copy must fit)
                let _ = std::process::Command::new("qemu-img").args(["resize", "-q", "--shrink"]).arg(d.vms.pools.path(&vol)?).arg(size.to_string()).status();
            }
            made.push(vol.clone());
            x.volume = vol;
            x.file.clear();
        }
        vm.desired = "stopped".into();
        vm.cdroms = vm.cdroms.iter().map(|c| if c.is_empty() || d.vms.iso_dir().join(c).exists() { c.clone() } else { String::new() }).collect();
        std::fs::write(dir.join("vm.json"), serde_json::to_vec_pretty(&vm)?)?;
        crate::vmrun::start_with(d, &name, true)?;
        let q = |cmd: &str, a: Value| d.vms.qmp(&name, cmd, a);
        q("object-add", json!({"qom-type": "tls-creds-x509", "id": "ogtls", "dir": tls, "endpoint": "server", "verify-peer": true}))?;
        let nbd_port = free_port()?;
        q("nbd-server-start", json!({"addr": {"type": "inet", "data": {"host": "0.0.0.0", "port": nbd_port.to_string()}}, "tls-creds": "ogtls"}))?;
        for x in &vm.disks {
            q("block-export-add", json!({"type": "nbd", "id": format!("exp-{}", x.id), "node-name": format!("drive-{}", x.id), "name": x.id, "writable": true}))?;
        }
        let mut mig_port = 0;
        if b.live {
            mig_port = free_port()?;
            q("migrate-set-parameters", json!({"tls-creds": "ogtls"}))?;
            q("migrate-incoming", json!({"uri": format!("tcp:0.0.0.0:{mig_port}")}))?;
        }
        Ok(json!({"nbd_port": nbd_port, "migrate_port": mig_port, "disks": vm.disks.iter().map(|x| json!({"id": x.id, "volume": x.volume})).collect::<Vec<_>>()}))
    })();
    if r.is_err() {
        let _ = d.vms.qmp(&name, "quit", json!({}));
        std::thread::sleep(Duration::from_millis(500));
        let _ = std::fs::remove_dir_all(&dir);
        for v in made {
            let _ = d.vms.pools.delete(&v);
        }
    }
    r
}

#[derive(Deserialize)]
pub struct Finish {
    live: bool,
    start: bool,
}

pub async fn finish(State(d): State<Arc<Daemon>>, UrlPath(name): UrlPath<String>, Json(b): Json<Finish>) -> Response {
    let d2 = d.clone();
    let r = tokio::task::spawn_blocking(move || -> Result<()> {
        let _ = d2.vms.qmp(&name, "nbd-server-stop", json!({}));
        if b.live {
            // the VM runs here now
            d2.vms.set_desired(&name, "running")?;
        } else {
            let _ = d2.vms.qmp(&name, "quit", json!({}));
            for _ in 0..40 {
                if d2.vms.status(&name).state == "stopped" {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            if let Ok(vm) = d2.vms.get(&name) {
                crate::vmrun::cleanup(&d2, &vm);
            }
            if b.start {
                crate::vmrun::start(&d2, &name)?;
            }
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
    match r {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": format!("{e:#}")}))).into_response(),
    }
}

pub async fn abort(State(d): State<Arc<Daemon>>, UrlPath(name): UrlPath<String>) -> Response {
    let d2 = d.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = d2.vms.qmp(&name, "quit", json!({}));
        std::thread::sleep(Duration::from_secs(1));
        if let Ok(vm) = d2.vms.get(&name) {
            crate::vmrun::cleanup(&d2, &vm);
            for x in &vm.disks {
                let _ = d2.vms.pools.delete(&x.volume);
            }
        }
        let _ = std::fs::remove_dir_all(d2.vms.dir(&name));
    })
    .await;
    Json(json!({"ok": true})).into_response()
}

// ---- source side ----

#[derive(Deserialize)]
pub struct MigrateReq {
    node: String,
    #[serde(default)]
    pool: Option<String>,
    /// start it on the target (offline migration of a stopped VM)
    #[serde(default)]
    start: bool,
}

pub async fn migrate(State(d): State<Arc<Daemon>>, Extension(w): Extension<og_access::Who>, UrlPath(name): UrlPath<String>, Json(b): Json<MigrateReq>) -> Response {
    let d2 = d.clone();
    let label = format!("{name} -> {}", b.node);
    match d.tasks.spawn("vm-migrate", &label, &w.label(), move |t| {
        let h = tokio::runtime::Handle::current();
        h.block_on(run(&d2, t, &name, &b))
    }) {
        Ok(id) => Json(json!({"task": id})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

async fn run(d: &Arc<Daemon>, t: &crate::tasks::Task, name: &str, b: &MigrateReq) -> Result<()> {
    let vm = d.vms.get(name)?;
    if vm.template {
        bail!("templates don't migrate (clone them on the other node)");
    }
    if !vm.hostpci.is_empty() || !vm.usb.is_empty() {
        bail!("a VM with passed-through devices can't move (they belong to this server)");
    }
    if vm.disks.iter().any(|x| x.volume.is_empty()) {
        bail!("move the VM's disks into a storage pool first (old-style disk files)");
    }
    let node = d.cluster.node_pub(&b.node)?;
    if node.name == d.cluster.me {
        bail!("it's on this node already");
    }
    let host = node.addr.rsplit_once(':').map(|(h, _)| h.trim_matches(['[', ']']).to_string()).context("bad node address")?;
    let live = d.vms.status(name).state != "stopped";
    let c = d.cluster.client_pub()?;
    let base = format!("https://{}/cluster/v1/vm", node.addr);
    let tls = tls_dir(d)?;
    let mut sizes = serde_json::Map::new();
    for x in &vm.disks {
        sizes.insert(x.id.clone(), json!(d.vms.pools.size(&x.volume)?));
    }
    t.log(format!("{} migration of {name} to {} ({host})", if live { "live" } else { "offline" }, node.name));
    let files = tar_b64(&d.vms.dir(name), &["efivars.fd", "tpm"])?;
    let r = c.post(format!("{base}/prepare")).json(&json!({"vm": vm, "files": files, "sizes": sizes, "pool": b.pool, "live": live})).send().await?;
    let st = r.status();
    let prep: Value = r.json().await.unwrap_or_default();
    if !st.is_success() {
        bail!("{}: {}", node.name, prep["error"].as_str().unwrap_or("prepare failed"));
    }
    let nbd_port = prep["nbd_port"].as_u64().unwrap_or(0);
    t.log(format!("target ready: disks {}", prep["disks"].as_array().map(|a| a.iter().filter_map(|x| x["volume"].as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()));
    let outcome: Result<()> = if live {
        live_copy(d, t, name, &vm, &host, nbd_port, prep["migrate_port"].as_u64().unwrap_or(0), &tls).await
    } else {
        offline_copy(d, t, &vm, &host, nbd_port, &tls).await
    };
    if let Err(e) = outcome {
        t.warn(format!("{e:#}: undoing on {}", node.name));
        let _ = c.post(format!("{base}/{name}/abort")).json(&json!({})).send().await;
        if live {
            let _ = d.vms.qmp(name, "cont", json!({}));
        }
        return Err(e);
    }
    let r = c.post(format!("{base}/{name}/finish")).json(&json!({"live": live, "start": b.start})).send().await?;
    if !r.status().is_success() {
        let v: Value = r.json().await.unwrap_or_default();
        bail!("finishing on {}: {}", node.name, v["error"].as_str().unwrap_or("?"));
    }
    // gone from here: the VM (its QEMU, settings and volumes)
    if live {
        let _ = d.vms.qmp(name, "quit", json!({}));
        for _ in 0..40 {
            if d.vms.status(name).state == "stopped" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        crate::vmrun::cleanup(d, &vm);
    }
    d.vms.remove(&d.node.net, name)?;
    t.log(format!("{name} now lives on {}", node.name));
    Ok(())
}

async fn offline_copy(d: &Daemon, t: &crate::tasks::Task, vm: &Vm, host: &str, port: u64, tls: &std::path::Path) -> Result<()> {
    for x in &vm.disks {
        t.check()?;
        let src = d.vms.pools.path(&x.volume)?;
        t.log(format!("copying {} ({:.1} GiB)", x.id, d.vms.pools.size(&x.volume)? as f64 / (1u64 << 30) as f64));
        let fmt = if src.extension().is_some_and(|e| e == "qcow2") { "qcow2" } else { "raw" };
        let target = format!("driver=raw,file.driver=nbd,file.server.type=inet,file.server.host={host},file.server.port={port},file.export={},file.tls-creds=ogtls,file.tls-hostname={host}", x.id);
        let out = tokio::process::Command::new("qemu-img")
            .args(["convert", "-p", "-n", "--target-is-zero", "-f", fmt, "--object"])
            .arg(format!("tls-creds-x509,id=ogtls,endpoint=client,dir={}", tls.display()))
            .arg(&src)
            .args(["--target-image-opts", &target])
            .output()
            .await?;
        if !out.status.success() {
            bail!("copying {}: {}", x.id, String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    Ok(())
}

async fn live_copy(d: &Daemon, t: &crate::tasks::Task, name: &str, vm: &Vm, host: &str, nbd_port: u64, mig_port: u64, tls: &std::path::Path) -> Result<()> {
    let q = |cmd: &str, a: Value| d.vms.qmp(name, cmd, a);
    let _ = q("object-del", json!({"id": "ogtls"}));
    q("object-add", json!({"qom-type": "tls-creds-x509", "id": "ogtls", "dir": tls, "endpoint": "client"}))?;
    for x in &vm.disks {
        // zero blocks (unused space on thin volumes) go over as write-zeroes requests, not data
        q("blockdev-add", json!({"driver": "nbd", "node-name": format!("tgt-{}", x.id), "server": {"type": "inet", "host": host, "port": nbd_port.to_string()}, "export": x.id, "tls-creds": "ogtls", "tls-hostname": host, "detect-zeroes": "unmap", "discard": "unmap"}))?;
        q("blockdev-mirror", json!({"job-id": format!("mirror-{}", x.id), "device": format!("drive-{}", x.id), "target": format!("tgt-{}", x.id), "sync": "full"}))?;
    }
    t.log("mirroring the disks while the VM runs");
    let t0 = Instant::now();
    let mut last = Instant::now();
    loop {
        t.check()?;
        let jobs = q("query-block-jobs", json!({}))?;
        let list = jobs.as_array().cloned().unwrap_or_default();
        if list.len() < vm.disks.len() {
            bail!("a disk mirror stopped");
        }
        if list.iter().all(|j| j["ready"] == true) {
            break;
        }
        if last.elapsed() > Duration::from_secs(10) {
            last = Instant::now();
            let (done, total) = list.iter().fold((0u64, 0u64), |a, j| (a.0 + j["offset"].as_u64().unwrap_or(0), a.1 + j["len"].as_u64().unwrap_or(0)));
            t.log(format!("  disks {:.0}%", if total > 0 { done as f64 * 100.0 / total as f64 } else { 0.0 }));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    t.log(format!("disks in sync after {:.0} s; moving memory", t0.elapsed().as_secs_f64()));
    q("migrate-set-capabilities", json!({"capabilities": [{"capability": "events", "state": true}]}))?;
    // the guest is paused for at most ~50 ms at the switch (QEMU's default allows 300 ms;
    // Proxmox uses 100 ms); QEMU caps the bandwidth at 128 MiB/s unless told otherwise
    q("migrate-set-parameters", json!({"tls-creds": "ogtls", "tls-hostname": host, "downtime-limit": 50, "max-bandwidth": 10u64 << 30}))?;
    q("migrate", json!({"uri": format!("tcp:{host}:{mig_port}")}))?;
    let t1 = Instant::now();
    loop {
        let m = q("query-migrate", json!({}))?;
        match m["status"].as_str().unwrap_or("") {
            "completed" => {
                t.log(format!("memory moved in {:.1} s (downtime {} ms)", t1.elapsed().as_secs_f64(), m["downtime"].as_u64().unwrap_or(0)));
                break;
            }
            "failed" | "cancelled" => bail!("migration {}: {}", m["status"].as_str().unwrap_or(""), m["error-desc"].as_str().unwrap_or("")),
            _ => {}
        }
        if t.cancelled() {
            let _ = q("migrate_cancel", json!({}));
            bail!("stopped");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    for x in &vm.disks {
        let _ = q("block-job-cancel", json!({"device": format!("mirror-{}", x.id), "force": true}));
    }
    Ok(())
}
