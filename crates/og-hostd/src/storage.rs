//! Datastores: where backups go. A local directory, an NFS export or an SMB/CIFS share
//! (mounted on demand under /run/og-host/mnt/<id>); cluster nodes at other sites come later.
//! Config in `<root>/storage.json`; SMB passwords in `<root>/storage-secrets/<id>.cred` and
//! encryption keys in `<root>/backup-keys/<id>.key` (both 0600, never sent back in lists).
//! Removing a datastore only forgets it: its backups stay where they are.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Store {
    pub id: String,
    /// dir | nfs | cifs
    #[serde(rename = "type")]
    pub kind: String,
    /// dir: the directory
    pub path: String,
    /// nfs / cifs: host name or address
    pub server: String,
    /// nfs: the export path
    pub export: String,
    /// cifs: share name
    pub share: String,
    pub username: String,
    pub domain: String,
    /// extra mount options (nfs: e.g. vers=4.2)
    pub options: String,
    /// seal new backups with this datastore's key
    pub encrypt: bool,
    pub enabled: bool,
    pub comment: String,
    /// garbage collection schedule (see `schedule`), default weekly
    pub gc_schedule: String,
}

impl Default for Store {
    fn default() -> Self {
        Store { id: String::new(), kind: "dir".into(), path: String::new(), server: String::new(), export: String::new(), share: String::new(), username: String::new(), domain: String::new(), options: String::new(), encrypt: false, enabled: true, comment: String::new(), gc_schedule: "sun 03:30".into() }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    stores: Vec<Store>,
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !s.starts_with('-')
}

fn safe_text(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() || c == ',' || c == '\n')
}

pub struct Storage {
    root: PathBuf,
}

impl Storage {
    pub fn new(root: &Path) -> Storage {
        Storage { root: root.to_path_buf() }
    }

    fn file(&self) -> PathBuf {
        self.root.join("storage.json")
    }

    pub fn list(&self) -> Vec<Store> {
        let mut f: File = std::fs::read(self.file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if !f.stores.iter().any(|s| s.id == "local") {
            f.stores.insert(0, Store { id: "local".into(), path: self.root.join("backups").to_string_lossy().into(), comment: "This server's disk".into(), ..Default::default() });
        }
        f.stores
    }

    pub fn get(&self, id: &str) -> Result<Store> {
        self.list().into_iter().find(|s| s.id == id).with_context(|| format!("no datastore {id:?}"))
    }

    fn save(&self, stores: Vec<Store>) -> Result<()> {
        let tmp = self.file().with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&File { stores })?)?;
        std::fs::rename(tmp, self.file())?;
        Ok(())
    }

    pub fn upsert(&self, mut s: Store, password: Option<&str>) -> Result<()> {
        if !valid_id(&s.id) {
            bail!("datastore names are letters, digits, - and _ (up to 32)");
        }
        match s.kind.as_str() {
            "dir" => {
                if !s.path.starts_with('/') || s.path.contains("/../") || s.path == "/" {
                    bail!("give the directory as an absolute path");
                }
            }
            "nfs" => {
                if s.server.is_empty() || !s.export.starts_with('/') {
                    bail!("NFS needs a server and an export path like /srv/backups");
                }
            }
            "cifs" => {
                if s.server.is_empty() || s.share.is_empty() {
                    bail!("SMB needs a server and a share name");
                }
            }
            _ => bail!("type is dir, nfs or cifs"),
        }
        for v in [&s.server, &s.export, &s.share, &s.username, &s.domain, &s.path] {
            if !safe_text(v) {
                bail!("names and paths can't contain commas or control characters");
            }
        }
        if s.options.chars().any(|c| c.is_control() || c.is_whitespace()) {
            bail!("mount options are comma-separated, without spaces");
        }
        if s.options.split(',').any(|o| ["credentials", "password", "pass", "user", "username"].contains(&o.split('=').next().unwrap_or(""))) {
            bail!("put credentials in the user/password fields, not in the options");
        }
        if !s.gc_schedule.is_empty() {
            crate::schedule::Schedule::parse(&s.gc_schedule)?;
        }
        s.comment = s.comment.chars().take(200).collect();
        let mut all = self.list();
        let was = all.iter().position(|x| x.id == s.id);
        if let Some(pw) = password.filter(|p| !p.is_empty()) {
            if s.kind != "cifs" {
                bail!("only SMB shares take a password");
            }
            write_secret(&self.root.join("storage-secrets").join(format!("{}.cred", s.id)), &format!("username={}\npassword={pw}\n{}", s.username, if s.domain.is_empty() { String::new() } else { format!("domain={}\n", s.domain) }))?;
        }
        if s.encrypt && self.key(&s.id).is_none() {
            let k = og_backup::Key::generate();
            write_secret(&self.key_path(&s.id), &hex::encode(k))?;
        }
        match was {
            Some(i) => all[i] = s,
            None => all.push(s),
        }
        self.save(all)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        if id == "local" {
            bail!("the local datastore can't be removed");
        }
        let s = self.get(id)?;
        if s.kind != "dir" {
            let _ = unmount(&self.mountpoint(id));
        }
        let all: Vec<Store> = self.list().into_iter().filter(|x| x.id != id).collect();
        let _ = std::fs::remove_file(self.root.join("storage-secrets").join(format!("{id}.cred")));
        // the key stays (renamed), so backups left on the share can still be read
        let _ = std::fs::rename(self.key_path(id), self.key_path(id).with_extension(format!("key.removed-{}", crate::containers::unix_now())));
        self.save(all)
    }

    fn key_path(&self, id: &str) -> PathBuf {
        self.root.join("backup-keys").join(format!("{id}.key"))
    }

    pub fn key(&self, id: &str) -> Option<[u8; 32]> {
        og_backup::Key::parse(&std::fs::read_to_string(self.key_path(id)).ok()?).ok()
    }

    /// Put in a key (e.g. the one from another site, to read its backups here).
    pub fn set_key(&self, id: &str, hex_key: &str) -> Result<String> {
        self.get(id)?;
        let k = og_backup::Key::parse(hex_key)?;
        write_secret(&self.key_path(id), &hex::encode(k))?;
        Ok(og_backup::Key::new(&k).fingerprint)
    }

    fn mountpoint(&self, id: &str) -> PathBuf {
        PathBuf::from("/run/og-host/mnt").join(id)
    }

    /// The datastore's directory, mounting it first if needed.
    pub fn path(&self, s: &Store) -> Result<PathBuf> {
        if !s.enabled {
            bail!("datastore {} is turned off", s.id);
        }
        match s.kind.as_str() {
            "dir" => {
                std::fs::create_dir_all(&s.path).with_context(|| format!("creating {}", s.path))?;
                Ok(PathBuf::from(&s.path))
            }
            _ => {
                let mp = self.mountpoint(&s.id);
                if !is_mounted(&mp) {
                    std::fs::create_dir_all(&mp)?;
                    let mut c = Command::new("mount");
                    if s.kind == "nfs" {
                        let opts = if s.options.is_empty() { "soft,timeo=150,retrans=3".to_string() } else { format!("soft,{}", s.options) };
                        c.args(["-t", "nfs", "-o", &opts, &format!("{}:{}", s.server, s.export)]);
                    } else {
                        let cred = self.root.join("storage-secrets").join(format!("{}.cred", s.id));
                        let mut opts = format!("vers=3.0,uid=0,gid=0,file_mode=0600,dir_mode=0700,noperm");
                        if cred.exists() {
                            opts.push_str(&format!(",credentials={}", cred.display()));
                        } else {
                            opts.push_str(",guest");
                        }
                        if !s.options.is_empty() {
                            opts.push(',');
                            opts.push_str(&s.options);
                        }
                        c.args(["-t", "cifs", "-o", &opts, &format!("//{}/{}", s.server, s.share)]);
                    }
                    c.arg(&mp);
                    let out = c.output().context("running mount")?;
                    if !out.status.success() {
                        bail!("mounting {} failed: {}", s.id, String::from_utf8_lossy(&out.stderr).trim());
                    }
                }
                Ok(mp)
            }
        }
    }

    /// Open the backup repository of a datastore.
    pub fn repo(&self, id: &str) -> Result<(Store, og_backup::Repo)> {
        let s = self.get(id)?;
        let dir = self.path(&s)?.join("og-backup");
        let key = self.key(id);
        if s.encrypt && key.is_none() {
            bail!("datastore {id} encrypts backups but its key is missing (put it in under Datastores)");
        }
        // a key we have also reads sealed backups made while the store was encrypting
        let repo = og_backup::Repo::open_with(&dir, key.as_ref(), s.encrypt)?;
        Ok((s, repo))
    }

    /// Space on the datastore's file system (total, free), if reachable now.
    pub fn space(&self, s: &Store) -> Option<(u64, u64)> {
        let p = match s.kind.as_str() {
            "dir" => PathBuf::from(&s.path),
            _ => {
                let mp = self.mountpoint(&s.id);
                if !is_mounted(&mp) {
                    return None;
                }
                mp
            }
        };
        let p = if p.exists() { p } else { p.parent()?.to_path_buf() };
        let c = std::ffi::CString::new(p.to_string_lossy().as_bytes()).ok()?;
        let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut v) } != 0 {
            return None;
        }
        Some((v.f_blocks as u64 * v.f_frsize as u64, v.f_bavail as u64 * v.f_frsize as u64))
    }

    pub fn mounted(&self, s: &Store) -> bool {
        s.kind == "dir" || is_mounted(&self.mountpoint(&s.id))
    }
}

fn write_secret(p: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p.parent().unwrap())?;
    let tmp = p.with_extension("tmp");
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?.write_all(text.as_bytes())?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

fn is_mounted(p: &Path) -> bool {
    let target = p.to_string_lossy();
    std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default().lines().any(|l| l.split(' ').nth(4) == Some(&*target))
}

fn unmount(p: &Path) -> Result<()> {
    if is_mounted(p) {
        let st = Command::new("umount").arg(p).status()?;
        if !st.success() {
            bail!("umount {} failed", p.display());
        }
    }
    Ok(())
}
