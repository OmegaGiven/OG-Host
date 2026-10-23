//! Container lifecycle on one node: prepare rootfs + bundle, launch og-shim, track state.
//!
//! Layout under the state root:
//!   containers/<id>/meta.json     what was asked for (image, name, options)
//!   containers/<id>/rootfs/       root filesystem: overlay of layers/ + upper/ (or a private copy)
//!   containers/<id>/upper/        its own changes (overlayfs upper layer)
//!   layers/<digest>-<offset>/     image layers unpacked once, shared by containers
//!   containers/<id>/bundle/       OCI bundle (config.json)
//!   containers/<id>/log           stdout + stderr
//!   containers/<id>/status.json   written by og-shim: running / exited / failed

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use og_oci::{registry::ImageConfig, store::Store, unpack};
use og_runtime::{IdMap, RunOptions};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub name: String,
    pub image: String,
    pub manifest_digest: String,
    pub created_at: u64,
    pub args: Vec<String>,
    #[serde(default)]
    pub net: Option<og_net::Attachment>,
    /// `host`, `none`, `container:<id>`, or a network name.
    #[serde(default)]
    pub network_mode: String,
    /// Compose project and service this container belongs to.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub restart: Option<String>,
    /// "running" or "stopped" — what the user wants; the supervisor works towards it.
    #[serde(default)]
    pub desired: String,
    /// Bind mounts as requested (before id-mapping), so they can be rebuilt after a reboot.
    #[serde(default)]
    pub binds: Vec<(PathBuf, PathBuf, bool)>,
    /// Runs in the host's user namespace (`userns_mode: host`).
    #[serde(default)]
    pub userns_host: bool,
    #[serde(default)]
    pub healthcheck: Option<og_compose::Healthcheck>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// Named volumes: (volume dir, target, read-only).
    #[serde(default)]
    pub volumes: Vec<(PathBuf, String, bool)>,
    /// Resources before a `minimal` preset, for `restore`.
    #[serde(default)]
    pub saved_resources: Option<crate::resources::Resources>,
    /// CDI devices (GPUs) it was given.
    #[serde(default)]
    pub gpus: Vec<String>,
    /// Shared unpacked image layers (bottom first) its rootfs stacks with overlayfs; empty =
    /// a private unpacked rootfs (older containers, or no overlayfs).
    #[serde(default)]
    pub lowers: Vec<PathBuf>,
    /// Remove the container once it exits (docker run --rm).
    #[serde(default)]
    pub auto_remove: bool,
    /// Extra names other containers on the same network can use (network aliases).
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Extra settings that only compose (or later the API) provides.
#[derive(Debug, Clone, Default)]
pub struct Extra {
    pub project: Option<String>,
    pub service: Option<String>,
    pub restart: Option<String>,
    /// Named volumes: (volume dir on host, target in container, read-only).
    pub volumes: Vec<(PathBuf, String, bool)>,
    pub extra_hosts: Vec<(String, String)>,
    /// DNS servers, search domains and resolver options for the container's /etc/resolv.conf
    pub dns: Vec<String>,
    pub dns_search: Vec<String>,
    pub dns_opt: Vec<String>,
    /// `userns_mode: host`: run in the host's user namespace (no id-mapping: binds of
    /// filesystems that can't be id-mapped, like NFS, work). Capabilities, seccomp, read-only
    /// root and `user:` still apply.
    pub userns_host: bool,
    /// Other services already running in the project (name, ip): known from the start.
    pub peers: Vec<(String, String)>,
    pub healthcheck: Option<og_compose::Healthcheck>,
    pub labels: std::collections::BTreeMap<String, String>,
    pub auto_remove: bool,
    /// Create only; don't start (Docker API `create`).
    pub no_start: bool,
    pub aliases: Vec<String>,
}

/// How a container is connected.
#[derive(Debug, Clone)]
pub enum NetChoice {
    Network(String),
    None,
    Host,
    Container(String),
}

impl NetChoice {
    pub fn parse(s: &str) -> Self {
        match s {
            "none" => NetChoice::None,
            "host" => NetChoice::Host,
            _ => match s.strip_prefix("container:").or_else(|| s.strip_prefix("service:")) {
                Some(c) => NetChoice::Container(c.to_string()),
                None => NetChoice::Network(s.to_string()),
            },
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Status {
    pub state: String,
    pub pid: Option<i32>,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub oom_killed: bool,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub error: Option<String>,
}

pub struct Node {
    pub root: PathBuf,
    pub store: Store,
    pub runtime_root: PathBuf,
    pub net: og_net::Net,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn unix_now() -> u64 {
    now()
}

pub fn new_id() -> Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Container names: like Docker, `[a-zA-Z0-9][a-zA-Z0-9_.-]*`.
fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 128 && n.starts_with(|c: char| c.is_ascii_alphanumeric())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

impl Node {
    pub(crate) fn dir(&self, id: &str) -> PathBuf {
        self.root.join("containers").join(id)
    }

    pub fn list(&self) -> Result<Vec<(Meta, Status)>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.root.join("containers")) else { return Ok(out) };
        for e in rd.flatten() {
            let d = e.path();
            let Ok(meta) = std::fs::read(d.join("meta.json")).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice::<Meta>(&b)?)) else { continue };
            let mut st: Status = std::fs::read(d.join("status.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            // a "running" container whose process is gone (host reboot, shim killed) is dead
            if st.state == "running" && st.pid.is_some_and(|p| unsafe { libc::kill(p, 0) } != 0) {
                st.state = "dead".into();
            }
            if st.state.is_empty() {
                st.state = "created".into();
            }
            out.push((meta, st));
        }
        out.sort_by_key(|(m, _)| std::cmp::Reverse(m.created_at));
        Ok(out)
    }

    /// Find a container by id, id prefix (≥4 chars) or name.
    pub fn find(&self, key: &str) -> Result<Meta> {
        let all = self.list()?;
        let hits: Vec<_> = all.into_iter().filter(|(m, _)| m.name == key || m.id == key || (key.len() >= 4 && m.id.starts_with(key))).collect();
        match hits.len() {
            0 => bail!("no container {key:?}"),
            1 => Ok(hits.into_iter().next().unwrap().0),
            _ => bail!("{key:?} matches more than one container; use a longer id"),
        }
    }

    pub fn status(&self, id: &str) -> Status {
        std::fs::read(self.dir(id).join("status.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Create and start a container from a pulled image.
    pub fn run(
        &self,
        image: &str,
        manifest_digest: &str,
        name: Option<&str>,
        net_choice: NetChoice,
        ports: Vec<og_net::PortMap>,
        extra: Extra,
        opts_in: impl FnOnce(&mut RunOptions),
    ) -> Result<Meta> {
        let id = new_id()?;
        let name = name.map(str::to_string).unwrap_or_else(|| format!("c-{}", &id[..8]));
        if !valid_name(&name) {
            bail!("invalid container name {name:?}");
        }
        if self.list()?.iter().any(|(m, _)| m.name == name) {
            bail!("a container named {name:?} already exists");
        }
        let dir = self.dir(&id);
        let rootfs = dir.join("rootfs");
        std::fs::create_dir_all(&rootfs)?;

        // image config + layers -> rootfs, ownership shifted into the userns range: the image's
        // layers unpacked once and shared, stacked with overlayfs (a private writable layer per
        // container); a private unpacked copy when that's not possible
        let manifest: og_oci::registry::Manifest = serde_json::from_slice(&std::fs::read(self.store.blob_path(manifest_digest)?)?)?;
        let image_cfg: ImageConfig = serde_json::from_slice(&std::fs::read(self.store.blob_path(&manifest.config.digest)?)?)?;
        let map = IdMap::default();
        let mut lowers = self.layered_rootfs(&dir, &manifest, map.host_start).unwrap_or_else(|e| {
            eprintln!("og-hostd: {name}: no shared layers ({e:#}); unpacking a private copy");
            vec![]
        });
        if lowers.is_empty() {
            let unpack_opts = unpack::UnpackOptions { preserve_owner: true, id_offset: map.host_start, overlay: false };
            for layer in &manifest.layers {
                let path = self.store.blob_path(&layer.digest)?;
                unpack::apply_layer(&rootfs, unpack::open_layer(&path)?, &unpack_opts).with_context(|| format!("layer {}", layer.digest))?;
            }
        }
        std::os::unix::fs::lchown(&rootfs, Some(map.host_start), Some(map.host_start))?;

        let mut opts = RunOptions::new(name.clone(), rootfs.clone(), PathBuf::from(format!("/og-host/{name}")));
        opts.userns = Some(map);
        opts_in(&mut opts);
        if extra.userns_host {
            opts.userns = None;
        }
        // og.vram: place on the GPU with the most free VRAM (batch work is refused when none fits)
        if let Some(need) = extra.labels.get("og.vram").and_then(|v| parse_size(v).ok()) {
            if opts.cdi_devices.iter().any(|d| d == "nvidia.com/gpu=all") {
                let gpus = crate::brain::read_gpus();
                let need_mib = need.max(0) as u64 >> 20;
                match crate::brain::pick_gpu(&gpus, need_mib) {
                    Some(i) => {
                        opts.cdi_devices.retain(|d| d != "nvidia.com/gpu=all");
                        opts.cdi_devices.push(format!("nvidia.com/gpu={i}"));
                    }
                    None if !gpus.is_empty() => {
                        let best = gpus.iter().map(|g| g.free_mib()).max().unwrap_or(0);
                        if crate::brain::GpuClass::from_labels(&extra.labels) == crate::brain::GpuClass::Batch {
                            bail!("no GPU has {need_mib} MiB of free VRAM (most free: {best} MiB)");
                        }
                        eprintln!("warning: {name}: no GPU has {need_mib} MiB of free VRAM (most free: {best} MiB); starting anyway");
                    }
                    None => {}
                }
            }
        }

        // desktop profile: look like a real PC (DESIGN.md §4.9)
        let desktop = crate::desktop::is_desktop(&extra.labels);
        if desktop {
            opts.desktop = true;
            opts.hostname = crate::desktop::hostname(&extra.labels, &id, &opts.hostname, &name);
        }
        let mac = crate::desktop::mac(&extra.labels, &id);

        // networking: namespace + address before the container exists
        if !ports.is_empty() && !matches!(net_choice, NetChoice::Network(_)) {
            bail!("ports can only be published on a network (not with none/host/container:)");
        }
        // own sandbox (user + network namespace) unless sharing another container's or the host's
        let sandbox = dir.join("sandbox");
        if !extra.userns_host && matches!(net_choice, NetChoice::Network(_) | NetChoice::None) {
            self.create_sandbox(&id, &sandbox, map)?;
            opts.userns_path = Some(sandbox.join("user"));
        }
        let (attachment, network_mode, my_ip) = match &net_choice {
            NetChoice::Network(n) => {
                let a = self.net.attach_mac(&id, n, &ports, mac.as_deref()).with_context(|| format!("attaching to network {n:?}"))?;
                opts.network = og_runtime::Network::Join(a.netns.clone());
                let ip = a.ip;
                (Some(a), n.clone(), ip)
            }
            NetChoice::None => {
                let ns = self.net.create_netns(&id)?;
                opts.network = og_runtime::Network::Join(ns.clone());
                (Some(og_net::Attachment { owner: id.clone(), netns: ns, network: None, ip: None, gateway: None, prefix: None, host_veth: None, ports: vec![] }), "none".into(), None)
            }
            NetChoice::Host => {
                opts.network = og_runtime::Network::Host;
                opts.userns = None; // the host network can't be used from a user namespace
                (None, "host".into(), None)
            }
            NetChoice::Container(other) => {
                let o = self.find(other)?;
                let ns = o.net.as_ref().map(|a| a.netns.clone()).context("that container has no network namespace to share")?;
                opts.network = og_runtime::Network::Join(ns);
                // join its user namespace too: a netns can only be entered from the userns owning it
                // (or from the host's, which owns every user namespace: userns_mode host)
                if !extra.userns_host {
                    opts.userns_path = Some(self.dir(&o.id).join("sandbox/user"));
                }
                (None, format!("container:{}", o.id), o.net.as_ref().and_then(|a| a.ip))
            }
        };
        if opts.userns.is_none() {
            // without a user namespace, files keep their image ownership
            if !lowers.is_empty() {
                umount(&rootfs);
                let _ = std::fs::remove_dir_all(dir.join("upper"));
                let _ = std::fs::remove_dir_all(dir.join("work"));
                lowers = self.layered_rootfs(&dir, &manifest, 0).unwrap_or_default();
            }
            if lowers.is_empty() {
                let _ = std::fs::remove_dir_all(&rootfs);
                std::fs::create_dir_all(&rootfs)?;
                let plain = unpack::UnpackOptions { preserve_owner: true, id_offset: 0, overlay: false };
                for layer in &manifest.layers {
                    let path = self.store.blob_path(&layer.digest)?;
                    unpack::apply_layer(&rootfs, unpack::open_layer(&path)?, &plain)?;
                }
            }
        }

        // named volumes: created on first use and seeded from the image's own content at that
        // path (Docker's copy-up), so e.g. a database data dir starts with the right owner.
        // Their files are already in the container's id range: plain bind, no id-mapping.
        let mut volume_mounts = Vec::new();
        for (vol_dir, target, ro) in &extra.volumes {
            let data = vol_dir.join("_data");
            if !data.exists() {
                std::fs::create_dir_all(&data)?;
                let src = og_oci::unpack::secure_join(&rootfs, std::path::Path::new(target.trim_start_matches('/')))?;
                if src.is_dir() {
                    let st = Command::new("cp").arg("-a").arg(format!("{}/.", src.display())).arg(&data).status()?;
                    if !st.success() {
                        bail!("seeding volume {} from the image failed", vol_dir.display());
                    }
                    use std::os::unix::fs::MetadataExt;
                    let md = std::fs::metadata(&src)?;
                    std::os::unix::fs::lchown(&data, Some(md.uid()), Some(md.gid()))?;
                } else {
                    let owner = if opts.userns.is_some() { map.host_start } else { 0 };
                    std::os::unix::fs::lchown(&data, Some(owner), Some(owner))?;
                }
            }
            volume_mounts.push(og_runtime::BindMount { source: data, target: PathBuf::from(target), read_only: *ro });
        }

        let requested_binds: Vec<(PathBuf, PathBuf, bool)> = opts.mounts.iter().map(|m| (m.source.clone(), m.target.clone(), m.read_only)).collect();
        // bind mounts into a user-namespaced container go through id-mapped staging mounts
        if let Some(userns) = opts.userns_path.clone() {
            let staging_dir = PathBuf::from("/run/og-host/idmap").join(&id);
            for (n, m) in opts.mounts.iter_mut().enumerate() {
                let staging = staging_dir.join(n.to_string());
                crate::idmap::map_mount(&m.source, &staging, &userns)?;
                m.source = staging;
            }
        }

        opts.mounts.extend(volume_mounts);

        // per-container /etc/resolv.conf, /etc/hosts, /etc/hostname (bind-mounted read-only)
        let etc = dir.join("etc");
        std::fs::create_dir_all(&etc)?;
        std::fs::write(etc.join("resolv.conf"), og_net::container_resolv_conf_with(&extra.dns, &extra.dns_search, &extra.dns_opt))?;
        let gateway = attachment.as_ref().and_then(|a| a.gateway).map(|g| g.to_string()).unwrap_or_else(|| "127.0.0.1".into());
        let extra_hosts: Vec<(String, String)> =
            extra.extra_hosts.iter().map(|(h, ip)| (h.clone(), if ip == "host-gateway" { gateway.clone() } else { ip.clone() })).collect();
        let hosts_entries: Vec<(String, String)> = extra.peers.iter().cloned().chain(extra_hosts.iter().cloned()).collect();
        std::fs::write(etc.join("hosts"), og_net::container_hosts(extra.service.as_deref().unwrap_or(&name), my_ip, &hosts_entries))?;
        std::fs::write(etc.join("extra_hosts.json"), serde_json::to_vec(&extra_hosts)?)?;
        std::fs::write(etc.join("hostname"), format!("{}\n", opts.hostname))?;
        let mut etc_files = vec!["resolv.conf", "hosts", "hostname"];
        if desktop {
            std::fs::write(etc.join("machine-id"), format!("{}\n", crate::desktop::machine_id(&id)))?;
            etc_files.push("machine-id");
            // DMI/SMBIOS: generic desktop values over a VM host's, or og.dmi.* labels
            let dmi = dir.join("dmi");
            for (field, value) in crate::desktop::dmi_overrides(&extra.labels, crate::desktop::host_is_virtual()) {
                if !std::path::Path::new("/sys/class/dmi/id").join(&field).exists() {
                    continue;
                }
                std::fs::create_dir_all(&dmi)?;
                std::fs::write(dmi.join(&field), format!("{value}\n"))?;
                opts.mounts.push(og_runtime::BindMount { source: dmi.join(&field), target: PathBuf::from("/sys/devices/virtual/dmi/id").join(&field), read_only: true });
            }
        }
        if !matches!(net_choice, NetChoice::Host) {
            for f in etc_files {
                // the target must exist inside the image for the bind mount
                let target = og_oci::unpack::secure_join(&rootfs, std::path::Path::new("etc").join(f).as_path())?;
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p)?;
                }
                if std::fs::symlink_metadata(&target).map(|m| !m.is_file()).unwrap_or(true) {
                    let _ = std::fs::remove_file(&target);
                    std::fs::write(&target, "")?;
                }
                opts.mounts.push(og_runtime::BindMount { source: etc.join(f), target: PathBuf::from("/etc").join(f), read_only: true });
            }
        }
        let spec = og_runtime::build_spec(&image_cfg, &opts)?;
        let args = spec.process().as_ref().and_then(|p| p.args().clone()).unwrap_or_default();
        og_runtime::write_bundle(&dir.join("bundle"), &spec)?;

        let meta = Meta {
            id: id.clone(),
            name,
            image: image.to_string(),
            manifest_digest: manifest_digest.to_string(),
            created_at: now(),
            args,
            net: attachment,
            network_mode,
            project: extra.project,
            service: extra.service,
            restart: extra.restart,
            desired: if extra.no_start { "created".into() } else { "running".into() },
            binds: requested_binds,
            userns_host: extra.userns_host,
            healthcheck: extra.healthcheck,
            labels: extra.labels,
            auto_remove: extra.auto_remove,
            aliases: extra.aliases,
            gpus: opts.cdi_devices.clone(),
            volumes: extra.volumes.clone(),
            saved_resources: None,
            lowers,
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
        if extra.no_start {
            return Ok(meta);
        }
        self.refresh_network_hosts(&meta); // before the process starts
        if let Err(e) = self.spawn_shim(&id) {
            let _ = self.net.detach(&id);
            self.destroy_sandbox(&id);
            return Err(e);
        }
        self.refresh_network_hosts(&meta);
        Ok(meta)
    }

    /// uid:gid of the container's root on the host (from its rootfs owner), if it's remapped.
    fn stdio_owner(&self, id: &str) -> Option<String> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(self.dir(id).join("rootfs")).ok()?;
        (m.uid() != 0).then(|| format!("{}:{}", m.uid(), m.gid()))
    }

    fn create_sandbox(&self, id: &str, dir: &std::path::Path, map: IdMap) -> Result<()> {
        let shim = std::env::current_exe()?.with_file_name("og-shim");
        let out = Command::new(&shim)
            .arg("sandbox")
            .arg("--dir").arg(dir)
            .arg("--netns-link").arg(PathBuf::from("/run/netns").join(og_net::netns_name(id)))
            .arg("--host-start").arg(map.host_start.to_string())
            .arg("--size").arg(map.size.to_string())
            .output()
            .context("running og-shim sandbox")?;
        if !out.status.success() {
            bail!("creating sandbox: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    fn destroy_sandbox(&self, id: &str) {
        for f in ["user", "net"] {
            let p = self.dir(id).join("sandbox").join(f);
            if let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) {
                unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
            }
        }
    }

    pub(crate) fn save_meta(&self, m: &Meta) -> Result<()> {
        let p = self.dir(&m.id).join("meta.json");
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn set_desired(&self, id: &str, desired: &str) -> Result<()> {
        let mut m = self.find(id)?;
        m.desired = desired.to_string();
        self.save_meta(&m)
    }

    /// Start an existing container again. Recreates what lives under /run and is lost on a
    /// host reboot or after a stop: the sandbox namespaces, network attachment (same address,
    /// same published ports) and id-mapped bind mounts. Everything is idempotent.
    pub fn start(&self, id: &str) -> Result<()> {
        let mut meta = self.find(id)?;
        let current = self.list()?.into_iter().find(|(m, _)| m.id == meta.id).map(|(_, st)| st.state).unwrap_or_default();
        if current == "running" {
            return Ok(());
        }
        let dir = self.dir(&meta.id);
        let mounted = mounted_paths();
        let sandbox = dir.join("sandbox");
        let own_sandbox = !meta.userns_host && (meta.network_mode == "none" || (meta.net.as_ref().is_some_and(|a| a.network.is_some()) && !meta.network_mode.starts_with("container:")));
        if own_sandbox && !mounted.contains(&sandbox.join("user")) {
            let _ = self.net.detach_netns_only(&meta.id);
            self.create_sandbox(&meta.id, &sandbox, IdMap::default())?;
        }
        if let Some(a) = meta.net.clone() {
            if let Some(network) = &a.network {
                let fresh = self.net.attach_mac(&meta.id, network, &a.ports, crate::desktop::mac(&meta.labels, &meta.id).as_deref()).with_context(|| format!("re-attaching to {network}"))?;
                meta.net = Some(fresh);
                self.save_meta(&meta)?;
            }
        }
        if meta.network_mode.starts_with("container:") {
            let other = meta.network_mode.trim_start_matches("container:");
            if self.status(other).state != "running" {
                bail!("{} shares the network of a container that isn't running", meta.name);
            }
        }
        let userns_file = if meta.network_mode.starts_with("container:") {
            self.dir(meta.network_mode.trim_start_matches("container:")).join("sandbox/user")
        } else {
            sandbox.join("user")
        };
        let staging_dir = PathBuf::from("/run/og-host/idmap").join(&meta.id);
        if userns_file.exists() && !meta.userns_host {
            for (n, (src, _, _)) in meta.binds.iter().enumerate() {
                let staging = staging_dir.join(n.to_string());
                if !mounted.contains(&staging) {
                    crate::idmap::map_mount(src, &staging, &userns_file)?;
                }
            }
        }
        // GPU device numbers (/dev/nvidiaN) can change between boots: point the container's
        // devices at the GPUs it was given, as the CDI spec describes them now
        if !meta.gpus.is_empty() {
            refresh_gpu_devices(&dir.join("bundle/config.json"), &meta.gpus).with_context(|| format!("{}: updating GPU devices", meta.name))?;
        }
        let _ = std::fs::remove_file(dir.join("status.json"));
        meta.desired = "running".into();
        self.save_meta(&meta)?;
        self.refresh_network_hosts(&meta); // before the process starts
        self.spawn_shim(&meta.id)?;
        self.refresh_network_hosts(&meta);
        Ok(())
    }

    /// Network a container is reachable on (its own, or the one it shares).
    fn network_of(&self, m: &Meta, all: &[(Meta, Status)]) -> Option<(String, std::net::Ipv4Addr)> {
        if let Some(a) = &m.net {
            if let (Some(n), Some(ip)) = (&a.network, a.ip) {
                return Some((n.clone(), ip));
            }
        }
        let other = m.network_mode.strip_prefix("container:")?;
        let (o, _) = all.iter().find(|(o, _)| o.id == other)?;
        let a = o.net.as_ref()?;
        Some((a.network.clone()?, a.ip?))
    }

    /// Docker-style name resolution: every running container on the same network gets every
    /// other's name, service name and aliases in its /etc/hosts.
    pub fn refresh_network_hosts(&self, changed: &Meta) {
        let Ok(all) = self.list() else { return };
        let Some((net, _)) = self.network_of(changed, &all) else { return };
        let members: Vec<(&Meta, std::net::Ipv4Addr)> = all
            .iter()
            // the container being started counts too: its hosts file must be complete
            // before its process starts resolving names
            .filter(|(m, st)| st.state == "running" || m.id == changed.id)
            .filter_map(|(m, _)| self.network_of(m, &all).filter(|(n, _)| *n == net).map(|(_, ip)| (m, ip)))
            .collect();
        for (m, _) in &members {
            let mut peers = Vec::new();
            for (o, ip) in &members {
                if o.id == m.id {
                    continue;
                }
                let mut names = vec![o.name.clone()];
                names.extend(o.service.clone());
                names.extend(o.aliases.iter().cloned());
                names.sort();
                names.dedup();
                for n in names {
                    peers.push((n, ip.to_string()));
                }
            }
            let _ = self.set_hosts(m, &peers);
        }
    }

    fn spawn_shim(&self, id: &str) -> Result<()> {
        let dir = self.dir(id);
        // the overlay goes away with a reboot
        if let Ok(meta) = self.find(id) {
            if !meta.lowers.is_empty() && !mounted_paths().contains(&dir.join("rootfs")) {
                mount_overlay(&dir, &meta.lowers)?;
            }
        }
        let shim = std::env::current_exe()?.with_file_name("og-shim");
        if !shim.exists() {
            bail!("og-shim not found next to og-hostd ({})", shim.display());
        }
        let mut cmd = Command::new(&shim);
        cmd.args(["run", "--id", id, "--bundle"]).arg(dir.join("bundle"))
            .arg("--state-root").arg(&self.runtime_root)
            .arg("--log").arg(dir.join("log"))
            .arg("--status").arg(dir.join("status.json"))
            .args(self.stdio_owner(id).map(|o| vec!["--stdio-owner".to_string(), o]).unwrap_or_default())
            .stdin(Stdio::null()).stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::OpenOptions::new().create(true).append(true).open(dir.join("shim.log"))?));
        // new session: the shim (and so the container) outlives this process and its terminal
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().context("starting og-shim")?;
        // wait for the shim to report running/failed: usually milliseconds, but createContainer
        // hooks (e.g. NVIDIA's CDI hooks linking the driver libraries) can take a while on a busy disk
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_secs(60) {
            let st = self.status(id);
            match st.state.as_str() {
                "running" | "exited" => return Ok(()),
                "failed" => bail!("container failed to start: {}", st.error.unwrap_or_default()),
                _ => std::thread::sleep(std::time::Duration::from_millis(if t0.elapsed().as_secs() < 5 { 5 } else { 100 })),
            }
        }
        bail!("container did not report a state within 60s (see {})", dir.join("shim.log").display())
    }

    pub fn stop(&self, id: &str, timeout_s: u64) -> Result<Status> {
        let _ = self.set_desired(id, "stopped");
        let st = self.status(id);
        let Some(pid) = st.pid.filter(|_| st.state == "running") else { return Ok(st) };
        let gone = |ms: u64| {
            let t = std::time::Instant::now();
            while t.elapsed() < std::time::Duration::from_millis(ms) {
                if self.status(id).state != "running" {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            false
        };
        // timeout 0: straight to SIGKILL (docker rm -f)
        if timeout_s > 0 {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            if gone(timeout_s * 1000) {
                return Ok(self.status(id));
            }
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        gone(3000);
        Ok(self.status(id))
    }

    /// Send a signal to the container's main process.
    pub fn kill(&self, id: &str, signal: i32) -> Result<()> {
        let st = self.status(id);
        let pid = st.pid.filter(|_| st.state == "running").context("container is not running")?;
        if signal == libc::SIGKILL || signal == libc::SIGTERM {
            let _ = self.set_desired(id, "stopped");
        }
        if unsafe { libc::kill(pid, signal) } != 0 {
            bail!("kill: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        if self.status(id).state == "running" {
            bail!("container is running; stop it first");
        }
        let dir = self.dir(id);
        if !dir.starts_with(self.root.join("containers")) || dir == self.root.join("containers") {
            bail!("refusing to remove {}", dir.display());
        }
        if self.list()?.iter().any(|(m, st)| m.network_mode == format!("container:{id}") && st.state == "running") {
            bail!("another running container uses this container's network");
        }
        self.net.detach(id)?;
        crate::idmap::cleanup(&PathBuf::from("/run/og-host/idmap").join(id));
        self.destroy_sandbox(id);
        umount(&dir.join("rootfs"));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Delete unpacked layers that no container uses and no tagged image needs.
    pub fn prune_layers(&self) {
        let mut keep: std::collections::HashSet<PathBuf> = self.list().unwrap_or_default().into_iter().flat_map(|(m, _)| m.lowers).collect();
        let mut keep_hex = std::collections::HashSet::new();
        fn walk(d: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() { walk(&p, out) } else { out.push(p) }
            }
        }
        let mut refs = vec![];
        walk(&self.root.join("refs"), &mut refs);
        for r in refs {
            let Ok(digest) = std::fs::read_to_string(&r) else { continue };
            let Ok(blob) = self.store.blob_path(digest.trim()) else { continue };
            if let Some(m) = std::fs::read(blob).ok().and_then(|b| serde_json::from_slice::<og_oci::registry::Manifest>(&b).ok()) {
                keep_hex.extend(m.layers.iter().map(|l| l.digest.split(':').next_back().unwrap_or_default().to_string()));
            }
        }
        for e in std::fs::read_dir(self.root.join("layers")).into_iter().flatten().flatten() {
            let (p, n) = (e.path(), e.file_name().to_string_lossy().into_owned());
            let hex = n.trim_start_matches('.').split(['-', '.']).next().unwrap_or_default().to_string();
            if keep.remove(&p) || keep_hex.contains(&hex) {
                continue;
            }
            let _ = std::fs::remove_dir_all(&p);
        }
    }

    /// The image's layers, each unpacked once into `<root>/layers/<digest>-<id offset>/`
    /// (overlayfs whiteouts), stacked under `dir/upper` and mounted at `dir/rootfs`.
    fn layered_rootfs(&self, dir: &Path, manifest: &og_oci::registry::Manifest, offset: u32) -> Result<Vec<PathBuf>> {
        let mut lowers = vec![];
        for layer in &manifest.layers {
            let hex = layer.digest.split(':').next_back().unwrap_or_default();
            if hex.len() < 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("odd layer digest {}", layer.digest);
            }
            let ld = self.root.join("layers").join(format!("{hex}-{offset}"));
            if !ld.exists() {
                // unpacked beside it, then renamed: a half-unpacked layer never shows, and two
                // containers making the same layer at once don't clash
                let tmp = self.root.join("layers").join(format!(".{hex}-{offset}.{}", new_id()?));
                std::fs::create_dir_all(&tmp)?;
                let o = unpack::UnpackOptions { preserve_owner: true, id_offset: offset, overlay: true };
                let r = unpack::apply_layer(&tmp, unpack::open_layer(&self.store.blob_path(&layer.digest)?)?, &o).with_context(|| format!("layer {}", layer.digest));
                if let Err(e) = r {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(e);
                }
                std::os::unix::fs::lchown(&tmp, Some(offset), Some(offset))?;
                if std::fs::rename(&tmp, &ld).is_err() {
                    let _ = std::fs::remove_dir_all(&tmp);
                }
            }
            lowers.push(ld);
        }
        if lowers.is_empty() {
            bail!("an image without layers");
        }
        mount_overlay(dir, &lowers)?;
        Ok(lowers)
    }

    /// Rewrite a container's /etc/hosts in place (same inode, so the bind mount sees it).
    pub fn set_hosts(&self, meta: &Meta, peers: &[(String, String)]) -> Result<()> {
        let dir = self.dir(&meta.id).join("etc");
        let extra: Vec<(String, String)> = std::fs::read(dir.join("extra_hosts.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let ip = meta.net.as_ref().and_then(|a| a.ip);
        let all: Vec<(String, String)> = peers.iter().cloned().chain(extra).collect();
        let text = og_net::container_hosts(meta.service.as_deref().unwrap_or(&meta.name), ip, &all);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).truncate(true).open(dir.join("hosts"))?;
        f.write_all(text.as_bytes())?;
        Ok(())
    }

    /// Run a command inside a running container (its namespaces and root, as `user`).
    /// Returns (exit code, combined output). Used for healthchecks.
    pub fn exec(&self, id: &str, argv: &[String], user: Option<(u32, u32)>, timeout_s: u64) -> Result<(i32, String)> {
        // same confinement as docker exec (cgroup, capabilities, seccomp), bounded by `timeout`
        let user = user.map(|(u, g)| format!("{u}:{g}"));
        let inner = self.exec_command(id, argv, user.as_deref(), &[], None)?;
        let mut cmd = Command::new("timeout");
        cmd.arg("--kill-after=2").arg(format!("{timeout_s}s")).arg(inner.get_program()).args(inner.get_args());
        let out = cmd.stdin(Stdio::null()).output().context("running og-shim exec")?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.code().unwrap_or(-1), text))
    }

    /// `og-shim exec` command for a running container. `user` is Docker's form (name, uid,
    /// uid:gid, name:group), resolved against the container's own /etc/passwd and /etc/group;
    /// None means the container's configured user.
    pub fn exec_command(&self, id: &str, argv: &[String], user: Option<&str>, env: &[String], cwd: Option<&str>) -> Result<std::process::Command> {
        let st = self.status(id);
        if st.state != "running" {
            bail!("container {id} is not running");
        }
        if argv.is_empty() {
            bail!("no command given");
        }
        let (uid, gid, extra) = match user.filter(|u| !u.is_empty()) {
            Some(u) => {
                let ids = og_runtime::users::resolve(&self.dir(id).join("rootfs"), Some(u)).map_err(|e| anyhow::anyhow!("user {u:?}: {e}"))?;
                (ids.uid, ids.gid, ids.additional_gids)
            }
            None => {
                // the container's own user, with the supplementary groups its process has
                let (u, g) = self.process_user(id).unwrap_or((0, 0));
                let extra = serde_json::from_slice::<serde_json::Value>(&std::fs::read(self.dir(id).join("bundle/config.json")).unwrap_or_default())
                    .ok()
                    .and_then(|c| c["process"]["user"]["additionalGids"].as_array().map(|a| a.iter().filter_map(|v| v.as_u64().map(|x| x as u32)).collect()))
                    .unwrap_or_default();
                (u, g, extra)
            }
        };
        let shim = std::env::current_exe()?.with_file_name("og-shim");
        let mut cmd = Command::new(&shim);
        cmd.args(["exec", "--id", id]).arg("--state-root").arg(&self.runtime_root).arg("--user").arg(format!("{uid}:{gid}"));
        for g in extra {
            cmd.arg("--additional-gids").arg(g.to_string());
        }
        if let Some(c) = cwd.filter(|c| !c.is_empty()) {
            cmd.arg("--cwd").arg(c);
        }
        for e in env {
            cmd.arg("--env").arg(e);
        }
        let owner = self.stdio_owner(id).or_else(|| {
            let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(self.dir(id).join("bundle/config.json")).ok()?).ok()?;
            let host = |k: &str| cfg["linux"][k].as_array()?.iter().find(|m| m["containerID"].as_u64() == Some(0))?["hostID"].as_u64();
            Some(format!("{}:{}", host("uidMappings")?, host("gidMappings")?))
        });
        if let Some(o) = owner {
            cmd.arg("--stdio-owner").arg(o);
        }
        cmd.arg("--").args(argv);
        Ok(cmd)
    }

    /// uid/gid the container's process runs as (inside the container), from its bundle.
    pub fn process_user(&self, id: &str) -> Option<(u32, u32)> {
        let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(self.dir(id).join("bundle/config.json")).ok()?).ok()?;
        Some((cfg["process"]["user"]["uid"].as_u64()? as u32, cfg["process"]["user"]["gid"].as_u64()? as u32))
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.dir(id).join("log")
    }
}

/// Docker's `seccomp=unconfined` / `seccomp:unconfined` security option.
/// Replace a bundle's per-GPU device nodes (/dev/nvidiaN, /dev/dri/*) with those the CDI
/// registry gives for `gpus` now, in both the device list and the device cgroup rules.
fn refresh_gpu_devices(config: &Path, gpus: &[String]) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    let edits = og_runtime::cdi::Registry::load().resolve(gpus)?;
    let per_gpu = |p: &str| p.strip_prefix("/dev/nvidia").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) || p.starts_with("/dev/dri/");
    let mut cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(config)?)?;
    let old: Vec<(i64, i64)> = cfg["linux"]["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["path"].as_str().is_some_and(per_gpu))
        .filter_map(|d| Some((d["major"].as_i64()?, d["minor"].as_i64()?)))
        .collect();
    let mut new_devs = Vec::new();
    for host in edits.device_paths() {
        let h = host.to_string_lossy().into_owned();
        if !per_gpu(&h) {
            continue;
        }
        let md = std::fs::metadata(&host).with_context(|| format!("{h}"))?;
        if !md.file_type().is_char_device() {
            continue;
        }
        let (maj, min) = (libc::major(md.rdev()) as i64, libc::minor(md.rdev()) as i64);
        new_devs.push(serde_json::json!({"path": h, "type": "c", "major": maj, "minor": min, "fileMode": 0o666, "uid": 0, "gid": 0}));
    }
    if new_devs.is_empty() {
        return Ok(());
    }
    if let Some(devs) = cfg["linux"]["devices"].as_array_mut() {
        devs.retain(|d| !d["path"].as_str().is_some_and(per_gpu));
        devs.extend(new_devs.iter().cloned());
    }
    if let Some(rules) = cfg["linux"]["resources"]["devices"].as_array_mut() {
        rules.retain(|r| !(r["type"] == "c" && old.iter().any(|(ma, mi)| r["major"].as_i64() == Some(*ma) && r["minor"].as_i64() == Some(*mi))));
        for d in &new_devs {
            rules.push(serde_json::json!({"allow": true, "type": "c", "major": d["major"], "minor": d["minor"], "access": "rwm"}));
        }
    }
    let tmp = config.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&cfg)?)?;
    std::fs::rename(&tmp, config)?;
    Ok(())
}

pub fn is_seccomp_unconfined(opt: &str) -> bool {
    matches!(opt.trim(), "seccomp=unconfined" | "seccomp:unconfined")
}

/// A custom seccomp profile from `security_opt`: `seccomp=<file>` (relative to `base`) or the
/// profile's JSON itself (what the docker CLI sends). Unreadable or invalid = an error, never a
/// silent fallback to a different profile.
pub fn seccomp_profile(opts: &[String], base: &Path) -> Result<Option<serde_json::Value>> {
    for o in opts {
        let o = o.trim();
        let Some(v) = o.strip_prefix("seccomp=").or_else(|| o.strip_prefix("seccomp:")) else { continue };
        if v == "unconfined" || v == "builtin" {
            continue;
        }
        let text = if v.trim_start().starts_with('{') {
            v.to_string()
        } else {
            let p = base.join(v);
            std::fs::read_to_string(&p).with_context(|| format!("reading seccomp profile {}", p.display()))?
        };
        let json: serde_json::Value = serde_json::from_str(&text).context("seccomp profile isn't valid JSON")?;
        return Ok(Some(json));
    }
    Ok(None)
}

/// `/path[:opt,opt,...]` (compose `tmpfs:` / docker `--tmpfs`) -> (target, size, other options).
pub fn parse_tmpfs(spec: &str) -> Result<(PathBuf, Option<u64>, Vec<String>)> {
    let (target, opts) = spec.split_once(':').unwrap_or((spec, ""));
    let mut size = None;
    let mut rest = Vec::new();
    for o in opts.split(',').map(str::trim).filter(|o| !o.is_empty()) {
        match o.strip_prefix("size=") {
            Some(v) => size = Some(parse_size(v).with_context(|| format!("tmpfs {target}: size {v:?}"))?.max(0) as u64),
            None => rest.push(o.to_string()),
        }
    }
    if !target.starts_with('/') {
        bail!("tmpfs target {target:?} must be an absolute path");
    }
    Ok((PathBuf::from(target), size, rest))
}

/// `/host/path:/container/path[:ro|:rw]`
pub fn parse_bind(s: &str) -> Result<og_runtime::BindMount> {
    let parts: Vec<&str> = s.split(':').collect();
    let (src, dst, ro) = match parts.as_slice() {
        [a, b] => (*a, *b, false),
        [a, b, "ro"] => (*a, *b, true),
        [a, b, "rw"] => (*a, *b, false),
        _ => bail!("bad volume {s:?} (use /host:/container[:ro])"),
    };
    let source = std::fs::canonicalize(src).with_context(|| format!("volume source {src:?}"))?;
    if !dst.starts_with('/') {
        bail!("volume target {dst:?} must be absolute");
    }
    Ok(og_runtime::BindMount { source, target: PathBuf::from(dst), read_only: ro })
}

/// Paths that are currently mount points (from /proc/self/mountinfo).
/// Mount `dir/rootfs` as overlayfs: `lowers` (bottom first) under `dir/upper`.
fn mount_overlay(dir: &Path, lowers: &[PathBuf]) -> Result<()> {
    let (rootfs, upper, work) = (dir.join("rootfs"), dir.join("upper"), dir.join("work"));
    for d in [&rootfs, &upper, &work] {
        std::fs::create_dir_all(d)?;
    }
    let lower = lowers.iter().rev().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(":");
    let data = format!("lowerdir={lower},upperdir={},workdir={},index=off,xino=off", upper.display(), work.display());
    if data.len() > 4000 {
        bail!("too many layers for one overlay mount");
    }
    let c = |s: &str| std::ffi::CString::new(s).map_err(|_| anyhow::anyhow!("NUL in a path"));
    let (src, target, fstype, opts) = (c("overlay")?, c(&rootfs.to_string_lossy())?, c("overlay")?, c(&data)?);
    if unsafe { libc::mount(src.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, opts.as_ptr().cast()) } != 0 {
        bail!("overlay mount: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn umount(p: &Path) {
    if let Ok(c) = std::ffi::CString::new(p.to_string_lossy().as_bytes()) {
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    }
}

pub fn mounted_paths() -> std::collections::HashSet<PathBuf> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|p| PathBuf::from(p.replace("\\040", " ")))
        .collect()
}

pub fn parse_size(s: &str) -> Result<i64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = match s.chars().last() {
        Some('k') => (&s[..s.len() - 1], 1i64 << 10),
        Some('m') => (&s[..s.len() - 1], 1 << 20),
        Some('g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s.as_str(), 1),
    };
    let n: f64 = num.trim_end_matches('b').parse().with_context(|| format!("bad size {s:?}"))?;
    Ok((n * mult as f64) as i64)
}
