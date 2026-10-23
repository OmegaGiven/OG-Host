//! Live statistics for the web UI.
//!
//! One sampler (every 2 s) reads the node and every container's cgroup once, serializes the
//! result once, and fans it out to all connected browsers over Server-Sent Events. History is
//! kept in memory: 1 h at 2 s for the node, 24 h at 1 min, 5 min at 2 s per container.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

const FAST_POINTS: usize = 1800; // 1 h at 2 s
const SLOW_POINTS: usize = 1440; // 24 h at 1 min
const CONTAINER_POINTS: usize = 150; // 5 min at 2 s

/// An event for every connected browser: (event name, JSON data), serialized once.
pub type Event = (&'static str, Arc<str>);

pub struct Hub {
    pub tx: tokio::sync::broadcast::Sender<Event>,
    /// Latest of each event kind, so a new client starts with a full picture.
    pub latest: RwLock<HashMap<&'static str, Arc<str>>>,
    pub history: Mutex<History>,
    sampler: Mutex<Sampler>,
}

impl Default for Hub {
    fn default() -> Self {
        Hub { tx: tokio::sync::broadcast::channel(64).0, latest: Default::default(), history: Default::default(), sampler: Default::default() }
    }
}

impl Hub {
    pub fn publish(&self, name: &'static str, data: String) {
        let data: Arc<str> = data.into();
        self.latest.write().unwrap().insert(name, data.clone());
        let _ = self.tx.send((name, data));
    }

    /// Publish only when the content changed since the last time.
    pub fn publish_if_changed(&self, name: &'static str, data: String) {
        if self.latest.read().unwrap().get(name).is_some_and(|d| **d == *data) {
            return;
        }
        self.publish(name, data);
    }

    pub fn sample(&self, containers: &[(String, String, Option<String>)]) -> Value {
        let s = self.sampler.lock().unwrap().sample(containers);
        self.history.lock().unwrap().push(&s);
        serde_json::to_value(&s).unwrap_or(Value::Null)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Point {
    pub t: u64,
    pub cpu: f32,
    pub mem: u64,
    pub rx: u64,
    pub tx: u64,
    pub dr: u64,
    pub dw: u64,
}

#[derive(Default)]
pub struct History {
    fast: VecDeque<Point>,
    slow: VecDeque<Point>,
    acc: Vec<Point>,
    containers: HashMap<String, VecDeque<(u64, f32, u64)>>,
}

impl History {
    fn push(&mut self, s: &Snapshot) {
        let p = Point { t: s.t, cpu: s.node.cpu, mem: s.node.mem.used, rx: s.node.net.rx, tx: s.node.net.tx, dr: s.node.disk.r, dw: s.node.disk.w };
        self.fast.push_back(p);
        while self.fast.len() > FAST_POINTS {
            self.fast.pop_front();
        }
        self.acc.push(p);
        if self.acc.first().is_some_and(|a| p.t / 60 != a.t / 60) {
            let n = self.acc.len() as u64;
            let avg = Point {
                t: self.acc[0].t / 60 * 60,
                cpu: self.acc.iter().map(|x| x.cpu).sum::<f32>() / n as f32,
                mem: self.acc.iter().map(|x| x.mem).sum::<u64>() / n,
                rx: self.acc.iter().map(|x| x.rx).sum::<u64>() / n,
                tx: self.acc.iter().map(|x| x.tx).sum::<u64>() / n,
                dr: self.acc.iter().map(|x| x.dr).sum::<u64>() / n,
                dw: self.acc.iter().map(|x| x.dw).sum::<u64>() / n,
            };
            self.acc.clear();
            self.slow.push_back(avg);
            while self.slow.len() > SLOW_POINTS {
                self.slow.pop_front();
            }
        }
        for (id, c) in &s.containers {
            let h = self.containers.entry(id.clone()).or_default();
            h.push_back((s.t, c.cpu, c.mem));
            while h.len() > CONTAINER_POINTS {
                h.pop_front();
            }
        }
        self.containers.retain(|id, _| s.containers.contains_key(id));
    }

    /// Node series for a range in seconds: 2 s points up to an hour, minute points beyond.
    pub fn node(&self, range_s: u64) -> Vec<Point> {
        let now = self.fast.back().map(|p| p.t).unwrap_or(0);
        let src = if range_s <= 3600 { &self.fast } else { &self.slow };
        src.iter().filter(|p| p.t + range_s >= now).copied().collect()
    }

    pub fn containers(&self) -> Value {
        json!(self.containers.iter().map(|(id, h)| (id.clone(), json!(h.iter().map(|(t, c, m)| json!([t, c, m])).collect::<Vec<_>>()))).collect::<serde_json::Map<String, Value>>())
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Mem {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub cached: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Rate {
    pub rx: u64,
    pub tx: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Disk {
    pub r: u64,
    pub w: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Fs {
    pub mount: String,
    pub total: u64,
    pub used: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NodeStats {
    pub cpu: f32,
    pub load: [f32; 3],
    pub mem: Mem,
    pub net: Rate,
    pub disk: Disk,
    pub fs: Vec<Fs>,
    pub uptime: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CStats {
    /// Percent of one core.
    pub cpu: f32,
    pub mem: u64,
    pub mem_max: Option<u64>,
    pub rx: u64,
    pub tx: u64,
    pub io_r: u64,
    pub io_w: u64,
    pub pids: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub t: u64,
    pub node: NodeStats,
    pub containers: HashMap<String, CStats>,
}

#[derive(Default)]
struct Raw {
    cpu_busy: u64,
    cpu_total: u64,
    net: (u64, u64),
    disk: (u64, u64),
    containers: HashMap<String, (u64, u64, u64, u64, u64)>, // cpu_usec, rx, tx, io_r, io_w
}

#[derive(Default)]
struct Sampler {
    prev: Option<(Instant, Raw)>,
}

fn read(p: impl AsRef<Path>) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn read_u64(p: impl AsRef<Path>) -> Option<u64> {
    read(p).trim().parse().ok()
}

fn cpu_times() -> (u64, u64) {
    let t = read("/proc/stat");
    let Some(line) = t.lines().next() else { return (0, 0) };
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    let total: u64 = v.iter().take(8).sum();
    let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
    (total - idle, total)
}

/// Physical interfaces only (they have a device); bridges, veths and loopback don't.
fn net_bytes() -> (u64, u64) {
    let mut rx = 0;
    let mut tx = 0;
    for line in read("/proc/net/dev").lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if !Path::new("/sys/class/net").join(name).join("device").exists() {
            continue;
        }
        let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        rx += v.first().copied().unwrap_or(0);
        tx += v.get(8).copied().unwrap_or(0);
    }
    (rx, tx)
}

/// Whole disks only (partitions would count twice).
fn disk_bytes() -> (u64, u64) {
    let mut r = 0;
    let mut w = 0;
    for line in read("/proc/diskstats").lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || !Path::new("/sys/block").join(f[2]).join("device").exists() {
            continue;
        }
        r += f[5].parse::<u64>().unwrap_or(0) * 512;
        w += f[9].parse::<u64>().unwrap_or(0) * 512;
    }
    (r, w)
}

fn meminfo() -> Mem {
    let mut m = HashMap::new();
    for line in read("/proc/meminfo").lines() {
        if let Some((k, v)) = line.split_once(':') {
            m.insert(k.to_string(), v.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024);
        }
    }
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    Mem {
        total: g("MemTotal"),
        used: g("MemTotal").saturating_sub(g("MemAvailable")),
        available: g("MemAvailable"),
        cached: g("Cached") + g("Buffers"),
        swap_total: g("SwapTotal"),
        swap_used: g("SwapTotal").saturating_sub(g("SwapFree")),
    }
}

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let total = s.f_blocks as u64 * s.f_frsize as u64;
    let free = s.f_bavail as u64 * s.f_frsize as u64;
    Some((total, total.saturating_sub(free)))
}

fn filesystems() -> Vec<Fs> {
    // real filesystems, one entry per device
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in read("/proc/mounts").lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || !f[0].starts_with("/dev/") || !matches!(f[2], "ext4" | "xfs" | "btrfs" | "zfs" | "f2fs" | "vfat") {
            continue;
        }
        if !seen.insert(f[0].to_string()) || f[1].starts_with("/var/lib/og-host/") {
            continue;
        }
        if let Some((total, used)) = statvfs(f[1]) {
            out.push(Fs { mount: f[1].replace("\\040", " "), total, used });
        }
    }
    out
}

fn cgroup(name: &str) -> PathBuf {
    PathBuf::from(crate::brain::CGROUP_ROOT).join(name)
}

fn io_bytes(dir: &Path) -> (u64, u64) {
    let mut r = 0;
    let mut w = 0;
    for line in read(dir.join("io.stat")).lines() {
        for kv in line.split_whitespace().skip(1) {
            if let Some(v) = kv.strip_prefix("rbytes=") {
                r += v.parse::<u64>().unwrap_or(0);
            } else if let Some(v) = kv.strip_prefix("wbytes=") {
                w += v.parse::<u64>().unwrap_or(0);
            }
        }
    }
    (r, w)
}

impl Sampler {
    /// `containers`: (id, name, host veth) of running containers.
    fn sample(&mut self, containers: &[(String, String, Option<String>)]) -> Snapshot {
        let now = Instant::now();
        let (busy, total) = cpu_times();
        let mut raw = Raw { cpu_busy: busy, cpu_total: total, net: net_bytes(), disk: disk_bytes(), containers: HashMap::new() };
        for (id, name, veth) in containers {
            let dir = cgroup(name);
            let usec = read(dir.join("cpu.stat")).lines().find_map(|l| l.strip_prefix("usage_usec ").and_then(|v| v.trim().parse().ok())).unwrap_or(0);
            // the host side of the veth: what it receives the container sent
            let (rx, tx) = veth
                .as_ref()
                .map(|v| {
                    let b = PathBuf::from("/sys/class/net").join(v).join("statistics");
                    (read_u64(b.join("tx_bytes")).unwrap_or(0), read_u64(b.join("rx_bytes")).unwrap_or(0))
                })
                .unwrap_or((0, 0));
            let (ir, iw) = io_bytes(&dir);
            raw.containers.insert(id.clone(), (usec, rx, tx, ir, iw));
        }
        let mut snap = Snapshot { t: crate::containers::unix_now(), ..Default::default() };
        let loads: Vec<f32> = read("/proc/loadavg").split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
        snap.node.load = [loads.first().copied().unwrap_or(0.0), loads.get(1).copied().unwrap_or(0.0), loads.get(2).copied().unwrap_or(0.0)];
        snap.node.mem = meminfo();
        snap.node.fs = filesystems();
        snap.node.uptime = read("/proc/uptime").split('.').next().and_then(|x| x.parse().ok()).unwrap_or(0);
        if let Some((at, p)) = &self.prev {
            let secs = now.duration_since(*at).as_secs_f64().max(0.001);
            let rate = |a: u64, b: u64| (a.saturating_sub(b) as f64 / secs) as u64;
            let dt = raw.cpu_total.saturating_sub(p.cpu_total);
            snap.node.cpu = if dt > 0 { raw.cpu_busy.saturating_sub(p.cpu_busy) as f32 * 100.0 / dt as f32 } else { 0.0 };
            snap.node.net = Rate { rx: rate(raw.net.0, p.net.0), tx: rate(raw.net.1, p.net.1) };
            snap.node.disk = Disk { r: rate(raw.disk.0, p.disk.0), w: rate(raw.disk.1, p.disk.1) };
            for (id, name, _) in containers {
                let dir = cgroup(name);
                let c = raw.containers[id];
                let prev = p.containers.get(id).copied().unwrap_or(c);
                snap.containers.insert(
                    id.clone(),
                    CStats {
                        cpu: (c.0.saturating_sub(prev.0) as f64 / 1e4 / secs) as f32,
                        mem: read_u64(dir.join("memory.current")).unwrap_or(0),
                        mem_max: read_u64(dir.join("memory.max")),
                        rx: rate(c.1, prev.1),
                        tx: rate(c.2, prev.2),
                        io_r: rate(c.3, prev.3),
                        io_w: rate(c.4, prev.4),
                        pids: read_u64(dir.join("pids.current")).unwrap_or(0),
                    },
                );
            }
        }
        self.prev = Some((now, raw));
        snap
    }
}

/// Static facts about the node, sent once per connection.
pub fn node_info(root: &Path) -> Value {
    let os = read("/etc/os-release").lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())).unwrap_or_default();
    let cpu = read("/proc/cpuinfo").lines().find_map(|l| l.strip_prefix("model name").and_then(|v| v.split_once(':')).map(|(_, v)| v.trim().to_string())).unwrap_or_default();
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut uts) };
    let s = |b: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned();
    json!({
        "hostname": s(&uts.nodename), "kernel": s(&uts.release), "arch": s(&uts.machine), "os": os,
        "cpu_model": cpu, "cores": cores, "mem_total": meminfo().total, "version": env!("CARGO_PKG_VERSION"),
        "root": root, "virtual": crate::desktop::host_is_virtual(),
    })
}

/// This node's hostname (the `host` of its metric series).
pub fn hostname() -> String {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut uts) };
    unsafe { std::ffi::CStr::from_ptr(uts.nodename.as_ptr()) }.to_string_lossy().into_owned()
}
