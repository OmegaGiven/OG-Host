//! OpenClaw bridge (chat proxy, live progress from the relay, stop, exec approvals, one-shot
//! runs for watches), station AI/Gaming control, and the linuxserver.io catalog.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::Lab;

#[derive(Default)]
pub struct Claw {
    /// thread -> the upstream task of its current run
    runs: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    catalog: Mutex<(u64, Vec<Value>)>,
}

fn agent(lab: &Lab) -> &str {
    if lab.cfg.openclaw_agent.is_empty() { "openclaw/main" } else { &lab.cfg.openclaw_agent }
}

fn session_key(lab: &Lab, thread: &str) -> String {
    format!("agent:{}:openai-user:{thread}", agent(lab).split('/').nth(1).unwrap_or("main"))
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, axum::Json(json!({"error": msg.into()}))).into_response()
}

pub fn context(lab: &Lab) -> String {
    let hosts: Vec<String> = lab.fleet.view().iter().map(|h| format!("{}{}", h["name"].as_str().unwrap_or(""), if h["online"] == true { "" } else { " (offline)" })).collect();
    format!(
        "You are the AI assistant working inside OG-Host — the homelab control panel at {url}.
Call dashboard_guide before changing the dashboard. To watch a web page for restocks, prices or changes use add_watch (OG-Host re-checks it on schedule and messages the user on Signal); list_watches shows results. For \"what happened / when did X start\" questions use query_metrics (history up to 1 year) together with search_logs. Use the \"omegaenv\" MCP tools (list_metrics, query_metrics, dashboard_guide, update_app_tile, remove_app_tile, list_stacks, lab_overview, get_host, list_apps, used_ports, search_logs, tail_logs, noisy_logs, search_catalog, submit_install, install_status, add_app_tile) and follow the \"omegaenv\" skill.
To install an app: check used_ports on the target host, draft a docker compose file, then call submit_install. It lands as \"pending\" and the human approves it in OG-Host → Apps before anything runs. Never claim an install happened until install_status says \"done\".
Keep answers short and concrete; this chat is shown in a narrow side panel.
Current hosts: {hosts}.",
        url = lab.link(""),
        hosts = hosts.join(", ")
    )
}

async fn relay(lab: &Lab, path: &str, body: Option<Value>, timeout: Duration) -> Result<reqwest::Response, String> {
    if lab.cfg.openclaw_relay_url.is_empty() {
        return Err("AI relay is not configured".into());
    }
    let url = format!("{}{path}", lab.cfg.openclaw_relay_url.trim_end_matches('/'));
    let mut rq = if body.is_some() { lab.http.post(&url) } else { lab.http.get(&url) };
    rq = rq.bearer_auth(&lab.cfg.openclaw_password).timeout(timeout);
    if let Some(b) = body {
        rq = rq.json(&b);
    }
    rq.send().await.map_err(|e| e.to_string())
}

pub async fn stop(lab: &Lab, thread: &str) -> Value {
    if let Some(h) = lab.claw.runs.lock().unwrap().remove(thread) {
        h.abort();
    }
    match relay(lab, "/abort", Some(json!({"sessionKey": session_key(lab, thread)})), Duration::from_secs(20)).await {
        Ok(r) => r.json().await.unwrap_or(json!({"ok": true})),
        Err(e) => json!({"error": e}),
    }
}

pub async fn approve(lab: &Lab, id: &str, decision: &str) -> Response {
    match relay(lab, "/approve", Some(json!({"id": id, "decision": decision})), Duration::from_secs(20)).await {
        Ok(r) if r.status().is_success() => axum::Json(r.json::<Value>().await.unwrap_or(json!({"ok": true}))).into_response(),
        Ok(r) => err(StatusCode::BAD_GATEWAY, format!("relay HTTP {}", r.status())),
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

/// Live progress of one thread: the relay's SSE, passed through.
pub async fn events(lab: Arc<Lab>, thread: &str) -> Response {
    if thread.is_empty() {
        return err(StatusCode::BAD_REQUEST, "thread required");
    }
    let path = format!("/events?session={}", urlencode(&session_key(&lab, thread)));
    match relay(&lab, &path, None, Duration::from_secs(24 * 3600)).await {
        Ok(r) if r.status().is_success() => sse_response(Body::from_stream(r.bytes_stream())),
        Ok(r) => err(StatusCode::BAD_GATEWAY, format!("relay HTTP {}", r.status())),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("relay unreachable ({e})")),
    }
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}

pub fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Streamed chat: one live upstream run per thread; a new message replaces the old run. If the
/// browser goes away the run finishes anyway (its answer lands in the OpenClaw session).
pub async fn chat(lab: Arc<Lab>, body: Value) -> Response {
    if lab.cfg.openclaw_url.is_empty() || lab.cfg.openclaw_password.is_empty() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "AI is not configured");
    }
    let thread = body["thread"].as_str().unwrap_or("omegaenv").to_string();
    let prev = lab.claw.runs.lock().unwrap().remove(&thread);
    if let Some(h) = prev {
        h.abort();
        if !lab.cfg.openclaw_relay_url.is_empty() {
            let _ = stop(&lab, &thread).await;
        }
    }
    let msgs: Vec<Value> = body["messages"].as_array().cloned().unwrap_or_default();
    let tail = msgs.len().saturating_sub(20);
    let mut messages = vec![json!({"role": "system", "content": context(&lab)})];
    messages.extend(msgs.into_iter().skip(tail));
    let payload = json!({"model": agent(&lab), "stream": true, "user": thread, "messages": messages});
    let url = format!("{}/v1/chat/completions", lab.cfg.openclaw_url.trim_end_matches('/'));
    let resp = match lab.http.post(&url).bearer_auth(&lab.cfg.openclaw_password).json(&payload).timeout(Duration::from_secs(20 * 60)).send().await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("AI unreachable ({e})")),
    };
    if !resp.status().is_success() {
        let code = resp.status();
        let t: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
        return err(StatusCode::BAD_GATEWAY, format!("AI HTTP {code}: {t}"));
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes_v::Bytes, std::io::Error>>(64);
    let l = lab.clone();
    let th = thread.clone();
    let task = tokio::spawn(async move {
        use futures_util::StreamExt;
        let mut up = resp.bytes_stream();
        let mut keep = tokio::time::interval(Duration::from_secs(20));
        keep.tick().await;
        let mut gone = false;
        loop {
            tokio::select! {
                chunk = up.next() => match chunk {
                    Some(Ok(c)) => { if !gone && tx.send(Ok(c)).await.is_err() { gone = true; } }
                    Some(Err(e)) => {
                        let msg = json!({"choices": [{"delta": {"content": format!("\n\n[OG-Host: stream broke ({e})]")}}]});
                        let _ = tx.send(Ok(format!("data: {msg}\n\ndata: [DONE]\n\n").into())).await;
                        break;
                    }
                    None => break,
                },
                _ = keep.tick() => { if !gone && tx.send(Ok(": still working\n\n".into())).await.is_err() { gone = true; } }
            }
        }
        let mut runs = l.claw.runs.lock().unwrap();
        if runs.get(&th).is_some_and(|h| h.id() == tokio::task::id()) {
            runs.remove(&th);
        }
    });
    lab.claw.runs.lock().unwrap().insert(thread, task.abort_handle());
    sse_response(Body::from_stream(tokio_stream_from(rx)))
}

fn tokio_stream_from<T: Send + 'static>(mut rx: tokio::sync::mpsc::Receiver<T>) -> impl futures_util::Stream<Item = T> {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

pub mod bytes_v {
    pub use axum::body::Bytes;
}

/// One non-streaming run (watches): long timeout, browser + local model.
pub async fn run_once(lab: &Lab, agent: &str, message: &str, user: &str) -> Result<String, String> {
    if lab.cfg.openclaw_url.is_empty() || lab.cfg.openclaw_password.is_empty() {
        return Err("AI not configured".into());
    }
    let url = format!("{}/v1/chat/completions", lab.cfg.openclaw_url.trim_end_matches('/'));
    let r = lab
        .http
        .post(&url)
        .bearer_auth(&lab.cfg.openclaw_password)
        .json(&json!({"model": agent, "stream": false, "user": user, "messages": [{"role": "user", "content": message}]}))
        .timeout(Duration::from_secs(15 * 60))
        .send()
        .await
        .map_err(|e| if e.is_timeout() { "timed out after 15 min".to_string() } else { e.to_string() })?;
    let code = r.status();
    let t = r.text().await.unwrap_or_default();
    if code.is_success() { Ok(t) } else { Err(format!("HTTP {code} {}", t.chars().take(200).collect::<String>())) }
}

// ---- station GPU mode ----

pub async fn station(lab: &Lab, path: &str) -> Value {
    if lab.cfg.station_control.is_empty() {
        return json!({"error": "not configured"});
    }
    let url = format!("{}{path}?token={}", lab.cfg.station_control.trim_end_matches('/'), urlencode(&lab.cfg.station_control_token));
    match lab.http.get(&url).timeout(Duration::from_secs(60)).send().await {
        Ok(r) => {
            let ok = r.status().is_success();
            let t = r.text().await.unwrap_or_default();
            serde_json::from_str(&t).unwrap_or_else(|_| {
                let plain: String = t.split('<').map(|p| p.split_once('>').map(|x| x.1).unwrap_or(p)).collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
                json!({"ok": ok, "text": plain.chars().take(600).collect::<String>()})
            })
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

/// False only when station says its model is unloaded (Gaming mode).
pub async fn station_ready(lab: &Lab) -> bool {
    station(lab, "/status").await["llama_server"] != false
}

// ---- community catalog ----

pub async fn catalog(lab: &Lab) -> Vec<Value> {
    {
        let c = lab.claw.catalog.lock().unwrap();
        if !c.1.is_empty() && crate::now_ms() - c.0 < 6 * 3_600_000 {
            return c.1.clone();
        }
    }
    let url = "https://api.linuxserver.io/api/v1/images?include_config=true&include_deprecated=false";
    let Ok(r) = lab.http.get(url).timeout(Duration::from_secs(20)).send().await else { return lab.claw.catalog.lock().unwrap().1.clone() };
    let Ok(j) = r.json::<Value>().await else { return vec![] };
    let strip = |s: &str| -> String { s.split('<').map(|p| p.split_once('>').map(|x| x.1).unwrap_or(p)).collect::<String>().chars().take(400).collect() };
    let items: Vec<Value> = j["data"]["repositories"]["linuxserver"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| {
            let name = x["name"].as_str().unwrap_or("");
            let cfg = &x["config"];
            json!({
                "name": name, "description": strip(x["description"].as_str().unwrap_or("")), "category": x["category"].as_str().unwrap_or(""),
                "logo": x["project_logo"].as_str().unwrap_or(""), "project": x["project_url"].as_str().unwrap_or(""),
                "image": format!("lscr.io/linuxserver/{name}:latest"), "stars": x["stars"].as_u64().unwrap_or(0),
                "config": if cfg.is_object() { json!({
                    "ports": cfg["ports"].as_array().into_iter().flatten().map(|p| json!({"external": p["external"], "internal": p["internal"], "desc": p["desc"]})).collect::<Vec<_>>(),
                    "volumes": cfg["volumes"].as_array().into_iter().flatten().map(|v| json!({"path": v["path"], "desc": v["desc"]})).collect::<Vec<_>>(),
                    "env": cfg["env_vars"].as_array().into_iter().flatten().map(|e| json!({"name": e["name"], "value": e["value"], "desc": e["desc"]})).collect::<Vec<_>>(),
                }) } else { Value::Null },
            })
        })
        .collect();
    *lab.claw.catalog.lock().unwrap() = (crate::now_ms(), items.clone());
    items
}

pub fn warm(lab: Arc<Lab>) {
    tokio::spawn(async move {
        let _ = catalog(&lab).await;
    });
}
