//! `/v1/access/*`: users, groups, roles, ACL, API tokens, the audit log, and the caller's own
//! account (`/v1/access/me/*`: password, two-factor, tokens, sessions).
//!
//! The route table in `auth::rule` already requires Access.Audit / Access.Modify on /access.
//! On top of that: granting a role on a path needs Access.Modify on that path, and changing
//! an account that can itself change accounts or the system needs full Sys.Modify on /, so
//! an account manager can't take over an administrator.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use og_access::{AclEntry, Who, SUPERUSER};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::serve::Daemon;

type D = State<Arc<Daemon>>;

fn bad(e: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response()
}

fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"error": msg}))).into_response()
}

fn done(r: anyhow::Result<()>) -> Response {
    match r {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

/// Can `who` change account `target`?
fn may_manage(d: &Daemon, who: &Who, target: &str) -> bool {
    if who.user() == target && !matches!(who, Who::Token(..)) {
        return true;
    }
    if target == SUPERUSER {
        return *who == Who::System;
    }
    let t = Who::User(target.into());
    let powerful = d.access.allowed_anywhere(&t, "/", "Access.Modify") || d.access.allowed_anywhere(&t, "/", "Sys.Modify");
    !powerful || d.access.allowed(who, "/", "Sys.Modify")
}

pub fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/v1/access/users", get(users).post(user_put))
        .route("/v1/access/users/{id}", post(user_update).delete(user_delete))
        .route("/v1/access/users/{id}/tfa", delete(user_tfa_reset))
        .route("/v1/access/groups", get(groups).post(group_put))
        .route("/v1/access/groups/{id}", delete(group_delete))
        .route("/v1/access/roles", get(roles).post(role_put))
        .route("/v1/access/roles/{id}", delete(role_delete))
        .route("/v1/access/privileges", get(privileges))
        .route("/v1/access/acl", get(acl).post(acl_set))
        .route("/v1/access/tokens", get(tokens).post(token_create))
        .route("/v1/access/tokens/{id}", delete(token_delete))
        .route("/v1/access/audit", get(audit))
        .route("/v1/access/me/password", post(me_password))
        .route("/v1/access/me/tfa", get(me_tfa).post(me_tfa_begin).delete(me_tfa_off))
        .route("/v1/access/me/tfa/confirm", post(me_tfa_confirm))
        .route("/v1/access/me/tokens", get(me_tokens).post(me_token_create))
        .route("/v1/access/me/tokens/{name}", delete(me_token_delete))
        .route("/v1/access/me/sessions", get(me_sessions))
        .route("/v1/access/me/sessions/{id}", delete(me_session_end))
        .with_state(d)
}

// ---- users ----

async fn users(State(d): D) -> Json<Value> {
    Json(json!(d.access.users()))
}

#[derive(Deserialize)]
struct UserReq {
    id: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(flatten)]
    fields: Value,
}

async fn user_put(State(d): D, Extension(w): Extension<Who>, Json(b): Json<UserReq>) -> Response {
    if !may_manage(&d, &w, &b.id) {
        return forbidden("changing this account needs Sys.Modify on /");
    }
    let (d2, f) = (d.clone(), b.fields);
    done(tokio::task::spawn_blocking(move || d2.access.upsert_user(&b.id, &f, b.password.as_deref())).await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}"))))
}

async fn user_update(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>, Json(mut b): Json<Value>) -> Response {
    if !may_manage(&d, &w, &id) {
        return forbidden("changing this account needs Sys.Modify on /");
    }
    // you can't lock yourself out, and only others manage your groups
    if w.user() == id && (b.get("enable").is_some() || b.get("groups").is_some() || b.get("expire").is_some()) && !d.access.allowed(&w, "/access", "Access.Modify") {
        return forbidden("ask an administrator to change that");
    }
    if w.user() == id && b["enable"] == json!(false) {
        return bad("you can't disable your own account");
    }
    let pw = b.as_object_mut().and_then(|o| o.remove("password")).and_then(|v| v.as_str().map(String::from));
    let d2 = d.clone();
    done(tokio::task::spawn_blocking(move || d2.access.upsert_user(&id, &b, pw.as_deref())).await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}"))))
}

async fn user_delete(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>) -> Response {
    if w.user() == id {
        return bad("you can't remove your own account");
    }
    if !may_manage(&d, &w, &id) {
        return forbidden("removing this account needs Sys.Modify on /");
    }
    done(d.access.delete_user(&id))
}

async fn user_tfa_reset(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>) -> Response {
    if !may_manage(&d, &w, &id) {
        return forbidden("this needs Sys.Modify on /");
    }
    done(d.access.tfa_disable(&id))
}

// ---- groups, roles, acl ----

async fn groups(State(d): D) -> Json<Value> {
    Json(json!(d.access.groups()))
}

#[derive(Deserialize)]
struct GroupReq {
    id: String,
    #[serde(default)]
    comment: String,
}

async fn group_put(State(d): D, Json(b): Json<GroupReq>) -> Response {
    done(d.access.upsert_group(&b.id, &b.comment))
}

async fn group_delete(State(d): D, Path(id): Path<String>) -> Response {
    done(d.access.delete_group(&id))
}

async fn roles(State(d): D) -> Json<Value> {
    Json(json!(d.access.roles().into_iter().map(|r| json!({"id": r.id, "privs": r.privs, "builtin": r.builtin, "comment": r.comment})).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
struct RoleReq {
    id: String,
    privs: Vec<String>,
    #[serde(default)]
    comment: String,
}

async fn role_put(State(d): D, Json(b): Json<RoleReq>) -> Response {
    done(d.access.upsert_role(&b.id, &b.privs, &b.comment))
}

async fn role_delete(State(d): D, Path(id): Path<String>) -> Response {
    done(d.access.delete_role(&id))
}

async fn privileges() -> Json<Value> {
    Json(json!(og_access::PRIVILEGES))
}

async fn acl(State(d): D) -> Json<Value> {
    Json(json!(d.access.acl()))
}

#[derive(Deserialize)]
struct AclReq {
    #[serde(flatten)]
    entry: AclEntry,
    #[serde(default)]
    delete: bool,
}

async fn acl_set(State(d): D, Extension(w): Extension<Who>, Json(b): Json<AclReq>) -> Response {
    if !d.access.allowed(&w, &b.entry.path, "Access.Modify") {
        return forbidden(&format!("granting on {} needs Access.Modify there", b.entry.path));
    }
    // handing out a role you don't hold yourself, or touching an administrator's grants
    let grants: Vec<String> = d.access.roles().into_iter().find(|r| r.id == b.entry.role).map(|r| r.privs).unwrap_or_default();
    let mine = d.access.privileges(&w, &b.entry.path);
    if !b.delete && !grants.iter().all(|p| mine.contains(p)) {
        return forbidden("you can only grant privileges you hold on that path");
    }
    let target = b.entry.ugid.split('!').next().unwrap_or("").to_string();
    if !target.starts_with('@') && !may_manage(&d, &w, &target) && w.user() != target {
        return forbidden("changing this account's grants needs Sys.Modify on /");
    }
    done(d.access.set_acl(b.entry, b.delete))
}

// ---- tokens ----

#[derive(Deserialize)]
struct TokenQ {
    #[serde(default)]
    user: Option<String>,
}

async fn tokens(State(d): D, Query(q): Query<TokenQ>) -> Json<Value> {
    Json(json!(d.access.tokens(q.user.as_deref())))
}

#[derive(Deserialize)]
struct TokenReq {
    #[serde(default)]
    user: String,
    name: String,
    #[serde(default = "yes")]
    privsep: bool,
    #[serde(default)]
    expire: u64,
    #[serde(default)]
    comment: String,
}

fn yes() -> bool {
    true
}

fn make_token(d: &Daemon, user: &str, b: &TokenReq) -> Response {
    match d.access.create_token(user, &b.name, b.privsep, b.expire, &b.comment) {
        Ok(secret) => {
            let id = format!("{user}!{}", b.name);
            Json(json!({"id": id, "secret": secret, "header": format!("Authorization: OGAPIToken {id}={secret}")})).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn token_create(State(d): D, Extension(w): Extension<Who>, Json(b): Json<TokenReq>) -> Response {
    if !may_manage(&d, &w, &b.user) {
        return forbidden("making tokens for this account needs Sys.Modify on /");
    }
    make_token(&d, &b.user.clone(), &b)
}

async fn token_delete(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>) -> Response {
    let owner = id.split('!').next().unwrap_or("");
    if !may_manage(&d, &w, owner) {
        return forbidden("this needs Sys.Modify on /");
    }
    done(d.access.delete_token(&id))
}

#[derive(Deserialize)]
struct AuditQ {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    q: String,
}

async fn audit(State(d): D, Query(q): Query<AuditQ>) -> Json<Value> {
    Json(json!(d.access.audit_tail(q.limit.unwrap_or(200).min(2000), &q.q)))
}

// ---- the caller's own account ----

fn me_user(w: &Who) -> Result<String, Response> {
    match w {
        Who::User(u) => Ok(u.clone()),
        _ => Err(forbidden("sign in as a user for this")),
    }
}

#[derive(Deserialize)]
struct PwReq {
    current: String,
    new: String,
}

async fn me_password(State(d): D, Extension(w): Extension<Who>, Json(b): Json<PwReq>) -> Response {
    let u = match me_user(&w) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if !u.ends_with("@local") {
        return bad("system (pam) accounts change their password on the server itself");
    }
    let d2 = d.clone();
    let r = tokio::task::spawn_blocking(move || {
        if d2.access.check_password(&u, &b.current) == og_access::Login::Denied {
            anyhow::bail!("the current password isn't right");
        }
        d2.access.set_password(&u, &b.new)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
    // other sessions end (their fingerprint changed); this one is renewed by signing in again
    done(r)
}

async fn me_tfa(State(d): D, Extension(w): Extension<Who>) -> Json<Value> {
    Json(d.access.tfa_status(w.user()))
}

async fn me_tfa_begin(State(d): D, Extension(w): Extension<Who>) -> Response {
    let u = match me_user(&w) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match d.access.tfa_begin(&u) {
        Ok((secret, uri, qr)) => Json(json!({"secret": secret, "uri": uri, "qr_svg": qr})).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct CodeReq {
    code: String,
}

async fn me_tfa_confirm(State(d): D, Extension(w): Extension<Who>, Json(b): Json<CodeReq>) -> Response {
    let u = match me_user(&w) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match d.access.tfa_confirm(&u, &b.code) {
        Ok(codes) => Json(json!({"recovery_codes": codes})).into_response(),
        Err(e) => bad(e),
    }
}

async fn me_tfa_off(State(d): D, Extension(w): Extension<Who>, Json(b): Json<CodeReq>) -> Response {
    let u = match me_user(&w) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if !d.access.check_tfa(&u, &b.code) {
        return bad("that code isn't right");
    }
    done(d.access.tfa_disable(&u))
}

async fn me_tokens(State(d): D, Extension(w): Extension<Who>) -> Json<Value> {
    Json(json!(d.access.tokens(Some(w.user()))))
}

async fn me_token_create(State(d): D, Extension(w): Extension<Who>, Json(b): Json<TokenReq>) -> Response {
    let u = match me_user(&w) {
        Ok(u) => u,
        Err(r) => return r,
    };
    make_token(&d, &u, &b)
}

async fn me_token_delete(State(d): D, Extension(w): Extension<Who>, Path(name): Path<String>) -> Response {
    let u = match me_user(&w) {
        Ok(u) => u,
        Err(r) => return r,
    };
    done(d.access.delete_token(&format!("{u}!{name}")))
}

async fn me_sessions(State(d): D, Extension(w): Extension<Who>, headers: HeaderMap) -> Json<Value> {
    Json(json!(d.sessions.list(&d, w.user(), crate::auth::cookie_token(&headers).as_deref())))
}

async fn me_session_end(State(d): D, Extension(w): Extension<Who>, Path(id): Path<String>) -> Response {
    if d.sessions.revoke(w.user(), &id) { Json(json!({"ok": true})).into_response() } else { bad("no such session") }
}
