//! Tasks: long jobs (backups, restores, garbage collection, …) run in the background with a
//! log, like Proxmox's task list. `<root>/tasks/<id>.log` holds each log and
//! `<root>/tasks/index.jsonl` one line per finished task (kept to the last 2000).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub user: String,
    pub start: u64,
    #[serde(default)]
    pub end: Option<u64>,
    /// running | ok | warnings | error | stopped
    pub status: String,
    #[serde(default)]
    pub error: String,
}

pub struct Task {
    pub info: Mutex<Info>,
    cancel: AtomicBool,
    log: Mutex<std::fs::File>,
    warned: AtomicBool,
}

impl Task {
    pub fn log(&self, line: impl AsRef<str>) {
        let t = crate::containers::unix_now();
        let mut f = self.log.lock().unwrap();
        let _ = writeln!(f, "{} {}", og_backup::time_name(t), line.as_ref());
    }

    pub fn warn(&self, line: impl AsRef<str>) {
        self.warned.store(true, Ordering::Relaxed);
        self.log(format!("WARNING: {}", line.as_ref()));
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Err if the task was asked to stop (call between steps).
    pub fn check(&self) -> Result<()> {
        if self.cancelled() {
            anyhow::bail!("stopped by request");
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Tasks {
    dir: PathBuf,
    running: Mutex<HashMap<String, Arc<Task>>>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() < 160 && id.chars().all(|c| c.is_ascii_alphanumeric() || "_.:-@".contains(c))
}

impl Tasks {
    pub fn open(root: &Path) -> Tasks {
        let dir = root.join("tasks");
        let _ = std::fs::create_dir_all(&dir);
        // tasks that were running when the daemon stopped
        Tasks { dir, running: Mutex::new(HashMap::new()) }
    }

    /// Start `f` on a blocking thread; returns the task id at once.
    pub fn spawn<F>(self: &Arc<Self>, kind: &str, target: &str, user: &str, f: F) -> Result<String>
    where
        F: FnOnce(&Task) -> Result<()> + Send + 'static,
    {
        let now = crate::containers::unix_now();
        let rnd: String = crate::containers::new_id()?.chars().take(8).collect();
        let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || "_.-@".contains(c) { c } else { '_' }).take(60).collect::<String>();
        let id = format!("{}:{}:{}:{rnd}", clean(kind), clean(target), now);
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{id}.log")))?;
        let task = Arc::new(Task { info: Mutex::new(Info { id: id.clone(), kind: kind.into(), target: target.into(), user: user.into(), start: now, end: None, status: "running".into(), error: String::new() }), cancel: AtomicBool::new(false), log: Mutex::new(log), warned: AtomicBool::new(false) });
        self.running.lock().unwrap().insert(id.clone(), task.clone());
        let me = self.clone();
        let id2 = id.clone();
        let started = format!("{kind} {target} started by {user}");
        tokio::task::spawn_blocking(move || {
            task.log(started);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&task)));
            let (status, error) = match r {
                Ok(Ok(())) if task.warned.load(Ordering::Relaxed) => ("warnings", String::new()),
                Ok(Ok(())) => ("ok", String::new()),
                Ok(Err(e)) if task.cancelled() => ("stopped", format!("{e:#}")),
                Ok(Err(e)) => ("error", format!("{e:#}")),
                Err(_) => ("error", "internal error (panic)".to_string()),
            };
            if !error.is_empty() {
                task.log(format!("TASK ERROR: {error}"));
            }
            task.log(format!("TASK {}", status.to_uppercase()));
            let info = {
                let mut i = task.info.lock().unwrap();
                i.end = Some(crate::containers::unix_now());
                i.status = status.into();
                i.error = error;
                i.clone()
            };
            me.record(&info);
            me.running.lock().unwrap().remove(&id2);
        });
        Ok(id)
    }

    fn record(&self, info: &Info) {
        let p = self.dir.join("index.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{}", serde_json::to_string(info).unwrap_or_default());
        }
        // trim now and then
        if std::fs::metadata(&p).is_ok_and(|m| m.len() > 2_000_000) {
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let keep = &lines[lines.len().saturating_sub(2000)..];
            let gone: Vec<&str> = lines[..lines.len().saturating_sub(2000)].to_vec();
            let _ = std::fs::write(&p, keep.join("\n") + "\n");
            for l in gone {
                if let Ok(i) = serde_json::from_str::<Info>(l) {
                    let _ = std::fs::remove_file(self.dir.join(format!("{}.log", i.id)));
                }
            }
        }
    }

    /// Running tasks first, then finished ones, newest first.
    pub fn list(&self, limit: usize) -> Vec<Info> {
        let mut out: Vec<Info> = self.running.lock().unwrap().values().map(|t| t.info.lock().unwrap().clone()).collect();
        out.sort_by(|a, b| b.start.cmp(&a.start));
        let text = std::fs::read_to_string(self.dir.join("index.jsonl")).unwrap_or_default();
        out.extend(text.lines().rev().filter_map(|l| serde_json::from_str::<Info>(l).ok()).take(limit));
        out.truncate(limit);
        out
    }

    pub fn get(&self, id: &str) -> Option<Info> {
        if let Some(t) = self.running.lock().unwrap().get(id) {
            return Some(t.info.lock().unwrap().clone());
        }
        let text = std::fs::read_to_string(self.dir.join("index.jsonl")).ok()?;
        text.lines().rev().filter_map(|l| serde_json::from_str::<Info>(l).ok()).find(|i| i.id == id)
    }

    /// Log lines from line `since` on.
    pub fn log(&self, id: &str, since: usize) -> Vec<String> {
        if !valid_id(id) {
            return vec![];
        }
        std::fs::read_to_string(self.dir.join(format!("{id}.log"))).unwrap_or_default().lines().skip(since).take(5000).map(String::from).collect()
    }

    pub fn stop(&self, id: &str) -> bool {
        match self.running.lock().unwrap().get(id) {
            Some(t) => {
                t.cancel.store(true, Ordering::Relaxed);
                t.log("stop requested");
                true
            }
            None => false,
        }
    }

    pub fn busy(&self, kind: &str, target: &str) -> bool {
        self.running.lock().unwrap().values().any(|t| {
            let i = t.info.lock().unwrap();
            i.kind == kind && i.target == target
        })
    }
}
