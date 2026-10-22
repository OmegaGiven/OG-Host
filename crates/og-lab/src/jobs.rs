//! Agent jobs (long-poll delivery, as omega-agent expects) and the install queue: requests
//! from OpenClaw or a person (install, remove, start/stop/restart/update, compose edit) wait
//! for approval in the UI, then become jobs on the target machine. Jobs for the OG-Host node
//! itself run directly through [`crate::LocalNode`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::{now_ms, uuid, Lab};

pub struct Job {
    pub id: String,
    pub host: String,
    /// What goes to the agent: { id, type, name, op?, content?, compose?, apply? }
    pub wire: Value,
    pub status: &'static str,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub by: String,
    pub stack_id: Option<String>,
    pub log: String,
    pub data: Option<Value>,
    done: Arc<Notify>,
}

impl Job {
    pub fn public(&self) -> Value {
        let mut v = self.wire.clone();
        if let Some(o) = v.as_object_mut() {
            o.remove("content");
            o.insert("host".into(), json!(self.host));
            o.insert("status".into(), json!(self.status));
            o.insert("createdAt".into(), json!(self.created_at));
            o.insert("startedAt".into(), json!(self.started_at));
            o.insert("finishedAt".into(), json!(self.finished_at));
            o.insert("by".into(), json!(self.by));
            o.insert("stackId".into(), json!(self.stack_id));
            o.insert("log".into(), json!(self.log));
            o.insert("data".into(), self.data.clone().unwrap_or(Value::Null));
        }
        v
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QItem {
    pub id: String,
    pub name: String,
    pub host: String,
    /// deploy | remove | op | edit
    pub action: String,
    pub op: Option<String>,
    pub compose: String,
    pub notes: String,
    pub app: Option<Value>,
    pub requested_by: String,
    /// pending | deploying | done | failed | rejected
    pub status: String,
    pub created_at: u64,
    pub decided_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub log: String,
}

pub struct Jobs {
    dir: PathBuf,
    pub jobs: Mutex<HashMap<String, Job>>,
    wake: Mutex<HashMap<String, Arc<Notify>>>,
    pub queue: Mutex<Vec<QItem>>,
}

fn valid_stack_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 41 && n.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn valid_compose(c: &str) -> Result<()> {
    let v: Value = serde_yaml::from_str(c).map_err(|e| anyhow::anyhow!("compose is not valid YAML: {e}"))?;
    if v.get("services").is_none() {
        bail!("compose is not valid YAML: missing services:");
    }
    Ok(())
}

impl Jobs {
    pub fn load(dir: &Path) -> Jobs {
        Jobs { dir: dir.to_path_buf(), jobs: Default::default(), wake: Default::default(), queue: Mutex::new(crate::store::load(dir, "stacks.json").unwrap_or_default()) }
    }

    fn save_queue(&self) {
        crate::store::save(&self.dir, "stacks.json", &*self.queue.lock().unwrap());
    }

    fn wake_for(&self, host: &str) -> Arc<Notify> {
        self.wake.lock().unwrap().entry(host.to_string()).or_default().clone()
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.jobs.lock().unwrap().get(id).map(|j| j.public())
    }

    pub fn queue_item(&self, id: &str) -> Option<QItem> {
        self.queue.lock().unwrap().iter().find(|s| s.id == id).cloned()
    }

    /// Long poll for an agent: queued jobs for `host` now, or after up to `wait`.
    pub async fn poll(&self, host: &str, wait: Duration) -> Vec<Value> {
        let n = self.wake_for(host);
        let fut = n.notified();
        tokio::pin!(fut);
        fut.as_mut().enable(); // registered before the check: a job queued in between still wakes us
        if !self.has_queued(host) {
            let _ = tokio::time::timeout(wait, fut).await;
        }
        let mut jobs = self.jobs.lock().unwrap();
        let mut out = Vec::new();
        for j in jobs.values_mut().filter(|j| j.host == host && j.status == "queued") {
            j.status = "running";
            j.started_at = Some(now_ms());
            out.push(j.wire.clone());
        }
        out
    }

    fn has_queued(&self, host: &str) -> bool {
        self.jobs.lock().unwrap().values().any(|j| j.host == host && j.status == "queued")
    }
}

/// Queue a job for a machine; returns its id.
pub fn enqueue(lab: &Arc<Lab>, host: &str, mut wire: Value, by: &str, stack_id: Option<String>) -> String {
    let id = uuid();
    wire["id"] = json!(id);
    let local = lab.local_name().is_some_and(|n| n == host);
    let job = Job { id: id.clone(), host: host.into(), wire: wire.clone(), status: if local { "running" } else { "queued" }, created_at: now_ms(), started_at: local.then(now_ms), finished_at: None, by: by.into(), stack_id, log: String::new(), data: None, done: Arc::new(Notify::new()) };
    lab.jobs.jobs.lock().unwrap().insert(id.clone(), job);
    if local {
        let node = lab.local.read().unwrap().clone();
        if let Some(node) = node {
            let l = lab.clone();
            let jid = id.clone();
            tokio::spawn(async move {
                let (ok, log, data) = node.run_job(wire).await;
                finish(&l, &jid, ok, &log, data);
            });
        }
    } else {
        lab.jobs.wake_for(host).notify_waiters();
        let l = lab.clone();
        let jid = id.clone();
        let h = host.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(120)).await;
            let queued = l.jobs.jobs.lock().unwrap().get(&jid).is_some_and(|j| j.status == "queued");
            if queued {
                finish(&l, &jid, false, &format!("{h} did not pick up the job within 2 minutes (agent offline?)"), None);
            }
        });
    }
    id
}

/// Record a job's result (agent or local), update its queue entry and wake waiters.
pub fn finish(lab: &Arc<Lab>, id: &str, ok: bool, log: &str, data: Option<Value>) -> bool {
    let (stack_id, notify, activity) = {
        let mut jobs = lab.jobs.jobs.lock().unwrap();
        let Some(j) = jobs.get_mut(id) else { return false };
        if j.status == "done" || j.status == "failed" {
            return true;
        }
        j.status = if ok { "done" } else { "failed" };
        let n = log.chars().count();
        j.log = log.chars().skip(n.saturating_sub(8000)).collect();
        j.data = data;
        j.finished_at = Some(now_ms());
        let t = j.wire["type"].as_str().unwrap_or("");
        let name = j.wire["name"].as_str().unwrap_or("");
        let what = match t {
            "compose" => format!("{} {name}", j.wire["op"].as_str().unwrap_or("")),
            "write" => format!("edit {name} compose{}", if j.wire["apply"] == true { " + apply" } else { "" }),
            _ => format!("{t} {name}"),
        };
        let tail: String = j.log.chars().skip(j.log.chars().count().saturating_sub(600)).collect();
        let activity = (t != "read").then(|| json!({"by": j.by, "host": j.host, "what": what, "ok": ok, "log": tail}));
        (j.stack_id.clone(), j.done.clone(), activity)
    };
    notify.notify_waiters();
    if let Some(a) = activity {
        lab.apps.log_activity(a);
    }
    let l = lab.clone();
    let jid = id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        l.jobs.jobs.lock().unwrap().remove(&jid);
    });
    if let Some(sid) = stack_id {
        let deployed = {
            let mut q = lab.jobs.queue.lock().unwrap();
            q.iter_mut().find(|s| s.id == sid).map(|s| {
                s.status = if ok { "done".into() } else { "failed".into() };
                let n = log.chars().count();
                s.log = log.chars().skip(n.saturating_sub(8000)).collect();
                s.finished_at = Some(now_ms());
                (ok && s.action == "deploy").then(|| s.clone())
            })
        };
        lab.jobs.save_queue();
        if let Some(Some(s)) = deployed {
            if let Some(app) = s.app.as_ref() {
                let name = app["name"].as_str().unwrap_or(&s.name).to_string();
                let a = lab.apps.upsert(&json!({
                    "id": crate::apps::slug(&name), "name": name, "group": app["group"].as_str().unwrap_or("Installed"),
                    "href": app["href"].as_str().unwrap_or(""), "ping": app["ping"].as_str().or(app["href"].as_str()).unwrap_or(""),
                    "icon": app["icon"].as_str().unwrap_or(""), "description": app["description"].as_str().map(String::from).unwrap_or(format!("Installed by OG-Host on {}", s.host)),
                    "host": s.host, "source": "omegaenv",
                }));
                let l = lab.clone();
                tokio::spawn(async move { crate::apps::check_app(&l, &a).await });
            }
        }
    }
    true
}

/// Wait up to `ms` for a job to finish; returns its public view.
pub async fn wait(lab: &Lab, id: &str, ms: u64) -> Option<Value> {
    let n = lab.jobs.jobs.lock().unwrap().get(id)?.done.clone();
    let fut = n.notified();
    tokio::pin!(fut);
    fut.as_mut().enable();
    let finished = lab.jobs.jobs.lock().unwrap().get(id).is_some_and(|j| j.status == "done" || j.status == "failed");
    if !finished {
        let _ = tokio::time::timeout(Duration::from_millis(ms), fut).await;
    }
    lab.jobs.get(id)
}

/// A compose project on a machine, from its latest snapshot.
pub fn find_project(lab: &Lab, host: &str, name: &str) -> Option<Value> {
    lab.fleet.latest(host)?["projects"].as_array()?.iter().find(|p| p["name"] == name).cloned()
}

/// Submit a request to the install queue (pending until approved).
pub fn submit(lab: &Lab, b: &Value, requested_by: &str) -> Result<QItem> {
    let action = b["action"].as_str().unwrap_or("deploy");
    let name = b["name"].as_str().unwrap_or("").to_string();
    let host = b["host"].as_str().unwrap_or("").to_string();
    let compose = b["compose"].as_str().unwrap_or("").to_string();
    let notes: String = b["notes"].as_str().unwrap_or("").chars().take(4000).collect();
    let mut item = QItem { id: uuid(), name: name.clone(), host: host.clone(), compose: compose.clone(), notes, requested_by: requested_by.into(), status: "pending".into(), created_at: now_ms(), ..Default::default() };
    if action == "op" || action == "edit" {
        let pr = find_project(lab, &host, &name).ok_or_else(|| anyhow::anyhow!("no compose project \"{name}\" on {host}. Use list_projects."))?;
        let op = b["op"].as_str().unwrap_or("");
        if action == "op" && !["start", "stop", "restart", "update"].contains(&op) {
            bail!("op must be start, stop, restart or update");
        }
        if pr["protected"] == true && ["stop", "update"].contains(&op) {
            bail!("{name} is protected (Pterodactyl); manage it from the panel");
        }
        if action == "edit" {
            valid_compose(&compose)?;
        }
        item.action = action.into();
        item.op = (action == "op").then(|| op.to_string());
    } else {
        if !valid_stack_name(&name) {
            bail!("name must be lowercase letters, digits, - or _ (max 41 chars)");
        }
        let q = lab.jobs.queue.lock().unwrap();
        if action == "remove" && !q.iter().any(|s| s.name == name && s.host == host && s.action != "remove" && s.status == "done") {
            bail!("\"{name}\" on {host} was not installed by OG-Host, so there is no stack to remove. To take a tile off the dashboard use remove_app_tile or update_app_tile {{hidden:true}}. To stop a container OG-Host did not install, tell the human the command to run on that host.");
        }
        drop(q);
        let snap = lab.fleet.latest(&host).ok_or_else(|| anyhow::anyhow!("unknown host \"{host}\". Known: {}", lab.fleet.names().join(", ")))?;
        if !snap["docker"].as_bool().unwrap_or(false) {
            bail!("{host} has no docker");
        }
        if action != "remove" {
            valid_compose(&compose)?;
        }
        item.action = if action == "remove" { "remove".into() } else { "deploy".into() };
        item.app = b.get("app").filter(|a| a.is_object()).cloned();
    }
    lab.jobs.queue.lock().unwrap().push(item.clone());
    lab.jobs.save_queue();
    Ok(item)
}

fn to_job(s: &QItem) -> Value {
    match s.action.as_str() {
        "remove" => json!({"type": "remove", "name": s.name}),
        "op" => json!({"type": "compose", "name": s.name, "op": s.op}),
        "edit" => json!({"type": "write", "name": s.name, "content": s.compose, "apply": true}),
        _ => json!({"type": "deploy", "name": s.name, "compose": s.compose}),
    }
}

/// Approve or reject a pending request.
pub fn decide(lab: &Arc<Lab>, id: &str, approve: bool, by: &str) -> Result<QItem> {
    let item = {
        let mut q = lab.jobs.queue.lock().unwrap();
        let Some(s) = q.iter_mut().find(|s| s.id == id) else { bail!("not found") };
        if s.status != "pending" {
            bail!("already {}", s.status);
        }
        s.status = if approve { "deploying".into() } else { "rejected".into() };
        s.decided_at = Some(now_ms());
        s.clone()
    };
    lab.jobs.save_queue();
    if approve {
        enqueue(lab, &item.host, to_job(&item), &format!("{} (approved)", item.requested_by), Some(item.id.clone()));
    } else {
        lab.apps.log_activity(json!({"by": by, "host": item.host, "what": format!("rejected {} {}", item.action, item.name), "ok": true}));
    }
    Ok(item)
}

pub fn start(_lab: Arc<Lab>) {}
