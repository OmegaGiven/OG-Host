//! More usable memory from the same RAM.
//!
//! - **Compressed swap (zram):** a swap device in RAM, zstd-compressed (~3:1 for typical
//!   application memory). Pages nobody touches are packed small instead of taking a full 4 KiB;
//!   touching one again costs microseconds, not a disk read. Set up at start when the node has
//!   no swap: a quarter of RAM, at most 16 GiB. Off with OG_ZRAM=0.
//! - **Idle containers give memory back:** every 5 minutes, a container that has used almost no
//!   CPU for that long is asked to reclaim a share of its memory (cgroup `memory.reclaim`): cold
//!   cache is dropped and cold anonymous memory goes to zram. Active containers aren't touched.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::serve::Daemon;

const ZRAM_MAX: u64 = 16 << 30;
/// Idle = under this much CPU (percent of one core) on average over the last period.
const IDLE_CPU: f64 = 0.5;
/// Containers smaller than this aren't worth it.
const MIN_MEM: u64 = 128 << 20;
/// Share of a container's memory asked back per period.
const SHARE: u64 = 10;
const PERIOD: Duration = Duration::from_secs(300);

fn run(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd).args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

fn mem_total() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:").and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok()))
        .unwrap_or(0)
        * 1024
}

/// zram swap, unless there's swap already (or OG_ZRAM=0).
pub fn setup_zram() {
    if std::env::var("OG_ZRAM").as_deref() == Ok("0") {
        return;
    }
    let swaps = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    if swaps.lines().count() > 1 {
        return; // some swap exists (ours from before a daemon restart, or the admin's)
    }
    if !run("modprobe", &["zram"]) || !PathBuf::from("/sys/block/zram0").exists() {
        eprintln!("og-hostd: zram isn't available; no compressed swap");
        return;
    }
    let size = (mem_total() / 4).min(ZRAM_MAX);
    let dev = PathBuf::from("/sys/block/zram0");
    let ok = std::fs::read_to_string(dev.join("disksize")).is_ok_and(|s| s.trim() != "0") || {
        let _ = std::fs::write(dev.join("comp_algorithm"), "zstd");
        std::fs::write(dev.join("disksize"), size.to_string()).is_ok()
    };
    if ok && run("mkswap", &["/dev/zram0"]) && run("swapon", &["-p", "100", "/dev/zram0"]) {
        // zram swap is cheap: prefer it to dropping file cache, and read single pages
        let _ = std::fs::write("/proc/sys/vm/swappiness", "100");
        let _ = std::fs::write("/proc/sys/vm/page-cluster", "0");
        eprintln!("og-hostd: compressed swap: {} GiB of zram (zstd)", size >> 30);
    } else {
        eprintln!("og-hostd: setting up zram swap failed");
    }
}

/// Every 5 minutes: idle containers give back a share of their memory.
pub async fn reclaimer(d: Arc<Daemon>) {
    let mut prev: std::collections::HashMap<String, u64> = Default::default();
    loop {
        tokio::time::sleep(PERIOD).await;
        let d2 = d.clone();
        let p = std::mem::take(&mut prev);
        prev = tokio::task::spawn_blocking(move || reclaim_idle(&d2, p)).await.unwrap_or_default();
    }
}

fn reclaim_idle(d: &Daemon, prev: std::collections::HashMap<String, u64>) -> std::collections::HashMap<String, u64> {
    let mut now_cpu = std::collections::HashMap::new();
    let Ok(list) = d.node.list() else { return now_cpu };
    let mut freed: u64 = 0;
    for (m, st) in list.iter().filter(|(_, st)| st.state == "running") {
        let _ = st;
        let cg = PathBuf::from(crate::brain::CGROUP_ROOT).join(&m.name);
        let usec: u64 = std::fs::read_to_string(cg.join("cpu.stat"))
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("usage_usec ").and_then(|v| v.trim().parse().ok()))
            .unwrap_or(0);
        now_cpu.insert(m.id.clone(), usec);
        let Some(before) = prev.get(&m.id) else { continue };
        let cpu_pct = usec.saturating_sub(*before) as f64 / 1e4 / PERIOD.as_secs_f64();
        if cpu_pct > IDLE_CPU {
            continue;
        }
        let cur: u64 = std::fs::read_to_string(cg.join("memory.current")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        if cur < MIN_MEM {
            continue;
        }
        let ask = cur / SHARE;
        // EAGAIN when it couldn't reclaim all of it: fine, whatever was cold went
        let _ = std::fs::write(cg.join("memory.reclaim"), ask.to_string());
        let after: u64 = std::fs::read_to_string(cg.join("memory.current")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(cur);
        freed += cur.saturating_sub(after);
    }
    if freed > 64 << 20 {
        eprintln!("og-hostd: idle containers gave back {} MiB", freed >> 20);
    }
    now_cpu
}
