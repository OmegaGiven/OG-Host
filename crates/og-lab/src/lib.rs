//! OG-Host lab control plane: OmegaEnv's backend, in Rust, inside og-hostd.
//!
//! One OG-Host node can manage the whole homelab: other machines run the existing
//! omega-agent (unchanged: `/api/ingest`, `/api/agent/jobs`, `/api/jobs/result`,
//! `/api/logs/ingest`), OpenClaw talks to `/mcp` with the API token as before, and the node
//! itself reports natively through [`LocalNode`]. Data files keep OmegaEnv's formats
//! (apps.json, checks.json, metrics.db, the log directory, ...) so an existing OmegaEnv
//! data directory can be imported as-is.
//!
//! Modules: [`metrics`] (SQLite history), [`logs`] (file log store), [`fleet`] (hosts and
//! alerts), [`apps`] (Home tiles, health checks, known issues), [`jobs`] (agent jobs and the
//! install queue), [`notify`] (Signal), [`watches`], [`claw`] (OpenClaw, station, catalog),
//! [`mcp`] and [`http`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod apps;
pub mod claw;
pub mod fleet;
pub mod http;
pub mod jobs;
pub mod logs;
pub mod mcp;
pub mod metrics;
pub mod notify;
pub mod quickcheck;
pub mod store;
pub mod watches;

/// `<root>/lab/config.json` (mode 0600). Field names follow OmegaEnv's environment
/// variables, lower-cased without the `OMEGA_` prefix.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Base URL people use for this UI (links in Signal messages), e.g. https://og-host:8006.
    pub public_url: String,
    pub openclaw_url: String,
    pub openclaw_password: String,
    pub openclaw_agent: String,
    pub openclaw_relay_url: String,
    pub watch_agent: String,
    pub station_control: String,
    pub station_control_token: String,
    pub homepage_services: String,
    pub signal_ssh: String,
    pub signal_url: String,
    pub signal_to: String,
    /// Log directory (default `<lab>/logs`) and retention.
    pub log_dir: String,
    pub log_retain_days: u64,
    /// Metric database (default `<lab>/metrics.db`).
    pub metrics_db: String,
    /// false = never send Signal alerts.
    pub notify: Option<bool>,
    /// Time zone for times in messages (IANA name, display only).
    pub tz: String,
}

/// `<lab>/secrets.json`, OmegaEnv-compatible (created with random values when missing).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Secrets {
    pub admin_key: String,
    pub agent_token: String,
    pub api_token: String,
    pub notify_token: String,
}

/// Who is calling, as decided by the HTTP layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// Signed-in UI user (or the OmegaEnv admin key).
    Admin,
    /// OpenClaw / scripts with the API token.
    Api,
    /// A machine's omega-agent.
    Agent,
    Anonymous,
}

impl Caller {
    pub fn admin(self) -> bool {
        self == Caller::Admin
    }
    pub fn api(self) -> bool {
        matches!(self, Caller::Admin | Caller::Api)
    }
}

/// The OG-Host node itself, reporting without an agent.
pub trait LocalNode: Send + Sync + 'static {
    fn name(&self) -> String;
    /// A snapshot in omega-agent's format (host, cpu, mem, disks, net, gpus, containers, projects, ...).
    fn snapshot(&self) -> Value;
    /// Run an agent-style job (`compose` / `deploy` / `remove` / `read` / `write`) on this node.
    fn run_job(&self, job: Value) -> futures_util::future::BoxFuture<'static, (bool, String, Option<Value>)>;
}

pub struct Lab {
    pub dir: PathBuf,
    pub cfg: Config,
    pub secrets: Secrets,
    pub metrics: Arc<metrics::Metrics>,
    pub logs: Arc<logs::Logs>,
    pub fleet: fleet::Fleet,
    pub apps: apps::Apps,
    pub jobs: jobs::Jobs,
    pub watches: watches::Watches,
    pub notify: notify::Notifier,
    pub claw: claw::Claw,
    pub views: Mutex<Vec<Value>>,
    pub local: RwLock<Option<Arc<dyn LocalNode>>>,
    /// OG-Host's AI model server (for watch checks the rules can't decide)
    pub ai: RwLock<Option<quickcheck::AiCfg>>,
    pub http: reqwest::Client,
    /// Same, but never following redirects (health checks, icon fetches).
    http_nr: reqwest::Client,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Constant-time comparison of two secrets (false when either is empty).
pub fn safe_eq(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn random_token(bytes: usize) -> String {
    use std::io::Read;
    let mut b = vec![0u8; bytes];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    // base64url without padding, like OmegaEnv's randomBytes(18).toString('base64url')
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk.iter().fold(0u32, |acc, x| (acc << 8) | *x as u32) << (8 * (3 - chunk.len()));
        for i in 0..chunk.len() + 1 {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub fn uuid() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn fmt_bytes(n: f64) -> String {
    let u = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut n = n;
    let mut i = 0;
    while n >= 1024.0 && i < u.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if n >= 10.0 || i == 0 { format!("{:.0} {}", n, u[i]) } else { format!("{:.1} {}", n, u[i]) }
}

impl Lab {
    /// Open (or create) the lab directory and start every background task.
    pub async fn open(dir: &Path) -> Result<Arc<Lab>> {
        std::fs::create_dir_all(dir)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)); // logs and secrets live here
        }
        let cfg: Config = store::load(dir, "config.json").unwrap_or_default();
        let mut secrets: Secrets = store::load(dir, "secrets.json").unwrap_or_default();
        let mut changed = false;
        for s in [&mut secrets.admin_key, &mut secrets.agent_token, &mut secrets.api_token, &mut secrets.notify_token] {
            if s.is_empty() {
                *s = random_token(18);
                changed = true;
            }
        }
        if changed {
            store::save_private(dir, "secrets.json", &secrets)?;
        }
        let log_dir = if cfg.log_dir.is_empty() { dir.join("logs") } else { PathBuf::from(&cfg.log_dir) };
        let db = if cfg.metrics_db.is_empty() { dir.join("metrics.db") } else { PathBuf::from(&cfg.metrics_db) };
        let metrics = Arc::new(metrics::Metrics::open(&db).with_context(|| format!("opening {}", db.display()))?);
        let logs = Arc::new(logs::Logs::new(log_dir, if cfg.log_retain_days > 0 { cfg.log_retain_days } else { 14 }));
        let http = reqwest::Client::builder().user_agent(concat!("og-host/", env!("CARGO_PKG_VERSION"))).build()?;
        let http_nr = reqwest::Client::builder().user_agent(concat!("og-host/", env!("CARGO_PKG_VERSION"))).redirect(reqwest::redirect::Policy::none()).build()?;
        let lab = Arc::new(Lab {
            apps: apps::Apps::load(dir),
            fleet: fleet::Fleet::load(dir),
            jobs: jobs::Jobs::load(dir),
            watches: watches::Watches::load(dir),
            notify: notify::Notifier::load(dir),
            claw: claw::Claw::default(),
            views: Mutex::new(store::load(dir, "views.json").unwrap_or_default()),
            local: RwLock::new(None),
            ai: RwLock::new(None),
            dir: dir.to_path_buf(),
            cfg,
            secrets,
            metrics,
            logs,
            http,
            http_nr,
        });
        lab.start_background();
        Ok(lab)
    }

    pub fn http_noredirect(&self) -> &reqwest::Client {
        &self.http_nr
    }

    pub fn set_local(&self, node: Arc<dyn LocalNode>) {
        *self.local.write().unwrap() = Some(node);
    }

    pub fn local_name(&self) -> Option<String> {
        self.local.read().unwrap().as_ref().map(|n| n.name())
    }

    /// Decide the caller from a bearer token (`Authorization: Bearer` or `X-Omega-Key`).
    pub fn caller_for_token(&self, token: &str) -> Caller {
        if safe_eq(token, &self.secrets.admin_key) {
            Caller::Admin
        } else if safe_eq(token, &self.secrets.api_token) {
            Caller::Api
        } else if safe_eq(token, &self.secrets.agent_token) {
            Caller::Agent
        } else {
            Caller::Anonymous
        }
    }

    pub fn link(&self, hash: &str) -> String {
        let base = if self.cfg.public_url.is_empty() { "https://og-host:8006".to_string() } else { self.cfg.public_url.trim_end_matches('/').to_string() };
        format!("{base}/{hash}")
    }

    fn start_background(self: &Arc<Self>) {
        // the node itself, every 10 s, like an agent
        let l = self.clone();
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let node = l.local.read().unwrap().clone();
                if let Some(node) = node {
                    let snap = tokio::task::spawn_blocking(move || node.snapshot()).await.unwrap_or(Value::Null);
                    if snap.is_object() {
                        let slow = n % 3 == 0;
                        l.fleet.ingest(&l.metrics, snap, slow);
                    }
                }
                n += 1;
            }
        });
        apps::start(self.clone());
        metrics::start(self.metrics.clone());
        logs::start(self.logs.clone());
        notify::start(self.clone());
        watches::start(self.clone());
        jobs::start(self.clone());
        fleet::start(self.clone());
        claw::warm(self.clone());
    }
}

/// Copy an OmegaEnv data directory (and optional .env) into a lab directory.
pub fn import_omegaenv(from: &Path, env_file: Option<&Path>, to: &Path, with_secrets: bool) -> Result<Vec<String>> {
    std::fs::create_dir_all(to)?;
    let mut done = Vec::new();
    for f in [
        "apps.json", "groups.json", "removed-apps.json", "hosts.json", "checks.json", "known.json", "demo.json", "views.json",
        "watches.json", "stacks.json", "activity.json", "notify.json",
    ] {
        let src = from.join(f);
        if src.exists() {
            std::fs::copy(&src, to.join(f)).with_context(|| format!("copying {f}"))?;
            done.push(f.to_string());
        }
    }
    if with_secrets && from.join("secrets.json").exists() {
        let s: Secrets = serde_json::from_slice(&std::fs::read(from.join("secrets.json"))?)?;
        store::save_private(to, "secrets.json", &s)?;
        done.push("secrets.json".into());
    }
    if let Some(env) = env_file {
        let mut cfg: Config = store::load(to, "config.json").unwrap_or_default();
        let text = std::fs::read_to_string(env)?;
        let vars: HashMap<String, String> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').trim_matches('\'').to_string()))
            .collect();
        let g = |k: &str| vars.get(k).cloned().unwrap_or_default();
        cfg.openclaw_url = g("OMEGA_OPENCLAW_URL");
        cfg.openclaw_password = g("OMEGA_OPENCLAW_PASSWORD");
        cfg.openclaw_agent = g("OMEGA_OPENCLAW_AGENT");
        cfg.openclaw_relay_url = g("OMEGA_OPENCLAW_RELAY_URL");
        cfg.watch_agent = g("OMEGA_WATCH_AGENT");
        cfg.station_control = g("OMEGA_STATION_CONTROL");
        cfg.station_control_token = g("OMEGA_STATION_CONTROL_TOKEN");
        cfg.homepage_services = g("OMEGA_HOMEPAGE_SERVICES");
        cfg.signal_ssh = g("OMEGA_SIGNAL_SSH");
        cfg.signal_url = g("OMEGA_SIGNAL_URL");
        cfg.signal_to = g("OMEGA_SIGNAL_TO");
        cfg.log_dir = g("OMEGA_LOG_DIR");
        cfg.metrics_db = g("OMEGA_METRICS_DB");
        store::save_private(to, "config.json", &cfg)?;
        done.push("config.json (from .env)".into());
    }
    Ok(done)
}
