//! Log store (OmegaEnv's layout): `<dir>/<host>/<kind>/<source>/<YYYY-MM-DD>.log`, one line
//! per entry `"<ts ms>\t<e|w|i>\t<message>"`, plus in-memory recent lines and per-minute
//! counts per source. Kept `retain_days` and under 50 GB.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

const RECENT: usize = 600;
const MINUTES: i64 = 60;
const MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct Line {
    pub ts: i64,
    pub level: &'static str,
    pub msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Default)]
struct Source {
    host: String,
    kind: String,
    source: String,
    last_ts: i64,
    recent: std::collections::VecDeque<(i64, &'static str, String)>,
    mins: HashMap<i64, [u32; 3]>,
    last_error: Option<(i64, String)>,
}

pub struct Logs {
    dir: PathBuf,
    retain_days: u64,
    sources: Mutex<HashMap<(String, String, String), Source>>,
    files: Mutex<HashMap<PathBuf, (std::fs::File, Instant)>>,
    disk_cache: Mutex<(Instant, Vec<(String, String, String)>)>,
    total: Mutex<u64>,
}

fn safe(s: &str) -> String {
    let mut out = String::new();
    let mut last_us = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '@' || c == '-' {
            out.push(c);
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    let out: String = out.chars().take(120).collect();
    if out.is_empty() { "_".into() } else { out }
}

fn level_char(l: &str) -> char {
    match l {
        "error" => 'e',
        "warn" => 'w',
        _ => 'i',
    }
}

fn level_of(c: &str) -> &'static str {
    match c {
        "e" => "error",
        "w" => "warn",
        _ => "info",
    }
}

fn norm_level(l: &str) -> &'static str {
    match l {
        "error" => "error",
        "warn" => "warn",
        _ => "info",
    }
}

fn level_ok(lv: &str, want: &str) -> bool {
    want.is_empty() || want == "all" || if want == "warn" { lv != "info" } else { lv == want }
}

/// Local date of a ms timestamp (YYYY-MM-DD), as the files are named.
pub fn day(ts: i64) -> String {
    let t = (ts / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

fn parse(line: &str) -> Option<(i64, &'static str, &str)> {
    let mut it = line.splitn(3, '\t');
    let ts = it.next()?.parse().ok()?;
    let lv = level_of(it.next()?);
    Some((ts, lv, it.next()?))
}

/// Guess a level from a raw log line (for sources that don't tag one).
pub fn guess_level(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    let word = |w: &str| m.match_indices(w).any(|(i, _)| (i == 0 || !m.as_bytes()[i - 1].is_ascii_alphanumeric()) && m.as_bytes().get(i + w.len()).is_none_or(|c| !c.is_ascii_alphanumeric()));
    if ["error", "fatal", "panic", "exception", "critical", "crit", "failed"].iter().any(|w| word(w)) {
        "error"
    } else if ["warn", "warning"].iter().any(|w| word(w)) {
        "warn"
    } else {
        "info"
    }
}

impl Logs {
    pub fn new(dir: PathBuf, retain_days: u64) -> Logs {
        let _ = std::fs::create_dir_all(&dir);
        Logs { dir, retain_days, sources: Default::default(), files: Default::default(), disk_cache: Mutex::new((Instant::now() - std::time::Duration::from_secs(3600), vec![])), total: Mutex::new(0) }
    }

    fn file_of(&self, host: &str, kind: &str, source: &str, ts: i64) -> PathBuf {
        self.dir.join(safe(host)).join(safe(kind)).join(safe(source)).join(format!("{}.log", day(ts)))
    }

    /// `lines`: `[ts, kind, source, level, msg]` arrays, as omega-agent sends them.
    pub fn ingest(&self, host: &str, lines: &[Value]) {
        let mut by_file: HashMap<PathBuf, String> = HashMap::new();
        {
            let mut sources = self.sources.lock().unwrap();
            for l in lines {
                let (Some(ts), Some(kind), Some(source), Some(msg)) = (l[0].as_f64(), l[1].as_str(), l[2].as_str(), l[4].as_str()) else { continue };
                if source.is_empty() {
                    continue;
                }
                let ts = ts as i64;
                let lv = norm_level(l[3].as_str().unwrap_or("info"));
                let text = msg.replace("\r\n", " ⏎ ").replace(['\r', '\n'], " ⏎ ");
                let s = sources.entry((host.to_string(), kind.to_string(), source.to_string())).or_insert_with(|| Source { host: host.into(), kind: kind.into(), source: source.into(), ..Default::default() });
                s.last_ts = s.last_ts.max(ts);
                s.recent.push_back((ts, lv, text.clone()));
                while s.recent.len() > RECENT {
                    s.recent.pop_front();
                }
                let c = s.mins.entry(ts / 60_000).or_default();
                c[0] += 1;
                if lv == "warn" {
                    c[1] += 1;
                }
                if lv == "error" {
                    c[2] += 1;
                    s.last_error = Some((ts, text.clone()));
                }
                let f = self.file_of(host, kind, source, ts);
                let e = by_file.entry(f).or_default();
                e.push_str(&format!("{ts}\t{}\t{text}\n", level_char(lv)));
            }
            let cutoff = crate::now_ms() as i64 / 60_000 - MINUTES;
            for s in sources.values_mut() {
                s.mins.retain(|m, _| *m >= cutoff);
            }
        }
        let mut files = self.files.lock().unwrap();
        for (path, rows) in by_file {
            if !files.contains_key(&path) {
                if !path.exists() {
                    self.disk_cache.lock().unwrap().0 = Instant::now() - std::time::Duration::from_secs(3600);
                }
                if let Some(p) = path.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    Ok(f) => {
                        files.insert(path.clone(), (f, Instant::now()));
                    }
                    Err(e) => {
                        eprintln!("og-lab: log write {}: {e}", path.display());
                        continue;
                    }
                }
            }
            if let Some((f, used)) = files.get_mut(&path) {
                let _ = f.write_all(rows.as_bytes());
                *used = Instant::now();
            }
        }
    }

    fn counts(s: &Source, minutes: i64) -> (u32, u32, u32) {
        let from = crate::now_ms() as i64 / 60_000 - minutes;
        s.mins.iter().filter(|(m, _)| **m >= from).fold((0, 0, 0), |a, (_, c)| (a.0 + c[0], a.1 + c[1], a.2 + c[2]))
    }

    fn disk_sources(&self) -> Vec<(String, String, String)> {
        let mut c = self.disk_cache.lock().unwrap();
        if c.0.elapsed().as_secs() < 30 {
            return c.1.clone();
        }
        let mut list = Vec::new();
        let rd = |p: &Path| -> Vec<String> { std::fs::read_dir(p).map(|r| r.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default() };
        for h in rd(&self.dir) {
            for k in rd(&self.dir.join(&h)) {
                for s in rd(&self.dir.join(&h).join(&k)) {
                    list.push((h.clone(), k.clone(), s));
                }
            }
        }
        *c = (Instant::now(), list.clone());
        list
    }

    pub fn list_sources(&self) -> Vec<Value> {
        let mut out: HashMap<(String, String, String), Value> = HashMap::new();
        for (h, k, s) in self.disk_sources() {
            out.insert((h.clone(), k.clone(), s.clone()), json!({"host": h, "kind": k, "source": s, "lastTs": 0, "lines": 0, "warn": 0, "error": 0}));
        }
        for s in self.sources.lock().unwrap().values() {
            let (n, w, e) = Self::counts(s, 60);
            out.insert(
                (s.host.clone(), s.kind.clone(), safe(&s.source)),
                json!({"host": s.host, "kind": s.kind, "source": s.source, "lastTs": s.last_ts, "lines": n, "warn": w, "error": e,
                       "lastError": s.last_error.as_ref().map(|(ts, m)| json!({"ts": ts, "msg": m}))}),
            );
        }
        out.into_values().collect()
    }

    fn read_tail_lines(path: &Path, max: u64) -> Vec<String> {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = size.saturating_sub(max);
        let _ = f.seek(SeekFrom::Start(start));
        let mut buf = Vec::new();
        let _ = f.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        if start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        lines
    }

    /// Newest-last lines of one source.
    #[allow(clippy::too_many_arguments)]
    pub fn tail(&self, host: &str, kind: &str, source: &str, limit: usize, after: Option<i64>, before: Option<i64>, level: &str, q: &str) -> Vec<Line> {
        let limit = limit.clamp(1, 2000);
        let q = q.to_lowercase();
        let matches = |lv: &str, m: &str| level_ok(lv, level) && (q.is_empty() || m.to_lowercase().contains(&q));
        let mk = |ts, lv, m: &str| Line { ts, level: lv, msg: m.to_string(), host: None, kind: None, source: None };
        let mem: Vec<Line> = self
            .sources
            .lock()
            .unwrap()
            .get(&(host.into(), kind.into(), source.into()))
            .map(|s| s.recent.iter().filter(|(ts, lv, m)| after.is_none_or(|a| *ts > a) && before.is_none_or(|b| *ts < b) && matches(lv, m)).map(|(ts, lv, m)| mk(*ts, *lv, m)).collect())
            .unwrap_or_default();
        if after.is_some() || mem.len() >= limit {
            let n = mem.len();
            return mem.into_iter().skip(n.saturating_sub(limit)).collect();
        }
        let dir = self.dir.join(safe(host)).join(safe(kind)).join(safe(source));
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "log")).collect()).unwrap_or_default();
        files.sort();
        files.reverse();
        let mut out: Vec<Line> = Vec::new();
        for f in files.iter().take(self.retain_days as usize) {
            let rows: Vec<Line> = Self::read_tail_lines(f, 16 * 1024 * 1024).iter().filter_map(|l| parse(l)).filter(|(ts, lv, m)| before.is_none_or(|b| *ts < b) && matches(lv, m)).map(|(ts, lv, m)| mk(ts, lv, m)).collect();
            out.splice(0..0, rows);
            if out.len() >= limit {
                break;
            }
        }
        let n = out.len();
        out.into_iter().skip(n.saturating_sub(limit)).collect()
    }

    /// Across every source, newest first; gives up after 6 s to stay responsive.
    #[allow(clippy::too_many_arguments)]
    pub fn search(&self, q: &str, host: Option<&str>, level: &str, hours: f64, limit: usize, from: Option<i64>, to: Option<i64>) -> (Vec<Line>, bool) {
        let q = q.to_lowercase();
        let limit = limit.clamp(1, 1000);
        let until = to.filter(|t| *t > 0).unwrap_or(crate::now_ms() as i64);
        let hours = if hours > 0.0 { hours.min(24.0 * self.retain_days as f64) } else { 24.0 };
        let since = from.filter(|f| *f > 0).unwrap_or(until - (hours * 3_600_000.0) as i64);
        let mut days = Vec::new();
        let mut t = since;
        while t <= until + 86_400_000 {
            let d = day(t);
            if !days.contains(&d) {
                days.push(d);
            }
            t += 86_400_000;
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(6);
        let mut hits: Vec<Line> = Vec::new();
        let mut truncated = false;
        'outer: for (h, k, s) in self.disk_sources() {
            if host.is_some_and(|x| !x.is_empty() && safe(x) != h) {
                continue;
            }
            for d in &days {
                let p = self.dir.join(&h).join(&k).join(&s).join(format!("{d}.log"));
                let Ok(f) = std::fs::File::open(&p) else { continue };
                for line in std::io::BufReader::new(f).lines().map_while(|l| l.ok()) {
                    if let Some((ts, lv, m)) = parse(&line) {
                        if ts >= since && ts <= until && level_ok(lv, level) && (q.is_empty() || m.to_lowercase().contains(&q)) {
                            hits.push(Line { ts, level: lv, msg: m.to_string(), host: Some(h.clone()), kind: Some(k.clone()), source: Some(s.clone()) });
                            if hits.len() > limit * 20 {
                                hits.sort_by(|a, b| b.ts.cmp(&a.ts));
                                hits.truncate(limit);
                            }
                        }
                    }
                }
                if Instant::now() > deadline {
                    truncated = true;
                    break 'outer;
                }
            }
        }
        hits.sort_by(|a, b| b.ts.cmp(&a.ts));
        hits.truncate(limit);
        (hits, truncated)
    }

    /// Sources that logged at least `threshold` errors in the last `minutes`.
    pub fn noisy(&self, minutes: i64, threshold: u32) -> Vec<Value> {
        let mut out: Vec<(u32, Value)> = self
            .sources
            .lock()
            .unwrap()
            .values()
            .filter_map(|s| {
                let (_, _, e) = Self::counts(s, minutes);
                (e >= threshold).then(|| (e, json!({"host": s.host, "kind": s.kind, "source": s.source, "errors": e, "lastError": s.last_error.as_ref().map(|(ts, m)| json!({"ts": ts, "msg": m}))})))
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        out.into_iter().map(|x| x.1).collect()
    }

    /// Newest in-memory lines of one host/source within `minutes` containing `needle`.
    pub fn recent_matching(&self, host: &str, source: &str, needle: &str, minutes: i64) -> Option<String> {
        let since = crate::now_ms() as i64 - minutes * 60_000;
        let sources = self.sources.lock().unwrap();
        let mut best: Option<(i64, String)> = None;
        for s in sources.values().filter(|s| s.host == host && s.source == source) {
            for (ts, _, m) in s.recent.iter().filter(|(ts, _, m)| *ts >= since && m.contains(needle)) {
                if best.as_ref().is_none_or(|b| *ts > b.0) {
                    best = Some((*ts, m.clone()));
                }
            }
        }
        best.map(|b| b.1)
    }

    pub fn stats(&self) -> Value {
        json!({"bytes": *self.total.lock().unwrap(), "retainDays": self.retain_days})
    }

    fn sweep(&self) {
        self.files.lock().unwrap().retain(|_, (_, used)| used.elapsed().as_secs() < 120);
        let cutoff = day(crate::now_ms() as i64 - self.retain_days as i64 * 86_400_000);
        let mut files = Vec::new();
        for (h, k, s) in self.disk_sources() {
            let p = self.dir.join(&h).join(&k).join(&s);
            for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.len() >= 10 && name[..10] < *cutoff {
                    let _ = std::fs::remove_file(e.path());
                    continue;
                }
                if let Ok(m) = e.metadata() {
                    files.push((e.path(), m.len(), name));
                }
            }
        }
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by(|a, b| a.2.cmp(&b.2));
        for f in files {
            if total <= MAX_BYTES {
                break;
            }
            let _ = std::fs::remove_file(&f.0);
            total -= f.1;
        }
        *self.total.lock().unwrap() = total;
    }
}

pub fn start(l: Arc<Logs>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        loop {
            let a = l.clone();
            let _ = tokio::task::spawn_blocking(move || a.sweep()).await;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_tail_search_noisy() {
        let d = std::env::temp_dir().join(format!("oglab-l-{}", std::process::id()));
        let l = Logs::new(d.clone(), 14);
        let now = crate::now_ms() as i64;
        let lines: Vec<Value> = (0..12).map(|i| json!([now - 1000 + i, "container", "web", if i % 2 == 0 { "error" } else { "info" }, format!("line {i}\nmore")])).collect();
        l.ingest("go", &lines);
        let t = l.tail("go", "container", "web", 5, None, None, "all", "");
        assert_eq!(t.len(), 5);
        assert!(t[4].msg.contains("line 11 ⏎ more"));
        assert_eq!(l.tail("go", "container", "web", 100, None, None, "error", "").len(), 6);
        drop(l);
        // a fresh store reads the files back
        let l2 = Logs::new(d.clone(), 14);
        let (hits, _) = l2.search("line 3", None, "all", 1.0, 10, None, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source.as_deref(), Some("web"));
        assert_eq!(l2.tail("go", "container", "web", 100, None, None, "all", "").len(), 12);
        let l3 = Logs::new(d.clone(), 14);
        l3.ingest("go", &lines);
        assert_eq!(l3.noisy(15, 5)[0]["errors"], 6);
        assert_eq!(guess_level("ERROR: boom"), "error");
        assert_eq!(guess_level("all good, no errors"), "info");
        assert_eq!(guess_level("[warn] disk"), "warn");
        let _ = std::fs::remove_dir_all(d);
    }
}
