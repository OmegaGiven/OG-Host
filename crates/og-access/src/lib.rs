//! OG-Host accounts and permissions, modelled on Proxmox VE's access control.
//!
//! - Realms: `local` (OG-Host's own accounts, argon2id) and `pam` (Linux system accounts).
//!   User ids are `name@realm`; `root@pam` is the superuser and always has every privilege.
//! - Users belong to groups. Roles are sets of privileges (built-in ones plus custom ones).
//! - ACL entries grant a role to a user, a group (`@name`) or an API token on a path
//!   (`/`, `/containers/<name>`, `/vms/<id>`, `/storage/<id>`, `/nodes/<node>`, `/lab`, ...).
//!   Evaluation walks from `/` to the path: at every level that has entries for the caller,
//!   those replace what was inherited (user entries win over group entries at the same
//!   level); entries without `propagate` only apply to their exact path. `NoAccess` removes
//!   everything below it.
//! - API tokens (`user@realm!name`) act as their user, or, with privilege separation, get only
//!   what both the user and the token's own ACL entries allow.
//! - Two-factor: TOTP with single-use recovery codes.
//! - Every change and sign-in is appended to an audit log (JSON lines).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub mod pam;
pub mod totp;

pub const SUPERUSER: &str = "root@pam";

pub const PRIVILEGES: &[&str] = &[
    "Sys.Audit", "Sys.Modify", "Sys.Console", "Sys.Update",
    "Access.Audit", "Access.Modify",
    "Container.Audit", "Container.PowerMgmt", "Container.Console", "Container.Allocate", "Container.Config",
    "VM.Audit", "VM.PowerMgmt", "VM.Console", "VM.Allocate", "VM.Config", "VM.Snapshot",
    "Stack.Audit", "Stack.PowerMgmt", "Stack.Modify",
    "Image.Modify", "Network.Modify",
    "Datastore.Audit", "Datastore.Allocate", "Backup.Create", "Backup.Restore",
    "Cluster.Audit", "Cluster.Modify",
    "Lab.Audit", "Lab.Modify",
    "AI.Use", "AI.Approve",
];

/// Built-in roles (not stored, can't be changed).
pub fn builtin_roles() -> Vec<Role> {
    let all: Vec<String> = PRIVILEGES.iter().map(|s| s.to_string()).collect();
    let audit: Vec<String> = all.iter().filter(|p| p.ends_with(".Audit")).cloned().collect();
    let mut operator = audit.clone();
    operator.extend(
        ["Container.PowerMgmt", "Container.Console", "VM.PowerMgmt", "VM.Console", "Stack.PowerMgmt", "Backup.Create", "AI.Use"].iter().map(|s| s.to_string()),
    );
    vec![
        Role { id: "Administrator".into(), privs: all, builtin: true, comment: "Everything".into() },
        Role { id: "Operator".into(), privs: operator, builtin: true, comment: "See everything; start, stop and use consoles; make backups; use the AI".into() },
        Role { id: "Auditor".into(), privs: audit, builtin: true, comment: "Read-only".into() },
        Role { id: "NoAccess".into(), privs: vec![], builtin: true, comment: "Nothing (blocks inherited access)".into() },
    ]
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tfa {
    pub secret: String,
    pub enabled: bool,
    /// sha256 of each unused recovery code
    pub recovery: Vec<String>,
    /// last accepted TOTP step (no reuse)
    pub last_step: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    pub id: String,
    pub enable: bool,
    /// Unix seconds; 0 = never.
    pub expire: u64,
    pub firstname: String,
    pub lastname: String,
    pub email: String,
    pub comment: String,
    pub groups: Vec<String>,
    /// argon2id PHC string (local realm only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tfa: Option<Tfa>,
    pub created: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Group {
    pub id: String,
    pub comment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Role {
    pub id: String,
    pub privs: Vec<String>,
    #[serde(skip_serializing)]
    pub builtin: bool,
    pub comment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AclEntry {
    pub path: String,
    /// user id, `@group`, or token id `user@realm!name`
    pub ugid: String,
    pub role: String,
    pub propagate: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Token {
    /// `user@realm!name`
    pub id: String,
    pub user: String,
    /// sha256 of the secret
    pub hash: String,
    pub privsep: bool,
    pub expire: u64,
    pub comment: String,
    pub created: u64,
}

/// Who a request comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    User(String),
    /// token id, owning user
    Token(String, String),
    /// the server's own bearer token / unix socket root
    System,
}

impl Who {
    pub fn user(&self) -> &str {
        match self {
            Who::User(u) | Who::Token(_, u) => u,
            Who::System => SUPERUSER,
        }
    }
    pub fn label(&self) -> String {
        match self {
            Who::User(u) => u.clone(),
            Who::Token(t, _) => t.clone(),
            Who::System => "system".into(),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Db {
    users: BTreeMap<String, User>,
    groups: BTreeMap<String, Group>,
    roles: BTreeMap<String, Role>,
    acl: Vec<AclEntry>,
    tokens: BTreeMap<String, Token>,
}

pub struct Access {
    dir: PathBuf,
    db: RwLock<Db>,
    /// mtime of access.json as last loaded or saved: the CLI may change it under a running
    /// daemon, so a newer file is read again
    stamp: std::sync::Mutex<Option<(u64, std::time::SystemTime)>>,
}

/// Every save renames a new file into place, so the inode changes even within one mtime tick.
fn file_stamp(p: &Path) -> Option<(u64, std::time::SystemTime)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().and_then(|m| Some((m.ino(), m.modified().ok()?)))
}

fn roles_in(db: &Db) -> Vec<Role> {
    let mut r = builtin_roles();
    r.extend(db.roles.values().cloned());
    r
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sha256_hex(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut b = vec![0u8; n];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    b
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) && !s.starts_with(['.', '-'])
}

/// `name@realm` with a known realm.
pub fn parse_userid(id: &str) -> Result<(&str, &str)> {
    let (n, r) = id.rsplit_once('@').context("user ids look like name@local or name@pam")?;
    if !valid_name(n) || !["local", "pam"].contains(&r) {
        bail!("user ids look like name@local or name@pam");
    }
    Ok((n, r))
}

pub fn valid_path(p: &str) -> bool {
    p == "/" || (p.starts_with('/') && !p.ends_with('/') && p.len() < 200 && p[1..].split('/').all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || "_.-:@".contains(c))))
}

fn hash_password(pw: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    if pw.chars().count() < 10 {
        bail!("passwords need at least 10 characters");
    }
    let salt = SaltString::encode_b64(&random_bytes(16)).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(argon2::Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("{e}"))?.to_string())
}

fn verify_password(hash: &str, pw: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    PasswordHash::new(hash).is_ok_and(|h| argon2::Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

/// Result of checking a password.
#[derive(Debug, PartialEq, Eq)]
pub enum Login {
    Ok,
    /// password right, a second factor is required
    NeedTfa,
    Denied,
}

impl Access {
    /// Open `<root>/access`, migrating `<root>/ui-users.json` (first OG-Host accounts) once.
    pub fn open(root: &Path) -> Access {
        let dir = root.join("access");
        let _ = std::fs::create_dir_all(&dir);
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let db: Db = std::fs::read(dir.join("access.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let stamp = file_stamp(&dir.join("access.json"));
        let a = Access { dir, db: RwLock::new(db), stamp: std::sync::Mutex::new(stamp) };
        a.migrate_ui_users(root);
        a
    }

    fn migrate_ui_users(&self, root: &Path) {
        let old = root.join("ui-users.json");
        let Some(v) = std::fs::read(&old).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else { return };
        let mut db = self.db.write().unwrap();
        let mut moved = 0;
        for (name, u) in v["users"].as_object().into_iter().flatten() {
            let id = format!("{name}@local");
            if db.users.contains_key(&id) || !valid_name(name) {
                continue;
            }
            db.users.insert(id.clone(), User { id: id.clone(), enable: true, hash: u["hash"].as_str().map(String::from), created: now(), comment: "migrated".into(), ..Default::default() });
            db.acl.push(AclEntry { path: "/".into(), ugid: id, role: "Administrator".into(), propagate: true });
            moved += 1;
        }
        drop(db);
        if moved > 0 && self.save().is_ok() {
            let _ = std::fs::rename(&old, root.join("ui-users.json.migrated"));
        }
    }

    fn reload_if_changed(&self) {
        let p = self.dir.join("access.json");
        let m = file_stamp(&p);
        let mut st = self.stamp.lock().unwrap();
        if m.is_some() && m != *st {
            if let Some(db) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Db>(&b).ok()) {
                *self.db.write().unwrap() = db;
            }
            *st = m;
        }
    }

    fn rd(&self) -> std::sync::RwLockReadGuard<'_, Db> {
        self.reload_if_changed();
        self.db.read().unwrap()
    }

    fn wr(&self) -> std::sync::RwLockWriteGuard<'_, Db> {
        self.reload_if_changed();
        self.db.write().unwrap()
    }

    fn save(&self) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let db = self.db.read().unwrap();
        let tmp = self.dir.join("access.json.tmp");
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?.write_all(&serde_json::to_vec_pretty(&*db)?)?;
        std::fs::rename(tmp, self.dir.join("access.json"))?;
        *self.stamp.lock().unwrap() = file_stamp(&self.dir.join("access.json"));
        Ok(())
    }

    // ---- audit ----

    pub fn audit(&self, who: &str, ip: &str, action: &str, target: &str, ok: bool) {
        use std::io::Write;
        let line = serde_json::json!({"ts": now(), "user": who, "ip": ip, "action": action, "target": target, "ok": ok}).to_string();
        let p = self.dir.join("audit.log");
        // rotate at 20 MB, keep one old file
        if std::fs::metadata(&p).is_ok_and(|m| m.len() > 20 * 1024 * 1024) {
            let _ = std::fs::rename(&p, self.dir.join("audit.log.1"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Newest first.
    pub fn audit_tail(&self, limit: usize, filter: &str) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(self.dir.join("audit.log")).unwrap_or_default();
        let f = filter.to_lowercase();
        text.lines().rev().filter(|l| f.is_empty() || l.to_lowercase().contains(&f)).take(limit).filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    // ---- authentication ----

    pub fn has_users(&self) -> bool {
        !self.rd().users.is_empty()
    }

    /// Check a password. Disabled/expired users and unknown users are denied (same timing).
    pub fn check_password(&self, id: &str, password: &str) -> Login {
        let Ok((name, realm)) = parse_userid(id) else { return Login::Denied };
        let user = self.rd().users.get(id).cloned();
        let active = |u: &User| u.enable && (u.expire == 0 || u.expire > now());
        let ok = match realm {
            "local" => {
                let hash = user.as_ref().and_then(|u| u.hash.clone());
                // unknown users still cost a hash
                static DUMMY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| hash_password("not-a-real-password").unwrap_or_default());
                let h = hash.unwrap_or_else(|| DUMMY.clone());
                verify_password(&h, password) && user.as_ref().is_some_and(active)
            }
            // root@pam always may sign in; other system users need an OG-Host user entry
            "pam" => (id == SUPERUSER || user.as_ref().is_some_and(active)) && pam::authenticate(name, password).is_ok(),
            _ => false,
        };
        if !ok {
            return Login::Denied;
        }
        if user.and_then(|u| u.tfa).is_some_and(|t| t.enabled) { Login::NeedTfa } else { Login::Ok }
    }

    /// Second factor: a TOTP code or an unused recovery code (consumed).
    pub fn check_tfa(&self, id: &str, code: &str) -> bool {
        let mut db = self.wr();
        let Some(t) = db.users.get_mut(id).and_then(|u| u.tfa.as_mut()).filter(|t| t.enabled) else { return false };
        let ok = if let Some(step) = totp::base32_decode(&t.secret).and_then(|s| totp::verify(&s, code, now())) {
            if step <= t.last_step {
                false // replayed
            } else {
                t.last_step = step;
                true
            }
        } else {
            let h = sha256_hex(&code.trim().to_lowercase().replace('-', ""));
            match t.recovery.iter().position(|r| ct_eq(r, &h)) {
                Some(i) => {
                    t.recovery.remove(i);
                    true
                }
                None => false,
            }
        };
        drop(db);
        if ok {
            let _ = self.save();
        }
        ok
    }

    /// Start TOTP enrolment: a new secret (not yet enabled) and its otpauth URI + QR (SVG).
    pub fn tfa_begin(&self, id: &str) -> Result<(String, String, String)> {
        let secret = totp::base32_encode(&random_bytes(20));
        {
            let mut db = self.wr();
            let u = db.users.get_mut(id).context("no such user")?;
            let mut t = u.tfa.clone().unwrap_or_default();
            if t.enabled {
                bail!("two-factor is already on; turn it off first to set up a new device");
            }
            t.secret = secret.clone();
            u.tfa = Some(t);
        }
        self.save()?;
        let uri = totp::otpauth_uri("OG-Host", id, &secret);
        let qr = qrcode::QrCode::new(uri.as_bytes()).map(|c| c.render::<qrcode::render::svg::Color>().min_dimensions(220, 220).quiet_zone(true).build()).unwrap_or_default();
        Ok((secret, uri, qr))
    }

    /// Confirm enrolment with a code from the app; returns the recovery codes (shown once).
    pub fn tfa_confirm(&self, id: &str, code: &str) -> Result<Vec<String>> {
        let mut db = self.wr();
        let t = db.users.get_mut(id).and_then(|u| u.tfa.as_mut()).context("start the set-up first")?;
        let secret = totp::base32_decode(&t.secret).context("bad secret")?;
        let step = totp::verify(&secret, code, now()).context("that code isn't right — check the time on your phone")?;
        let codes: Vec<String> = (0..10)
            .map(|_| {
                let b = random_bytes(5);
                let s: String = totp::base32_encode(&b).to_lowercase();
                format!("{}-{}", &s[..4], &s[4..8])
            })
            .collect();
        t.recovery = codes.iter().map(|c| sha256_hex(&c.replace('-', ""))).collect();
        t.enabled = true;
        t.last_step = step;
        drop(db);
        self.save()?;
        Ok(codes)
    }

    pub fn tfa_disable(&self, id: &str) -> Result<()> {
        let mut db = self.wr();
        db.users.get_mut(id).context("no such user")?.tfa = None;
        drop(db);
        self.save()
    }

    pub fn tfa_status(&self, id: &str) -> serde_json::Value {
        let db = self.rd();
        let t = db.users.get(id).and_then(|u| u.tfa.as_ref());
        serde_json::json!({"enabled": t.is_some_and(|t| t.enabled), "recovery_left": t.map(|t| t.recovery.len()).unwrap_or(0)})
    }

    /// Fingerprint of a user's credentials (password hash + TFA secret + enable state): a
    /// session made under another fingerprint is no longer valid.
    pub fn fingerprint(&self, id: &str) -> Option<String> {
        if id == SUPERUSER {
            return Some("root".into());
        }
        let db = self.rd();
        let u = db.users.get(id)?;
        if !u.enable || (u.expire != 0 && u.expire <= now()) {
            return None;
        }
        Some(sha256_hex(&format!("{}|{}", u.hash.as_deref().unwrap_or(""), u.tfa.as_ref().filter(|t| t.enabled).map(|t| t.secret.clone()).unwrap_or_default()))[..16].to_string())
    }

    // ---- users, groups, roles, acl ----

    pub fn users(&self) -> Vec<serde_json::Value> {
        let db = self.rd();
        db.users
            .values()
            .map(|u| serde_json::json!({"id": u.id, "enable": u.enable, "expire": u.expire, "firstname": u.firstname, "lastname": u.lastname, "email": u.email, "comment": u.comment, "groups": u.groups, "tfa": u.tfa.as_ref().is_some_and(|t| t.enabled), "created": u.created, "tokens": db.tokens.values().filter(|t| t.user == u.id).count()}))
            .collect()
    }

    pub fn upsert_user(&self, id: &str, fields: &serde_json::Value, password: Option<&str>) -> Result<()> {
        let (_, realm) = parse_userid(id)?;
        if realm == "pam" && !pam::user_exists(id.rsplit_once('@').unwrap().0) {
            bail!("there is no system account named {} on this server", id.rsplit_once('@').unwrap().0);
        }
        let hash = match password {
            Some(p) if realm == "local" => Some(hash_password(p)?),
            Some(_) => bail!("system (pam) users change their password on the server itself"),
            None => None,
        };
        let mut db = self.wr();
        let groups: Vec<String> = fields["groups"].as_array().map(|a| a.iter().filter_map(|g| g.as_str().map(String::from)).collect()).unwrap_or_default();
        for g in &groups {
            if !db.groups.contains_key(g) {
                bail!("no group {g}");
            }
        }
        let fresh = !db.users.contains_key(id);
        if fresh && realm == "local" && hash.is_none() {
            bail!("a new local user needs a password");
        }
        let u = db.users.entry(id.to_string()).or_insert_with(|| User { id: id.into(), enable: true, created: now(), ..Default::default() });
        if let Some(h) = hash {
            u.hash = Some(h);
        }
        let s = |k: &str| fields[k].as_str().map(|v| v.chars().take(200).collect::<String>());
        if let Some(v) = fields["enable"].as_bool() {
            u.enable = v;
        }
        if let Some(v) = fields["expire"].as_u64() {
            u.expire = v;
        }
        for (k, dst) in [("firstname", &mut u.firstname), ("lastname", &mut u.lastname), ("email", &mut u.email), ("comment", &mut u.comment)] {
            if let Some(v) = s(k) {
                *dst = v;
            }
        }
        if fields.get("groups").is_some() {
            u.groups = groups;
        }
        drop(db);
        self.save()
    }

    pub fn set_password(&self, id: &str, password: &str) -> Result<()> {
        self.upsert_user(id, &serde_json::json!({}), Some(password))
    }

    pub fn delete_user(&self, id: &str) -> Result<()> {
        if id == SUPERUSER {
            bail!("root@pam can't be removed");
        }
        let mut db = self.wr();
        if db.users.remove(id).is_none() {
            bail!("no such user");
        }
        db.acl.retain(|e| e.ugid != id && !e.ugid.starts_with(&format!("{id}!")));
        db.tokens.retain(|_, t| t.user != id);
        drop(db);
        self.save()
    }

    pub fn groups(&self) -> Vec<serde_json::Value> {
        let db = self.rd();
        db.groups.values().map(|g| serde_json::json!({"id": g.id, "comment": g.comment, "members": db.users.values().filter(|u| u.groups.contains(&g.id)).map(|u| u.id.clone()).collect::<Vec<_>>()})).collect()
    }

    pub fn upsert_group(&self, id: &str, comment: &str) -> Result<()> {
        if !valid_name(id) {
            bail!("group names are letters, digits, _ . -");
        }
        self.wr().groups.insert(id.into(), Group { id: id.into(), comment: comment.chars().take(200).collect() });
        self.save()
    }

    pub fn delete_group(&self, id: &str) -> Result<()> {
        let mut db = self.wr();
        db.groups.remove(id).context("no such group")?;
        for u in db.users.values_mut() {
            u.groups.retain(|g| g != id);
        }
        let ug = format!("@{id}");
        db.acl.retain(|e| e.ugid != ug);
        drop(db);
        self.save()
    }

    pub fn roles(&self) -> Vec<Role> {
        roles_in(&self.rd())
    }

    fn role(&self, id: &str) -> Option<Role> {
        self.roles().into_iter().find(|r| r.id == id)
    }

    pub fn upsert_role(&self, id: &str, privs: &[String], comment: &str) -> Result<()> {
        if !valid_name(id) || builtin_roles().iter().any(|r| r.id == id) {
            bail!("pick another name (built-in roles can't be changed)");
        }
        for p in privs {
            if !PRIVILEGES.contains(&p.as_str()) {
                bail!("unknown privilege {p}");
            }
        }
        self.wr().roles.insert(id.into(), Role { id: id.into(), privs: privs.to_vec(), builtin: false, comment: comment.chars().take(200).collect() });
        self.save()
    }

    pub fn delete_role(&self, id: &str) -> Result<()> {
        let mut db = self.wr();
        db.roles.remove(id).context("no such custom role")?;
        db.acl.retain(|e| e.role != id);
        drop(db);
        self.save()
    }

    pub fn acl(&self) -> Vec<AclEntry> {
        self.rd().acl.clone()
    }

    pub fn set_acl(&self, e: AclEntry, delete: bool) -> Result<()> {
        if !valid_path(&e.path) {
            bail!("paths look like / or /containers/web");
        }
        if self.role(&e.role).is_none() {
            bail!("no role {}", e.role);
        }
        {
            let db = self.rd();
            let known = if let Some(g) = e.ugid.strip_prefix('@') { db.groups.contains_key(g) } else if e.ugid.contains('!') { db.tokens.contains_key(&e.ugid) } else { db.users.contains_key(&e.ugid) || e.ugid == SUPERUSER };
            if !known && !delete {
                bail!("no user, group or token {}", e.ugid);
            }
        }
        let mut db = self.wr();
        db.acl.retain(|x| !(x.path == e.path && x.ugid == e.ugid && x.role == e.role));
        if !delete {
            db.acl.push(e);
        }
        drop(db);
        self.save()
    }

    // ---- tokens ----

    pub fn tokens(&self, user: Option<&str>) -> Vec<serde_json::Value> {
        self.rd().tokens.values().filter(|t| user.is_none_or(|u| t.user == u)).map(|t| serde_json::json!({"id": t.id, "user": t.user, "privsep": t.privsep, "expire": t.expire, "comment": t.comment, "created": t.created})).collect()
    }

    /// Returns the secret (shown once).
    pub fn create_token(&self, user: &str, name: &str, privsep: bool, expire: u64, comment: &str) -> Result<String> {
        if !valid_name(name) {
            bail!("token names are letters, digits, _ . -");
        }
        let id = format!("{user}!{name}");
        let secret: String = random_bytes(24).iter().map(|b| format!("{b:02x}")).collect();
        let mut db = self.wr();
        if user != SUPERUSER && !db.users.contains_key(user) {
            bail!("no such user");
        }
        if db.tokens.contains_key(&id) {
            bail!("a token named {name} exists already");
        }
        db.tokens.insert(id.clone(), Token { id: id.clone(), user: user.into(), hash: sha256_hex(&secret), privsep, expire, comment: comment.chars().take(200).collect(), created: now() });
        drop(db);
        self.save()?;
        Ok(secret)
    }

    pub fn delete_token(&self, id: &str) -> Result<()> {
        let mut db = self.wr();
        db.tokens.remove(id).context("no such token")?;
        db.acl.retain(|e| e.ugid != id);
        drop(db);
        self.save()
    }

    /// `user@realm!name=secret` -> the caller, if valid.
    pub fn check_token(&self, full: &str) -> Option<Who> {
        let (id, secret) = full.split_once('=')?;
        let db = self.rd();
        let t = db.tokens.get(id)?;
        if t.expire != 0 && t.expire <= now() {
            return None;
        }
        if !ct_eq(&t.hash, &sha256_hex(secret)) {
            return None;
        }
        let user_ok = t.user == SUPERUSER || db.users.get(&t.user).is_some_and(|u| u.enable && (u.expire == 0 || u.expire > now()));
        user_ok.then(|| Who::Token(t.id.clone(), t.user.clone()))
    }

    // ---- permissions ----

    fn privs_for(&self, db: &Db, ugids: &[String], user_id: &str, path: &str) -> BTreeSet<String> {
        let roles = roles_in(db);
        let role_privs = |r: &str| roles.iter().find(|x| x.id == r).map(|x| x.privs.clone()).unwrap_or_default();
        let mut levels = vec!["/".to_string()];
        if path != "/" {
            let mut acc = String::new();
            for seg in path.trim_start_matches('/').split('/') {
                acc.push('/');
                acc.push_str(seg);
                levels.push(acc.clone());
            }
        }
        let mut cur: BTreeSet<String> = BTreeSet::new();
        for (i, lvl) in levels.iter().enumerate() {
            let last = i == levels.len() - 1;
            let at: Vec<&AclEntry> = db.acl.iter().filter(|e| &e.path == lvl && (last || e.propagate) && ugids.contains(&e.ugid)).collect();
            if at.is_empty() {
                continue;
            }
            let own: Vec<&&AclEntry> = at.iter().filter(|e| e.ugid == user_id).collect();
            let pick: Vec<&AclEntry> = if own.is_empty() { at.clone() } else { own.into_iter().copied().collect() };
            cur = pick.iter().flat_map(|e| role_privs(&e.role)).collect();
        }
        cur
    }

    /// Effective privileges of a caller on a path.
    pub fn privileges(&self, who: &Who, path: &str) -> BTreeSet<String> {
        let all = || PRIVILEGES.iter().map(|s| s.to_string()).collect();
        let user = who.user().to_string();
        if *who == Who::System || user == SUPERUSER && !matches!(who, Who::Token(_, _)) {
            return all();
        }
        let db = self.rd();
        let user_set: BTreeSet<String> = if user == SUPERUSER {
            all()
        } else {
            let Some(u) = db.users.get(&user).filter(|u| u.enable && (u.expire == 0 || u.expire > now())) else { return BTreeSet::new() };
            let mut ugids = vec![user.clone()];
            ugids.extend(u.groups.iter().map(|g| format!("@{g}")));
            self.privs_for(&db, &ugids, &user, path)
        };
        match who {
            Who::Token(tid, _) if db.tokens.get(tid).is_some_and(|t| t.privsep) => {
                let tok = self.privs_for(&db, &[tid.clone()], tid, path);
                user_set.intersection(&tok).cloned().collect()
            }
            _ => user_set,
        }
    }

    pub fn allowed(&self, who: &Who, path: &str, privilege: &str) -> bool {
        self.privileges(who, path).contains(privilege)
    }

    /// Has the privilege on the path or anywhere below it (for menus and lists).
    pub fn allowed_anywhere(&self, who: &Who, prefix: &str, privilege: &str) -> bool {
        if self.allowed(who, prefix, privilege) {
            return true;
        }
        let paths: Vec<String> = self.rd().acl.iter().map(|e| e.path.clone()).filter(|p| p.starts_with(prefix)).collect();
        paths.iter().any(|p| self.allowed(who, p, privilege))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("ogacc-{}-{}", std::process::id(), random_bytes(4).iter().map(|b| format!("{b:02x}")).collect::<String>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn users_acl_tokens_tfa() {
        let d = tmp();
        let a = Access::open(&d);
        a.upsert_group("ops", "operators").unwrap();
        a.upsert_user("ann@local", &serde_json::json!({"groups": ["ops"]}), Some("correct horse battery")).unwrap();
        a.upsert_user("bob@local", &serde_json::json!({}), Some("another long password")).unwrap();
        assert!(a.upsert_user("x@local", &serde_json::json!({}), Some("short")).is_err());
        assert_eq!(a.check_password("ann@local", "correct horse battery"), Login::Ok);
        assert_eq!(a.check_password("ann@local", "wrong password!!"), Login::Denied);
        assert_eq!(a.check_password("nobody@local", "correct horse battery"), Login::Denied);

        let ann = Who::User("ann@local".into());
        let bob = Who::User("bob@local".into());
        // group Operator everywhere, but NoAccess for ann on one container
        a.set_acl(AclEntry { path: "/".into(), ugid: "@ops".into(), role: "Operator".into(), propagate: true }, false).unwrap();
        a.set_acl(AclEntry { path: "/containers/secret".into(), ugid: "ann@local".into(), role: "NoAccess".into(), propagate: true }, false).unwrap();
        assert!(a.allowed(&ann, "/containers/web", "Container.PowerMgmt"));
        assert!(!a.allowed(&ann, "/containers/web", "Container.Allocate"));
        assert!(!a.allowed(&ann, "/containers/secret", "Container.Audit"));
        assert!(a.privileges(&bob, "/").is_empty());
        // non-propagating entry applies only to its exact path
        a.set_acl(AclEntry { path: "/vms".into(), ugid: "bob@local".into(), role: "Administrator".into(), propagate: false }, false).unwrap();
        assert!(a.allowed(&bob, "/vms", "VM.Allocate"));
        assert!(!a.allowed(&bob, "/vms/100", "VM.Allocate"));
        assert!(a.allowed_anywhere(&bob, "/vms", "VM.Audit"));
        // tokens: same as user, or (privsep) intersection
        let s = a.create_token("ann@local", "ci", true, 0, "").unwrap();
        let t = a.check_token(&format!("ann@local!ci={s}")).unwrap();
        assert!(a.privileges(&t, "/containers/web").is_empty());
        a.set_acl(AclEntry { path: "/containers".into(), ugid: "ann@local!ci".into(), role: "Administrator".into(), propagate: true }, false).unwrap();
        assert!(a.allowed(&t, "/containers/web", "Container.PowerMgmt"));
        assert!(!a.allowed(&t, "/containers/web", "Container.Allocate")); // ann herself can't
        assert!(a.check_token("ann@local!ci=wrong").is_none());
        // disabling the user kills the token
        a.upsert_user("ann@local", &serde_json::json!({"enable": false}), None).unwrap();
        assert!(a.check_token(&format!("ann@local!ci={s}")).is_none());
        assert_eq!(a.check_password("ann@local", "correct horse battery"), Login::Denied);
        // two-factor
        let (secret, uri, qr) = a.tfa_begin("bob@local").unwrap();
        assert!(uri.starts_with("otpauth://totp/OG-Host:bob%40local?secret=") && qr.contains("<svg"));
        let key = totp::base32_decode(&secret).unwrap();
        let code = format!("{:06}", totp::code_at(&key, now() / 30));
        let rec = a.tfa_confirm("bob@local", &code).unwrap();
        assert_eq!(rec.len(), 10);
        assert_eq!(a.check_password("bob@local", "another long password"), Login::NeedTfa);
        assert!(!a.check_tfa("bob@local", &code)); // already used
        assert!(a.check_tfa("bob@local", &rec[0]));
        assert!(!a.check_tfa("bob@local", &rec[0])); // recovery codes are single-use
        // persisted
        let b = Access::open(&d);
        assert_eq!(b.users().len(), 2);
        assert!(b.allowed(&Who::System, "/anything", "Sys.Modify"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn migrates_first_accounts() {
        let d = tmp();
        std::fs::write(d.join("ui-users.json"), r#"{"users":{"admin":{"hash":"$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$aaaa"}}}"#).unwrap();
        let a = Access::open(&d);
        assert!(a.users().iter().any(|u| u["id"] == "admin@local"));
        assert!(a.allowed(&Who::User("admin@local".into()), "/", "Sys.Modify"));
        assert!(d.join("ui-users.json.migrated").exists());
        let _ = std::fs::remove_dir_all(d);
    }
    #[test]
    fn sees_changes_from_another_process() {
        let d = tmp();
        let daemon = Access::open(&d);
        let cli = Access::open(&d);
        cli.upsert_user("cat@local", &serde_json::json!({}), Some("a long enough password")).unwrap();
        assert!(daemon.users().iter().any(|u| u["id"] == "cat@local"));
        daemon.upsert_group("g", "").unwrap();
        assert!(cli.groups().len() == 1 && cli.users().len() == 1);
    }
}
