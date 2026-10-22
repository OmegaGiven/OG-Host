//! "AI": OG-Host's own assistant harness. It talks to any OpenAI-compatible model server
//! (llama.cpp's llama-server, Ollama, vLLM, …; a local one by default) and gives the model
//! OG-Host's tools directly, with no gateway in between.
//!
//! - Every tool call runs as the signed-in user, through OG-Host's own API and its permission
//!   checks: the assistant can never do more than the person asking.
//! - Tools that change something wait for a click (approve / deny) in the chat, unless the
//!   user allowed that tool for the conversation.
//! - Kept small for speed on local models: a fixed system prompt and tool list (the model
//!   server can cache them), live context in the latest message, compact tool results, a
//!   bounded history and a step limit.
//!
//! The chat streams OpenAI-style `data: {"choices":[{"delta":{"content":…}}]}` lines plus
//! `data: {"og":{…}}` lines for tool progress and approval requests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::StreamExt;
use og_access::Who;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::serve::Daemon;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// OpenAI-compatible base URL, e.g. http://station:8080/v1
    pub url: String,
    pub model: String,
    /// optional bearer key (kept on the server, never sent to the browser)
    pub key: String,
    pub temperature: f64,
    pub max_steps: usize,
    /// tools that run without asking (names), on top of the read-only ones
    pub auto_approve: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config { url: String::new(), model: String::new(), key: String::new(), temperature: 0.2, max_steps: 10, auto_approve: vec![] }
    }
}

/// Give the lab the model server too (watch checks the rules can't decide).
pub fn share_with_lab(d: &Daemon) {
    if let Some(lab) = &d.lab {
        let c = config(d);
        *lab.ai.write().unwrap() = (!c.url.is_empty()).then(|| og_lab::quickcheck::AiCfg { url: c.url, model: c.model, key: c.key });
    }
}

pub fn config(d: &Daemon) -> Config {
    std::fs::read(d.root.join("ai.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_config(d: &Daemon, c: &Config) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let p = d.root.join("ai.json");
    let tmp = p.with_extension("tmp");
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?.write_all(&serde_json::to_vec_pretty(c)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// A tool: name, what it does, its parameters, whether it changes something, and how it runs.
struct Tool {
    name: &'static str,
    desc: &'static str,
    params: Value,
    required: &'static [&'static str],
    changes: bool,
}

fn s() -> Value {
    json!({"type": "string"})
}
fn n() -> Value {
    json!({"type": "number"})
}
fn en(v: &[&str]) -> Value {
    json!({"type": "string", "enum": v})
}

fn tools(lab: bool) -> Vec<Tool> {
    let mut t = vec![
        Tool { name: "server_status", desc: "This server: host name, CPU, memory, disks, GPUs, version, containers/VMs running, resource brain state.", params: json!({}), required: &[], changes: false },
        Tool { name: "list_containers", desc: "Containers on this server: name, image, state, health, CPU %, memory, ports, stack.", params: json!({}), required: &[], changes: false },
        Tool { name: "container_logs", desc: "Last log lines of a container.", params: json!({"name": s(), "lines": n()}), required: &["name"], changes: false },
        Tool { name: "container_action", desc: "Start, stop or restart a container.", params: json!({"name": s(), "action": en(&["start", "stop", "restart"])}), required: &["name", "action"], changes: true },
        Tool { name: "list_stacks", desc: "Compose stacks on this server with their services and states.", params: json!({}), required: &[], changes: false },
        Tool { name: "read_stack", desc: "The compose file of a stack.", params: json!({"name": s()}), required: &["name"], changes: false },
        Tool { name: "write_stack", desc: "Save a new compose file for a stack (create or change). Read it first; change only what's needed.", params: json!({"name": s(), "compose": s()}), required: &["name", "compose"], changes: true },
        Tool { name: "stack_action", desc: "Bring a stack up, down, restart it, or pull new images and recreate (pull).", params: json!({"name": s(), "action": en(&["up", "down", "restart", "pull"])}), required: &["name", "action"], changes: true },
        Tool { name: "list_vms", desc: "Virtual machines: name, state, CPU, memory, addresses.", params: json!({}), required: &[], changes: false },
        Tool { name: "vm_action", desc: "Start, shut down (ACPI), reboot or force-stop a VM.", params: json!({"name": s(), "action": en(&["start", "shutdown", "reboot", "stop"])}), required: &["name", "action"], changes: true },
        Tool { name: "list_backups", desc: "Backups in a datastore (default \"local\"), newest first, optionally of one group like stack/immich.", params: json!({"store": s(), "group": s()}), required: &[], changes: false },
        Tool { name: "backup_now", desc: "Back up a stack, container or VM now.", params: json!({"kind": en(&["stack", "container", "vm"]), "name": s(), "store": s()}), required: &["kind", "name"], changes: true },
        Tool { name: "list_tasks", desc: "Recent background tasks (backups, restores, updates…) and their status.", params: json!({}), required: &[], changes: false },
        Tool { name: "task_log", desc: "The log of one task.", params: json!({"id": s()}), required: &["id"], changes: false },
        Tool { name: "cluster_nodes", desc: "Cluster nodes and their status (if this server is in a cluster).", params: json!({}), required: &[], changes: false },
    ];
    if lab {
        t.extend([
            Tool { name: "lab_overview", desc: "The whole homelab: machines online, app health, alerts.", params: json!({}), required: &[], changes: false },
            Tool { name: "get_host", desc: "One machine of the lab in detail: CPU, memory, GPUs, disks, containers.", params: json!({"name": s()}), required: &["name"], changes: false },
            Tool { name: "search_logs", desc: "Search logs of every machine; newest first. level error finds problems.", params: json!({"query": s(), "host": s(), "level": en(&["all", "warn", "error"]), "hours": n(), "limit": n()}), required: &[], changes: false },
            Tool { name: "query_metrics", desc: "History of a metric (min/avg/max + points). Use list_metrics to find names.", params: json!({"host": s(), "metric": s(), "label": s(), "hours": n()}), required: &["host", "metric"], changes: false },
            Tool { name: "list_metrics", desc: "Which metric series exist.", params: json!({"host": s(), "metric": s()}), required: &[], changes: false },
            Tool { name: "list_apps", desc: "Home screen app tiles with health.", params: json!({}), required: &[], changes: false },
            Tool { name: "add_app_tile", desc: "Add or update a Home screen tile for an app that already runs.", params: json!({"name": s(), "href": s(), "icon": s(), "group": s(), "description": s(), "host": s()}), required: &["name", "href"], changes: true },
            Tool { name: "list_watches", desc: "Web pages being watched (restock/price/change) and their latest result.", params: json!({}), required: &[], changes: false },
            Tool { name: "add_watch", desc: "Watch a web page: kind restock, price (targetPrice) or change (question: what matters).", params: json!({"name": s(), "url": s(), "kind": en(&["restock", "price", "change"]), "question": s(), "targetPrice": n(), "everyMin": n()}), required: &["name", "url"], changes: true },
            Tool { name: "search_catalog", desc: "Search the linuxserver.io app catalog (images, ports, volumes, env).", params: json!({"query": s()}), required: &["query"], changes: false },
            Tool { name: "used_ports", desc: "TCP ports in use on a machine (check before picking a port).", params: json!({"host": s()}), required: &["host"], changes: false },
        ]);
    }
    t
}

fn tool_specs(list: &[Tool]) -> Vec<Value> {
    list.iter()
        .map(|t| {
            json!({"type": "function", "function": {"name": t.name, "description": t.desc, "parameters": {"type": "object", "properties": t.params, "required": t.required}}})
        })
        .collect()
}

const SYSTEM: &str = "You are AI, the assistant built into OG-Host, a server OS for containers and VMs, managed from the browser. You act through the tools; they run with the permissions of the person you're talking to.
Rules: look before you change (read state with the list/read tools first). Changes (start/stop, compose edits, backups, VM power) are shown to the person for approval: say what you're about to do and why in one line, then call the tool. Never claim something happened unless a tool result says so. If a tool fails, say what failed. Prefer the smallest change that solves the problem. Answers: short and concrete, plain text or simple lists; this chat is a narrow side panel.";

/// Run a /v1 call through our own API as `who` (its permission checks apply).
async fn api(d: &Daemon, who: &Who, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
    let app = d.cluster.app.get().cloned().context("starting")?;
    let mut rq = Request::builder().method(method).uri(path);
    let b = match body {
        Some(v) => {
            rq = rq.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v)?)
        }
        None => Body::empty(),
    };
    let mut req = rq.body(b)?;
    req.extensions_mut().insert(crate::cluster::ClusterUser(who.clone()));
    use tower::ServiceExt;
    let resp = app.oneshot(req).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let st = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20).await?;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    if !st.is_success() {
        bail!("{}", v["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {st}")));
    }
    Ok(v)
}

fn arg<'a>(a: &'a Value, k: &str) -> &'a str {
    a[k].as_str().unwrap_or("")
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn container_id(d: &Daemon, name: &str) -> Result<String> {
    Ok(d.node.find(name)?.id)
}

/// Execute one tool. Results are trimmed to what the model needs.
async fn run_tool(d: &Arc<Daemon>, who: &Who, name: &str, a: &Value) -> Result<Value> {
    let g = Method::GET;
    let p = Method::POST;
    Ok(match name {
        "server_status" => {
            let i = api(d, who, g.clone(), "/v1/info", None).await?;
            let b = api(d, who, g, "/v1/brain", None).await.unwrap_or(Value::Null);
            json!({"info": i, "brain": {"pressure": b["pressure"], "actions": b["actions"]}})
        }
        "list_containers" => {
            let v = api(d, who, g, "/v1/containers", None).await?;
            json!(v.as_array().into_iter().flatten().map(|c| json!({"name": c["name"], "image": c["image"], "state": c["state"], "health": c["health"]["status"], "stack": c["project"], "ports": c["ports"], "ip": c["ip"]})).collect::<Vec<_>>())
        }
        "container_logs" => {
            let id = container_id(d, arg(a, "name")).await?;
            let lines = a["lines"].as_u64().unwrap_or(60).min(300);
            let v = api(d, who, g, &format!("/v1/containers/{id}/logs?tail={lines}"), None).await?;
            v
        }
        "container_action" => {
            let id = container_id(d, arg(a, "name")).await?;
            let act = arg(a, "action");
            if !["start", "stop", "restart"].contains(&act) {
                bail!("action is start, stop or restart");
            }
            api(d, who, p, &format!("/v1/containers/{id}/{act}"), Some(json!({}))).await?
        }
        "list_stacks" => api(d, who, g, "/v1/stacks", None).await?,
        "read_stack" => api(d, who, g, &format!("/v1/stacks/{}", enc(arg(a, "name"))), None).await?,
        "write_stack" => api(d, who, Method::PUT, &format!("/v1/stacks/{}", enc(arg(a, "name"))), Some(json!({"content": arg(a, "compose")}))).await?,
        "stack_action" => {
            let act = arg(a, "action");
            if !["up", "down", "restart", "pull"].contains(&act) {
                bail!("action is up, down, restart or pull");
            }
            api(d, who, p, &format!("/v1/stacks/{}/{act}", enc(arg(a, "name"))), Some(json!({}))).await?
        }
        "list_vms" => {
            let v = api(d, who, g, "/v1/vms", None).await?;
            json!(v.as_array().into_iter().flatten().map(|x| json!({"name": x["vm"]["name"], "state": x["state"], "os": x["vm"]["os"], "cores": x["vm"]["cores"], "memory_mib": x["vm"]["memory"], "cpu_pct": x["cpu"], "nics": x["nics"]})).collect::<Vec<_>>())
        }
        "vm_action" => {
            let act = arg(a, "action");
            if !["start", "shutdown", "reboot", "stop"].contains(&act) {
                bail!("action is start, shutdown, reboot or stop");
            }
            api(d, who, p, &format!("/v1/vms/{}/{act}", enc(arg(a, "name"))), Some(json!({}))).await?
        }
        "list_backups" => {
            let store = if arg(a, "store").is_empty() { "local" } else { arg(a, "store") };
            let mut q = String::new();
            if !arg(a, "group").is_empty() {
                q = format!("?group={}", enc(arg(a, "group")));
            }
            let v = api(d, who, g, &format!("/v1/storage/{}/backups{q}", enc(store)), None).await?;
            json!(v.as_array().into_iter().flatten().take(30).map(|b| json!({"id": b["id"], "size": b["size"], "notes": b["notes"], "verified": b["verified"]["ok"], "protected": b["protected"]})).collect::<Vec<_>>())
        }
        "backup_now" => {
            let store = if arg(a, "store").is_empty() { "local" } else { arg(a, "store") };
            api(d, who, p, "/v1/backup", Some(json!({"store": store, "targets": [{"kind": arg(a, "kind"), "name": arg(a, "name")}]}))).await?
        }
        "list_tasks" => {
            let v = api(d, who, g, "/v1/tasks?limit=15", None).await?;
            v
        }
        "task_log" => {
            let v = api(d, who, g, &format!("/v1/tasks/{}", enc(arg(a, "id"))), None).await?;
            let log: Vec<Value> = v["log"].as_array().cloned().unwrap_or_default();
            json!({"info": v["info"], "log": log.iter().rev().take(40).rev().collect::<Vec<_>>()})
        }
        "cluster_nodes" => {
            let v = api(d, who, g, "/v1/cluster", None).await?;
            json!({"cluster": v["cluster"], "nodes": v["nodes"].as_array().into_iter().flatten().map(|n| json!({"name": n["name"], "site": n["site"], "online": n["online"], "load": n["info"]["load"], "cores": n["info"]["cores"], "containers": n["info"]["containers_running"], "vms": n["info"]["vms_running"]})).collect::<Vec<_>>()})
        }
        other => {
            // the lab's tools (fleet, logs, metrics, apps, watches, catalog)
            let lab = d.lab.clone().context("the lab isn't on")?;
            if !d.access.allowed(who, "/lab", if tools(true).iter().any(|t| t.name == other && t.changes) { "Lab.Modify" } else { "Lab.Audit" }) {
                bail!("permission denied for {other}");
            }
            og_lab::mcp::call(&lab, other, a).await?
        }
    })
}

/// Shorten a tool result for the model: compact JSON, long arrays/strings cut.
fn compact(v: &Value, budget: usize) -> String {
    fn trim(v: &Value, depth: usize) -> Value {
        match v {
            Value::Array(a) if a.len() > 40 => {
                let mut out: Vec<Value> = a.iter().take(40).map(|x| trim(x, depth + 1)).collect();
                out.push(json!(format!("… {} more", a.len() - 40)));
                Value::Array(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| trim(x, depth + 1)).collect()),
            Value::Object(o) => Value::Object(o.iter().filter(|(_, x)| !x.is_null()).map(|(k, x)| (k.clone(), trim(x, depth + 1))).collect()),
            Value::String(s) if s.len() > 3000 => Value::String(format!("{}… ({} chars cut)", &s[..s.char_indices().nth(3000).map(|c| c.0).unwrap_or(s.len())], s.len() - 3000)),
            x => x.clone(),
        }
    }
    let s = serde_json::to_string(&trim(v, 0)).unwrap_or_default();
    if s.len() > budget {
        let cut = s.char_indices().nth(budget).map(|c| c.0).unwrap_or(s.len());
        format!("{}… (result cut at {budget} chars)", &s[..cut])
    } else {
        s
    }
}

/// Live facts for the latest message (kept out of the system prompt, so its cache holds).
fn live_context(d: &Daemon, who: &Who) -> String {
    let cts = d.node.list().unwrap_or_default();
    let running = cts.iter().filter(|(_, s)| s.state == "running").count();
    let unhealthy: Vec<String> = cts.iter().filter(|(_, s)| s.state != "running" && s.state != "exited" && s.state != "created").map(|(m, s)| format!("{} ({})", m.name, s.state)).collect();
    let vms = d.vms.list();
    let vm_running = vms.iter().filter(|v| d.vms.status(&v.name).state == "running").count();
    let nodes = d.cluster.state().map(|s| s.nodes.len()).unwrap_or(1);
    format!(
        "[context: server {} · {} of {} containers running{} · {} of {} VMs running · {} node(s) · user {} · {}]",
        crate::tls::hostname(),
        running,
        cts.len(),
        if unhealthy.is_empty() { String::new() } else { format!(" · not ok: {}", unhealthy.join(", ")) },
        vm_running,
        vms.len(),
        nodes,
        who.label(),
        og_backup::time_name(crate::containers::unix_now())
    )
}

// ---- conversations: approvals and stop ----

#[derive(Default)]
pub struct Ai {
    /// approval id -> answer channel
    waiting: Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
    /// thread -> running loop
    runs: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// thread -> tools allowed without asking (for this conversation)
    allowed: Mutex<HashMap<String, Vec<String>>>,
}

#[derive(Deserialize)]
struct ChatReq {
    #[serde(default)]
    thread: String,
    messages: Vec<Value>,
}

fn sse(v: Value) -> axum::body::Bytes {
    format!("data: {v}\n\n").into()
}

fn delta(text: &str) -> axum::body::Bytes {
    sse(json!({"choices": [{"delta": {"content": text}}]}))
}

async fn chat(State(d): State<Arc<Daemon>>, Extension(w): Extension<Who>, Json(b): Json<ChatReq>) -> Response {
    if !d.access.allowed(&w, "/lab", "AI.Use") && !d.access.allowed(&w, "/", "AI.Use") {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "permission denied: AI.Use"}))).into_response();
    }
    let cfg = config(&d);
    if cfg.url.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "AI has no model server yet: Settings → AI"}))).into_response();
    }
    let thread = if b.thread.is_empty() { "default".to_string() } else { b.thread.chars().take(64).collect() };
    let thread_key = format!("{}:{thread}", w.label());
    if let Some(h) = d.ai.runs.lock().unwrap().remove(&thread_key) {
        h.abort();
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(64);
    let d2 = d.clone();
    let key2 = thread_key.clone();
    let task = tokio::spawn(async move {
        let r = agent_loop(&d2, &w, &cfg, &key2, b.messages, tx.clone()).await;
        if let Err(e) = r {
            let _ = tx.send(Ok(delta(&format!("\n\n[AI: {e:#}]")))).await;
        }
        let _ = tx.send(Ok("data: [DONE]\n\n".into())).await;
        d2.ai.runs.lock().unwrap().remove(&key2);
    });
    d.ai.runs.lock().unwrap().insert(thread_key, task.abort_handle());
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}

type Tx = tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>;

async fn agent_loop(d: &Arc<Daemon>, who: &Who, cfg: &Config, thread: &str, history: Vec<Value>, tx: Tx) -> Result<()> {
    let list = tools(d.lab.is_some());
    let specs = tool_specs(&list);
    // bounded history: last 16 messages from the browser (user/assistant text only)
    let tail = history.len().saturating_sub(16);
    let mut msgs: Vec<Value> = vec![json!({"role": "system", "content": SYSTEM})];
    let hist: Vec<Value> = history.into_iter().skip(tail).filter(|m| ["user", "assistant"].contains(&m["role"].as_str().unwrap_or(""))).map(|m| json!({"role": m["role"], "content": m["content"].as_str().unwrap_or("")})).collect();
    let last_user = hist.iter().rposition(|m| m["role"] == "user");
    for (i, m) in hist.into_iter().enumerate() {
        if Some(i) == last_user {
            msgs.push(json!({"role": "user", "content": format!("{}\n\n{}", m["content"].as_str().unwrap_or(""), live_context(d, who))}));
        } else {
            msgs.push(m);
        }
    }
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(600)).build()?;
    let url = format!("{}/chat/completions", cfg.url.trim_end_matches('/'));
    for step in 0..cfg.max_steps.clamp(1, 30) {
        let mut body = json!({"model": if cfg.model.is_empty() { "local" } else { &cfg.model }, "messages": msgs, "tools": specs, "tool_choice": "auto", "stream": true, "temperature": cfg.temperature, "cache_prompt": true});
        if step + 1 == cfg.max_steps {
            body["tool_choice"] = json!("none");
        }
        let mut rq = http.post(&url).json(&body);
        if !cfg.key.is_empty() {
            rq = rq.bearer_auth(&cfg.key);
        }
        let resp = rq.send().await.context("the model server isn't reachable")?;
        if !resp.status().is_success() {
            let st = resp.status();
            let t: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
            bail!("model server: HTTP {st}: {t}");
        }
        // stream: forward text, collect tool calls
        let mut text = String::new();
        let mut calls: Vec<(String, String, String)> = vec![]; // id, name, args
        let mut buf = String::new();
        let mut up = resp.bytes_stream();
        while let Some(chunk) = up.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(i) = buf.find('\n') {
                let line = buf[..i].trim().to_string();
                buf.drain(..=i);
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                let dl = &v["choices"][0]["delta"];
                if let Some(c) = dl["content"].as_str() {
                    if !c.is_empty() {
                        text.push_str(c);
                        let _ = tx.send(Ok(delta(c))).await;
                    }
                }
                for tc in dl["tool_calls"].as_array().into_iter().flatten() {
                    let idx = tc["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
                    while calls.len() <= idx {
                        calls.push((String::new(), String::new(), String::new()));
                    }
                    if let Some(id) = tc["id"].as_str() {
                        calls[idx].0 = id.to_string();
                    }
                    if let Some(n) = tc["function"]["name"].as_str() {
                        calls[idx].1.push_str(n);
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        calls[idx].2.push_str(a);
                    }
                }
            }
        }
        calls.retain(|c| !c.1.is_empty());
        if calls.is_empty() {
            return Ok(());
        }
        let tc_json: Vec<Value> = calls.iter().enumerate().map(|(i, (id, name, args))| json!({"id": if id.is_empty() { format!("call{step}_{i}") } else { id.clone() }, "type": "function", "function": {"name": name, "arguments": if args.is_empty() { "{}" } else { args }}})).collect();
        msgs.push(json!({"role": "assistant", "content": text, "tool_calls": tc_json}));
        for (i, (_, name, args)) in calls.iter().enumerate() {
            let call_id = tc_json[i]["id"].as_str().unwrap_or("").to_string();
            let a: Value = serde_json::from_str(if args.is_empty() { "{}" } else { args }).unwrap_or(json!({}));
            let tool = list.iter().find(|t| t.name == name);
            let result = match tool {
                None => format!("{{\"error\":\"there is no tool {name}\"}}"),
                Some(t) => {
                    let _ = tx.send(Ok(sse(json!({"og": {"tool": name, "args": a, "status": "running"}})))).await;
                    let ok = if t.changes && !cfg.auto_approve.iter().any(|x| x == name) && !d.ai.allowed.lock().unwrap().get(thread).is_some_and(|v| v.iter().any(|x| x == name)) {
                        approve(d, &tx, thread, name, &a).await
                    } else {
                        true
                    };
                    if !ok {
                        let _ = tx.send(Ok(sse(json!({"og": {"tool": name, "status": "denied"}})))).await;
                        "{\"error\":\"the person said no; ask what they want instead\"}".to_string()
                    } else {
                        match run_tool(d, who, name, &a).await {
                            Ok(v) => {
                                let _ = tx.send(Ok(sse(json!({"og": {"tool": name, "status": "done"}})))).await;
                                if t.changes {
                                    d.access.audit(&who.label(), "ai", &format!("AI {name}"), &compact(&a, 200), true);
                                }
                                compact(&v, 6000)
                            }
                            Err(e) => {
                                let _ = tx.send(Ok(sse(json!({"og": {"tool": name, "status": "error", "error": format!("{e:#}")}})))).await;
                                json!({"error": format!("{e:#}")}).to_string()
                            }
                        }
                    }
                }
            };
            msgs.push(json!({"role": "tool", "tool_call_id": call_id, "name": name, "content": result}));
        }
        if !text.is_empty() {
            let _ = tx.send(Ok(delta("\n\n"))).await;
        }
    }
    Ok(())
}

/// Ask the person in the chat; true = go ahead.
async fn approve(d: &Daemon, tx: &Tx, thread: &str, tool: &str, args: &Value) -> bool {
    let id = format!("ap{}", &crate::containers::new_id().unwrap_or_default()[..10]);
    let (stx, srx) = tokio::sync::oneshot::channel();
    d.ai.waiting.lock().unwrap().insert(id.clone(), stx);
    let _ = tx.send(Ok(sse(json!({"og": {"approval": {"id": id, "tool": tool, "args": args, "thread": thread}}})))).await;
    let r = tokio::time::timeout(Duration::from_secs(900), srx).await;
    d.ai.waiting.lock().unwrap().remove(&id);
    matches!(r, Ok(Ok(true)))
}

#[derive(Deserialize)]
struct ApproveReq {
    id: String,
    /// allow | always | deny
    decision: String,
    #[serde(default)]
    thread: String,
    #[serde(default)]
    tool: String,
}

async fn approve_req(State(d): State<Arc<Daemon>>, Extension(w): Extension<Who>, Json(b): Json<ApproveReq>) -> Response {
    if !d.access.allowed(&w, "/lab", "AI.Approve") && !d.access.allowed(&w, "/", "AI.Approve") {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "permission denied: AI.Approve"}))).into_response();
    }
    if b.decision == "always" && !b.tool.is_empty() {
        let key = if b.thread.contains(':') { b.thread.clone() } else { format!("{}:{}", w.label(), b.thread) };
        d.ai.allowed.lock().unwrap().entry(key).or_default().push(b.tool.clone());
    }
    match d.ai.waiting.lock().unwrap().remove(&b.id) {
        Some(tx) => {
            let _ = tx.send(b.decision != "deny");
            Json(json!({"ok": true})).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "that request isn't waiting any more"}))).into_response(),
    }
}

async fn stop(State(d): State<Arc<Daemon>>, Extension(w): Extension<Who>, Json(b): Json<Value>) -> Response {
    let key = format!("{}:{}", w.label(), b["thread"].as_str().unwrap_or("default"));
    let had = d.ai.runs.lock().unwrap().remove(&key).map(|h| h.abort()).is_some();
    Json(json!({"stopped": had})).into_response()
}

async fn get_config(State(d): State<Arc<Daemon>>) -> Json<Value> {
    let c = config(&d);
    Json(json!({"url": c.url, "model": c.model, "has_key": !c.key.is_empty(), "temperature": c.temperature, "max_steps": c.max_steps, "auto_approve": c.auto_approve, "configured": !c.url.is_empty(),
        "tools": tools(d.lab.is_some()).iter().map(|t| json!({"name": t.name, "changes": t.changes, "description": t.desc})).collect::<Vec<_>>()}))
}

#[derive(Deserialize)]
struct ConfigReq {
    #[serde(flatten)]
    c: Config,
    /// "" keeps the stored key, "-" clears it
    #[serde(default)]
    key_change: Option<String>,
}

async fn set_config(State(d): State<Arc<Daemon>>, Json(b): Json<ConfigReq>) -> Response {
    let mut c = b.c;
    let old = config(&d);
    c.key = match b.key_change.as_deref() {
        None | Some("") => old.key,
        Some("-") => String::new(),
        Some(k) => k.to_string(),
    };
    if !c.url.is_empty() && !(c.url.starts_with("http://") || c.url.starts_with("https://")) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "the model server URL starts with http:// or https://"}))).into_response();
    }
    c.max_steps = c.max_steps.clamp(1, 30);
    match save_config(&d, &c) {
        Ok(()) => {
            share_with_lab(&d);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// Ask the model server which models it has (to fill the settings form).
async fn models(State(d): State<Arc<Daemon>>) -> Response {
    let c = config(&d);
    if c.url.is_empty() {
        return Json(json!([])).into_response();
    }
    let http = reqwest::Client::builder().timeout(Duration::from_secs(8)).build().unwrap();
    let mut rq = http.get(format!("{}/models", c.url.trim_end_matches('/')));
    if !c.key.is_empty() {
        rq = rq.bearer_auth(&c.key);
    }
    match rq.send().await {
        Ok(r) => {
            let v: Value = r.json().await.unwrap_or_default();
            Json(json!(v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str()).collect::<Vec<_>>())).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": format!("model server not reachable: {e}")}))).into_response(),
    }
}

pub fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/v1/ai/chat", post(chat))
        .route("/v1/ai/approve", post(approve_req))
        .route("/v1/ai/stop", post(stop))
        .route("/v1/ai/config", get(get_config).put(set_config))
        .route("/v1/ai/models", get(models))
        .with_state(d)
}
