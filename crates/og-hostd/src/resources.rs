//! Live resource caps (DESIGN.md §4.10): read and change a container's memory, CPU, pids and
//! disk I/O limits while it runs. Changes go to its cgroup at once and into its bundle
//! (config.json), so they survive restarts. Presets: `minimal` and `restore`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::containers::Node;

const MIB: i64 = 1 << 20;

/// A change (fields left out stay as they are) or, from [`current`], the full picture.
/// Unlimited: memory_max / memory_high 0, cpus 0, pids_max 0.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_high: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_low: Option<i64>,
    /// CPU quota in cores, e.g. 1.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<f64>,
    /// cpu.weight 1..=10000 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u64>,
    /// Pinning, e.g. "0-3"; empty = all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpuset_cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_max: Option<i64>,
    /// io.weight 1..=10000 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_weight: Option<u16>,
    /// Replaces all disk I/O caps; empty list = none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io: Option<Vec<og_runtime::IoLimit>>,
}

fn bundle(node: &Node, id: &str) -> PathBuf {
    node.dir(id).join("bundle/config.json")
}

fn load(node: &Node, id: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(bundle(node, id)).context("reading the container's bundle")?)?)
}

/// The container's cgroup directory.
pub fn cgroup_of(node: &Node, id: &str) -> Option<PathBuf> {
    cgroup_dir(&load(node, id).ok()?)
}

fn cgroup_dir(spec: &Value) -> Option<PathBuf> {
    spec["linux"]["cgroupsPath"].as_str().map(|p| PathBuf::from("/sys/fs/cgroup").join(p.trim_start_matches('/')))
}

/// A value set in the spec's `unified` map (e.g. "memory.high"), as the bundle has it.
pub fn configured_unified(node: &Node, id: &str, key: &str) -> Option<String> {
    load(node, id).ok()?["linux"]["resources"]["unified"][key].as_str().map(String::from)
}

fn shares_to_weight(shares: u64) -> u64 {
    // the conversion runc/libcontainer use for cgroup v2
    1 + (shares.clamp(2, 262144) - 2) * 9999 / 262142
}

pub fn current(node: &Node, id: &str) -> Result<Resources> {
    let spec = load(node, id)?;
    let r = &spec["linux"]["resources"];
    let unified = |k: &str| r["unified"][k].as_str().map(String::from);
    let num = |s: Option<String>| s.and_then(|v| if v == "max" { Some(0) } else { v.parse::<i64>().ok() });
    let io = ["throttleReadBpsDevice", "throttleWriteBpsDevice", "throttleReadIOPSDevice", "throttleWriteIOPSDevice"];
    let mut limits: Vec<og_runtime::IoLimit> = Vec::new();
    for (i, key) in io.iter().enumerate() {
        for d in r["blockIO"][key].as_array().into_iter().flatten() {
            let path = PathBuf::from(format!("/dev/block/{}:{}", d["major"], d["minor"]));
            let l = match limits.iter_mut().find(|l| l.device == path) {
                Some(l) => l,
                None => {
                    limits.push(og_runtime::IoLimit { device: path.clone(), read_bps: None, write_bps: None, read_iops: None, write_iops: None });
                    limits.last_mut().unwrap()
                }
            };
            let rate = d["rate"].as_u64();
            match i {
                0 => l.read_bps = rate,
                1 => l.write_bps = rate,
                2 => l.read_iops = rate,
                _ => l.write_iops = rate,
            }
        }
    }
    Ok(Resources {
        memory_max: Some(r["memory"]["limit"].as_i64().filter(|v| *v > 0).unwrap_or(0)),
        memory_high: Some(num(unified("memory.high")).unwrap_or(0)),
        memory_low: Some(num(unified("memory.low")).unwrap_or(0)),
        cpus: Some(match (r["cpu"]["quota"].as_i64(), r["cpu"]["period"].as_u64()) {
            (Some(q), Some(p)) if q > 0 && p > 0 => q as f64 / p as f64,
            _ => 0.0,
        }),
        cpu_weight: Some(unified("cpu.weight").and_then(|w| w.parse().ok()).or_else(|| r["cpu"]["shares"].as_u64().map(shares_to_weight)).unwrap_or(100)),
        cpuset_cpus: Some(r["cpu"]["cpus"].as_str().unwrap_or("").to_string()),
        pids_max: Some(r["pids"]["limit"].as_i64().filter(|v| *v > 0).unwrap_or(0)),
        io_weight: Some(unified("io.weight").and_then(|w| w.trim_start_matches("default ").parse().ok()).unwrap_or(100)),
        io: Some(limits),
    })
}

fn validate(r: &Resources) -> Result<()> {
    if r.memory_max.is_some_and(|m| m != 0 && m < 6 * MIB) {
        bail!("memory_max must be at least 6 MiB (or 0 for unlimited)");
    }
    if r.cpus.is_some_and(|c| !(0.0..=1024.0).contains(&c) || (c > 0.0 && c < 0.01)) {
        bail!("cpus must be 0 (unlimited) or between 0.01 and 1024");
    }
    if r.cpu_weight.is_some_and(|w| !(1..=10000).contains(&w)) {
        bail!("cpu_weight must be 1..=10000");
    }
    if r.io_weight.is_some_and(|w| !(1..=10000).contains(&w)) {
        bail!("io_weight must be 1..=10000");
    }
    if let Some(c) = &r.cpuset_cpus {
        if !c.chars().all(|ch| ch.is_ascii_digit() || ch == ',' || ch == '-') {
            bail!("cpuset_cpus looks like 0-3,6");
        }
    }
    Ok(())
}

/// Apply a change: bundle first (so a failure leaves the cgroup as it was), then the cgroup.
pub fn update(node: &Node, id: &str, r: &Resources) -> Result<Resources> {
    validate(r)?;
    let m = node.find(id)?;
    let before = current(node, &m.id)?;
    let mut spec = load(node, &m.id)?;
    {
        let res = &mut spec["linux"]["resources"];
        if !res.is_object() {
            *res = json!({});
        }
        let set_unified = |res: &mut Value, k: &str, v: Option<String>| {
            if !res["unified"].is_object() {
                res["unified"] = json!({});
            }
            match v {
                Some(v) => res["unified"][k] = json!(v),
                None => {
                    res["unified"].as_object_mut().unwrap().remove(k);
                }
            }
        };
        if let Some(v) = r.memory_max {
            if v > 0 {
                res["memory"]["limit"] = json!(v);
            } else if let Some(o) = res["memory"].as_object_mut() {
                o.remove("limit");
            }
        }
        if let Some(v) = r.memory_high {
            set_unified(res, "memory.high", (v > 0).then(|| v.to_string()));
        }
        if let Some(v) = r.memory_low {
            set_unified(res, "memory.low", (v > 0).then(|| v.to_string()));
        }
        if let Some(c) = r.cpus {
            if !res["cpu"].is_object() {
                res["cpu"] = json!({});
            }
            if c > 0.0 {
                res["cpu"]["quota"] = json!((c * 100_000.0).round() as i64);
                res["cpu"]["period"] = json!(100_000);
            } else {
                let o = res["cpu"].as_object_mut().unwrap();
                o.remove("quota");
                o.remove("period");
            }
        }
        if let Some(w) = r.cpu_weight {
            if let Some(o) = res["cpu"].as_object_mut() {
                o.remove("shares");
            }
            set_unified(res, "cpu.weight", Some(w.to_string()));
        }
        if let Some(c) = &r.cpuset_cpus {
            if !res["cpu"].is_object() {
                res["cpu"] = json!({});
            }
            if c.is_empty() {
                res["cpu"].as_object_mut().unwrap().remove("cpus");
            } else {
                res["cpu"]["cpus"] = json!(c);
            }
        }
        if let Some(p) = r.pids_max {
            res["pids"]["limit"] = json!(if p > 0 { p } else { -1 });
        }
        if let Some(w) = r.io_weight {
            set_unified(res, "io.weight", Some(format!("default {w}")));
        }
        if let Some(io) = &r.io {
            let mut lists: [Vec<Value>; 4] = Default::default();
            for l in io {
                let (maj, min) = l.disk().map_err(|e| anyhow::anyhow!("{e}"))?;
                for (i, v) in [l.read_bps, l.write_bps, l.read_iops, l.write_iops].into_iter().enumerate() {
                    if let Some(rate) = v {
                        lists[i].push(json!({ "major": maj, "minor": min, "rate": rate }));
                    }
                }
            }
            let [a, b, c, d] = lists;
            res["blockIO"] = json!({ "throttleReadBpsDevice": a, "throttleWriteBpsDevice": b, "throttleReadIOPSDevice": c, "throttleWriteIOPSDevice": d });
        }
    }
    let path = bundle(node, &m.id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&spec)?)?;
    std::fs::rename(&tmp, &path)?;

    if node.status(&m.id).state == "running" {
        if let Some(dir) = cgroup_dir(&spec) {
            write_cgroup(&dir, r, &before)?;
        }
    }
    current(node, &m.id)
}

fn write_cgroup(dir: &std::path::Path, r: &Resources, before: &Resources) -> Result<()> {
    let w = |f: &str, v: String| std::fs::write(dir.join(f), &v).with_context(|| format!("{f} <- {v}"));
    let max_or = |v: i64| if v > 0 { v.to_string() } else { "max".into() };
    if let Some(v) = r.memory_max {
        w("memory.max", max_or(v))?;
    }
    if let Some(v) = r.memory_high {
        w("memory.high", max_or(v))?;
    }
    if let Some(v) = r.memory_low {
        w("memory.low", v.max(0).to_string())?;
    }
    if let Some(c) = r.cpus {
        w("cpu.max", if c > 0.0 { format!("{} 100000", (c * 100_000.0).round() as i64) } else { "max 100000".into() })?;
    }
    if let Some(v) = r.cpu_weight {
        w("cpu.weight", v.to_string())?;
    }
    if let Some(c) = &r.cpuset_cpus {
        // empty = inherit everything from the parent
        let all = std::fs::read_to_string(dir.join("../cpuset.cpus.effective")).unwrap_or_default();
        w("cpuset.cpus", if c.is_empty() { all.trim().to_string() } else { c.clone() })?;
    }
    if let Some(p) = r.pids_max {
        w("pids.max", max_or(p))?;
    }
    if let Some(v) = r.io_weight {
        w("io.weight", format!("default {v}"))?;
    }
    if let Some(io) = &r.io {
        let mut done = Vec::new();
        for l in io {
            w("io.max", l.io_max_line().map_err(|e| anyhow::anyhow!("{e}"))?)?;
            done.push(l.disk().map_err(|e| anyhow::anyhow!("{e}"))?);
        }
        // devices no longer capped
        for l in before.io.iter().flatten() {
            if let Ok((maj, min)) = l.disk() {
                if !done.contains(&(maj, min)) {
                    w("io.max", format!("{maj}:{min} rbps=max wbps=max riops=max wiops=max"))?;
                }
            }
        }
    }
    Ok(())
}

/// `minimal`: a quarter core, lowest CPU and I/O weight, memory held where it is (nothing is
/// taken away, it just can't grow). `restore`: back to what it had before `minimal`.
pub fn preset(node: &Node, id: &str, name: &str) -> Result<Resources> {
    let mut m = node.find(id)?;
    match name {
        "minimal" => {
            if m.saved_resources.is_none() {
                m.saved_resources = Some(current(node, &m.id)?);
                node.save_meta(&m)?;
            }
            let spec = load(node, &m.id)?;
            let used = cgroup_dir(&spec).and_then(|d| std::fs::read_to_string(d.join("memory.current")).ok()).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0);
            update(node, &m.id, &Resources { cpus: Some(0.25), cpu_weight: Some(1), io_weight: Some(1), memory_high: Some(used.max(32 * MIB)), ..Default::default() })
        }
        "restore" => {
            let Some(saved) = m.saved_resources.take() else { bail!("nothing to restore: no minimal preset is active") };
            let r = update(node, &m.id, &saved)?;
            node.save_meta(&m)?;
            Ok(r)
        }
        _ => bail!("unknown preset {name:?} (minimal, restore)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_and_validation() {
        assert_eq!(shares_to_weight(1024), 39);
        assert_eq!(shares_to_weight(2), 1);
        assert_eq!(shares_to_weight(262144), 10000);
        assert!(validate(&Resources { memory_max: Some(MIB), ..Default::default() }).is_err());
        assert!(validate(&Resources { memory_max: Some(0), cpus: Some(1.5), ..Default::default() }).is_ok());
        assert!(validate(&Resources { cpus: Some(0.001), ..Default::default() }).is_err());
        assert!(validate(&Resources { cpuset_cpus: Some("0-3;rm".into()), ..Default::default() }).is_err());
    }
}
