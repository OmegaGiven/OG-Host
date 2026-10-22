//! Schedules for jobs, in local time: `hourly`, `daily`, `weekly`, `monthly`, `02:30`,
//! `daily 02:30`, `sun 03:30`, `mon..fri 01:00`, `mon,wed,fri 22:15`, `every 6h`, or a
//! five-field cron line (`30 2 * * 1-5`).

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    min: u64,
    hour: u32,
    dom: u32,
    month: u16,
    dow: u8,
}

const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn field(s: &str, lo: u32, hi: u32, names: &[&str]) -> Result<u64> {
    let mut bits = 0u64;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().ok().filter(|v| *v > 0).with_context(|| format!("bad step in {part:?}"))?),
            None => (part, 1),
        };
        let val = |v: &str| -> Result<u32> {
            if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(v)) {
                return Ok(i as u32 + lo);
            }
            let n: u32 = v.parse().with_context(|| format!("bad value {v:?}"))?;
            // cron allows 7 for Sunday
            Ok(if names.len() == 7 && n == 7 { 0 } else { n })
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-').or_else(|| range.split_once("..")) {
            (val(a)?, val(b)?)
        } else {
            let v = val(range)?;
            (v, if part.contains('/') { hi } else { v })
        };
        if a < lo || b > hi || a > b {
            bail!("{part:?} is out of range {lo}-{hi}");
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok(bits)
}

impl Schedule {
    pub fn parse(s: &str) -> Result<Schedule> {
        let s = s.trim().to_ascii_lowercase();
        let all = |lo: u32, hi: u32| -> u64 { (lo..=hi).fold(0, |a, v| a | 1 << v) };
        let at = |t: &str| -> Result<(u64, u32)> {
            let (h, m) = t.split_once(':').context("times look like 02:30")?;
            let (h, m): (u32, u32) = (h.parse().context("bad hour")?, m.parse().context("bad minute")?);
            if h > 23 || m > 59 {
                bail!("bad time {t}");
            }
            Ok((1 << m, 1 << h))
        };
        let mk = |min: u64, hour: u64, dom: u64, month: u64, dow: u64| Schedule { min, hour: hour as u32, dom: dom as u32, month: month as u16, dow: dow as u8 };
        let words: Vec<&str> = s.split_whitespace().collect();
        Ok(match words.as_slice() {
            [] => bail!("empty schedule"),
            ["hourly"] => mk(1, all(0, 23), all(1, 31), all(1, 12), all(0, 6)),
            ["daily"] => mk(1, 1, all(1, 31), all(1, 12), all(0, 6)),
            ["weekly"] => mk(1, 1, all(1, 31), all(1, 12), 1),
            ["monthly"] => mk(1, 1, 1 << 1, all(1, 12), all(0, 6)),
            ["every", n] => {
                let h: u32 = n.strip_suffix('h').and_then(|v| v.parse().ok()).filter(|v| (1..=12).contains(v)).context("use e.g. every 6h")?;
                mk(1, field(&format!("*/{h}"), 0, 23, &[])?, all(1, 31), all(1, 12), all(0, 6))
            }
            [t] if t.contains(':') => {
                let (m, h) = at(t)?;
                mk(m, h as u64, all(1, 31), all(1, 12), all(0, 6))
            }
            ["daily", t] => {
                let (m, h) = at(t)?;
                mk(m, h as u64, all(1, 31), all(1, 12), all(0, 6))
            }
            [days, t] => {
                let (m, h) = at(t)?;
                mk(m, h as u64, all(1, 31), all(1, 12), field(days, 0, 6, &DAYS)?)
            }
            [m, h, dom, mon, dow] => mk(field(m, 0, 59, &[])?, field(h, 0, 23, &[])?, field(dom, 1, 31, &[])?, field(mon, 1, 12, &[])?, field(dow, 0, 6, &DAYS)?),
            _ => bail!("unknown schedule {s:?}: try daily 02:00, sun 03:30, every 6h or a cron line"),
        })
    }

    fn matches(&self, tm: &libc::tm) -> bool {
        self.min & (1 << tm.tm_min) != 0 && self.hour & (1 << tm.tm_hour) != 0 && self.dom & (1 << tm.tm_mday) != 0 && self.month & (1 << (tm.tm_mon + 1)) != 0 && self.dow & (1 << tm.tm_wday) != 0
    }

    /// The first matching minute after `t` (unix seconds), within a year.
    pub fn next_after(&self, t: u64) -> Option<u64> {
        let mut m = t / 60 * 60 + 60;
        let end = t + 366 * 86400;
        while m < end {
            let tm = local(m);
            if self.matches(&tm) {
                return Some(m);
            }
            // skip whole hours / days that can't match
            if self.hour & (1 << tm.tm_hour) == 0 || self.dom & (1 << tm.tm_mday) == 0 || self.dow & (1 << tm.tm_wday) == 0 || self.month & (1 << (tm.tm_mon + 1)) == 0 {
                m += (60 - tm.tm_min as u64) * 60;
            } else {
                m += 60;
            }
        }
        None
    }
}

fn local(t: u64) -> libc::tm {
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    tm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_finds_next() {
        for s in ["hourly", "daily", "weekly", "monthly", "02:30", "daily 02:30", "sun 03:30", "mon..fri 01:00", "mon,wed,fri 22:15", "every 6h", "30 2 * * 1-5", "*/15 * * * *"] {
            Schedule::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        }
        for s in ["", "25:00", "every 0h", "1 2 3", "60 * * * *", "funday 02:00"] {
            assert!(Schedule::parse(s).is_err(), "{s}");
        }
        let s = Schedule::parse("every 6h").unwrap();
        let n = s.next_after(1_792_627_200 + 60).unwrap(); // just after midnight UTC-ish
        assert_eq!(n % 3600, 0);
        assert!(n - 1_792_627_200 <= 6 * 3600);
        let w = Schedule::parse("sun 03:30").unwrap();
        let n = w.next_after(1_792_627_200).unwrap();
        let tm = local(n);
        assert_eq!((tm.tm_wday, tm.tm_hour, tm.tm_min), (0, 3, 30));
    }
}
