//! og-runtime: turn an image config + run options into an OCI runtime bundle.
//!
//! Defaults are chosen for safety and Docker compatibility:
//! - user namespace on by default: container root maps to an unprivileged host range;
//! - Docker's default capability set (not more), `no_new_privileges`;
//! - pid limit, masked/readonly kernel paths as in runc/Docker;
//! - memory/CPU controls exposed as cgroup v2 knobs (`memory.low/high/max`, `cpu.weight`)
//!   so OG-Host's resource brain can adjust them live.

pub mod cdi;
pub mod seccomp;
pub mod users;

pub use oci_spec::runtime::Capability;

/// Disk I/O cap on one block device (a partition or a path on it means its whole disk).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IoLimit {
    pub device: PathBuf,
    pub read_bps: Option<u64>,
    pub write_bps: Option<u64>,
    pub read_iops: Option<u64>,
    pub write_iops: Option<u64>,
}

impl IoLimit {
    /// `MAJ:MIN` of the whole disk behind `device`.
    pub fn disk(&self) -> Result<(u32, u32)> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let md = std::fs::metadata(&self.device).map_err(|e| Error::Invalid(format!("{}: {e}", self.device.display())))?;
        let dev = if md.file_type().is_block_device() { md.rdev() } else { md.dev() };
        let (mut maj, mut min) = (libc_major(dev) as u32, libc_minor(dev) as u32);
        // a partition: use its parent disk
        let sys = PathBuf::from(format!("/sys/dev/block/{maj}:{min}"));
        if sys.join("partition").exists() {
            if let Ok(d) = std::fs::read_to_string(sys.join("../dev")) {
                if let Some((a, b)) = d.trim().split_once(':') {
                    maj = a.parse().unwrap_or(maj);
                    min = b.parse().unwrap_or(min);
                }
            }
        }
        Ok((maj, min))
    }

    /// The line for cgroup v2 io.max.
    pub fn io_max_line(&self) -> Result<String> {
        let (maj, min) = self.disk()?;
        let v = |o: Option<u64>| o.map_or("max".to_string(), |x| x.to_string());
        Ok(format!("{maj}:{min} rbps={} wbps={} riops={} wiops={}", v(self.read_bps), v(self.write_bps), v(self.read_iops), v(self.write_iops)))
    }
}

/// Paths hidden from the container. The desktop profile leaves /proc alone: the kernel only
/// lets a nested user namespace mount a fresh procfs (bubblewrap, Steam's pressure-vessel) when
/// no part of the existing /proc is covered. The user namespace still keeps the sensitive files
/// (kcore, sysrq-trigger, /proc/sys) out of reach: their checks are against the host's root.
fn masked_paths(desktop: bool) -> Vec<String> {
    let proc = ["/proc/acpi", "/proc/asound", "/proc/kcore", "/proc/keys", "/proc/latency_stats", "/proc/timer_list", "/proc/timer_stats", "/proc/sched_debug", "/proc/scsi"];
    let sys = ["/sys/firmware", "/sys/devices/virtual/powercap"];
    let mut v: Vec<String> = sys.iter().map(|s| s.to_string()).collect();
    if !desktop {
        v.extend(proc.iter().map(|s| s.to_string()));
    }
    v
}

fn readonly_paths(desktop: bool) -> Vec<String> {
    if desktop {
        return vec![];
    }
    ["/proc/bus", "/proc/fs", "/proc/irq", "/proc/sys", "/proc/sysrq-trigger"].iter().map(|s| s.to_string()).collect()
}



use std::path::{Path, PathBuf};

use og_oci::registry::ImageConfig;
use oci_spec::runtime::{
    LinuxBuilder, LinuxCapabilitiesBuilder, LinuxDeviceBuilder, LinuxDeviceCgroup, LinuxDeviceCgroupBuilder,
    LinuxDeviceType, HookBuilder, HooksBuilder, LinuxIdMappingBuilder, LinuxMemoryBuilder, LinuxNamespace,
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
    /// tmpfs mounts (target, size in bytes, extra mount options such as `mode=0700`, `noexec`).
    pub tmpfs: Vec<(PathBuf, Option<u64>, Vec<String>)>,
    /// Host device nodes passed through at the same path (e.g. /dev/net/tun).
    pub devices: Vec<PathBuf>,
    /// CDI devices, e.g. `nvidia.com/gpu=all` (resolved from /etc/cdi, /var/run/cdi).
    pub cdi_devices: Vec<String>,
    pub network: Network,
    /// `None` disables the user namespace (needed for host networking, some privileged apps).
    pub userns: Option<IdMap>,
    /// Join this existing user namespace (a sandbox's) instead of creating one; its id
    /// mapping must match `userns` (used for rootfs ownership).
    pub userns_path: Option<PathBuf>,
    pub cap_add: Vec<Capability>,
    pub cap_drop: Vec<Capability>,
    /// Start from no capabilities at all (`cap_drop: [ALL]`); `cap_add` still applies.
    pub cap_drop_all: bool,
    /// A seccomp profile in Docker's JSON format instead of the default one.
    pub seccomp_profile: Option<serde_json::Value>,
    /// Run without the default seccomp profile (Docker's `--security-opt seccomp=unconfined`).
    pub seccomp_unconfined: bool,
    pub read_only_root: bool,
    /// cgroup v2 memory.max (hard cap), memory.high (throttle point), memory.low (protection).
    pub memory_max: Option<i64>,
    pub memory_high: Option<i64>,
    pub memory_low: Option<i64>,
    /// cgroup v2 cpu.weight equivalent (shares 2..262144; Docker's default is 1024).
    pub cpu_shares: Option<u64>,
    /// CPU quota per period in microseconds (`cpus: 1.5` = 150000 per 100000).
    pub cpu_quota: Option<(i64, u64)>,
    /// CPU / memory-node pinning, e.g. "0-3".
    pub cpuset_cpus: Option<String>,
    pub cpuset_mems: Option<String>,
    /// io.weight, 1..=10000 (default 100).
    pub io_weight: Option<u16>,
    pub io_limits: Vec<IoLimit>,
    /// Desktop profile (DESIGN.md §4.9): /proc left fully visible and nested namespaces
    /// allowed, so bubblewrap / Steam's pressure-vessel work inside.
    pub desktop: bool,
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
            tmpfs: Vec::new(),
            devices: Vec::new(),
            cdi_devices: Vec::new(),
            network: Network::Isolated,
            userns: Some(IdMap::default()),
            userns_path: None,
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            cap_drop_all: false,
            seccomp_profile: None,
            seccomp_unconfined: false,
            read_only_root: false,
            memory_max: None,
            memory_high: None,
            memory_low: None,
            cpu_shares: None,
            cpu_quota: None,
            cpuset_cpus: None,
            cpuset_mems: None,
            io_weight: None,
            io_limits: Vec::new(),
            desktop: false,
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
    // CDI devices (GPUs): fold their edits into the options before anything else uses them
    let cdi = if opts.cdi_devices.is_empty() { None } else { Some(cdi::Registry::load().resolve(&opts.cdi_devices)?) };
    let mut opts = opts.clone();
    if let Some(e) = &cdi {
        opts.devices.extend(e.device_paths());
        opts.env.extend(e.env.iter().cloned());
    }
    let opts = &opts;
    let args = resolve_args(image, opts)?;
    let env = resolve_env(image, opts);
    let cwd = opts.working_dir.clone().or_else(|| image.config.working_dir.clone()).filter(|w| !w.is_empty()).unwrap_or_else(|| "/".into());
    if !cwd.starts_with('/') {
        return Err(Error::Invalid(format!("working dir {cwd:?} must be absolute")));
    }
    let user_spec = opts.user.clone().or_else(|| image.config.user.clone()).filter(|u| !u.is_empty());
    let ids = users::resolve(&opts.rootfs, user_spec.as_deref()).map_err(|e| Error::Invalid(e.to_string()))?;

    // capabilities: Docker default + add - drop; the same set for every capability list
    let mut caps: Vec<Capability> = if opts.cap_drop_all { Vec::new() } else { DOCKER_DEFAULT_CAPS.to_vec() };
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

    if let Some(e) = &cdi {
        for m in &e.mounts {
            if !Path::new(&m.host_path).exists() {
                continue; // stale spec entry (driver updated): skip rather than fail
            }
            let mut o: Vec<String> = m.options.clone();
            if !o.iter().any(|x| x == "bind" || x == "rbind") {
                o.push("bind".into());
            }
            mounts.push(
                MountBuilder::default()
                    .destination(PathBuf::from(&m.container_path))
                    .typ("bind".to_string())
                    .source(PathBuf::from(&m.host_path))
                    .options(o)
                    .build()?,
            );
        }
    }
    for (target, size, extra) in &opts.tmpfs {
        if !target.is_absolute() {
            return Err(Error::Invalid(format!("tmpfs target {} must be absolute", target.display())));
        }
        let mut o = vec!["nosuid".to_string(), "nodev".to_string(), "mode=1777".to_string()];
        if let Some(sz) = size {
            o.push(format!("size={sz}"));
        }
        for x in extra {
            let key = x.split('=').next().unwrap_or(x);
            if !matches!(key, "mode" | "uid" | "gid" | "nr_inodes" | "noexec" | "exec" | "nosuid" | "suid" | "nodev" | "dev" | "ro" | "rw" | "noatime" | "relatime" | "strictatime" | "size") {
                return Err(Error::Invalid(format!("tmpfs {}: option {x:?} isn't supported", target.display())));
            }
            // a later option wins over the default with the same key (mode=), or its opposite (exec)
            let opposite = |a: &str| match a { "exec" => Some("noexec"), "noexec" => Some("exec"), "suid" => Some("nosuid"), "nosuid" => Some("suid"), "dev" => Some("nodev"), "nodev" => Some("dev"), "ro" => Some("rw"), "rw" => Some("ro"), _ => None };
            o.retain(|y| y.split('=').next() != Some(key) && Some(y.as_str()) != opposite(key));
            o.push(x.clone());
        }
        mounts.push(MountBuilder::default().destination(target.clone()).typ("tmpfs".to_string()).source(PathBuf::from("tmpfs")).options(o).build()?);
    }

    // devices: deny everything, allow the standard set (as runc/Docker do), plus passthroughs.
    // Without this, a container lacking a user namespace could mknod e.g. a raw disk.
    let mut device_rules = vec![LinuxDeviceCgroupBuilder::default().allow(false).access("rwm".to_string()).build()?];
    let allow = |typ: LinuxDeviceType, major: Option<i64>, minor: Option<i64>, access: &str| -> Result<LinuxDeviceCgroup> {
        let mut b = LinuxDeviceCgroupBuilder::default().allow(true).typ(typ).access(access.to_string());
        if let Some(m) = major {
            b = b.major(m);
        }
        if let Some(m) = minor {
            b = b.minor(m);
        }
        Ok(b.build()?)
    };
    for (maj, min) in [(1, 3), (1, 5), (1, 7), (1, 8), (1, 9), (5, 0), (5, 2)] {
        device_rules.push(allow(LinuxDeviceType::C, Some(maj), Some(min), "rwm")?);
    }
    device_rules.push(allow(LinuxDeviceType::C, Some(136), None, "rwm")?); // /dev/pts/*
    let mut linux_devices = Vec::new();
    for dev in &opts.devices {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let md = std::fs::metadata(dev).map_err(|e| Error::Invalid(format!("device {}: {e}", dev.display())))?;
        let ft = md.file_type();
        let typ = if ft.is_char_device() {
            LinuxDeviceType::C
        } else if ft.is_block_device() {
            LinuxDeviceType::B
        } else {
            return Err(Error::Invalid(format!("{} is not a device node", dev.display())));
        };
        let rdev = md.rdev();
        let (major, minor) = (libc_major(rdev), libc_minor(rdev));
        linux_devices.push(
            LinuxDeviceBuilder::default()
                .path(dev.clone())
                .typ(typ)
                .major(major)
                .minor(minor)
                .file_mode(md.mode() & 0o777)
                .uid(0u32)
                .gid(0u32)
                .build()?,
        );
        device_rules.push(allow(typ, Some(major), Some(minor), "rwm")?);
    }

    // resources: cgroup v2 memory knobs go through `unified` so low/high are expressible
    let mut unified = std::collections::HashMap::new();
    if let Some(h) = opts.memory_high {
        unified.insert("memory.high".to_string(), h.to_string());
    }
    if let Some(l) = opts.memory_low {
        unified.insert("memory.low".to_string(), l.to_string());
    }
    let mut res = LinuxResourcesBuilder::default().pids(LinuxPidsBuilder::default().limit(opts.pids_max).build()?).devices(device_rules);
    if let Some(m) = opts.memory_max {
        res = res.memory(LinuxMemoryBuilder::default().limit(m).build()?);
    }
    if opts.cpu_shares.is_some() || opts.cpu_quota.is_some() || opts.cpuset_cpus.is_some() || opts.cpuset_mems.is_some() {
        let mut cpu = LinuxCpuBuilder::default();
        if let Some(s) = opts.cpu_shares {
            cpu = cpu.shares(s);
        }
        if let Some((q, p)) = opts.cpu_quota {
            cpu = cpu.quota(q).period(p);
        }
        if let Some(c) = &opts.cpuset_cpus {
            cpu = cpu.cpus(c.clone());
        }
        if let Some(m) = &opts.cpuset_mems {
            cpu = cpu.mems(m.clone());
        }
        res = res.cpu(cpu.build()?);
    }
    if let Some(w) = opts.io_weight {
        unified.insert("io.weight".to_string(), format!("default {}", w.clamp(1, 10000)));
    }
    if !opts.io_limits.is_empty() {
        use oci_spec::runtime::{LinuxBlockIoBuilder, LinuxThrottleDevice, LinuxThrottleDeviceBuilder};
        let (mut rb, mut wb, mut ri, mut wi) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let dev = |maj: u32, min: u32, rate: u64| -> Result<LinuxThrottleDevice> {
            Ok(LinuxThrottleDeviceBuilder::default().major(maj as i64).minor(min as i64).rate(rate).build()?)
        };
        for l in &opts.io_limits {
            let (maj, min) = l.disk()?;
            if let Some(r) = l.read_bps { rb.push(dev(maj, min, r)?); }
            if let Some(r) = l.write_bps { wb.push(dev(maj, min, r)?); }
            if let Some(r) = l.read_iops { ri.push(dev(maj, min, r)?); }
            if let Some(r) = l.write_iops { wi.push(dev(maj, min, r)?); }
        }
        res = res.block_io(
            LinuxBlockIoBuilder::default()
                .throttle_read_bps_device(rb)
                .throttle_write_bps_device(wb)
                .throttle_read_iops_device(ri)
                .throttle_write_iops_device(wi)
                .build()?,
        );
    }
    if !unified.is_empty() {
        res = res.unified(unified);
    }

    let mut linux = LinuxBuilder::default()
        .namespaces(namespaces)
        .devices(linux_devices)
        .resources(res.build()?)
        .cgroups_path(opts.cgroup_path.clone())
        .masked_paths(masked_paths(opts.desktop))
        .readonly_paths(readonly_paths(opts.desktop));
    if let Some(map) = opts.userns.filter(|_| opts.userns_path.is_none()) {
        let m = LinuxIdMappingBuilder::default().container_id(0u32).host_id(map.host_start).size(map.size).build()?;
        linux = linux.uid_mappings(vec![m]).gid_mappings(vec![m]);
    }
    if !opts.seccomp_unconfined {
        let profile = match &opts.seccomp_profile {
            Some(p) => seccomp::from_docker(p, &caps).map_err(|e| Error::Invalid(format!("seccomp profile: {e}")))?,
            None => seccomp::default_profile(&caps, opts.desktop),
        };
        linux = linux.seccomp(profile);
    }

    // CDI hooks (e.g. nvidia-cdi-hook update-ldcache / create-symlinks)
    let mut hooks = std::collections::BTreeMap::<String, Vec<oci_spec::runtime::Hook>>::new();
    if let Some(e) = &cdi {
        for h in &e.hooks {
            let mut hb = HookBuilder::default().path(PathBuf::from(&h.path)).args(h.args.clone());
            if !h.env.is_empty() {
                hb = hb.env(h.env.clone());
            }
            hooks.entry(h.hook_name.clone()).or_default().push(hb.build()?);
        }
    }
    let mut hb = HooksBuilder::default();
    for (name, list) in hooks {
        hb = match name.as_str() {
            "createRuntime" => hb.create_runtime(list),
            "createContainer" => hb.create_container(list),
            "startContainer" => hb.start_container(list),
            "prestart" => hb.prestart(list),
            "poststart" => hb.poststart(list),
            "poststop" => hb.poststop(list),
            _ => hb,
        };
    }

    let spec = SpecBuilder::default()
        .version("1.0.2".to_string())
        .hooks(hb.build()?)
        .root(RootBuilder::default().path(opts.rootfs.clone()).readonly(opts.read_only_root).build()?)
        .hostname(opts.hostname.clone())
        .process(process)
        .mounts(mounts)
        .linux(linux.build()?)
        .build()?;
    Ok(spec)
}

// glibc's major()/minor() encoding for dev_t
fn libc_major(dev: u64) -> i64 {
    (((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0x0000_0fff)) as i64
}
fn libc_minor(dev: u64) -> i64 {
    (((dev >> 12) & 0xffff_ff00) | (dev & 0x0000_00ff)) as i64
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
    fn devices_default_deny_and_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts();
        o.rootfs = dir.path().to_path_buf();
        o.devices = vec![PathBuf::from("/dev/null")];
        let s = build_spec(&img(None, Some(&["x"]), &[]), &o).unwrap();
        let rules = s.linux().as_ref().unwrap().resources().as_ref().unwrap().devices().clone().unwrap();
        assert!(!rules[0].allow(), "first rule denies everything");
        let devs = s.linux().as_ref().unwrap().devices().clone().unwrap();
        assert_eq!((devs[0].major(), devs[0].minor()), (1, 3), "/dev/null is 1:3");
        o.devices = vec![PathBuf::from("/etc/hostname")];
        assert!(build_spec(&img(None, Some(&["x"]), &[]), &o).is_err(), "regular files are not devices");
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
