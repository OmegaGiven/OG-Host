//! Starting and stopping VMs: network devices, the TPM emulator, PCI passthrough (vfio),
//! the VM's cgroup (og-host/vm-<name>: memory and CPU caps), then QEMU; and tidying up
//! after a VM ends (also when its guest powered it off). VMs with `autostart`, or that
//! were running when the server went down, start again with og-hostd.

use std::os::fd::IntoRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::serve::Daemon;
use crate::vms::{NetDev, Vm};

/// og-net owner of a VM's n-th NIC.
pub fn nic_owner(name: &str, i: usize) -> String {
    let o = crate::vms::owner(name);
    format!("{}{i}", &o[..9])
}

fn cgroup(name: &str) -> PathBuf {
    PathBuf::from("/sys/fs/cgroup/og-host").join(format!("vm-{name}"))
}

fn setup_cgroup(vm: &Vm) -> Result<PathBuf> {
    let parent = Path::new("/sys/fs/cgroup/og-host");
    std::fs::create_dir_all(parent)?;
    let _ = std::fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+cpu +memory +io +pids");
    let _ = std::fs::write(parent.join("cgroup.subtree_control"), "+cpu +memory +io +pids");
    let cg = cgroup(&vm.name);
    std::fs::create_dir_all(&cg)?;
    // guest RAM plus QEMU's own overhead (device emulation, page tables, video memory)
    let max = (vm.memory + (vm.memory / 8).max(384)) << 20;
    let _ = std::fs::write(cg.join("memory.max"), max.to_string());
    let _ = std::fs::write(cg.join("memory.swap.max"), "0");
    let cpu = if vm.cpu_limit > 0.0 { format!("{} 100000", (vm.cpu_limit * 100000.0) as u64) } else { "max 100000".into() };
    let _ = std::fs::write(cg.join("cpu.max"), cpu);
    Ok(cg)
}

fn bind_vfio(addr: &str) -> Result<()> {
    let dev = Path::new("/sys/bus/pci/devices").join(addr);
    let _ = Command::new("modprobe").arg("vfio-pci").status();
    std::fs::write(dev.join("driver_override"), "vfio-pci").context("setting driver_override")?;
    if let Ok(drv) = std::fs::read_link(dev.join("driver")) {
        if drv.file_name().is_some_and(|n| n == "vfio-pci") {
            return Ok(());
        }
        std::fs::write(dev.join("driver/unbind"), addr).with_context(|| format!("unbinding {addr} from its driver (in use by the host?)"))?;
    }
    std::fs::write("/sys/bus/pci/drivers_probe", addr)?;
    if !std::fs::read_link(dev.join("driver")).is_ok_and(|d| d.file_name().is_some_and(|n| n == "vfio-pci")) {
        bail!("{addr} didn't bind to vfio-pci (is the IOMMU on? intel_iommu=on / amd_iommu=on)");
    }
    Ok(())
}

fn release_vfio(addr: &str) {
    let dev = Path::new("/sys/bus/pci/devices").join(addr);
    let _ = std::fs::write(dev.join("driver_override"), "\n");
    if std::fs::read_link(dev.join("driver")).is_ok_and(|d| d.file_name().is_some_and(|n| n == "vfio-pci")) {
        let _ = std::fs::write(dev.join("driver/unbind"), addr);
    }
    let _ = std::fs::write("/sys/bus/pci/drivers_probe", addr);
}

pub fn start(d: &Daemon, name: &str) -> Result<()> {
    start_with(d, name, false)
}

/// `incoming`: start paused, waiting for a migration (QMP migrate-incoming) or a disk copy
/// over NBD; the desired state is left alone.
pub fn start_with(d: &Daemon, name: &str, incoming: bool) -> Result<()> {
    let vm = d.vms.get(name)?;
    if vm.template {
        bail!("{name} is a template: clone it to make a VM");
    }
    if d.vms.status(name).state != "stopped" {
        return Ok(());
    }
    d.vms.cloudinit_seed(&vm)?;
    // a GPU containers are using: free it (og.gpu-preempt=true) or refuse
    let conflicts = crate::vmbrain::gpu_conflicts(d, &vm);
    if !conflicts.is_empty() {
        if vm.labels.get("og.gpu-preempt").map(String::as_str) == Some("true") {
            for (gpu, users) in &conflicts {
                for c in users {
                    if let Ok(m) = d.node.find(c) {
                        let _ = d.node.stop(&m.id, 30);
                        d.brain.lock().unwrap().note(format!("stopped {c}: VM {name} takes GPU {gpu}"));
                    }
                }
            }
        } else {
            let list: Vec<String> = conflicts.iter().map(|(g, u)| format!("{g} (used by {})", u.join(", "))).collect();
            bail!("GPU in use by containers: {}. Stop them, or label the VM og.gpu-preempt=true", list.join("; "));
        }
    }
    let qemu = crate::vms::qemu_binary().context("QEMU isn't installed (qemu-system-x86)")?;
    if !Path::new("/dev/kvm").exists() {
        bail!("this server has no /dev/kvm (virtualization off in the BIOS, or a VM without nested virtualization)");
    }
    let run = PathBuf::from("/run/og-host/vms").join(name);
    let _ = std::fs::remove_dir_all(&run);
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&run)?;
    }
    // network devices
    let mut devs = vec![];
    let mut fds = vec![];
    for (i, nic) in vm.nics.iter().enumerate() {
        let owner = nic_owner(name, i);
        if let Some(br) = nic.network.strip_prefix("bridge:") {
            let tap = og_net::tap_name(&owner);
            crate::hostnet::attach_tap(&tap, br, nic.vlan)?;
            devs.push((nic.id.clone(), NetDev::Tap(tap)));
        } else if let Some(parent) = nic.network.strip_prefix("lan:") {
            let path = d.node.net.attach_macvtap(&owner, parent, &nic.mac)?;
            let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
            let target = 10 + fds.len() as i32;
            fds.push((f.into_raw_fd(), target));
            devs.push((nic.id.clone(), NetDev::Fd(target)));
        } else {
            let ports: Vec<og_net::PortMap> = nic.ports.iter().filter_map(|p| og_net::PortMap::parse(p).ok()).collect();
            let a = d.node.net.attach_tap(&owner, &nic.network, &ports)?;
            let net = d.node.net.get(&nic.network)?.context("network vanished")?;
            if let (Some(ip), Some(gw), Some(prefix)) = (a.ip, a.gateway, a.prefix) {
                d.dhcp.set(&nic.mac, crate::dhcp::Lease { ip, prefix, gateway: gw, dns: og_net::upstream_dns(), hostname: vm.name.clone(), bridge: net.bridge.clone() });
            }
            devs.push((nic.id.clone(), NetDev::Tap(a.host_veth.unwrap_or_default())));
        }
    }
    let r = (|| -> Result<()> {
        for p in &vm.hostpci {
            bind_vfio(p)?;
        }
        if vm.tpm {
            let state = d.vms.dir(name).join("tpm");
            std::fs::create_dir_all(&state)?;
            let out = Command::new("swtpm")
                .args(["socket", "--tpm2", "--daemon", "--terminate"])
                .args(["--tpmstate", &format!("dir={}", state.display())])
                .args(["--ctrl", &format!("type=unixio,path={}", run.join("swtpm.sock").display())])
                .args(["--pid", &format!("file={}", run.join("swtpm.pid").display())])
                .args(["--log", &format!("file={},level=1", d.vms.dir(name).join("swtpm.log").display())])
                .output()
                .context("running swtpm (is swtpm installed?)")?;
            if !out.status.success() {
                bail!("swtpm: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            let t = Instant::now();
            while !run.join("swtpm.sock").exists() && t.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let cg = setup_cgroup(&vm)?;
        let mut args = d.vms.command(&vm, &devs)?;
        if incoming {
            args.extend(["-incoming".to_string(), "defer".to_string()]);
        }
        let mut c = Command::new(&qemu);
        c.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let procs = std::ffi::CString::new(cg.join("cgroup.procs").to_string_lossy().as_bytes())?;
        let fds2 = fds.clone();
        unsafe {
            use std::os::unix::process::CommandExt;
            c.pre_exec(move || {
                // join the VM's cgroup before exec (the daemonized QEMU inherits it); only
                // async-signal-safe calls here: "0" moves the writing process itself
                let fd = libc::open(procs.as_ptr(), libc::O_WRONLY);
                if fd >= 0 {
                    libc::write(fd, b"0".as_ptr() as *const libc::c_void, 1);
                    libc::close(fd);
                }
                for (fd, target) in &fds2 {
                    if libc::dup2(*fd, *target) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let out = c.output().context("starting QEMU")?;
        if !out.status.success() {
            bail!("QEMU: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    })();
    for (fd, _) in &fds {
        unsafe { libc::close(*fd) };
    }
    if let Err(e) = r {
        cleanup(d, &vm);
        return Err(e);
    }
    if !incoming {
        d.vms.set_desired(name, "running")?;
    }
    refresh_firewall(d);
    Ok(())
}

/// Rebuild the VM firewall table from the running VMs.
pub fn refresh_firewall(d: &Daemon) {
    let mut v = vec![];
    for vm in d.vms.list() {
        if d.vms.status(&vm.name).state == "stopped" {
            continue;
        }
        for (i, n) in vm.nics.iter().enumerate() {
            if !n.network.starts_with("lan:") {
                v.push((og_net::tap_name(&nic_owner(&vm.name, i)), vm.firewall.clone()));
            }
        }
    }
    if let Err(e) = crate::vmfw::apply(&v) {
        eprintln!("og-hostd: {e:#}");
    }
}

/// After QEMU is gone: stop swtpm, give PCI devices back, drop the cgroup and DHCP leases.
pub fn cleanup(d: &Daemon, vm: &Vm) {
    let run = PathBuf::from("/run/og-host/vms").join(&vm.name);
    if let Ok(pid) = std::fs::read_to_string(run.join("swtpm.pid")).map(|s| s.trim().parse::<i32>().unwrap_or(0)) {
        if pid > 0 {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
    for p in &vm.hostpci {
        release_vfio(p);
    }
    for (i, nic) in vm.nics.iter().enumerate() {
        if nic.network.starts_with("lan:") || nic.network.starts_with("bridge:") {
            let _ = d.node.net.detach(&nic_owner(&vm.name, i));
        }
        d.dhcp.remove(&nic.mac);
    }
    refresh_firewall(d);
    let _ = std::fs::remove_dir(cgroup(&vm.name));
    let _ = std::fs::remove_file(run.join("qemu.pid"));
}

fn wait_stopped(d: &Daemon, name: &str, secs: u64) -> bool {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if d.vms.status(name).state == "stopped" {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Power off: ACPI shutdown first (`timeout` s), then pull the plug. `force` pulls it at once.
pub fn stop(d: &Daemon, name: &str, force: bool, timeout: u64) -> Result<()> {
    let vm = d.vms.get(name)?;
    d.vms.set_desired(name, "stopped")?;
    if d.vms.status(name).state != "stopped" {
        if !force {
            let _ = d.vms.qmp(name, "system_powerdown", json!({}));
            if wait_stopped(d, name, timeout) {
                cleanup(d, &vm);
                return Ok(());
            }
        }
        let _ = d.vms.qmp(name, "quit", json!({}));
        if !wait_stopped(d, name, 10) {
            if let Some(pid) = d.vms.status(name).pid {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            wait_stopped(d, name, 5);
        }
    }
    cleanup(d, &vm);
    Ok(())
}

pub fn action(d: &Daemon, name: &str, act: &str) -> Result<()> {
    match act {
        "start" => start(d, name),
        "shutdown" => stop(d, name, false, 120),
        "stop" => stop(d, name, true, 0),
        "reboot" => {
            stop(d, name, false, 120)?;
            start(d, name)
        }
        "reset" => d.vms.qmp(name, "system_reset", json!({})).map(|_| ()),
        "pause" => d.vms.qmp(name, "stop", json!({})).map(|_| ()),
        "resume" => d.vms.qmp(name, "cont", json!({})).map(|_| ()),
        _ => bail!("unknown action {act:?} (start, shutdown, stop, reboot, reset, pause, resume)"),
    }
}

/// Every few seconds: tidy up VMs that ended on their own; at daemon start, start the ones
/// that should run (autostart, or running when the server went down).
pub async fn supervisor(d: std::sync::Arc<Daemon>) {
    let mut first = true;
    loop {
        let d2 = d.clone();
        let boot = first;
        let _ = tokio::task::spawn_blocking(move || {
            for vm in d2.vms.list() {
                let st = d2.vms.status(&vm.name).state;
                let run = PathBuf::from("/run/og-host/vms").join(&vm.name);
                if st == "stopped" && run.join("qemu.pid").exists() {
                    // guest powered off, or QEMU died
                    eprintln!("og-hostd: VM {} stopped", vm.name);
                    cleanup(&d2, &vm);
                    let _ = d2.vms.set_desired(&vm.name, "stopped");
                } else if st == "stopped" && boot && (vm.autostart || vm.desired == "running") {
                    match start(&d2, &vm.name) {
                        Ok(()) => eprintln!("og-hostd: VM {} started", vm.name),
                        Err(e) => eprintln!("og-hostd: VM {} didn't start: {e:#}", vm.name),
                    }
                } else if st != "stopped" && boot {
                    refresh_firewall(&d2);
                    // og-hostd restarted while it ran: give the DHCP server its leases back
                    for (i, nic) in vm.nics.iter().enumerate().filter(|(_, n)| !n.network.starts_with("lan:")) {
                        let ports: Vec<og_net::PortMap> = nic.ports.iter().filter_map(|p| og_net::PortMap::parse(p).ok()).collect();
                        if let (Ok(a), Ok(Some(net))) = (d2.node.net.attach_tap(&nic_owner(&vm.name, i), &nic.network, &ports), d2.node.net.get(&nic.network)) {
                            if let (Some(ip), Some(gw), Some(prefix)) = (a.ip, a.gateway, a.prefix) {
                                d2.dhcp.set(&nic.mac, crate::dhcp::Lease { ip, prefix, gateway: gw, dns: og_net::upstream_dns(), hostname: vm.name.clone(), bridge: net.bridge });
                            }
                        }
                    }
                }
            }
        })
        .await;
        first = false;
        let d3 = d.clone();
        let _ = tokio::task::spawn_blocking(move || crate::vmbrain::tick(&d3)).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

/// CPU and memory use of a running VM's QEMU process: (cpu seconds, rss bytes).
pub fn usage(pid: i32) -> Option<(f64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(") ")?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let cpu = (f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / ticks;
    let rss = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?.lines().find_map(|l| l.strip_prefix("VmRSS:").and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok()))? * 1024;
    Some((cpu, rss))
}
