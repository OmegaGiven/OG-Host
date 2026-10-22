//! The brain: resource policy (DESIGN.md §4.7, §4.5).
//!
//! Every container has a priority class, from the `og.priority` label:
//!
//! - **critical** — protected: `memory.low` covers its working set, CPU weight 400, OOM score
//!   -900. Never squeezed or stopped by the brain.
//! - **normal** (default) — left as configured.
//! - **batch** — CPU weight 25, OOM score +500, killed as a whole on OOM. Under memory
//!   pressure its `memory.high` is lowered step by step (the kernel throttles and reclaims it,
//!   nothing is killed), and given back gradually once the node is calm again.
//! - **idle-stop** — put to sleep (stopped, desired state `sleeping`) after `og.idle-after`
//!   (default 30m) without CPU or network activity, and first when memory runs out.
//!   `start` wakes it.
//!
//! Memory pressure is what the *protected* workloads feel: PSI of the host's own services and of
//! every non-batch container, plus MemAvailable. A squeezed batch job stalls itself, and that
//! must not count as pressure. Without swap a squeeze can't free memory, only stop growth; if
//! the others still suffer when every batch job is at the floor and nothing is left to put to
//! sleep, the largest batch job is evicted (stopped, desired state `evicted`).
//!
//! GPUs: containers with GPUs have a GPU class from `og.gpu-class` (gaming, interactive
//! (default), batch). While a gaming container runs, batch GPU containers are preempted —
//! stopped (frees VRAM, default) or frozen (`og.gpu-preempt=pause`) — and resumed afterwards.
//! `og.vram` (e.g. `4G`) places a container on the GPU with the most free VRAM, and refuses a
//! batch container when no GPU has room. NVENC sessions are watched against the GeForce limit.
//!
//! [`Brain::decide`] is pure (snapshots in, actions out) so the policy is testable; [`apply`]
//! carries actions out on the node.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::containers::Node;

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup/og-host";
/// Concurrent NVENC sessions allowed per system on GeForce cards.
pub const NVENC_LIMIT: u32 = 8;
const MIB: u64 = 1 << 20;
/// memory.high is never squeezed below this.
const SQUEEZE_FLOOR: u64 = 64 * MIB;
/// Critical containers' memory.low may add up to at most this share of RAM.
const LOW_SHARE_MAX: f64 = 0.6;
const CALM_FOR: Duration = Duration::from_secs(30);
const ALERT_EVERY: Duration = Duration::from_secs(300);
const EVICT_AFTER: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    Critical,
    Normal,
    Batch,
    IdleStop,
}

impl Class {
    pub fn from_labels(l: &BTreeMap<String, String>) -> Class {
        match l.get("og.priority").map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("critical") => Class::Critical,
            Some("batch") => Class::Batch,
            Some("idle-stop" | "idle_stop" | "idlestop") => Class::IdleStop,
            _ => Class::Normal,
        }
    }
    /// Is the class set explicitly? Unlabeled containers keep their own CPU/OOM settings.
    pub fn explicit(l: &BTreeMap<String, String>) -> bool {
        l.contains_key("og.priority")
    }
    pub fn cpu_weight(self) -> u64 {
        match self {
            Class::Critical => 400,
            Class::Normal => 100,
            Class::Batch => 25,
            Class::IdleStop => 50,
        }
    }
    pub fn oom_score_adj(self) -> i32 {
        match self {
            Class::Critical => -900,
            Class::Normal => 0,
            Class::Batch => 500,
            Class::IdleStop => 300,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuClass {
    Gaming,
    Interactive,
    Batch,
}

impl GpuClass {
    pub fn from_labels(l: &BTreeMap<String, String>) -> GpuClass {
        match l.get("og.gpu-class").map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("gaming") => GpuClass::Gaming,
            Some("batch") => GpuClass::Batch,
            _ => GpuClass::Interactive,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preempt {
    Stop,
    Pause,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Psi {
    pub some10: f64,
    pub full10: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NodeSnap {
    pub mem_total: u64,
    pub mem_available: u64,
    /// Whole node (shown; not used for decisions).
    pub psi: Psi,
    /// Host services outside og-host (system.slice, user.slice, init.scope): worst of them.
    pub host_psi: Psi,
}

#[derive(Clone, Debug, Serialize)]
pub struct CSnap {
    pub id: String,
    pub name: String,
    pub class: Class,
    /// Some when the container has GPUs.
    pub gpu_class: Option<GpuClass>,
    pub preempt: Preempt,
    pub running: bool,
    pub desired: String,
    pub mem_current: u64,
    pub mem_max: Option<u64>,
    pub mem_low_floor: u64,
    pub psi: Psi,
    pub cpu_usec: u64,
    /// Bytes through the container's host-side veth (both directions), if it has one.
    pub net_bytes: Option<u64>,
    pub idle_after: Duration,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Gpu {
    pub index: u32,
    pub uuid: String,
    pub name: String,
    pub mem_total_mib: u64,
    pub mem_used_mib: u64,
    pub encoder_sessions: u32,
    pub util_pct: Option<f64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
}

impl Gpu {
    pub fn free_mib(&self) -> u64 {
        self.mem_total_mib.saturating_sub(self.mem_used_mib)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Action {
    SetLow { id: String, name: String, bytes: u64 },
    /// Sum of the children's memory.low, on the og-host parent cgroup (protection is only
    /// effective when every ancestor has it too).
    SetParentLow { bytes: u64 },
    SetHigh { id: String, name: String, bytes: Option<u64> },
    Sleep { id: String, name: String, reason: String },
    Evict { id: String, name: String },
    Preempt { id: String, name: String, how: Preempt },
    Resume { id: String, name: String, how: Preempt },
    Alert { message: String },
}

#[derive(Debug, Default)]
struct Activity {
    cpu_usec: u64,
    net_bytes: Option<u64>,
    at: Option<Instant>,
    last_active: Option<Instant>,
}

#[derive(Default)]
pub struct Brain {
    lows: HashMap<String, u64>,
    parent_low: u64,
    squeezed: HashMap<String, u64>,
    activity: HashMap<String, Activity>,
    calm_since: Option<Instant>,
    stuck_since: Option<Instant>,
    preempted: HashMap<String, Preempt>,
    last_alert: HashMap<String, Instant>,
    pub log: VecDeque<(u64, String)>,
    pub last_node: NodeSnap,
    pub last_containers: Vec<CSnap>,
    pub last_gpus: Vec<Gpu>,
}

impl Brain {
    pub fn decide(&mut self, node: &NodeSnap, cs: &[CSnap], gpus: &[Gpu], now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let ids: std::collections::HashSet<&str> = cs.iter().map(|c| c.id.as_str()).collect();
        self.lows.retain(|k, _| ids.contains(k.as_str()));
        self.squeezed.retain(|k, _| ids.contains(k.as_str()));
        self.activity.retain(|k, _| ids.contains(k.as_str()));
        self.preempted.retain(|k, _| ids.contains(k.as_str()));

        // ---- memory.low for critical containers
        let cap = (node.mem_total as f64 * LOW_SHARE_MAX) as u64;
        let mut want: Vec<(&CSnap, u64)> = cs
            .iter()
            .filter(|c| c.running && c.class == Class::Critical)
            .map(|c| {
                let w = (c.mem_current + c.mem_current / 4).max(c.mem_low_floor);
                (c, c.mem_max.map_or(w, |m| w.min(m)))
            })
            .collect();
        let total: u64 = want.iter().map(|(_, w)| *w).sum();
        if total > cap && total > 0 {
            let f = cap as f64 / total as f64;
            for (_, w) in want.iter_mut() {
                *w = (*w as f64 * f) as u64;
            }
        }
        let mut sum = 0;
        for (c, w) in &want {
            let prev = self.lows.get(&c.id).copied().unwrap_or(0);
            // only rewrite on a >5% change, so the cgroup isn't churned every tick
            if prev == 0 || w.abs_diff(prev) * 20 > prev {
                self.lows.insert(c.id.clone(), *w);
                out.push(Action::SetLow { id: c.id.clone(), name: c.name.clone(), bytes: *w });
            }
            sum += self.lows[&c.id];
        }
        for c in cs.iter().filter(|c| c.class != Class::Critical || !c.running) {
            if self.lows.remove(&c.id).is_some() {
                out.push(Action::SetLow { id: c.id.clone(), name: c.name.clone(), bytes: 0 });
            }
        }
        if sum != self.parent_low {
            self.parent_low = sum;
            out.push(Action::SetParentLow { bytes: sum });
        }

        // ---- memory pressure
        let avail_pct = if node.mem_total > 0 { node.mem_available as f64 * 100.0 / node.mem_total as f64 } else { 100.0 };
        let felt = cs.iter().filter(|c| c.running && c.class != Class::Batch).fold(node.host_psi, |a, c| Psi { some10: a.some10.max(c.psi.some10), full10: a.full10.max(c.psi.full10) });
        let pressure = felt.some10 >= 10.0 || avail_pct < 5.0;
        let severe = felt.some10 >= 25.0 || felt.full10 >= 10.0 || avail_pct < 2.0;
        let calm = felt.some10 < 2.0 && avail_pct > 15.0;
        if !pressure {
            self.stuck_since = None;
        }
        if pressure {
            self.calm_since = None;
            let mut squeezable = false;
            for c in cs.iter().filter(|c| c.running && c.class == Class::Batch) {
                let cur = self.squeezed.get(&c.id).copied().unwrap_or(c.mem_current);
                let next = (cur / 5 * 4).max(SQUEEZE_FLOOR);
                // writing memory.high reclaims at once; usage still >10% above the limit set last
                // tick means the kernel can't reclaim it (anonymous memory, no swap)
                let ineffective = self.squeezed.contains_key(&c.id) && c.mem_current > cur + cur / 10;
                if next < cur {
                    squeezable |= !ineffective;
                    self.squeezed.insert(c.id.clone(), next);
                    out.push(Action::SetHigh { id: c.id.clone(), name: c.name.clone(), bytes: Some(next) });
                }
            }
            if severe {
                if let Some(v) = cs.iter().filter(|c| c.running && c.class == Class::IdleStop).max_by_key(|c| c.mem_current) {
                    out.push(Action::Sleep { id: v.id.clone(), name: v.name.clone(), reason: "memory pressure".into() });
                }
            }
            // squeezing has stopped helping (at the floor, or unreclaimable without swap):
            // evict the biggest batch job before the kernel's OOM killer has to step in
            if squeezable {
                self.stuck_since = None;
            } else {
                let since = *self.stuck_since.get_or_insert(now);
                {
                    let biggest = cs.iter().filter(|c| c.running && c.class == Class::Batch).max_by_key(|c| c.mem_current);
                    match biggest {
                        // give the squeeze a moment to show, then evict
                        Some(b) if now.duration_since(since) >= EVICT_AFTER => {
                            self.squeezed.remove(&b.id);
                            self.stuck_since = None;
                            out.push(Action::Evict { id: b.id.clone(), name: b.name.clone() });
                        }
                        Some(_) => {}
                        None if severe => self.alert(&mut out, now, "memory", format!(
                            "memory pressure (PSI some {:.0}%, {:.0}% available) and nothing left to squeeze or stop; critical containers stay protected",
                            felt.some10, avail_pct
                        )),
                        None => {}
                    }
                }
            }
        } else if calm && !self.squeezed.is_empty() {
            let since = *self.calm_since.get_or_insert(now);
            if now.duration_since(since) >= CALM_FOR {
                let held: Vec<&CSnap> = cs.iter().filter(|c| self.squeezed.contains_key(&c.id)).collect();
                for c in held {
                    let cur = self.squeezed[&c.id];
                    let next = cur + cur / 4;
                    if next >= c.mem_max.unwrap_or(u64::MAX) || next >= node.mem_total {
                        self.squeezed.remove(&c.id);
                        out.push(Action::SetHigh { id: c.id.clone(), name: c.name.clone(), bytes: None });
                    } else {
                        self.squeezed.insert(c.id.clone(), next);
                        out.push(Action::SetHigh { id: c.id.clone(), name: c.name.clone(), bytes: Some(next) });
                    }
                }
            }
        } else if !calm {
            self.calm_since = None;
        }
        // a squeezed container that stopped starts over unsqueezed
        for c in cs.iter().filter(|c| !c.running) {
            if self.squeezed.remove(&c.id).is_some() {
                out.push(Action::SetHigh { id: c.id.clone(), name: c.name.clone(), bytes: None });
            }
        }

        // ---- idle-stop
        for c in cs.iter().filter(|c| c.class == Class::IdleStop) {
            if !c.running {
                self.activity.remove(&c.id);
                continue;
            }
            let a = self.activity.entry(c.id.clone()).or_default();
            let active = match a.at {
                None => true, // first sight counts as activity
                Some(at) => {
                    let secs = now.duration_since(at).as_secs_f64().max(0.001);
                    let cpu = c.cpu_usec.saturating_sub(a.cpu_usec) as f64 / 1e6 / secs; // cores
                    let net = match (c.net_bytes, a.net_bytes) {
                        (Some(n), Some(p)) => n.saturating_sub(p) as f64 / secs,
                        _ => 0.0,
                    };
                    cpu > 0.02 || net > 2048.0
                }
            };
            a.cpu_usec = c.cpu_usec;
            a.net_bytes = c.net_bytes;
            a.at = Some(now);
            if active {
                a.last_active = Some(now);
            }
            if let Some(last) = a.last_active {
                if now.duration_since(last) >= c.idle_after {
                    let mins = c.idle_after.as_secs() / 60;
                    out.push(Action::Sleep { id: c.id.clone(), name: c.name.clone(), reason: format!("idle for {mins}m") });
                    self.activity.remove(&c.id);
                }
            }
        }

        // ---- GPUs: gaming first
        let gaming = cs.iter().any(|c| c.running && c.gpu_class == Some(GpuClass::Gaming));
        if gaming {
            let fresh: Vec<&CSnap> = cs.iter().filter(|c| c.running && c.gpu_class == Some(GpuClass::Batch) && !self.preempted.contains_key(&c.id)).collect();
            for c in fresh {
                self.preempted.insert(c.id.clone(), c.preempt);
                out.push(Action::Preempt { id: c.id.clone(), name: c.name.clone(), how: c.preempt });
            }
        } else {
            let mut resumed = Vec::new();
            for c in cs.iter().filter(|c| self.preempted.contains_key(&c.id)) {
                let how = self.preempted[&c.id];
                // a stopped-for-gaming container whose desired state changed was taken over by the user
                if how == Preempt::Stop && c.desired != "preempted" {
                    resumed.push(c.id.clone());
                    continue;
                }
                out.push(Action::Resume { id: c.id.clone(), name: c.name.clone(), how });
                resumed.push(c.id.clone());
            }
            for id in resumed {
                self.preempted.remove(&id);
            }
        }
        let sessions: u32 = gpus.iter().map(|g| g.encoder_sessions).sum();
        if !gpus.is_empty() && sessions >= NVENC_LIMIT {
            self.alert(&mut out, now, "nvenc", format!("{sessions} NVENC sessions: at the GeForce limit of {NVENC_LIMIT}; new encodes will fail"));
        }
        out
    }

    fn alert(&mut self, out: &mut Vec<Action>, now: Instant, key: &str, message: String) {
        if self.last_alert.get(key).is_some_and(|t| now.duration_since(*t) < ALERT_EVERY) {
            return;
        }
        self.last_alert.insert(key.into(), now);
        out.push(Action::Alert { message });
    }

    pub fn note(&mut self, msg: String) {
        eprintln!("og-hostd brain: {msg}");
        self.log.push_back((crate::containers::unix_now(), msg));
        while self.log.len() > 200 {
            self.log.pop_front();
        }
    }

    pub fn squeezed_high(&self, id: &str) -> Option<u64> {
        self.squeezed.get(id).copied()
    }
    pub fn low(&self, id: &str) -> Option<u64> {
        self.lows.get(id).copied()
    }
    pub fn is_preempted(&self, id: &str) -> bool {
        self.preempted.contains_key(id)
    }
}

/// GPU with the most free VRAM that fits `need_mib`.
pub fn pick_gpu(gpus: &[Gpu], need_mib: u64) -> Option<u32> {
    gpus.iter().filter(|g| g.free_mib() >= need_mib).max_by_key(|g| g.free_mib()).map(|g| g.index)
}

/// `30m`, `2h`, `90s`, or a bare number of minutes.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (n, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        _ => (s, 60),
    };
    n.trim().parse::<u64>().ok().map(|v| Duration::from_secs(v * mult))
}

// ---- reading the node -----------------------------------------------------------------------

fn cg(name: &str) -> PathBuf {
    PathBuf::from(CGROUP_ROOT).join(name)
}

fn read_u64(p: PathBuf) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

pub fn read_psi(path: &str) -> Psi {
    let mut psi = Psi::default();
    if let Ok(t) = std::fs::read_to_string(path) {
        for line in t.lines() {
            let avg10 = line.split_whitespace().find_map(|f| f.strip_prefix("avg10=")).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            if line.starts_with("some") {
                psi.some10 = avg10;
            } else if line.starts_with("full") {
                psi.full10 = avg10;
            }
        }
    }
    psi
}

pub fn read_node() -> NodeSnap {
    let host_psi = ["system.slice", "user.slice", "init.scope"].iter().map(|s| read_psi(&format!("/sys/fs/cgroup/{s}/memory.pressure"))).fold(Psi::default(), |a, p| Psi { some10: a.some10.max(p.some10), full10: a.full10.max(p.full10) });
    let mut n = NodeSnap { psi: read_psi("/proc/pressure/memory"), host_psi, ..Default::default() };
    if let Ok(t) = std::fs::read_to_string("/proc/meminfo") {
        for line in t.lines() {
            let kb = |l: &str| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
            if line.starts_with("MemTotal:") {
                n.mem_total = kb(line);
            } else if line.starts_with("MemAvailable:") {
                n.mem_available = kb(line);
            }
        }
    }
    n
}

pub fn snapshot(node: &Node) -> Vec<CSnap> {
    let Ok(list) = node.list() else { return vec![] };
    list.into_iter()
        .map(|(m, st)| {
            let dir = cg(&m.name);
            let cpu_usec = std::fs::read_to_string(dir.join("cpu.stat"))
                .ok()
                .and_then(|t| t.lines().find_map(|l| l.strip_prefix("usage_usec ").and_then(|v| v.trim().parse().ok())))
                .unwrap_or(0);
            let net_bytes = m.net.as_ref().and_then(|a| a.host_veth.as_ref()).and_then(|v| {
                let base = PathBuf::from("/sys/class/net").join(v).join("statistics");
                Some(read_u64(base.join("rx_bytes"))? + read_u64(base.join("tx_bytes"))?)
            });
            let size = |k: &str| m.labels.get(k).and_then(|v| crate::containers::parse_size(v).ok()).map(|v| v.max(0) as u64);
            CSnap {
                id: m.id.clone(),
                name: m.name.clone(),
                class: Class::from_labels(&m.labels),
                gpu_class: (!m.gpus.is_empty()).then(|| GpuClass::from_labels(&m.labels)),
                preempt: if m.labels.get("og.gpu-preempt").is_some_and(|v| v == "pause") { Preempt::Pause } else { Preempt::Stop },
                running: st.state == "running",
                desired: m.desired.clone(),
                mem_current: read_u64(dir.join("memory.current")).unwrap_or(0),
                mem_max: read_u64(dir.join("memory.max")),
                mem_low_floor: size("og.memory-low").unwrap_or(0),
                psi: read_psi(&dir.join("memory.pressure").to_string_lossy()),
                cpu_usec,
                net_bytes,
                idle_after: m.labels.get("og.idle-after").and_then(|v| parse_duration(v)).unwrap_or(Duration::from_secs(1800)),
            }
        })
        .collect()
}

/// NVIDIA GPUs via nvidia-smi; empty when there is no driver.
pub fn read_gpus() -> Vec<Gpu> {
    let Ok(out) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,uuid,name,memory.total,memory.used,encoder.stats.sessionCount,utilization.gpu,temperature.gpu,power.draw", "--format=csv,noheader,nounits"])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return vec![];
    };
    if !out.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&out.stdout).lines().filter_map(parse_gpu_line).collect()
}

fn parse_gpu_line(line: &str) -> Option<Gpu> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() < 9 {
        return None;
    }
    let num = |s: &str| s.parse::<f64>().ok();
    Some(Gpu {
        index: f[0].parse().ok()?,
        uuid: f[1].into(),
        name: f[2].into(),
        mem_total_mib: num(f[3])? as u64,
        mem_used_mib: num(f[4]).unwrap_or(0.0) as u64,
        encoder_sessions: num(f[5]).unwrap_or(0.0) as u32,
        util_pct: num(f[6]),
        temp_c: num(f[7]),
        power_w: num(f[8]),
    })
}

// ---- acting on the node ---------------------------------------------------------------------

fn write(p: PathBuf, v: &str) -> std::io::Result<()> {
    std::fs::write(&p, v)
}

fn write_if_changed(p: PathBuf, v: &str) {
    if std::fs::read_to_string(&p).map(|c| c.trim() != v).unwrap_or(true) {
        let _ = write(p, v);
    }
}

/// Class-fixed settings, re-applied every tick (cheap; survives restarts).
pub fn apply_class(cs: &[CSnap], labels: &HashMap<String, BTreeMap<String, String>>) {
    for c in cs.iter().filter(|c| c.running) {
        if !labels.get(&c.id).is_some_and(Class::explicit) {
            continue;
        }
        let dir = cg(&c.name);
        write_if_changed(dir.join("cpu.weight"), &c.class.cpu_weight().to_string());
        if c.class == Class::Batch {
            write_if_changed(dir.join("memory.oom.group"), "1");
        }
        let adj = c.class.oom_score_adj().to_string();
        if let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) {
            for pid in procs.lines() {
                write_if_changed(PathBuf::from(format!("/proc/{pid}/oom_score_adj")), &adj);
            }
        }
    }
}

pub fn apply(node: &Node, brain: &std::sync::Mutex<Brain>, actions: Vec<Action>) {
    for a in actions {
        let msg = match &a {
            Action::SetLow { id, name, bytes } => {
                // never below what the container was configured with
                let configured = crate::resources::configured_unified(node, id, "memory.low").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                write(cg(name).join("memory.low"), &(*bytes).max(configured).to_string()).err().map(|e| format!("{name}: memory.low: {e}"))
            }
            Action::SetParentLow { bytes } => write(PathBuf::from(CGROUP_ROOT).join("memory.low"), &bytes.to_string()).err().map(|e| format!("og-host memory.low: {e}")),
            Action::SetHigh { id, name, bytes } => {
                // giving back means back to the configured memory.high, not unlimited
                let v = bytes.map_or_else(|| crate::resources::configured_unified(node, id, "memory.high").unwrap_or_else(|| "max".into()), |b| b.to_string());
                Some(match write(cg(name).join("memory.high"), &v) {
                    Ok(()) => match bytes {
                        Some(b) => format!("{name}: memory.high -> {} MiB (memory pressure)", b / MIB),
                        None => format!("{name}: memory.high restored"),
                    },
                    Err(e) => format!("{name}: memory.high: {e}"),
                })
            }
            Action::Sleep { id, name, reason } => Some(match node.stop(id, 10).and_then(|_| node.set_desired(id, "sleeping")) {
                Ok(()) => format!("{name}: put to sleep ({reason}); start it to wake"),
                Err(e) => format!("{name}: sleep failed: {e:#}"),
            }),
            Action::Evict { id, name } => Some(match node.stop(id, 10).and_then(|_| node.set_desired(id, "evicted")) {
                Ok(()) => format!("{name}: evicted (memory pressure, and squeezing it no longer freed memory); start it to run again"),
                Err(e) => format!("{name}: evict failed: {e:#}"),
            }),
            Action::Preempt { id, name, how } => Some(match how {
                Preempt::Pause => match write(cg(name).join("cgroup.freeze"), "1") {
                    Ok(()) => format!("{name}: paused for gaming"),
                    Err(e) => format!("{name}: pause failed: {e}"),
                },
                Preempt::Stop => match node.stop(id, 10).and_then(|_| node.set_desired(id, "preempted")) {
                    Ok(()) => format!("{name}: stopped for gaming (frees VRAM); resumes afterwards"),
                    Err(e) => format!("{name}: preempt failed: {e:#}"),
                },
            }),
            Action::Resume { id, name, how } => Some(match how {
                Preempt::Pause => match write(cg(name).join("cgroup.freeze"), "0") {
                    Ok(()) => format!("{name}: resumed"),
                    Err(e) => format!("{name}: resume failed: {e}"),
                },
                Preempt::Stop => match node.start(id) {
                    Ok(()) => format!("{name}: restarted after gaming"),
                    Err(e) => format!("{name}: restart failed: {e:#}"),
                },
            }),
            Action::Alert { message } => Some(format!("ALERT: {message}")),
        };
        if let Some(m) = msg {
            brain.lock().unwrap().note(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, class: Class, mem: u64) -> CSnap {
        CSnap {
            id: id.into(),
            name: id.into(),
            class,
            gpu_class: None,
            preempt: Preempt::Stop,
            running: true,
            desired: "running".into(),
            mem_current: mem,
            mem_max: None,
            mem_low_floor: 0,
            psi: Psi::default(),
            cpu_usec: 0,
            net_bytes: Some(0),
            idle_after: Duration::from_secs(600),
        }
    }
    fn node(psi: f64, avail_pct: u64) -> NodeSnap {
        let p = Psi { some10: psi, full10: 0.0 };
        NodeSnap { mem_total: 1000 * MIB, mem_available: avail_pct * 10 * MIB, psi: p, host_psi: p }
    }

    #[test]
    fn protects_critical_with_low_capped() {
        let mut b = Brain::default();
        let t = Instant::now();
        let cs = [c("db", Class::Critical, 400 * MIB), c("web", Class::Normal, 100 * MIB), c("j", Class::Critical, 400 * MIB)];
        let a = b.decide(&node(0.0, 50), &cs, &[], t);
        // 2 x 500 MiB wanted, capped to 60% of 1000 MiB
        let lows: u64 = a.iter().filter_map(|x| if let Action::SetLow { bytes, .. } = x { Some(*bytes) } else { None }).sum();
        assert!(lows <= 600 * MIB && lows > 590 * MIB, "{lows}");
        assert!(a.iter().any(|x| matches!(x, Action::SetParentLow { .. })));
        // unchanged next tick: nothing rewritten
        assert!(b.decide(&node(0.0, 50), &cs, &[], t).is_empty());
    }

    #[test]
    fn squeezes_batch_then_sleeps_idle_then_alerts_then_restores() {
        let mut b = Brain::default();
        let t = Instant::now();
        let cs = [c("db", Class::Critical, 100 * MIB), c("job", Class::Batch, 500 * MIB), c("wiki", Class::IdleStop, 200 * MIB)];
        b.decide(&node(0.0, 50), &cs, &[], t);
        let a = b.decide(&node(15.0, 10), &cs, &[], t);
        assert!(a.contains(&Action::SetHigh { id: "job".into(), name: "job".into(), bytes: Some(400 * MIB) }));
        assert!(!a.iter().any(|x| matches!(x, Action::Sleep { .. }))); // not severe yet
        let a = b.decide(&node(30.0, 3), &cs, &[], t);
        assert!(a.contains(&Action::SetHigh { id: "job".into(), name: "job".into(), bytes: Some(320 * MIB) }));
        assert!(a.iter().any(|x| matches!(x, Action::Sleep { id, .. } if id == "wiki")));
        assert!(!a.iter().any(|x| matches!(x, Action::SetHigh { id, .. } | Action::Sleep { id, .. } if id == "db")));
        // the batch job's own stall is not pressure
        let mut b2 = Brain::default();
        let mut hog = c("hog", Class::Batch, 900 * MIB);
        hog.psi = Psi { some10: 95.0, full10: 90.0 };
        assert!(!b2.decide(&node(0.0, 40), &[hog.clone()], &[], t).iter().any(|x| matches!(x, Action::SetHigh { .. })));
        // severe for others, batch at the floor, nothing idle: evicted after EVICT_AFTER
        let mut hog = c("hog", Class::Batch, 60 * MIB);
        hog.psi = Psi { some10: 95.0, full10: 90.0 };
        let a = b2.decide(&node(40.0, 1), &[hog.clone()], &[], t);
        assert!(!a.iter().any(|x| matches!(x, Action::Evict { .. })));
        let a = b2.decide(&node(40.0, 1), &[hog.clone()], &[], t + Duration::from_secs(25));
        assert!(a.iter().any(|x| matches!(x, Action::Evict { id, .. } if id == "hog")));
        // no swap: usage stays far above the squeezed limit -> evicted without waiting for the floor
        let mut b3 = Brain::default();
        let big = c("big", Class::Batch, 1000 * MIB);
        b3.decide(&node(40.0, 1), &[big.clone()], &[], t); // 800
        b3.decide(&node(40.0, 1), &[big.clone()], &[], t + Duration::from_secs(5)); // 640: 1000 > 1.5 x 640
        let a = b3.decide(&node(40.0, 1), &[big.clone()], &[], t + Duration::from_secs(25));
        assert!(a.iter().any(|x| matches!(x, Action::Evict { .. })), "{a:?}");
        // squeezed down to the floor, nothing idle or batch left: one alert, rate limited
        let cs2 = [c("db", Class::Critical, 100 * MIB)];
        let a = b.decide(&node(40.0, 1), &cs2, &[], t);
        assert!(a.iter().any(|x| matches!(x, Action::Alert { .. })));
        assert!(!b.decide(&node(40.0, 1), &cs2, &[], t + Duration::from_secs(5)).iter().any(|x| matches!(x, Action::Alert { .. })));
        // calm: restore only after CALM_FOR, gradually
        let cs = [c("job", Class::Batch, 300 * MIB)];
        let mut b = Brain::default();
        b.decide(&node(15.0, 10), &cs, &[], t);
        assert!(b.decide(&node(0.0, 60), &cs, &[], t + Duration::from_secs(5)).is_empty());
        let a = b.decide(&node(0.0, 60), &cs, &[], t + Duration::from_secs(40));
        assert!(a.iter().any(|x| matches!(x, Action::SetHigh { bytes: Some(_), .. })));
        let mut done = false;
        for i in 0..30 {
            if b.decide(&node(0.0, 60), &cs, &[], t + Duration::from_secs(45 + i)).iter().any(|x| matches!(x, Action::SetHigh { bytes: None, .. })) {
                done = true;
                break;
            }
        }
        assert!(done, "memory.high eventually restored to max");
    }

    #[test]
    fn idle_stop_after_quiet_period() {
        let mut b = Brain::default();
        let t = Instant::now();
        let mut w = c("wiki", Class::IdleStop, 50 * MIB);
        assert!(b.decide(&node(0.0, 50), &[w.clone()], &[], t).is_empty());
        // traffic at 5 min keeps it awake
        w.net_bytes = Some(1_000_000);
        assert!(b.decide(&node(0.0, 50), &[w.clone()], &[], t + Duration::from_secs(300)).is_empty());
        assert!(b.decide(&node(0.0, 50), &[w.clone()], &[], t + Duration::from_secs(600)).is_empty());
        let a = b.decide(&node(0.0, 50), &[w.clone()], &[], t + Duration::from_secs(901));
        assert!(matches!(&a[..], [Action::Sleep { reason, .. }] if reason == "idle for 10m"));
    }

    #[test]
    fn gaming_preempts_batch_gpu_work_and_resumes() {
        let mut b = Brain::default();
        let t = Instant::now();
        let mut game = c("game", Class::Normal, MIB);
        game.gpu_class = Some(GpuClass::Gaming);
        let mut ai = c("ai", Class::Normal, MIB);
        ai.gpu_class = Some(GpuClass::Batch);
        let mut enc = c("enc", Class::Normal, MIB);
        enc.gpu_class = Some(GpuClass::Batch);
        enc.preempt = Preempt::Pause;
        let mut ui = c("ui", Class::Normal, MIB);
        ui.gpu_class = Some(GpuClass::Interactive);
        let a = b.decide(&node(0.0, 50), &[game.clone(), ai.clone(), enc.clone(), ui.clone()], &[], t);
        assert_eq!(a.len(), 2);
        assert!(a.contains(&Action::Preempt { id: "ai".into(), name: "ai".into(), how: Preempt::Stop }));
        assert!(a.contains(&Action::Preempt { id: "enc".into(), name: "enc".into(), how: Preempt::Pause }));
        assert!(b.decide(&node(0.0, 50), &[game.clone(), ai.clone(), enc.clone()], &[], t).is_empty());
        game.running = false;
        ai.running = false;
        ai.desired = "preempted".into();
        let a = b.decide(&node(0.0, 50), &[game, ai, enc], &[], t);
        assert_eq!(a.len(), 2);
        assert!(a.iter().all(|x| matches!(x, Action::Resume { .. })));
    }

    #[test]
    fn gpu_placement_and_nvenc() {
        let g = |i, total, used, enc| Gpu { index: i, mem_total_mib: total, mem_used_mib: used, encoder_sessions: enc, ..Default::default() };
        let gpus = [g(0, 8192, 7000, 3), g(1, 6144, 1000, 5)];
        assert_eq!(pick_gpu(&gpus, 4096), Some(1));
        assert_eq!(pick_gpu(&gpus, 6000), None);
        let mut b = Brain::default();
        let a = b.decide(&node(0.0, 50), &[], &gpus, Instant::now());
        assert!(matches!(&a[..], [Action::Alert { .. }]));
        let line = parse_gpu_line("0, GPU-abc, NVIDIA GeForce RTX 2060, 6144, 5611, 0, 3, 52, 18.40").unwrap();
        assert_eq!((line.index, line.mem_total_mib, line.mem_used_mib, line.encoder_sessions), (0, 6144, 5611, 0));
        assert!(parse_gpu_line("0, GPU-x, Foo, [N/A], 1, 0, 0, 0, 0").is_none());
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("15"), Some(Duration::from_secs(900)));
    }
}
