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
    pub fn run(&self, image: &str, manifest_digest: &str, name: Option<&str>, opts_in: impl FnOnce(&mut RunOptions)) -> Result<Meta> {
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
        let spec = og_runtime::build_spec(&image_cfg, &opts)?;
        let args = spec.process().as_ref().and_then(|p| p.args().clone()).unwrap_or_default();
        og_runtime::write_bundle(&dir.join("bundle"), &spec)?;

        let meta = Meta { id: id.clone(), name, image: image.to_string(), manifest_digest: manifest_digest.to_string(), created_at: now(), args };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
        self.spawn_shim(&id)?;
        Ok(meta)
    }

    fn spawn_shim(&self, id: &str) -> Result<()> {
        let dir = self.dir(id);
        let shim = std::env::current_exe()?.with_file_name("og-shim");
        if !shim.exists() {
            bail!("og-shim not found next to og-hostd ({})", shim.display());
        }
        let mut cmd = Command::new(&shim);
        cmd.args(["--id", id, "--bundle"]).arg(dir.join("bundle"))
            .arg("--state-root").arg(&self.runtime_root)
            .arg("--log").arg(dir.join("log"))
            .arg("--status").arg(dir.join("status.json"))
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
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.dir(id).join("log")
    }
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
