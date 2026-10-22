//! Storage pools for VM disks (and images): where volumes live and how they're made, grown,
//! cloned and removed. A volume is named `<pool>:<name>`.
//!
//! - `dir`: qcow2 files in a directory (default pool `local`, `<root>/images`); linked clones
//!   are qcow2 overlays on a template's (read-only) disk.
//! - `lvmthin`: raw logical volumes in an LVM thin pool; linked clones are thin snapshots.
//! - `zfs`: zvols under a dataset; linked clones are ZFS clones of a snapshot.
//!
//! Config in `<root>/pools.json`. Removing a pool only forgets it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pool {
    pub id: String,
    /// dir | lvmthin | zfs
    #[serde(rename = "type")]
    pub kind: String,
    /// dir: directory
    pub path: String,
    /// lvmthin: volume group and thin pool
    pub vg: String,
    pub thinpool: String,
    /// zfs: parent dataset, e.g. rpool/data
    pub dataset: String,
    pub enabled: bool,
    pub comment: String,
}

impl Default for Pool {
    fn default() -> Self {
        Pool { id: String::new(), kind: "dir".into(), path: String::new(), vg: String::new(), thinpool: String::new(), dataset: String::new(), enabled: true, comment: String::new() }
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("running {cmd} (is it installed?)"))?;
    if !out.status.success() {
        bail!("{cmd} {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn valid_vol(n: &str) -> bool {
    !n.is_empty() && n.len() <= 100 && n.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) && !n.starts_with(['.', '-'])
}

fn safe_name(s: &str) -> bool {
    !s.is_empty() && s.len() < 200 && s.chars().all(|c| c.is_ascii_alphanumeric() || "_.-/".contains(c)) && !s.contains("..") && !s.starts_with(['-', '/'])
}

pub struct Pools {
    root: PathBuf,
}

impl Pools {
    pub fn new(root: &Path) -> Pools {
        Pools { root: root.to_path_buf() }
    }

    fn file(&self) -> PathBuf {
        self.root.join("pools.json")
    }

    pub fn list(&self) -> Vec<Pool> {
        let mut v: Vec<Pool> = std::fs::read(self.file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if !v.iter().any(|p| p.id == "local") {
            v.insert(0, Pool { id: "local".into(), path: self.root.join("images").to_string_lossy().into(), comment: "Disk images on this server".into(), ..Default::default() });
        }
        v
    }

    pub fn get(&self, id: &str) -> Result<Pool> {
        self.list().into_iter().find(|p| p.id == id).with_context(|| format!("no storage pool {id:?}"))
    }

    fn save(&self, v: &[Pool]) -> Result<()> {
        let tmp = self.file().with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
        std::fs::rename(tmp, self.file())?;
        Ok(())
    }

    pub fn upsert(&self, mut p: Pool) -> Result<()> {
        if !crate::storage::valid_id(&p.id) {
            bail!("pool names are letters, digits, - and _");
        }
        match p.kind.as_str() {
            "dir" => {
                if !p.path.starts_with('/') || p.path == "/" || p.path.contains("..") {
                    bail!("give an absolute directory");
                }
                std::fs::create_dir_all(&p.path)?;
            }
            "lvmthin" => {
                if !safe_name(&p.vg) || !safe_name(&p.thinpool) || p.vg.contains('/') || p.thinpool.contains('/') {
                    bail!("give the volume group and its thin pool");
                }
                run("lvs", &["--noheadings", &format!("{}/{}", p.vg, p.thinpool)]).context("that thin pool doesn't exist")?;
            }
            "zfs" => {
                if !safe_name(&p.dataset) {
                    bail!("give the parent dataset, e.g. rpool/data");
                }
                run("zfs", &["list", "-H", "-o", "name", &p.dataset]).context("that dataset doesn't exist")?;
            }
            _ => bail!("type is dir, lvmthin or zfs"),
        }
        p.comment = p.comment.chars().take(200).collect();
        let mut all = self.list();
        match all.iter_mut().find(|x| x.id == p.id) {
            Some(x) => *x = p,
            None => all.push(p),
        }
        self.save(&all)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        if id == "local" {
            bail!("the local pool can't be removed");
        }
        let all: Vec<Pool> = self.list().into_iter().filter(|p| p.id != id).collect();
        self.save(&all)
    }

    /// `pool:name` -> (pool, name)
    pub fn split(&self, vol: &str) -> Result<(Pool, String)> {
        let (p, n) = vol.split_once(':').context("volumes look like pool:name")?;
        if !valid_vol(n) {
            bail!("bad volume name {n:?}");
        }
        Ok((self.get(p)?, n.to_string()))
    }

    /// Path QEMU opens (file or block device).
    pub fn path(&self, vol: &str) -> Result<PathBuf> {
        let (p, n) = self.split(vol)?;
        Ok(match p.kind.as_str() {
            "dir" => Path::new(&p.path).join(if n.contains('.') { n } else { format!("{n}.qcow2") }),
            "lvmthin" => {
                let dev = PathBuf::from(format!("/dev/{}/{n}", p.vg));
                // thin snapshots (linked clones) are marked "skip activation", so they're
                // inactive after a reboot: bring a volume up when it's asked for
                if !dev.exists() {
                    let _ = std::process::Command::new("lvchange").args(["-q", "-ay", "-K", &format!("{}/{n}", p.vg)]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
                }
                dev
            }
            _ => PathBuf::from(format!("/dev/zvol/{}/{n}", p.dataset)),
        })
    }

    pub fn exists(&self, vol: &str) -> bool {
        self.path(vol).is_ok_and(|p| p.exists())
    }

    /// New empty volume of `gib`.
    pub fn create(&self, vol: &str, gib: u64) -> Result<PathBuf> {
        if gib == 0 || gib > 64 * 1024 {
            bail!("size 1 GiB to 64 TiB");
        }
        let (p, n) = self.split(vol)?;
        let path = self.path(vol)?;
        if path.exists() {
            bail!("volume {vol} exists already");
        }
        match p.kind.as_str() {
            "dir" => {
                std::fs::create_dir_all(&p.path)?;
                run("qemu-img", &["create", "-q", "-f", "qcow2", "-o", "lazy_refcounts=on,extended_l2=on,cluster_size=128k", &path.to_string_lossy(), &format!("{gib}G")])?;
            }
            "lvmthin" => {
                run("lvcreate", &["-q", "-V", &format!("{gib}G"), "-T", &format!("{}/{}", p.vg, p.thinpool), "-n", &n])?;
            }
            _ => {
                run("zfs", &["create", "-s", "-o", "volblocksize=16k", "-V", &format!("{gib}G"), &format!("{}/{n}", p.dataset)])?;
                wait_dev(&path)?;
            }
        }
        Ok(path)
    }

    pub fn resize(&self, vol: &str, gib: u64) -> Result<()> {
        let (p, n) = self.split(vol)?;
        match p.kind.as_str() {
            "dir" => run("qemu-img", &["resize", "-q", &self.path(vol)?.to_string_lossy(), &format!("{gib}G")]).map(|_| ()),
            "lvmthin" => run("lvresize", &["-q", "-L", &format!("{gib}G"), &format!("{}/{n}", p.vg)]).map(|_| ()),
            _ => run("zfs", &["set", &format!("volsize={gib}G"), &format!("{}/{n}", p.dataset)]).map(|_| ()),
        }
    }

    pub fn delete(&self, vol: &str) -> Result<()> {
        let (p, n) = self.split(vol)?;
        match p.kind.as_str() {
            "dir" => {
                let f = self.path(vol)?;
                if f.exists() {
                    std::fs::remove_file(f)?;
                }
                Ok(())
            }
            "lvmthin" => run("lvremove", &["-q", "-y", &format!("{}/{n}", p.vg)]).map(|_| ()),
            _ => run("zfs", &["destroy", "-r", &format!("{}/{n}", p.dataset)]).map(|_| ()),
        }
    }

    /// Copy (`linked` = copy-on-write from `src`, which must then stay unchanged: a template).
    pub fn clone_vol(&self, src: &str, dst: &str, linked: bool) -> Result<PathBuf> {
        let (sp, sn) = self.split(src)?;
        let (dp, dn) = self.split(dst)?;
        let spath = self.path(src)?;
        let dpath = self.path(dst)?;
        if dpath.exists() {
            bail!("volume {dst} exists already");
        }
        if linked && sp.id == dp.id {
            match sp.kind.as_str() {
                "dir" => {
                    run("qemu-img", &["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", &spath.to_string_lossy(), &dpath.to_string_lossy()])?;
                }
                "lvmthin" => {
                    run("lvcreate", &["-q", "-s", "-n", &dn, &format!("{}/{sn}", sp.vg)])?;
                    run("lvchange", &["-q", "-ay", "-K", &format!("{}/{dn}", sp.vg)])?;
                }
                _ => {
                    let snap = format!("{}/{sn}@og-base", sp.dataset);
                    if run("zfs", &["list", "-H", "-t", "snapshot", &snap]).is_err() {
                        run("zfs", &["snapshot", &snap])?;
                    }
                    run("zfs", &["clone", &snap, &format!("{}/{dn}", dp.dataset)])?;
                    wait_dev(&dpath)?;
                }
            }
            return Ok(dpath);
        }
        // full copy (any pool to any pool)
        let gib = self.size(src)?.div_ceil(1 << 30).max(1);
        if dp.kind != "dir" {
            self.create(dst, gib)?;
        }
        let out_fmt = if dp.kind == "dir" { "qcow2" } else { "raw" };
        // parallel, out-of-order writes; no page cache on either side (a copy the size of the
        // disk would only push everything else out of it)
        let mut args = vec!["convert", "-q", "-W", "-m", "16", "-T", "none", "-O", out_fmt];
        if dp.kind != "dir" {
            // target exists and is new (reads as zeros): don't write the zeros
            args.extend(["-n", "--target-is-zero", "-t", "none"]);
        }
        let s = spath.to_string_lossy().into_owned();
        let d = dpath.to_string_lossy().into_owned();
        args.push(&s);
        args.push(&d);
        if let Err(e) = run("qemu-img", &args) {
            let _ = self.delete(dst);
            return Err(e);
        }
        let _ = dn;
        Ok(dpath)
    }

    /// Rename a volume within its pool.
    pub fn rename(&self, vol: &str, new_name: &str) -> Result<String> {
        let (p, n) = self.split(vol)?;
        if !valid_vol(new_name) {
            bail!("bad volume name {new_name:?}");
        }
        let dst = format!("{}:{new_name}", p.id);
        if self.exists(&dst) {
            bail!("volume {dst} exists already");
        }
        match p.kind.as_str() {
            "dir" => std::fs::rename(self.path(vol)?, self.path(&dst)?)?,
            "lvmthin" => {
                run("lvrename", &[&p.vg, &n, new_name])?;
            }
            _ => {
                run("zfs", &["rename", &format!("{}/{n}", p.dataset), &format!("{}/{new_name}", p.dataset)])?;
            }
        }
        Ok(dst)
    }

    /// Virtual size in bytes.
    pub fn size(&self, vol: &str) -> Result<u64> {
        let out = run("qemu-img", &["info", "-U", "--output=json", &self.path(vol)?.to_string_lossy()])?;
        let v: Value = serde_json::from_str(&out)?;
        v["virtual-size"].as_u64().context("no size")
    }

    /// Turn any disk image (qcow2, raw, vmdk, vdi, vhdx…) into a volume.
    pub fn import(&self, src: &Path, dst: &str, grow_gib: Option<u64>) -> Result<PathBuf> {
        let (dp, _) = self.split(dst)?;
        let dpath = self.path(dst)?;
        if dpath.exists() {
            bail!("volume {dst} exists already");
        }
        let info: Value = serde_json::from_str(&run("qemu-img", &["info", "-U", "--output=json", &src.to_string_lossy()])?)?;
        let vsize = info["virtual-size"].as_u64().unwrap_or(0);
        let gib = vsize.div_ceil(1 << 30).max(grow_gib.unwrap_or(0)).max(1);
        if dp.kind != "dir" {
            self.create(dst, gib)?;
            run("qemu-img", &["convert", "-q", "-n", "--target-is-zero", "-O", "raw", &src.to_string_lossy(), &dpath.to_string_lossy()])?;
        } else {
            std::fs::create_dir_all(&dp.path)?;
            run("qemu-img", &["convert", "-q", "-O", "qcow2", &src.to_string_lossy(), &dpath.to_string_lossy()])?;
            if gib << 30 > vsize {
                run("qemu-img", &["resize", "-q", &dpath.to_string_lossy(), &format!("{gib}G")])?;
            }
        }
        Ok(dpath)
    }

    /// Pool usage: (total, used) bytes, if known.
    pub fn usage(&self, p: &Pool) -> Option<(u64, u64)> {
        match p.kind.as_str() {
            "dir" => {
                let c = std::ffi::CString::new(p.path.as_bytes()).ok()?;
                let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
                (unsafe { libc::statvfs(c.as_ptr(), &mut v) } == 0).then(|| {
                    let t = v.f_blocks as u64 * v.f_frsize as u64;
                    (t, t - v.f_bavail as u64 * v.f_frsize as u64)
                })
            }
            "lvmthin" => {
                let o = run("lvs", &["--noheadings", "--units", "b", "--nosuffix", "-o", "lv_size,data_percent", &format!("{}/{}", p.vg, p.thinpool)]).ok()?;
                let f: Vec<&str> = o.split_whitespace().collect();
                let size: f64 = f.first()?.parse().ok()?;
                let pct: f64 = f.get(1)?.replace(',', ".").parse().ok()?;
                Some((size as u64, (size * pct / 100.0) as u64))
            }
            _ => {
                let o = run("zfs", &["list", "-H", "-p", "-o", "used,avail", &p.dataset]).ok()?;
                let f: Vec<u64> = o.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                Some((f.first()? + f.get(1)?, *f.first()?))
            }
        }
    }

    /// Volumes in a pool.
    pub fn volumes(&self, p: &Pool) -> Vec<Value> {
        match p.kind.as_str() {
            "dir" => std::fs::read_dir(&p.path)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let m = e.metadata().ok()?;
                    (m.is_file() && valid_vol(&n)).then(|| json!({"volume": format!("{}:{n}", p.id), "allocated": std::os::unix::fs::MetadataExt::blocks(&m) * 512}))
                })
                .collect(),
            "lvmthin" => run("lvs", &["--noheadings", "--units", "b", "--nosuffix", "-o", "lv_name,lv_size,pool_lv", &p.vg])
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split_whitespace().collect();
                    (f.len() >= 3 && f[2] == p.thinpool).then(|| json!({"volume": format!("{}:{}", p.id, f[0]), "size": f[1].parse::<u64>().unwrap_or(0)}))
                })
                .collect(),
            _ => run("zfs", &["list", "-H", "-p", "-t", "volume", "-o", "name,volsize,used", "-r", &p.dataset])
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split('\t').collect();
                    let n = f.first()?.strip_prefix(&format!("{}/", p.dataset))?;
                    Some(json!({"volume": format!("{}:{n}", p.id), "size": f.get(1)?.parse::<u64>().unwrap_or(0), "allocated": f.get(2)?.parse::<u64>().unwrap_or(0)}))
                })
                .collect(),
        }
    }
}

fn wait_dev(p: &Path) -> Result<()> {
    for _ in 0..50 {
        if p.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("{} didn't appear", p.display())
}
