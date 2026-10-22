//! OmegaEnv's HTTP API on the same paths (`/api/...`, `/mcp`, `/message`), so agents, the
//! OpenClaw skill and scripts keep working. The embedding server decides who the caller is
//! and puts a [`Caller`] in the request extensions.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};

use crate::{now_ms, Caller, Lab};

type L = State<Arc<Lab>>;
type Q = Query<std::collections::HashMap<String, String>>;

fn e(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

fn ok(v: Value) -> Response {
    let mut r = Json(v).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    r
}

macro_rules! need {
    ($c:expr, admin) => {
        if !$c.admin() {
            return e(StatusCode::UNAUTHORIZED, "sign in (admin) required");
        }
    };
    ($c:expr, api) => {
        if !$c.api() {
            return e(StatusCode::UNAUTHORIZED, "sign in or API token required");
        }
    };
    ($c:expr, agent) => {
        if $c != Caller::Agent {
            return e(StatusCode::UNAUTHORIZED, "bad agent token");
        }
    };
}

/// JSON body, gunzipped when the agent compressed it.
fn body_json(headers: &HeaderMap, bytes: &Bytes, limit: usize) -> Result<Value, Response> {
    let raw: Vec<u8> = if headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()) == Some("gzip") {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..]).take(64 * 1024 * 1024).read_to_end(&mut out).map_err(|x| e(StatusCode::BAD_REQUEST, format!("bad gzip body: {x}")))?;
        out
    } else {
        bytes.to_vec()
    };
    if raw.len() > limit {
        return Err(e(StatusCode::PAYLOAD_TOO_LARGE, "body too large"));
    }
    if raw.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&raw).map_err(|x| e(StatusCode::BAD_REQUEST, format!("bad JSON: {x}")))
}

macro_rules! body {
    ($h:expr, $b:expr) => {
        match body_json(&$h, &$b, 2_000_000) {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
    ($h:expr, $b:expr, $limit:expr) => {
        match body_json(&$h, &$b, $limit) {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

pub fn router(lab: Arc<Lab>) -> Router {
    Router::new()
        .route("/api/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/api/apps", get(apps_list).post(apps_upsert))
        .route("/api/apps/import", post(apps_import))
        .route("/api/apps/{id}", axum::routing::patch(apps_patch).delete(apps_delete))
        .route("/api/hosts", get(hosts))
        .route("/api/overview", get(overview))
        .route("/api/stacks", get(queue_list).post(queue_submit))
        .route("/api/stacks/{id}", get(queue_get))
        .route("/api/stacks/{id}/{decision}", post(queue_decide))
        .route("/api/catalog", get(catalog))
        .route("/api/metrics/series", get(metric_series))
        .route("/api/metrics/query", get(metric_query))
        .route("/api/metrics/stats", get(metric_stats))
        .route("/api/views", get(views_list).post(views_save))
        .route("/api/views/{id}", axum::routing::delete(views_delete))
        .route("/api/layout/groups", get(groups_get).put(groups_put).post(groups_add))
        .route("/api/layout/groups/rename", post(groups_rename))
        .route("/api/layout/groups/delete", post(groups_delete))
        .route("/api/layout/apps", post(layout_apps))
        .route("/api/checks", get(checks_get).put(checks_put))
        .route("/api/demo", get(demo_get).put(demo_put))
        .route("/api/demo/exit", post(demo_exit))
        .route("/api/watches", get(watches_list).post(watches_add))
        .route("/api/watches/_debug", get(watches_debug))
        .route("/api/watches/{id}", axum::routing::patch(watches_patch).delete(watches_delete))
        .route("/api/watches/{id}/check", post(watches_check))
        .route("/api/projects", get(projects))
        .route("/api/projects/{host}/{name}/{op}", get(project_file_get).put(project_file_put).post(project_op))
        .route("/api/jobs/{id}", get(job_get))
        .route("/api/activity", get(activity))
        .route("/api/known", post(known_add).delete(known_clear))
        .route("/api/notify/test", post(notify_test))
        .route("/api/widgets/station", get(station_status))
        .route("/api/widgets/station/{mode}", post(station_mode))
        .route("/api/auth/check", post(auth_check))
        .route("/api/ingest", post(ingest))
        .route("/api/agent/jobs", get(agent_jobs))
        .route("/api/jobs/result", post(job_result))
        .route("/api/logs/ingest", post(logs_ingest))
        .route("/api/logs/sources", get(logs_sources))
        .route("/api/logs/search", get(logs_search))
        .route("/api/logs", get(logs_tail))
        .route("/api/assistant/chat", post(claw_chat))
        .route("/api/assistant/events", get(claw_events))
        .route("/api/assistant/stop", post(claw_stop))
        .route("/api/assistant/approve", post(claw_approve))
        .route("/api/icon", get(icon))
        .route("/mcp", post(mcp))
        .route("/message", post(gotify))
        .with_state(lab)
}

// ---- apps ----

async fn apps_list(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    let list: Vec<Value> = lab
        .apps
        .list
        .read()
        .unwrap()
        .iter()
        .map(|a| {
            let mut v = serde_json::to_value(a).unwrap();
            v["health"] = lab.apps.health_of(&a.id);
            v["known"] = lab.apps.known_for(&lab, Some(&a.id), None, None, None).unwrap_or(Value::Null);
            v
        })
        .collect();
    ok(json!(list))
}

async fn apps_upsert(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, api);
    let b = body!(h, b);
    if b["name"].as_str().is_none_or(|s| s.is_empty()) || b["href"].as_str().is_none_or(|s| s.is_empty()) {
        return e(StatusCode::BAD_REQUEST, "name and href required");
    }
    let a = lab.apps.upsert(&b);
    let l = lab.clone();
    let a2 = a.clone();
    tokio::spawn(async move { crate::apps::check_app(&l, &a2).await });
    ok(serde_json::to_value(a).unwrap())
}

async fn apps_import(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, admin);
    match lab.apps.import_homepage(&lab.cfg.homepage_services) {
        Ok(n) => ok(json!({"imported": n})),
        Err(x) => e(StatusCode::BAD_REQUEST, x.to_string()),
    }
}

async fn apps_patch(State(lab): L, Extension(c): Extension<Caller>, UrlPath(id): UrlPath<String>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    match lab.apps.patch(&id, &b, &["hidden", "pinned", "group", "name", "href", "ping", "icon", "description", "host", "order", "onDemand", "sensitive"]) {
        Ok(a) => {
            if b.get("href").is_some() || b.get("ping").is_some() {
                let l = lab.clone();
                let a2 = a.clone();
                tokio::spawn(async move { crate::apps::check_app(&l, &a2).await });
            }
            ok(serde_json::to_value(a).unwrap())
        }
        Err(x) => e(StatusCode::NOT_FOUND, x.to_string()),
    }
}

async fn apps_delete(State(lab): L, Extension(c): Extension<Caller>, UrlPath(id): UrlPath<String>) -> Response {
    need!(c, admin);
    match lab.apps.remove(&id) {
        Ok(v) => ok(v),
        Err(x) => e(StatusCode::NOT_FOUND, x.to_string()),
    }
}

// ---- fleet ----

/// `?history=N`: only the newest N history points per host (default all 360).
async fn hosts(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, api);
    let mut v = lab.fleet.view();
    if let Some(n) = q.get("history").and_then(|n| n.parse::<usize>().ok()) {
        for h in v.iter_mut() {
            if let Some(a) = h["history"].as_array_mut() {
                let len = a.len();
                a.drain(0..len.saturating_sub(n));
            }
        }
    }
    ok(json!(v))
}

async fn overview(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    let hv = lab.fleet.view();
    let al = crate::fleet::alerts(&lab);
    let apps = lab.apps.list.read().unwrap().clone();
    let visible: Vec<_> = apps.iter().filter(|a| !a.hidden).collect();
    let up = visible.iter().filter(|a| lab.apps.state_of(&a.id) == "up").count();
    let known = |id: &str| lab.apps.known_for(&lab, Some(id), None, None, None).is_some();
    let down = visible.iter().filter(|a| lab.apps.state_of(&a.id) == "down" && !known(&a.id)).count();
    let kn = visible.iter().filter(|a| known(&a.id)).count();
    let status = if al.iter().any(|a| a["sev"] == "crit") { "crit" } else if al.iter().any(|a| a["sev"] == "warn") { "warn" } else { "ok" };
    ok(json!({
        "hosts": {"total": hv.len(), "online": hv.iter().filter(|h| h["online"] == true).count()},
        "apps": {"total": visible.len(), "up": up, "down": down, "known": kn},
        "alerts": al, "status": status,
        "openclaw": !lab.cfg.openclaw_url.is_empty() && !lab.cfg.openclaw_password.is_empty(),
        "local": lab.local_name(),
    }))
}

async fn ingest(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, agent);
    let snap = body!(h, b, 16_000_000);
    if snap["host"].as_str().is_none_or(|s| s.is_empty()) {
        return e(StatusCode::BAD_REQUEST, "host required");
    }
    lab.fleet.ingest(&lab.metrics, snap, true);
    ok(json!({"ok": true}))
}

// ---- install queue + jobs ----

async fn queue_list(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    let mut q = lab.jobs.queue.lock().unwrap().clone();
    q.reverse();
    ok(serde_json::to_value(q).unwrap())
}

async fn queue_submit(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, api);
    let b = body!(h, b);
    match crate::jobs::submit(&lab, &b, if c.admin() { "admin" } else { "openclaw" }) {
        Ok(it) => (StatusCode::CREATED, Json(json!({"id": it.id, "status": "pending", "message": "Submitted. A human must approve it in OG-Host → Apps before it runs."}))).into_response(),
        Err(x) => e(StatusCode::BAD_REQUEST, x.to_string()),
    }
}

async fn queue_get(State(lab): L, Extension(c): Extension<Caller>, UrlPath(id): UrlPath<String>) -> Response {
    need!(c, api);
    match lab.jobs.queue_item(&id) {
        Some(it) => ok(serde_json::to_value(it).unwrap()),
        None => e(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn queue_decide(State(lab): L, Extension(c): Extension<Caller>, UrlPath((id, decision)): UrlPath<(String, String)>) -> Response {
    need!(c, admin);
    if decision != "approve" && decision != "reject" {
        return e(StatusCode::NOT_FOUND, "not found");
    }
    match crate::jobs::decide(&lab, &id, decision == "approve", "admin") {
        Ok(it) => ok(serde_json::to_value(it).unwrap()),
        Err(x) => e(if x.to_string() == "not found" { StatusCode::NOT_FOUND } else { StatusCode::CONFLICT }, x.to_string()),
    }
}

async fn agent_jobs(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, agent);
    let host = q.get("host").cloned().unwrap_or_default();
    let wait = q.get("wait").and_then(|w| w.parse::<u64>().ok()).unwrap_or(25).min(30);
    ok(json!({"jobs": lab.jobs.poll(&host, Duration::from_secs(wait)).await}))
}

async fn job_result(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, agent);
    let b = body!(h, b, 16_000_000);
    let id = b["id"].as_str().unwrap_or("");
    if crate::jobs::finish(&lab, id, b["ok"] == true, b["log"].as_str().unwrap_or(""), b.get("data").cloned().filter(|d| !d.is_null())) {
        ok(json!({"ok": true}))
    } else {
        e(StatusCode::NOT_FOUND, "no such job")
    }
}

async fn job_get(State(lab): L, Extension(c): Extension<Caller>, UrlPath(id): UrlPath<String>) -> Response {
    need!(c, admin);
    match lab.jobs.get(&id) {
        Some(j) => ok(j),
        None => e(StatusCode::NOT_FOUND, "job expired or unknown"),
    }
}

async fn projects(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    let known = lab.apps.known.read().unwrap().clone();
    ok(json!(lab.fleet.view().iter().filter(|h| h["docker"] == true).map(|h| {
        let host = h["name"].as_str().unwrap_or("");
        json!({"host": host, "online": h["online"], "projects": h["projects"].as_array().into_iter().flatten().map(|p| {
            let mut p = p.clone();
            p["known"] = known.get(&format!("project:{host}/{}", p["name"].as_str().unwrap_or(""))).cloned().unwrap_or(Value::Null);
            p
        }).collect::<Vec<_>>()})
    }).collect::<Vec<_>>()))
}

async fn project_file_get(State(lab): L, Extension(c): Extension<Caller>, UrlPath((host, name, op)): UrlPath<(String, String, String)>) -> Response {
    need!(c, admin);
    if op != "file" {
        return e(StatusCode::NOT_FOUND, "not found");
    }
    if crate::jobs::find_project(&lab, &host, &name).is_none() {
        return e(StatusCode::NOT_FOUND, format!("no project {name} on {host}"));
    }
    let id = crate::jobs::enqueue(&lab, &host, json!({"type": "read", "name": name}), "admin", None);
    let j = crate::jobs::wait(&lab, &id, 20_000).await.unwrap_or(Value::Null);
    if j["status"] == "done" {
        ok(j["data"].clone())
    } else {
        e(StatusCode::BAD_GATEWAY, j["log"].as_str().filter(|l| !l.is_empty()).unwrap_or("agent did not answer"))
    }
}

async fn project_file_put(State(lab): L, Extension(c): Extension<Caller>, UrlPath((host, name, op)): UrlPath<(String, String, String)>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    if op != "file" {
        return e(StatusCode::NOT_FOUND, "not found");
    }
    let b = body!(h, b);
    if crate::jobs::find_project(&lab, &host, &name).is_none() {
        return e(StatusCode::NOT_FOUND, format!("no project {name} on {host}"));
    }
    let content = b["content"].as_str().unwrap_or("");
    let valid: Result<Value, _> = serde_yaml::from_str(content);
    if valid.as_ref().map(|v| v.get("services").is_none()).unwrap_or(true) {
        return e(StatusCode::BAD_REQUEST, format!("not valid compose YAML: {}", valid.err().map(|x| x.to_string()).unwrap_or_else(|| "missing services:".into())));
    }
    let id = crate::jobs::enqueue(&lab, &host, json!({"type": "write", "name": name, "content": content, "apply": b["apply"] == true}), "admin", None);
    ok(crate::jobs::wait(&lab, &id, 120_000).await.unwrap_or(Value::Null))
}

async fn project_op(State(lab): L, Extension(c): Extension<Caller>, UrlPath((host, name, op)): UrlPath<(String, String, String)>) -> Response {
    need!(c, admin);
    if !["start", "stop", "restart", "update"].contains(&op.as_str()) {
        return e(StatusCode::NOT_FOUND, "not found");
    }
    let Some(pr) = crate::jobs::find_project(&lab, &host, &name) else { return e(StatusCode::NOT_FOUND, format!("no project {name} on {host}")) };
    if pr["protected"] == true && (op == "stop" || op == "update") {
        return e(StatusCode::FORBIDDEN, format!("{name} is protected (Pterodactyl) — use the panel"));
    }
    let id = crate::jobs::enqueue(&lab, &host, json!({"type": "compose", "name": name, "op": op}), "admin", None);
    ok(crate::jobs::wait(&lab, &id, if op == "update" { 15_000 } else { 60_000 }).await.unwrap_or(Value::Null))
}

async fn activity(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, admin);
    ok(json!(lab.apps.activity()))
}

async fn known_add(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let key = if let (Some(host), Some(name)) = (b["project"]["host"].as_str(), b["project"]["name"].as_str()) {
        format!("project:{host}/{name}")
    } else if let Some(id) = b["appId"].as_str() {
        match lab.apps.find(id).and_then(|a| lab.apps.app_project(&lab, &a)) {
            Some((h, p)) => format!("project:{h}/{p}"),
            None => format!("app:{id}"),
        }
    } else {
        return e(StatusCode::BAD_REQUEST, "project or appId required");
    };
    let entry = json!({"note": b["note"].as_str().unwrap_or("").chars().take(300).collect::<String>(), "since": now_ms()});
    lab.apps.known.write().unwrap().insert(key.clone(), entry.clone());
    lab.apps.save_known();
    lab.apps.log_activity(json!({"by": "admin", "host": key.split([':', '/']).nth(1), "what": format!("marked {} as known issue", key.split_once(':').map(|x| x.1).unwrap_or(&key)), "ok": true}));
    let mut out = entry;
    out["key"] = json!(key);
    ok(out)
}

async fn known_clear(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let key = b["key"].as_str().unwrap_or("").to_string();
    if lab.apps.known.write().unwrap().remove(&key).is_none() {
        return e(StatusCode::NOT_FOUND, "not marked");
    }
    lab.apps.save_known();
    lab.apps.log_activity(json!({"by": "admin", "what": format!("cleared known issue {}", key.split_once(':').map(|x| x.1).unwrap_or(&key)), "ok": true}));
    ok(json!({"ok": true}))
}

// ---- catalog, metrics, views ----

async fn catalog(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, api);
    let q = q.get("q").map(|s| s.to_lowercase()).unwrap_or_default();
    let mut items: Vec<Value> = crate::claw::catalog(&lab)
        .await
        .into_iter()
        .filter(|i| q.is_empty() || i["name"].as_str().unwrap_or("").contains(&q) || i["description"].as_str().unwrap_or("").to_lowercase().contains(&q) || i["category"].as_str().unwrap_or("").to_lowercase().contains(&q))
        .collect();
    items.sort_by(|a, b| b["stars"].as_u64().cmp(&a["stars"].as_u64()));
    items.truncate(60);
    ok(json!(items))
}

async fn metric_series(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, api);
    let l = lab.clone();
    let series = tokio::task::spawn_blocking(move || serde_json::to_value(l.metrics.list_series(q.get("host").map(|s| s.as_str()), q.get("metric").map(|s| s.as_str()))).unwrap()).await.unwrap_or(Value::Null);
    ok(json!({"metrics": crate::metrics::metrics_json(), "series": series}))
}

async fn metric_query(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, api);
    let (Some(host), Some(metric)) = (q.get("host").cloned(), q.get("metric").cloned()) else { return e(StatusCode::BAD_REQUEST, "host and metric required") };
    let label = q.get("label").cloned().unwrap_or_default();
    let num = |k: &str| q.get(k).and_then(|v| v.parse::<f64>().ok()).map(|v| v as i64);
    let (from, to, points) = (num("from"), num("to"), num("points").unwrap_or(600).min(2000));
    let l = lab.clone();
    ok(tokio::task::spawn_blocking(move || l.metrics.query(&host, &metric, &label, from, to, points)).await.unwrap_or(Value::Null))
}

async fn metric_stats(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    ok(lab.metrics.stats())
}

async fn views_list(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    ok(json!(*lab.views.lock().unwrap()))
}

async fn views_save(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let (Some(name), Some(panels)) = (b["name"].as_str(), b["panels"].as_array()) else { return e(StatusCode::BAD_REQUEST, "name and panels required") };
    let v = json!({"id": b["id"].as_str().map(String::from).unwrap_or_else(crate::uuid), "name": name.chars().take(80).collect::<String>(), "panels": panels.iter().take(24).collect::<Vec<_>>(), "range": b["range"].as_str().unwrap_or("24h"), "updatedAt": now_ms()});
    let mut views = lab.views.lock().unwrap();
    views.retain(|x| x["id"] != v["id"]);
    views.push(v.clone());
    crate::store::save(&lab.dir, "views.json", &*views);
    ok(v)
}

async fn views_delete(State(lab): L, Extension(c): Extension<Caller>, UrlPath(id): UrlPath<String>) -> Response {
    need!(c, admin);
    let mut views = lab.views.lock().unwrap();
    views.retain(|x| x["id"] != id.as_str());
    crate::store::save(&lab.dir, "views.json", &*views);
    ok(json!({"ok": true}))
}

// ---- layout ----

async fn groups_get(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    ok(json!({"groups": lab.apps.group_list()}))
}

async fn groups_put(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let mut names: Vec<String> = Vec::new();
    for g in b["groups"].as_array().into_iter().flatten().filter_map(|g| g.as_str()) {
        let g: String = g.trim().chars().take(60).collect();
        if !g.is_empty() && !names.contains(&g) {
            names.push(g);
        }
    }
    lab.apps.set_groups(names);
    ok(json!({"groups": lab.apps.group_list()}))
}

async fn groups_add(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let name: String = b["name"].as_str().unwrap_or("").trim().chars().take(60).collect();
    if name.is_empty() {
        return e(StatusCode::BAD_REQUEST, "name required");
    }
    let mut g = lab.apps.group_list();
    if g.contains(&name) {
        return e(StatusCode::CONFLICT, "that group already exists");
    }
    g.push(name);
    lab.apps.set_groups(g);
    ok(json!({"groups": lab.apps.group_list()}))
}

async fn groups_rename(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let from = b["from"].as_str().unwrap_or("").to_string();
    let to: String = b["to"].as_str().unwrap_or("").trim().chars().take(60).collect();
    let g = lab.apps.group_list();
    if to.is_empty() || !g.contains(&from) {
        return e(StatusCode::BAD_REQUEST, "unknown group or empty name");
    }
    for a in lab.apps.list.write().unwrap().iter_mut().filter(|a| a.group == from) {
        a.group = to.clone();
    }
    let next: Vec<String> = if g.contains(&to) { g.into_iter().filter(|x| *x != from).collect() } else { g.into_iter().map(|x| if x == from { to.clone() } else { x }).collect() };
    lab.apps.save();
    lab.apps.set_groups(next);
    ok(json!({"groups": lab.apps.group_list()}))
}

async fn groups_delete(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let name = b["name"].as_str().unwrap_or("").to_string();
    let moved = {
        let mut apps = lab.apps.list.write().unwrap();
        let in_group = apps.iter().filter(|a| a.group == name).count();
        if name == crate::apps::UNGROUPED && in_group > 0 {
            return e(StatusCode::BAD_REQUEST, "move the apps out of Ungrouped first");
        }
        let mut n = apps.iter().filter(|a| a.group == crate::apps::UNGROUPED).map(|a| a.order).fold(0.0, f64::max);
        for a in apps.iter_mut().filter(|a| a.group == name) {
            a.group = crate::apps::UNGROUPED.into();
            n += 1.0;
            a.order = n;
        }
        in_group
    };
    let mut g: Vec<String> = lab.apps.group_list().into_iter().filter(|x| *x != name).collect();
    if moved > 0 && !g.iter().any(|x| x == crate::apps::UNGROUPED) {
        g.push(crate::apps::UNGROUPED.into());
    }
    lab.apps.save();
    lab.apps.set_groups(g);
    ok(json!({"groups": lab.apps.group_list(), "moved": moved}))
}

async fn layout_apps(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    {
        let mut apps = lab.apps.list.write().unwrap();
        for x in b["items"].as_array().into_iter().flatten() {
            if let Some(a) = apps.iter_mut().find(|a| x["id"] == a.id.as_str()) {
                if let Some(g) = x["group"].as_str() {
                    a.group = g.chars().take(60).collect();
                }
                if let Some(o) = x["order"].as_f64() {
                    a.order = o;
                }
            }
        }
    }
    lab.apps.save();
    let g = lab.apps.group_list();
    lab.apps.set_groups(g);
    ok(json!({"ok": true, "groups": lab.apps.group_list()}))
}

async fn checks_get(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    ok(lab.fleet.checks.read().unwrap().clone())
}

async fn checks_put(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let mut cur = lab.fleet.checks.read().unwrap().clone();
    for k in ["services", "containers", "exitNode", "egress", "gamingStops", "quietLogs"] {
        if let Some(v) = b.get(k) {
            cur[k] = v.clone();
        }
    }
    lab.fleet.save_checks(cur.clone());
    ok(cur)
}

async fn demo_get(State(lab): L) -> Response {
    let mut d = lab.apps.demo.read().unwrap().clone();
    if let Some(o) = d.as_object_mut() {
        o.remove("exitCode");
    }
    ok(d)
}

async fn demo_put(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    let prev_code = lab.apps.demo.read().unwrap()["exitCode"].as_str().unwrap_or("password").to_string();
    let uniq = |v: &Value, lower: bool| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for x in v.as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
            let x = if lower { x.trim().to_lowercase() } else { x.to_string() };
            if !x.is_empty() && !out.contains(&x) {
                out.push(x);
            }
        }
        out
    };
    let d = json!({"exitCode": b["exitCode"].as_str().filter(|s| !s.is_empty()).map(|s| s.chars().take(64).collect::<String>()).unwrap_or(prev_code), "groups": uniq(&b["groups"], false), "keywords": uniq(&b["keywords"], true)});
    *lab.apps.demo.write().unwrap() = d.clone();
    lab.apps.save_demo();
    ok(d)
}

async fn demo_exit(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    let b = body!(h, b);
    let code = b["code"].as_str().unwrap_or("").trim().to_string();
    let want = lab.apps.demo.read().unwrap()["exitCode"].as_str().unwrap_or("password").to_string();
    let good = crate::safe_eq(&code, &want) || crate::safe_eq(&code, &lab.secrets.admin_key) || c.admin() && code.is_empty();
    if !good {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    (if good { StatusCode::OK } else { StatusCode::UNAUTHORIZED }, Json(json!({"ok": good}))).into_response()
}

// ---- watches ----

async fn watches_list(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    let l = lab.clone();
    ok(tokio::task::spawn_blocking(move || {
        json!(l.watches.public().into_iter().map(|mut w| {
            let id = w["id"].as_str().unwrap_or("").to_string();
            w["trend"] = crate::watches::trend(&l, &id);
            w
        }).collect::<Vec<_>>())
    })
    .await
    .unwrap_or(Value::Null))
}

async fn watches_debug(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    ok(lab.watches.debug())
}

async fn watches_add(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    match lab.watches.add(&b, "admin") {
        Ok(w) => ok(serde_json::to_value(w).unwrap()),
        Err(x) => e(StatusCode::BAD_REQUEST, x.to_string()),
    }
}

async fn watches_patch(State(lab): L, Extension(c): Extension<Caller>, UrlPath(id): UrlPath<String>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    match lab.watches.update(&id, &b) {
        Ok(w) => ok(serde_json::to_value(w).unwrap()),
        Err(x) => e(StatusCode::BAD_REQUEST, x.to_string()),
    }
}

async fn watches_delete(State(lab): L, Extension(c): Extension<Caller>, UrlPath(id): UrlPath<String>) -> Response {
    need!(c, admin);
    ok(json!({"ok": lab.watches.remove(&id)}))
}

async fn watches_check(State(lab): L, Extension(c): Extension<Caller>, UrlPath(id): UrlPath<String>) -> Response {
    need!(c, admin);
    match lab.watches.check_now(&id) {
        Ok(()) => ok(json!({"ok": true})),
        Err(x) => e(StatusCode::BAD_REQUEST, x.to_string()),
    }
}

// ---- logs ----

async fn logs_ingest(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, agent);
    let b = body!(h, b, 64_000_000);
    let (Some(host), Some(lines)) = (b["host"].as_str().map(String::from), b["lines"].as_array().cloned()) else { return e(StatusCode::BAD_REQUEST, "host and lines required") };
    let n = lines.len();
    let l = lab.clone();
    let _ = tokio::task::spawn_blocking(move || {
        l.logs.ingest(&host, &lines);
        l.fleet.count_log_errors(&l.metrics, &host, &lines);
    })
    .await;
    ok(json!({"ok": true, "n": n}))
}

async fn logs_sources(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    let l = lab.clone();
    ok(tokio::task::spawn_blocking(move || json!({"sources": l.logs.list_sources(), "stats": l.logs.stats()})).await.unwrap_or(Value::Null))
}

async fn logs_search(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, api);
    let l = lab.clone();
    ok(tokio::task::spawn_blocking(move || {
        let num = |k: &str| q.get(k).and_then(|v| v.parse::<f64>().ok());
        let (r, truncated) = l.logs.search(q.get("q").map(|s| s.as_str()).unwrap_or(""), q.get("host").map(|s| s.as_str()), q.get("level").map(|s| s.as_str()).unwrap_or("all"), num("hours").unwrap_or(24.0), num("limit").unwrap_or(300.0) as usize, num("from").map(|v| v as i64), num("to").map(|v| v as i64));
        json!({"results": r, "truncated": truncated})
    })
    .await
    .unwrap_or(Value::Null))
}

async fn logs_tail(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, api);
    let (Some(host), Some(kind), Some(source)) = (q.get("host").cloned(), q.get("kind").cloned(), q.get("source").cloned()) else { return e(StatusCode::BAD_REQUEST, "host, kind, source required") };
    let l = lab.clone();
    ok(tokio::task::spawn_blocking(move || {
        let num = |k: &str| q.get(k).and_then(|v| v.parse::<f64>().ok());
        json!(l.logs.tail(&host, &kind, &source, num("limit").unwrap_or(300.0) as usize, num("after").map(|v| v as i64), num("before").map(|v| v as i64), q.get("level").map(|s| s.as_str()).unwrap_or("all"), q.get("q").map(|s| s.as_str()).unwrap_or("")))
    })
    .await
    .unwrap_or(Value::Null))
}

// ---- notify, station, auth ----

async fn notify_test(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, admin);
    match crate::notify::send_signal(&lab, "🟣 Test from OG-Host — notifications are working.").await {
        Ok(()) => ok(json!({"ok": true})),
        Err(x) => ok(json!({"ok": false, "error": x})),
    }
}

async fn gotify(State(lab): L, Query(q): Q, h: HeaderMap, b: Bytes) -> Response {
    let tok = q.get("token").cloned().or_else(|| h.get("x-gotify-key").and_then(|v| v.to_str().ok()).map(String::from)).unwrap_or_default();
    if !crate::safe_eq(&tok, &lab.secrets.notify_token) {
        return e(StatusCode::UNAUTHORIZED, "bad token");
    }
    let is_json = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("json"));
    let body: Value = if is_json {
        body!(h, b)
    } else {
        let form: std::collections::HashMap<String, String> = String::from_utf8_lossy(&b).split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.replace('+', " "))).collect();
        json!(form)
    };
    let title = body["title"].as_str().unwrap_or("");
    let msg = body["message"].as_str().unwrap_or("");
    let text = format!("{}{msg}", if title.is_empty() { String::new() } else { format!("{title}\n") });
    match crate::notify::send_signal(&lab, text.trim()).await {
        Ok(()) => ok(json!({"id": now_ms(), "title": title, "message": msg})),
        Err(x) => e(StatusCode::BAD_GATEWAY, x),
    }
}

async fn station_status(State(lab): L, Extension(c): Extension<Caller>) -> Response {
    need!(c, api);
    ok(crate::claw::station(&lab, "/status").await)
}

async fn station_mode(State(lab): L, Extension(c): Extension<Caller>, UrlPath(mode): UrlPath<String>) -> Response {
    need!(c, admin);
    if mode != "gaming" && mode != "ai" {
        return e(StatusCode::NOT_FOUND, "not found");
    }
    let r = crate::claw::station(&lab, &format!("/{mode}")).await;
    ok(json!({"result": r, "status": crate::claw::station(&lab, "/status").await}))
}

async fn auth_check(Extension(c): Extension<Caller>) -> Response {
    (if c.admin() { StatusCode::OK } else { StatusCode::UNAUTHORIZED }, Json(json!({"ok": c.admin()}))).into_response()
}

// ---- assistant ----

async fn claw_chat(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    crate::claw::chat(lab, b).await
}

async fn claw_events(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, admin);
    crate::claw::events(lab, q.get("thread").map(|s| s.as_str()).unwrap_or("")).await
}

async fn claw_stop(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    ok(crate::claw::stop(&lab, b["thread"].as_str().unwrap_or("")).await)
}

async fn claw_approve(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, admin);
    let b = body!(h, b);
    crate::claw::approve(&lab, b["id"].as_str().unwrap_or(""), b["decision"].as_str().unwrap_or("deny")).await
}

// ---- icons ----

async fn icon(State(lab): L, Extension(c): Extension<Caller>, Query(q): Q) -> Response {
    need!(c, api);
    let src = q.get("src").cloned().unwrap_or_default();
    match crate::apps::icon(&lab, &src).await {
        Some((ct, bytes)) => Response::builder()
            .header(header::CONTENT_TYPE, ct)
            .header(header::CACHE_CONTROL, "private, max-age=604800")
            // an SVG opened directly can't run scripts or load anything
            .header(header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'; sandbox")
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .body(Body::from(bytes))
            .unwrap(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---- MCP ----

async fn mcp(State(lab): L, Extension(c): Extension<Caller>, h: HeaderMap, b: Bytes) -> Response {
    need!(c, api);
    let msg = body!(h, b);
    match crate::mcp::handle(&lab, msg).await {
        Some(v) => Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}
