//! The OG-Host installer: runs on the install ISO's console (tty1), or unattended from an
//! answers file (`key = value` lines): `og.auto=<path>` on the kernel command line, or
//! `og.auto=label` for `og-auto.cfg` on a drive labelled OG-AUTO (a USB stick).
//!
//! Disk layout (GPT, like Proxmox's): 1 MiB BIOS boot, 512 MiB EFI system partition, the
//! system (ext4 or xfs, `root_gb`), and the rest as LVM with a thin pool `og/data` that
//! becomes the `local-thin` storage pool for VM disks (or the system takes the whole disk).
//! The system is the ISO's own image (squashfs) copied over; then hostname, network,
//! timezone, passwords, the first OG-Host account, fresh machine id and SSH keys, and GRUB
//! for UEFI (also at the removable path) and BIOS.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

const TARGET: &str = "/mnt/og-target";
const SQUASH_CD: &str = "/run/live/medium/live/filesystem.squashfs";

/// The system image: on the install medium, or wherever live-boot put one it fetched over
/// the network (`fetch=` on the kernel command line, for remote installs).
fn squash() -> String {
    if Path::new(SQUASH_CD).exists() {
        return SQUASH_CD.into();
    }
    let out = Command::new("find").args(["/run/live", "-maxdepth", "4", "-name", "filesystem.squashfs"]).output().ok();
    out.and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(String::from)).unwrap_or_else(|| SQUASH_CD.into())
}

#[derive(Debug, Clone)]
struct Answers {
    disk: String,
    fs: String,
    root_gb: u64,
    thin: bool,
    hostname: String,
    net: String,
    iface: String,
    address: String,
    gateway: String,
    dns: Vec<String>,
    timezone: String,
    admin_user: String,
    admin_password: String,
    root_password: String,
    ssh_key: String,
    /// extra kernel options for the installed system (e.g. memtest=4)
    kernel_args: String,
    reboot: bool,
}

fn sh(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).output().with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn chroot(args: &[&str]) -> Result<String> {
    let mut a = vec![TARGET];
    a.extend_from_slice(args);
    sh("chroot", &a)
}

fn chroot_input(args: &[&str], input: &str) -> Result<()> {
    let mut c = Command::new("chroot");
    c.arg(TARGET).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut ch = c.spawn()?;
    ch.stdin.take().unwrap().write_all(input.as_bytes())?;
    let out = ch.wait_with_output()?;
    if !out.status.success() {
        bail!("{}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Disks to install on: (path, bytes, description), not the install medium, at least 16 GB.
fn disks() -> Vec<(String, u64, String)> {
    let medium = std::fs::read_to_string("/proc/mounts").unwrap_or_default().lines().find(|l| l.split_whitespace().nth(1) == Some("/run/live/medium")).and_then(|l| l.split_whitespace().next().map(String::from)).unwrap_or_default();
    let v: Value = serde_json::from_str(&sh("lsblk", &["-dbJ", "-o", "NAME,PATH,SIZE,MODEL,TYPE,RO,TRAN"]).unwrap_or_default()).unwrap_or_default();
    v["blockdevices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["type"] == "disk" && d["ro"] != true && d["ro"] != "1")
        .filter_map(|d| {
            let path = d["path"].as_str()?.to_string();
            let size = d["size"].as_u64().or_else(|| d["size"].as_str().and_then(|s| s.parse().ok()))?;
            if size < 16 << 30 || (!medium.is_empty() && medium.starts_with(&path)) {
                return None;
            }
            let model = d["model"].as_str().unwrap_or("").trim().to_string();
            let tran = d["tran"].as_str().unwrap_or("").to_string();
            Some((path, size, format!("{model} {tran}").trim().to_string()))
        })
        .collect()
}

fn part(disk: &str, n: u32) -> String {
    if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) { format!("{disk}p{n}") } else { format!("{disk}{n}") }
}

fn nics() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/sys/class/net").into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| Path::new("/sys/class/net").join(n).join("device").exists()).collect();
    v.sort();
    v
}

// ---- interactive ----

fn ask(prompt: &str, default: &str) -> String {
    let stdin = std::io::stdin();
    loop {
        if default.is_empty() {
            print!("  {prompt}: ");
        } else {
            print!("  {prompt} [{default}]: ");
        }
        let _ = std::io::stdout().flush();
        let mut s = String::new();
        if stdin.lock().read_line(&mut s).unwrap_or(0) == 0 {
            return default.to_string();
        }
        let s = s.trim().to_string();
        if s.is_empty() && !default.is_empty() {
            return default.to_string();
        }
        if !s.is_empty() {
            return s;
        }
    }
}

fn ask_secret(prompt: &str) -> String {
    loop {
        let _ = Command::new("stty").arg("-echo").status();
        let a = ask(prompt, "");
        println!();
        let b = ask("again", "");
        println!();
        let _ = Command::new("stty").arg("echo").status();
        if a != b {
            println!("  \x1b[31mThey don't match, once more.\x1b[0m");
        } else if a.chars().count() < 10 {
            println!("  \x1b[31mAt least 10 characters.\x1b[0m");
        } else {
            return a;
        }
    }
}

fn banner() {
    print!("\x1b[2J\x1b[H");
    println!("\x1b[1m  ┌──────┐\n  │  OG  │   OG-Host {}\n  │ HOST │   installer\n  └──────┘\x1b[0m\n", crate::update::VERSION);
}

fn interactive() -> Result<Answers> {
    banner();
    let ds = disks();
    if ds.is_empty() {
        bail!("no disk of 16 GB or more to install on");
    }
    println!("  Disks:");
    for (i, (p, s, d)) in ds.iter().enumerate() {
        println!("    {}) {p}  {:.0} GB  {d}", i + 1, *s as f64 / 1e9);
    }
    let pick: usize = ask("Install on disk number", "1").parse().unwrap_or(1).clamp(1, ds.len());
    let (disk, size, _) = ds[pick - 1].clone();
    let gb = size >> 30;
    println!("\n  Layout: the system, and the rest of the disk as a thin pool for VM disks (like Proxmox).");
    let root_default = (gb / 4).clamp(16, 64);
    let root_gb: u64 = ask("System size in GB (the rest is for VMs; 0 = whole disk for the system)", &root_default.to_string()).parse().unwrap_or(root_default);
    let fs = ask("File system for the system (ext4 / xfs)", "ext4");
    println!();
    let hostname = ask("Server name", "og-host");
    let ns = nics();
    println!("  Network cards: {}", ns.join(", "));
    let iface = ask("Network card", ns.first().map(String::as_str).unwrap_or("eth0"));
    let net = ask("Address: dhcp or static", "dhcp");
    let (mut address, mut gateway, mut dns) = (String::new(), String::new(), vec![]);
    if net == "static" {
        address = ask("Address with prefix (e.g. 192.168.1.20/24)", "");
        gateway = ask("Gateway", "");
        dns = ask("DNS servers", "1.1.1.1 9.9.9.9").split_whitespace().map(String::from).collect();
    }
    let timezone = ask("Time zone", "UTC");
    println!("\n  The first account for the web interface (an administrator):");
    let admin_user = ask("User name", "admin");
    let admin_password = ask_secret("Password");
    println!("  A password for root (the console):");
    let root_password = ask_secret("Password");
    let ssh_key = ask("SSH public key for root (optional, Enter to skip)", "-");
    let ssh_key = if ssh_key == "-" { String::new() } else { ssh_key };
    println!("\n  \x1b[1;31mEverything on {disk} will be erased.\x1b[0m");
    if ask("Type yes to install", "") != "yes" {
        bail!("cancelled");
    }
    Ok(Answers { disk, fs, root_gb, thin: root_gb > 0, hostname, net, iface, address, gateway, dns, timezone, admin_user, admin_password, root_password, ssh_key, kernel_args: String::new(), reboot: true })
}

fn from_file(p: &Path) -> Result<Answers> {
    let ps = p.to_string_lossy();
    let text = if ps.starts_with("http://") || ps.starts_with("https://") {
        // remote installs: the answers come from a URL (retried while the network comes up)
        let mut got = None;
        for _ in 0..30 {
            if let Ok(t) = sh("curl", &["-fsSL", "--max-time", "20", &ps]) {
                got = Some(t);
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        got.with_context(|| format!("fetching the answers from {ps}"))?
    } else {
        std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?
    };
    let kv: BTreeMap<String, String> = text.lines().filter(|l| !l.trim().starts_with('#')).filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))).collect();
    let g = |k: &str, d: &str| kv.get(k).cloned().unwrap_or_else(|| d.to_string());
    let disk = match g("disk", "auto").as_str() {
        "auto" => disks().first().map(|d| d.0.clone()).context("no disk to install on")?,
        d => d.to_string(),
    };
    let root_gb = g("root_gb", "24").parse().unwrap_or(24);
    // the install system itself takes SSH logins with this key (watch a remote install)
    let live_key = g("live_ssh_key", "");
    if !live_key.is_empty() {
        let _ = std::fs::create_dir_all("/root/.ssh");
        let _ = std::fs::write("/root/.ssh/authorized_keys", format!("{live_key}\n"));
        let _ = sh("ssh-keygen", &["-A"]);
        let _ = sh("systemctl", &["restart", "ssh"]);
        step("SSH into the install system is open for the given key");
    }
    Ok(Answers {
        disk,
        fs: g("fs", "ext4"),
        root_gb,
        thin: root_gb > 0 && g("thin", "yes") == "yes",
        hostname: g("hostname", "og-host"),
        net: g("net", "dhcp"),
        // `mac = aa:bb:..`: the card with that address (names differ between systems)
        iface: match kv.get("mac").map(|m| m.to_lowercase()) {
            Some(m) => nics().into_iter().find(|n| std::fs::read_to_string(format!("/sys/class/net/{n}/address")).is_ok_and(|a| a.trim() == m)).with_context(|| format!("no network card with address {m}"))?,
            None => g("iface", nics().first().map(String::as_str).unwrap_or("eth0")),
        },
        address: g("address", ""),
        gateway: g("gateway", ""),
        dns: g("dns", "").split_whitespace().map(String::from).collect(),
        timezone: g("timezone", "UTC"),
        admin_user: g("admin_user", "admin"),
        admin_password: g("admin_password", ""),
        root_password: g("root_password", ""),
        ssh_key: g("ssh_key", ""),
        kernel_args: g("kernel_args", "").chars().filter(|c| c.is_ascii_graphic() || *c == ' ').filter(|c| !"\"'`$\\".contains(*c)).collect(),
        reboot: g("reboot", "yes") == "yes",
    })
}

fn step(msg: &str) {
    println!("  \x1b[36m•\x1b[0m {msg}");
    // also to the kernel log, so a serial console shows an unattended install's progress
    let _ = std::fs::OpenOptions::new().write(true).open("/dev/kmsg").and_then(|mut f| f.write_all(format!("og-installer: {msg}\n").as_bytes()));
}

fn install(a: &Answers) -> Result<()> {
    let squash = squash();
    if !Path::new(&squash).exists() {
        bail!("this isn't the OG-Host install system ({squash} missing)");
    }
    if !crate::vms::valid_name(&a.hostname.to_lowercase()) {
        bail!("server names are letters, digits and -");
    }
    // key-only installs (remote): no passwords; root signs in with the SSH key and makes the
    // web account afterwards (og-hostd passwd)
    let key_only = a.admin_password.is_empty() && a.root_password.is_empty() && !a.ssh_key.is_empty();
    if !key_only && (a.admin_password.chars().count() < 10 || a.root_password.is_empty()) {
        bail!("passwords missing (admin: 10+ characters), or give an ssh_key and no passwords");
    }
    let disk = a.disk.as_str();
    step(&format!("partitioning {disk}"));
    // what an earlier attempt (or an older install on this disk) left mounted or active
    let _ = sh("swapoff", &["-a"]);
    let _ = sh("umount", &["-R", TARGET]);
    let _ = sh("umount", &["-R", "-l", TARGET]);
    for v in ["og", "pve"] {
        let _ = sh("vgchange", &["-an", v]);
    }
    let _ = sh("dmsetup", &["remove_all"]);
    let _ = sh("udevadm", &["settle"]);
    sh("wipefs", &["-a", "-f", disk])?;
    // GPT: BIOS boot, EFI, /boot (GRUB can't read thin volumes), the rest LVM
    let table = "label: gpt\nsize=1MiB, type=21686148-6449-6E6F-744E-656564454649, name=bios\nsize=512MiB, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name=esp\nsize=1GiB, type=BC13C2FF-59E6-4262-A352-B275FD6F7172, name=og-boot\ntype=E6D6D379-F507-44C2-A23C-238F2A3DF928, name=og-lvm\n";
    {
        let mut c = Command::new("sfdisk").args(["--quiet", "--wipe", "always", disk]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
        c.stdin.take().unwrap().write_all(table.as_bytes())?;
        let out = c.wait_with_output()?;
        if !out.status.success() {
            bail!("sfdisk: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    let _ = sh("partprobe", &[disk]);
    let _ = sh("udevadm", &["settle"]);
    let (esp, bootp, lvmp) = (part(disk, 2), part(disk, 3), part(disk, 4));
    step("volumes: the system (og/root, with room for update snapshots), OG-Host's data (og/state), VM disks (og/data)");
    sh("pvcreate", &["-ff", "-y", &lvmp])?;
    sh("vgcreate", &["og", &lvmp])?;
    let free_gib: u64 = sh("vgs", &["--noheadings", "--units", "g", "--nosuffix", "-o", "vg_free", "og"])?.trim().parse::<f64>().unwrap_or(0.0) as u64;
    // the system's own pool: the root plus half again for the snapshot an update keeps
    let root_gib = if a.thin { a.root_gb } else { (free_gib * 6 / 10).max(16) };
    let sys_gib = if a.thin { (root_gib * 3 / 2).min(free_gib.saturating_sub(8)) } else { free_gib * 95 / 100 };
    sh("lvcreate", &["-q", "-y", "-L", &format!("{sys_gib}G"), "-T", "og/sys"])?;
    sh("lvcreate", &["-q", "-y", "-V", &format!("{root_gib}G"), "-T", "og/sys", "-n", "root"])?;
    let state_pool = if a.thin {
        sh("lvcreate", &["-q", "-y", "-l", "95%FREE", "-T", "og/data"])?;
        "og/data"
    } else {
        "og/sys"
    };
    let state_gib = if a.thin { free_gib.saturating_sub(sys_gib) * 9 / 10 } else { free_gib * 3 / 10 };
    sh("lvcreate", &["-q", "-y", "-V", &format!("{}G", state_gib.max(8)), "-T", state_pool, "-n", "state"])?;
    let (rootp, statep) = ("/dev/og/root".to_string(), "/dev/og/state".to_string());
    step("making file systems");
    sh("mkfs.vfat", &["-F", "32", "-n", "OGEFI", &esp])?;
    sh("mkfs.ext4", &["-q", "-F", "-L", "og-boot", &bootp])?;
    match a.fs.as_str() {
        "xfs" => sh("mkfs.xfs", &["-f", "-L", "og-root", &rootp])?,
        _ => sh("mkfs.ext4", &["-q", "-F", "-L", "og-root", &rootp])?,
    };
    sh("mkfs.ext4", &["-q", "-F", "-L", "og-state", &statep])?;
    std::fs::create_dir_all(TARGET)?;
    sh("mount", &[&rootp, TARGET])?;
    step("copying the system (a few minutes)");
    // onto the root first (the image has hard links across what become separate volumes), then
    // /boot and OG-Host's data move onto their own
    sh("unsquashfs", &["-f", "-q", "-n", "-d", TARGET, &squash])?;
    for (dev, at) in [(&bootp, "boot"), (&statep, "var/lib/og-host")] {
        let dir = format!("{TARGET}/{at}");
        std::fs::create_dir_all(&dir)?;
        let tmp = "/mnt/og-move";
        std::fs::create_dir_all(tmp)?;
        sh("mount", &[dev, tmp])?;
        sh("cp", &["-a", &format!("{dir}/."), tmp])?;
        sh("umount", &[tmp])?;
        sh("find", &[&dir, "-mindepth", "1", "-delete"])?;
        sh("mount", &[dev, &dir])?;
    }
    std::fs::create_dir_all(format!("{TARGET}/boot/efi"))?;
    sh("mount", &[&esp, &format!("{TARGET}/boot/efi")])?;
    for m in ["dev", "proc", "sys", "run"] {
        let p = format!("{TARGET}/{m}");
        std::fs::create_dir_all(&p)?;
        sh("mount", &["--rbind", &format!("/{m}"), &p])?;
    }
    if Path::new("/sys/firmware/efi/efivars").exists() {
        let _ = sh("mount", &["-t", "efivarfs", "efivarfs", &format!("{TARGET}/sys/firmware/efi/efivars")]);
    }
    let r = configure(a, disk, &rootp, &esp, &bootp, &statep);
    // unmount whatever happened
    let _ = sh("sync", &[]);
    let _ = sh("umount", &["-R", TARGET]);
    r?;
    Ok(())
}

fn configure(a: &Answers, disk: &str, rootp: &str, esp: &str, bootp: &str, statep: &str) -> Result<()> {
    step("configuring");
    let uuid = |p: &str| sh("blkid", &["-s", "UUID", "-o", "value", p]).map(|s| s.trim().to_string());
    let (ru, eu, bu, su) = (uuid(rootp)?, uuid(esp)?, uuid(bootp)?, uuid(statep)?);
    let opts = if a.fs == "xfs" { "defaults" } else { "defaults,errors=remount-ro" };
    std::fs::write(
        format!("{TARGET}/etc/fstab"),
        format!(
            "# OG-Host: the system (og/root, snapshotted for updates), /boot, EFI, OG-Host's data (og/state, never rolled back)\nUUID={ru} / {} {opts} 0 1\nUUID={bu} /boot ext4 defaults 0 2\nUUID={eu} /boot/efi vfat umask=0077 0 1\nUUID={su} /var/lib/og-host ext4 defaults 0 2\n",
            if a.fs == "xfs" { "xfs" } else { "ext4" }
        ),
    )?;
    let host = a.hostname.to_lowercase();
    std::fs::write(format!("{TARGET}/etc/hostname"), format!("{host}\n"))?;
    std::fs::write(format!("{TARGET}/etc/hosts"), format!("127.0.0.1\tlocalhost\n127.0.1.1\t{host}\n::1\tlocalhost ip6-localhost ip6-loopback\n"))?;
    // network (systemd-networkd): the card is matched by its burned-in address, so it's still
    // found if its name changes (another slot, new firmware); bridges made later take over
    let mac = std::fs::read_to_string(format!("/sys/class/net/{}/address", a.iface)).map(|m| m.trim().to_string()).unwrap_or_default();
    let matcher = if mac.is_empty() { format!("Name={}", a.iface) } else { format!("PermanentMACAddress={mac}") };
    let net = if a.net == "static" {
        let mut s = format!("[Match]\n{matcher}\n\n[Network]\nAddress={}\n", a.address);
        if !a.gateway.is_empty() {
            s.push_str(&format!("Gateway={}\n", a.gateway));
        }
        for d in &a.dns {
            s.push_str(&format!("DNS={d}\n"));
        }
        s
    } else {
        format!("[Match]\n{matcher}\n\n[Network]\nDHCP=yes\n\n[DHCPv4]\nClientIdentifier=mac\n")
    };
    std::fs::write(format!("{TARGET}/etc/systemd/network/10-og-install.network"), format!("# written by the OG-Host installer\n{net}"))?;
    // the install system's catch-all DHCP goes: only the chosen card is configured
    let _ = std::fs::remove_file(format!("{TARGET}/etc/systemd/network/99-og-live.network"));
    if Path::new(&format!("/usr/share/zoneinfo/{}", a.timezone)).exists() {
        let _ = std::fs::remove_file(format!("{TARGET}/etc/localtime"));
        std::os::unix::fs::symlink(format!("/usr/share/zoneinfo/{}", a.timezone), format!("{TARGET}/etc/localtime"))?;
        std::fs::write(format!("{TARGET}/etc/timezone"), format!("{}\n", a.timezone))?;
    }
    // identity: new machine id and SSH host keys for this server
    std::fs::write(format!("{TARGET}/etc/machine-id"), "")?;
    let _ = std::fs::remove_file(format!("{TARGET}/var/lib/dbus/machine-id"));
    chroot(&["systemd-machine-id-setup"])?;
    for e in std::fs::read_dir(format!("{TARGET}/etc/ssh")).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("ssh_host_") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let _ = chroot(&["ssh-keygen", "-A"]);
    if a.root_password.is_empty() {
        let _ = chroot(&["passwd", "-l", "root"]);
    } else {
        chroot_input(&["chpasswd"], &format!("root:{}\n", a.root_password))?;
    }
    if !a.ssh_key.is_empty() {
        std::fs::create_dir_all(format!("{TARGET}/root/.ssh"))?;
        std::fs::write(format!("{TARGET}/root/.ssh/authorized_keys"), format!("{}\n", a.ssh_key))?;
        chroot(&["chmod", "-R", "go-rwx", "/root/.ssh"])?;
    }
    if a.admin_password.is_empty() {
        step("no web account yet: sign in over SSH and run 'og-hostd passwd <name>' to make the first one");
    } else {
        step("first OG-Host account");
        chroot_input(&["/usr/local/bin/og-hostd", "passwd", &a.admin_user], &format!("{}\n", a.admin_password))?;
    }
    std::fs::create_dir_all(format!("{TARGET}/var/lib/og-host/images"))?;
    if a.thin {
        std::fs::write(format!("{TARGET}/var/lib/og-host/pools.json"), serde_json::to_vec_pretty(&serde_json::json!([
            {"id": "local", "type": "dir", "path": "/var/lib/og-host/images", "enabled": true, "comment": "Disk images on the system disk"},
            {"id": "local-thin", "type": "lvmthin", "vg": "og", "thinpool": "data", "enabled": true, "comment": "VM disks (thin)"}
        ]))?)?;
    }
    // the install system's own pieces go
    step("boot loader");
    let _ = chroot(&["dpkg", "--purge", "live-boot", "live-boot-initramfs-tools"]);
    let _ = chroot(&["systemctl", "disable", "og-installer.service", "og-live-shell.service"]);
    chroot(&["update-initramfs", "-u", "-k", "all"])?;
    // GRUB boots the saved entry: an update tries a new kernel once before it becomes the default;
    // a kernel that panics reboots (into the previous one) after 10 s
    std::fs::create_dir_all(format!("{TARGET}/etc/default/grub.d"))?;
    std::fs::write(format!("{TARGET}/etc/default/grub.d/og-host.cfg"), format!("GRUB_DISTRIBUTOR=\"OG-Host\"\nGRUB_TIMEOUT=3\nGRUB_CMDLINE_LINUX_DEFAULT=\"quiet{}\"\n", if a.kernel_args.is_empty() { String::new() } else { format!(" {}", a.kernel_args) }))?;
    std::fs::write(format!("{TARGET}/etc/default/grub.d/og-host-saved.cfg"), "# OG-Host updates: boot the saved entry; a panicking kernel reboots after 10 s\nGRUB_DEFAULT=saved\nGRUB_CMDLINE_LINUX=\"$GRUB_CMDLINE_LINUX panic=10\"\n")?;
    // UEFI (also at the fallback path, so it boots without an NVRAM entry) and BIOS
    let efi = Path::new("/sys/firmware/efi").exists();
    chroot(&["grub-install", "--target=x86_64-efi", "--efi-directory=/boot/efi", "--bootloader-id=og-host", "--removable", "--no-nvram"])?;
    if efi {
        let _ = chroot(&["grub-install", "--target=x86_64-efi", "--efi-directory=/boot/efi", "--bootloader-id=og-host"]);
    }
    chroot(&["grub-install", "--target=i386-pc", disk])?;
    chroot(&["update-grub"])?;
    step("done");
    Ok(())
}

/// `og-auto.cfg` on the drive labelled OG-AUTO (waits a little for USB sticks to show up).
fn auto_from_label() -> Result<PathBuf> {
    for _ in 0..30 {
        if let Ok(dev) = sh("blkid", &["-L", "OG-AUTO"]) {
            let dir = "/run/og-auto";
            std::fs::create_dir_all(dir)?;
            let _ = sh("mount", &["-o", "ro", dev.trim(), dir]);
            let f = Path::new(dir).join("og-auto.cfg");
            if f.exists() {
                return Ok(f);
            }
            bail!("the OG-AUTO drive has no og-auto.cfg");
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    bail!("no drive labelled OG-AUTO with the answers (og-auto.cfg)")
}

pub fn run(auto: Option<PathBuf>) -> Result<()> {
    // unattended: og.auto=<file> on the kernel command line (or --auto)
    let auto = match auto.or_else(|| std::fs::read_to_string("/proc/cmdline").ok().and_then(|c| c.split_whitespace().find_map(|w| w.strip_prefix("og.auto=").map(PathBuf::from)))) {
        Some(p) if p.as_os_str() == "label" => Some(auto_from_label()?),
        a => a,
    };
    loop {
        let a = match &auto {
            Some(p) => from_file(p),
            None => interactive(),
        };
        let r = a.and_then(|a| {
            println!();
            install(&a)?;
            Ok(a.reboot)
        });
        match r {
            Ok(reboot) => {
                println!("\n  \x1b[1;32mOG-Host is installed.\x1b[0m Remove the install medium; after the restart the server shows its web address here.");
                if reboot {
                    std::thread::sleep(std::time::Duration::from_secs(if auto.is_some() { 3 } else { 8 }));
                    let _ = Command::new("systemctl").arg("reboot").status();
                }
                return Ok(());
            }
            Err(e) => {
                println!("\n  \x1b[1;31mThe install didn't finish:\x1b[0m {e:#}");
                step(&format!("FAILED: {e:#}"));
                if auto.is_some() {
                    return Err(e);
                }
                ask("Press Enter to start over", "x");
            }
        }
    }
}
