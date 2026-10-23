//! Silent corruption detection for app data (no ECC, no checksumming filesystem underneath).
//!
//! A scrub hashes (SHA-256) every file under the apps and volumes folders and compares with the
//! last scrub: a file whose size and modification time are unchanged but whose contents differ
//! was changed by something other than a program writing it (a bit flip in RAM on the way to
//! disk, the disk itself). Those are reported, and the earlier hash is kept so it stays reported
//! until the file is restored or rewritten. Files that changed the normal way just get their new
//! hash. Runs weekly at idle CPU and I/O priority; `og-hostd scrub` runs one now.

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;

/// Folder names skipped anywhere: caches and in-progress downloads change all the time.
const SKIP: &[&str] = &[".cache", "__pycache__", "downloads", "incomplete", "tmp", "temp", "logs", "node_modules/.cache"];
const EVERY: Duration = Duration::from_secs(7 * 86400);

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    p: String,
    size: u64,
    mtime: i64,
    sha: String,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Corrupt {
    pub path: String,
    pub size: u64,
    pub expected: String,
    pub got: String,
    pub first_seen: u64,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Report {
    pub started: u64,
    pub finished: u64,
    pub files: u64,
    pub bytes: u64,
    pub new: u64,
    pub changed: u64,
    pub unreadable: u64,
    pub corrupt: Vec<Corrupt>,
    pub baseline: bool,
}

fn dir(root: &Path) -> PathBuf {
    root.join("integrity")
}

pub fn report(root: &Path) -> Option<Report> {
    serde_json::from_slice(&std::fs::read(dir(root).join("report.json")).ok()?).ok()
}

fn load_manifest(root: &Path) -> BTreeMap<String, Entry> {
    let Ok(f) = std::fs::File::open(dir(root).join("manifest.jsonl")) else { return BTreeMap::new() };
    std::io::BufReader::new(f).lines().map_while(|l| l.ok()).filter_map(|l| serde_json::from_str::<Entry>(&l).ok()).map(|e| (e.p.clone(), e)).collect()
}

fn save_manifest(root: &Path, m: &BTreeMap<String, Entry>) -> Result<()> {
    let p = dir(root).join("manifest.jsonl");
    let tmp = p.with_extension("tmp");
    let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    for e in m.values() {
        serde_json::to_writer(&mut w, e)?;
        w.write_all(b"\n")?;
    }
    w.flush()?;
    drop(w);
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// Lowest CPU and I/O priority for this thread.
fn background_priority() {
    unsafe {
        let p = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &p);
        // ioprio_set(IOPRIO_WHO_PROCESS, this thread, IOPRIO_CLASS_IDLE)
        libc::syscall(libc::SYS_ioprio_set, 1, libc::gettid(), 3 << 13);
    }
}

fn hash(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn walk(p: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(p) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let path = e.path();
        if ft.is_dir() {
            let name = e.file_name();
            if SKIP.iter().any(|s| name == *s) {
                continue;
            }
            // other filesystems (NFS shares bind-mounted into an app folder) aren't ours to check
            if e.metadata().ok().zip(std::fs::metadata(p).ok()).is_some_and(|(a, b)| a.dev() != b.dev()) {
                continue;
            }
            walk(&path, out);
        } else if ft.is_file() {
            out.push(path);
        }
    }
}

pub fn scrub(root: &Path) -> Result<Report> {
    std::fs::create_dir_all(dir(root))?;
    background_priority();
    let old = load_manifest(root);
    let prev = report(root).unwrap_or_default();
    let baseline = old.is_empty();
    let mut rep = Report { started: crate::containers::unix_now(), baseline, ..Default::default() };
    let mut files = Vec::new();
    for sub in ["apps", "volumes"] {
        walk(&root.join(sub), &mut files);
    }
    let mut new_m = BTreeMap::new();
    for f in files {
        let key = f.to_string_lossy().into_owned();
        let Ok(md) = std::fs::symlink_metadata(&f) else { continue };
        let (size, mtime) = (md.len(), md.mtime() * 1_000_000_000 + md.mtime_nsec());
        let sha = match hash(&f) {
            Ok(s) => s,
            Err(_) => {
                rep.unreadable += 1;
                if let Some(e) = old.get(&key) {
                    new_m.insert(key, e.clone());
                }
                continue;
            }
        };
        // written to while we read it: a normal change, look again next time
        let still = std::fs::symlink_metadata(&f).is_ok_and(|m| m.len() == size && m.mtime() * 1_000_000_000 + m.mtime_nsec() == mtime);
        rep.files += 1;
        rep.bytes += size;
        match old.get(&key) {
            Some(e) if e.size == size && e.mtime == mtime && e.sha != sha && still => {
                let first = prev.corrupt.iter().find(|c| c.path == key).map(|c| c.first_seen).unwrap_or(rep.started);
                eprintln!("og-hostd: INTEGRITY: {key} changed without being written (sha256 {} -> {})", &e.sha[..12], &sha[..12]);
                rep.corrupt.push(Corrupt { path: key.clone(), size, expected: e.sha.clone(), got: sha, first_seen: first });
                new_m.insert(key, e.clone()); // keep the good hash: stays reported until restored
            }
            Some(e) if e.size == size && e.mtime == mtime => {
                new_m.insert(key, e.clone());
            }
            Some(_) => {
                if still {
                    rep.changed += 1;
                    new_m.insert(key.clone(), Entry { p: key, size, mtime, sha });
                }
            }
            None => {
                if still {
                    rep.new += 1;
                    new_m.insert(key.clone(), Entry { p: key, size, mtime, sha });
                }
            }
        }
    }
    save_manifest(root, &new_m)?;
    rep.finished = crate::containers::unix_now();
    std::fs::write(dir(root).join("report.json"), serde_json::to_vec_pretty(&rep)?).context("writing the report")?;
    Ok(rep)
}

/// Weekly, checked hourly.
pub async fn scheduler(root: PathBuf) {
    tokio::time::sleep(Duration::from_secs(600)).await;
    loop {
        let due = report(&root).is_none_or(|r| crate::containers::unix_now().saturating_sub(r.finished) >= EVERY.as_secs());
        if due {
            let r = root.clone();
            match tokio::task::spawn_blocking(move || scrub(&r)).await {
                Ok(Ok(rep)) => eprintln!(
                    "og-hostd: integrity scrub: {} files, {} GiB in {} min, {} new, {} changed, {} corrupt{}",
                    rep.files,
                    rep.bytes >> 30,
                    (rep.finished - rep.started) / 60,
                    rep.new,
                    rep.changed,
                    rep.corrupt.len(),
                    if rep.baseline { " (first scrub: baseline)" } else { "" }
                ),
                Ok(Err(e)) => eprintln!("og-hostd: integrity scrub failed: {e:#}"),
                Err(_) => {}
            }
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

pub fn cli(root: &Path) -> Result<()> {
    let rep = scrub(root)?;
    println!("{} files, {:.1} GiB, {} new, {} changed, {} unreadable, {} corrupt", rep.files, rep.bytes as f64 / (1u64 << 30) as f64, rep.new, rep.changed, rep.unreadable, rep.corrupt.len());
    for c in &rep.corrupt {
        println!("CORRUPT {}  ({} bytes; expected {} got {})", c.path, c.size, c.expected, c.got);
    }
    Ok(())
}

pub fn api_json(root: &Path) -> serde_json::Value {
    match report(root) {
        Some(r) => serde_json::to_value(r).unwrap_or_default(),
        None => serde_json::json!({"never": true}),
    }
}

