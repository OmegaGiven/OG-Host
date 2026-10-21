//! og-runtime: turn an image config + run options into an OCI runtime bundle.
//!
//! Defaults are chosen for safety and Docker compatibility:
//! - user namespace on by default: container root maps to an unprivileged host range;
//! - Docker's default capability set (not more), `no_new_privileges`;
//! - pid limit, masked/readonly kernel paths as in runc/Docker;
//! - memory/CPU controls exposed as cgroup v2 knobs (`memory.low/high/max`, `cpu.weight`)
//!   so OG-Host's resource brain can adjust them live.

pub mod users;

use std::path::{Path, PathBuf};

use og_oci::registry::ImageConfig;
use oci_spec::runtime::{
    Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxIdMappingBuilder, LinuxMemoryBuilder, LinuxNamespace,
    LinuxNamespaceBuilder, LinuxNamespaceType, LinuxPidsBuilder, LinuxResourcesBuilder, LinuxCpuBuilder, Mount,
    MountBuilder, ProcessBuilder, RootBuilder, Spec, SpecBuilder, UserBuilder,
};

/// Docker's default capability set. Containers get exactly these unless they add/drop.
pub const DOCKER_DEFAULT_CAPS: &[Capability] = &[
    Capability::Chown,
    Capability::DacOverride,
    Capability::Fsetid,
    Capability::Fowner,
    Capability::Mknod,
    Capability::NetRaw,
    Capability::Setgid,
    Capability::Setuid,
    Capability::Setfcap,
    Capability::Setpcap,
    Capability::NetBindService,
    Capability::SysChroot,
    Capability::Kill,
    Capability::AuditWrite,
];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("image has no command to run (no Entrypoint/Cmd and none given)")]
    NoCommand,
    #[error(transparent)]
    Spec(#[from] oci_spec::OciSpecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Host id range a container's ids 0..size map onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdMap {
    pub host_start: u32,
    pub size: u32,
}

impl Default for IdMap {
    fn default() -> Self {
        IdMap { host_start: 100_000, size: 65_536 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// Own, empty network namespace (only loopback). The default until og-net wires bridges.
    Isolated,
    /// Join an existing network namespace (bridge networking, or `network_mode: service:x`).
    Join(PathBuf),
    /// Share the host's network (`network_mode: host`). Requires `userns: None`.
    Host,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindMount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub hostname: String,
    pub rootfs: PathBuf,
    /// Replaces the image Entrypoint when set (`entrypoint:` in compose).
    pub entrypoint: Option<Vec<String>>,
    /// Replaces the image Cmd when set (`command:` in compose).
    pub command: Option<Vec<String>>,
    /// Extra or overriding `KEY=value` entries.
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    /// `uid`, `uid:gid`, `name` or `name:group`; resolved inside the rootfs.
    pub user: Option<String>,
    pub mounts: Vec<BindMount>,
    pub network: Network,
    /// `None` disables the user namespace (needed for host networking, some privileged apps).
    pub userns: Option<IdMap>,
    /// Join this existing user namespace (a sandbox's) instead of creating one; its id
    /// mapping must match `userns` (used for rootfs ownership).
    pub userns_path: Option<PathBuf>,
    pub cap_add: Vec<Capability>,
    pub cap_drop: Vec<Capability>,
    pub read_only_root: bool,
    /// cgroup v2 memory.max (hard cap), memory.high (throttle point), memory.low (protection).
    pub memory_max: Option<i64>,
    pub memory_high: Option<i64>,
    pub memory_low: Option<i64>,
    /// cgroup v2 cpu.weight equivalent (shares 2..262144; Docker's default is 1024).
    pub cpu_shares: Option<u64>,
    pub pids_max: i64,
    pub shm_size: Option<u64>,
    /// cgroup path relative to the cgroup root, e.g. `/og-host/<stack>/<name>`.
    pub cgroup_path: PathBuf,
    pub terminal: bool,
}

impl RunOptions {
    pub fn new(hostname: impl Into<String>, rootfs: impl Into<PathBuf>, cgroup_path: impl Into<PathBuf>) -> Self {
        RunOptions {
            hostname: hostname.into(),
            rootfs: rootfs.into(),
            entrypoint: None,
            command: None,
            env: Vec::new(),
            working_dir: None,
            user: None,
            mounts: Vec::new(),
            network: Network::Isolated,
            userns: Some(IdMap::default()),
            userns_path: None,
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            read_only_root: false,
            memory_max: None,
            memory_high: None,
            memory_low: None,
            cpu_shares: None,
            pids_max: 4096,
            shm_size: None,
            cgroup_path: cgroup_path.into(),
            terminal: false,
        }
    }
}

/// Docker's rule: Entrypoint + Cmd, where a given entrypoint resets the image Cmd.
pub fn resolve_args(image: &ImageConfig, opts: &RunOptions) -> Result<Vec<String>> {
    let entry = opts.entrypoint.clone().or_else(|| image.config.entrypoint.clone()).unwrap_or_default();
    let cmd = match (&opts.command, &opts.entrypoint) {
        (Some(c), _) => c.clone(),
        (None, Some(_)) => Vec::new(),
        (None, None) => image.config.cmd.clone().unwrap_or_default(),
    };
    let args: Vec<String> = entry.into_iter().chain(cmd).collect();
    if args.is_empty() {
        return Err(Error::NoCommand);
    }
    Ok(args)
}

/// Merge env: image env first, then overrides; later keys win; PATH defaulted like Docker.
pub fn resolve_env(image: &ImageConfig, opts: &RunOptions) -> Vec<String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut set = |kv: &str| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        if k.is_empty() {
            return;
        }
        match out.iter_mut().find(|(ek, _)| ek == k) {
            Some(e) => e.1 = v.to_string(),
            None => out.push((k.to_string(), v.to_string())),
        }
    };
    for kv in image.config.env.iter().flatten() {
        set(kv);
    }
    for kv in &opts.env {
        set(kv);
    }
    if !out.iter().any(|(k, _)| k == "PATH") {
        out.insert(0, ("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()));
    }
    if !out.iter().any(|(k, _)| k == "HOSTNAME") {
        out.push(("HOSTNAME".into(), opts.hostname.clone()));
    }
    out.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

fn ns(t: LinuxNamespaceType, path: Option<&Path>) -> Result<LinuxNamespace> {
    let mut b = LinuxNamespaceBuilder::default().typ(t);
    if let Some(p) = path {
        b = b.path(p.to_path_buf());
    }
    Ok(b.build()?)
}

fn mount(dst: &str, typ: &str, src: &str, options: &[&str]) -> Result<Mount> {
    Ok(MountBuilder::default()
        .destination(PathBuf::from(dst))
        .typ(typ.to_string())
        .source(PathBuf::from(src))
        .options(options.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .build()?)
}

/// Build the OCI runtime spec for one container.
pub fn build_spec(image: &ImageConfig, opts: &RunOptions) -> Result<Spec> {
    if matches!(opts.network, Network::Host) && opts.userns.is_some() {
        return Err(Error::Invalid("host networking needs the user namespace disabled".into()));
    }
    let args = resolve_args(image, opts)?;
    let env = resolve_env(image, opts);
    let cwd = opts.working_dir.clone().or_else(|| image.config.working_dir.clone()).filter(|w| !w.is_empty()).unwrap_or_else(|| "/".into());
    if !cwd.starts_with('/') {
        return Err(Error::Invalid(format!("working dir {cwd:?} must be absolute")));
    }
    let user_spec = opts.user.clone().or_else(|| image.config.user.clone()).filter(|u| !u.is_empty());
    let ids = users::resolve(&opts.rootfs, user_spec.as_deref()).map_err(|e| Error::Invalid(e.to_string()))?;

    // capabilities: Docker default + add - drop; the same set for every capability list
    let mut caps: Vec<Capability> = DOCKER_DEFAULT_CAPS.to_vec();
    for c in &opts.cap_add {
        if !caps.contains(c) {
            caps.push(*c);
        }
    }
    caps.retain(|c| !opts.cap_drop.contains(c));
    let capset: std::collections::HashSet<Capability> = caps.iter().copied().collect();
    let capabilities = LinuxCapabilitiesBuilder::default()
        .bounding(capset.clone())
        .effective(capset.clone())
        .permitted(capset.clone())
        .inheritable(std::collections::HashSet::new())
        .ambient(std::collections::HashSet::new())
        .build()?;

    let process = ProcessBuilder::default()
        .terminal(opts.terminal)
        .user(
            UserBuilder::default()
                .uid(ids.uid)
                .gid(ids.gid)
                .additional_gids(ids.additional_gids.clone())
                .build()?,
        )
        .args(args)
        .env(env)
        .cwd(PathBuf::from(&cwd))
        .capabilities(capabilities)
        .no_new_privileges(true)
        .build()?;

    // namespaces
    let mut namespaces = vec![
        ns(LinuxNamespaceType::Pid, None)?,
        ns(LinuxNamespaceType::Ipc, None)?,
        ns(LinuxNamespaceType::Uts, None)?,
        ns(LinuxNamespaceType::Mount, None)?,
        ns(LinuxNamespaceType::Cgroup, None)?,
    ];
    match &opts.network {
        Network::Isolated => namespaces.push(ns(LinuxNamespaceType::Network, None)?),
        Network::Join(p) => namespaces.push(ns(LinuxNamespaceType::Network, Some(p))?),
        Network::Host => {}
    }
    if opts.userns.is_some() {
        namespaces.push(ns(LinuxNamespaceType::User, opts.userns_path.as_deref())?);
    }

    // mounts: the standard container set, then user binds
    let shm = format!("size={}", opts.shm_size.unwrap_or(64 << 20));
    let mut mounts = vec![
        mount("/proc", "proc", "proc", &["nosuid", "noexec", "nodev"])?,
        mount("/dev", "tmpfs", "tmpfs", &["nosuid", "strictatime", "mode=755", "size=65536k"])?,
        mount("/dev/pts", "devpts", "devpts", &["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"])?,
        mount("/dev/shm", "tmpfs", "shm", &["nosuid", "noexec", "nodev", "mode=1777", &shm])?,
        mount("/dev/mqueue", "mqueue", "mqueue", &["nosuid", "noexec", "nodev"])?,
    ];
    // sysfs can't be mounted fresh in the host's network namespace from a user namespace;
    // with host networking (no userns) or isolated netns, a read-only sysfs is fine.
    mounts.push(mount("/sys", "sysfs", "sysfs", &["nosuid", "noexec", "nodev", "ro"])?);
    mounts.push(mount("/sys/fs/cgroup", "cgroup", "cgroup", &["nosuid", "noexec", "nodev", "relatime", "ro"])?);
    for m in &opts.mounts {
        if !m.target.is_absolute() {
            return Err(Error::Invalid(format!("mount target {} must be absolute", m.target.display())));
        }
        let mut o = vec!["rbind", "rprivate"];
        if m.read_only {
            o.push("ro");
        }
        mounts.push(
            MountBuilder::default()
                .destination(m.target.clone())
                .typ("bind".to_string())
                .source(m.source.clone())
                .options(o.into_iter().map(String::from).collect::<Vec<_>>())
                .build()?,
        );
    }

    // resources: cgroup v2 memory knobs go through `unified` so low/high are expressible
    let mut unified = std::collections::HashMap::new();
    if let Some(h) = opts.memory_high {
        unified.insert("memory.high".to_string(), h.to_string());
    }
    if let Some(l) = opts.memory_low {
        unified.insert("memory.low".to_string(), l.to_string());
    }
    let mut res = LinuxResourcesBuilder::default().pids(LinuxPidsBuilder::default().limit(opts.pids_max).build()?);
    if let Some(m) = opts.memory_max {
        res = res.memory(LinuxMemoryBuilder::default().limit(m).build()?);
    }
    if let Some(s) = opts.cpu_shares {
        res = res.cpu(LinuxCpuBuilder::default().shares(s).build()?);
    }
    if !unified.is_empty() {
        res = res.unified(unified);
    }

    let mut linux = LinuxBuilder::default()
        .namespaces(namespaces)
        .resources(res.build()?)
        .cgroups_path(opts.cgroup_path.clone())
        .masked_paths(
            [
                "/proc/acpi", "/proc/asound", "/proc/kcore", "/proc/keys", "/proc/latency_stats", "/proc/timer_list",
                "/proc/timer_stats", "/proc/sched_debug", "/proc/scsi", "/sys/firmware", "/sys/devices/virtual/powercap",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        )
        .readonly_paths(
            ["/proc/bus", "/proc/fs", "/proc/irq", "/proc/sys", "/proc/sysrq-trigger"].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        );
    if let Some(map) = opts.userns.filter(|_| opts.userns_path.is_none()) {
        let m = LinuxIdMappingBuilder::default().container_id(0u32).host_id(map.host_start).size(map.size).build()?;
        linux = linux.uid_mappings(vec![m]).gid_mappings(vec![m]);
    }

    let spec = SpecBuilder::default()
        .version("1.0.2".to_string())
        .root(RootBuilder::default().path(opts.rootfs.clone()).readonly(opts.read_only_root).build()?)
        .hostname(opts.hostname.clone())
        .process(process)
        .mounts(mounts)
        .linux(linux.build()?)
        .build()?;
    Ok(spec)
}

/// Write `<bundle>/config.json`.
pub fn write_bundle(bundle: &Path, spec: &Spec) -> Result<()> {
    std::fs::create_dir_all(bundle)?;
    let tmp = bundle.join("config.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(spec)?)?;
    std::fs::rename(tmp, bundle.join("config.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use og_oci::registry::RunConfig;

    fn img(entry: Option<&[&str]>, cmd: Option<&[&str]>, env: &[&str]) -> ImageConfig {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        ImageConfig {
            config: RunConfig { entrypoint: entry.map(v), cmd: cmd.map(v), env: Some(v(env)), ..Default::default() },
            ..Default::default()
        }
    }

    fn opts() -> RunOptions {
        RunOptions::new("web", "/nonexistent-rootfs", "/og-host/test/web")
    }

    #[test]
    fn docker_entrypoint_cmd_rules() {
        let i = img(Some(&["/docker-entrypoint.sh"]), Some(&["nginx", "-g", "daemon off;"]), &[]);
        assert_eq!(resolve_args(&i, &opts()).unwrap(), ["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]);
        let mut o = opts();
        o.command = Some(vec!["nginx".into(), "-T".into()]);
        assert_eq!(resolve_args(&i, &o).unwrap(), ["/docker-entrypoint.sh", "nginx", "-T"]);
        let mut o = opts();
        o.entrypoint = Some(vec!["/bin/sh".into()]);
        assert_eq!(resolve_args(&i, &o).unwrap(), ["/bin/sh"], "a new entrypoint resets the image Cmd");
        assert!(matches!(resolve_args(&img(None, None, &[]), &opts()), Err(Error::NoCommand)));
    }

    #[test]
    fn env_merge_overrides_and_defaults() {
        let i = img(None, Some(&["x"]), &["A=1", "B=2"]);
        let mut o = opts();
        o.env = vec!["B=3".into(), "C=4".into()];
        let env = resolve_env(&i, &o);
        assert!(env.contains(&"B=3".to_string()) && env.contains(&"C=4".to_string()) && env.contains(&"A=1".to_string()));
        assert!(env.iter().any(|e| e.starts_with("PATH=")));
        assert!(env.contains(&"HOSTNAME=web".to_string()));
        assert_eq!(env.iter().filter(|e| e.starts_with("B=")).count(), 1);
    }

    #[test]
    fn spec_defaults_are_locked_down() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts();
        o.rootfs = dir.path().to_path_buf();
        let s = build_spec(&img(None, Some(&["sleep", "1"]), &[]), &o).unwrap();
        let p = s.process().as_ref().unwrap();
        assert_eq!(p.no_new_privileges(), Some(true));
        let caps = p.capabilities().as_ref().unwrap().effective().as_ref().unwrap();
        assert!(!caps.contains(&Capability::SysAdmin) && !caps.contains(&Capability::NetAdmin));
        let linux = s.linux().as_ref().unwrap();
        let types: Vec<_> = linux.namespaces().as_ref().unwrap().iter().map(|n| n.typ()).collect();
        assert!(types.contains(&LinuxNamespaceType::User) && types.contains(&LinuxNamespaceType::Network));
        assert_eq!(linux.uid_mappings().as_ref().unwrap()[0].host_id(), 100_000);
    }

    #[test]
    fn cap_add_and_host_network_rules() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts();
        o.rootfs = dir.path().to_path_buf();
        o.cap_add = vec![Capability::NetAdmin];
        o.cap_drop = vec![Capability::NetRaw];
        let s = build_spec(&img(None, Some(&["x"]), &[]), &o).unwrap();
        let caps = s.process().as_ref().unwrap().capabilities().as_ref().unwrap().bounding().clone().unwrap();
        assert!(caps.contains(&Capability::NetAdmin) && !caps.contains(&Capability::NetRaw));
        o.network = Network::Host;
        assert!(build_spec(&img(None, Some(&["x"]), &[]), &o).is_err(), "host network + userns must be refused");
        o.userns = None;
        let s = build_spec(&img(None, Some(&["x"]), &[]), &o).unwrap();
        let types: Vec<_> = s.linux().as_ref().unwrap().namespaces().as_ref().unwrap().iter().map(|n| n.typ()).collect();
        assert!(!types.contains(&LinuxNamespaceType::Network) && !types.contains(&LinuxNamespaceType::User));
    }
}
