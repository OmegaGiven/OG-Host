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
    /// unattended: the target's serial number, checked before anything is written
    disk_serial: String,
    /// unattended: show the plan and wait for `/run/og-installer/go` (holding the target's
    /// serial) before writing anything; for remote installs watched over SSH
    wait_for_go: bool,
}

/// (kernel name e.g. /dev/nvme0n1, serial, model, size) of a whole disk; `path` may be a
/// /dev/disk/by-id/... link.
fn disk_info(path: &str) -> Result<(String, String, String, u64)> {
    let canon = std::fs::canonicalize(path).with_context(|| format!("no disk {path}"))?;
    let c = canon.to_string_lossy().into_owned();
    let v: Value = serde_json::from_str(&sh("lsblk", &["-dbJ", "-o", "PATH,TYPE,SERIAL,MODEL,SIZE", &c])?)?;
    let d = &v["blockdevices"][0];
    if d["type"] != "disk" {
        bail!("{path} ({c}) is not a whole disk");
    }
    let size = d["size"].as_u64().or_else(|| d["size"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
    Ok((c, d["serial"].as_str().unwrap_or("").trim().to_string(), d["model"].as_str().unwrap_or("").trim().to_string(), size))
}

/// Every block device that isn't the target disk or one of its partitions.
fn other_block_devices(target: &str) -> Vec<String> {
    // the target and everything stacked on it (partitions, LVM volumes on them)
    let mine: Vec<String> = sh("lsblk", &["-nlpo", "NAME", target]).unwrap_or_default().lines().map(|l| l.trim().to_string()).collect();
    let out = sh("lsblk", &["-nlpo", "NAME,TYPE,PKNAME"]).unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (name, typ) = (*f.first()?, *f.get(1)?);
            let parent = f.get(2).copied().unwrap_or("");
            // no media in it (an empty floppy or card reader): nothing to write to
            let size: u64 = std::fs::read_to_string(format!("/sys/class/block/{}/size", name.trim_start_matches("/dev/"))).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(1);
            if size == 0 || name == target || parent == target || mine.iter().any(|m| m == name) || matches!(typ, "loop" | "rom") || name.starts_with("/dev/zram") || name.starts_with("/dev/ram") {
                return None;
            }
            Some(name.to_string())
        })
        .collect()
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

/// Human sizes the way drives are sold: 4.0 TB, 512 GB.
fn human(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1000.0 { format!("{:.1} TB", gb / 1000.0) } else { format!("{gb:.0} GB") }
}

/// The disk's stable name under /dev/disk/by-id (model + serial), e.g. nvme-Samsung_SSD_..._S7U8.
fn by_id(dev: &str) -> String {
    let mut best = String::new();
    for e in std::fs::read_dir("/dev/disk/by-id").into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("wwn-") || n.starts_with("nvme-eui") || n.contains("-part") {
            continue;
        }
        if std::fs::canonicalize(e.path()).is_ok_and(|t| t.to_string_lossy() == dev) && (best.is_empty() || n.len() < best.len()) {
            best = n;
        }
    }
    best
}

/// What's on a disk already: file systems with labels and sizes, so disks holding data stand out.
fn disk_contents(dev: &str) -> String {
    let parts: Vec<String> = sh("lsblk", &["-nlpo", "FSTYPE,LABEL,SIZE", dev])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let fs = *f.first()?;
            (f.len() >= 2 && !fs.chars().next()?.is_ascii_digit()).then(|| f.join(" "))
        })
        .collect();
    if parts.is_empty() { "no file systems found".into() } else { format!("HAS DATA: {}", parts.join(", ")) }
}

/// Pick the install disk: arrow keys + Enter (or the number), size first. Every disk is listed
/// with model, serial, stable id and what's on it. Nothing is preselected for Enter alone to take.
fn pick_disk(ds: &[(String, u64, String)]) -> Result<usize> {
    struct Row {
        size: String,
        dev: String,
        model: String,
        serial: String,
        id: String,
        has: String,
    }
    let rows: Vec<Row> = ds
        .iter()
        .map(|(p, s, d)| {
            let (_, serial, model, _) = disk_info(p).unwrap_or_default();
            Row { size: human(*s), dev: p.clone(), model: if model.is_empty() { d.clone() } else { model }, serial, id: by_id(p), has: disk_contents(p) }
        })
        .collect();
    let draw = |cur: Option<usize>| {
        print!("\x1b[2J\x1b[H");
        println!("\n  \x1b[1mWhich disk should OG-Host be installed on?\x1b[0m");
        println!("  Only that disk is erased. Every other disk is locked read-only before anything is written.");
        println!("  \x1b[2mUp/Down to move, Enter to choose (or type its number). Nothing is selected yet.\x1b[0m\n");
        for (i, r) in rows.iter().enumerate() {
            let on = cur == Some(i);
            let (a, b) = if on { ("\x1b[7m", "\x1b[0m") } else { ("", "") };
            println!("  {a}{} {}) \x1b[1m{:>8}\x1b[22m  {:<36} {:<14}{b}", if on { ">" } else { " " }, i + 1, r.size, r.model, r.dev);
            println!("        serial {}   id {}", if r.serial.is_empty() { "?" } else { &r.serial }, if r.id.is_empty() { "?" } else { &r.id });
            let warn = r.has.starts_with("HAS DATA");
            println!("        {}{}{}\n", if warn { "\x1b[33m" } else { "" }, r.has, if warn { "\x1b[0m" } else { "" });
        }
    };
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if !tty {
        draw(None);
        loop {
            match ask("Install on disk number", "").parse::<usize>() {
                Ok(n) if (1..=rows.len()).contains(&n) => return Ok(n),
                _ => println!("  Type the number of the disk (1 to {}).", rows.len()),
            }
        }
    }
    let _ = Command::new("stty").args(["-icanon", "-echo", "min", "1"]).status();
    let mut cur: Option<usize> = None;
    let result = (|| -> Result<usize> {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        loop {
            draw(cur);
            let _ = std::io::stdout().flush();
            let mut b = [0u8; 1];
            stdin.read_exact(&mut b)?;
            match b[0] {
                0x1b => {
                    let mut seq = [0u8; 2];
                    stdin.read_exact(&mut seq)?;
                    let n = rows.len();
                    match seq[1] {
                        b'A' => cur = Some(cur.map(|c| (c + n - 1) % n).unwrap_or(n - 1)),
                        b'B' => cur = Some(cur.map(|c| (c + 1) % n).unwrap_or(0)),
                        _ => {}
                    }
                }
                b'\r' | b'\n' => {
                    if let Some(c) = cur {
                        return Ok(c + 1);
                    }
                }
                d @ b'1'..=b'9' => {
                    let i = (d - b'1') as usize;
                    if i < rows.len() {
                        cur = Some(i);
                    }
                }
                _ => {}
            }
        }
    })();
    let _ = Command::new("stty").args(["icanon", "echo"]).status();
    println!();
    result
}

fn interactive() -> Result<Answers> {
    banner();
    let ds = disks();
    if ds.is_empty() {
        bail!("no disk of 16 GB or more to install on");
    }
    let pick = pick_disk(&ds)?;
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
    let (_, serial, model, _) = disk_info(&disk)?;
    let short = disk.trim_start_matches("/dev/").to_string();
    println!("\n  \x1b[1;31mEverything on {disk} ({model}, serial {serial}) will be erased.\x1b[0m");
    if ask(&format!("Type the disk's name ({short}) to install"), "") != short {
        bail!("cancelled");
    }
    Ok(Answers { disk, fs, root_gb, thin: root_gb > 0, hostname, net, iface, address, gateway, dns, timezone, admin_user, admin_password, root_password, ssh_key, kernel_args: String::new(), reboot: true, disk_serial: String::new(), wait_for_go: false })
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
    // the target: never a guess when there's more than one disk
    let candidates = disks();
    let disk = match g("disk", "auto").as_str() {
        "auto" if candidates.len() == 1 => candidates[0].0.clone(),
        "auto" => bail!("this machine has {} disks: name the one to install on (disk = /dev/disk/by-id/...) and its serial (disk_serial = ...)", candidates.len()),
        d => d.to_string(),
    };
    let (disk, serial, model, size) = disk_info(&disk)?;
    if !candidates.iter().any(|c| c.0 == disk) {
        bail!("{disk} isn't a disk OG-Host can be installed on (read-only, too small or the install medium)");
    }
    let want_serial = g("disk_serial", "");
    if candidates.len() > 1 && want_serial.is_empty() {
        bail!("this machine has {} disks: disk_serial is required (the target's is {serial:?})", candidates.len());
    }
    if !want_serial.is_empty() && want_serial != serial {
        bail!("{disk} has serial {serial:?}, not {want_serial:?}: refusing to install");
    }
    step(&format!("target disk: {disk} ({model}, {} GB, serial {serial})", size / 1_000_000_000));
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
        disk_serial: want_serial,
        wait_for_go: g("wait_for_go", "no") == "yes",
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
    let (_, serial, model, size) = disk_info(disk)?;
    // every other disk is made read-only in the kernel first: whatever happens below (or any
    // tool it runs) can't write to them; it lasts until the next boot
    let mut others = other_block_devices(disk);
    let mut no_media = Vec::new();
    for d in &others {
        if sh("blockdev", &["--setro", d]).is_err() {
            // a device that can't even be opened (empty floppy/card slot) can't be written either
            let gone = std::fs::File::open(d).err().and_then(|e| e.raw_os_error()).is_some_and(|c| c == libc::ENXIO || c == libc::ENOMEDIUM);
            if !gone {
                bail!("couldn't make {d} read-only: refusing to go on");
            }
            no_media.push(d.clone());
        }
    }
    others.retain(|d| !no_media.contains(d));
    let still_rw: Vec<&String> = others.iter().filter(|d| sh("blockdev", &["--getro", d]).map(|r| r.trim() != "1").unwrap_or(true)).collect();
    if !still_rw.is_empty() {
        bail!("still writable: {still_rw:?}: refusing to go on");
    }
    step(&format!("will erase {disk} ({model}, {} GB, serial {serial}); read-only now: {}", size / 1_000_000_000, if others.is_empty() { "none".to_string() } else { others.join(" ") }));
    if a.wait_for_go {
        let go = Path::new("/run/og-installer/go");
        std::fs::create_dir_all("/run/og-installer")?;
        std::fs::write("/run/og-installer/plan.txt", format!("erase {disk} {model} {} GB serial {serial}\nread-only: {}\n", size / 1_000_000_000, others.join(" ")))?;
        step(&format!("waiting: write the target's serial to {} to start (plan in /run/og-installer/plan.txt)", go.display()));
        loop {
            if let Ok(t) = std::fs::read_to_string(go) {
                if t.trim() == serial {
                    break;
                }
                let _ = std::fs::remove_file(go);
                step("that isn't the target's serial: still waiting");
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
    step(&format!("partitioning {disk}"));
    // what an earlier attempt (or an older install on this disk) left mounted or active, on
    // this disk only
    let _ = sh("swapoff", &["-a"]);
    let _ = sh("umount", &["-R", TARGET]);
    let _ = sh("umount", &["-R", "-l", TARGET]);
    let tree = sh("lsblk", &["-nlpo", "NAME,TYPE", disk]).unwrap_or_default();
    let mapped: Vec<String> = tree.lines().filter_map(|l| l.split_once(char::is_whitespace)).filter(|(_, t)| matches!(t.trim(), "lvm" | "crypt" | "dm")).map(|(n, _)| n.to_string()).collect();
    for m in mapped.iter().rev() {
        let name = m.trim_start_matches("/dev/mapper/");
        let _ = sh("dmsetup", &["remove", "--retry", "-f", name]);
    }
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
    let _ = chroot(&["dpkg", "--purge", "live-boot", "live-boot-initramfs-tools", "os-prober"]);
    let _ = chroot(&["systemctl", "disable", "og-installer.service", "og-live-shell.service"]);
    chroot(&["update-initramfs", "-u", "-k", "all"])?;
    // GRUB boots the saved entry: an update tries a new kernel once before it becomes the default;
    // a kernel that panics reboots (into the previous one) after 10 s
    std::fs::create_dir_all(format!("{TARGET}/etc/default/grub.d"))?;
    std::fs::write(format!("{TARGET}/etc/default/grub.d/og-host.cfg"), format!("GRUB_DISTRIBUTOR=\"OG-Host\"\nGRUB_TIMEOUT=3\n# never look at other disks for operating systems (mounting them can write to them)\nGRUB_DISABLE_OS_PROBER=true\nGRUB_CMDLINE_LINUX_DEFAULT=\"quiet{}\"\n", if a.kernel_args.is_empty() { String::new() } else { format!(" {}", a.kernel_args) }))?;
    std::fs::write(format!("{TARGET}/etc/default/grub.d/og-host-saved.cfg"), "# OG-Host updates: boot the saved entry; a panicking kernel reboots after 10 s\nGRUB_DEFAULT=saved\nGRUB_CMDLINE_LINUX=\"$GRUB_CMDLINE_LINUX panic=10\"\n")?;
    // UEFI (also at the fallback path, so it boots without an NVRAM entry) and BIOS
    let efi = Path::new("/sys/firmware/efi").exists();
    chroot(&["grub-install", "--target=x86_64-efi", "--efi-directory=/boot/efi", "--bootloader-id=og-host", "--removable", "--no-nvram"])?;
    if efi {
        let _ = chroot(&["grub-install", "--target=x86_64-efi", "--efi-directory=/boot/efi", "--bootloader-id=og-host"]);
    }
    chroot(&["grub-install", "--target=i386-pc", disk])?;
    chroot(&["update-grub"])?;
    // everything on the disk before saying done: a power cut right after must not leave the
    // thin pools' metadata half-written
    let _ = sh("umount", &["-R", TARGET]);
    let _ = sh("sync", &[]);
    if sh("vgchange", &["-an", "og"]).is_err() {
        let _ = sh("umount", &["-R", "-l", TARGET]);
        sh("vgchange", &["-an", "og"]).context("deactivating the new volumes")?;
    }
    let _ = sh("sync", &[]);
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
