//! When each part of a container's log was written, without changing the log itself: every
//! sample (2 s) og-hostd notes the log's size, appending (time, size) to `log.idx` next to it
//! when it grew. A line's time is the first observation that covers it (at most ~2 s late).
//! Content that was there before the first observation gets the container's start time.
//! This is what the Docker API's `since=` and `timestamps=1` use.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// (unix time in ns, log size in bytes) per record, little-endian.
const REC: usize = 16;

static LAST: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);

fn idx_path(log: &Path) -> PathBuf {
    log.with_extension("idx")
}

pub fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Note the log's current size (called for every running container every sample).
/// `started_ns` dates whatever was in the log before og-hostd first looked.
pub fn observe(log: &Path, started_ns: u64) {
    let Ok(size) = std::fs::metadata(log).map(|m| m.len()) else { return };
    let mut last = LAST.lock().unwrap();
    let last = last.get_or_insert_with(HashMap::new);
    let prev = match last.get(log) {
        Some(p) => *p,
        None => read(log).last().map(|r| r.1).unwrap_or(u64::MAX),
    };
    if prev == size {
        last.insert(log.to_path_buf(), size);
        return;
    }
    let mut recs = Vec::new();
    if prev == u64::MAX || size < prev {
        // first look (or the log was replaced): older content dates from the start
        let _ = std::fs::remove_file(idx_path(log));
        recs.push((started_ns, size));
    } else {
        recs.push((now_ns(), size));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(idx_path(log)) {
        let mut buf = Vec::with_capacity(REC * recs.len());
        for (t, s) in recs {
            buf.extend_from_slice(&t.to_le_bytes());
            buf.extend_from_slice(&s.to_le_bytes());
        }
        let _ = f.write_all(&buf);
    }
    last.insert(log.to_path_buf(), size);
}

/// Forget containers that are gone.
pub fn retain(logs: &std::collections::HashSet<PathBuf>) {
    if let Some(m) = LAST.lock().unwrap().as_mut() {
        m.retain(|k, _| logs.contains(k));
    }
}

/// All records: (time ns, size), in order.
pub fn read(log: &Path) -> Vec<(u64, u64)> {
    let b = std::fs::read(idx_path(log)).unwrap_or_default();
    b.chunks_exact(REC).map(|c| (u64::from_le_bytes(c[..8].try_into().unwrap()), u64::from_le_bytes(c[8..].try_into().unwrap()))).collect()
}

/// Time (ns) of the byte at `offset`: the first record whose size covers it; past the last
/// record (written in the last couple of seconds) it's `now`.
pub fn time_at(recs: &[(u64, u64)], offset: u64, now: u64) -> u64 {
    let i = recs.partition_point(|r| r.1 <= offset);
    recs.get(i).map(|r| r.0).unwrap_or(now)
}

/// RFC 3339 with nanoseconds, UTC (Docker's `timestamps=1` prefix).
pub fn rfc3339(ns: u64) -> String {
    let secs = (ns / 1_000_000_000) as i64;
    let (days, sod) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil from days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:09}Z", sod / 3600, sod % 3600 / 60, sod % 60, ns % 1_000_000_000)
}

/// Docker's `since` / `until`: unix seconds with an optional fraction, or RFC 3339.
pub fn parse_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(f) = s.parse::<f64>() {
        return Some((f * 1e9) as u64);
    }
    // YYYY-MM-DDTHH:MM:SS[.frac]Z
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let frac = s[19..].strip_prefix('.').map(|f| f.trim_end_matches(|c: char| !c.is_ascii_digit())).unwrap_or("");
    let frac_ns = format!("{:0<9}", &frac[..frac.len().min(9)]).parse::<u64>().unwrap_or(0);
    let (yy, mm) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86400 + h * 3600 + mi * 60 + se) as u64) * 1_000_000_000 + frac_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000000000Z");
        let t = parse_time("2026-10-23T08:21:05.123456789Z").unwrap();
        assert_eq!(rfc3339(t), "2026-10-23T08:21:05.123456789Z");
        assert_eq!(parse_time("1792758076.5"), Some(1_792_758_076_500_000_000));
        let recs = [(100, 10), (200, 25)];
        assert_eq!(time_at(&recs, 0, 999), 100);
        assert_eq!(time_at(&recs, 10, 999), 200);
        assert_eq!(time_at(&recs, 24, 999), 200);
        assert_eq!(time_at(&recs, 25, 999), 999);
    }

    #[test]
    fn observe_index() {
        let d = std::env::temp_dir().join(format!("oglogidx-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let log = d.join("log");
        std::fs::write(&log, "old line\n").unwrap();
        observe(&log, 5);
        std::fs::write(&log, "old line\nnew line\n").unwrap();
        observe(&log, 5);
        let r = read(&log);
        assert_eq!(r[0], (5, 9));
        assert_eq!(r[1].1, 18);
        assert!(r[1].0 > 5);
        let _ = std::fs::remove_dir_all(d);
    }
}
