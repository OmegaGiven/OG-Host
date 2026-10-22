//! High availability for VMs, by replication: no shared storage needed.
//!
//! An HA VM has an owner node (where it runs) and a standby node. Every `interval_min` the
//! owner backs it up (paused for a moment, incremental) and copies that to the standby's
//! datastore. If the owner is gone for `TAKEOVER_AFTER`, the standby restores the latest copy
//! and starts the VM (losing at most one interval of changes), and becomes its owner.
//!
//! Split brain (the VM running twice) is avoided the way Proxmox does it, without a
//! hardware watchdog: a node only takes over when it has quorum (more than half the nodes)
//! or, in a two-node cluster, when it can still reach the witness (an address both sites
//! should see, e.g. a router or a third box); and an owner that can reach neither the others
//! nor the witness for `FENCE_AFTER` stops its HA VMs itself, well before anyone takes over.
//! Ownership lives in a cluster-wide document, so a returning node doesn't start a VM that
//! moved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::serve::Daemon;

// the owner fences itself at FENCE_AFTER; a minute later the standby may take over
pub const TAKEOVER_AFTER: Duration = Duration::from_secs(120);
pub const FENCE_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Resource {
    pub enabled: bool,
    pub owner: String,
    pub standby: String,
    pub interval_min: u64,
    /// datastore used on both sides
    pub store: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Doc {
    pub version: u64,
    pub origin: String,
    /// address to ping when deciding who's isolated (two-node clusters)
    pub witness: String,
    pub resources: BTreeMap<String, Resource>,
}

fn path(root: &Path) -> PathBuf {
    root.join("cluster").join("ha.json")
}

pub fn doc(root: &Path) -> Doc {
    std::fs::read(path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(root: &Path, d: &Doc) -> Result<()> {
    let p = path(root);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(d)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// Change the document (new version from this node) and tell the others.
pub fn update(d: &Arc<Daemon>, f: impl FnOnce(&mut Doc) -> Result<()>) -> Result<Doc> {
    let mut doc = doc(&d.root);
    f(&mut doc)?;
    doc.version += 1;
    doc.origin = d.cluster.me.clone();
    save(&d.root, &doc)?;
    let d2 = d.clone();
    tokio::runtime::Handle::current().spawn(async move { d2.cluster.push_all_pub().await });
    Ok(doc)
}

/// A copy from another node: kept if newer.
pub fn apply_remote(root: &Path, v: &Value) -> bool {
    let Ok(remote) = serde_json::from_value::<Doc>(v.clone()) else { return false };
    let cur = doc(root);
    if (remote.version, remote.origin.as_str()) <= (cur.version, cur.origin.as_str()) {
        return false;
    }
    save(root, &remote).is_ok()
}

/// May the normal autostart start this VM? Not HA VMs: the HA loop starts those once it
/// knows who owns them now.
pub fn may_run(d: &Daemon, vm: &str) -> bool {
    !doc(&d.root).resources.get(vm).is_some_and(|r| r.enabled)
}

fn ping(addr: &str) -> bool {
    !addr.is_empty() && std::process::Command::new("ping").args(["-c", "1", "-W", "2", addr]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

#[derive(Default)]
pub struct State {
    last_replica: BTreeMap<String, Instant>,
    replicating: BTreeMap<String, bool>,
    /// since when we've had neither quorum nor the witness
    isolated_since: Option<Instant>,
    /// since when each node has been offline (from our view)
    offline_since: BTreeMap<String, Instant>,
    pub log: Vec<String>,
    /// the ownership document is current (pulled from a peer, or nobody to ask)
    synced: bool,
    started_at: Option<Instant>,
    start_failed: BTreeMap<String, Instant>,
    /// last round: (quorum, witness reachable, may act)
    last_round: Option<(bool, bool, bool)>,
    takeover_tried: BTreeMap<String, Instant>,
}

fn note(d: &Daemon, msg: String) {
    eprintln!("og-hostd ha: {msg}");
    d.access.audit("system", "", "ha", &msg, true);
    let mut st = d.ha.lock().unwrap();
    st.log.push(format!("{} {msg}", og_backup::time_name(crate::containers::unix_now())));
    let n = st.log.len();
    if n > 100 {
        st.log.drain(..n - 100);
    }
}

/// Every 10 s: replicate what we own, fence ourselves if isolated, take over what's ours to.
pub async fn background(d: Arc<Daemon>) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let Some(cl) = d.cluster.state() else { continue };
        let me = d.cluster.me.clone();
        // after a start: the newest ownership document from the others first
        if !d.ha.lock().unwrap().synced {
            let started = *d.ha.lock().unwrap().started_at.get_or_insert_with(Instant::now);
            let mut got = false;
            if let Ok(c) = d.cluster.client_pub() {
                for n in cl.nodes.iter().filter(|n| n.name != d.cluster.me) {
                    if let Ok(r) = c.get(format!("https://{}/cluster/v1/ha", n.addr)).timeout(Duration::from_secs(5)).send().await {
                        if let Ok(v) = r.json::<Value>().await {
                            apply_remote(&d.root, &v);
                            got = true;
                        }
                    }
                }
            }
            // alone for 2 minutes: our own copy is all there is
            if got || started.elapsed() > Duration::from_secs(120) {
                d.ha.lock().unwrap().synced = true;
            } else {
                continue;
            }
        }
        let doc = doc(&d.root);
        if doc.resources.values().all(|r| !r.enabled) {
            continue;
        }
        // who's online (from here)
        let peers = d.cluster.peers.lock().unwrap().clone();
        let total = cl.nodes.len();
        let online = 1 + cl.nodes.iter().filter(|n| n.name != me && peers.get(&n.name).is_some_and(|p| p.online)).count();
        let quorum = online * 2 > total;
        let witness = if quorum { true } else { tokio::task::spawn_blocking({ let w = doc.witness.clone(); move || ping(&w) }).await.unwrap_or(false) };
        let can_act = quorum || (total == 2 && witness);
        d.ha.lock().unwrap().last_round = Some((quorum, witness, can_act));
        {
            let mut st = d.ha.lock().unwrap();
            for n in cl.nodes.iter().filter(|n| n.name != me) {
                let on = peers.get(&n.name).is_some_and(|p| p.online);
                if on {
                    st.offline_since.remove(&n.name);
                } else {
                    st.offline_since.entry(n.name.clone()).or_insert_with(Instant::now);
                }
            }
            if can_act {
                st.isolated_since = None;
            } else if st.isolated_since.is_none() {
                st.isolated_since = Some(Instant::now());
            }
        }
        // 1. fence: isolated owner stops its HA VMs
        let isolated_for = d.ha.lock().unwrap().isolated_since.map(|t| t.elapsed());
        if isolated_for.is_some_and(|t| t > FENCE_AFTER) {
            for (name, _r) in doc.resources.iter().filter(|(_, r)| r.enabled && r.owner == me) {
                if d.vms.status(name).state != "stopped" {
                    note(&d, format!("{name}: this node is cut off (no quorum, no witness): stopping it so it can run elsewhere"));
                    let d2 = d.clone();
                    let n = name.clone();
                    let _ = tokio::task::spawn_blocking(move || crate::vmrun::stop(&d2, &n, true, 0)).await;
                }
            }
            continue;
        }
        // 2. VMs that moved away while we were gone: keep them off
        for (name, r) in doc.resources.iter().filter(|(_, r)| r.enabled && r.owner != me) {
            if d.vms.get(name).is_ok() && d.vms.status(name).state != "stopped" {
                note(&d, format!("{name}: now owned by {}; stopping the copy here", r.owner));
                let d2 = d.clone();
                let n = name.clone();
                let _ = tokio::task::spawn_blocking(move || crate::vmrun::stop(&d2, &n, true, 0)).await;
            }
        }
        for (name, _r) in doc.resources.iter().filter(|(_, r)| r.enabled && r.owner == me) {
            if let Ok(vm) = d.vms.get(name) {
                let waited = d.ha.lock().unwrap().start_failed.get(name).is_none_or(|t| t.elapsed() > Duration::from_secs(120));
                if vm.desired == "running" && d.vms.status(name).state == "stopped" && !d.tasks.busy("ha-takeover", name) && waited {
                    let d2 = d.clone();
                    let n = name.clone();
                    match tokio::task::spawn_blocking(move || crate::vmrun::start(&d2, &n)).await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}"))) {
                        Ok(()) => {
                            d.ha.lock().unwrap().start_failed.remove(name);
                            note(&d, format!("{name}: started (HA, owned here)"));
                        }
                        Err(e) => {
                            d.ha.lock().unwrap().start_failed.insert(name.clone(), Instant::now());
                            note(&d, format!("{name}: won't start: {e:#} (trying again in 2 minutes)"));
                        }
                    }
                }
            }
        }
        for (name, r) in doc.resources.iter().filter(|(_, r)| r.enabled) {
            // 3. replicate what we own
            if r.owner == me && d.vms.get(name).is_ok() {
                let due = d.ha.lock().unwrap().last_replica.get(name).is_none_or(|t| t.elapsed() >= Duration::from_secs(r.interval_min.max(1) * 60));
                let busy = d.ha.lock().unwrap().replicating.get(name).copied().unwrap_or(false);
                let standby_up = peers.get(&r.standby).is_some_and(|p| p.online);
                if due && !busy && standby_up {
                    d.ha.lock().unwrap().replicating.insert(name.clone(), true);
                    let d2 = d.clone();
                    let (n, res) = (name.clone(), r.clone());
                    let _ = d.tasks.spawn("ha-replicate", name, "system", move |t| {
                        let r = replicate(&d2, t, &n, &res);
                        let mut st = d2.ha.lock().unwrap();
                        st.replicating.insert(n.clone(), false);
                        if r.is_ok() {
                            st.last_replica.insert(n.clone(), Instant::now());
                        }
                        r
                    });
                }
            }
            // 4. take over from a dead owner
            if r.standby == me && r.owner != me && can_act {
                let gone = d.ha.lock().unwrap().offline_since.get(&r.owner).is_some_and(|t| t.elapsed() > TAKEOVER_AFTER);
                let retry = d.ha.lock().unwrap().takeover_tried.get(name).is_none_or(|t| t.elapsed() > Duration::from_secs(60));
                if gone && retry && !d.tasks.busy("ha-takeover", name) {
                    d.ha.lock().unwrap().takeover_tried.insert(name.clone(), Instant::now());
                    let d2 = d.clone();
                    let (n, res) = (name.clone(), r.clone());
                    let _ = d.tasks.spawn("ha-takeover", name, "system", move |t| takeover(&d2, t, &n, &res));
                }
            }
        }
    }
}

fn replicate(d: &Arc<Daemon>, t: &crate::tasks::Task, name: &str, r: &Resource) -> Result<()> {
    let opts = crate::backup::Opts { mode: "pause".into(), include_binds: false, notes: "HA replica".into() };
    crate::backup::run(d, t, &r.store, &crate::backup::Target::Vm(name.into()), &opts)?;
    let job = crate::cluster::SyncJob { id: format!("ha-{name}"), enabled: true, store: r.store.clone(), node: r.standby.clone(), remote_store: r.store.clone(), groups: vec![format!("vm/{name}")], keep: og_backup::prune::Keep { last: 3, ..Default::default() }, ..Default::default() };
    tokio::runtime::Handle::current().block_on(crate::cluster::run_sync_pub(d, t, &job))?;
    // and keep only a few replicas here too
    let (_, repo) = d.storage.repo(&r.store)?;
    let _ = crate::backup::prune(&repo, &format!("vm/{name}"), &og_backup::prune::Keep { last: 3, ..Default::default() }, false);
    Ok(())
}

fn takeover(d: &Arc<Daemon>, t: &crate::tasks::Task, name: &str, r: &Resource) -> Result<()> {
    let old = r.owner.clone();
    t.log(format!("{old} has been gone for over {} s: starting {name} here from its latest replica", TAKEOVER_AFTER.as_secs()));
    let (_, repo) = d.storage.repo(&r.store)?;
    // the newest replica that really holds every disk
    let complete = |m: &og_backup::Manifest| {
        m.meta["volumes"].as_array().into_iter().flatten().all(|v| {
            let a = v["archive"].as_str().unwrap_or("");
            // anything more than an empty tar (disks are stored sparse, so smaller than their size)
            m.archives.iter().any(|x| x.name == a && x.size > 2048)
        })
    };
    let snap = repo.snapshots().into_iter().filter(|m| m.group == format!("vm/{name}") && complete(m)).max_by_key(|m| m.time).context("no complete replica of this VM here: not taking over")?;
    t.log(format!("replica from {} ({} ago)", og_backup::time_name(snap.time), crate::containers::unix_now().saturating_sub(snap.time)));
    // ownership first, so the old owner keeps its copy off when it comes back
    let me = d.cluster.me.clone();
    update(d, |doc| {
        let res = doc.resources.get_mut(name).context("no longer an HA VM")?;
        res.owner = me.clone();
        res.standby = old.clone();
        Ok(())
    })?;
    if d.vms.get(name).is_ok() {
        let _ = crate::vmrun::stop(d, name, true, 0);
        d.vms.remove(&d.node.net, name)?;
    }
    crate::backup::restore(d, t, &r.store, &snap.id(), &crate::backup::RestoreOpts { start: true, ..Default::default() })?;
    note(d, format!("{name}: taken over from {old} (replica {})", og_backup::time_name(snap.time)));
    Ok(())
}

// ---- API ----

use axum::extract::{Path as UrlPath, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};

pub fn router(d: Arc<Daemon>) -> Router {
    Router::new().route("/v1/ha", get(status)).route("/v1/ha/witness", put(set_witness)).route("/v1/ha/{vm}", put(set_vm).delete(del_vm)).with_state(d)
}

fn bad(e: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response()
}

async fn status(AxState(d): AxState<Arc<Daemon>>) -> Json<Value> {
    let doc = doc(&d.root);
    let st = d.ha.lock().unwrap();
    let res: Vec<Value> = doc
        .resources
        .iter()
        .map(|(n, r)| json!({"vm": n, "resource": r, "last_replica_s": st.last_replica.get(n).map(|t| t.elapsed().as_secs()), "replicating": st.replicating.get(n).copied().unwrap_or(false)}))
        .collect();
    Json(json!({"witness": doc.witness, "resources": res, "isolated_s": st.isolated_since.map(|t| t.elapsed().as_secs()), "synced": st.synced, "round": st.last_round, "offline_s": st.offline_since.iter().map(|(k, t)| (k.clone(), t.elapsed().as_secs())).collect::<BTreeMap<_, _>>(), "log": st.log.iter().rev().take(30).collect::<Vec<_>>(), "takeover_after_s": TAKEOVER_AFTER.as_secs(), "fence_after_s": FENCE_AFTER.as_secs()}))
}

#[derive(Deserialize)]
struct WitnessReq {
    witness: String,
}

async fn set_witness(AxState(d): AxState<Arc<Daemon>>, Json(b): Json<WitnessReq>) -> Response {
    let w = b.witness.trim().to_string();
    if !w.is_empty() && !w.chars().all(|c| c.is_ascii_alphanumeric() || ".:-".contains(c)) {
        return bad("give an address or host name");
    }
    match update(&d, |doc| {
        doc.witness = w;
        Ok(())
    }) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct VmReq {
    #[serde(default = "yes")]
    enabled: bool,
    standby: String,
    #[serde(default)]
    interval_min: Option<u64>,
    #[serde(default)]
    store: Option<String>,
}

fn yes() -> bool {
    true
}

async fn set_vm(AxState(d): AxState<Arc<Daemon>>, UrlPath(vm): UrlPath<String>, Json(b): Json<VmReq>) -> Response {
    let r = (|| -> Result<()> {
        let st = d.cluster.state().context("HA needs a cluster")?;
        if d.vms.get(&vm).is_err() {
            bail!("no VM {vm} on this node (set HA up on the node it runs on)");
        }
        if b.standby == d.cluster.me || !st.nodes.iter().any(|n| n.name == b.standby) {
            bail!("pick another cluster node as the standby");
        }
        let store = b.store.clone().unwrap_or_else(|| "local".into());
        d.storage.get(&store)?;
        let me = d.cluster.me.clone();
        update(&d, |doc| {
            doc.resources.insert(vm.clone(), Resource { enabled: b.enabled, owner: me.clone(), standby: b.standby.clone(), interval_min: b.interval_min.unwrap_or(15).clamp(1, 1440), store: store.clone() });
            Ok(())
        })?;
        Ok(())
    })();
    match r {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(format!("{e:#}")),
    }
}

async fn del_vm(AxState(d): AxState<Arc<Daemon>>, UrlPath(vm): UrlPath<String>) -> Response {
    match update(&d, |doc| {
        doc.resources.remove(&vm);
        Ok(())
    }) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}
