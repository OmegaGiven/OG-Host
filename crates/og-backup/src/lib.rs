//! OG-Host's backup store: deduplicating, compressed, optionally encrypted (like Proxmox
//! Backup Server's datastores, smaller).
//!
//! A repository is a directory (local disk, NFS/SMB mount, or later another node):
//!
//! ```text
//! chunks/ab/abcdef…          one chunk: "OGC1", flags, [nonce], zstd data (encrypted if keyed)
//! snapshots/<group>/<time>/  manifest.json + <archive>.idx (chunk list)
//! .lock                      backups share it, garbage collection takes it alone
//! ```
//!
//! An archive is a tar stream (ownership, modes, symlinks, devices kept) cut into chunks at
//! content-defined boundaries ([`chunker`]), so a second backup of mostly-unchanged data
//! stores only the chunks that changed. A chunk's id is the SHA-256 of its data, or, with a
//! key, an HMAC of it (ids don't reveal content); with a key chunks are sealed with
//! XChaCha20-Poly1305. Snapshot groups: `ct/<name>`, `stack/<name>`, `vm/<name>`.

pub mod chunker;
pub mod disk;
pub mod prune;

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Id = [u8; 32];

const CHUNK_MAGIC: &[u8; 4] = b"OGC1";
const INDEX_MAGIC: &[u8; 4] = b"OGI1";
const F_ZSTD: u8 = 1;
const F_SEALED: u8 = 2;

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    b
}

/// An encryption key (32 bytes). Keep a copy: without it encrypted backups can't be read.
#[derive(Clone)]
pub struct Key {
    seal: [u8; 32],
    ids: [u8; 32],
    pub fingerprint: String,
}

impl Key {
    pub fn generate() -> [u8; 32] {
        random()
    }

    pub fn new(master: &[u8; 32]) -> Key {
        let derive = |label: &[u8]| -> [u8; 32] {
            let mut m = <Hmac<Sha256> as Mac>::new_from_slice(master).unwrap();
            m.update(label);
            m.finalize().into_bytes().into()
        };
        let fp = derive(b"og-backup fingerprint");
        Key { seal: derive(b"og-backup seal"), ids: derive(b"og-backup ids"), fingerprint: hex::encode(&fp[..8]).as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(":") }
    }

    /// Hex text of a key file.
    pub fn parse(text: &str) -> Result<[u8; 32]> {
        let b = hex::decode(text.trim()).context("a backup key is 64 hex characters")?;
        b.try_into().map_err(|_| anyhow::anyhow!("a backup key is 64 hex characters"))
    }
}

pub fn id_hex(id: &Id) -> String {
    hex::encode(id)
}

#[derive(Clone)]
pub struct Repo {
    pub dir: PathBuf,
    key: Option<Key>,
    /// seal new chunks (else the key only reads sealed ones)
    seal: bool,
    level: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArchiveInfo {
    pub name: String,
    /// bytes in the stream
    pub size: u64,
    pub chunks: u64,
    /// sha256 of the whole stream
    pub csum: String,
    /// chunks / bytes this archive added to the store (after compression)
    #[serde(default)]
    pub new_chunks: u64,
    #[serde(default)]
    pub new_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub group: String,
    /// unix seconds; with the group, the snapshot's id
    pub time: u64,
    pub host: String,
    pub archives: Vec<ArchiveInfo>,
    /// what was backed up (container metadata, compose file path, …)
    pub meta: serde_json::Value,
    pub notes: String,
    pub protected: bool,
    /// key fingerprint when encrypted
    pub encrypted: Option<String>,
    pub duration_ms: u64,
    pub verified: Option<Verified>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Verified {
    pub time: u64,
    pub ok: bool,
    pub error: String,
}

impl Manifest {
    pub fn id(&self) -> String {
        format!("{}/{}", self.group, time_name(self.time))
    }
    pub fn size(&self) -> u64 {
        self.archives.iter().map(|a| a.size).sum()
    }
}

/// `2026-10-21T22:00:05Z` (UTC), the snapshot directory name.
pub fn time_name(t: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(t as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn parse_time_name(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[19] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60 + se) as u64)
}

pub(crate) fn civil(t: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d, secs / 3600, secs % 3600 / 60, secs % 60)
}

pub fn valid_group(g: &str) -> bool {
    let Some((kind, name)) = g.split_once('/') else { return false };
    ["ct", "stack", "vm", "host"].contains(&kind) && !name.is_empty() && name.len() <= 128 && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) && !name.starts_with('.')
}

fn valid_archive(n: &str) -> bool {
    !n.is_empty() && n.len() <= 100 && n.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) && !n.starts_with('.')
}

/// A held lock on the repository (shared for backups, exclusive for GC).
pub struct Lock(#[allow(dead_code)] std::fs::File);

impl Repo {
    /// Open (creating the layout) a repository; `key` = encrypt new chunks and read sealed ones.
    pub fn open(dir: &Path, key: Option<&[u8; 32]>) -> Result<Repo> {
        Self::open_with(dir, key, true)
    }

    /// `seal`: encrypt new chunks with the key; without it the key only reads sealed chunks.
    pub fn open_with(dir: &Path, key: Option<&[u8; 32]>, seal: bool) -> Result<Repo> {
        std::fs::create_dir_all(dir.join("chunks")).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::create_dir_all(dir.join("snapshots"))?;
        Ok(Repo { dir: dir.to_path_buf(), key: key.map(Key::new), seal: seal && key.is_some(), level: 3 })
    }

    /// Fingerprint of the key new backups are sealed with.
    pub fn key_fingerprint(&self) -> Option<&str> {
        self.key.as_ref().filter(|_| self.seal).map(|k| k.fingerprint.as_str())
    }

    pub fn lock(&self, exclusive: bool) -> Result<Lock> {
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.dir.join(".lock"))?;
        use std::os::fd::AsRawFd;
        let op = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH } | libc::LOCK_NB;
        if unsafe { libc::flock(f.as_raw_fd(), op) } != 0 {
            bail!(if exclusive { "a backup is running on this datastore; try again when it's done" } else { "garbage collection is running on this datastore" });
        }
        Ok(Lock(f))
    }

    fn chunk_path(&self, id: &Id) -> PathBuf {
        let h = id_hex(id);
        self.dir.join("chunks").join(&h[..2]).join(h)
    }

    fn id_of(&self, data: &[u8], sealed: bool) -> Id {
        match self.key.as_ref().filter(|_| sealed) {
            Some(k) => {
                let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&k.ids).unwrap();
                m.update(data);
                m.finalize().into_bytes().into()
            }
            None => Sha256::digest(data).into(),
        }
    }

    /// Store a chunk (or just refresh it if present). Returns its id and the bytes written
    /// (0 when it was already there).
    pub fn put(&self, data: &[u8]) -> Result<(Id, u64)> {
        let id = self.id_of(data, self.seal);
        let p = self.chunk_path(&id);
        if p.exists() {
            // fresh mtime: garbage collection's grace period sees it as in use
            let _ = std::fs::File::options().append(true).open(&p).and_then(|f| f.set_modified(std::time::SystemTime::now()));
            return Ok((id, 0));
        }
        let z = zstd::bulk::compress(data, self.level)?;
        let (flags, body) = if z.len() < data.len() { (F_ZSTD, z) } else { (0, data.to_vec()) };
        let mut out = Vec::with_capacity(body.len() + 48);
        out.extend_from_slice(CHUNK_MAGIC);
        match self.key.as_ref().filter(|_| self.seal) {
            Some(k) => {
                out.push(flags | F_SEALED);
                let nonce: [u8; 24] = random();
                out.extend_from_slice(&nonce);
                let sealed = XChaCha20Poly1305::new((&k.seal).into()).encrypt(XNonce::from_slice(&nonce), Payload { msg: &body, aad: &id }).map_err(|_| anyhow::anyhow!("sealing failed"))?;
                out.extend_from_slice(&sealed);
            }
            None => {
                out.push(flags);
                out.extend_from_slice(&body);
            }
        }
        std::fs::create_dir_all(p.parent().unwrap())?;
        // a name of its own: worker threads may store the same chunk at the same moment
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = p.with_extension(format!("tmp{}-{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, &p)?;
        Ok((id, out.len() as u64))
    }

    pub fn get(&self, id: &Id) -> Result<Vec<u8>> {
        self.get_flagged(id).map(|(d, _)| d)
    }

    /// A chunk's data, checked against its id, and whether it was sealed (encrypted).
    pub fn get_flagged(&self, id: &Id) -> Result<(Vec<u8>, bool)> {
        let raw = std::fs::read(self.chunk_path(id)).with_context(|| format!("chunk {} is missing", id_hex(id)))?;
        if raw.len() < 5 || &raw[..4] != CHUNK_MAGIC {
            bail!("chunk {} is damaged", id_hex(id));
        }
        let flags = raw[4];
        let body = if flags & F_SEALED != 0 {
            let k = self.key.as_ref().context("this backup is encrypted: the datastore's key is needed")?;
            if raw.len() < 5 + 24 {
                bail!("chunk {} is damaged", id_hex(id));
            }
            XChaCha20Poly1305::new((&k.seal).into()).decrypt(XNonce::from_slice(&raw[5..29]), Payload { msg: &raw[29..], aad: id }).map_err(|_| anyhow::anyhow!("chunk {} can't be decrypted (wrong key or damaged)", id_hex(id)))?
        } else {
            raw[5..].to_vec()
        };
        let data = if flags & F_ZSTD != 0 { zstd::bulk::decompress(&body, chunker::MAX + 1024)? } else { body };
        if self.id_of(&data, flags & F_SEALED != 0) != *id {
            bail!("chunk {} is damaged (checksum mismatch)", id_hex(id));
        }
        Ok((data, flags & F_SEALED != 0))
    }

    // ---- raw access, for copying backups between datastores (other sites) ----

    pub fn has_chunk(&self, id: &Id) -> bool {
        self.chunk_path(id).exists()
    }

    /// A chunk file as stored (compressed, sealed if encrypted).
    pub fn raw_chunk(&self, id: &Id) -> Result<Vec<u8>> {
        std::fs::read(self.chunk_path(id)).with_context(|| format!("chunk {} is missing", id_hex(id)))
    }

    /// Store a chunk file copied from another datastore (it can't always be checked here:
    /// sealed chunks need the key; `verify` checks them where the key is).
    pub fn put_raw(&self, id: &Id, raw: &[u8]) -> Result<()> {
        if raw.len() < 5 || &raw[..4] != CHUNK_MAGIC || raw.len() > chunker::MAX + 4096 {
            bail!("not a chunk");
        }
        let p = self.chunk_path(id);
        if p.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension(format!("tmp{}-{}", std::process::id(), random::<8>().iter().map(|b| format!("{b:02x}")).collect::<String>()));
        std::fs::write(&tmp, raw)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    /// Every chunk id a snapshot uses.
    pub fn chunk_ids(&self, m: &Manifest) -> Result<Vec<Id>> {
        let mut out = vec![];
        for a in &m.archives {
            out.extend(self.index(m, &a.name)?.into_iter().map(|(id, _)| id));
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// A snapshot's index files, as bytes (to copy it elsewhere).
    pub fn index_files(&self, m: &Manifest) -> Result<Vec<(String, Vec<u8>)>> {
        m.archives.iter().map(|a| Ok((a.name.clone(), std::fs::read(self.snap_dir(&m.group, m.time).join(format!("{}.idx", a.name)))?))).collect()
    }

    pub fn has_snapshot(&self, group: &str, time: u64) -> bool {
        self.snap_dir(group, time).join("manifest.json").exists()
    }

    /// Add a snapshot copied from another datastore; all its chunks must be here already.
    pub fn import_snapshot(&self, m: &Manifest, indexes: &[(String, Vec<u8>)]) -> Result<()> {
        if !valid_group(&m.group) || m.archives.iter().any(|a| !valid_archive(&a.name)) {
            bail!("bad snapshot");
        }
        let _lock = self.lock(false)?;
        let fin = self.snap_dir(&m.group, m.time);
        if fin.join("manifest.json").exists() {
            return Ok(());
        }
        let tmp = fin.with_file_name(format!(".tmp-{}", time_name(m.time)));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let r = (|| -> Result<()> {
            for a in &m.archives {
                let (_, b) = indexes.iter().find(|(n, _)| n == &a.name).with_context(|| format!("index {} missing", a.name))?;
                if b.len() < 12 || &b[..4] != INDEX_MAGIC {
                    bail!("index {} is damaged", a.name);
                }
                for e in b[12..].chunks_exact(36) {
                    let id: Id = e[..32].try_into().unwrap();
                    if !self.has_chunk(&id) {
                        bail!("chunk {} hasn't arrived", id_hex(&id));
                    }
                }
                std::fs::write(tmp.join(format!("{}.idx", a.name)), b)?;
            }
            let mut m = m.clone();
            m.verified = None;
            std::fs::write(tmp.join("manifest.json"), serde_json::to_vec_pretty(&m)?)?;
            std::fs::rename(&tmp, &fin)?;
            Ok(())
        })();
        if r.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        r
    }

    // ---- snapshots ----

    fn snap_dir(&self, group: &str, time: u64) -> PathBuf {
        self.dir.join("snapshots").join(group).join(time_name(time))
    }

    /// Start a snapshot; archives are added to it, then [`SnapshotWriter::commit`].
    pub fn begin(&self, group: &str, time: u64) -> Result<SnapshotWriter<'_>> {
        if !valid_group(group) {
            bail!("bad backup group {group:?}");
        }
        let lock = self.lock(false)?;
        let fin = self.snap_dir(group, time);
        if fin.exists() {
            bail!("a backup of {group} at {} exists already", time_name(time));
        }
        let tmp = fin.with_file_name(format!(".tmp-{}", time_name(time)));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        Ok(SnapshotWriter { repo: self, tmp, fin, archives: vec![], _lock: lock, group: group.into(), time })
    }

    pub fn snapshots(&self) -> Vec<Manifest> {
        let mut out = vec![];
        let root = self.dir.join("snapshots");
        for kind in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            for name in std::fs::read_dir(kind.path()).into_iter().flatten().flatten() {
                for t in std::fs::read_dir(name.path()).into_iter().flatten().flatten() {
                    if t.file_name().to_string_lossy().starts_with('.') {
                        continue;
                    }
                    if let Some(m) = std::fs::read(t.path().join("manifest.json")).ok().and_then(|b| serde_json::from_slice::<Manifest>(&b).ok()) {
                        out.push(m);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.group.cmp(&b.group).then(b.time.cmp(&a.time)));
        out
    }

    /// `group/2026-…Z` -> its manifest.
    pub fn snapshot(&self, id: &str) -> Result<Manifest> {
        let (group, t) = id.rsplit_once('/').context("bad snapshot id")?;
        let time = parse_time_name(t).context("bad snapshot id")?;
        if !valid_group(group) {
            bail!("bad snapshot id");
        }
        let b = std::fs::read(self.snap_dir(group, time).join("manifest.json")).context("no such backup")?;
        Ok(serde_json::from_slice(&b)?)
    }

    fn save_manifest(&self, m: &Manifest) -> Result<()> {
        let p = self.snap_dir(&m.group, m.time).join("manifest.json");
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn set_notes(&self, id: &str, notes: Option<&str>, protected: Option<bool>) -> Result<()> {
        let mut m = self.snapshot(id)?;
        if let Some(n) = notes {
            m.notes = n.chars().take(2000).collect();
        }
        if let Some(p) = protected {
            m.protected = p;
        }
        self.save_manifest(&m)
    }

    pub fn delete(&self, id: &str, force: bool) -> Result<()> {
        let m = self.snapshot(id)?;
        if m.protected && !force {
            bail!("{id} is protected");
        }
        let dir = self.snap_dir(&m.group, m.time);
        std::fs::remove_dir_all(&dir)?;
        let _ = std::fs::remove_dir(dir.parent().unwrap());
        Ok(())
    }

    fn index(&self, m: &Manifest, archive: &str) -> Result<Vec<(Id, u32)>> {
        if !valid_archive(archive) {
            bail!("bad archive name");
        }
        let b = std::fs::read(self.snap_dir(&m.group, m.time).join(format!("{archive}.idx"))).with_context(|| format!("{} has no archive {archive}", m.id()))?;
        if b.len() < 12 || &b[..4] != INDEX_MAGIC {
            bail!("index {archive} is damaged");
        }
        let n = u64::from_le_bytes(b[4..12].try_into().unwrap()) as usize;
        if b.len() != 12 + n * 36 {
            bail!("index {archive} is damaged");
        }
        Ok(b[12..].chunks_exact(36).map(|e| (e[..32].try_into().unwrap(), u32::from_le_bytes(e[32..].try_into().unwrap()))).collect())
    }

    /// Read an archive back as a byte stream.
    pub fn reader(&self, m: &Manifest, archive: &str) -> Result<ArchiveReader<'_>> {
        Ok(ArchiveReader { repo: self, idx: self.index(m, archive)?, ahead: Default::default(), next: 0, pos: 0, buf: vec![], off: 0, hash: Sha256::new() })
    }

    /// Re-read every chunk of a snapshot and check the archives' checksums.
    pub fn verify(&self, id: &str) -> Result<Manifest> {
        let mut m = self.snapshot(id)?;
        let mut err = String::new();
        for a in &m.archives {
            let r = (|| -> Result<()> {
                if a.csum.starts_with("b:") {
                    return self.verify_disk(&m, &a.name);
                }
                let mut r = self.reader(&m, &a.name)?;
                std::io::copy(&mut r, &mut std::io::sink())?;
                if r.csum() != a.csum {
                    bail!("{}: checksum mismatch", a.name);
                }
                Ok(())
            })();
            if let Err(e) = r {
                err = format!("{e:#}");
                break;
            }
        }
        m.verified = Some(Verified { time: now(), ok: err.is_empty(), error: err });
        self.save_manifest(&m)?;
        Ok(m)
    }

    /// Remove chunks no snapshot refers to (taking the repository alone).
    pub fn gc(&self) -> Result<GcReport> {
        let _l = self.lock(true)?;
        let start = std::time::SystemTime::now();
        let mut used: HashSet<Id> = HashSet::new();
        let snaps = self.snapshots();
        for m in &snaps {
            for a in &m.archives {
                for (id, _) in self.index(m, &a.name)? {
                    used.insert(id);
                }
            }
        }
        let grace = start - std::time::Duration::from_secs(300);
        let mut r = GcReport { snapshots: snaps.len() as u64, ..Default::default() };
        for sub in std::fs::read_dir(self.dir.join("chunks"))?.flatten() {
            for f in std::fs::read_dir(sub.path()).into_iter().flatten().flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                let Ok(md) = f.metadata() else { continue };
                let keep = hex::decode(&name).ok().and_then(|b| <Id>::try_from(b).ok()).is_some_and(|id| used.contains(&id));
                if keep {
                    r.chunks += 1;
                    r.bytes += md.len();
                } else if md.modified().is_ok_and(|t| t < grace) {
                    if std::fs::remove_file(f.path()).is_ok() {
                        r.removed_chunks += 1;
                        r.removed_bytes += md.len();
                    }
                } else {
                    r.pending += 1;
                }
            }
        }
        r.logical = snaps.iter().map(|m| m.size()).sum();
        Ok(r)
    }

    /// Bytes used by chunks (quick estimate from the files).
    pub fn usage(&self) -> (u64, u64) {
        let (mut n, mut b) = (0, 0);
        for sub in std::fs::read_dir(self.dir.join("chunks")).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(sub.path()).into_iter().flatten().flatten() {
                if let Ok(m) = f.metadata() {
                    n += 1;
                    b += m.len();
                }
            }
        }
        (n, b)
    }
}

#[derive(Debug, Default, Serialize)]
pub struct GcReport {
    pub snapshots: u64,
    pub chunks: u64,
    pub bytes: u64,
    pub removed_chunks: u64,
    pub removed_bytes: u64,
    /// unreferenced but too new to remove (a backup may be writing them)
    pub pending: u64,
    /// total size of everything backed up (before dedup + compression)
    pub logical: u64,
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub struct SnapshotWriter<'a> {
    repo: &'a Repo,
    tmp: PathBuf,
    fin: PathBuf,
    archives: Vec<ArchiveInfo>,
    _lock: Lock,
    group: String,
    time: u64,
}

impl SnapshotWriter<'_> {
    /// A new archive in this snapshot; write the stream into it, then [`ArchiveWriter::finish`].
    pub fn archive(&mut self, name: &str) -> Result<ArchiveWriter<'_>> {
        if !valid_archive(name) || self.archives.iter().any(|a| a.name == name) {
            bail!("bad or repeated archive name {name:?}");
        }
        Ok(ArchiveWriter { repo: self.repo, pool: PutPool::new(self.repo), cutter: Default::default(), buf: Vec::with_capacity(chunker::MAX * 2), idx: vec![], info: ArchiveInfo { name: name.into(), ..Default::default() }, hash: Sha256::new(), dir: self.tmp.clone(), done: &mut self.archives })
    }

    /// No archive added yet.
    pub fn is_empty(&self) -> bool {
        self.archives.is_empty()
    }

    pub fn commit(self, host: &str, meta: serde_json::Value, notes: &str, duration_ms: u64) -> Result<Manifest> {
        let m = Manifest { group: self.group.clone(), time: self.time, host: host.into(), archives: self.archives.clone(), meta, notes: notes.into(), protected: false, encrypted: self.repo.key_fingerprint().map(String::from), duration_ms, verified: None };
        std::fs::write(self.tmp.join("manifest.json"), serde_json::to_vec_pretty(&m)?)?;
        std::fs::rename(&self.tmp, &self.fin)?;
        Ok(m)
    }
}

impl Drop for SnapshotWriter<'_> {
    fn drop(&mut self) {
        // not committed: throw the partial snapshot away (its chunks go at the next GC)
        if self.tmp.exists() {
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }
}

/// Chunks are compressed (and sealed) and written by worker threads while the stream is cut;
/// the index keeps the stream's order.
struct PutPool {
    tx: Option<std::sync::mpsc::SyncSender<(usize, Vec<u8>)>>,
    rx: std::sync::mpsc::Receiver<(usize, Result<(Id, u64)>)>,
    workers: Vec<std::thread::JoinHandle<()>>,
    pending: usize,
}

fn threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 8)
}

impl PutPool {
    fn new(repo: &Repo) -> PutPool {
        let n = threads();
        let (tx, jobs) = std::sync::mpsc::sync_channel::<(usize, Vec<u8>)>(n * 2);
        let (done, rx) = std::sync::mpsc::channel();
        let jobs = std::sync::Arc::new(std::sync::Mutex::new(jobs));
        let workers = (0..n)
            .map(|_| {
                let (repo, jobs, done) = (repo.clone(), jobs.clone(), done.clone());
                std::thread::spawn(move || loop {
                    let job = jobs.lock().unwrap().recv();
                    let Ok((seq, data)) = job else { break };
                    if done.send((seq, repo.put(&data))).is_err() {
                        break;
                    }
                })
            })
            .collect();
        PutPool { tx: Some(tx), rx, workers, pending: 0 }
    }
}

pub struct ArchiveWriter<'a> {
    repo: &'a Repo,
    pool: PutPool,
    cutter: chunker::Cutter,
    buf: Vec<u8>,
    idx: Vec<(Id, u32)>,
    info: ArchiveInfo,
    hash: Sha256,
    dir: PathBuf,
    done: &'a mut Vec<ArchiveInfo>,
}

impl ArchiveWriter<'_> {
    fn emit(&mut self, n: usize) -> std::io::Result<()> {
        let seq = self.idx.len();
        self.idx.push(([0; 32], n as u32));
        let data = self.buf.drain(..n).collect::<Vec<u8>>();
        self.pool.pending += 1;
        self.pool.tx.as_ref().unwrap().send((seq, data)).map_err(|_| std::io::Error::other("backup workers stopped"))?;
        // collect what's done meanwhile
        while let Ok(r) = self.pool.rx.try_recv() {
            self.done_chunk(r).map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    fn done_chunk(&mut self, (seq, r): (usize, Result<(Id, u64)>)) -> Result<()> {
        self.pool.pending -= 1;
        let (id, written) = r?;
        self.idx[seq].0 = id;
        if written > 0 {
            self.info.new_chunks += 1;
            self.info.new_bytes += written;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<ArchiveInfo> {
        if !self.buf.is_empty() {
            let n = self.buf.len();
            self.emit(n)?;
        }
        drop(self.pool.tx.take());
        while self.pool.pending > 0 {
            let r = self.pool.rx.recv().context("backup workers stopped")?;
            self.done_chunk(r)?;
        }
        for w in std::mem::take(&mut self.pool.workers) {
            let _ = w.join();
        }
        let mut b = Vec::with_capacity(12 + self.idx.len() * 36);
        b.extend_from_slice(INDEX_MAGIC);
        b.extend_from_slice(&(self.idx.len() as u64).to_le_bytes());
        for (id, n) in &self.idx {
            b.extend_from_slice(id);
            b.extend_from_slice(&n.to_le_bytes());
        }
        std::fs::write(self.dir.join(format!("{}.idx", self.info.name)), b)?;
        self.info.chunks = self.idx.len() as u64;
        self.info.csum = hex::encode(std::mem::take(&mut self.hash).finalize());
        self.done.push(self.info.clone());
        Ok(self.info.clone())
    }
}

impl Write for ArchiveWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.hash.update(data);
        self.info.size += data.len() as u64;
        self.buf.extend_from_slice(data);
        while let Some(n) = self.cutter.find(&self.buf) {
            self.emit(n)?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct ArchiveReader<'a> {
    repo: &'a Repo,
    idx: Vec<(Id, u32)>,
    /// chunks read, checked and decompressed ahead by worker threads, in order
    ahead: std::collections::VecDeque<std::sync::mpsc::Receiver<Result<Vec<u8>>>>,
    next: usize,
    pos: usize,
    buf: Vec<u8>,
    off: usize,
    hash: Sha256,
}

impl ArchiveReader<'_> {
    /// sha256 of what was read so far (all of it, once read to the end).
    pub fn csum(&self) -> String {
        hex::encode(self.hash.clone().finalize())
    }
}

impl Read for ArchiveReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.off >= self.buf.len() {
            if self.pos >= self.idx.len() {
                return Ok(0);
            }
            // keep a few chunks in flight
            while self.next < self.idx.len() && self.ahead.len() < threads() * 2 {
                let (repo, id) = (self.repo.clone(), self.idx[self.next].0);
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(repo.get(&id));
                });
                self.ahead.push_back(rx);
                self.next += 1;
            }
            let (id, n) = self.idx[self.pos];
            let rx = self.ahead.pop_front().expect("chunk in flight");
            self.buf = rx.recv().map_err(|_| std::io::Error::other("chunk reader stopped"))?.map_err(std::io::Error::other)?;
            if self.buf.len() != n as usize {
                return Err(std::io::Error::other(format!("chunk {} has the wrong size", id_hex(&id))));
            }
            self.hash.update(&self.buf);
            self.off = 0;
            self.pos += 1;
        }
        let n = out.len().min(self.buf.len() - self.off);
        out[..n].copy_from_slice(&self.buf[self.off..self.off + n]);
        self.off += n;
        Ok(n)
    }
}

// ---- directories as tar streams ----

/// Write `src` into `w` as a tar stream under `prefix`, keeping ownership, modes, symlinks and
/// device nodes, staying on one file system, skipping sockets and paths in `exclude` (relative
/// to `src`, prefix match).
pub fn tar_dir<W: Write>(w: W, src: &Path, prefix: &str, exclude: &[String]) -> Result<W> {
    use std::os::unix::fs::MetadataExt;
    let mut b = tar::Builder::new(w);
    b.mode(tar::HeaderMode::Complete);
    b.follow_symlinks(false);
    let dev = std::fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?.dev();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        // the root itself as given (joining "" would add a trailing slash, which a file
        // root doesn't survive)
        let full = if rel.as_os_str().is_empty() { src.to_path_buf() } else { src.join(&rel) };
        let md = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(_) => continue, // removed while we walk
        };
        let ft = md.file_type();
        let name = if rel.as_os_str().is_empty() { PathBuf::from(prefix) } else { Path::new(prefix).join(&rel) };
        let rs = rel.to_string_lossy();
        if !rs.is_empty() && exclude.iter().any(|e| rs == e.as_str() || rs.starts_with(&format!("{}/", e.trim_end_matches('/')))) {
            continue;
        }
        {
            use std::os::unix::fs::FileTypeExt;
            if ft.is_socket() {
                continue;
            }
        }
        if ft.is_dir() {
            b.append_dir(&name, &full)?;
            if md.dev() != dev {
                continue; // a mount point: keep the directory, not what's mounted on it
            }
            let mut kids: Vec<_> = std::fs::read_dir(&full).into_iter().flatten().flatten().map(|e| rel.join(e.file_name())).collect();
            kids.sort();
            stack.extend(kids.into_iter().rev());
        } else if ft.is_file() {
            match std::fs::File::open(&full) {
                Ok(mut f) => b.append_file(&name, &mut f)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {}", full.display())),
            }
        } else {
            // symlinks, devices, fifos: header only
            b.append_path_with_name(&full, &name)?;
        }
    }
    Ok(b.into_inner()?)
}

/// Unpack a tar stream into `dest`, ownership and modes as stored.
pub fn untar<R: Read>(r: R, dest: &Path) -> Result<u64> {
    std::fs::create_dir_all(dest)?;
    let mut a = tar::Archive::new(r);
    a.set_preserve_permissions(true);
    a.set_preserve_ownerships(true);
    a.set_preserve_mtime(true);
    a.set_overwrite(true);
    let mut n = 0;
    for e in a.entries()? {
        let mut e = e?;
        if !e.unpack_in(dest)? {
            bail!("refusing a path outside the target: {}", e.path()?.display());
        }
        n += 1;
    }
    Ok(n)
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub kind: &'static str,
    pub mtime: u64,
}

/// List a tar stream (at most `limit` entries).
pub fn list<R: Read>(r: R, limit: usize) -> Result<Vec<Entry>> {
    let mut a = tar::Archive::new(r);
    let mut out = vec![];
    for e in a.entries()? {
        let e = e?;
        let h = e.header();
        let kind = match h.entry_type() {
            tar::EntryType::Directory => "dir",
            tar::EntryType::Symlink => "link",
            tar::EntryType::Regular | tar::EntryType::Continuous => "file",
            _ => "other",
        };
        out.push(Entry { path: e.path()?.to_string_lossy().into_owned(), size: h.size().unwrap_or(0), kind, mtime: h.mtime().unwrap_or(0) });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(dir: &Path, seed: u8) {
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let big: Vec<u8> = (0..6_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8 ^ seed).collect();
        std::fs::write(dir.join("big.bin"), &big).unwrap();
        std::fs::write(dir.join("sub/hello.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("sub/hello.txt", dir.join("link")).unwrap();
        std::fs::write(dir.join("skip.log"), b"x").unwrap();
    }

    fn backup(repo: &Repo, src: &Path, t: u64) -> Manifest {
        let mut s = repo.begin("ct/web", t).unwrap();
        let mut w = s.archive("vol-data").unwrap();
        w = tar_dir(w, src, "data", &["skip.log".into()]).unwrap();
        w.finish().unwrap();
        s.commit("test", serde_json::json!({"name": "web"}), "", 1).unwrap()
    }

    #[test]
    fn roundtrip_dedup_gc_and_keys() {
        for key in [None, Some(Key::generate())] {
            let t = tempfile::tempdir().unwrap();
            let src = t.path().join("src");
            fill(&src, 1);
            let repo = Repo::open(&t.path().join("repo"), key.as_ref()).unwrap();
            let m1 = backup(&repo, &src, 1_700_000_000);
            assert_eq!(m1.encrypted.is_some(), key.is_some());
            let first_new = m1.archives[0].new_bytes;
            assert!(first_new > 0 && first_new < 7_000_000);
            // unchanged: nothing new stored
            let m2 = backup(&repo, &src, 1_700_000_100);
            assert_eq!(m2.archives[0].new_chunks, 0);
            assert_eq!(m2.archives[0].csum, m1.archives[0].csum);
            // restore
            let out = t.path().join("out");
            let n = untar(repo.reader(&m2, "vol-data").unwrap(), &out).unwrap();
            assert!(n >= 4);
            assert_eq!(std::fs::read(out.join("data/sub/hello.txt")).unwrap(), b"hello");
            assert_eq!(std::fs::read(out.join("data/big.bin")).unwrap(), std::fs::read(src.join("big.bin")).unwrap());
            assert_eq!(std::fs::read_link(out.join("data/link")).unwrap(), Path::new("sub/hello.txt"));
            assert!(!out.join("data/skip.log").exists());
            assert!(list(repo.reader(&m2, "vol-data").unwrap(), 100).unwrap().iter().any(|e| e.path == "data/big.bin" && e.size == 6_000_000));
            assert!(repo.verify(&m2.id()).unwrap().verified.unwrap().ok);
            assert_eq!(repo.snapshots().len(), 2);
            // GC keeps everything in use; after deleting both, chunks are only removed after the grace period
            assert_eq!(repo.gc().unwrap().removed_chunks, 0);
            repo.delete(&m1.id(), false).unwrap();
            repo.delete(&m2.id(), false).unwrap();
            let r = repo.gc().unwrap();
            assert_eq!((r.removed_chunks, r.chunks), (0, 0));
            assert!(r.pending > 0);
            // wrong key can't read
            if key.is_some() {
                let m3 = backup(&repo, &src, 1_700_000_200);
                let other = Repo::open(&repo.dir, Some(&Key::generate())).unwrap();
                assert!(std::io::copy(&mut other.reader(&m3, "vol-data").unwrap(), &mut std::io::sink()).is_err());
                let plain = Repo::open(&repo.dir, None).unwrap();
                assert!(std::io::copy(&mut plain.reader(&m3, "vol-data").unwrap(), &mut std::io::sink()).is_err());
                // encryption turned off: the key still reads old sealed backups, new ones are plain
                let off = Repo::open_with(&repo.dir, key.as_ref(), false).unwrap();
                assert!(std::io::copy(&mut off.reader(&m3, "vol-data").unwrap(), &mut std::io::sink()).is_ok());
                let m4 = backup(&off, &src, 1_700_000_300);
                assert!(m4.encrypted.is_none() && m4.archives[0].new_chunks > 0);
                assert!(std::io::copy(&mut plain.reader(&m4, "vol-data").unwrap(), &mut std::io::sink()).is_ok());
            }
        }
    }

    #[test]
    fn a_single_file_as_the_root() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("disk.qcow2");
        std::fs::write(&f, vec![7u8; 3_000_000]).unwrap();
        let repo = Repo::open(&t.path().join("repo"), None).unwrap();
        let mut s = repo.begin("vm/x", 1).unwrap();
        let w = s.archive("vol-disk0").unwrap();
        let a = tar_dir(w, &f, "disk", &[]).unwrap().finish().unwrap();
        assert!(a.size > 3_000_000, "the file is in the archive");
        let m = s.commit("t", serde_json::json!({}), "", 0).unwrap();
        let out = t.path().join("out");
        untar(repo.reader(&m, "vol-disk0").unwrap(), &out).unwrap();
        assert_eq!(std::fs::read(out.join("disk")).unwrap().len(), 3_000_000);
    }

    #[test]
    fn names_and_times() {
        assert_eq!(time_name(0), "1970-01-01T00:00:00Z");
        assert_eq!(time_name(1_792_620_005), "2026-10-21T22:00:05Z");
        assert_eq!(parse_time_name("2026-10-21T22:00:05Z"), Some(1_792_620_005));
        assert!(valid_group("stack/immich") && !valid_group("ct/../x") && !valid_group("foo/bar"));
    }
}
