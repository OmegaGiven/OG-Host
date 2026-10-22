//! `og-hostd up` / `down`: run a compose project on this node.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use og_compose::{MountKind, Project, Service};
use og_oci::store::Store;

use crate::containers::{parse_size, Extra, NetChoice, Node};

fn container_name(p: &Project, s: &Service) -> String {
    s.container_name.clone().unwrap_or_else(|| format!("{}-{}-1", p.name, s.name))
}

fn network_name(p: &Project, net: &str) -> String {
    if p.networks.get(net).copied().unwrap_or(false) { net.to_string() } else { format!("{}_{}", p.name, net) }
}

fn volume_dir(root: &Path, p: &Project, vol: &str) -> PathBuf {
    let name = if p.volumes.get(vol).copied().unwrap_or(false) { vol.to_string() } else { format!("{}_{}", p.name, vol) };
    root.join("volumes").join(name)
}

fn capability(name: &str) -> Result<og_runtime::Capability> {
    let n = name.trim().to_ascii_uppercase();
    let n = if n.starts_with("CAP_") { n } else { format!("CAP_{n}") };
    serde_json::from_value(serde_json::Value::String(n.clone())).with_context(|| format!("unknown capability {name:?}"))
}

/// Image name for a service: its `image:`, or `<project>-<service>` for build-only services.
fn image_for(p: &Project, s: &Service) -> String {
    s.image.clone().unwrap_or_else(|| format!("{}-{}", p.name, s.name))
}

pub async fn up(node: &Node, root: &Path, store: &Store, p: &Project, rebuild: bool) -> Result<()> {
    for w in &p.warnings {
        eprintln!("warning: {w}");
    }
    let names: BTreeMap<String, String> = p.services.iter().map(|s| (s.name.clone(), container_name(p, s))).collect();

    // pull everything first, so a bad image fails before anything starts
    let mut digests = BTreeMap::new();
    for s in &p.services {
        let image = image_for(p, s);
        let d = match &s.build {
            Some(b) if rebuild || !crate::build::have_image(root, &image) => {
                let spec = crate::build::BuildSpec {
                    context: b.context.clone(),
                    dockerfile: b.dockerfile.clone(),
                    tag: image.clone(),
                    args: b.args.clone(),
                    target: b.target.clone(),
                };
                let root = root.to_path_buf();
                let svc = s.name.clone();
                tokio::task::spawn_blocking(move || crate::build::build(&root, &spec)).await?.with_context(|| format!("building service {svc}"))?
            }
            _ => crate::ensure_pulled(root, store, &image, true).await.with_context(|| format!("service {}", s.name))?,
        };
        digests.insert(s.name.clone(), d);
    }

    let existing = node.list()?;
    for s in &p.services {
        let name = names[&s.name].clone();
        if let Some((m, st)) = existing.iter().find(|(m, _)| m.name == name) {
            if st.state == "running" {
                println!("{name:<28} up to date");
                continue;
            }
            node.remove(&m.id)?;
        }
        let choice = match s.network_mode.as_deref() {
            Some("host") => NetChoice::Host,
            Some("none") => NetChoice::None,
            Some(m) if m.starts_with("service:") => {
                let other = &m["service:".len()..];
                NetChoice::Container(names.get(other).cloned().with_context(|| format!("{}: unknown service {other}", s.name))?)
            }
            Some(m) if m.starts_with("container:") => NetChoice::Container(m["container:".len()..].to_string()),
            Some(m) => bail!("{}: network_mode {m:?} not supported", s.name),
            None => {
                if s.networks.len() > 1 {
                    eprintln!("warning: {}: only the first of its {} networks is attached for now", s.name, s.networks.len());
                }
                let net = s.networks.first().map(|n| network_name(p, n)).unwrap_or_else(|| format!("{}_default", p.name));
                if s.networks.first().is_some_and(|n| p.networks.get(n).copied().unwrap_or(false)) && node.net.get(&net)?.is_none() {
                    bail!("{}: external network {net:?} does not exist", s.name);
                }
                NetChoice::Network(net)
            }
        };
        let ports = s.ports.iter().map(|x| og_net::PortMap::parse(x)).collect::<std::result::Result<Vec<_>, _>>().with_context(|| format!("service {}", s.name))?;

        let mut binds = Vec::new();
        let mut tmpfs = Vec::new();
        let mut volumes = Vec::new();
        for m in &s.mounts {
            match m.kind {
                MountKind::Bind => {
                    if m.source.ends_with("docker.sock") {
                        eprintln!("warning: {}: docker.sock mount skipped (OG-Host's Docker API isn't available yet)", s.name);
                        continue;
                    }
                    let src = PathBuf::from(&m.source);
                    if !src.exists() {
                        std::fs::create_dir_all(&src).with_context(|| format!("creating bind source {}", src.display()))?;
                    }
                    binds.push(og_runtime::BindMount { source: std::fs::canonicalize(&src)?, target: PathBuf::from(&m.target), read_only: m.read_only });
                }
                MountKind::Volume => volumes.push((volume_dir(root, p, &m.source), m.target.clone(), m.read_only)),
                MountKind::Tmpfs => tmpfs.push((PathBuf::from(&m.target), None)),
            }
        }
        let cap_add = s.cap_add.iter().map(|c| capability(c)).collect::<Result<Vec<_>>>()?;
        let cap_drop = s.cap_drop.iter().filter(|c| c.to_ascii_uppercase() != "ALL").map(|c| capability(c)).collect::<Result<Vec<_>>>()?;
        if s.cap_drop.iter().any(|c| c.eq_ignore_ascii_case("all")) {
            eprintln!("warning: {}: cap_drop ALL not supported yet, using the default set", s.name);
        }
        let devices: Vec<PathBuf> = s.devices.iter().map(|d| PathBuf::from(d.split(':').next().unwrap_or(d))).collect();
        let memory = s.mem_limit.as_deref().map(parse_size).transpose()?;
        let mut io_limits: Vec<og_runtime::IoLimit> = Vec::new();
        for (path, kind, rate) in &s.blkio_devices {
            let rate = parse_size(rate).with_context(|| format!("{}: blkio rate {rate:?}", s.name))?.max(0) as u64;
            let l = match io_limits.iter_mut().find(|l| l.device.as_os_str() == path.as_str()) {
                Some(l) => l,
                None => {
                    io_limits.push(og_runtime::IoLimit { device: path.into(), read_bps: None, write_bps: None, read_iops: None, write_iops: None });
                    io_limits.last_mut().unwrap()
                }
            };
            match kind {
                0 => l.read_bps = Some(rate),
                1 => l.write_bps = Some(rate),
                2 => l.read_iops = Some(rate),
                _ => l.write_iops = Some(rate),
            }
        }
        let shm = s.shm_size.as_deref().map(parse_size).transpose()?.map(|v| v as u64);
        if s.privileged {
            eprintln!("warning: {}: privileged mode is not supported; running with default privileges", s.name);
        }

        // wait for dependencies to be ready, not just started
        for (dep, cond) in &s.depends_on {
            let dep_name = &names[dep];
            let dep_svc = p.services.iter().find(|x| &x.name == dep).unwrap();
            wait_for(node, dep_name, dep_svc, cond)?;
        }
        let extra = Extra {
            project: Some(p.name.clone()),
            service: Some(s.name.clone()),
            restart: s.restart.clone(),
            volumes,
            extra_hosts: s.extra_hosts.clone(),
            peers: project_peers(node, &p.name)?,
            healthcheck: s.healthcheck.clone(),
            labels: {
                let mut l = s.labels.clone();
                l.insert("com.docker.compose.project".into(), p.name.clone());
                l.insert("com.docker.compose.service".into(), s.name.clone());
                l.insert("com.docker.compose.project.working_dir".into(), p.dir.display().to_string());
                if let Some(f) = &p.file {
                    l.insert("com.docker.compose.project.config_files".into(), f.display().to_string());
                }
                l
            },
            auto_remove: false,
            no_start: false,
            aliases: vec![s.name.clone()],
        };
        let image = image_for(p, s);
        let digest = digests[&s.name].clone();
        let svc = s.clone();
        let t0 = std::time::Instant::now();
        let meta = tokio::task::block_in_place(|| {
            node.run(&image, &digest, Some(&name), choice, ports, extra, |o| {
                o.hostname = svc.hostname.clone().unwrap_or_else(|| svc.name.clone());
                o.env = svc.environment.clone();
                o.command = svc.command.clone();
                o.entrypoint = svc.entrypoint.clone();
                o.user = svc.user.clone();
                o.working_dir = svc.working_dir.clone();
                o.mounts = binds;
                o.tmpfs = tmpfs;
                o.devices = devices;
                o.seccomp_unconfined = svc.security_opt.iter().any(|x| crate::containers::is_seccomp_unconfined(x));
                if svc.gpus {
                    o.cdi_devices = vec!["nvidia.com/gpu=all".into()];
                }
                o.cap_add = cap_add;
                o.cap_drop = cap_drop;
                o.memory_max = memory;
                o.memory_low = svc.mem_reservation.as_deref().and_then(|v| crate::containers::parse_size(v).ok());
                o.cpu_quota = svc.cpus.map(|c| ((c * 100_000.0).round() as i64, 100_000));
                o.cpuset_cpus = svc.cpuset.clone();
                o.cpu_shares = svc.cpu_shares;
                if let Some(p) = svc.pids_limit {
                    o.pids_max = p;
                }
                o.io_weight = svc.blkio_weight.map(|w| w.saturating_mul(10).min(10000));
                o.io_limits = io_limits.clone();
                o.shm_size = shm;
            })
        })
        .with_context(|| format!("starting service {}", s.name))?;
        let ip = meta.net.as_ref().and_then(|a| a.ip).map(|i| i.to_string()).unwrap_or_else(|| meta.network_mode.clone());
        println!("{name:<28} started in {:.2}s  {ip}", t0.elapsed().as_secs_f64());
        refresh_hosts(node, &p.name)?; // earlier services learn about this one
    }
    Ok(())
}

/// (service or container name, ip) for every running container in the project.
fn project_peers(node: &Node, project: &str) -> Result<Vec<(String, String)>> {
    let all = node.list()?;
    let mut peers = Vec::new();
    for (m, st) in all.iter().filter(|(m, st)| m.project.as_deref() == Some(project) && st.state == "running") {
        let _ = st;
        let ip = match m.net.as_ref().and_then(|a| a.ip) {
            Some(ip) => Some(ip),
            // service:x containers answer on x's address
            None => m.network_mode.strip_prefix("container:").and_then(|id| all.iter().find(|(o, _)| o.id == id)).and_then(|(o, _)| o.net.as_ref().and_then(|a| a.ip)),
        };
        if let (Some(ip), Some(svc)) = (ip, m.service.as_ref()) {
            peers.push((svc.clone(), ip.to_string()));
            if &m.name != svc {
                peers.push((m.name.clone(), ip.to_string()));
            }
        }
    }
    Ok(peers)
}

fn refresh_hosts(node: &Node, project: &str) -> Result<()> {
    let peers = project_peers(node, project)?;
    for (m, _) in node.list()?.iter().filter(|(m, _)| m.project.as_deref() == Some(project)) {
        let own = m.service.clone();
        node.set_hosts(m, &peers.iter().filter(|(n, _)| Some(n) != own.as_ref() && n != &m.name).cloned().collect::<Vec<_>>())?;
    }
    Ok(())
}

/// Block until `dep` satisfies the depends_on condition (or give up with an error).
fn wait_for(node: &Node, container: &str, svc: &Service, cond: &og_compose::DependsCondition) -> Result<()> {
    use og_compose::DependsCondition as C;
    let m = node.find(container)?;
    match cond {
        C::Started => Ok(()),
        C::CompletedSuccessfully => {
            for _ in 0..3600 {
                let st = node.status(&m.id);
                match (st.state.as_str(), st.exit_code) {
                    ("exited", Some(0)) => return Ok(()),
                    ("exited", Some(c)) => bail!("{} exited with {c}; dependants not started", svc.name),
                    ("failed", _) => bail!("{} failed to start", svc.name),
                    _ => std::thread::sleep(std::time::Duration::from_secs(1)),
                }
            }
            bail!("{} did not complete within an hour", svc.name)
        }
        C::Healthy => {
            let Some(hc) = svc.healthcheck.as_ref().filter(|h| !h.disabled && !h.test.is_empty()) else {
                bail!("{} has no healthcheck, but a dependant waits for it to be healthy", svc.name);
            };
            let argv: Vec<String> = match hc.test.first().map(String::as_str) {
                Some("CMD-SHELL") => vec!["/bin/sh".into(), "-c".into(), hc.test[1..].join(" ")],
                Some("CMD") => hc.test[1..].to_vec(),
                _ => vec!["/bin/sh".into(), "-c".into(), hc.test.join(" ")],
            };
            let user = node.process_user(&m.id);
            let start = std::time::Instant::now();
            let mut failures = 0;
            let interval = hc.interval_s.clamp(1, 300);
            loop {
                let (code, out) = node.exec(&m.id, &argv, user, hc.timeout_s.clamp(1, 300))?;
                if code == 0 {
                    println!("{container:<28} healthy after {:.1}s", start.elapsed().as_secs_f64());
                    return Ok(());
                }
                if start.elapsed().as_secs() >= hc.start_period_s {
                    failures += 1;
                }
                if failures > hc.retries.max(1) + 10 {
                    bail!("{} stayed unhealthy: {}", svc.name, out.trim().lines().last().unwrap_or(""));
                }
                if node.status(&m.id).state != "running" {
                    bail!("{} stopped while waiting for it to become healthy", svc.name);
                }
                std::thread::sleep(std::time::Duration::from_secs(interval.min(2)));
            }
        }
    }
}

pub fn down(node: &Node, root: &Path, p: &Project, volumes: bool) -> Result<()> {
    let list = node.list()?;
    // reverse start order: dependents first
    for s in p.services.iter().rev() {
        let name = container_name(p, s);
        if let Some((m, _)) = list.iter().find(|(m, _)| m.name == name) {
            let st = node.stop(&m.id, s.stop_grace_s.unwrap_or(10))?;
            node.remove(&m.id)?;
            println!("{name:<28} removed ({})", st.state);
        }
    }
    let mut nets: Vec<String> = p.services.iter().flat_map(|s| s.networks.iter().map(|n| network_name(p, n))).collect();
    nets.push(format!("{}_default", p.name));
    nets.sort();
    nets.dedup();
    for n in nets {
        if p.networks.iter().any(|(k, ext)| *ext && *k == n) {
            continue; // external: not ours to remove
        }
        if node.net.get(&n)?.is_some() {
            node.net.remove(&n)?;
            println!("network {n:<20} removed");
        }
    }
    if volumes {
        for (v, ext) in &p.volumes {
            if !ext {
                let d = volume_dir(root, p, v);
                if d.exists() {
                    std::fs::remove_dir_all(&d)?;
                    println!("volume {v:<21} removed");
                }
            }
        }
    }
    Ok(())
}
