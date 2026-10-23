//! Whole-system updates with rollback: Debian's packages (base, security, kernel) and the OG-Host
//! release in one transaction, so a server is installed once and never rewritten.
//!
//! 1. Before anything changes: a copy of `/boot` (kernels, GRUB) is saved, then the system root
//!    (an LVM thin volume, `og/root`, on its own thin pool) is snapshotted: instant, and only
//!    what changes afterwards takes space.
//! 2. `apt-get full-upgrade` (Debian's signed mirrors), then the OG-Host release's binaries.
//! 3. A new kernel is only tried once (`grub-reboot`); GRUB's default stays on the old one until
//!    the new one has proven itself. Without a new kernel or library that needs it, there's no
//!    reboot: og-hostd restarts.
//! 4. Trial: the updated system has to come up healthy (og-hostd running for 90 s, the network
//!    and SSH up) on the kernel it was meant to boot. Then the update is kept: the new kernel
//!    becomes the default and the snapshot stays as "the previous system" until the next update.
//! 5. Otherwise (og-hostd keeps failing, the new kernel didn't boot, nothing confirmed the update
//!    within 10 minutes of booting): the snapshot is merged back into the root, `/boot` comes
//!    back from the copy, and the server reboots into exactly the system it had before.
//!
//! OG-Host's own data (`/var/lib/og-host`: VM settings, containers, volumes) is on its own
//! volume (`og/state`), outside the snapshot: a rollback brings back the operating system and
//! the binaries, never older VMs or lost data.
//!
//! The transaction's state lives on `/boot` (not on the root that may be rolled back), so it
//! survives the rollback. Systems installed before this layout (root not on LVM thin) still get
//! updates and the kernel trial, just no root rollback.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const STATE: &str = "/boot/og-host-update.json";
const HEALTHY_AFTER: Duration = Duration::from_secs(90);
/// a trial nobody confirmed (og-hostd never came up) is rolled back by the boot check timer
const CONFIRM_WITHIN_MIN: u64 = 10;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Txn {
    pub id: String,
    /// applying | await-reboot | trial | done | rolling-back | rolled-back | failed
    pub stage: String,
    pub started: u64,
    pub finished: u64,
    pub from_version: String,
    pub to_version: String,
    /// LVM root: vg/lv and the snapshot taken before (empty: no root rollback here)
    pub root_lv: String,
    pub snapshot: String,
    pub boot_copy: String,
    pub old_kernel: String,
    /// set when the update brought a newer kernel: the trial boots it once
    pub new_kernel: String,
    pub packages: Vec<String>,
    pub reboot: bool,
    pub log: Vec<String>,
    pub error: String,
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).output().with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn state() -> Option<Txn> {
    std::fs::read(STATE).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save(t: &Txn) -> Result<()> {
    let tmp = format!("{STATE}.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(t)?)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, STATE)?;
    if let Ok(d) = std::fs::File::open("/boot") {
        let _ = d.sync_all();
    }
    Ok(())
}

fn note(msg: impl Into<String>, t: &mut Txn, task: Option<&crate::tasks::Task>) {
    let m = msg.into();
    if let Some(task) = task {
        task.log(m.clone());
    }
    eprintln!("og-hostd system update: {m}");
    t.log.push(format!("{} {m}", crate::containers::unix_now()));
}

/// The root file system's LVM thin volume (`vg/lv`), if it is one.
pub fn root_thin_lv() -> Option<String> {
    let src = run("findmnt", &["-no", "SOURCE", "/"]).ok()?.trim().to_string();
    let out = run("lvs", &["--noheadings", "-o", "vg_name,lv_name,pool_lv", &src]).ok()?;
    let f: Vec<&str> = out.split_whitespace().collect();
    (f.len() == 3).then(|| format!("{}/{}", f[0], f[1]))
}

/// Installed kernel versions, oldest first.
pub fn kernels() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/boot").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_string_lossy().strip_prefix("vmlinuz-").map(String::from)).collect();
    v.sort_by(|a, b| cmp_versions(a, b));
    v
}

fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let ok = |op: &str| Command::new("dpkg").args(["--compare-versions", a, op, b]).status().is_ok_and(|s| s.success());
    if ok("lt") {
        std::cmp::Ordering::Less
    } else if ok("gt") {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}

pub fn running_kernel() -> String {
    run("uname", &["-r"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// GRUB's id path for a kernel's entry ("advanced submenu>entry"), from grub.cfg.
fn grub_entry(kernel: &str) -> Option<String> {
    let cfg = std::fs::read_to_string("/boot/grub/grub.cfg").ok()?;
    let mut submenu = None;
    for l in cfg.lines() {
        let l = l.trim();
        let id = l.rsplit_once("$menuentry_id_option '").and_then(|(_, r)| r.split('\'').next()).map(String::from);
        if l.starts_with("submenu ") {
            submenu = id;
        } else if l.starts_with("menuentry ") && l.contains(&format!("Linux {kernel}'")) && !l.contains("recovery") {
            let id = id?;
            return Some(match &submenu {
                Some(s) => format!("{s}>{id}"),
                None => id,
            });
        }
    }
    None
}

/// GRUB boots the saved entry (so a new kernel can be tried once without becoming the default).
fn ensure_grub_saved_default() -> Result<()> {
    let p = Path::new("/etc/default/grub.d/og-host-saved.cfg");
    if !p.exists() {
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, "# OG-Host updates: boot the saved entry; a panicking kernel reboots after 10 s\nGRUB_DEFAULT=saved\nGRUB_CMDLINE_LINUX=\"$GRUB_CMDLINE_LINUX panic=10\"\n")?;
        run("update-grub", &[])?;
    }
    Ok(())
}

fn pool_used_pct(root_lv: &str) -> Option<f64> {
    let pool = run("lvs", &["--noheadings", "-o", "pool_lv", root_lv]).ok()?.trim().to_string();
    let vg = root_lv.split('/').next()?;
    run("lvs", &["--noheadings", "-o", "data_percent", &format!("{vg}/{pool}")]).ok()?.trim().parse().ok()
}

/// The boot check timer: if a trial isn't confirmed within 10 minutes of booting (og-hostd never
/// came up to do it), roll back. Runs from the previous og-hostd (kept by the release guard).
fn install_boot_check(guard_bin: &Path, root: &Path) -> Result<()> {
    std::fs::write("/etc/systemd/system/og-host-update-check.service", format!("[Unit]\nDescription=OG-Host: roll back an update that didn't come up\n\n[Service]\nType=oneshot\nExecStart={} --root {} update-boot-check\n", guard_bin.display(), root.display()))?;
    std::fs::write("/etc/systemd/system/og-host-update-check.timer", format!("[Unit]\nDescription=OG-Host: check an update on trial\n\n[Timer]\nOnBootSec={CONFIRM_WITHIN_MIN}min\n\n[Install]\nWantedBy=timers.target\n"))?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "og-host-update-check.timer"])?;
    Ok(())
}

fn remove_boot_check() {
    let _ = run("systemctl", &["disable", "og-host-update-check.timer"]);
    let _ = std::fs::remove_file("/etc/systemd/system/og-host-update-check.service");
    let _ = std::fs::remove_file("/etc/systemd/system/og-host-update-check.timer");
    let _ = run("systemctl", &["daemon-reload"]);
}

/// What `apt` would upgrade (names).
fn apt_pending() -> Vec<String> {
    crate::update::apt_upgradable().unwrap_or_default().iter().filter_map(|v| v["name"].as_str().map(String::from)).collect()
}

/// Run a whole-system update now (a task). `reboot`: reboot right away when the update needs
/// it (else it waits, stage await-reboot). Returns whether anything was updated.
pub fn run_update(d: &crate::serve::Daemon, task: &crate::tasks::Task, release: Option<&crate::update::Available>, reboot: bool) -> Result<bool> {
    if let Some(t) = state() {
        if ["applying", "await-reboot", "trial", "rolling-back"].contains(&t.stage.as_str()) {
            bail!("an update is in progress already ({}, stage {})", t.id, t.stage);
        }
    }
    task.log("apt-get update");
    crate::update::apt_run(task, false)?;
    let pkgs = apt_pending();
    let rel = release.filter(|a| a.newer);
    if pkgs.is_empty() && rel.is_none() {
        task.log("nothing to update");
        return Ok(false);
    }
    let id = format!("{}", crate::containers::unix_now());
    let mut t = Txn { id: id.clone(), stage: "applying".into(), started: crate::containers::unix_now(), from_version: crate::update::VERSION.into(), to_version: rel.map(|a| a.manifest.version.clone()).unwrap_or_else(|| crate::update::VERSION.into()), old_kernel: running_kernel(), packages: pkgs.clone(), ..Default::default() };
    note(format!("{} package(s) to update{}", pkgs.len(), rel.map(|a| format!(", OG-Host {} -> {}", crate::update::VERSION, a.manifest.version)).unwrap_or_default()), &mut t, Some(task));
    ensure_grub_saved_default()?;
    // 1. the way back: /boot copy, then the root snapshot
    let updates = d.root.join("updates");
    std::fs::create_dir_all(&updates)?;
    let boot_copy = updates.join(format!("boot-{id}.tar"));
    run("tar", &["-C", "/boot", "--exclude=./efi", "--exclude=./og-host-update.json", "-cf", &boot_copy.to_string_lossy(), "."])?;
    t.boot_copy = boot_copy.to_string_lossy().into_owned();
    match root_thin_lv() {
        Some(lv) => {
            if pool_used_pct(&lv).is_some_and(|p| p > 85.0) {
                bail!("the system's thin pool is over 85 % full: not updating (free space or grow it first)");
            }
            let _ = run("sync", &[]);
            // the snapshot of the previous update goes; this one becomes "the previous system"
            let vg = lv.split('/').next().unwrap_or("og");
            for old in run("lvs", &["--noheadings", "-o", "lv_name", vg]).unwrap_or_default().split_whitespace().filter(|n| n.starts_with("root-pre-")) {
                let _ = run("lvremove", &["-q", "-y", &format!("{vg}/{old}")]);
            }
            let snap = format!("root-pre-{id}");
            run("lvcreate", &["-q", "-s", "-n", &snap, &lv])?;
            t.root_lv = lv.clone();
            t.snapshot = format!("{vg}/{snap}");
            note(format!("snapshot of the system: {}", t.snapshot), &mut t, Some(task));
        }
        None => note("the root is not an LVM thin volume (installed before 0.3): updating without a system snapshot", &mut t, Some(task)),
    }
    save(&t)?;
    // 2. apply
    let applied = (|| -> Result<()> {
        if !pkgs.is_empty() {
            crate::update::apt_run(task, true)?;
        }
        if let Some(a) = rel {
            crate::update::stage_release(&d.root, task, a)?;
        }
        Ok(())
    })();
    if let Err(e) = applied {
        t.error = format!("{e:#}");
        note(format!("the update failed: {}; rolling back", t.error), &mut t, Some(task));
        save(&t)?;
        return rollback(&mut t, "the update itself failed").map(|_| true);
    }
    // 3. kernel and reboot
    let newest = kernels().pop().unwrap_or_default();
    if !newest.is_empty() && cmp_versions(&newest, &t.old_kernel) == std::cmp::Ordering::Greater {
        t.new_kernel = newest.clone();
    }
    t.reboot = !t.new_kernel.is_empty() || Path::new("/var/run/reboot-required").exists();
    if t.reboot {
        // the old kernel stays GRUB's default until the new system proves itself
        if let Some(old) = grub_entry(&t.old_kernel) {
            run("grub-set-default", &[&old])?;
        }
        if !t.new_kernel.is_empty() {
            let e = grub_entry(&t.new_kernel).context("the new kernel isn't in GRUB's menu")?;
            run("grub-reboot", &[&e])?;
            note(format!("new kernel {}: tried once on the next boot (default stays {})", t.new_kernel, t.old_kernel), &mut t, Some(task));
        }
    }
    if let Some(g) = guard_bin(&d.root) {
        if let Err(e) = install_boot_check(&g, &d.root) {
            note(format!("boot check not installed: {e:#}"), &mut t, Some(task));
        }
    }
    t.stage = if t.reboot { "await-reboot".into() } else { "trial".into() };
    save(&t)?;
    if t.reboot {
        if reboot {
            note("rebooting into the updated system", &mut t, Some(task));
            save(&t)?;
            t.stage = "trial".into();
            save(&t)?;
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(3));
                let _ = Command::new("systemctl").arg("reboot").status();
            });
        } else {
            note("updated: reboot to finish (the update is checked after the reboot)", &mut t, Some(task));
        }
    } else {
        note("updated: restarting og-hostd to check it", &mut t, Some(task));
        crate::update::restart_soon();
    }
    Ok(true)
}

fn guard_bin(root: &Path) -> Option<PathBuf> {
    let g = root.join("updates/guard/og-hostd");
    if g.exists() {
        return Some(g);
    }
    // no release in this update: the current binary is the known-good one
    std::env::current_exe().ok()
}

/// Reboot now into an update that waits for it.
pub fn reboot_now() -> Result<()> {
    let mut t = state().context("no update waiting")?;
    if t.stage != "await-reboot" {
        bail!("no update waits for a reboot (stage {})", t.stage);
    }
    t.stage = "trial".into();
    save(&t)?;
    let _ = Command::new("systemctl").arg("reboot").status();
    Ok(())
}

/// Put the system back as it was before the update and reboot (blocking; the reboot follows).
pub fn rollback(t: &mut Txn, why: &str) -> Result<()> {
    note(format!("rolling back: {why}"), t, None);
    t.stage = "rolling-back".into();
    t.error = why.into();
    save(t)?;
    if !t.boot_copy.is_empty() && Path::new(&t.boot_copy).exists() {
        run("tar", &["-C", "/boot", "-xf", &t.boot_copy])?;
    }
    if let Some(old) = grub_entry(&t.old_kernel) {
        let _ = run("grub-set-default", &[&old]);
    }
    if !t.snapshot.is_empty() {
        // merges when the root is next activated (at boot)
        run("lvconvert", &["-q", "--merge", &t.snapshot])?;
        save(t)?;
        let _ = run("sync", &[]);
        let _ = Command::new("systemctl").arg("reboot").status();
    } else {
        // no snapshot: the release's binaries go back (the guard does that); packages stay
        t.stage = "failed".into();
        save(t)?;
    }
    Ok(())
}

/// At og-hostd's start (also on the boot after an update or a rollback).
pub fn on_start(root: &Path) {
    let Some(mut t) = state() else { return };
    if t.stage == "applying" {
        // the power went (or the machine was reset) while packages were being installed
        let _ = rollback(&mut t, "the update was interrupted while it was being applied");
        return;
    }
    if t.stage == "await-reboot" {
        // rebooted some other way: GRUB was set up for the trial already
        t.stage = "trial".into();
        let _ = save(&t);
    }
    match t.stage.as_str() {
        "rolling-back" => {
            // the merge happened at activation; this is the previous system again
            t.stage = "rolled-back".into();
            t.finished = crate::containers::unix_now();
            t.snapshot.clear();
            let _ = std::fs::remove_file(&t.boot_copy);
            note(format!("back on the system from before the update (kernel {})", running_kernel()), &mut t, None);
            remove_boot_check();
            let _ = save(&t);
        }
        "trial" => {
            if !t.new_kernel.is_empty() && running_kernel() != t.new_kernel {
                let why = format!("the new kernel {} didn't boot (running {})", t.new_kernel, running_kernel());
                let _ = rollback(&mut t, &why);
                return;
            }
            let root = root.to_path_buf();
            std::thread::spawn(move || {
                std::thread::sleep(HEALTHY_AFTER);
                let Some(mut t) = state().filter(|t| t.stage == "trial") else { return };
                let net = run("ip", &["route", "show", "default"]).is_ok_and(|s| !s.trim().is_empty());
                let ssh = run("systemctl", &["is-active", "ssh"]).is_ok_and(|s| s.trim() == "active");
                if !net {
                    let _ = rollback(&mut t, "no network after the update");
                    return;
                }
                if !t.new_kernel.is_empty() {
                    if let Some(e) = grub_entry(&t.new_kernel) {
                        let _ = run("grub-set-default", &[&e]);
                    }
                }
                t.stage = "done".into();
                t.finished = crate::containers::unix_now();
                let _ = std::fs::remove_file(&t.boot_copy);
                note(format!("the update is kept (og-hostd {}, kernel {}{}); the previous system stays as {} until the next update", crate::update::VERSION, running_kernel(), if ssh { "" } else { ", ssh not active" }, if t.snapshot.is_empty() { "nothing".into() } else { t.snapshot.clone() }), &mut t, None);
                remove_boot_check();
                let _ = save(&t);
                let _ = root;
            });
        }
        _ => {}
    }
}

/// `og-hostd update-boot-check` (timer, 10 minutes after boot): a trial nobody confirmed.
pub fn boot_check() {
    if let Some(mut t) = state() {
        if t.stage == "trial" {
            let _ = rollback(&mut t, &format!("nothing confirmed the update within {CONFIRM_WITHIN_MIN} minutes of booting"));
        }
    }
}

/// From the release guard: og-hostd keeps failing during a system trial.
pub fn guard_tripped() -> bool {
    match state() {
        Some(mut t) if t.stage == "trial" && !t.snapshot.is_empty() => {
            let _ = rollback(&mut t, "og-hostd kept failing to start");
            true
        }
        _ => false,
    }
}

/// Go back to the previous system by hand (its snapshot is kept until the next update).
pub fn manual_rollback() -> Result<()> {
    let mut t = state().context("no update to undo")?;
    if t.snapshot.is_empty() || !Path::new(&format!("/dev/{}", t.snapshot)).exists() && run("lvs", &[&t.snapshot]).is_err() {
        bail!("the previous system's snapshot isn't there");
    }
    rollback(&mut t, "rolled back by hand")
}
