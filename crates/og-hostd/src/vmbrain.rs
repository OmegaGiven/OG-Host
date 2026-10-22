//! The brain's VM side: memory ballooning. VMs with a balloon (`balloon` > 0 = the least they
//! may be squeezed to) give memory back to the host when it's needed and get it back when the
//! host is calm, by their `og.priority` label like containers:
//!
//! - critical: never ballooned.
//! - normal (default): under pressure, down halfway to their minimum; when the host is
//!   above 70 % used and the VM is idle (under 3 % CPU for 10 minutes), down to its minimum.
//! - batch: under pressure (or idle above 70 %), down to their minimum.
//!
//! Back to full memory when the host is below 60 % used and not under pressure (or right away
//! when an idle VM gets busy and the host has room). [`decide`] is pure; [`tick`] reads and
//! applies over QMP.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::brain::Class;

const IDLE_FOR: Duration = Duration::from_secs(600);
const IDLE_CPU: f64 = 3.0;

#[derive(Debug, Clone)]
pub struct VmSnap {
    pub name: String,
    pub class: Class,
    /// MiB
    pub memory: u64,
    pub balloon_min: u64,
    /// current balloon target, MiB
    pub target: u64,
    pub idle: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct HostSnap {
    /// share of RAM in use, 0-100
    pub used_pct: f64,
    pub pressure: bool,
}

/// New balloon targets (MiB) for VMs whose target should change.
pub fn decide(host: HostSnap, vms: &[VmSnap]) -> Vec<(String, u64)> {
    let mut out = vec![];
    for v in vms {
        if v.balloon_min == 0 || v.balloon_min >= v.memory || v.class == Class::Critical {
            if v.target < v.memory && v.balloon_min > 0 {
                out.push((v.name.clone(), v.memory));
            }
            continue;
        }
        let half = v.balloon_min + (v.memory - v.balloon_min) / 2;
        let want = if host.pressure {
            match v.class {
                Class::Batch | Class::IdleStop => v.balloon_min,
                _ if v.idle => v.balloon_min,
                _ => half,
            }
        } else if host.used_pct > 70.0 && v.idle {
            v.balloon_min
        } else if host.used_pct < 60.0 || !v.idle {
            if host.used_pct < 85.0 { v.memory } else { v.target }
        } else {
            v.target
        };
        if want != v.target {
            out.push((v.name.clone(), want));
        }
    }
    out
}

#[derive(Default, Serialize)]
pub struct State {
    #[serde(skip)]
    cpu: HashMap<String, VecDeque<(Instant, f64)>>,
    pub targets: HashMap<String, u64>,
    pub last: Vec<(String, u64)>,
}

/// One round: read VMs and the host, balloon as decided.
pub fn tick(d: &crate::serve::Daemon) {
    let node = crate::brain::read_node();
    let used_pct = if node.mem_total > 0 { 100.0 * (node.mem_total.saturating_sub(node.mem_available)) as f64 / node.mem_total as f64 } else { 0.0 };
    let host = HostSnap { used_pct, pressure: node.host_psi.some10 > 10.0 || node.mem_available < node.mem_total / 10 };
    let mut snaps = vec![];
    {
        let mut st = d.vmbrain.lock().unwrap();
        let now = Instant::now();
        for vm in d.vms.list() {
            let s = d.vms.status(&vm.name);
            let Some(pid) = s.pid.filter(|_| s.state == "running") else {
                st.cpu.remove(&vm.name);
                st.targets.remove(&vm.name);
                continue;
            };
            if vm.balloon == 0 {
                continue;
            }
            let Some((secs, _)) = crate::vmrun::usage(pid) else { continue };
            let q = st.cpu.entry(vm.name.clone()).or_default();
            q.push_back((now, secs));
            while q.front().is_some_and(|(t, _)| now.duration_since(*t) > IDLE_FOR + Duration::from_secs(60)) {
                q.pop_front();
            }
            let idle = match (q.front(), q.back()) {
                (Some((t0, c0)), Some((t1, c1))) if t1.duration_since(*t0) >= IDLE_FOR => (c1 - c0) / t1.duration_since(*t0).as_secs_f64() * 100.0 < IDLE_CPU,
                _ => false,
            };
            let target = *st.targets.entry(vm.name.clone()).or_insert(vm.memory);
            snaps.push(VmSnap { name: vm.name.clone(), class: Class::from_labels(&vm.labels), memory: vm.memory, balloon_min: vm.balloon, target, idle });
        }
    }
    let changes = decide(host, &snaps);
    for (name, mib) in &changes {
        match d.vms.qmp(name, "balloon", serde_json::json!({"value": mib << 20})) {
            Ok(_) => {
                d.vmbrain.lock().unwrap().targets.insert(name.clone(), *mib);
                d.brain.lock().unwrap().note(format!("VM {name}: memory {} MiB (host {used_pct:.0}% used{})", mib, if host.pressure { ", under pressure" } else { "" }));
            }
            Err(e) => eprintln!("og-hostd: balloon {name}: {e:#}"),
        }
    }
    if !changes.is_empty() {
        d.vmbrain.lock().unwrap().last = changes;
    }
}

/// GPUs (PCI addresses) a VM would take that containers are using now: (gpu, containers).
pub fn gpu_conflicts(d: &crate::serve::Daemon, vm: &crate::vms::Vm) -> Vec<(String, Vec<String>)> {
    let gpus: Vec<&String> = vm.hostpci.iter().filter(|p| std::fs::read_to_string(format!("/sys/bus/pci/devices/{p}/class")).is_ok_and(|c| c.trim().starts_with("0x03"))).collect();
    if gpus.is_empty() {
        return vec![];
    }
    // PCI address -> nvidia index
    let idx: HashMap<String, String> = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,pci.bus_id", "--format=csv,noheader"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    let (i, b) = l.split_once(',')?;
                    // 00000000:01:00.0 -> 0000:01:00.0
                    let b = b.trim().to_ascii_lowercase();
                    let b = if b.len() == 16 { b[4..].to_string() } else { b };
                    Some((b, i.trim().to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let running: Vec<crate::containers::Meta> = d.node.list().unwrap_or_default().into_iter().filter(|(_, s)| s.state == "running").map(|(m, _)| m).collect();
    gpus.into_iter()
        .filter_map(|g| {
            let i = idx.get(&g.to_ascii_lowercase());
            let users: Vec<String> = running.iter().filter(|m| m.gpus.iter().any(|x| x.ends_with("=all") || i.is_some_and(|i| x.ends_with(&format!("={i}"))))).map(|m| m.name.clone()).collect();
            (!users.is_empty()).then(|| (g.clone(), users))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(name: &str, class: Class, idle: bool, target: u64) -> VmSnap {
        VmSnap { name: name.into(), class, memory: 4096, balloon_min: 1024, target, idle }
    }

    #[test]
    fn balloons_by_class_and_load() {
        let calm = HostSnap { used_pct: 40.0, pressure: false };
        let busy = HostSnap { used_pct: 75.0, pressure: false };
        let press = HostSnap { used_pct: 95.0, pressure: true };
        // calm: nothing to do, full memory
        assert!(decide(calm, &[vm("a", Class::Normal, true, 4096)]).is_empty());
        // 75 % used: idle ones give back, busy ones keep
        assert_eq!(decide(busy, &[vm("a", Class::Normal, true, 4096), vm("b", Class::Normal, false, 4096)]), vec![("a".to_string(), 1024)]);
        // pressure: batch to min, normal halfway, critical untouched
        let r = decide(press, &[vm("b", Class::Batch, false, 4096), vm("n", Class::Normal, false, 4096), vm("c", Class::Critical, false, 4096)]);
        assert_eq!(r, vec![("b".to_string(), 1024), ("n".to_string(), 2560)]);
        // calm again: back to full
        assert_eq!(decide(calm, &[vm("b", Class::Batch, false, 1024)]), vec![("b".to_string(), 4096)]);
    }
}
