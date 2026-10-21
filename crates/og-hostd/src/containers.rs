//! Container lifecycle on one node: prepare rootfs + bundle, launch og-shim, track state.
//!
//! Layout under the state root:
//!   containers/<id>/meta.json     what was asked for (image, name, options)
//!   containers/<id>/rootfs/       private root filesystem (owned by the userns range)
//!   containers/<id>/bundle/       OCI bundle (config.json)
//!   containers/<id>/log           stdout + stderr
//!   containers/<id>/status.json   written by og-shim: running / exited / failed

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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

fn new_id() -> Result<String> {
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
    fn dir(&self, id: &str) -> PathBuf {
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

        // image config + layers -> private rootfs, ownership shifted into the userns range
        let manifest: og_oci::registry::Manifest = serde_json::from_slice(&std::fs::read(self.store.blob_path(manifest_digest)?)?)?;
        let image_cfg: ImageConfig = serde_json::from_slice(&std::fs::read(self.store.blob_path(&manifest.config.digest)?)?)?;
        let map = IdMap::default();
        let unpack_opts = unpack::UnpackOptions { preserve_owner: true, id_offset: map.host_start };
        for layer in &manifest.layers {
            let path = self.store.blob_path(&layer.digest)?;
            unpack::apply_layer(&rootfs, unpack::open_layer(&path)?, &unpack_opts).with_context(|| format!("layer {}", layer.digest))?;
        }
        std::os::unix::fs::lchown(&rootfs, Some(map.host_start), Some(map.host_start))?;

        let mut opts = RunOptions::new(name.clone(), rootfs.clone(), PathBuf::from(format!("/og-host/{name}")));
        opts.userns = Some(map);
        opts_in(&mut opts);

        // networking: namespace + address before the container exists
        if !ports.is_empty() && !matches!(net_choice, NetChoice::Network(_)) {
            bail!("ports can only be published on a network (not with none/host/container:)");
        }
        // own sandbox (user + network namespace) unless sharing another container's or the host's
        let sandbox = dir.join("sandbox");
        if matches!(net_choice, NetChoice::Network(_) | NetChoice::None) {
            self.create_sandbox(&id, &sandbox, map)?;
            opts.userns_path = Some(sandbox.join("user"));
        }
        let (attachment, network_mode, my_ip) = match &net_choice {
            NetChoice::Network(n) => {
                let a = self.net.attach(&id, n, &ports).with_context(|| format!("attaching to network {n:?}"))?;
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
                opts.userns_path = Some(self.dir(&o.id).join("sandbox/user"));
                (None, format!("container:{}", o.id), o.net.as_ref().and_then(|a| a.ip))
            }
        };
        if opts.userns.is_none() {
            // without a user namespace, files keep their image ownership
            let _ = std::fs::remove_dir_all(&rootfs);
            std::fs::create_dir_all(&rootfs)?;
            let plain = unpack::UnpackOptions { preserve_owner: true, id_offset: 0 };
            for layer in &manifest.layers {
                let path = self.store.blob_path(&layer.digest)?;
                unpack::apply_layer(&rootfs, unpack::open_layer(&path)?, &plain)?;
            }
        }

        // bind mounts into a user-namespaced container go through id-mapped staging mounts
        if let Some(userns) = opts.userns_path.clone() {
            let staging_dir = PathBuf::from("/run/og-host/idmap").join(&id);
            for (n, m) in opts.mounts.iter_mut().enumerate() {
                let staging = staging_dir.join(n.to_string());
                crate::idmap::map_mount(&m.source, &staging, &userns)?;
                m.source = staging;
            }
        }

        // per-container /etc/resolv.conf, /etc/hosts, /etc/hostname (bind-mounted read-only)
        let etc = dir.join("etc");
        std::fs::create_dir_all(&etc)?;
        std::fs::write(etc.join("resolv.conf"), og_net::container_resolv_conf())?;
        std::fs::write(etc.join("hosts"), og_net::container_hosts(&name, my_ip, &[]))?;
        std::fs::write(etc.join("hostname"), format!("{name}\n"))?;
        if !matches!(net_choice, NetChoice::Host) {
            for f in ["resolv.conf", "hosts", "hostname"] {
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

        let meta = Meta { id: id.clone(), name, image: image.to_string(), manifest_digest: manifest_digest.to_string(), created_at: now(), args, net: attachment, network_mode };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
        if let Err(e) = self.spawn_shim(&id) {
            let _ = self.net.detach(&id);
            self.destroy_sandbox(&id);
            return Err(e);
        }
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

    fn spawn_shim(&self, id: &str) -> Result<()> {
        let dir = self.dir(id);
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
        // wait briefly for the shim to report running/failed
        for _ in 0..100 {
            let st = self.status(id);
            match st.state.as_str() {
                "running" | "exited" => return Ok(()),
                "failed" => bail!("container failed to start: {}", st.error.unwrap_or_default()),
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        bail!("container did not report a state within 5s (see {})", dir.join("shim.log").display())
    }

    pub fn stop(&self, id: &str, timeout_s: u64) -> Result<Status> {
        let st = self.status(id);
        let Some(pid) = st.pid.filter(|_| st.state == "running") else { return Ok(st) };
        unsafe { libc::kill(pid, libc::SIGTERM) };
        for _ in 0..timeout_s * 10 {
            if self.status(id).state != "running" {
                return Ok(self.status(id));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        std::thread::sleep(std::time::Duration::from_millis(300));
        Ok(self.status(id))
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
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.dir(id).join("log")
    }
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
