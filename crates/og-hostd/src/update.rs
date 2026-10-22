//! Updates: signed releases from one or more mirrors, installed in place with automatic
//! rollback.
//!
//! A mirror is a plain directory over HTTPS (any web server; `og-hostd mirror sync` copies
//! one): `channels/<channel>.json` (+ `.sig`), `releases/<version>/og-host-<version>-<arch>.tar.zst`,
//! `keys/`. A channel file is signed with an ed25519 release key; og-hostd only believes
//! manifests signed by a key it trusts (built in, or in `<root>/trusted-keys/*.pub` /
//! `/etc/og-host/trusted-keys/*.pub`), and only installs artifacts whose SHA-256 the signed
//! manifest gives.
//!
//! Installing swaps the binaries (keeping the previous ones) and restarts the daemon;
//! containers and VMs keep running. The new version counts its start attempts: if it
//! doesn't stay up for a minute three times in a row, the previous binaries come back.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;

/// Release keys built into this binary (hex ed25519 public keys).
/// The OmegaGiven release key (key id from `og-hostd release keygen`).
pub const BUILTIN_KEYS: &[&str] = &["2e46d42da7b8a5d730aa7e96342f0a2da35dc129e4dee948e62175563d43edb3"];

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// stable | beta | dev
    pub channel: String,
    pub mirrors: Vec<String>,
    /// notify | download | install
    pub auto: String,
    pub schedule: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { channel: "stable".into(), mirrors: vec![], auto: "notify".into(), schedule: "daily 03:15".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Artifact {
    pub arch: String,
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Manifest {
    pub format: u32,
    pub channel: String,
    pub version: String,
    pub date: u64,
    pub notes: String,
    pub artifacts: Vec<Artifact>,
}

pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// `1.2.3` / `1.2.3-beta.4`: compare numerically, a pre-release sorts before its release.
pub fn version_key(v: &str) -> (Vec<u64>, u8, String) {
    let (main, pre) = v.trim_start_matches('v').split_once('-').map(|(a, b)| (a, Some(b))).unwrap_or((v.trim_start_matches('v'), None));
    let nums = main.split('.').map(|x| x.parse().unwrap_or(0)).collect();
    (nums, if pre.is_some() { 0 } else { 1 }, pre.unwrap_or("").to_string())
}

pub fn newer(a: &str, b: &str) -> bool {
    version_key(a) > version_key(b)
}

fn config_path(root: &Path) -> PathBuf {
    root.join("update.json")
}

pub fn config(root: &Path) -> Config {
    std::fs::read(config_path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_config(root: &Path, c: &Config) -> Result<()> {
    if !["stable", "beta", "dev"].contains(&c.channel.as_str()) {
        bail!("channel is stable, beta or dev");
    }
    if !["notify", "download", "install"].contains(&c.auto.as_str()) {
        bail!("automatic updates: notify, download or install");
    }
    for m in &c.mirrors {
        if !(m.starts_with("https://") || m.starts_with("http://")) || m.contains(char::is_whitespace) {
            bail!("mirrors are http(s) URLs");
        }
    }
    crate::schedule::Schedule::parse(&c.schedule)?;
    std::fs::write(config_path(root), serde_json::to_vec_pretty(c)?)?;
    Ok(())
}

/// Trusted release keys: (key id, key).
pub fn trusted_keys(root: &Path) -> Vec<(String, ed25519_dalek::VerifyingKey)> {
    let mut hexes: Vec<String> = BUILTIN_KEYS.iter().map(|s| s.to_string()).collect();
    for dir in [root.join("trusted-keys"), PathBuf::from("/etc/og-host/trusted-keys")] {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.path().extension().is_some_and(|x| x == "pub") {
                if let Ok(t) = std::fs::read_to_string(e.path()) {
                    hexes.push(t.trim().to_string());
                }
            }
        }
    }
    hexes
        .iter()
        .filter_map(|h| {
            let b: [u8; 32] = hex::decode(h.trim()).ok()?.try_into().ok()?;
            let k = ed25519_dalek::VerifyingKey::from_bytes(&b).ok()?;
            Some((key_id(&k), k))
        })
        .collect()
}

pub fn key_id(k: &ed25519_dalek::VerifyingKey) -> String {
    hex::encode(&sha2::Sha256::digest(k.as_bytes())[..8])
}

/// Check a channel file against its signature (`<hex sig>\n<key id>`).
pub fn verify(root: &Path, manifest: &[u8], sig: &str) -> Result<String> {
    let mut lines = sig.lines();
    let s: [u8; 64] = hex::decode(lines.next().unwrap_or("").trim()).ok().and_then(|b| b.try_into().ok()).context("bad signature file")?;
    let kid = lines.next().unwrap_or("").trim().to_string();
    let sig = ed25519_dalek::Signature::from_bytes(&s);
    let keys = trusted_keys(root);
    if keys.is_empty() {
        bail!("no trusted release keys (put one in {}/trusted-keys/)", root.display());
    }
    for (id, k) in &keys {
        if (kid.is_empty() || *id == kid) && k.verify_strict(manifest, &sig).is_ok() {
            return Ok(id.clone());
        }
    }
    bail!("the release isn't signed by a trusted key")
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().use_rustls_tls().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(1800)).user_agent(format!("og-hostd/{VERSION}")).build()?)
}

#[derive(Debug, Clone, Serialize)]
pub struct Available {
    pub mirror: String,
    pub key: String,
    pub manifest: Manifest,
    pub newer: bool,
}

/// Ask the mirrors (in order) for the channel's release. Errors from each mirror are listed if
/// none answered with a validly signed manifest.
pub async fn check(root: &Path) -> Result<Available> {
    let cfg = config(root);
    if cfg.mirrors.is_empty() {
        bail!("no update mirror set (Settings → Updates)");
    }
    let c = client()?;
    let mut errs = vec![];
    for m in &cfg.mirrors {
        let base = m.trim_end_matches('/');
        let r = async {
            let body = c.get(format!("{base}/channels/{}.json", cfg.channel)).send().await?.error_for_status()?.bytes().await?;
            let sig = c.get(format!("{base}/channels/{}.json.sig", cfg.channel)).send().await?.error_for_status()?.text().await?;
            let key = verify(root, &body, &sig)?;
            let man: Manifest = serde_json::from_slice(&body)?;
            if man.channel != cfg.channel {
                bail!("the mirror's {} file is for channel {}", cfg.channel, man.channel);
            }
            Ok::<_, anyhow::Error>(Available { mirror: base.to_string(), key, newer: newer(&man.version, VERSION), manifest: man })
        }
        .await;
        match r {
            Ok(a) => return Ok(a),
            Err(e) => errs.push(format!("{base}: {e:#}")),
        }
    }
    bail!("{}", errs.join("; "))
}

fn updates_dir(root: &Path) -> PathBuf {
    root.join("updates")
}

/// The installed binaries this updater manages.
fn targets() -> Vec<(&'static str, PathBuf)> {
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/local/bin/og-hostd"));
    let dir = me.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/usr/local/bin"));
    vec![("og-hostd", dir.join("og-hostd")), ("og-shim", dir.join("og-shim"))]
}

/// Download, check and install a release, then restart. Blocking (run as a task).
pub fn install(root: &Path, task: &crate::tasks::Task, a: &Available) -> Result<()> {
    let art = a.manifest.artifacts.iter().find(|x| x.arch == arch()).with_context(|| format!("release {} has no build for {}", a.manifest.version, arch()))?;
    if art.file.contains("..") || art.file.starts_with('/') {
        bail!("bad artifact path in the manifest");
    }
    let dir = updates_dir(root).join(&a.manifest.version);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let url = format!("{}/{}", a.mirror, art.file);
    task.log(format!("downloading {url} ({:.1} MiB)", art.size as f64 / 1048576.0));
    let h = tokio::runtime::Handle::current();
    let bytes = h.block_on(async { client()?.get(&url).send().await?.error_for_status()?.bytes().await.map_err(anyhow::Error::from) })?;
    let got = hex::encode(sha2::Sha256::digest(&bytes));
    if got != art.sha256 {
        bail!("checksum mismatch (got {got}): not installing");
    }
    task.log(format!("checksum ok, signed by key {}", a.key));
    let mut ar = tar::Archive::new(zstd::stream::read::Decoder::new(&bytes[..])?);
    ar.set_preserve_permissions(true);
    for e in ar.entries()? {
        let mut e = e?;
        if !e.unpack_in(&dir)? {
            bail!("bad path in the release archive");
        }
    }
    let new_bin = dir.join("bin/og-hostd");
    let out = std::process::Command::new(&new_bin).arg("--version").output().context("the new og-hostd doesn't run here")?;
    let said = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !said.contains(&a.manifest.version) {
        bail!("the new og-hostd says {:?}, expected {}", said.trim(), a.manifest.version);
    }
    task.log(format!("new binary runs: {}", said.trim()));
    // keep the current binaries for a rollback
    let prev = updates_dir(root).join("previous");
    let _ = std::fs::remove_dir_all(&prev);
    std::fs::create_dir_all(&prev)?;
    for (name, path) in targets() {
        if path.exists() {
            std::fs::copy(&path, prev.join(name))?;
        }
    }
    for (name, path) in targets() {
        let src = dir.join("bin").join(name);
        if !src.exists() {
            continue;
        }
        let tmp = path.with_extension("og-new");
        std::fs::copy(&src, &tmp)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        task.log(format!("installed {}", path.display()));
    }
    std::fs::write(updates_dir(root).join("pending.json"), serde_json::to_vec(&json!({"from": VERSION, "to": a.manifest.version, "attempts": 0, "time": crate::containers::unix_now()}))?)?;
    install_guard(root, task);
    task.log("restarting og-hostd (containers and VMs keep running)");
    restart_soon();
    Ok(())
}

/// Restart the daemon after this request has answered.
pub fn restart_soon() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(2));
        if std::env::var_os("INVOCATION_ID").is_some() {
            let _ = std::process::Command::new("systemctl").args(["--no-block", "restart", "og-hostd"]).status();
        } else {
            // not under systemd: start the new binary in our place
            use std::os::unix::process::CommandExt;
            let args: Vec<String> = std::env::args().skip(1).collect();
            let exe = targets()[0].1.clone();
            let err = std::process::Command::new(exe).args(args).exec();
            eprintln!("og-hostd: restart failed: {err}");
        }
    });
}

/// Under systemd the counting is done before og-hostd starts, by a copy of the previous
/// (known-good) og-hostd run as ExecStartPre: so even a new binary that can't start at all
/// is rolled back.
fn install_guard(root: &Path, task: &crate::tasks::Task) {
    if std::env::var_os("INVOCATION_ID").is_none() {
        return;
    }
    let r = (|| -> Result<()> {
        let g = updates_dir(root).join("guard");
        std::fs::create_dir_all(&g)?;
        std::fs::copy(updates_dir(root).join("previous/og-hostd"), g.join("og-hostd.tmp"))?;
        std::fs::rename(g.join("og-hostd.tmp"), g.join("og-hostd"))?;
        let dropin = Path::new("/etc/systemd/system/og-hostd.service.d");
        std::fs::create_dir_all(dropin)?;
        std::fs::write(dropin.join("update-guard.conf"), format!("# OG-Host update guard: rolls back a new version that keeps failing to start\n[Service]\nExecStartPre=-{} --root {} update-guard\n", g.join("og-hostd").display(), root.display()))?;
        let st = std::process::Command::new("systemctl").arg("daemon-reload").status()?;
        if !st.success() {
            bail!("systemctl daemon-reload failed");
        }
        Ok(())
    })();
    if let Err(e) = r {
        task.warn(format!("start guard not installed ({e:#}); the new version checks itself"));
    }
}

/// `og-hostd update-guard` (ExecStartPre): count this start of an update on trial; after three
/// that didn't stay up a minute, put the previous binaries back before systemd starts one.
pub fn guard(root: &Path) {
    let p = updates_dir(root).join("pending.json");
    let Some(mut v) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return };
    let attempts = v["attempts"].as_u64().unwrap_or(0) + 1;
    v["attempts"] = json!(attempts);
    v["guarded"] = json!(true);
    let _ = std::fs::write(&p, serde_json::to_vec(&v).unwrap_or_default());
    if attempts <= 3 {
        return;
    }
    eprintln!("og-hostd update guard: {} didn't stay up; putting {} back", v["to"].as_str().unwrap_or("?"), v["from"].as_str().unwrap_or("?"));
    rollback(root, &mut v);
}

fn rollback(root: &Path, v: &mut Value) {
    let prev = updates_dir(root).join("previous");
    for (name, path) in targets() {
        let src = prev.join(name);
        if src.exists() {
            let tmp = path.with_extension("og-new");
            if std::fs::copy(&src, &tmp).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    let _ = std::fs::remove_file(updates_dir(root).join("pending.json"));
    v["status"] = json!("rolled back");
    v["done"] = json!(crate::containers::unix_now());
    let _ = std::fs::write(updates_dir(root).join("last.json"), serde_json::to_vec(&v).unwrap_or_default());
}

/// First thing at `serve`: an update on trial counts its starts (unless the systemd guard
/// already did); after three that didn't last a minute, put the previous binaries back and run
/// them instead. After a minute up, the update is kept.
pub fn startup_check(root: &Path) {
    let p = updates_dir(root).join("pending.json");
    let Some(mut v) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return };
    let guarded = v["guarded"] == json!(true) && std::env::var_os("INVOCATION_ID").is_some();
    let attempts = if guarded { v["attempts"].as_u64().unwrap_or(1) } else { v["attempts"].as_u64().unwrap_or(0) + 1 };
    v["attempts"] = json!(attempts);
    let _ = std::fs::write(&p, serde_json::to_vec(&v).unwrap_or_default());
    if attempts <= 3 {
        eprintln!("og-hostd: update to {} on trial (start {attempts} of 3)", v["to"].as_str().unwrap_or("?"));
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            let pend = updates_dir(&root).join("pending.json");
            if let Ok(b) = std::fs::read(&pend) {
                let _ = std::fs::remove_file(&pend);
                let mut v: Value = serde_json::from_slice(&b).unwrap_or_default();
                v["status"] = json!("ok");
                v["done"] = json!(crate::containers::unix_now());
                let _ = std::fs::write(updates_dir(&root).join("last.json"), serde_json::to_vec(&v).unwrap_or_default());
                eprintln!("og-hostd: update to {} kept", v["to"].as_str().unwrap_or("?"));
            }
        });
        return;
    }
    eprintln!("og-hostd: the update to {} didn't stay up; going back to {}", v["to"].as_str().unwrap_or("?"), v["from"].as_str().unwrap_or("?"));
    rollback(root, &mut v);
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&targets()[0].1).args(std::env::args().skip(1)).exec();
    eprintln!("og-hostd: running the previous version failed: {err}");
}

pub fn last(root: &Path) -> Value {
    let pending = std::fs::read(updates_dir(root).join("pending.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let last = std::fs::read(updates_dir(root).join("last.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    json!({"pending": pending, "last": last})
}

// ---- system packages (Debian/Ubuntu hosts) ----

pub fn apt_upgradable() -> Result<Vec<Value>> {
    if !Path::new("/usr/bin/apt").exists() {
        bail!("this server has no apt");
    }
    let out = std::process::Command::new("apt").args(["list", "--upgradable"]).env("LANG", "C").output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("[upgradable from:"))
        .filter_map(|l| {
            // name/suite version arch [upgradable from: old]
            let (name, rest) = l.split_once('/')?;
            let f: Vec<&str> = rest.split_whitespace().collect();
            Some(json!({"name": name, "version": f.get(1), "from": l.rsplit_once("from: ").map(|x| x.1.trim_end_matches(']'))}))
        })
        .collect())
}

pub fn apt_run(task: &crate::tasks::Task, upgrade: bool) -> Result<()> {
    let mut steps: Vec<Vec<&str>> = vec![vec!["update"]];
    if upgrade {
        steps.push(vec!["-y", "-o", "Dpkg::Options::=--force-confdef", "-o", "Dpkg::Options::=--force-confold", "full-upgrade"]);
    }
    for args in steps {
        task.check()?;
        task.log(format!("apt-get {}", args.join(" ")));
        let mut c = std::process::Command::new("apt-get");
        c.args(&args).env("DEBIAN_FRONTEND", "noninteractive").env("LANG", "C").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let mut child = c.spawn()?;
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let t2 = std::thread::scope(|s| {
            let h = s.spawn(|| {
                use std::io::BufRead;
                for l in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                    task.log(format!("  {l}"));
                }
            });
            use std::io::BufRead;
            for l in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                task.log(format!("  {l}"));
            }
            let _ = h.join();
            child.wait()
        })?;
        if !t2.success() {
            bail!("apt-get {} failed", args.last().unwrap_or(&""));
        }
    }
    if Path::new("/var/run/reboot-required").exists() {
        task.warn("the system wants a reboot (new kernel or libraries)");
    }
    Ok(())
}

// ---- API ----

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use og_access::Who;
use std::sync::Arc;

use crate::serve::Daemon;

type D = State<Arc<Daemon>>;

fn bad(e: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response()
}

pub fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/v1/update", get(info))
        .route("/v1/update/config", put(set_config))
        .route("/v1/update/check", post(check_now))
        .route("/v1/update/install", post(install_now))
        .route("/v1/update/cluster", post(cluster_update))
        .route("/v1/update/packages", get(packages).post(packages_run))
        .with_state(d)
}

async fn info(State(d): D) -> Json<Value> {
    let keys: Vec<String> = trusted_keys(&d.root).into_iter().map(|(id, _)| id).collect();
    let mut v = last(&d.root);
    v["version"] = json!(VERSION);
    v["arch"] = json!(arch());
    v["config"] = json!(config(&d.root));
    v["keys"] = json!(keys);
    v["available"] = d.update_seen.lock().unwrap().clone().map(|a| json!(a)).unwrap_or(Value::Null);
    Json(v)
}

async fn set_config(State(d): D, Json(c): Json<Config>) -> Response {
    match save_config(&d.root, &c) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

async fn check_now(State(d): D) -> Response {
    match check(&d.root).await {
        Ok(a) => {
            *d.update_seen.lock().unwrap() = Some(a.clone());
            Json(json!(a)).into_response()
        }
        Err(e) => bad(format!("{e:#}")),
    }
}

pub fn start_install(d: &Arc<Daemon>, user: &str) -> Result<String> {
    if d.tasks.busy("update", "og-hostd") {
        bail!("an update is running already");
    }
    let root = d.root.clone();
    let d2 = d.clone();
    d.tasks.spawn("update", "og-hostd", user, move |t| {
        let a = tokio::runtime::Handle::current().block_on(check(&root))?;
        if !a.newer {
            t.log(format!("already on {VERSION} (the mirror has {})", a.manifest.version));
            return Ok(());
        }
        t.log(format!("updating {VERSION} -> {}", a.manifest.version));
        *d2.update_seen.lock().unwrap() = Some(a.clone());
        install(&root, t, &a)
    })
}

async fn install_now(State(d): D, Extension(w): Extension<Who>) -> Response {
    match start_install(&d, &w.label()) {
        Ok(t) => Json(json!({"task": t})).into_response(),
        Err(e) => bad(e),
    }
}

/// Update every node of the cluster, one at a time, this one last; stop at the first that
/// doesn't come back on the new version.
async fn cluster_update(State(d): D, Extension(w): Extension<Who>) -> Response {
    let Some(st) = d.cluster.state() else { return bad("not in a cluster: use Install") };
    let d2 = d.clone();
    let who = w.clone();
    match d.tasks.spawn("update", "cluster", &w.label(), move |t| {
        let h = tokio::runtime::Handle::current();
        let a = h.block_on(check(&d2.root))?;
        t.log(format!("target version {}", a.manifest.version));
        let mut order: Vec<String> = st.nodes.iter().map(|n| n.name.clone()).filter(|n| *n != d2.cluster.me).collect();
        order.push(d2.cluster.me.clone());
        for n in order {
            t.check()?;
            if n == d2.cluster.me {
                if !newer(&a.manifest.version, VERSION) {
                    t.log(format!("{n}: already on {VERSION}"));
                    continue;
                }
                t.log(format!("{n}: installing (this node restarts last)"));
                install(&d2.root, t, &a)?;
                continue;
            }
            let ver = |d: &Daemon| d.cluster.peers.lock().unwrap().get(&n).and_then(|p| p.info["version"].as_str().map(String::from));
            if ver(&d2).as_deref() == Some(a.manifest.version.as_str()) {
                t.log(format!("{n}: already on {}", a.manifest.version));
                continue;
            }
            t.log(format!("{n}: installing"));
            let path = "/v1/update/install".to_string();
            let r: Value = h.block_on(crate::cluster::call_node(&d2, &n, &who, reqwest::Method::POST, &path))?;
            t.log(format!("{n}: task {}", r["task"].as_str().unwrap_or("?")));
            let deadline = std::time::Instant::now() + Duration::from_secs(600);
            loop {
                std::thread::sleep(Duration::from_secs(5));
                if ver(&d2).as_deref() == Some(a.manifest.version.as_str()) {
                    t.log(format!("{n}: back up on {}", a.manifest.version));
                    break;
                }
                if std::time::Instant::now() > deadline {
                    bail!("{n} didn't come back on {} within 10 minutes; stopping here", a.manifest.version);
                }
            }
        }
        Ok(())
    }) {
        Ok(id) => Json(json!({"task": id})).into_response(),
        Err(e) => bad(e),
    }
}

async fn packages() -> Response {
    match tokio::task::spawn_blocking(apt_upgradable).await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}"))) {
        Ok(v) => Json(json!(v)).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PkgReq {
    upgrade: bool,
}

async fn packages_run(State(d): D, Extension(w): Extension<Who>, Json(b): Json<PkgReq>) -> Response {
    match d.tasks.spawn("apt", if b.upgrade { "upgrade" } else { "refresh" }, &w.label(), move |t| apt_run(t, b.upgrade)) {
        Ok(id) => Json(json!({"task": id})).into_response(),
        Err(e) => bad(e),
    }
}

/// On the update schedule: check, then tell / download / install as configured.
pub async fn auto(d: Arc<Daemon>) {
    let cfg = config(&d.root);
    if cfg.mirrors.is_empty() {
        return;
    }
    match check(&d.root).await {
        Ok(a) => {
            let fresh = d.update_seen.lock().unwrap().as_ref().map(|x| x.manifest.version.clone()) != Some(a.manifest.version.clone());
            *d.update_seen.lock().unwrap() = Some(a.clone());
            if !a.newer {
                return;
            }
            if cfg.auto == "install" {
                let _ = start_install(&d, "system");
            } else if fresh {
                if let Some(lab) = d.lab.clone() {
                    let text = format!("OG-Host {}: version {} is available (now {VERSION})", crate::tls::hostname(), a.manifest.version);
                    let _ = og_lab::notify::send_signal(&lab, &text).await;
                }
            }
        }
        Err(e) => eprintln!("og-hostd: update check: {e:#}"),
    }
}
