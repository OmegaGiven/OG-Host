//! Hardware health: disks (SMART), the kernel's hardware error reports (machine checks, memory,
//! GPU Xid, I/O errors) and the bad RAM record, checked every 30 minutes. `GET /v1/hardware`
//! has the latest picture; anything getting worse is logged as `og-hostd: HARDWARE: ...` (an
//! error line, so log alerts pick it up) and disk temperature / wear go into the metric history.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::serve::Daemon;

const EVERY: Duration = Duration::from_secs(1800);
/// Kernel messages that mean hardware trouble.
const KERNEL_PATTERNS: &[&str] = &["Hardware Error", "Machine check events logged", "EDAC.*(CE|UE|[Ee]rror)", "NVRM: Xid", "I/O error", "Uncorrected", "critical medium error", "nvme.*timeout", "ata[0-9.]+: failed", "bad mem addr"];

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
pub struct Disk {
    pub dev: String,
    pub model: String,
    pub passed: Option<bool>,
    pub temp_c: Option<f64>,
    /// percent of rated endurance used (NVMe), if reported
    pub wear_pct: Option<f64>,
    pub media_errors: Option<u64>,
    pub reallocated: Option<u64>,
    pub pending: Option<u64>,
    pub uncorrectable: Option<u64>,
    pub power_on_hours: Option<u64>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Report {
    pub checked: u64,
    pub disks: Vec<Disk>,
    /// kernel hardware messages since the previous check (most recent last, at most 20)
    pub kernel: Vec<String>,
    pub kernel_total: u64,
    pub bad_ram_pages: usize,
    pub problems: Vec<String>,
}

fn file(root: &Path) -> PathBuf {
    root.join("hardware.json")
}

pub fn load(root: &Path) -> Option<Report> {
    serde_json::from_slice(&std::fs::read(file(root)).ok()?).ok()
}

fn smart(dev: &str, kind: &str) -> Option<Disk> {
    let o = std::process::Command::new("smartctl").args(["-j", "-a", "-d", kind, dev]).output().ok()?;
    let v: Value = serde_json::from_slice(&o.stdout).ok()?;
    let nv = &v["nvme_smart_health_information_log"];
    let ata = |id: u64| -> Option<u64> {
        v["ata_smart_attributes"]["table"].as_array()?.iter().find(|a| a["id"].as_u64() == Some(id))?["raw"]["value"].as_u64()
    };
    Some(Disk {
        dev: dev.to_string(),
        model: v["model_name"].as_str().unwrap_or("").to_string(),
        passed: v["smart_status"]["passed"].as_bool(),
        temp_c: v["temperature"]["current"].as_f64(),
        wear_pct: nv["percentage_used"].as_f64(),
        media_errors: nv["media_errors"].as_u64(),
        reallocated: ata(5),
        pending: ata(197),
        uncorrectable: ata(198),
        power_on_hours: v["power_on_time"]["hours"].as_u64().or(nv["power_on_hours"].as_u64()),
    })
}

fn disks() -> Vec<Disk> {
    let Ok(o) = std::process::Command::new("smartctl").arg("--scan").output() else { return vec![] };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            // "/dev/nvme0 -d nvme # /dev/nvme0, NVMe device"
            let mut f = l.split_whitespace();
            let dev = f.next()?;
            let kind = if f.next() == Some("-d") { f.next().unwrap_or("auto") } else { "auto" };
            smart(dev, kind)
        })
        .collect()
}

fn kernel_messages(since: u64) -> Vec<String> {
    let mut cmd = std::process::Command::new("journalctl");
    cmd.args(["-k", "--no-pager", "-o", "short-iso"]);
    if since > 0 {
        cmd.arg(format!("--since=@{since}"));
    } else {
        cmd.arg("-b");
    }
    cmd.arg(format!("--grep={}", KERNEL_PATTERNS.join("|")));
    let Ok(o) = cmd.output() else { return vec![] };
    String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.starts_with("-- ")).map(|l| l.chars().take(300).collect()).collect()
}

fn check(d: &Daemon) -> Report {
    let prev = load(&d.root);
    let now = crate::containers::unix_now();
    let disks = disks();
    let mut problems = Vec::new();
    for disk in &disks {
        let before = prev.as_ref().and_then(|p| p.disks.iter().find(|x| x.dev == disk.dev));
        let worse = |a: Option<u64>, b: Option<u64>| a.unwrap_or(0) > b.unwrap_or(0);
        if disk.passed == Some(false) {
            problems.push(format!("{} ({}) reports SMART FAILED", disk.dev, disk.model));
        }
        for (what, now_v, was) in [
            ("media errors", disk.media_errors, before.and_then(|b| b.media_errors)),
            ("reallocated sectors", disk.reallocated, before.and_then(|b| b.reallocated)),
            ("pending sectors", disk.pending, before.and_then(|b| b.pending)),
            ("uncorrectable sectors", disk.uncorrectable, before.and_then(|b| b.uncorrectable)),
        ] {
            if before.is_some() && worse(now_v, was) || before.is_none() && now_v.unwrap_or(0) > 0 {
                problems.push(format!("{}: {what} {} (was {})", disk.dev, now_v.unwrap_or(0), was.unwrap_or(0)));
            }
        }
        if disk.temp_c.is_some_and(|t| t >= 70.0) {
            problems.push(format!("{}: {} °C", disk.dev, disk.temp_c.unwrap_or(0.0)));
        }
        if disk.wear_pct.is_some_and(|w| w >= 90.0) {
            problems.push(format!("{}: {}% of rated endurance used", disk.dev, disk.wear_pct.unwrap_or(0.0)));
        }
        let host = crate::stats::hostname();
        let ts = (now * 1000) as i64;
        d.metrics.record(&host, "disk_temp", &disk.dev, ts, disk.temp_c);
        d.metrics.record(&host, "disk_wear", &disk.dev, ts, disk.wear_pct);
    }
    let since = prev.as_ref().map(|p| p.checked).unwrap_or(0);
    let mut kernel = kernel_messages(since);
    let kernel_total = kernel.len() as u64;
    // bad memory found at boot is handled (reserved) by memcheck: not a new problem here
    let new_hw: Vec<&String> = kernel.iter().filter(|l| !l.contains("bad mem addr")).collect();
    if !new_hw.is_empty() {
        problems.push(format!("{} kernel hardware message(s), latest: {}", new_hw.len(), new_hw.last().map(|s| s.as_str()).unwrap_or("")));
    }
    if kernel.len() > 20 {
        kernel.drain(..kernel.len() - 20);
    }
    let bad_ram_pages = crate::memcheck::load(&d.root).pages.len();
    if let Some(p) = &prev {
        if bad_ram_pages > p.bad_ram_pages {
            problems.push(format!("bad RAM: {} known bad locations (was {})", bad_ram_pages, p.bad_ram_pages));
        }
    }
    for p in &problems {
        eprintln!("og-hostd: HARDWARE: error: {p}");
    }
    let rep = Report { checked: now, disks, kernel, kernel_total, bad_ram_pages, problems };
    if let Ok(b) = serde_json::to_vec_pretty(&rep) {
        let _ = std::fs::write(file(&d.root), b);
    }
    rep
}

pub async fn run(d: Arc<Daemon>) {
    tokio::time::sleep(Duration::from_secs(120)).await;
    loop {
        let d2 = d.clone();
        let _ = tokio::task::spawn_blocking(move || check(&d2)).await;
        tokio::time::sleep(EVERY).await;
    }
}

pub fn api_json(root: &Path) -> Value {
    load(root).map(|r| serde_json::to_value(r).unwrap_or_default()).unwrap_or_else(|| serde_json::json!({"pending": true}))
}

// ---- GPU power limits ----------------------------------------------------------------------
// The driver forgets them at every boot; kept in <root>/gpu-power.json (UUID -> watts) and
// applied when og-hostd starts. Measured on og-host2: RTX 2060 170 -> 125 W costs 4 % of LLM
// speed, RTX 3070 220 -> 160 W 1.5 % of SDXL speed, for ~27 % less power under load.

fn power_file(root: &Path) -> PathBuf {
    root.join("gpu-power.json")
}

pub fn power_limits(root: &Path) -> std::collections::BTreeMap<String, u32> {
    std::fs::read(power_file(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn set_limit(uuid: &str, watts: u32) -> anyhow::Result<()> {
    let st = std::process::Command::new("nvidia-smi").args(["-i", uuid, "-pl", &watts.to_string()]).stdout(std::process::Stdio::null()).status()?;
    anyhow::ensure!(st.success(), "nvidia-smi -pl {watts} failed for {uuid} (outside the card's allowed range?)");
    Ok(())
}

pub fn apply_power_limits(root: &Path) {
    for (uuid, w) in power_limits(root) {
        match set_limit(&uuid, w) {
            Ok(()) => eprintln!("og-hostd: GPU {uuid}: power limit {w} W"),
            Err(e) => eprintln!("og-hostd: {e:#}"),
        }
    }
}

/// `og-hostd gpu-power [UUID WATTS]`: list, or set and remember (0 = back to the default).
pub fn power_cli(root: &Path, uuid: Option<String>, watts: Option<u32>) -> anyhow::Result<()> {
    let mut m = power_limits(root);
    if let (Some(u), Some(w)) = (uuid, watts) {
        if w == 0 {
            let def = std::process::Command::new("nvidia-smi").args(["-i", &u, "--query-gpu=power.default_limit", "--format=csv,noheader,nounits"]).output()?;
            let def: f64 = String::from_utf8_lossy(&def.stdout).trim().parse().unwrap_or(0.0);
            set_limit(&u, def as u32)?;
            m.remove(&u);
        } else {
            set_limit(&u, w)?;
            m.insert(u, w);
        }
        std::fs::write(power_file(root), serde_json::to_vec_pretty(&m)?)?;
    }
    let o = std::process::Command::new("nvidia-smi").args(["--query-gpu=uuid,name,power.limit,power.default_limit,power.min_limit,power.max_limit", "--format=csv,noheader"]).output()?;
    print!("{}", String::from_utf8_lossy(&o.stdout));
    for (u, w) in &m {
        println!("kept across boots: {u} {w} W");
    }
    Ok(())
}
