//! Metric history (OmegaEnv's metrics.db, same schema): raw samples for 48 h, 1-minute
//! rollups for 30 days, 1-hour rollups for a year. Writes are buffered and flushed every 10 s.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;
const KEEP_RAW: i64 = 2 * DAY;
const KEEP_M1: i64 = 30 * DAY;
const KEEP_H1: i64 = 365 * DAY;

/// What each metric is, for pickers and axis formatting.
pub const METRICS: &[(&str, &str, &str)] = &[
    ("cpu", "CPU", "pct"), ("load1", "Load (1 min)", "num"), ("mem_used_pct", "Memory used", "pct"), ("mem_used", "Memory used (bytes)", "bytes"),
    ("swap_used_pct", "Swap used", "pct"), ("temp", "CPU temperature", "C"), ("net_rx", "Network in", "Bps"), ("net_tx", "Network out", "Bps"),
    ("gpu_util", "GPU utilisation", "pct"), ("gpu_mem_used", "GPU memory used", "bytes"), ("gpu_temp", "GPU temperature", "C"), ("gpu_power", "GPU power", "W"),
    ("disk_used_pct", "Disk used", "pct"), ("disk_free", "Disk free", "bytes"), ("ctr_cpu", "Container CPU", "pct"), ("ctr_mem", "Container memory", "bytes"),
    ("ctr_running", "Containers running", "num"), ("guest_cpu", "VM/LXC CPU", "pct"), ("guest_mem", "VM/LXC memory", "bytes"),
    ("app_up", "App up (1) / down (0)", "bool"), ("app_ms", "App response time", "ms"), ("log_errors", "Log errors per minute", "num"),
    ("watch_price", "Watched price", "usd"), ("watch_in_stock", "Watched item in stock", "bool"),
];

pub fn metrics_json() -> Value {
    json!(METRICS.iter().map(|(k, t, u)| (k.to_string(), json!({"title": t, "unit": u}))).collect::<serde_json::Map<_, _>>())
}

fn meta(metric: &str) -> (&'static str, &'static str) {
    METRICS.iter().find(|m| m.0 == metric).map(|m| (m.1, m.2)).unwrap_or(("", "num"))
}

pub struct Metrics {
    db: Mutex<Connection>,
    buffer: Mutex<Vec<(String, String, String, i64, f64)>>,
    sids: Mutex<std::collections::HashMap<(String, String, String), i64>>,
}

#[derive(Debug, Serialize)]
pub struct Series {
    pub id: i64,
    pub host: String,
    pub metric: String,
    pub label: String,
    pub last_ts: Option<i64>,
    pub title: String,
    pub unit: String,
}

impl Metrics {
    pub fn open(path: &Path) -> Result<Metrics> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "TRUNCATE")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS series (id INTEGER PRIMARY KEY, host TEXT NOT NULL, metric TEXT NOT NULL, label TEXT NOT NULL DEFAULT '', last_ts INTEGER, UNIQUE(host, metric, label));
             CREATE TABLE IF NOT EXISTS raw (sid INTEGER, ts INTEGER, v REAL, PRIMARY KEY (sid, ts)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS m1 (sid INTEGER, ts INTEGER, avg REAL, min REAL, max REAL, PRIMARY KEY (sid, ts)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS h1 (sid INTEGER, ts INTEGER, avg REAL, min REAL, max REAL, PRIMARY KEY (sid, ts)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v INTEGER);",
        )?;
        let schema: Option<i64> = db.query_row("SELECT v FROM meta WHERE k='schema'", [], |r| r.get(0)).optional()?;
        if schema.unwrap_or(1) < 2 {
            db.execute_batch("DELETE FROM m1; DELETE FROM h1; DELETE FROM meta; INSERT INTO meta (k, v) VALUES ('schema', 2)")?;
        }
        Ok(Metrics { db: Mutex::new(db), buffer: Mutex::new(Vec::new()), sids: Default::default() })
    }

    pub fn record(&self, host: &str, metric: &str, label: &str, ts: i64, v: Option<f64>) {
        let Some(v) = v.filter(|v| v.is_finite()) else { return };
        self.buffer.lock().unwrap().push((host.to_string(), metric.to_string(), label.to_string(), ts, v));
    }

    fn sid(&self, db: &Connection, host: &str, metric: &str, label: &str) -> rusqlite::Result<i64> {
        let key = (host.to_string(), metric.to_string(), label.to_string());
        if let Some(id) = self.sids.lock().unwrap().get(&key) {
            return Ok(*id);
        }
        let id = match db.query_row("SELECT id FROM series WHERE host=?1 AND metric=?2 AND label=?3", params![host, metric, label], |r| r.get(0)).optional()? {
            Some(id) => id,
            None => {
                db.execute("INSERT INTO series (host, metric, label) VALUES (?1, ?2, ?3)", params![host, metric, label])?;
                db.last_insert_rowid()
            }
        };
        self.sids.lock().unwrap().insert(key, id);
        Ok(id)
    }

    pub fn flush(&self) {
        let rows = std::mem::take(&mut *self.buffer.lock().unwrap());
        if rows.is_empty() {
            return;
        }
        let mut db = self.db.lock().unwrap();
        let r = (|| -> rusqlite::Result<()> {
            let tx = db.transaction()?;
            let mut last = std::collections::HashMap::new();
            for (h, m, l, ts, v) in &rows {
                let sid = self.sid(&tx, h, m, l)?;
                tx.execute("INSERT OR REPLACE INTO raw (sid, ts, v) VALUES (?1, ?2, ?3)", params![sid, ts, v])?;
                last.insert(sid, *ts);
            }
            for (sid, ts) in last {
                tx.execute("UPDATE series SET last_ts=?1 WHERE id=?2", params![ts, sid])?;
            }
            tx.commit()
        })();
        if let Err(e) = r {
            eprintln!("og-lab: metrics flush: {e}");
        }
    }

    /// One agent snapshot -> many series (OmegaEnv's mapping).
    pub fn ingest_snapshot(&self, snap: &Value, slow_fresh: bool) {
        let h = snap["host"].as_str().unwrap_or("");
        if h.is_empty() {
            return;
        }
        let ts = crate::now_ms() as i64;
        let r = |m: &str, l: &str, v: Option<f64>| self.record(h, m, l, ts, v);
        let f = |v: &Value| v.as_f64();
        r("cpu", "", f(&snap["cpu"]["pct"]));
        r("load1", "", f(&snap["cpu"]["load"][0]));
        if let (Some(total), Some(avail)) = (f(&snap["mem"]["total"]), f(&snap["mem"]["available"])) {
            if total > 0.0 {
                r("mem_used", "", Some(total - avail));
                r("mem_used_pct", "", Some(100.0 * (total - avail) / total));
                if let (Some(st), Some(sf)) = (f(&snap["mem"]["swapTotal"]), f(&snap["mem"]["swapFree"])) {
                    if st > 0.0 {
                        r("swap_used_pct", "", Some(100.0 * (st - sf) / st));
                    }
                }
            }
        }
        r("temp", "", f(&snap["temp"]));
        r("net_rx", "", f(&snap["net"]["rxBps"]));
        r("net_tx", "", f(&snap["net"]["txBps"]));
        let gpus = snap["gpus"].as_array().cloned().unwrap_or_default();
        for (i, g) in gpus.iter().enumerate() {
            let name = g["name"].as_str().unwrap_or("GPU");
            let l = if gpus.len() > 1 { format!("{i}: {name}") } else { name.to_string() };
            r("gpu_util", &l, f(&g["util"]));
            r("gpu_mem_used", &l, f(&g["memUsed"]));
            r("gpu_temp", &l, f(&g["temp"]));
            r("gpu_power", &l, f(&g["power"]));
        }
        for d in snap["disks"].as_array().into_iter().flatten() {
            let (Some(used), Some(total)) = (f(&d["used"]), f(&d["total"])) else { continue };
            let free = f(&d["avail"]).unwrap_or(total - used);
            let mount = d["mount"].as_str().unwrap_or("");
            if used + free > 0.0 {
                r("disk_used_pct", mount, Some(100.0 * used / (used + free)));
            }
            r("disk_free", mount, Some(free));
        }
        if slow_fresh {
            let cs = snap["containers"].as_array().cloned().unwrap_or_default();
            if snap["docker"].as_bool().unwrap_or(false) || !cs.is_empty() {
                r("ctr_running", "", Some(cs.iter().filter(|c| c["state"] == "running").count() as f64));
            }
            for c in cs.iter().filter(|c| c["state"] == "running") {
                let n = c["name"].as_str().unwrap_or("");
                r("ctr_cpu", n, f(&c["cpu"]));
                r("ctr_mem", n, f(&c["mem"]));
            }
            for g in snap["guests"].as_array().into_iter().flatten().filter(|g| g["status"] == "running") {
                let n = g["name"].as_str().unwrap_or("");
                r("guest_cpu", n, f(&g["cpu"]));
                r("guest_mem", n, f(&g["mem"]));
            }
        }
    }

    fn rollup(&self) {
        self.flush();
        let db = self.db.lock().unwrap();
        let now = crate::now_ms() as i64;
        let r = (|| -> rusqlite::Result<()> {
            let get = |k: &str| -> rusqlite::Result<Option<i64>> { db.query_row("SELECT v FROM meta WHERE k=?1", [k], |r| r.get(0)).optional() };
            let cur_min = now / MIN * MIN;
            let from_m = get("m1_done")?.unwrap_or(cur_min - KEEP_RAW);
            if cur_min > from_m {
                db.execute(
                    &format!("INSERT OR REPLACE INTO m1 (sid, ts, avg, min, max) SELECT sid, CAST(ts / {MIN} AS INTEGER) * {MIN} AS t, avg(v), min(v), max(v) FROM raw WHERE ts >= ?1 AND ts < ?2 GROUP BY sid, t"),
                    params![from_m, cur_min],
                )?;
                db.execute("INSERT OR REPLACE INTO meta (k, v) VALUES ('m1_done', ?1)", [cur_min])?;
            }
            let cur_hour = now / HOUR * HOUR;
            let from_h = get("h1_done")?.unwrap_or(cur_hour - KEEP_M1);
            if cur_hour > from_h {
                db.execute(
                    &format!("INSERT OR REPLACE INTO h1 (sid, ts, avg, min, max) SELECT sid, CAST(ts / {HOUR} AS INTEGER) * {HOUR} AS t, avg(avg), min(min), max(max) FROM m1 WHERE ts >= ?1 AND ts < ?2 GROUP BY sid, t"),
                    params![from_h, cur_hour],
                )?;
                db.execute("INSERT OR REPLACE INTO meta (k, v) VALUES ('h1_done', ?1)", [cur_hour])?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            eprintln!("og-lab: metrics rollup: {e}");
        }
    }

    fn prune(&self) {
        let db = self.db.lock().unwrap();
        let now = crate::now_ms() as i64;
        let _ = db.execute("DELETE FROM raw WHERE ts < ?1", [now - KEEP_RAW]);
        let _ = db.execute("DELETE FROM m1 WHERE ts < ?1", [now - KEEP_M1]);
        let _ = db.execute("DELETE FROM h1 WHERE ts < ?1", [now - KEEP_H1]);
    }

    pub fn list_series(&self, host: Option<&str>, metric: Option<&str>) -> Vec<Series> {
        let db = self.db.lock().unwrap();
        let mut q = "SELECT id, host, metric, label, last_ts FROM series WHERE 1=1".to_string();
        let mut args: Vec<String> = Vec::new();
        if let Some(h) = host.filter(|h| !h.is_empty()) {
            q.push_str(" AND host=?");
            args.push(h.into());
        }
        if let Some(m) = metric.filter(|m| !m.is_empty()) {
            q.push_str(" AND metric=?");
            args.push(m.into());
        }
        q.push_str(" ORDER BY host, metric, label");
        let Ok(mut st) = db.prepare(&q) else { return vec![] };
        let rows = st.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            let metric: String = r.get(2)?;
            let (title, unit) = meta(&metric);
            Ok(Series { id: r.get(0)?, host: r.get(1)?, label: r.get(3)?, last_ts: r.get(4)?, title: if title.is_empty() { metric.clone() } else { title.into() }, unit: unit.into(), metric })
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }

    /// The finest resolution that covers the range, bucketed to at most `max_points`.
    pub fn query(&self, host: &str, metric: &str, label: &str, from: Option<i64>, to: Option<i64>, max_points: i64) -> Value {
        self.flush();
        let to = to.filter(|t| *t > 0).unwrap_or(crate::now_ms() as i64);
        let from = from.filter(|f| *f > 0).unwrap_or(to - HOUR);
        let (title, unit) = meta(metric);
        let db = self.db.lock().unwrap();
        let id: Option<i64> = db.query_row("SELECT id FROM series WHERE host=?1 AND metric=?2 AND label=?3", params![host, metric, label], |r| r.get(0)).optional().ok().flatten();
        let empty = json!({"host": host, "metric": metric, "label": label, "title": title, "unit": unit, "resolution": null, "t": [], "avg": [], "min": [], "max": []});
        let Some(id) = id else { return empty };
        let now = crate::now_ms() as i64;
        let span = (to - from).max(1);
        let (table, res) = if from >= now - KEEP_RAW && span <= 12 * HOUR {
            ("raw", 10_000)
        } else if from >= now - KEEP_M1 && span <= 14 * DAY {
            ("m1", MIN)
        } else {
            ("h1", HOUR)
        };
        let max_points = max_points.clamp(10, 2000);
        let bucket = res.max(((span as f64 / max_points as f64 / res as f64).ceil() as i64) * res);
        let sel = |sql: &str, a: i64, b: i64| -> Vec<(i64, Option<f64>, Option<f64>, Option<f64>)> {
            let Ok(mut st) = db.prepare(sql) else { return vec![] };
            st.query_map(params![bucket, id, a, b], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|it| it.flatten().collect()).unwrap_or_default()
        };
        let mut rows = if table == "raw" {
            sel("SELECT CAST(ts / ?1 AS INTEGER) * ?1 AS t, avg(v), min(v), max(v) FROM raw WHERE sid=?2 AND ts>=?3 AND ts<=?4 GROUP BY t ORDER BY t", from, to)
        } else {
            sel(&format!("SELECT CAST(ts / ?1 AS INTEGER) * ?1 AS t, avg(avg), min(min), max(max) FROM {table} WHERE sid=?2 AND ts>=?3 AND ts<=?4 GROUP BY t ORDER BY t"), from, to)
        };
        // rollups lag a minute: top the tail up from raw so long ranges reach "now"
        if table != "raw" && to > now - 2 * MIN {
            let last = rows.last().map(|r| r.0).unwrap_or(from);
            rows.extend(sel("SELECT CAST(ts / ?1 AS INTEGER) * ?1 AS t, avg(v), min(v), max(v) FROM raw WHERE sid=?2 AND ts>?3 AND ts<=?4 GROUP BY t ORDER BY t", last + bucket - 1, to));
        }
        let r1 = |x: Option<f64>| x.map(|v| (v * 100.0).round() / 100.0);
        json!({
            "host": host, "metric": metric, "label": label, "title": title, "unit": unit, "resolution": table, "bucketMs": bucket, "from": from, "to": to,
            "t": rows.iter().map(|r| r.0).collect::<Vec<_>>(), "avg": rows.iter().map(|r| r1(r.1)).collect::<Vec<_>>(),
            "min": rows.iter().map(|r| r1(r.2)).collect::<Vec<_>>(), "max": rows.iter().map(|r| r1(r.3)).collect::<Vec<_>>(),
        })
    }

    pub fn stats(&self) -> Value {
        let db = self.db.lock().unwrap();
        let c = |t: &str| -> i64 { db.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0)).unwrap_or(0) };
        json!({"series": c("series"), "raw": c("raw"), "m1": c("m1"), "h1": c("h1")})
    }
}

pub fn start(m: Arc<Metrics>) {
    let a = m.clone();
    tokio::spawn(async move {
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            ticks += 1;
            let a = a.clone();
            let _ = tokio::task::spawn_blocking(move || {
                a.flush();
                if ticks % 6 == 0 {
                    a.rollup();
                }
                if ticks % 360 == 0 {
                    a.prune();
                }
            })
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_query_rollup() {
        let d = std::env::temp_dir().join(format!("oglab-m-{}", std::process::id()));
        let m = Metrics::open(&d.join("m.db")).unwrap();
        let now = crate::now_ms() as i64;
        for i in 0..30 {
            m.record("go", "cpu", "", now - (30 - i) * 10_000, Some(i as f64));
        }
        let q = m.query("go", "cpu", "", Some(now - 600_000), Some(now), 100);
        assert_eq!(q["resolution"], "raw");
        assert_eq!(q["t"].as_array().unwrap().len(), 30);
        assert_eq!(q["avg"][29], 29.0);
        m.rollup();
        assert!(m.stats()["m1"].as_i64().unwrap() > 0);
        assert_eq!(m.list_series(Some("go"), None)[0].title, "CPU");
        let snap = json!({"host": "h", "cpu": {"pct": 5.0, "load": [0.5]}, "mem": {"total": 100.0, "available": 25.0}, "disks": [{"mount": "/", "total": 10.0, "used": 4.0, "avail": 6.0}], "containers": [{"name": "c", "state": "running", "cpu": 1.0, "mem": 5.0}]});
        m.ingest_snapshot(&snap, true);
        m.flush();
        let names: Vec<String> = m.list_series(Some("h"), None).into_iter().map(|s| format!("{}:{}", s.metric, s.label)).collect();
        assert!(names.contains(&"mem_used_pct:".to_string()) && names.contains(&"disk_used_pct:/".to_string()) && names.contains(&"ctr_mem:c".to_string()));
        let _ = std::fs::remove_dir_all(d);
    }
}
