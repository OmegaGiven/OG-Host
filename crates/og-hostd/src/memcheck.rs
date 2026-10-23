//! Living with imperfect RAM: find bad memory while the server runs and keep it out of use.
//!
//! `og-hostd memtest` fills free memory with test patterns (all at once, so every cell has to
//! hold its value while the rest is written), reads it back, and for each mismatch finds the
//! physical address (/proc/self/pagemap) and takes that page out of service right away
//! (/sys/devices/system/memory/soft_offline_page). Bad pages are remembered in
//! <root>/badram.json; `memmap=` boot parameters reserve the 2 MiB around each of them from the
//! next boot on, before anything can use them, and og-hostd offlines them again at start.
//!
//! Without ECC this can't catch everything (memory in use by programs isn't tested until it's
//! freed), but weak cells tend to cluster and to fail again, so each one found protects the
//! data that would have landed on it.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const PAGE: u64 = 4096;
/// What a boot-time reservation covers around each bad page.
pub const RESERVE: u64 = 2 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BadPage {
    /// physical address of the failing 64-bit word
    pub addr: u64,
    pub pattern: String,
    /// bits that read back wrong (expected XOR got)
    pub bits: String,
    pub found: u64,
    /// where it was found: "memtest" (og-hostd) or "boot" (the kernel's early memtest)
    pub by: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BadRam {
    pub pages: Vec<BadPage>,
    /// completed scans: (time, bytes tested, failures)
    #[serde(default)]
    pub scans: Vec<(u64, u64, usize)>,
}

fn file(root: &Path) -> PathBuf {
    root.join("badram.json")
}

pub fn load(root: &Path) -> BadRam {
    std::fs::read(file(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(root: &Path, b: &BadRam) -> Result<()> {
    let tmp = file(root).with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(b)?)?;
    std::fs::rename(&tmp, file(root))?;
    Ok(())
}

fn now() -> u64 {
    crate::containers::unix_now()
}

/// Add pages (merging with what's known); returns how many were new.
fn remember(root: &Path, found: &[BadPage], scanned: Option<(u64, usize)>) -> Result<usize> {
    let mut b = load(root);
    let mut new = 0;
    for p in found {
        if !b.pages.iter().any(|q| q.addr / PAGE == p.addr / PAGE) {
            b.pages.push(p.clone());
            new += 1;
        }
    }
    b.pages.sort_by_key(|p| p.addr);
    if let Some((bytes, fails)) = scanned {
        b.scans.push((now(), bytes, fails));
    }
    save(root, &b)?;
    Ok(new)
}

/// Take the page holding `addr` out of service now (its contents are moved elsewhere first).
pub fn offline(addr: u64) -> Result<()> {
    std::fs::write("/sys/devices/system/memory/soft_offline_page", format!("{:#x}", addr / PAGE * PAGE)).with_context(|| format!("soft-offlining {addr:#x}"))
}

/// Pages the kernel's early memtest (memtest=N) reserved on this boot.
pub fn from_boot_log() -> Vec<BadPage> {
    let out = std::process::Command::new("journalctl").args(["-k", "-b", "--no-pager", "-o", "cat", "--grep", "bad mem addr"]).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let mut v = Vec::new();
    for l in text.lines() {
        // "  aaaaaaaaaaaaaaaa bad mem addr 0x00000001797f29a0 - 0x00000001797f29a8 reserved"
        let f: Vec<&str> = l.split_whitespace().collect();
        let Some(i) = f.iter().position(|w| *w == "addr") else { continue };
        let Some(a) = f.get(i + 1).and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok()) else { continue };
        v.push(BadPage { addr: a, pattern: f.first().unwrap_or(&"").to_string(), bits: String::new(), found: now(), by: "boot".into() });
    }
    v
}

/// The `memmap=` reservations for the known bad pages: 2 MiB aligned blocks, merged.
pub fn reservations(b: &BadRam) -> Vec<(u64, u64)> {
    let mut blocks: BTreeMap<u64, ()> = BTreeMap::new();
    for p in &b.pages {
        blocks.insert(p.addr / RESERVE * RESERVE, ());
    }
    let mut out: Vec<(u64, u64)> = Vec::new();
    for s in blocks.keys() {
        match out.last_mut() {
            Some((start, len)) if *start + *len == *s => *len += RESERVE,
            _ => out.push((*s, RESERVE)),
        }
    }
    out
}

/// Sourced after og-host.cfg (which sets GRUB_CMDLINE_LINUX_DEFAULT), so it can add to it: the
/// glob sorts "zz-" after "og-host.cfg" ("og-host-x" would sort before it, "-" < ".").
const GRUB_FILE: &str = "/etc/default/grub.d/zz-og-host-badram.cfg";

/// Write the boot reservations (and, with `patterns`, the kernel's early memtest strength,
/// memtest=1..17) into GRUB's config and regenerate grub.cfg.
pub fn apply_boot(b: &BadRam, patterns: Option<u32>) -> Result<Vec<String>> {
    let mut args: Vec<String> = reservations(b).iter().map(|(s, l)| format!("memmap={}K$0x{:x}", l / 1024, s)).collect();
    let keep_patterns = std::fs::read_to_string(GRUB_FILE).ok().and_then(|t| t.split_whitespace().find_map(|w| w.trim_matches('"').strip_prefix("memtest=").map(String::from)));
    if let Some(p) = patterns.map(|p| p.to_string()).or(keep_patterns) {
        args.push(format!("memtest={p}"));
    }
    if args.is_empty() {
        let _ = std::fs::remove_file(GRUB_FILE);
    } else {
        // grub.cfg needs `\$`; in this shell-sourced file that's written `\\\$`
        let escaped: Vec<String> = args.iter().map(|a| a.replace('$', "\\\\\\$")).collect();
        std::fs::create_dir_all("/etc/default/grub.d")?;
        std::fs::write(
            GRUB_FILE,
            format!("# Written by og-hostd memtest: keeps known-bad RAM out of use (see {})\nGRUB_CMDLINE_LINUX_DEFAULT=\"$GRUB_CMDLINE_LINUX_DEFAULT {}\"\n", "badram.json", escaped.join(" ")),
        )?;
    }
    let st = std::process::Command::new("update-grub").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().context("running update-grub")?;
    if !st.success() {
        bail!("update-grub failed");
    }
    Ok(args)
}

/// At daemon start: take in what this boot's kernel memtest found; when that's new, extend
/// the boot reservations (only if they're managed here already, or the kernel found something).
pub fn on_start(root: &Path) {
    let found = from_boot_log();
    if found.is_empty() {
        return;
    }
    match remember(root, &found, None) {
        Ok(0) => {}
        Ok(n) => {
            eprintln!("og-hostd: this boot's memory test found {n} new bad memory location(s)");
            match apply_boot(&load(root), None) {
                Ok(args) => eprintln!("og-hostd: reserved from the next boot on: {}", args.join(" ")),
                Err(e) => eprintln!("og-hostd: updating the boot reservations: {e:#}"),
            }
        }
        Err(e) => eprintln!("og-hostd: recording bad memory: {e:#}"),
    }
}

/// At daemon start: offline every known bad page that this boot's reservations don't cover.
pub fn reapply(root: &Path) {
    let b = load(root);
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let mut n = 0;
    for p in &b.pages {
        let block = p.addr / RESERVE * RESERVE;
        if cmdline.contains(&format!("$0x{block:x}")) {
            continue; // reserved before boot
        }
        if offline(p.addr).is_ok() {
            n += 1;
        }
    }
    if n > 0 {
        eprintln!("og-hostd: took {n} known-bad memory page(s) out of service");
    }
}

fn pagemap_pfn(pm: &mut std::fs::File, vaddr: u64) -> Option<u64> {
    pm.seek(SeekFrom::Start(vaddr / PAGE * 8)).ok()?;
    let mut b = [0u8; 8];
    pm.read_exact(&mut b).ok()?;
    let e = u64::from_le_bytes(b);
    (e >> 63 == 1).then_some(e & ((1 << 55) - 1)).filter(|p| *p != 0)
}

fn meminfo(key: &str) -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix(key).and_then(|v| v.trim_start_matches(':').split_whitespace().next()?.parse::<u64>().ok()))
        .unwrap_or(0)
        * 1024
}

struct Region {
    ptr: *mut u64,
    words: usize,
}

impl Drop for Region {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.words * 8) };
    }
}

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// Test `bytes` of free memory, `passes` times. Offlines and remembers what fails.
pub fn run(root: &Path, bytes: u64, passes: u32, leave_free: u64) -> Result<Vec<BadPage>> {
    let avail = meminfo("MemAvailable");
    let bytes = if bytes == 0 { avail.saturating_sub(leave_free) } else { bytes.min(avail.saturating_sub(leave_free)) };
    let bytes = bytes / (2 << 20) * (2 << 20);
    if bytes < 64 << 20 {
        bail!("not enough free memory to test ({} MiB available, keeping {} MiB free)", avail >> 20, leave_free >> 20);
    }
    // filling memory makes the kernel drop caches, which can touch memory that's already been
    // corrupted: get everything written to disk first
    unsafe { libc::sync() };
    // low priority: SCHED_IDLE, so anything else that wants the CPU gets it first
    unsafe {
        let p = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &p);
    }
    eprintln!("testing {} MiB of free memory ({} MiB available), {passes} pass(es)", bytes >> 20, avail >> 20);
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), bytes as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE | libc::MAP_NORESERVE, -1, 0) };
    if ptr == libc::MAP_FAILED {
        bail!("mmap {} MiB: {}", bytes >> 20, std::io::Error::last_os_error());
    }
    let r = Region { ptr: ptr as *mut u64, words: (bytes / 8) as usize };
    // no huge pages: a failure maps to one 4 KiB page; locked so nothing moves while testing
    unsafe {
        libc::madvise(ptr, bytes as usize, libc::MADV_NOHUGEPAGE);
        libc::mlock(ptr, bytes as usize);
    }
    let mut pm = std::fs::File::open("/proc/self/pagemap").context("opening /proc/self/pagemap")?;
    let base = ptr as u64;
    let mut found: BTreeMap<u64, BadPage> = BTreeMap::new();
    let t0 = std::time::Instant::now();
    type Pat = (&'static str, Box<dyn Fn(usize) -> u64>);
    let seed = now() | 1;
    for pass in 0..passes {
        let s = seed.wrapping_add(pass as u64 * 0x9e37_79b9_7f4a_7c15);
        let pats: Vec<Pat> = vec![
            ("zeros", Box::new(|_| 0)),
            ("ones", Box::new(|_| !0)),
            ("aaaa", Box::new(|_| 0xaaaa_aaaa_aaaa_aaaa)),
            ("5555", Box::new(|_| 0x5555_5555_5555_5555)),
            ("checker", Box::new(|i| if i & 1 == 0 { 0xaaaa_aaaa_aaaa_aaaa } else { 0x5555_5555_5555_5555 })),
            ("address", Box::new(move |i| (base + i as u64 * 8) ^ s)),
            ("random", Box::new(move |i| xorshift(s ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)))),
            ("random-inv", Box::new(move |i| !xorshift(s ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)))),
        ];
        for (name, f) in &pats {
            for i in 0..r.words {
                unsafe { std::ptr::write_volatile(r.ptr.add(i), f(i)) };
            }
            for i in 0..r.words {
                let want = f(i);
                let got = unsafe { std::ptr::read_volatile(r.ptr.add(i)) };
                if got != want {
                    let v = base + i as u64 * 8;
                    let Some(pfn) = pagemap_pfn(&mut pm, v) else { continue };
                    let addr = pfn * PAGE + v % PAGE;
                    eprintln!("  BAD {addr:#014x} pattern {name}: wrote {want:016x} read {got:016x}");
                    found.entry(addr / PAGE).or_insert(BadPage { addr, pattern: name.to_string(), bits: format!("{:016x}", want ^ got), found: now(), by: "memtest".into() });
                }
            }
        }
        eprintln!("  pass {} done, {} bad page(s) so far, {:.0} s", pass + 1, found.len(), t0.elapsed().as_secs_f64());
    }
    drop(r);
    let bad: Vec<BadPage> = found.into_values().collect();
    for p in &bad {
        if let Err(e) = offline(p.addr) {
            eprintln!("  {e:#}");
        }
    }
    let new = remember(root, &bad, Some((bytes * passes as u64, bad.len())))?;
    eprintln!("{} bad page(s) found, {new} new; known now: {}", bad.len(), load(root).pages.len());
    Ok(bad)
}

/// `og-hostd memtest`: also takes in what the kernel's boot memtest found, and updates the
/// boot reservations when anything new turned up.
pub fn cli(root: &Path, gib: u64, passes: u32, leave_free_gib: u64, boot: bool, boot_memtest: Option<u32>) -> Result<()> {
    let from_boot = from_boot_log();
    let nb = remember(root, &from_boot, None)?;
    if nb > 0 {
        eprintln!("{nb} bad page(s) from this boot's kernel memtest added");
    }
    let before = load(root).pages.len();
    if passes > 0 {
        run(root, gib << 30, passes, leave_free_gib << 30)?;
    }
    let b = load(root);
    if boot || boot_memtest.is_some() {
        if boot_memtest.is_some_and(|p| !(1..=17).contains(&p)) {
            bail!("--boot-memtest takes 1 to 17 (patterns the kernel tests at boot)");
        }
        let args = apply_boot(&b, boot_memtest)?;
        eprintln!("boot options from the next boot on ({} MiB reserved): {}", reservations(&b).iter().map(|r| r.1).sum::<u64>() >> 20, if args.is_empty() { "none".into() } else { args.join(" ") });
    } else if b.pages.len() > before || nb > 0 {
        eprintln!("run with --boot to reserve them from the next boot on");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_merge() {
        let p = |a| BadPage { addr: a, pattern: String::new(), bits: String::new(), found: 0, by: String::new() };
        let b = BadRam { pages: vec![p(0x1797f29a0), p(0x1797f7e98), p(0x3f993c60), p(0x179800010)], scans: vec![] };
        let r = reservations(&b);
        assert_eq!(r, vec![(0x3f800000, RESERVE), (0x179600000, 2 * RESERVE)]);
    }
}
