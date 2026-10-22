//! Disk images and block devices (VM disks): fixed-size blocks instead of content-defined
//! chunks, read, checked, compressed and written by all worker threads at once.
//!
//! A disk is mostly unchanged between backups and its data doesn't move around, so fixed
//! blocks dedup as well as content-defined chunks do, and they let every worker read its own
//! part of the disk (`pread`) instead of one thread feeding a chunker. Blocks that are all
//! zeros (unused space, unprovisioned thin volumes) are recognised without hashing them and
//! all share one chunk; a restore skips them on a fresh volume without reading anything.
//!
//! The archive's `csum` is `b:` + SHA-256 over the blocks' SHA-256s in order (the hashes are
//! made in parallel; a stream hash would be one thread reading the whole disk). It is an
//! ordinary archive otherwise: [`Repo::reader`] reads it back as a byte stream.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::{threads, valid_archive, ArchiveInfo, Id, Manifest, Repo, SnapshotWriter, INDEX_MAGIC};

/// Block size of disk archives.
pub const BLOCK: usize = 4 << 20;

/// Is every byte zero? (ORs 4 KiB at a time, which vectorizes; stops at the first data.)
pub fn is_zero(b: &[u8]) -> bool {
    b.chunks(4096)
        .all(|c| c.iter().fold(0u8, |a, &x| a | x) == 0)
}

fn size_of(f: &File) -> Result<u64> {
    let mut f = f;
    let n = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(0))?;
    Ok(n)
}

/// Read `buf.len()` bytes at `off` (short only at the end of the file).
fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], off + n as u64) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Don't keep what was just read or written in the page cache (a backup of a big disk would
/// only push everything else out of it).
fn drop_cache(f: &File, off: u64, len: usize) {
    unsafe {
        libc::posix_fadvise(
            f.as_raw_fd(),
            off as i64,
            len as i64,
            libc::POSIX_FADV_DONTNEED,
        )
    };
}

struct Block {
    id: Id,
    sum: [u8; 32],
    len: u32,
    written: u64,
}

impl SnapshotWriter<'_> {
    /// Back up a disk image or block device as archive `name` (see the module docs).
    /// `check` is called now and then; an error from it stops the backup.
    pub fn disk_archive(
        &mut self,
        name: &str,
        f: &File,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<ArchiveInfo> {
        if !valid_archive(name) || self.archives.iter().any(|a| a.name == name) {
            bail!("bad or repeated archive name {name:?}");
        }
        let repo = self.repo;
        let size = size_of(f)?;
        let n = size.div_ceil(BLOCK as u64) as usize;
        let zero = vec![0u8; BLOCK];
        let zero_id = repo.id_of(&zero, repo.seal);
        let zero_sum: [u8; 32] = Sha256::digest(&zero).into();
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let blocks: Mutex<Vec<Option<Block>>> = Mutex::new((0..n).map(|_| None).collect());
        let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..threads() {
                s.spawn(|| {
                    let mut buf = vec![0u8; BLOCK];
                    let r = (|| -> Result<()> {
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= n || stop.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            if i % 64 == 0 {
                                check()?;
                            }
                            let off = i as u64 * BLOCK as u64;
                            let want = (size - off).min(BLOCK as u64) as usize;
                            let got = read_at(f, &mut buf[..want], off)
                                .with_context(|| format!("reading at {off}"))?;
                            if got != want {
                                bail!("short read at {off}: the disk shrank while it was read");
                            }
                            drop_cache(f, off, want);
                            let data = &buf[..want];
                            let b = if want == BLOCK && is_zero(data) {
                                Block {
                                    id: zero_id,
                                    sum: zero_sum,
                                    len: BLOCK as u32,
                                    written: 0,
                                }
                            } else {
                                let (id, written) = repo.put(data)?;
                                // unkeyed, the chunk id already is the data's SHA-256
                                let sum = if repo.seal && repo.key.is_some() {
                                    Sha256::digest(data).into()
                                } else {
                                    id
                                };
                                Block {
                                    id,
                                    sum,
                                    len: want as u32,
                                    written,
                                }
                            };
                            blocks.lock().unwrap()[i] = Some(b);
                        }
                    })();
                    if let Err(e) = r {
                        stop.store(true, Ordering::Relaxed);
                        first_err.lock().unwrap().get_or_insert(e);
                    }
                });
            }
        });
        if let Some(e) = first_err.into_inner().unwrap() {
            return Err(e);
        }
        let blocks: Vec<Block> = blocks
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|b| b.context("a block wasn't read"))
            .collect::<Result<_>>()?;
        let mut info = ArchiveInfo {
            name: name.into(),
            size,
            chunks: n as u64,
            ..Default::default()
        };
        if blocks.iter().any(|b| b.id == zero_id) {
            let (_, w) = repo.put(&zero)?;
            if w > 0 {
                info.new_chunks += 1;
                info.new_bytes += w;
            }
        }
        let mut idx = Vec::with_capacity(12 + n * 36);
        idx.extend_from_slice(INDEX_MAGIC);
        idx.extend_from_slice(&(n as u64).to_le_bytes());
        let mut h = Sha256::new();
        for b in &blocks {
            idx.extend_from_slice(&b.id);
            idx.extend_from_slice(&b.len.to_le_bytes());
            h.update(b.sum);
            if b.written > 0 {
                info.new_chunks += 1;
                info.new_bytes += b.written;
            }
        }
        std::fs::write(self.tmp.join(format!("{name}.idx")), idx)?;
        info.csum = format!("b:{}", hex::encode(h.finalize()));
        self.archives.push(info.clone());
        Ok(info)
    }
}

impl Repo {
    /// Write an archive (a disk archive, or any other) into `out` at its offsets, with all
    /// worker threads. `fresh`: `out` is new and reads as zeros, so zero blocks are skipped
    /// (the shared zero chunk isn't even read). Checks the archive's checksum. Returns the
    /// bytes restored.
    pub fn restore_disk(
        &self,
        m: &Manifest,
        archive: &str,
        out: &File,
        fresh: bool,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<u64> {
        let info = m
            .archives
            .iter()
            .find(|a| a.name == archive)
            .context("no such archive")?;
        let idx = self.index(m, archive)?;
        let mut offs = Vec::with_capacity(idx.len());
        let mut total = 0u64;
        for (_, n) in &idx {
            offs.push(total);
            total += *n as u64;
        }
        let blockwise = info.csum.starts_with("b:");
        let zero = vec![0u8; BLOCK];
        let zero_id = self.id_of(&zero, self.has_sealed_zero(&idx));
        let zero_sum: [u8; 32] = Sha256::digest(&zero).into();
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let sums: Mutex<Vec<[u8; 32]>> = Mutex::new(vec![[0; 32]; idx.len()]);
        let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..threads() {
                s.spawn(|| {
                    let r = (|| -> Result<()> {
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= idx.len() || stop.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            if i % 64 == 0 {
                                check()?;
                            }
                            let (id, n) = idx[i];
                            if id == zero_id && n as usize == BLOCK {
                                sums.lock().unwrap()[i] = zero_sum;
                                if !fresh {
                                    out.write_all_at(&zero, offs[i])?;
                                }
                                continue;
                            }
                            let (data, sealed) = self.get_flagged(&id)?;
                            if data.len() != n as usize {
                                bail!("chunk {} has the wrong size", crate::id_hex(&id));
                            }
                            if blockwise {
                                // get() checked the data against its id: unsealed, the id is its SHA-256
                                sums.lock().unwrap()[i] = if sealed { Sha256::digest(&data).into() } else { id };
                            }
                            if fresh && is_zero(&data) {
                                continue;
                            }
                            out.write_all_at(&data, offs[i])?;
                            drop_cache(out, offs[i], data.len());
                        }
                    })();
                    if let Err(e) = r {
                        stop.store(true, Ordering::Relaxed);
                        first_err.lock().unwrap().get_or_insert(e);
                    }
                });
            }
        });
        if let Some(e) = first_err.into_inner().unwrap() {
            return Err(e);
        }
        // chunks are checked against their ids as they're read; blockwise archives also
        // against the archive's checksum (the order and completeness of the index)
        if blockwise {
            let mut h = Sha256::new();
            for s in sums.into_inner().unwrap() {
                h.update(s);
            }
            if format!("b:{}", hex::encode(h.finalize())) != info.csum {
                bail!("{archive}: checksum mismatch");
            }
        }
        out.sync_all()?;
        Ok(total)
    }

    /// Check a blockwise archive: every chunk read and checked, and the archive checksum.
    pub(crate) fn verify_disk(&self, m: &Manifest, archive: &str) -> Result<()> {
        let info = m
            .archives
            .iter()
            .find(|a| a.name == archive)
            .context("no such archive")?;
        let idx = self.index(m, archive)?;
        let zero = vec![0u8; BLOCK];
        let zero_sum: [u8; 32] = Sha256::digest(&zero).into();
        let ids: Vec<Id> = idx.iter().map(|x| x.0).collect();
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        // each distinct chunk once (the zero chunk is usually most of a disk)
        let next = AtomicUsize::new(0);
        let found: Mutex<std::collections::HashMap<Id, [u8; 32]>> = Mutex::new(Default::default());
        let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..threads() {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= uniq.len() {
                        return;
                    }
                    match self.get(&uniq[i]) {
                        Ok(d) => {
                            let sum: [u8; 32] = if d.len() == BLOCK && is_zero(&d) {
                                zero_sum
                            } else {
                                Sha256::digest(&d).into()
                            };
                            found.lock().unwrap().insert(uniq[i], sum);
                        }
                        Err(e) => {
                            first_err.lock().unwrap().get_or_insert(e);
                        }
                    }
                });
            }
        });
        if let Some(e) = first_err.into_inner().unwrap() {
            return Err(e);
        }
        let found = found.into_inner().unwrap();
        let mut h = Sha256::new();
        for id in &ids {
            h.update(found[id]);
        }
        if format!("b:{}", hex::encode(h.finalize())) != info.csum {
            bail!("{archive}: checksum mismatch");
        }
        Ok(())
    }

    /// Whether this index's zero blocks were sealed (keyed id) or not: a keyed datastore can
    /// hold both, from before and after a key was set.
    fn has_sealed_zero(&self, idx: &[(Id, u32)]) -> bool {
        match &self.key {
            None => false,
            Some(_) => {
                let zero = vec![0u8; BLOCK];
                let keyed = self.id_of(&zero, true);
                idx.iter().any(|(id, _)| *id == keyed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn image(dir: &std::path::Path) -> std::path::PathBuf {
        // zeros, data, a block of data with zeros around it, and a short last block
        let p = dir.join("disk.img");
        let mut f = File::create(&p).unwrap();
        let mut x = 7u64;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x % 7) as u8
                })
                .collect()
        };
        f.write_all(&vec![0u8; 2 * BLOCK]).unwrap();
        f.write_all(&noise(BLOCK + 12345)).unwrap();
        f.write_all(&vec![0u8; BLOCK]).unwrap();
        f.write_all(&noise(777)).unwrap();
        p
    }

    #[test]
    fn identical_blocks_stored_by_many_threads() {
        // the same non-zero block many times: workers store that chunk at the same moment
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("same.img");
        std::fs::write(&p, vec![b'a'; 32 * BLOCK]).unwrap();
        let repo = Repo::open(&t.path().join("store"), None).unwrap();
        let ok = || Ok(());
        let mut s = repo.begin("vm/x", 1000).unwrap();
        let a = s.disk_archive("raw-disk0", &File::open(&p).unwrap(), &ok).unwrap();
        assert_eq!(a.size, 32 * BLOCK as u64);
        s.commit("h", serde_json::json!({}), "", 0).unwrap();
    }

    fn roundtrip(key: Option<&[u8; 32]>) {
        let t = tempfile::tempdir().unwrap();
        let img = image(t.path());
        let repo = Repo::open(&t.path().join("store"), key).unwrap();
        let ok = || Ok(());
        let mut s = repo.begin("vm/x", 1000).unwrap();
        let a = s
            .disk_archive("raw-disk0", &File::open(&img).unwrap(), &ok)
            .unwrap();
        assert_eq!(a.size, std::fs::metadata(&img).unwrap().len());
        assert!(a.csum.starts_with("b:"));
        let m = s.commit("h", serde_json::json!({}), "", 0).unwrap();
        // a second backup of the same disk stores nothing new and has the same checksum
        let mut s = repo.begin("vm/x", 1001).unwrap();
        let b = s
            .disk_archive("raw-disk0", &File::open(&img).unwrap(), &ok)
            .unwrap();
        assert_eq!(b.new_bytes, 0);
        assert_eq!(a.csum, b.csum);
        s.commit("h", serde_json::json!({}), "", 0).unwrap();
        for fresh in [true, false] {
            let out = t.path().join(format!("out-{fresh}"));
            let f = File::create(&out).unwrap();
            if fresh {
                f.set_len(a.size).unwrap();
            } else {
                f.set_len(a.size).unwrap();
                f.write_all_at(&vec![9u8; a.size as usize], 0).unwrap();
            }
            repo.restore_disk(&m, "raw-disk0", &f, fresh, &ok).unwrap();
            assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&img).unwrap());
        }
        // the stream reader reads it back too
        let mut v = vec![];
        std::io::Read::read_to_end(&mut repo.reader(&m, "raw-disk0").unwrap(), &mut v).unwrap();
        assert_eq!(v, std::fs::read(&img).unwrap());
        assert!(repo.verify(&m.id()).unwrap().verified.unwrap().ok);
    }

    #[test]
    fn disk_roundtrip() {
        roundtrip(None);
    }

    #[test]
    fn disk_roundtrip_encrypted() {
        roundtrip(Some(&[3u8; 32]));
    }

    #[test]
    fn disk_index_tampering_is_caught() {
        let t = tempfile::tempdir().unwrap();
        let img = image(t.path());
        let repo = Repo::open(&t.path().join("store"), None).unwrap();
        let ok = || Ok(());
        let mut s = repo.begin("vm/x", 1000).unwrap();
        s.disk_archive("raw-disk0", &File::open(&img).unwrap(), &ok)
            .unwrap();
        let m = s.commit("h", serde_json::json!({}), "", 0).unwrap();
        // swap two index entries: every chunk is still valid, the order isn't
        let p = repo.snap_dir(&m.group, m.time).join("raw-disk0.idx");
        let mut b = std::fs::read(&p).unwrap();
        let (e1, e2) = (12, 12 + 2 * 36);
        let tmp = b[e1..e1 + 36].to_vec();
        b.copy_within(e2..e2 + 36, e1);
        b[e2..e2 + 36].copy_from_slice(&tmp);
        std::fs::write(&p, b).unwrap();
        assert!(!repo.verify(&m.id()).unwrap().verified.unwrap().ok);
        let out = File::create(t.path().join("o")).unwrap();
        out.set_len(m.archives[0].size).unwrap();
        assert!(repo.restore_disk(&m, "raw-disk0", &out, true, &ok).is_err());
    }
}
