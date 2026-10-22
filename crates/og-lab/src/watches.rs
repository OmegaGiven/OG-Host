//! Watches: OpenClaw checks web pages on a schedule (browser + local model) and reports a
//! structured answer through the `report_watch` MCP tool; OG-Host keeps the list, the
//! schedule (one check at a time — one GPU), history, price metrics, and messages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{now_ms, Lab};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Check {
    pub at: u64,
    pub status: String,
    pub in_stock: Option<bool>,
    pub price: Option<f64>,
    pub summary: String,
    pub changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Watch {
    pub id: String,
    pub name: String,
    pub url: String,
    pub kind: String,
    pub question: String,
    pub target_price: Option<f64>,
    pub every_min: u64,
    pub enabled: bool,
    pub notify: bool,
    pub created_at: u64,
    pub created_by: String,
    pub last: Option<Check>,
    pub history: Vec<Check>,
    pub fail_notified: bool,
    pub check_now: bool,
}

pub struct Watches {
    dir: PathBuf,
    pub list: Mutex<Vec<Watch>>,
    running: Mutex<HashMap<String, (u64, Option<String>)>>,
    queue: Mutex<Vec<String>>,
    busy: Mutex<bool>,
    last_tick: Mutex<Option<u64>>,
}

fn default_question(kind: &str) -> String {
    if kind == "change" {
        "Summarise the important content of this page in 1–2 sentences so a change can be spotted next time.".into()
    } else {
        "Is this item in stock / available to buy right now, and what is the current price?".into()
    }
}

impl Watches {
    pub fn load(dir: &Path) -> Watches {
        let list: Vec<Watch> = crate::store::load::<Vec<Value>>(dir, "watches.json")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|mut v| {
                // OmegaEnv files: notify missing means true
                if v.get("notify").is_none() {
                    v["notify"] = json!(true);
                }
                if v.get("enabled").is_none() {
                    v["enabled"] = json!(true);
                }
                serde_json::from_value(v).ok()
            })
            .collect();
        Watches { dir: dir.to_path_buf(), list: Mutex::new(list), running: Default::default(), queue: Default::default(), busy: Mutex::new(false), last_tick: Mutex::new(None) }
    }

    fn persist(&self) {
        crate::store::save(&self.dir, "watches.json", &*self.list.lock().unwrap());
    }

    pub fn public(&self) -> Vec<Value> {
        let running = self.running.lock().unwrap();
        self.list
            .lock()
            .unwrap()
            .iter()
            .map(|w| {
                let mut v = serde_json::to_value(w).unwrap();
                v["running"] = json!(running.contains_key(&w.id));
                v
            })
            .collect()
    }

    pub fn add(&self, b: &Value, by: &str) -> Result<Watch> {
        let (Some(name), Some(url)) = (b["name"].as_str().filter(|s| !s.is_empty()), b["url"].as_str().filter(|s| !s.is_empty())) else { bail!("name and url required") };
        let u = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("url is not valid"))?;
        if !["http", "https"].contains(&u.scheme()) {
            bail!("url must be http or https");
        }
        let kind = b["kind"].as_str().filter(|k| ["restock", "price", "change"].contains(k)).unwrap_or("restock").to_string();
        let w = Watch {
            id: crate::uuid()[..8].to_string(),
            name: name.chars().take(80).collect(),
            url: url.into(),
            question: b["question"].as_str().filter(|q| !q.is_empty()).map(|q| q.chars().take(600).collect()).unwrap_or_else(|| default_question(&kind)),
            kind,
            target_price: b["targetPrice"].as_f64().or_else(|| b["targetPrice"].as_str().and_then(|s| s.parse().ok())),
            every_min: b["everyMin"].as_u64().unwrap_or(60).max(15),
            enabled: b["enabled"] != false,
            notify: b["notify"] != false,
            created_at: now_ms(),
            created_by: by.into(),
            ..Default::default()
        };
        self.list.lock().unwrap().push(w.clone());
        self.persist();
        Ok(w)
    }

    pub fn update(&self, id: &str, b: &Value) -> Result<Watch> {
        let mut l = self.list.lock().unwrap();
        let Some(w) = l.iter_mut().find(|w| w.id == id) else { bail!("no such watch") };
        let mut v = serde_json::to_value(&*w)?;
        for k in ["name", "url", "kind", "question", "everyMin", "enabled", "targetPrice", "notify"] {
            if let Some(x) = b.get(k) {
                v[k] = if k == "targetPrice" && x == "" { Value::Null } else { x.clone() };
            }
        }
        *w = serde_json::from_value(v)?;
        w.every_min = w.every_min.max(15);
        let out = w.clone();
        drop(l);
        self.persist();
        Ok(out)
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut l = self.list.lock().unwrap();
        let n = l.len();
        l.retain(|w| w.id != id);
        let changed = l.len() != n;
        drop(l);
        self.persist();
        changed
    }

    pub fn check_now(&self, id: &str) -> Result<()> {
        let mut l = self.list.lock().unwrap();
        let Some(w) = l.iter_mut().find(|w| w.id == id) else { bail!("no such watch") };
        w.check_now = true;
        Ok(())
    }

    pub fn debug(&self) -> Value {
        json!({"busy": *self.busy.lock().unwrap(), "queue": *self.queue.lock().unwrap(), "running": self.running.lock().unwrap().iter().map(|(id, (since, tok))| json!({"id": id, "since": since, "reported": tok.is_none()})).collect::<Vec<_>>(), "lastTick": *self.last_tick.lock().unwrap()})
    }

    /// Called by the report_watch MCP tool.
    pub fn report(&self, lab: &Arc<Lab>, b: &Value) -> Result<Value> {
        let id = b["id"].as_str().unwrap_or("");
        let token = b["token"].as_str().unwrap_or("");
        {
            let mut r = self.running.lock().unwrap();
            match r.get_mut(id) {
                Some((_, Some(t))) if crate::safe_eq(t, token) => {
                    r.get_mut(id).unwrap().1 = None;
                }
                _ => bail!("this check is not running (wrong or expired token) — do not report again"),
            }
        }
        let res = Check {
            at: now_ms(),
            status: b["status"].as_str().filter(|s| ["ok", "blocked", "error"].contains(s)).unwrap_or("ok").into(),
            in_stock: b["in_stock"].as_bool(),
            price: b["price"].as_f64().or_else(|| b["price"].as_str().and_then(|s| s.parse().ok())).filter(|p| p.is_finite()),
            summary: b["summary"].as_str().unwrap_or("").chars().take(300).collect(),
            changed: b["changed"] == true,
        };
        send_all(lab, record(lab, id, res.clone()));
        Ok(json!({"ok": true, "recorded": res}))
    }
}

fn why_notify(w: &Watch, prev: Option<&Check>, cur: &Check) -> Option<String> {
    if cur.status != "ok" {
        return None;
    }
    let p = |n: f64| format!("${n:.2}");
    let Some(prev) = prev else {
        return (cur.in_stock == Some(true)).then(|| format!("🟢 {} is in stock{} (first check).", w.name, cur.price.map(|x| format!(" at {}", p(x))).unwrap_or_default()));
    };
    if cur.in_stock == Some(true) && prev.in_stock == Some(false) {
        return Some(format!("🟢 Back in stock: {}{}", w.name, cur.price.map(|x| format!(" — {}", p(x))).unwrap_or_default()));
    }
    if cur.in_stock == Some(false) && prev.in_stock == Some(true) {
        return Some(format!("⚪ {} went out of stock.", w.name));
    }
    if let (Some(t), Some(c)) = (w.target_price, cur.price) {
        if c <= t && prev.price.is_none_or(|pp| pp > t) {
            return Some(format!("💰 {} is {} — at or below your target {}", w.name, p(c), p(t)));
        }
    }
    if w.kind != "change" {
        if let (Some(c), Some(pp)) = (cur.price, prev.price) {
            if c < pp * 0.97 {
                return Some(format!("📉 Price drop: {} {} → {}", w.name, p(pp), p(c)));
            }
        }
    }
    if w.kind == "change" && cur.changed {
        return Some(format!("🔔 {} changed: {}", w.name, cur.summary));
    }
    None
}

fn send_all(lab: &Arc<Lab>, messages: Vec<String>) {
    for m in messages {
        let lab = lab.clone();
        tokio::spawn(async move {
            let _ = crate::notify::send_signal(&lab, &m).await;
        });
    }
}

/// Store a result; returns the messages it calls for.
fn record(lab: &Lab, id: &str, res: Check) -> Vec<String> {
    let mut messages: Vec<String> = Vec::new();
    {
        let mut l = lab.watches.list.lock().unwrap();
        let Some(w) = l.iter_mut().find(|w| w.id == id) else { return messages };
        let prev = w.last.clone().filter(|x| x.status == "ok").or_else(|| w.history.iter().rev().find(|h| h.status == "ok").cloned());
        w.last = Some(res.clone());
        w.history.push(res.clone());
        let n = w.history.len();
        if n > 200 {
            w.history.drain(0..n - 200);
        }
        if res.status == "ok" {
            if let Some(p) = res.price {
                lab.metrics.record("watches", "watch_price", &w.id, res.at as i64, Some(p));
            }
            if let Some(s) = res.in_stock {
                lab.metrics.record("watches", "watch_in_stock", &w.id, res.at as i64, Some(if s { 1.0 } else { 0.0 }));
            }
        }
        if w.notify {
            if let Some(m) = why_notify(w, prev.as_ref(), &res) {
                messages.push(format!("{m}\n{}", w.url));
            }
        }
        let last3: Vec<&Check> = w.history.iter().rev().take(3).collect();
        let bad = |c: &&Check| c.status != "ok" && c.status != "skipped";
        if res.status != "ok" && res.status != "skipped" && last3.len() == 3 && last3.iter().all(bad) && !w.fail_notified && w.notify {
            w.fail_notified = true;
            messages.push(format!("🟠 Watch \"{}\" has failed 3 times in a row ({}): {}\n{}", w.name, res.status, res.summary, lab.link("#watch")));
        }
        if res.status == "ok" && w.fail_notified {
            w.fail_notified = false;
        }
    }
    lab.watches.persist();
    messages
}

fn prompt(w: &Watch, token: &str) -> String {
    let prev = match &w.last {
        Some(l) => format!(
            "Previous result: in_stock={}, price={}, summary=\"{}\"",
            l.in_stock.map(|b| b.to_string()).unwrap_or("unknown".into()),
            l.price.map(|p| p.to_string()).unwrap_or("unknown".into()),
            l.summary
        ),
        None => "No previous result — this is the first check.".into(),
    };
    format!(
        "AUTOMATED OG-Host WATCH CHECK — no human is reading this chat, so nobody can approve anything. Use ONLY your browser tool (web fetch as a fallback) and the omegaenv report_watch tool. NEVER use exec, shell, terminal, curl or any command-running tool — it will hang. Do exactly this, nothing else:
1. Open {url} with your BROWSER tool (a real browser renders shop pages that block plain fetches; only use web fetch if the browser tool itself errors). Wait a few seconds for the page to load; scroll if needed. If the page is a robot/captcha check (e.g. \"Just a moment...\"), do not try to get around it — report status \"blocked\".
2. Answer: {q}
3. Call the omegaenv tool report_watch with: id \"{id}\", token \"{token}\", status (\"ok\", or \"blocked\" if a captcha/robot check/login wall stopped you, or \"error\"), in_stock (true/false, omit if not applicable), price (number in USD, omit if none), summary (one short sentence a human can read on a phone), and changed (true only if the answer meaningfully differs from the previous result).
{prev}
Watch name: {name}. Kind: {kind}.
After report_watch succeeds reply only: done",
        url = w.url, q = w.question, id = w.id, name = w.name, kind = w.kind
    )
}

async fn run_one(lab: &Arc<Lab>, w: Watch) {
    let token = crate::random_token(9);
    {
        let mut r = lab.watches.running.lock().unwrap();
        if r.contains_key(&w.id) {
            return;
        }
        r.insert(w.id.clone(), (now_ms(), Some(token.clone())));
    }
    let skipped = !crate::claw::station_ready(lab).await;
    if skipped {
        send_all(lab, record(lab, &w.id, Check { at: now_ms(), status: "skipped".into(), summary: "station is in Gaming mode — OpenClaw's model is unloaded".into(), ..Default::default() }));
    } else {
        let agent = if lab.cfg.watch_agent.is_empty() { "openclaw/watcher" } else { lab.cfg.watch_agent.as_str() };
        match crate::claw::run_once(lab, agent, &prompt(&w, &token), &format!("omegaenv-watch-{}-{}", w.id, now_ms())).await {
            Ok(_) => {
                let unreported = lab.watches.running.lock().unwrap().get(&w.id).is_some_and(|(_, t)| t.as_deref() == Some(token.as_str()));
                if unreported {
                    send_all(lab, record(lab, &w.id, Check { at: now_ms(), status: "error".into(), summary: "OpenClaw finished without reporting a result".into(), ..Default::default() }));
                }
            }
            Err(e) => send_all(lab, record(lab, &w.id, Check { at: now_ms(), status: "error".into(), summary: format!("OpenClaw: {e}").chars().take(200).collect(), ..Default::default() })),
        }
    }
    lab.watches.running.lock().unwrap().remove(&w.id);
}

async fn tick(lab: &Arc<Lab>) {
    *lab.watches.last_tick.lock().unwrap() = Some(now_ms());
    let now = now_ms();
    {
        let mut l = lab.watches.list.lock().unwrap();
        let running = lab.watches.running.lock().unwrap();
        let mut q = lab.watches.queue.lock().unwrap();
        for w in l.iter_mut() {
            let due = w.enabled && w.last.as_ref().is_none_or(|x| now - x.at >= w.every_min * 60_000);
            if (due || w.check_now) && !q.contains(&w.id) && !running.contains_key(&w.id) {
                q.push(w.id.clone());
            }
        }
    }
    {
        let mut b = lab.watches.busy.lock().unwrap();
        if *b {
            return;
        }
        *b = true;
    }
    loop {
        let next = {
            let mut q = lab.watches.queue.lock().unwrap();
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        };
        let Some(id) = next else { break };
        let w = {
            let mut l = lab.watches.list.lock().unwrap();
            l.iter_mut().find(|w| w.id == id).map(|w| {
                w.check_now = false;
                w.clone()
            })
        };
        if let Some(w) = w {
            run_one(lab, w).await;
        }
    }
    *lab.watches.busy.lock().unwrap() = false;
}

/// Price trend from the metric history (kept a year).
pub fn trend(lab: &Lab, id: &str) -> Value {
    let now = now_ms() as i64;
    const D: i64 = 86_400_000;
    let y = lab.metrics.query("watches", "watch_price", id, Some(now - 365 * D), Some(now), 400);
    let t: Vec<i64> = y["t"].as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default();
    let f = |k: &str| -> Vec<Option<f64>> { y[k].as_array().map(|a| a.iter().map(|x| x.as_f64()).collect()).unwrap_or_default() };
    let (avg, lo, hi) = (f("avg"), f("min"), f("max"));
    let pts: Vec<(i64, f64, f64, f64)> = t.iter().enumerate().filter_map(|(i, t)| Some((*t, avg.get(i).copied()??, lo.get(i).copied()??, hi.get(i).copied()??))).collect();
    if pts.is_empty() {
        return Value::Null;
    }
    let since = |d: i64| -> Vec<&(i64, f64, f64, f64)> { pts.iter().filter(|p| p.0 >= now - d * D).collect() };
    let m30 = since(30);
    let pick: Vec<&(i64, f64, f64, f64)> = if m30.is_empty() { pts.iter().collect() } else { m30 };
    let last = pts.last().unwrap().1;
    let week_ago = pts.iter().rev().find(|p| p.0 <= now - 7 * D);
    let min = |a: &[&(i64, f64, f64, f64)]| a.iter().map(|p| p.2).fold(f64::INFINITY, f64::min);
    let max = |a: &[&(i64, f64, f64, f64)]| a.iter().map(|p| p.3).fold(f64::NEG_INFINITY, f64::max);
    let all: Vec<&(i64, f64, f64, f64)> = pts.iter().collect();
    json!({
        "first": pts[0].0, "samples": pts.len(), "low30": min(&pick), "high30": max(&pick), "low365": min(&all), "high365": max(&all),
        "avg30": (100.0 * pick.iter().map(|p| p.1).sum::<f64>() / pick.len() as f64).round() / 100.0,
        "change7d": week_ago.map(|w| (1000.0 * (last - w.1) / w.1).round() / 10.0),
    })
}

pub fn start(lab: Arc<Lab>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        loop {
            tick(&lab).await;
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(status: &str, stock: Option<bool>, price: Option<f64>) -> Check {
        Check { at: 1, status: status.into(), in_stock: stock, price, summary: "s".into(), changed: false }
    }

    #[test]
    fn notify_rules() {
        let w = Watch { name: "Drive".into(), kind: "restock".into(), target_price: Some(300.0), ..Default::default() };
        assert!(why_notify(&w, None, &c("ok", Some(true), Some(350.0))).unwrap().contains("in stock at $350.00"));
        assert!(why_notify(&w, Some(&c("ok", Some(false), None)), &c("ok", Some(true), None)).unwrap().starts_with("🟢 Back in stock"));
        assert!(why_notify(&w, Some(&c("ok", Some(true), Some(320.0))), &c("ok", Some(true), Some(299.0))).unwrap().contains("target"));
        assert!(why_notify(&w, Some(&c("ok", Some(true), Some(500.0))), &c("ok", Some(true), Some(480.0))).unwrap().contains("Price drop"));
        assert!(why_notify(&w, Some(&c("ok", Some(true), Some(500.0))), &c("ok", Some(true), Some(495.0))).is_none());
        assert!(why_notify(&w, None, &c("blocked", None, None)).is_none());
    }
}
