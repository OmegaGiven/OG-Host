//! HTTP API for VMs and ISO images; privileges come from `auth::rule`. The console is VNC
//! carried over a WebSocket (RFB bytes as binary messages) to the UI's own client.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use futures_util::{SinkExt, StreamExt};
use og_access::Who;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::serve::Daemon;
use crate::vms::{Disk, Vm};

type D = State<Arc<Daemon>>;

fn bad(e: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, Response> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}"))).map_err(|e| bad(format!("{e:#}")))
}

pub fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/v1/vms", get(list).post(create))
        .route("/v1/vms/host", get(host))
        .route("/v1/vms/{name}", get(inspect).put(update).delete(remove))
        .route("/v1/vms/{name}/disks", post(disk_add))
        .route("/v1/vms/{name}/disks/{disk}/resize", post(disk_resize))
        .route("/v1/vms/{name}/cdrom", post(cdrom))
        .route("/v1/vms/{name}/snapshots", get(snaps).post(snap_create))
        .route("/v1/vms/{name}/snapshots/{snap}", delete(snap_delete))
        .route("/v1/vms/{name}/snapshots/{snap}/rollback", post(snap_rollback))
        .route("/v1/vms/{name}/template", post(make_template))
        .route("/v1/vms/{name}/clone", post(clone_vm))
        .route("/v1/vms/{name}/migrate", post(crate::migrate::migrate))
        .route("/v1/vms/{name}/cloudinit", post(set_cloudinit))
        .route("/v1/pools", get(pools).post(pool_put))
        .route("/v1/pools/{id}", delete(pool_delete))
        .route("/v1/pools/{id}/volumes", get(pool_volumes))
        .route("/v1/pools/{id}/volumes/{vol}", delete(volume_delete))
        .route("/v1/pools/{id}/import", post(pool_import))
        .route("/v1/vms/{name}/vnc", get(vnc))
        .route("/v1/vms/{name}/console", get(console_log))
        .route("/v1/vms/{name}/{action}", post(action))
        .route("/v1/iso", get(isos))
        .route("/v1/iso/{file}", axum::routing::put(iso_upload).delete(iso_delete))
        .with_state(d)
}

/// Previous CPU sample per VM (for a CPU % between two list calls).
static CPU: Mutex<BTreeMap<String, (f64, std::time::Instant)>> = Mutex::new(BTreeMap::new());

fn view(d: &Daemon, vm: &Vm) -> Value {
    let st = d.vms.status(&vm.name);
    let (mut cpu, mut rss) = (None, None);
    if let Some((secs, r)) = st.pid.and_then(crate::vmrun::usage) {
        rss = Some(r);
        let mut m = CPU.lock().unwrap();
        if let Some((prev, t)) = m.get(&vm.name) {
            let dt = t.elapsed().as_secs_f64();
            if dt > 0.2 {
                cpu = Some(((secs - prev) / dt * 100.0).max(0.0));
            }
        }
        m.insert(vm.name.clone(), (secs, std::time::Instant::now()));
    }
    let ips: Vec<Value> = vm.nics.iter().map(|n| json!({"id": n.id, "network": n.network, "mac": n.mac, "ip": d.dhcp.ip_of(&n.mac), "ports": n.ports})).collect();
    let disks: Vec<Value> = vm
        .disks
        .iter()
        .map(|x| {
            let p = d.vms.disk_path(vm, x);
            let alloc = std::fs::metadata(&p).map(|m| std::os::unix::fs::MetadataExt::blocks(&m) * 512).ok();
            json!({"id": x.id, "bus": x.bus, "file": x.file, "allocated": alloc})
        })
        .collect();
    json!({"vm": vm, "state": st.state, "pid": st.pid, "cpu": cpu, "rss": rss, "nics": ips, "disks": disks})
}

async fn list(State(d): D) -> Json<Value> {
    let d2 = d.clone();
    Json(tokio::task::spawn_blocking(move || json!(d2.vms.list().iter().map(|v| view(&d2, v)).collect::<Vec<_>>())).await.unwrap_or_default())
}

async fn host(State(d): D) -> Json<Value> {
    let mut h = crate::vms::host_info();
    h["isos"] = json!(iso_list(&d));
    h["bridges"] = json!(std::fs::read_dir("/sys/class/net").into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with("og") && std::path::Path::new("/sys/class/net").join(n).join("bridge").exists()).collect::<Vec<_>>());
    h["pools"] = json!(d.vms.pools.list().iter().map(|p| json!({"id": p.id, "type": p.kind})).collect::<Vec<_>>());
    h["templates"] = json!(d.vms.list().into_iter().filter(|v| v.template).map(|v| json!({"name": v.name, "os": v.os, "memory": v.memory, "cores": v.cores, "comment": v.comment})).collect::<Vec<_>>());
    h["networks"] = json!(d.node.net.list().unwrap_or_default().iter().map(|n| json!({"name": n.name, "subnet": n.subnet.to_string()})).collect::<Vec<_>>());
    h["ksm"] = json!(d.vmbrain.lock().unwrap().ksm);
    Json(h)
}

async fn inspect(State(d): D, Path(name): Path<String>) -> Response {
    let d2 = d.clone();
    match blocking(move || Ok(view(&d2, &d2.vms.get(&name)?))).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct CreateReq {
    vm: Vm,
    /// GiB per new disk id
    #[serde(default)]
    sizes: BTreeMap<String, u64>,
    #[serde(default)]
    start: bool,
}

async fn create(State(d): D, Json(b): Json<CreateReq>) -> Response {
    let d2 = d.clone();
    match blocking(move || {
        let vm = d2.vms.create(b.vm, &b.sizes)?;
        if b.start {
            crate::vmrun::start(&d2, &vm.name)?;
        }
        Ok(view(&d2, &vm))
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

async fn update(State(d): D, Path(name): Path<String>, Json(mut vm): Json<Vm>) -> Response {
    vm.name = name;
    let d2 = d.clone();
    match blocking(move || {
        let v = d2.vms.update(vm)?;
        crate::vmrun::refresh_firewall(&d2);
        Ok(view(&d2, &v))
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

async fn remove(State(d): D, Path(name): Path<String>) -> Response {
    let d2 = d.clone();
    match blocking(move || d2.vms.remove(&d2.node.net, &name)).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(r) => r,
    }
}

async fn action(State(d): D, Extension(w): Extension<Who>, Path((name, act)): Path<(String, String)>) -> Response {
    // shutting down can take minutes: run it as a task
    if act == "shutdown" || act == "reboot" {
        let d2 = d.clone();
        let n = name.clone();
        let a = act.clone();
        return match d.tasks.spawn(&format!("vm-{act}"), &name, &w.label(), move |t| {
            t.log(format!("{a} {n}: asking the guest (ACPI), up to 2 minutes before forcing"));
            crate::vmrun::action(&d2, &n, &a)
        }) {
            Ok(id) => Json(json!({"task": id})).into_response(),
            Err(e) => bad(e),
        };
    }
    let d2 = d.clone();
    match blocking(move || crate::vmrun::action(&d2, &name, &act)).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct DiskReq {
    disk: Disk,
    #[serde(default)]
    size: u64,
}

async fn disk_add(State(d): D, Path(name): Path<String>, Json(b): Json<DiskReq>) -> Response {
    let d2 = d.clone();
    match blocking(move || Ok(view(&d2, &d2.vms.add_disk(&name, b.disk, b.size)?))).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct SizeReq {
    size: u64,
}

async fn disk_resize(State(d): D, Path((name, disk)): Path<(String, String)>, Json(b): Json<SizeReq>) -> Response {
    let d2 = d.clone();
    match blocking(move || d2.vms.resize_disk(&name, &disk, b.size)).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct CdromReq {
    #[serde(default)]
    index: usize,
    /// "" ejects
    iso: String,
}

/// Change the disc (live when running).
async fn cdrom(State(d): D, Path(name): Path<String>, Json(b): Json<CdromReq>) -> Response {
    let d2 = d.clone();
    match blocking(move || {
        let mut vm = d2.vms.get(&name)?;
        if b.index >= vm.cdroms.len() {
            anyhow::bail!("the VM has no cdrom{}", b.index);
        }
        vm.cdroms[b.index] = b.iso.clone();
        d2.vms.normalize(&mut vm)?;
        if d2.vms.status(&name).state != "stopped" {
            let id = format!("cdrom{}", b.index);
            if b.iso.is_empty() {
                d2.vms.qmp(&name, "blockdev-open-tray", json!({"id": id, "force": true}))?;
                d2.vms.qmp(&name, "blockdev-remove-medium", json!({"id": id}))?;
            } else {
                let p = if b.iso.starts_with('/') { std::path::PathBuf::from(&b.iso) } else { d2.vms.iso_dir().join(&b.iso) };
                d2.vms.qmp(&name, "blockdev-change-medium", json!({"id": id, "filename": p, "format": "raw", "read-only-mode": "read-only"}))?;
            }
        }
        // saved without touching the rest (allowed while running)
        let p = d2.vms.dir(&name).join("vm.json");
        std::fs::write(&p, serde_json::to_vec_pretty(&vm)?)?;
        Ok(())
    })
    .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(r) => r,
    }
}

// ---- snapshots (qcow2 internal; with RAM when the VM runs) ----

fn valid_snap(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
}

fn hmp(d: &Daemon, name: &str, cmd: &str) -> anyhow::Result<String> {
    let r = d.vms.qmp(name, "human-monitor-command", json!({"command-line": cmd}))?;
    let out = r.as_str().unwrap_or("").trim().to_string();
    if out.to_lowercase().contains("error") {
        anyhow::bail!("{out}");
    }
    Ok(out)
}

async fn snaps(State(d): D, Path(name): Path<String>) -> Response {
    let d2 = d.clone();
    match blocking(move || {
        let vm = d2.vms.get(&name)?;
        let Some(first) = vm.disks.iter().find(|x| x.file.ends_with(".qcow2")) else { return Ok(json!([])) };
        let out = std::process::Command::new("qemu-img").args(["snapshot", "-U", "-l", &d2.vms.disk_path(&vm, first).to_string_lossy()]).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // ID TAG VM_SIZE DATE VM_CLOCK ICOUNT
        let list: Vec<Value> = text
            .lines()
            .skip_while(|l| !l.starts_with("ID"))
            .skip(1)
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f.len() >= 5).then(|| json!({"name": f[1], "vm_state": f[2] != "0" && f[2] != "0 B", "date": format!("{} {}", f[f.len() - 4], f[f.len() - 3])}))
            })
            .collect();
        Ok(json!(list))
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct SnapReq {
    name: String,
}

fn qcow2_only(d: &Daemon, vm: &Vm) -> anyhow::Result<()> {
    if vm.disks.iter().any(|x| !x.file.ends_with(".qcow2")) {
        anyhow::bail!("snapshots need all disks to be qcow2");
    }
    if !vm.hostpci.is_empty() && d.vms.status(&vm.name).state != "stopped" {
        anyhow::bail!("a VM with passed-through devices can only be snapshotted while stopped");
    }
    Ok(())
}

async fn snap_create(State(d): D, Extension(w): Extension<Who>, Path(name): Path<String>, Json(b): Json<SnapReq>) -> Response {
    if !valid_snap(&b.name) {
        return bad("snapshot names are letters, digits, - and _");
    }
    let d2 = d.clone();
    let n = name.clone();
    match d.tasks.spawn("vm-snapshot", &name, &w.label(), move |t| {
        let vm = d2.vms.get(&n)?;
        qcow2_only(&d2, &vm)?;
        if d2.vms.status(&n).state != "stopped" {
            t.log(format!("saving {} with its memory (the VM pauses briefly)", b.name));
            hmp(&d2, &n, &format!("savevm {}", b.name))?;
        } else {
            for x in &vm.disks {
                let out = std::process::Command::new("qemu-img").args(["snapshot", "-c", &b.name, &d2.vms.disk_path(&vm, x).to_string_lossy()]).output()?;
                if !out.status.success() {
                    anyhow::bail!("qemu-img: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
            }
        }
        t.log("done");
        Ok(())
    }) {
        Ok(id) => Json(json!({"task": id})).into_response(),
        Err(e) => bad(e),
    }
}

async fn snap_rollback(State(d): D, Extension(w): Extension<Who>, Path((name, snap)): Path<(String, String)>) -> Response {
    if !valid_snap(&snap) {
        return bad("bad snapshot name");
    }
    let d2 = d.clone();
    let n = name.clone();
    match d.tasks.spawn("vm-rollback", &name, &w.label(), move |t| {
        let vm = d2.vms.get(&n)?;
        qcow2_only(&d2, &vm)?;
        if d2.vms.status(&n).state != "stopped" {
            hmp(&d2, &n, &format!("loadvm {snap}"))?;
        } else {
            for x in &vm.disks {
                let out = std::process::Command::new("qemu-img").args(["snapshot", "-a", &snap, &d2.vms.disk_path(&vm, x).to_string_lossy()]).output()?;
                if !out.status.success() {
                    anyhow::bail!("qemu-img: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
            }
        }
        t.log(format!("back at {snap}"));
        Ok(())
    }) {
        Ok(id) => Json(json!({"task": id})).into_response(),
        Err(e) => bad(e),
    }
}

async fn snap_delete(State(d): D, Path((name, snap)): Path<(String, String)>) -> Response {
    if !valid_snap(&snap) {
        return bad("bad snapshot name");
    }
    let d2 = d.clone();
    match blocking(move || {
        let vm = d2.vms.get(&name)?;
        if d2.vms.status(&name).state != "stopped" {
            hmp(&d2, &name, &format!("delvm {snap}"))?;
        } else {
            for x in vm.disks.iter().filter(|x| x.file.ends_with(".qcow2")) {
                let _ = std::process::Command::new("qemu-img").args(["snapshot", "-d", &snap, &d2.vms.disk_path(&vm, x).to_string_lossy()]).output();
            }
        }
        Ok(())
    })
    .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(r) => r,
    }
}

// ---- console ----

/// VNC over a WebSocket. The browser's own origin only (the session cookie is SameSite=Strict
/// too, but a WebSocket handshake isn't covered by the CSRF header check).
async fn vnc(State(d): D, Path(name): Path<String>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(|o| o.split("://").nth(1).unwrap_or(o).to_string());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).map(String::from);
    if origin.is_some() && origin != host && origin.as_deref() != headers.get("x-forwarded-host").and_then(|v| v.to_str().ok()) {
        return (StatusCode::FORBIDDEN, "cross-origin console").into_response();
    }
    if d.vms.get(&name).is_err() {
        return bad("no such VM");
    }
    let sock = std::path::PathBuf::from("/run/og-host/vms").join(&name).join("vnc.sock");
    ws.protocols(["binary"]).max_message_size(1 << 20).on_upgrade(move |socket| pipe(socket, sock))
}

async fn pipe(socket: WebSocket, sock: std::path::PathBuf) {
    let Ok(unix) = tokio::net::UnixStream::connect(&sock).await else {
        let mut s = socket;
        let _ = s.send(Message::Close(None)).await;
        return;
    };
    let (mut ur, mut uw) = unix.into_split();
    let (mut wtx, mut wrx) = socket.split();
    let up = async move {
        use tokio::io::AsyncWriteExt;
        while let Some(Ok(m)) = wrx.next().await {
            match m {
                Message::Binary(b) => {
                    if uw.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    let down = async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match ur.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if wtx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = wtx.send(Message::Close(None)).await;
    };
    tokio::select! { _ = up => {}, _ = down => {} }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TailQ {
    bytes: Option<u64>,
}

/// The end of the VM's serial console output.
async fn console_log(State(d): D, Path(name): Path<String>, Query(q): Query<TailQ>) -> Response {
    if d.vms.get(&name).is_err() {
        return bad("no such VM");
    }
    let p = d.vms.dir(&name).join("console.log");
    let want = q.bytes.unwrap_or(64 * 1024).min(1 << 20);
    let text = std::fs::read(&p).map(|b| {
        let start = b.len().saturating_sub(want as usize);
        String::from_utf8_lossy(&b[start..]).into_owned()
    });
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text.unwrap_or_default()).into_response()
}

// ---- ISO images ----

fn valid_iso(f: &str) -> bool {
    !f.is_empty() && f.len() <= 120 && f.chars().all(|c| c.is_ascii_alphanumeric() || "_.-+".contains(c)) && !f.starts_with('.') && (f.ends_with(".iso") || f.ends_with(".img"))
}

fn iso_list(d: &Daemon) -> Vec<Value> {
    let mut v: Vec<Value> = std::fs::read_dir(d.vms.iso_dir()).into_iter().flatten().flatten().filter_map(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        valid_iso(&n).then(|| json!({"name": n, "size": e.metadata().map(|m| m.len()).unwrap_or(0)}))
    }).collect();
    v.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    v
}

async fn isos(State(d): D) -> Json<Value> {
    Json(json!(iso_list(&d)))
}

/// Upload an ISO as the raw request body (streamed to disk).
async fn iso_upload(State(d): D, Path(file): Path<String>, body: Body) -> Response {
    if !valid_iso(&file) {
        return bad("ISO names are letters, digits, . _ - + and end in .iso or .img");
    }
    let dir = d.vms.iso_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return bad("can't create the ISO directory");
    }
    let dest = dir.join(&file);
    if dest.exists() {
        return bad("an ISO with that name exists already");
    }
    let tmp = dir.join(format!(".{file}.part"));
    let r = async {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut s = body.into_data_stream();
        let mut n: u64 = 0;
        while let Some(chunk) = s.next().await {
            let c = chunk.map_err(std::io::Error::other)?;
            n += c.len() as u64;
            if n > 64 << 30 {
                return Err(std::io::Error::other("ISO larger than 64 GiB"));
            }
            f.write_all(&c).await?;
        }
        f.flush().await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok::<u64, std::io::Error>(n)
    }
    .await;
    match r {
        Ok(n) => Json(json!({"name": file, "size": n})).into_response(),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            bad(e)
        }
    }
}

async fn iso_delete(State(d): D, Path(file): Path<String>) -> Response {
    if !valid_iso(&file) {
        return bad("bad ISO name");
    }
    let used: Vec<String> = d.vms.list().into_iter().filter(|v| v.cdroms.iter().any(|c| c == &file)).map(|v| v.name).collect();
    if !used.is_empty() {
        return bad(format!("in use by {}", used.join(", ")));
    }
    match std::fs::remove_file(d.vms.iso_dir().join(&file)) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

// ---- templates, clones, cloud-init ----

async fn make_template(State(d): D, Path(name): Path<String>) -> Response {
    let d2 = d.clone();
    match blocking(move || Ok(view(&d2, &d2.vms.make_template(&name)?))).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct CloneReq {
    name: String,
    #[serde(default)]
    linked: bool,
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    start: bool,
}

async fn clone_vm(State(d): D, Extension(w): Extension<Who>, Path(src): Path<String>, Json(b): Json<CloneReq>) -> Response {
    let d2 = d.clone();
    let label = format!("{src} -> {}", b.name);
    match d.tasks.spawn("vm-clone", &label, &w.label(), move |t| {
        t.log(format!("{} clone of {src} as {}", if b.linked { "linked" } else { "full" }, b.name));
        let vm = d2.vms.clone_vm(&src, &b.name, b.linked, b.pool.as_deref())?;
        for x in &vm.disks {
            t.log(format!("  {} -> {}", x.id, x.volume));
        }
        if b.start {
            crate::vmrun::start(&d2, &vm.name)?;
            t.log("started");
        }
        Ok(())
    }) {
        Ok(id) => Json(json!({"task": id})).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct CloudInitReq {
    /// null turns cloud-init off
    cloudinit: Option<crate::vms::CloudInit>,
    /// a new password (hashed here; empty keeps the current one)
    #[serde(default)]
    password: String,
}

async fn set_cloudinit(State(d): D, Path(name): Path<String>, Json(b): Json<CloudInitReq>) -> Response {
    let d2 = d.clone();
    match blocking(move || {
        let mut vm = d2.vms.get(&name)?;
        let old = vm.cloudinit.clone();
        vm.cloudinit = match b.cloudinit {
            None => None,
            Some(mut c) => {
                c.password_hash = if b.password.is_empty() { old.map(|o| o.password_hash).unwrap_or_default() } else { crate::vms::hash_password(&b.password)? };
                if !c.user.is_empty() && !c.user.chars().all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_' || x == '-') {
                    anyhow::bail!("user names are lower-case letters, digits, _ and -");
                }
                if !["", "dhcp", "static"].contains(&c.ip.as_str()) {
                    anyhow::bail!("ip is dhcp or static");
                }
                c.ssh_keys.retain(|k| !k.trim().is_empty());
                Some(c)
            }
        };
        // saved directly: allowed while running (applies at the next boot)
        let p = d2.vms.dir(&name).join("vm.json");
        std::fs::write(&p, serde_json::to_vec_pretty(&vm)?)?;
        Ok(view(&d2, &vm))
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

// ---- storage pools ----

async fn pools(State(d): D) -> Json<Value> {
    let d2 = d.clone();
    Json(tokio::task::spawn_blocking(move || {
        json!(d2.vms.pools.list().iter().map(|p| {
            let u = d2.vms.pools.usage(p);
            json!({"pool": p, "total": u.map(|x| x.0), "used": u.map(|x| x.1)})
        }).collect::<Vec<_>>())
    }).await.unwrap_or_default())
}

async fn pool_put(State(d): D, Json(p): Json<crate::pools::Pool>) -> Response {
    let d2 = d.clone();
    match blocking(move || d2.vms.pools.upsert(p)).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(r) => r,
    }
}

async fn pool_delete(State(d): D, Path(id): Path<String>) -> Response {
    match d.vms.pools.remove(&id) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

async fn pool_volumes(State(d): D, Path(id): Path<String>) -> Response {
    let d2 = d.clone();
    match blocking(move || {
        let p = d2.vms.pools.get(&id)?;
        let used: std::collections::HashMap<String, String> = d2.vms.list().into_iter().flat_map(|v| v.disks.iter().map(|x| (x.volume.clone(), v.name.clone())).collect::<Vec<_>>()).collect();
        Ok(json!(d2.vms.pools.volumes(&p).into_iter().map(|mut v| {
            v["vm"] = json!(used.get(v["volume"].as_str().unwrap_or("")));
            v
        }).collect::<Vec<_>>()))
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

async fn volume_delete(State(d): D, Path((id, vol)): Path<(String, String)>) -> Response {
    let d2 = d.clone();
    match blocking(move || {
        let full = format!("{id}:{vol}");
        if let Some(v) = d2.vms.list().into_iter().find(|v| v.disks.iter().any(|x| x.volume == full)) {
            anyhow::bail!("{full} is a disk of {}", v.name);
        }
        d2.vms.pools.delete(&full)
    })
    .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct ImportReq {
    /// a disk image to download (cloud images: qcow2 / raw / vmdk / vhdx)
    #[serde(default)]
    url: String,
    /// or an uploaded file in the ISO/images folder
    #[serde(default)]
    file: String,
    /// volume name in the pool
    name: String,
    /// grow it to this many GiB
    #[serde(default)]
    grow: Option<u64>,
}

async fn pool_import(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>, Json(b): Json<ImportReq>) -> Response {
    if !crate::pools::valid_vol(&b.name) {
        return bad("volume names are letters, digits, . _ -");
    }
    if b.url.is_empty() == b.file.is_empty() {
        return bad("give a url or a file");
    }
    if !b.url.is_empty() && !(b.url.starts_with("https://") || b.url.starts_with("http://")) {
        return bad("the url must be http(s)");
    }
    if !b.file.is_empty() && !valid_iso(&b.file) && !b.file.chars().all(|c| c.is_ascii_alphanumeric() || "_.-+".contains(c)) {
        return bad("bad file name");
    }
    let d2 = d.clone();
    let label = format!("{id}:{}", b.name);
    match d.tasks.spawn("import", &label, &w.label(), move |t| {
        let vol = format!("{id}:{}", b.name);
        let tmp;
        let src = if !b.url.is_empty() {
            tmp = d2.vms.iso_dir().join(format!(".import-{}", crate::containers::unix_now()));
            std::fs::create_dir_all(d2.vms.iso_dir())?;
            t.log(format!("downloading {}", b.url));
            let h = tokio::runtime::Handle::current();
            let n = h.block_on(async {
                use tokio::io::AsyncWriteExt;
                let r = reqwest::Client::new().get(&b.url).send().await?.error_for_status()?;
                let total = r.content_length();
                let mut f = tokio::fs::File::create(&tmp).await?;
                let mut s = r.bytes_stream();
                let (mut n, mut last) = (0u64, 0u64);
                while let Some(c) = s.next().await {
                    let c = c?;
                    f.write_all(&c).await?;
                    n += c.len() as u64;
                    if n - last > 64 << 20 {
                        last = n;
                        t.log(format!("  {} MiB{}", n >> 20, total.map(|x| format!(" of {}", x >> 20)).unwrap_or_default()));
                    }
                    if t.cancelled() {
                        anyhow::bail!("stopped");
                    }
                }
                f.flush().await?;
                Ok::<u64, anyhow::Error>(n)
            });
            match n {
                Ok(n) => t.log(format!("downloaded {} MiB", n >> 20)),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            }
            Some(tmp.clone())
        } else {
            None
        };
        let path = src.clone().unwrap_or_else(|| d2.vms.iso_dir().join(&b.file));
        let r = d2.vms.pools.import(&path, &vol, b.grow);
        if let Some(t) = src {
            let _ = std::fs::remove_file(t);
        }
        let p = r?;
        t.log(format!("imported as {vol} ({})", p.display()));
        Ok(())
    }) {
        Ok(id) => Json(json!({"task": id})).into_response(),
        Err(e) => bad(e),
    }
}
