//! og-compose: read a compose file into a normalised, typed project.
//!
//! Every syntax variant compose allows is folded into one shape (short/long `ports` and
//! `volumes`, `environment` as map or list, `command` as string or list, `depends_on` as list
//! or map with conditions, ...). Keys OG-Host doesn't implement yet are reported in
//! [`Project::warnings`] instead of being silently ignored.

pub mod interp;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("interpolation: {0}")]
    Interp(String),
    #[error("dependency cycle: {0}")]
    Cycle(String),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum MountKind {
    /// Host path (resolved relative to the project directory).
    Bind,
    /// Named volume managed by OG-Host.
    Volume,
    Tmpfs,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Mount {
    pub kind: MountKind,
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Build {
    pub context: PathBuf,
    pub dockerfile: Option<String>,
    pub args: BTreeMap<String, String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Healthcheck {
    /// `["CMD", ...]`, `["CMD-SHELL", "..."]`, or empty when disabled.
    pub test: Vec<String>,
    pub interval_s: u64,
    pub timeout_s: u64,
    pub retries: u32,
    pub start_period_s: u64,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum DependsCondition {
    Started,
    Healthy,
    CompletedSuccessfully,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Service {
    pub name: String,
    pub image: Option<String>,
    pub build: Option<Build>,
    pub container_name: Option<String>,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    /// `KEY=value`, env_file entries first, then `environment` (which wins).
    pub environment: Vec<String>,
    pub ports: Vec<String>,
    pub mounts: Vec<Mount>,
    pub restart: Option<String>,
    pub depends_on: Vec<(String, DependsCondition)>,
    pub networks: Vec<String>,
    pub network_mode: Option<String>,
    pub hostname: Option<String>,
    pub extra_hosts: Vec<(String, String)>,
    pub dns: Vec<String>,
    pub dns_search: Vec<String>,
    pub dns_opt: Vec<String>,
    /// `read_only: true`: the container's root filesystem is mounted read-only
    pub read_only: bool,
    /// `tmpfs:` entries, `/path[:options]` (e.g. `/run:size=64m,mode=0755`)
    pub tmpfs: Vec<String>,
    pub devices: Vec<String>,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub shm_size: Option<String>,
    pub mem_limit: Option<String>,
    /// mem_reservation / deploy.resources.reservations.memory (-> memory.low).
    pub mem_reservation: Option<String>,
    /// cpus / deploy.resources.limits.cpus.
    pub cpus: Option<f64>,
    pub cpuset: Option<String>,
    pub cpu_shares: Option<u64>,
    /// pids_limit / deploy.resources.limits.pids.
    pub pids_limit: Option<i64>,
    pub blkio_weight: Option<u16>,
    /// blkio_config device limits: (path, kind 0=read_bps 1=write_bps 2=read_iops 3=write_iops, rate).
    pub blkio_devices: Vec<(String, u8, String)>,
    pub user: Option<String>,
    pub working_dir: Option<String>,
    pub healthcheck: Option<Healthcheck>,
    pub security_opt: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub privileged: bool,
    pub gpus: bool,
    /// GPUs asked for by index or UUID (`device_ids`); empty with `gpus` = all of them
    pub gpu_ids: Vec<String>,
    pub stop_grace_s: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Project {
    pub name: String,
    pub dir: PathBuf,
    /// The compose file it was loaded from.
    pub file: Option<PathBuf>,
    /// Services in start order (dependencies first).
    pub services: Vec<Service>,
    /// name -> external?
    pub networks: BTreeMap<String, bool>,
    /// name -> external?
    pub volumes: BTreeMap<String, bool>,
    pub warnings: Vec<String>,
}

const SUPPORTED_SERVICE_KEYS: &[&str] = &[
    "image", "build", "container_name", "command", "entrypoint", "environment", "env_file", "ports", "volumes",
    "restart", "depends_on", "networks", "network_mode", "hostname", "extra_hosts", "dns", "dns_search", "dns_opt", "read_only", "tmpfs", "devices", "cap_add",
    "cap_drop", "shm_size", "mem_limit", "mem_reservation", "cpus", "cpuset", "cpu_shares", "pids_limit",
    "blkio_config", "user", "working_dir", "healthcheck", "security_opt", "labels",
    "privileged", "deploy", "runtime", "stop_grace_period", "expose", "tty", "stdin_open", "init",
];

fn s(v: &Value) -> Option<String> {
    match v {
        Value::String(x) => Some(x.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Sequence(seq)) => seq.iter().filter_map(s).collect(),
        Some(x) => s(x).into_iter().collect(),
        None => vec![],
    }
}

/// Minimal POSIX-ish word splitting for `command: "..."` strings (quotes and backslashes).
pub fn shell_split(input: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err(Error::Invalid(format!("unclosed ' in {input:?}"))),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(x @ ('"' | '\\' | '$' | '`')) => cur.push(x),
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err(Error::Invalid(format!("trailing \\ in {input:?}"))),
                        },
                        Some(x) => cur.push(x),
                        None => return Err(Error::Invalid(format!("unclosed \" in {input:?}"))),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(x) = chars.next() {
                    cur.push(x);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

fn command(v: Option<&Value>) -> Result<Option<Vec<String>>> {
    Ok(match v {
        None | Some(Value::Null) => None,
        Some(Value::String(x)) => Some(shell_split(x)?),
        Some(Value::Sequence(_)) => Some(str_list(v)),
        Some(other) => return Err(Error::Invalid(format!("bad command {other:?}"))),
    })
}

/// `KEY=value` list from a map or list form; `KEY` alone takes the value from `vars` (or is dropped).
fn env_entries(v: Option<&Value>, vars: &interp::Vars) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Some(Value::Mapping(m)) => {
            for (k, val) in m {
                let Some(k) = s(k) else { continue };
                match val {
                    Value::Null => {
                        if let Some(x) = vars.get(&k) {
                            out.push(format!("{k}={x}"));
                        }
                    }
                    other => out.push(format!("{k}={}", s(other).unwrap_or_default())),
                }
            }
        }
        Some(Value::Sequence(seq)) => {
            for item in seq.iter().filter_map(s) {
                if item.contains('=') {
                    out.push(item);
                } else if let Some(x) = vars.get(&item) {
                    out.push(format!("{item}={x}"));
                }
            }
        }
        _ => {}
    }
    out
}

fn is_named_volume(src: &str) -> bool {
    !(src.starts_with('/') || src.starts_with('.') || src.starts_with('~'))
}

fn mounts(v: Option<&Value>, dir: &Path, warnings: &mut Vec<String>, svc: &str) -> Result<Vec<Mount>> {
    let mut out = Vec::new();
    let Some(Value::Sequence(seq)) = v else { return Ok(out) };
    for item in seq {
        match item {
            Value::String(spec) => {
                let parts: Vec<&str> = spec.split(':').collect();
                let (src, dst, mode) = match parts.as_slice() {
                    [d] => ("", *d, ""),
                    [a, b] => (*a, *b, ""),
                    [a, b, m] => (*a, *b, *m),
                    _ => return Err(Error::Invalid(format!("{svc}: bad volume {spec:?}"))),
                };
                if src.is_empty() {
                    warnings.push(format!("{svc}: anonymous volume {dst} becomes a named volume"));
                    out.push(Mount { kind: MountKind::Volume, source: format!("{svc}-anon{}", dst.replace('/', "-")), target: dst.into(), read_only: false });
                    continue;
                }
                let ro = mode.split(',').any(|m| m == "ro");
                if is_named_volume(src) {
                    out.push(Mount { kind: MountKind::Volume, source: src.into(), target: dst.into(), read_only: ro });
                } else {
                    out.push(Mount { kind: MountKind::Bind, source: resolve_path(src, dir), target: dst.into(), read_only: ro });
                }
            }
            Value::Mapping(m) => {
                let get = |k: &str| m.get(Value::from(k)).and_then(s);
                let typ = get("type").unwrap_or_else(|| "volume".into());
                let target = get("target").ok_or_else(|| Error::Invalid(format!("{svc}: volume without target")))?;
                let ro = matches!(m.get(Value::from("read_only")), Some(Value::Bool(true)));
                let source = get("source").unwrap_or_default();
                let kind = match typ.as_str() {
                    "bind" => MountKind::Bind,
                    "volume" => MountKind::Volume,
                    "tmpfs" => MountKind::Tmpfs,
                    other => {
                        warnings.push(format!("{svc}: volume type {other:?} not supported, skipped"));
                        continue;
                    }
                };
                let source = if kind == MountKind::Bind { resolve_path(&source, dir) } else { source };
                out.push(Mount { kind, source, target, read_only: ro });
            }
            _ => return Err(Error::Invalid(format!("{svc}: bad volume entry"))),
        }
    }
    Ok(out)
}

fn resolve_path(p: &str, dir: &Path) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(h) = std::env::var("HOME") {
            return format!("{h}/{rest}");
        }
    }
    let path = Path::new(p);
    if path.is_absolute() { p.to_string() } else { dir.join(path).to_string_lossy().replace("/./", "/") }
}

fn ports(v: Option<&Value>, svc: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let Some(Value::Sequence(seq)) = v else { return Ok(out) };
    for item in seq {
        match item {
            Value::Mapping(m) => {
                let get = |k: &str| m.get(Value::from(k)).and_then(s);
                let target = get("target").ok_or_else(|| Error::Invalid(format!("{svc}: port without target")))?;
                let published = get("published").unwrap_or_else(|| target.clone());
                let proto = get("protocol").unwrap_or_else(|| "tcp".into());
                let host_ip = get("host_ip").map(|h| format!("{h}:")).unwrap_or_default();
                out.push(format!("{host_ip}{published}:{target}/{proto}"));
            }
            other => out.push(s(other).ok_or_else(|| Error::Invalid(format!("{svc}: bad port")))?),
        }
    }
    Ok(out)
}

fn duration_s(v: Option<&Value>, default: u64) -> u64 {
    let Some(text) = v.and_then(s) else { return default };
    let t = text.trim();
    let (num, mult) = if let Some(n) = t.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = t.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (t, 1.0)
    };
    num.parse::<f64>().map(|n| (n * mult).ceil() as u64).unwrap_or(default)
}

fn healthcheck(v: Option<&Value>) -> Option<Healthcheck> {
    let Value::Mapping(m) = v? else { return None };
    let get = |k: &str| m.get(Value::from(k));
    let disabled = matches!(get("disable"), Some(Value::Bool(true)));
    let test = match get("test") {
        Some(Value::String(x)) => vec!["CMD-SHELL".into(), x.clone()],
        t => str_list(t),
    };
    let disabled = disabled || test.first().map(String::as_str) == Some("NONE");
    Some(Healthcheck {
        test,
        interval_s: duration_s(get("interval"), 30),
        timeout_s: duration_s(get("timeout"), 30),
        retries: get("retries").and_then(s).and_then(|x| x.parse().ok()).unwrap_or(3),
        start_period_s: duration_s(get("start_period"), 0),
        disabled,
    })
}

fn depends(v: Option<&Value>) -> Vec<(String, DependsCondition)> {
    match v {
        Some(Value::Sequence(_)) => str_list(v).into_iter().map(|n| (n, DependsCondition::Started)).collect(),
        Some(Value::Mapping(m)) => m
            .iter()
            .filter_map(|(k, val)| {
                let cond = val.get("condition").and_then(s).unwrap_or_default();
                let c = match cond.as_str() {
                    "service_healthy" => DependsCondition::Healthy,
                    "service_completed_successfully" => DependsCondition::CompletedSuccessfully,
                    _ => DependsCondition::Started,
                };
                Some((s(k)?, c))
            })
            .collect(),
        _ => vec![],
    }
}

fn extra_hosts(v: Option<&Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    match v {
        Some(Value::Mapping(m)) => {
            for (k, val) in m {
                if let (Some(k), Some(val)) = (s(k), s(val)) {
                    out.push((k, val));
                }
            }
        }
        _ => {
            for e in str_list(v) {
                // host:ip, host=ip; the ip may itself contain ':' (IPv6)
                if let Some((h, ip)) = e.split_once('=').or_else(|| e.split_once(':')) {
                    out.push((h.to_string(), ip.to_string()));
                }
            }
        }
    }
    out
}

/// Load a compose file. `env` overrides values from `<dir>/.env` (like the shell does).
pub fn load(file: &Path, project_name: Option<&str>, env: &interp::Vars) -> Result<Project> {
    let dir = file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf();
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let mut vars = std::fs::read_to_string(dir.join(".env")).map(|t| interp::parse_env_file(&t)).unwrap_or_default();
    vars.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut doc: Value = serde_yaml::from_str(&std::fs::read_to_string(file)?)?;
    interp::interpolate_value(&mut doc, &vars)?;

    let mut warnings = Vec::new();
    let name = project_name
        .map(str::to_string)
        .or_else(|| doc.get("name").and_then(s))
        .unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "default".into()))
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>();

    let top_map = |key: &str| -> BTreeMap<String, bool> {
        match doc.get(key) {
            Some(Value::Mapping(m)) => m
                .iter()
                .filter_map(|(k, v)| {
                    let ext = matches!(v.get("external"), Some(Value::Bool(true)));
                    Some((s(k)?, ext))
                })
                .collect(),
            _ => BTreeMap::new(),
        }
    };
    let networks = top_map("networks");
    let volumes = top_map("volumes");
    for key in doc.as_mapping().map(|m| m.keys().filter_map(s).collect::<Vec<_>>()).unwrap_or_default() {
        if !["services", "networks", "volumes", "name", "version", "configs", "secrets"].contains(&key.as_str()) && !key.starts_with("x-") {
            warnings.push(format!("top-level key {key:?} ignored"));
        }
        if key == "configs" || key == "secrets" {
            warnings.push(format!("top-level {key} not supported yet"));
        }
    }

    let Some(Value::Mapping(svcs)) = doc.get("services") else {
        return Err(Error::Invalid("compose file has no services".into()));
    };
    let mut services = Vec::new();
    for (k, v) in svcs {
        let svc_name = s(k).ok_or_else(|| Error::Invalid("service name must be a string".into()))?;
        let Value::Mapping(m) = v else { return Err(Error::Invalid(format!("service {svc_name} must be a mapping"))) };
        let get = |key: &str| m.get(Value::from(key));
        for key in m.keys().filter_map(s) {
            if !SUPPORTED_SERVICE_KEYS.contains(&key.as_str()) && !key.starts_with("x-") {
                warnings.push(format!("{svc_name}: `{key}` not supported yet, ignored"));
            }
        }

        // env_file first, then environment (wins)
        let mut environment = Vec::new();
        for f in str_list(get("env_file")) {
            let p = Path::new(&resolve_path(&f, &dir)).to_path_buf();
            match std::fs::read_to_string(&p) {
                Ok(t) => environment.extend(interp::parse_env_file(&t).into_iter().map(|(k, v)| format!("{k}={v}"))),
                Err(e) => return Err(Error::Invalid(format!("{svc_name}: env_file {}: {e}", p.display()))),
            }
        }
        environment.extend(env_entries(get("environment"), &vars));

        let build = match get("build") {
            None => None,
            Some(Value::String(ctx)) => Some(Build { context: PathBuf::from(resolve_path(ctx, &dir)), dockerfile: None, args: BTreeMap::new(), target: None }),
            Some(Value::Mapping(b)) => {
                let bget = |k: &str| b.get(Value::from(k));
                let args = match bget("args") {
                    Some(Value::Mapping(a)) => a.iter().filter_map(|(k, v)| Some((s(k)?, s(v).unwrap_or_default()))).collect(),
                    Some(Value::Sequence(_)) => str_list(bget("args")).into_iter().filter_map(|e| e.split_once('=').map(|(a, b)| (a.to_string(), b.to_string()))).collect(),
                    _ => BTreeMap::new(),
                };
                Some(Build {
                    context: PathBuf::from(resolve_path(&bget("context").and_then(s).unwrap_or_else(|| ".".into()), &dir)),
                    dockerfile: bget("dockerfile").and_then(s),
                    args,
                    target: bget("target").and_then(s),
                })
            }
            Some(_) => return Err(Error::Invalid(format!("{svc_name}: bad build"))),
        };

        let deploy = get("deploy");
        let mem_limit = get("mem_limit").and_then(s).or_else(|| {
            deploy.and_then(|d| d.get("resources")).and_then(|r| r.get("limits")).and_then(|l| l.get("memory")).and_then(s)
        });
        let limits = deploy.and_then(|d| d.get("resources")).and_then(|r| r.get("limits"));
        let reservations = deploy.and_then(|d| d.get("resources")).and_then(|r| r.get("reservations"));
        let num = |v: Option<&Value>| -> Option<f64> {
            match v? {
                Value::Number(n) => n.as_f64(),
                Value::String(x) => x.trim().parse().ok(),
                _ => None,
            }
        };
        let mem_reservation = get("mem_reservation").and_then(s).or_else(|| reservations.and_then(|r| r.get("memory")).and_then(s));
        let cpus = num(get("cpus")).or_else(|| num(limits.and_then(|l| l.get("cpus")))).filter(|c| *c > 0.0);
        let pids_limit = num(get("pids_limit")).or_else(|| num(limits.and_then(|l| l.get("pids")))).map(|p| p as i64).filter(|p| *p > 0);
        let blkio = get("blkio_config");
        let blkio_weight = num(blkio.and_then(|b| b.get("weight"))).map(|w| w as u16);
        let mut blkio_devices = Vec::new();
        for (key, kind) in [("device_read_bps", 0u8), ("device_write_bps", 1), ("device_read_iops", 2), ("device_write_iops", 3)] {
            for d in blkio.and_then(|b| b.get(key)).and_then(Value::as_sequence).into_iter().flatten() {
                if let (Some(p), Some(r)) = (d.get("path").and_then(s), d.get("rate").and_then(s)) {
                    blkio_devices.push((p, kind, r));
                }
            }
        }
        let gpus = get("runtime").and_then(s).as_deref() == Some("nvidia")
            || deploy
                .and_then(|d| d.get("resources"))
                .and_then(|r| r.get("reservations"))
                .and_then(|r| r.get("devices"))
                .and_then(Value::as_sequence)
                .is_some_and(|devs| {
                    devs.iter().any(|d| {
                        d.get("driver").and_then(s).as_deref() == Some("nvidia") || str_list(d.get("capabilities")).iter().any(|c| c == "gpu")
                    })
                });

        let gpu_ids: Vec<String> = deploy
            .and_then(|d| d.get("resources"))
            .and_then(|r| r.get("reservations"))
            .and_then(|r| r.get("devices"))
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
            .flat_map(|d| str_list(d.get("device_ids")))
            .collect();

        let svc_networks = match get("networks") {
            Some(Value::Mapping(n)) => n.keys().filter_map(s).collect(),
            other => str_list(other),
        };
        let labels = match get("labels") {
            Some(Value::Mapping(l)) => l.iter().filter_map(|(k, v)| Some((s(k)?, s(v).unwrap_or_default()))).collect(),
            other => str_list(other).into_iter().filter_map(|e| e.split_once('=').map(|(a, b)| (a.to_string(), b.to_string()))).collect(),
        };

        let svc = Service {
            name: svc_name.clone(),
            image: get("image").and_then(s),
            build,
            container_name: get("container_name").and_then(s),
            command: command(get("command"))?,
            entrypoint: command(get("entrypoint"))?,
            environment,
            ports: ports(get("ports"), &svc_name)?,
            mounts: mounts(get("volumes"), &dir, &mut warnings, &svc_name)?,
            restart: get("restart").and_then(s),
            depends_on: depends(get("depends_on")),
            networks: svc_networks,
            network_mode: get("network_mode").and_then(s),
            hostname: get("hostname").and_then(s),
            extra_hosts: extra_hosts(get("extra_hosts")),
            dns: str_list(get("dns")),
            dns_search: str_list(get("dns_search")),
            dns_opt: str_list(get("dns_opt")),
            read_only: matches!(get("read_only"), Some(Value::Bool(true))),
            tmpfs: str_list(get("tmpfs")),
            devices: str_list(get("devices")),
            cap_add: str_list(get("cap_add")),
            cap_drop: str_list(get("cap_drop")),
            shm_size: get("shm_size").and_then(s),
            mem_limit,
            mem_reservation,
            cpus,
            cpuset: get("cpuset").and_then(s),
            cpu_shares: num(get("cpu_shares")).map(|c| c as u64).filter(|c| *c > 0),
            pids_limit,
            blkio_weight,
            blkio_devices,
            user: get("user").and_then(s),
            working_dir: get("working_dir").and_then(s),
            healthcheck: healthcheck(get("healthcheck")),
            security_opt: str_list(get("security_opt")),
            labels,
            privileged: matches!(get("privileged"), Some(Value::Bool(true))),
            gpus,
            gpu_ids,
            stop_grace_s: get("stop_grace_period").map(|v| duration_s(Some(v), 10)),
        };
        if svc.image.is_none() && svc.build.is_none() {
            return Err(Error::Invalid(format!("service {svc_name} has neither image nor build")));
        }
        services.push(svc);
    }
    let services = start_order(services)?;
    let file = std::fs::canonicalize(file).ok();
    Ok(Project { name, dir, file, services, networks, volumes, warnings })
}

/// Topological order by depends_on (and `network_mode: service:x`), stable by name otherwise.
pub fn start_order(services: Vec<Service>) -> Result<Vec<Service>> {
    let names: BTreeSet<String> = services.iter().map(|s| s.name.clone()).collect();
    let deps = |s: &Service| -> Vec<String> {
        let mut d: Vec<String> = s.depends_on.iter().map(|(n, _)| n.clone()).collect();
        if let Some(n) = s.network_mode.as_deref().and_then(|m| m.strip_prefix("service:")) {
            d.push(n.to_string());
        }
        d
    };
    for s in &services {
        for d in deps(s) {
            if !names.contains(&d) {
                return Err(Error::Invalid(format!("{} depends on unknown service {d}", s.name)));
            }
        }
    }
    let mut by_name: BTreeMap<String, Service> = services.into_iter().map(|s| (s.name.clone(), s)).collect();
    let mut done: Vec<Service> = Vec::new();
    let mut placed = BTreeSet::new();
    while !by_name.is_empty() {
        let ready: Vec<String> = by_name.values().filter(|s| deps(s).iter().all(|d| placed.contains(d))).map(|s| s.name.clone()).collect();
        if ready.is_empty() {
            return Err(Error::Cycle(by_name.keys().cloned().collect::<Vec<_>>().join(" <-> ")));
        }
        for n in ready {
            placed.insert(n.clone());
            done.push(by_name.remove(&n).unwrap());
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_str(yaml: &str, env: &[(&str, &str)]) -> Result<Project> {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("docker-compose.yml");
        std::fs::write(&f, yaml).unwrap();
        let vars = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let r = load(&f, Some("test"), &vars);
        drop(d);
        r
    }

    #[test]
    fn real_world_shapes() {
        let p = load_str(
            r#"
services:
  db:
    image: postgres:16
    environment:
      POSTGRES_PASSWORD: ${DB_PASS:-secret}
      POSTGRES_USER: app
    volumes: [dbdata:/var/lib/postgresql/data]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U app"]
      interval: 5s
      retries: 10
  web:
    image: nginx
    command: nginx -g "daemon off;"
    ports: ["8080:80", {target: 443, published: 8443, protocol: tcp}]
    depends_on:
      db: {condition: service_healthy}
    volumes:
      - ./html:/usr/share/nginx/html:ro
      - type: tmpfs
        target: /cache
    environment: [A=1, B]
    extra_hosts: ["host.docker.internal:host-gateway"]
    deploy: {resources: {limits: {memory: 256M}}}
  vpn:
    image: qmcgaw/gluetun
    cap_add: [NET_ADMIN]
    devices: [/dev/net/tun]
  app:
    image: lscr.io/linuxserver/radarr
    network_mode: service:vpn
  gpu:
    image: ollama/ollama
    deploy: {resources: {reservations: {devices: [{driver: nvidia, count: all, capabilities: [gpu]}]}}}
    mystery_key: 1
volumes:
  dbdata: {}
"#,
            &[("B", "from-env")],
        )
        .unwrap();
        let order: Vec<_> = p.services.iter().map(|s| s.name.as_str()).collect();
        assert!(order.iter().position(|n| *n == "db") < order.iter().position(|n| *n == "web"));
        assert!(order.iter().position(|n| *n == "vpn") < order.iter().position(|n| *n == "app"));
        let db = &p.services.iter().find(|s| s.name == "db").unwrap();
        assert!(db.environment.contains(&"POSTGRES_PASSWORD=secret".to_string()));
        assert_eq!(db.mounts[0].kind, MountKind::Volume);
        assert_eq!(db.healthcheck.as_ref().unwrap().interval_s, 5);
        let web = p.services.iter().find(|s| s.name == "web").unwrap();
        assert_eq!(web.command.as_ref().unwrap(), &vec!["nginx".to_string(), "-g".into(), "daemon off;".into()]);
        assert_eq!(web.ports, vec!["8080:80".to_string(), "8443:443/tcp".into()]);
        assert!(web.environment.contains(&"B=from-env".to_string()));
        assert_eq!(web.mounts[0].kind, MountKind::Bind);
        assert!(web.mounts[0].read_only && web.mounts[0].source.ends_with("/html"));
        assert_eq!(web.mounts[1].kind, MountKind::Tmpfs);
        assert_eq!(web.mem_limit.as_deref(), Some("256M"));
        assert_eq!(web.depends_on[0].1, DependsCondition::Healthy);
        assert_eq!(web.extra_hosts[0], ("host.docker.internal".to_string(), "host-gateway".to_string()));
        assert!(p.services.iter().find(|s| s.name == "gpu").unwrap().gpus);
        assert!(p.warnings.iter().any(|w| w.contains("mystery_key")));
        assert_eq!(p.volumes.len(), 1);
    }

    #[test]
    fn cycles_and_unknown_dependencies_are_errors() {
        let cyc = "services:\n  a: {image: x, depends_on: [b]}\n  b: {image: x, depends_on: [a]}\n";
        assert!(matches!(load_str(cyc, &[]), Err(Error::Cycle(_))));
        let unknown = "services:\n  a: {image: x, depends_on: [ghost]}\n";
        assert!(load_str(unknown, &[]).is_err());
        assert!(load_str("services:\n  a: {restart: always}\n", &[]).is_err(), "no image or build");
    }

    #[test]
    fn required_variables() {
        let y = "services:\n  a: {image: \"x:${TAG:?TAG must be set}\"}\n";
        assert!(load_str(y, &[]).unwrap_err().to_string().contains("TAG must be set"));
        assert_eq!(load_str(y, &[("TAG", "1.2")]).unwrap().services[0].image.as_deref(), Some("x:1.2"));
    }

    #[test]
    fn shell_splitting() {
        assert_eq!(shell_split(r#"sh -c "echo 'hi there'" a\ b"#).unwrap(), ["sh", "-c", "echo 'hi there'", "a b"]);
        assert_eq!(shell_split("  a   b ").unwrap(), ["a", "b"]);
        assert_eq!(shell_split("''").unwrap(), [""]);
        assert!(shell_split("\"open").is_err());
    }
}
