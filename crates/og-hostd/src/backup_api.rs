//! HTTP API for datastores, backups, restores, backup jobs and tasks. Privileges come from
//! `auth::rule`; restoring into an arbitrary directory and reading encryption keys
//! additionally need Sys.Modify on /.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use og_access::Who;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::backup::{self, Job, Target};
use crate::serve::Daemon;

type D = State<Arc<Daemon>>;

fn bad(e: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response()
}

fn done(r: anyhow::Result<()>) -> Response {
    match r {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(format!("{e:#}")),
    }
}

fn task_started(r: anyhow::Result<String>) -> Response {
    match r {
        Ok(id) => Json(json!({"task": id})).into_response(),
        Err(e) => bad(format!("{e:#}")),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")))
}

pub fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/v1/storage", get(stores).post(store_put))
        .route("/v1/storage/{id}", get(store_status).put(store_update).delete(store_delete))
        .route("/v1/storage/{id}/key", get(key_get).post(key_set))
        .route("/v1/storage/{id}/backups", get(backups).delete(backup_delete))
        .route("/v1/storage/{id}/backups/notes", post(backup_notes))
        .route("/v1/storage/{id}/backups/files", get(backup_files))
        .route("/v1/storage/{id}/backups/download", get(backup_download))
        .route("/v1/storage/{id}/prune", post(prune))
        .route("/v1/storage/{id}/gc", post(gc))
        .route("/v1/storage/{id}/verify", post(verify))
        .route("/v1/backup", post(backup_now))
        .route("/v1/restore", post(restore))
        .route("/v1/backup/jobs", get(jobs).post(job_put))
        .route("/v1/backup/jobs/{id}", axum::routing::delete(job_delete))
        .route("/v1/backup/jobs/{id}/run", post(job_run))
        .route("/v1/tasks", get(tasks))
        .route("/v1/tasks/{id}", get(task))
        .route("/v1/tasks/{id}/stop", post(task_stop))
        .with_state(d)
}

// ---- datastores ----

async fn stores(State(d): D) -> Json<Value> {
    let st = d.storage.clone();
    let list = tokio::task::spawn_blocking(move || {
        st.list()
            .into_iter()
            .map(|s| {
                let space = st.space(&s);
                json!({"id": s.id, "type": s.kind, "path": s.path, "server": s.server, "export": s.export, "share": s.share, "username": s.username, "domain": s.domain, "options": s.options,
                       "encrypt": s.encrypt, "enabled": s.enabled, "comment": s.comment, "gc_schedule": s.gc_schedule, "mounted": st.mounted(&s),
                       "total": space.map(|x| x.0), "free": space.map(|x| x.1), "key": st.key(&s.id).map(|k| og_backup::Key::new(&k).fingerprint)})
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(json!(list))
}

#[derive(Deserialize)]
struct StoreReq {
    #[serde(flatten)]
    store: crate::storage::Store,
    #[serde(default)]
    password: Option<String>,
}

async fn store_put(State(d): D, Json(b): Json<StoreReq>) -> Response {
    let st = d.storage.clone();
    done(blocking(move || {
        if st.list().iter().any(|s| s.id == b.store.id) {
            anyhow::bail!("a datastore named {} exists already", b.store.id);
        }
        st.upsert(b.store.clone(), b.password.as_deref())?;
        // reachable? (mounts a share)
        st.repo(&b.store.id).map(|_| ())
    })
    .await)
}

async fn store_update(State(d): D, Path(id): Path<String>, Json(mut b): Json<StoreReq>) -> Response {
    b.store.id = id;
    let st = d.storage.clone();
    done(blocking(move || st.upsert(b.store, b.password.as_deref())).await)
}

async fn store_delete(State(d): D, Path(id): Path<String>) -> Response {
    let st = d.storage.clone();
    done(blocking(move || st.remove(&id)).await)
}

async fn store_status(State(d): D, Path(id): Path<String>) -> Response {
    let st = d.storage.clone();
    match blocking(move || {
        let (s, repo) = st.repo(&id)?;
        let (chunks, bytes) = repo.usage();
        let snaps = repo.snapshots();
        let logical: u64 = snaps.iter().map(|m| m.size()).sum();
        Ok(json!({"id": s.id, "chunks": chunks, "bytes": bytes, "backups": snaps.len(), "logical": logical, "dedup": if bytes > 0 { logical as f64 / bytes as f64 } else { 0.0 }, "space": st.space(&s)}))
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => bad(format!("{e:#}")),
    }
}

async fn key_get(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>) -> Response {
    if !d.access.allowed(&w, "/", "Sys.Modify") {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "reading a backup key needs Sys.Modify on /"}))).into_response();
    }
    match d.storage.key(&id) {
        Some(k) => {
            d.access.audit(&w.label(), "", "backup key shown", &id, true);
            Json(json!({"key": hex::encode(k), "fingerprint": og_backup::Key::new(&k).fingerprint})).into_response()
        }
        None => bad("this datastore has no key"),
    }
}

#[derive(Deserialize)]
struct KeyReq {
    key: String,
}

async fn key_set(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>, Json(b): Json<KeyReq>) -> Response {
    if !d.access.allowed(&w, "/", "Sys.Modify") {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "setting a backup key needs Sys.Modify on /"}))).into_response();
    }
    match d.storage.set_key(&id, &b.key) {
        Ok(fp) => Json(json!({"fingerprint": fp})).into_response(),
        Err(e) => bad(e),
    }
}

// ---- backups in a datastore ----

#[derive(Deserialize, Default)]
#[serde(default)]
struct SnapQ {
    snapshot: String,
    archive: String,
    path: String,
    group: String,
}

async fn backups(State(d): D, Path(id): Path<String>, Query(q): Query<SnapQ>) -> Response {
    let st = d.storage.clone();
    match blocking(move || {
        let (_, repo) = st.repo(&id)?;
        Ok(repo.snapshots().into_iter().filter(|m| q.group.is_empty() || m.group == q.group).map(|m| json!({"id": m.id(), "group": m.group, "time": m.time, "host": m.host, "size": m.size(), "archives": m.archives, "notes": m.notes, "protected": m.protected, "encrypted": m.encrypted, "verified": m.verified, "duration_ms": m.duration_ms, "kind": m.meta["kind"], "name": m.meta["name"], "mode": m.meta["mode"], "skipped_binds": m.meta["binds"].as_array().map(|b| b.iter().filter(|x| x["skipped"] == true).count()).unwrap_or(0)})).collect::<Vec<_>>())
    })
    .await
    {
        Ok(v) => Json(json!(v)).into_response(),
        Err(e) => bad(format!("{e:#}")),
    }
}

async fn backup_delete(State(d): D, Path(id): Path<String>, Query(q): Query<SnapQ>) -> Response {
    let st = d.storage.clone();
    done(blocking(move || st.repo(&id)?.1.delete(&q.snapshot, false)).await)
}

#[derive(Deserialize)]
struct NotesReq {
    snapshot: String,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    protected: Option<bool>,
}

async fn backup_notes(State(d): D, Path(id): Path<String>, Json(b): Json<NotesReq>) -> Response {
    let st = d.storage.clone();
    done(blocking(move || st.repo(&id)?.1.set_notes(&b.snapshot, b.notes.as_deref(), b.protected)).await)
}

async fn backup_files(State(d): D, Path(id): Path<String>, Query(q): Query<SnapQ>) -> Response {
    let st = d.storage.clone();
    match blocking(move || {
        let (_, repo) = st.repo(&id)?;
        let m = repo.snapshot(&q.snapshot)?;
        og_backup::list(repo.reader(&m, &q.archive)?, 20000)
    })
    .await
    {
        Ok(v) => Json(json!(v)).into_response(),
        Err(e) => bad(format!("{e:#}")),
    }
}

/// One file out of a backup.
async fn backup_download(State(d): D, Path(id): Path<String>, Query(q): Query<SnapQ>) -> Response {
    let st = d.storage.clone();
    let name = q.path.rsplit('/').next().unwrap_or("file").to_string();
    match blocking(move || {
        let (_, repo) = st.repo(&id)?;
        let m = repo.snapshot(&q.snapshot)?;
        let mut a = tar::Archive::new(repo.reader(&m, &q.archive)?);
        for e in a.entries()? {
            let mut e = e?;
            if e.path()?.to_string_lossy() == q.path && e.header().entry_type().is_file() {
                if e.header().size()? > 2 << 30 {
                    anyhow::bail!("files over 2 GiB: restore to a directory instead");
                }
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut e, &mut buf)?;
                return Ok(buf);
            }
        }
        anyhow::bail!("no file {} in {}", q.path, q.archive)
    })
    .await
    {
        Ok(bytes) => {
            let mut r = bytes.into_response();
            let safe: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", if safe.is_empty() { "file" } else { &safe })) {
                r.headers_mut().insert(header::CONTENT_DISPOSITION, v);
            }
            r
        }
        Err(e) => bad(format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct PruneReq {
    #[serde(default)]
    group: String,
    keep: og_backup::prune::Keep,
    #[serde(default)]
    dry_run: bool,
}

async fn prune(State(d): D, Path(id): Path<String>, Json(b): Json<PruneReq>) -> Response {
    let st = d.storage.clone();
    match blocking(move || {
        let (_, repo) = st.repo(&id)?;
        if b.keep.is_empty() {
            anyhow::bail!("say what to keep");
        }
        let groups: std::collections::BTreeSet<String> = repo.snapshots().into_iter().map(|m| m.group).filter(|g| b.group.is_empty() || *g == b.group).collect();
        let mut gone = vec![];
        for g in groups {
            gone.extend(backup::prune(&repo, &g, &b.keep, b.dry_run)?);
        }
        Ok(gone)
    })
    .await
    {
        Ok(g) => Json(json!({"removed": g, "dry_run": b.dry_run})).into_response(),
        Err(e) => bad(format!("{e:#}")),
    }
}

async fn gc(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>) -> Response {
    if let Err(e) = d.storage.get(&id) {
        return bad(e);
    }
    let d2 = d.clone();
    let id2 = id.clone();
    task_started(d.tasks.spawn("gc", &id, &w.label(), move |t| backup::gc(&d2, t, &id2)))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct VerifyReq {
    /// one backup; default: all not verified in the last 30 days
    snapshot: String,
}

async fn verify(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>, Json(b): Json<VerifyReq>) -> Response {
    let d2 = d.clone();
    let id2 = id.clone();
    task_started(d.tasks.spawn("verify", &id, &w.label(), move |t| {
        let (_, repo) = d2.storage.repo(&id2)?;
        let list: Vec<String> = if b.snapshot.is_empty() {
            let cutoff = crate::containers::unix_now().saturating_sub(30 * 86400);
            repo.snapshots().into_iter().filter(|m| m.verified.as_ref().is_none_or(|v| v.time < cutoff || !v.ok)).map(|m| m.id()).collect()
        } else {
            vec![b.snapshot.clone()]
        };
        let mut bad = 0;
        for s in &list {
            t.check()?;
            let m = repo.verify(s)?;
            match m.verified {
                Some(v) if v.ok => t.log(format!("{s}: ok")),
                Some(v) => {
                    bad += 1;
                    t.warn(format!("{s}: {}", v.error));
                }
                None => {}
            }
        }
        t.log(format!("verified {} backup(s), {bad} damaged", list.len()));
        if bad > 0 {
            anyhow::bail!("{bad} damaged backup(s)");
        }
        Ok(())
    }))
}

// ---- backup now, restore ----

#[derive(Deserialize)]
struct BackupReq {
    store: String,
    targets: Vec<Target>,
    #[serde(default)]
    opts: backup::Opts,
}

async fn backup_now(State(d): D, Extension(w): Extension<Who>, Json(b): Json<BackupReq>) -> Response {
    if b.targets.is_empty() {
        return bad("pick something to back up");
    }
    for t in &b.targets {
        let (path, name) = match t {
            Target::Container(n) => ("containers", n),
            Target::Stack(n) => ("stacks", n),
        };
        if !d.access.allowed(&w, &format!("/{path}/{name}"), "Backup.Create") && !d.access.allowed(&w, &format!("/storage/{}", b.store), "Backup.Create") {
            return (StatusCode::FORBIDDEN, Json(json!({"error": format!("permission denied: Backup.Create on /{path}/{name}")}))).into_response();
        }
    }
    let label = if b.targets.len() == 1 { b.targets[0].label() } else { format!("{} items", b.targets.len()) };
    let d2 = d.clone();
    task_started(d.tasks.spawn("backup", &label, &w.label(), move |t| {
        let mut failed = 0;
        for target in &b.targets {
            if let Err(e) = backup::run(&d2, t, &b.store, target, &b.opts) {
                failed += 1;
                if b.targets.len() == 1 {
                    return Err(e);
                }
                t.warn(format!("{}: {e:#}", target.label()));
            }
        }
        if failed == b.targets.len() {
            anyhow::bail!("nothing was backed up");
        }
        Ok(())
    }))
}

#[derive(Deserialize)]
struct RestoreReq {
    store: String,
    snapshot: String,
    #[serde(flatten)]
    opts: backup::RestoreOpts,
}

async fn restore(State(d): D, Extension(w): Extension<Who>, Json(b): Json<RestoreReq>) -> Response {
    if b.opts.to.is_some() && !d.access.allowed(&w, "/", "Sys.Modify") {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "unpacking into a directory needs Sys.Modify on /"}))).into_response();
    }
    let d2 = d.clone();
    let label = b.snapshot.split('/').take(2).collect::<Vec<_>>().join("/");
    task_started(d.tasks.spawn("restore", &label, &w.label(), move |t| backup::restore(&d2, t, &b.store, &b.snapshot, &b.opts)))
}

// ---- jobs ----

async fn jobs(State(d): D) -> Json<Value> {
    let states = backup::job_states(&d.root);
    let now = crate::containers::unix_now();
    Json(json!(backup::jobs(&d.root)
        .into_iter()
        .map(|j| {
            let next = crate::schedule::Schedule::parse(&j.schedule).ok().and_then(|s| s.next_after(now)).filter(|_| j.enabled);
            let targets: Vec<String> = backup::job_targets(&d, &j).iter().map(|t| t.label()).collect();
            json!({"job": j, "state": states.get(&j.id), "next": next, "targets": targets})
        })
        .collect::<Vec<_>>()))
}

async fn job_put(State(d): D, Json(mut j): Json<Job>) -> Response {
    if let Err(e) = backup::validate_job(&d, &mut j) {
        return bad(format!("{e:#}"));
    }
    let mut all = backup::jobs(&d.root);
    match all.iter_mut().find(|x| x.id == j.id) {
        Some(x) => *x = j,
        None => all.push(j),
    }
    done(backup::save_jobs(&d.root, &all))
}

async fn job_delete(State(d): D, Path(id): Path<String>) -> Response {
    let all: Vec<Job> = backup::jobs(&d.root).into_iter().filter(|j| j.id != id).collect();
    done(backup::save_jobs(&d.root, &all))
}

async fn job_run(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>) -> Response {
    match backup::jobs(&d.root).into_iter().find(|j| j.id == id) {
        Some(j) => task_started(backup::start_job(&d, j, &w.label())),
        None => bad("no such job"),
    }
}

// ---- tasks ----

#[derive(Deserialize, Default)]
#[serde(default)]
struct TasksQ {
    limit: Option<usize>,
    since: usize,
}

async fn tasks(State(d): D, Query(q): Query<TasksQ>) -> Json<Value> {
    Json(json!(d.tasks.list(q.limit.unwrap_or(100).min(1000))))
}

async fn task(State(d): D, Path(id): Path<String>, Query(q): Query<TasksQ>) -> Response {
    match d.tasks.get(&id) {
        Some(i) => Json(json!({"info": i, "log": d.tasks.log(&id, q.since)})).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no such task"}))).into_response(),
    }
}

async fn task_stop(State(d): D, Path(id): Path<String>) -> Response {
    if d.tasks.stop(&id) { Json(json!({"ok": true})).into_response() } else { bad("that task isn't running") }
}
