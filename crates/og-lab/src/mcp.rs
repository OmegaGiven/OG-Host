//! MCP server for OpenClaw (streamable HTTP with JSON responses), OmegaEnv's tool set.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::{fmt_bytes, Lab};

pub const GUIDE: &str = "OG-Host (the homelab control panel, OmegaEnv's successor) — how it is put together
TABS: Home (app tiles grouped by \"group\", a widget row incl. station AI/Gaming mode), Monitor (machines, storage, alerts, app health table), Explore (metric history), Watch, Apps (install queue + catalog + compose apps on every machine), Logs (every container + systemd unit on every machine), Server (this OG-Host node's own containers, stacks, images).
THREE DIFFERENT THINGS — do not mix them up:
1. APP TILE = a link on the Home screen (name, href, icon, group, health check URL). Only a dashboard entry. Tools: list_apps, add_app_tile, update_app_tile (rename/regroup/hide/show/pin/change url or icon), remove_app_tile. Tile changes apply immediately, no approval needed, and never touch containers.
   - hidden=true keeps the tile but hides it (reversible). remove_app_tile deletes it and remembers that, so it will not come back.
2. CONTAINER / SERVICE = something actually running on a machine. See get_host (containers), tail_logs/search_logs. Use propose_project_action for compose apps; for anything else say what the human should run.
2b. COMPOSE APP (project) = any docker compose app on a machine (list_projects). You can read its file (get_compose_file) and PROPOSE start/stop/restart/update (propose_project_action) or a new compose file (propose_compose_edit); the human approves each in Apps. Pterodactyl's wings/panel are protected from stop/update.
3. STACK = a docker compose project that OG-Host itself installed via submit_install. submit_install action \"remove\" ONLY works for these; list_stacks shows them. Every stack change waits for the human to click Approve in Apps.
Removing an app completely therefore = remove_app_tile (dashboard) + (if OG-Host installed it) submit_install action:\"remove\", otherwise tell the human the command to stop it.
Be exact about what you did: never say something is removed/installed unless the tool result says so.";

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

pub fn tools() -> Vec<Value> {
    let s = || json!({"type": "string"});
    let n = || json!({"type": "number"});
    let b = || json!({"type": "boolean"});
    vec![
        json!({"name": "lab_overview", "description": "Overall homelab status: hosts online, app health counts and current alerts.", "inputSchema": obj(json!({}), &[])}),
        json!({"name": "get_host", "description": "Full detail for one machine: CPU, memory, GPUs, disks, network, containers (with ports, cpu, memory) and Proxmox guests.", "inputSchema": obj(json!({"name": s()}), &["name"])}),
        json!({"name": "list_apps", "description": "Every Home screen tile (id, name, group, href, host, hidden, source) with live health. Tiles are dashboard links, not containers.", "inputSchema": obj(json!({}), &[])}),
        json!({"name": "used_ports", "description": "Host TCP ports already in use on a machine (listening sockets + container port maps). Check this before choosing a port for a new app.", "inputSchema": obj(json!({"host": s()}), &["host"])}),
        json!({"name": "search_catalog", "description": "Search the linuxserver.io community container catalog. Returns image, description and the known ports, volumes and env vars.", "inputSchema": obj(json!({"query": s()}), &["query"])}),
        json!({"name": "submit_install", "description": "Queue a docker compose stack for a host. It is NOT run until the human approves it in OG-Host → Apps. Include an app tile so it appears on the dashboard once installed. Use action \"remove\" with just host+name to queue removal.",
            "inputSchema": obj(json!({"host": {"type": "string", "description": "machine name from lab_overview"}, "name": {"type": "string", "description": "stack name, lowercase"}, "compose": {"type": "string", "description": "full docker-compose.yml contents"}, "notes": {"type": "string", "description": "short explanation for the human"}, "action": {"type": "string", "enum": ["deploy", "remove"]}, "app": {"type": "object", "description": "dashboard tile", "properties": {"name": s(), "href": s(), "icon": s(), "group": s(), "description": s()}}}), &["host", "name"])}),
        json!({"name": "install_status", "description": "Status and log of a queued install (pending, approved, deploying, done, failed, rejected).", "inputSchema": obj(json!({"id": s()}), &["id"])}),
        json!({"name": "search_logs", "description": "Search logs from every machine (containers and systemd units). Returns newest matches first. Use level \"error\" to find problems.", "inputSchema": obj(json!({"query": s(), "host": s(), "level": {"type": "string", "enum": ["all", "warn", "error"]}, "hours": n(), "limit": n()}), &[])}),
        json!({"name": "tail_logs", "description": "Latest log lines of one container or systemd unit on a machine.", "inputSchema": obj(json!({"host": s(), "source": s(), "kind": {"type": "string", "enum": ["container", "unit"]}, "lines": n(), "level": {"type": "string", "enum": ["all", "warn", "error"]}}), &["host", "source"])}),
        json!({"name": "noisy_logs", "description": "Containers/services that logged 10+ errors in the last 15 minutes.", "inputSchema": obj(json!({}), &[])}),
        json!({"name": "list_metrics", "description": "Metric history series that exist (host, metric, label). Hosts are machine names plus \"apps\" for app health. Use before query_metrics.", "inputSchema": obj(json!({"host": s(), "metric": s()}), &[])}),
        json!({"name": "query_metrics", "description": "History of one metric: min/avg/max over the window plus ~40 points. Kept 2 days at 10 s, 30 days at 1 min, 1 year at 1 h.", "inputSchema": obj(json!({"host": s(), "metric": s(), "label": s(), "hours": n(), "from": s(), "to": s()}), &["host", "metric"])}),
        json!({"name": "list_projects", "description": "Docker compose apps on each machine (running or stopped): name, folder, compose file, services and their state, protected flag.", "inputSchema": obj(json!({"host": s()}), &[])}),
        json!({"name": "get_compose_file", "description": "Read the docker-compose file of an existing app on a machine.", "inputSchema": obj(json!({"host": s(), "project": s()}), &["host", "project"])}),
        json!({"name": "propose_project_action", "description": "Ask the human to start, stop, restart or update (pull new images + recreate) an existing compose app. Waits for approval in OG-Host → Apps.", "inputSchema": obj(json!({"host": s(), "project": s(), "op": {"type": "string", "enum": ["start", "stop", "restart", "update"]}, "notes": s()}), &["host", "project", "op"])}),
        json!({"name": "propose_compose_edit", "description": "Propose a new docker-compose file for an existing app (read it first with get_compose_file and change only what is needed). The human sees a diff and approves.", "inputSchema": obj(json!({"host": s(), "project": s(), "compose": s(), "notes": s()}), &["host", "project", "compose"])}),
        json!({"name": "report_watch", "description": "ONLY for automated watch checks: report the result of the check you were asked to do (id and token come from the request).", "inputSchema": obj(json!({"id": s(), "token": s(), "status": {"type": "string", "enum": ["ok", "blocked", "error"]}, "in_stock": b(), "price": n(), "summary": s(), "changed": b()}), &["id", "token", "status", "summary"])}),
        json!({"name": "list_watches", "description": "Web pages OG-Host watches for restocks, prices or changes, with the latest result.", "inputSchema": obj(json!({}), &[])}),
        json!({"name": "add_watch", "description": "Start watching a web page. kind \"restock\", \"price\" (optional targetPrice), or \"change\" (give a question saying what matters). OG-Host checks it on schedule and messages the user on Signal when it matters.", "inputSchema": obj(json!({"name": s(), "url": s(), "notify": b(), "kind": {"type": "string", "enum": ["restock", "price", "change"]}, "question": s(), "targetPrice": n(), "everyMin": n()}), &["name", "url"])}),
        json!({"name": "remove_watch", "description": "Stop watching a page (id from list_watches).", "inputSchema": obj(json!({"id": s()}), &["id"])}),
        json!({"name": "dashboard_guide", "description": "READ FIRST when changing the dashboard: how tiles, containers and stacks differ and which tool changes what.", "inputSchema": obj(json!({}), &[])}),
        json!({"name": "update_app_tile", "description": "Change a Home screen tile: hide/show, pin, rename, move to another group, change href/ping/icon/description. Applies immediately; does not touch containers.", "inputSchema": obj(json!({"id": s(), "hidden": b(), "pinned": b(), "name": s(), "group": s(), "href": s(), "ping": s(), "icon": s(), "description": s()}), &["id"])}),
        json!({"name": "remove_app_tile", "description": "Delete a tile from the Home screen for good. Dashboard only — the app/container keeps running.", "inputSchema": obj(json!({"id": s()}), &["id"])}),
        json!({"name": "list_stacks", "description": "Compose stacks OG-Host installed (or was asked to), with status. Only these can be removed with submit_install action \"remove\".", "inputSchema": obj(json!({}), &[])}),
        json!({"name": "add_app_tile", "description": "Add or update an app tile on the home screen (for apps that already run).", "inputSchema": obj(json!({"name": s(), "href": s(), "ping": s(), "icon": s(), "group": s(), "description": s(), "host": s()}), &["name", "href"])}),
    ]
}

fn host_summary(h: &Value) -> Value {
    let mem = &h["mem"];
    let f = |v: &Value| v.as_f64().unwrap_or(0.0);
    json!({
        "name": h["name"], "online": h["online"], "parent": h["parent"], "os": h["os"], "docker": h["docker"].as_bool().unwrap_or(false),
        "cpuPct": h["cpu"]["pct"], "cores": h["cpu"]["cores"],
        "memUsed": fmt_bytes(f(&mem["total"]) - f(&mem["available"])), "memTotal": fmt_bytes(f(&mem["total"])),
        "gpus": h["gpus"].as_array().into_iter().flatten().map(|g| format!("{} {}% util, {}/{} VRAM, {}°C", g["name"].as_str().unwrap_or(""), g["util"], fmt_bytes(f(&g["memUsed"])), fmt_bytes(f(&g["memTotal"])), g["temp"])).collect::<Vec<_>>(),
        "disks": h["disks"].as_array().into_iter().flatten().map(|d| format!("{}: {} free of {}", d["mount"].as_str().unwrap_or(""), fmt_bytes(d["avail"].as_f64().unwrap_or(f(&d["total"]) - f(&d["used"]))), fmt_bytes(f(&d["total"])))).collect::<Vec<_>>(),
        "swapUsed": fmt_bytes(f(&mem["swapTotal"]) - f(&mem["swapFree"])), "containers": h["containers"].as_array().map(|a| a.len()).unwrap_or(0),
    })
}

fn iso(ms: i64) -> String {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec, ms % 1000)
}

fn parse_iso(s: &str) -> Option<i64> {
    // YYYY-MM-DDTHH:MM[:SS][Z]
    let (d, t) = s.split_once('T')?;
    let dp: Vec<i32> = d.split('-').filter_map(|x| x.parse().ok()).collect();
    let tp: Vec<i32> = t.trim_end_matches('Z').split(':').filter_map(|x| x.split('.').next()?.parse().ok()).collect();
    if dp.len() != 3 || tp.len() < 2 {
        return None;
    }
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = dp[0] - 1900;
    tm.tm_mon = dp[1] - 1;
    tm.tm_mday = dp[2];
    tm.tm_hour = tp[0];
    tm.tm_min = tp[1];
    tm.tm_sec = *tp.get(2).unwrap_or(&0);
    Some(unsafe { libc::timegm(&mut tm) } as i64 * 1000)
}

pub async fn call(lab: &Arc<Lab>, name: &str, a: &Value) -> Result<Value> {
    let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
    Ok(match name {
        "lab_overview" => json!({"hosts": lab.fleet.view().iter().map(host_summary).collect::<Vec<_>>(), "alerts": crate::fleet::alerts(lab)}),
        "get_host" => {
            let mut h = lab.fleet.view().into_iter().find(|h| h["name"] == a["name"]).ok_or_else(|| anyhow!("unknown host {}", s("name")))?;
            h.as_object_mut().unwrap().remove("history");
            h
        }
        "list_apps" => json!(lab.apps.list.read().unwrap().iter().map(|x| json!({"id": x.id, "name": x.name, "group": x.group, "href": x.href, "host": x.host, "hidden": x.hidden, "pinned": x.pinned, "source": x.source, "health": lab.apps.state_of(&x.id)})).collect::<Vec<_>>()),
        "used_ports" => {
            let h = lab.fleet.latest(&s("host")).ok_or_else(|| anyhow!("unknown host {}", s("host")))?;
            let mut ports: Vec<u64> = h["listening"].as_array().into_iter().flatten().filter_map(|p| p.as_u64()).collect();
            for c in h["containers"].as_array().into_iter().flatten() {
                let p = c["ports"].as_str().unwrap_or("");
                for part in p.split("->").collect::<Vec<_>>().iter().rev().skip(1) {
                    if let Some(n) = part.rsplit(':').next().and_then(|x| x.parse().ok()) {
                        ports.push(n);
                    }
                }
            }
            ports.sort();
            ports.dedup();
            json!(ports)
        }
        "search_catalog" => {
            let q = s("query").to_lowercase();
            json!(crate::claw::catalog(lab).await.into_iter().filter(|i| i["name"].as_str().unwrap_or("").contains(&q) || i["description"].as_str().unwrap_or("").to_lowercase().contains(&q)).take(8).collect::<Vec<_>>())
        }
        "submit_install" => {
            let it = crate::jobs::submit(lab, a, "openclaw")?;
            json!({"id": it.id, "status": "pending", "message": format!("Queued. Tell the user to review and approve it in OG-Host → Apps ({}).", lab.link("#apps"))})
        }
        "install_status" => {
            let it = lab.jobs.queue_item(&s("id")).ok_or_else(|| anyhow!("not found"))?;
            json!({"name": it.name, "host": it.host, "status": it.status, "log": it.log})
        }
        "search_logs" => {
            let (r, _) = lab.logs.search(&s("query"), a["host"].as_str(), a["level"].as_str().unwrap_or("all"), a["hours"].as_f64().unwrap_or(6.0), a["limit"].as_u64().unwrap_or(60).min(200) as usize, None, None);
            let lines: Vec<String> = r.iter().map(|x| format!("{} {}/{} [{}] {}", iso(x.ts), x.host.as_deref().unwrap_or(""), x.source.as_deref().unwrap_or(""), x.level, x.msg.chars().take(400).collect::<String>())).collect();
            json!(if lines.is_empty() { "no matches".to_string() } else { lines.join("\n") })
        }
        "tail_logs" => {
            let host = s("host");
            let source = s("source");
            let kind = a["kind"].as_str().map(String::from).unwrap_or_else(|| {
                if lab.fleet.latest(&host).is_some_and(|h| h["containers"].as_array().is_some_and(|c| c.iter().any(|x| x["name"] == source.as_str()))) { "container".into() } else { "unit".into() }
            });
            let r = lab.logs.tail(&host, &kind, &source, a["lines"].as_u64().unwrap_or(80).min(400) as usize, None, None, a["level"].as_str().unwrap_or("all"), "");
            let lines: Vec<String> = r.iter().map(|x| format!("{} [{}] {}", iso(x.ts), x.level, x.msg.chars().take(500).collect::<String>())).collect();
            json!(if lines.is_empty() { "no lines".to_string() } else { lines.join("\n") })
        }
        "noisy_logs" => json!(lab.logs.noisy(15, 10)),
        "list_metrics" => json!({
            "metrics": crate::metrics::METRICS.iter().map(|(k, t, u)| (k.to_string(), json!(format!("{t} ({u})")))).collect::<serde_json::Map<_, _>>(),
            "series": lab.metrics.list_series(a["host"].as_str(), a["metric"].as_str()).iter().take(300).map(|x| format!("{} {}{}", x.host, x.metric, if x.label.is_empty() { String::new() } else { format!(" {}", x.label) })).collect::<Vec<_>>(),
        }),
        "query_metrics" => {
            let now = crate::now_ms() as i64;
            let t1 = a["to"].as_str().and_then(parse_iso).unwrap_or(now);
            let t0 = a["from"].as_str().and_then(parse_iso).unwrap_or(t1 - (a["hours"].as_f64().unwrap_or(24.0) * 3_600_000.0) as i64);
            let (host, metric, label) = (s("host"), s("metric"), s("label"));
            let r = lab.metrics.query(&host, &metric, &label, Some(t0), Some(t1), 40);
            let vals: Vec<f64> = r["avg"].as_array().into_iter().flatten().filter_map(|v| v.as_f64()).collect();
            if vals.is_empty() {
                json!(format!("no data for {host} {metric} {label}"))
            } else {
                let maxes: Vec<f64> = r["max"].as_array().into_iter().flatten().filter_map(|v| v.as_f64()).collect();
                let mx = maxes.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let mn = r["min"].as_array().into_iter().flatten().filter_map(|v| v.as_f64()).fold(f64::INFINITY, f64::min);
                let idx = r["max"].as_array().and_then(|a| a.iter().position(|v| v.as_f64() == Some(mx))).unwrap_or(0);
                let ts: Vec<i64> = r["t"].as_array().into_iter().flatten().filter_map(|v| v.as_i64()).collect();
                json!({
                    "series": format!("{host} {metric} {label}").trim(), "unit": r["unit"], "resolution": r["resolution"], "min": mn,
                    "avg": (100.0 * vals.iter().sum::<f64>() / vals.len() as f64).round() / 100.0, "max": mx, "maxAt": ts.get(idx).map(|t| iso(*t)),
                    "points": ts.iter().zip(r["avg"].as_array().into_iter().flatten()).map(|(t, v)| format!("{} {v}", iso(*t))).collect::<Vec<_>>(),
                })
            }
        }
        "list_projects" => json!(lab.fleet.view().iter().filter(|h| h["docker"] == true && a["host"].as_str().is_none_or(|x| h["name"] == x)).map(|h| json!({
            "host": h["name"],
            "projects": h["projects"].as_array().into_iter().flatten().map(|p| json!({"name": p["name"], "folder": p["workingDir"], "file": p["configFiles"][0], "running": format!("{}/{}", p["running"], p["services"].as_array().map(|x| x.len()).unwrap_or(0)), "protected": p["protected"], "services": p["services"].as_array().into_iter().flatten().map(|x| format!("{}:{}", x["service"].as_str().unwrap_or(""), x["state"].as_str().unwrap_or(""))).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>()),
        "get_compose_file" => {
            let (host, project) = (s("host"), s("project"));
            if crate::jobs::find_project(lab, &host, &project).is_none() {
                return Err(anyhow!("no project {project} on {host}"));
            }
            let id = crate::jobs::enqueue(lab, &host, json!({"type": "read", "name": project}), "openclaw", None);
            let j = crate::jobs::wait(lab, &id, 20_000).await.unwrap_or(Value::Null);
            if j["status"] != "done" {
                return Err(anyhow!("{}", j["log"].as_str().filter(|l| !l.is_empty()).unwrap_or("agent did not answer")));
            }
            j["data"].clone()
        }
        "propose_project_action" => {
            let it = crate::jobs::submit(lab, &json!({"action": "op", "host": a["host"], "name": a["project"], "op": a["op"], "notes": a["notes"]}), "openclaw")?;
            json!({"id": it.id, "status": "pending", "message": format!("Waiting for approval at {}", lab.link("#apps"))})
        }
        "propose_compose_edit" => {
            let it = crate::jobs::submit(lab, &json!({"action": "edit", "host": a["host"], "name": a["project"], "compose": a["compose"], "notes": a["notes"]}), "openclaw")?;
            json!({"id": it.id, "status": "pending", "message": format!("Waiting for approval at {}", lab.link("#apps"))})
        }
        "report_watch" => lab.watches.report(lab, a)?,
        "list_watches" => json!(lab.watches.list.lock().unwrap().clone().iter().map(|w| json!({"id": w.id, "name": w.name, "url": w.url, "kind": w.kind, "everyMin": w.every_min, "targetPrice": w.target_price, "enabled": w.enabled, "notify": w.notify, "last": w.last, "trend": crate::watches::trend(lab, &w.id)})).collect::<Vec<_>>()),
        "add_watch" => {
            let w = lab.watches.add(a, "openclaw")?;
            json!({"id": w.id, "message": format!("Watching \"{}\" every {} min. Manage it at {}", w.name, w.every_min, lab.link("#watch"))})
        }
        "remove_watch" => json!({"removed": lab.watches.remove(&s("id"))}),
        "dashboard_guide" => json!(GUIDE),
        "update_app_tile" => {
            let t = lab.apps.patch(&s("id"), a, &["name", "href", "ping", "icon", "group", "description", "host", "hidden", "pinned", "onDemand"])?;
            if a.get("href").is_some() || a.get("ping").is_some() {
                let l = lab.clone();
                let t2 = t.clone();
                tokio::spawn(async move { crate::apps::check_app(&l, &t2).await });
            }
            json!({"ok": true, "tile": {"id": t.id, "name": t.name, "group": t.group, "hidden": t.hidden, "pinned": t.pinned, "href": t.href}})
        }
        "remove_app_tile" => lab.apps.remove(&s("id"))?,
        "list_stacks" => json!(lab.jobs.queue.lock().unwrap().iter().map(|x| json!({"id": x.id, "name": x.name, "host": x.host, "action": x.action, "status": x.status, "createdAt": iso(x.created_at as i64)})).collect::<Vec<_>>()),
        "add_app_tile" => {
            let t = lab.apps.upsert(a);
            let l = lab.clone();
            let t2 = t.clone();
            tokio::spawn(async move { crate::apps::check_app(&l, &t2).await });
            serde_json::to_value(t)?
        }
        _ => return Err(anyhow!("unknown tool {name}")),
    })
}

/// One JSON-RPC message (or a batch).
pub async fn handle(lab: &Arc<Lab>, msg: Value) -> Option<Value> {
    async fn one(lab: &Arc<Lab>, m: &Value) -> Option<Value> {
        let id = m.get("id")?.clone();
        let ok = |r: Value| json!({"jsonrpc": "2.0", "id": id, "result": r});
        let fail = |code: i64, message: String| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        Some(match m["method"].as_str().unwrap_or("") {
            "initialize" => ok(json!({"protocolVersion": m["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "omegaenv", "version": env!("CARGO_PKG_VERSION")}, "instructions": GUIDE})),
            "ping" => ok(json!({})),
            "tools/list" => ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = m["params"]["name"].as_str().unwrap_or("");
                if !tools().iter().any(|t| t["name"] == name) {
                    return Some(fail(-32602, format!("unknown tool {name}")));
                }
                let args = m["params"]["arguments"].clone();
                let args = if args.is_object() { args } else { json!({}) };
                match call(lab, name, &args).await {
                    Ok(out) => ok(json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&out).unwrap_or_default()}]})),
                    Err(e) => ok(json!({"content": [{"type": "text", "text": format!("Error: {e}")}], "isError": true})),
                }
            }
            other => fail(-32601, format!("method not found: {other}")),
        })
    }
    if let Some(batch) = msg.as_array() {
        let mut out = Vec::new();
        for m in batch {
            if let Some(r) = one(lab, m).await {
                out.push(r);
            }
        }
        return (!out.is_empty()).then(|| json!(out));
    }
    one(lab, &msg).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let t = parse_iso("2026-10-21T13:05:00Z").unwrap();
        assert_eq!(iso(t), "2026-10-21T13:05:00.000Z");
        assert_eq!(tools().len(), 25);
    }
}
