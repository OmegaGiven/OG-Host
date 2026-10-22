//! Retention: which backups of a group to keep (the Proxmox Backup Server rules). Newest
//! first: keep the last N, then the newest backup of each of the last N days / ISO weeks /
//! months / years that have one. Protected backups are always kept. Times are local time.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Keep {
    pub last: u32,
    pub hourly: u32,
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
    pub yearly: u32,
}

impl Keep {
    pub fn is_empty(&self) -> bool {
        *self == Keep::default()
    }
}

fn local(t: u64) -> libc::tm {
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    tm
}

fn iso_week(tm: &libc::tm) -> (i32, i32) {
    // ISO 8601: the week with the year's first Thursday is week 1
    let wday = (tm.tm_wday + 6) % 7; // Monday = 0
    let thursday_yday = tm.tm_yday - wday + 3;
    let year = tm.tm_year + 1900;
    let days_in = |y: i32| if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 366 } else { 365 };
    if thursday_yday < 0 {
        let py = year - 1;
        let t = thursday_yday + days_in(py);
        (py, t / 7 + 1)
    } else if thursday_yday >= days_in(year) {
        (year + 1, 1)
    } else {
        (year, thursday_yday / 7 + 1)
    }
}

/// For snapshots given as (time, protected), newest first or not: which to keep, in the
/// same order as given. An empty `Keep` keeps everything.
pub fn plan(snaps: &[(u64, bool)], keep: &Keep) -> Vec<bool> {
    let mut out = vec![false; snaps.len()];
    if keep.is_empty() {
        return vec![true; snaps.len()];
    }
    let mut order: Vec<usize> = (0..snaps.len()).collect();
    order.sort_by(|a, b| snaps[*b].0.cmp(&snaps[*a].0));
    for (n, &i) in order.iter().enumerate() {
        if (n as u32) < keep.last || snaps[i].1 {
            out[i] = true;
        }
    }
    type Period = fn(&libc::tm) -> (i32, i32, i32, i32);
    let rules: [(u32, Period); 5] = [
        (keep.hourly, |t| (t.tm_year, t.tm_yday, t.tm_hour, 0)),
        (keep.daily, |t| (t.tm_year, t.tm_yday, 0, 0)),
        (keep.weekly, |t| {
            let (y, w) = iso_week(t);
            (y, w, 0, 0)
        }),
        (keep.monthly, |t| (t.tm_year, t.tm_mon, 0, 0)),
        (keep.yearly, |t| (t.tm_year, 0, 0, 0)),
    ];
    for (count, period) in rules {
        if count == 0 {
            continue;
        }
        let mut seen = std::collections::HashSet::new();
        for &i in &order {
            let p = period(&local(snaps[i].0));
            if seen.contains(&p) {
                continue;
            }
            if seen.len() as u32 >= count {
                break;
            }
            seen.insert(p);
            out[i] = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_last_daily_and_protected() {
        unsafe { libc::setenv(c"TZ".as_ptr(), c"UTC".as_ptr(), 1) };
        unsafe { libc_tzset() };
        let day = 86_400;
        let base = 1_792_627_200; // 2026-10-22 00:00 UTC
        // two backups a day for 10 days
        let snaps: Vec<(u64, bool)> = (0..20).map(|i| (base + (i / 2) * day + (i % 2) * 3600, i == 0)).collect();
        let k = plan(&snaps, &Keep { last: 2, daily: 3, ..Default::default() });
        let kept: Vec<u64> = snaps.iter().zip(&k).filter(|(_, k)| **k).map(|(s, _)| (s.0 - base) / 3600).collect();
        // last 2 = day 9 (both), daily 3 = newest of days 9, 8, 7; protected = first
        assert_eq!(kept, vec![0, 7 * 24 + 1, 8 * 24 + 1, 9 * 24, 9 * 24 + 1]);
        assert!(plan(&snaps, &Keep::default()).iter().all(|k| *k));
    }

    extern "C" {
        #[link_name = "tzset"]
        fn libc_tzset();
    }
}
