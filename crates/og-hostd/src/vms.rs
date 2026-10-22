//! Virtual machines: QEMU/KVM, one process per VM, run detached (they keep running when
//! og-hostd restarts) and driven over QMP.
//!
//! ```text
//! <root>/vms/<name>/vm.json      settings
//! <root>/vms/<name>/<disk>.qcow2 disks (or an absolute path to a raw device / image)
//! <root>/vms/<name>/efivars.fd   UEFI variables, tpm/ TPM state, console.log serial output
//! /run/og-host/vms/<name>/       qmp.sock, vnc.sock, qga.sock, swtpm.sock, pid files
//! ```
//!
//! Networks: an og-net network (tap on its bridge; the address comes from its IPAM through
//! OG-Host's DHCP, and published ports work like a container's), or `lan:<nic>` (macvtap:
//! the VM is on that LAN with its own MAC and the LAN's DHCP).
//!
//! `realistic` makes the VM look like a real PC to software that checks: no hypervisor CPU
//! bit or KVM signature, no Hyper-V enlightenments, no virtio devices (SATA disks with real
//! model names, an Intel NIC with an Intel MAC), no balloon, firmware/board SMBIOS and ACPI
//! OEM ids of a real board, an NEC USB controller. The emulated VGA still has a QEMU PCI id
//! (1234:1111): pass a GPU through for anything that looks at the display adapter.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Disk {
    pub id: String,
    /// virtio | scsi | sata | nvme
    pub bus: String,
    /// file name in the VM's directory, or an absolute path (image file or block device)
    pub file: String,
    /// none | writeback
    pub cache: String,
    pub discard: bool,
    pub ssd: bool,
    pub serial: String,
    pub model: String,
}

impl Default for Disk {
    fn default() -> Self {
        Disk { id: "disk0".into(), bus: "virtio".into(), file: String::new(), cache: "none".into(), discard: true, ssd: true, serial: String::new(), model: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Nic {
    pub id: String,
    /// virtio | e1000e | rtl8139
    pub model: String,
    /// og-net network name, or `lan:<host nic>`
    pub network: String,
    pub mac: String,
    /// published ports `host:guest[/udp]` (og-net networks only)
    pub ports: Vec<String>,
}

impl Default for Nic {
    fn default() -> Self {
        Nic { id: "net0".into(), model: "virtio".into(), network: og_net::DEFAULT_NETWORK.into(), mac: String::new(), ports: vec![] }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Smbios {
    pub bios_vendor: String,
    pub bios_version: String,
    pub bios_date: String,
    pub manufacturer: String,
    pub product: String,
    pub version: String,
    pub serial: String,
    pub sku: String,
    pub family: String,
    pub board_manufacturer: String,
    pub board_product: String,
    pub board_version: String,
    pub board_serial: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Vm {
    pub name: String,
    pub uuid: String,
    /// linux | windows | other
    pub os: String,
    pub cores: u32,
    /// MiB
    pub memory: u64,
    /// MiB the balloon may take it down to (0 = no balloon)
    pub balloon: u64,
    /// bios | uefi
    pub firmware: String,
    pub secure_boot: bool,
    pub tpm: bool,
    pub disks: Vec<Disk>,
    /// ISO files (names in <root>/iso or absolute paths); "" = empty drive
    pub cdroms: Vec<String>,
    pub nics: Vec<Nic>,
    /// std | virtio | qxl | none
    pub vga: String,
    /// PCI devices passed through (0000:01:00.0)
    pub hostpci: Vec<String>,
    /// USB devices passed through (vendor:product, hex)
    pub usb: Vec<String>,
    pub realistic: bool,
    pub smbios: Smbios,
    pub agent: bool,
    pub autostart: bool,
    /// device ids in boot order (disk0, cdrom0, net0)
    pub boot: Vec<String>,
    /// CPU cap in cores (0 = none)
    pub cpu_limit: f64,
    /// direct kernel boot (absolute paths on this server; empty = boot from disks/ISO)
    pub kernel: String,
    pub initrd: String,
    pub append: String,
    pub labels: BTreeMap<String, String>,
    pub comment: String,
    /// running | stopped
    pub desired: String,
    pub created: u64,
}

impl Default for Vm {
    fn default() -> Self {
        Vm {
            name: String::new(),
            uuid: String::new(),
            os: "linux".into(),
            cores: 2,
            memory: 2048,
            balloon: 0,
            firmware: "uefi".into(),
            secure_boot: false,
            tpm: false,
            disks: vec![],
            cdroms: vec![],
            nics: vec![Nic::default()],
            vga: "std".into(),
            hostpci: vec![],
            usb: vec![],
            realistic: false,
            smbios: Smbios::default(),
            agent: true,
            autostart: false,
            boot: vec![],
            cpu_limit: 0.0,
            kernel: String::new(),
            initrd: String::new(),
            append: String::new(),
            labels: BTreeMap::new(),
            comment: String::new(),
            desired: "stopped".into(),
            created: 0,
        }
    }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !n.starts_with('-') && !n.ends_with('-')
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// QEMU option values can't hold commas unescaped (`,,` is a literal comma).
fn qv(s: &str) -> String {
    s.replace(',', ",,")
}

fn plain(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn rand_hex(n: usize) -> String {
    use std::io::Read;
    let mut b = vec![0u8; n];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn new_uuid() -> String {
    let h = rand_hex(16);
    format!("{}-{}-4{}-a{}-{}", &h[..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
}

fn new_mac(realistic: bool) -> String {
    let h = rand_hex(3);
    // realistic: an Intel OUI (matches the e1000e); else locally administered
    let oui = if realistic { "3c:fd:fe" } else { "52:54:00" };
    format!("{oui}:{}:{}:{}", &h[..2], &h[2..4], &h[4..6])
}

/// A believable board for `realistic` VMs (values a common desktop board reports).
fn realistic_smbios(serial_seed: &str) -> Smbios {
    let s = serial_seed.to_ascii_uppercase();
    Smbios {
        bios_vendor: "American Megatrends Inc.".into(),
        bios_version: "3607".into(),
        bios_date: "03/22/2024".into(),
        manufacturer: "ASUS".into(),
        product: "System Product Name".into(),
        version: "System Version".into(),
        serial: "System Serial Number".into(),
        sku: "SKU".into(),
        family: "To be filled by O.E.M.".into(),
        board_manufacturer: "ASUSTeK COMPUTER INC.".into(),
        board_product: "ROG STRIX B550-F GAMING".into(),
        board_version: "Rev X.0x".into(),
        board_serial: format!("2109{}", &s[..s.len().min(11)]),
    }
}

pub struct Vms {
    pub root: PathBuf,
    run: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// running | paused | stopped | starting
    pub state: String,
    pub pid: Option<i32>,
    pub qmp: Option<String>,
}

impl Vms {
    pub fn new(root: &Path) -> Vms {
        Vms { root: root.join("vms"), run: PathBuf::from("/run/og-host/vms") }
    }

    pub fn dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn rundir(&self, name: &str) -> PathBuf {
        self.run.join(name)
    }

    pub fn iso_dir(&self) -> PathBuf {
        self.root.parent().unwrap().join("iso")
    }

    pub fn list(&self) -> Vec<Vm> {
        let mut out: Vec<Vm> = std::fs::read_dir(&self.root).into_iter().flatten().flatten().filter_map(|e| self.get(&e.file_name().to_string_lossy()).ok()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn get(&self, name: &str) -> Result<Vm> {
        if !valid_name(name) {
            bail!("no VM {name:?}");
        }
        let b = std::fs::read(self.dir(name).join("vm.json")).with_context(|| format!("no VM {name:?}"))?;
        Ok(serde_json::from_slice(&b)?)
    }

    fn save(&self, vm: &Vm) -> Result<()> {
        let p = self.dir(&vm.name).join("vm.json");
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(vm)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn disk_path(&self, vm: &Vm, d: &Disk) -> PathBuf {
        if d.file.starts_with('/') { PathBuf::from(&d.file) } else { self.dir(&vm.name).join(&d.file) }
    }

    fn iso_path(&self, iso: &str) -> PathBuf {
        if iso.starts_with('/') { PathBuf::from(iso) } else { self.iso_dir().join(iso) }
    }

    /// Check and fill in a VM's settings (new MACs, serials, firmware files exist, …).
    pub fn normalize(&self, vm: &mut Vm) -> Result<()> {
        if !valid_name(&vm.name) {
            bail!("VM names are lower-case letters, digits and - (up to 40)");
        }
        if !["linux", "windows", "other"].contains(&vm.os.as_str()) {
            bail!("os is linux, windows or other");
        }
        if vm.cores == 0 || vm.cores > 256 {
            bail!("1 to 256 cores");
        }
        if vm.memory < 64 || vm.memory > 4 * 1024 * 1024 {
            bail!("memory is 64 MiB to 4 TiB");
        }
        if vm.balloon > vm.memory {
            vm.balloon = vm.memory;
        }
        if !["bios", "uefi"].contains(&vm.firmware.as_str()) {
            bail!("firmware is bios or uefi");
        }
        if vm.secure_boot && vm.firmware != "uefi" {
            bail!("Secure Boot needs UEFI firmware");
        }
        if !["std", "virtio", "qxl", "none"].contains(&vm.vga.as_str()) {
            bail!("display is std, virtio, qxl or none");
        }
        if vm.uuid.is_empty() {
            vm.uuid = new_uuid();
        }
        if !vm.uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') || vm.uuid.len() != 36 {
            bail!("bad uuid");
        }
        let mut ids = std::collections::HashSet::new();
        for d in &mut vm.disks {
            if !valid_id(&d.id) || !ids.insert(d.id.clone()) {
                bail!("disk ids are short, unique, letters and digits");
            }
            if !["virtio", "scsi", "sata", "nvme"].contains(&d.bus.as_str()) {
                bail!("disk bus is virtio, scsi, sata or nvme");
            }
            if !["none", "writeback"].contains(&d.cache.as_str()) {
                bail!("disk cache is none or writeback");
            }
            if d.file.is_empty() || d.file.contains("..") || (!d.file.starts_with('/') && d.file.contains('/')) {
                bail!("disk {}: bad file", d.id);
            }
            if vm.realistic && d.bus == "virtio" {
                d.bus = "sata".into();
            }
            if d.serial.is_empty() {
                d.serial = if vm.realistic { format!("S6B{}", rand_hex(5).to_ascii_uppercase()) } else { format!("og{}", rand_hex(6)) };
            }
            if vm.realistic && d.model.is_empty() {
                d.model = "Samsung SSD 870 EVO 1TB".into();
            }
            d.serial = plain(&d.serial, 20);
            d.model = plain(&d.model, 40);
        }
        for (i, c) in vm.cdroms.iter().enumerate() {
            if !c.is_empty() && (c.contains("..") || (!c.starts_with('/') && c.contains('/'))) {
                bail!("cdrom{i}: bad ISO name");
            }
        }
        for n in &mut vm.nics {
            if !valid_id(&n.id) || !ids.insert(n.id.clone()) {
                bail!("network card ids are short, unique, letters and digits");
            }
            if vm.realistic && n.model == "virtio" {
                n.model = "e1000e".into();
            }
            if !["virtio", "e1000e", "rtl8139"].contains(&n.model.as_str()) {
                bail!("network card model is virtio, e1000e or rtl8139");
            }
            if n.mac.is_empty() {
                n.mac = new_mac(vm.realistic);
            }
            if !og_net::valid_mac(&n.mac) {
                bail!("bad MAC {}", n.mac);
            }
            if let Some(nic) = n.network.strip_prefix("lan:") {
                if nic.is_empty() || !Path::new("/sys/class/net").join(nic).exists() {
                    bail!("no network interface {nic:?} on this server");
                }
                if !n.ports.is_empty() {
                    bail!("published ports only work on OG-Host networks (the VM has its own LAN address)");
                }
            }
            for p in &n.ports {
                og_net::PortMap::parse(p).map_err(|e| anyhow::anyhow!("port {p}: {e}"))?;
            }
        }
        for p in &vm.hostpci {
            let ok = p.len() == 12 && p.chars().enumerate().all(|(i, c)| if [4, 7].contains(&i) { c == ':' } else if i == 10 { c == '.' } else { c.is_ascii_hexdigit() });
            if !ok || !Path::new("/sys/bus/pci/devices").join(p).exists() {
                bail!("no PCI device {p} (use the full address, e.g. 0000:01:00.0)");
            }
        }
        for u in &vm.usb {
            let ok = u.len() == 9 && u.as_bytes()[4] == b':' && u.chars().enumerate().all(|(i, c)| i == 4 || c.is_ascii_hexdigit());
            if !ok {
                bail!("USB devices look like 046d:c52b");
            }
        }
        if vm.realistic {
            vm.balloon = 0;
            vm.agent = false;
            if vm.smbios == Smbios::default() {
                vm.smbios = realistic_smbios(&rand_hex(6));
            }
            if vm.vga == "virtio" || vm.vga == "qxl" {
                vm.vga = "std".into();
            }
        }
        let fields = [&mut vm.smbios.bios_vendor, &mut vm.smbios.bios_version, &mut vm.smbios.bios_date, &mut vm.smbios.manufacturer, &mut vm.smbios.product, &mut vm.smbios.version, &mut vm.smbios.serial, &mut vm.smbios.sku, &mut vm.smbios.family, &mut vm.smbios.board_manufacturer, &mut vm.smbios.board_product, &mut vm.smbios.board_version, &mut vm.smbios.board_serial];
        for f in fields {
            *f = plain(f, 64);
        }
        let known: Vec<String> = vm.disks.iter().map(|d| d.id.clone()).chain((0..vm.cdroms.len()).map(|i| format!("cdrom{i}"))).chain(vm.nics.iter().map(|n| n.id.clone())).collect();
        vm.boot.retain(|b| known.contains(b));
        if vm.boot.is_empty() {
            vm.boot = known.iter().filter(|k| !k.starts_with("net")).cloned().collect();
            // a fresh VM with an installer ISO boots it first
            if !vm.cdroms.is_empty() && !vm.cdroms[0].is_empty() {
                vm.boot.retain(|b| b != "cdrom0");
                vm.boot.insert(0, "cdrom0".into());
            }
        }
        for f in [&vm.kernel, &vm.initrd] {
            if !f.is_empty() && (!f.starts_with('/') || f.contains("..") || !Path::new(f).is_file()) {
                bail!("kernel and initrd are absolute paths to files on this server");
            }
        }
        if !vm.initrd.is_empty() && vm.kernel.is_empty() {
            bail!("an initrd needs a kernel");
        }
        vm.append = plain(&vm.append, 1000);
        vm.comment = plain(&vm.comment, 300);
        if vm.cpu_limit < 0.0 || vm.cpu_limit > vm.cores as f64 {
            vm.cpu_limit = 0.0;
        }
        Ok(())
    }

    /// Create a VM: new disks of the given sizes (GiB) for disks whose file doesn't exist.
    pub fn create(&self, mut vm: Vm, sizes: &BTreeMap<String, u64>) -> Result<Vm> {
        if self.dir(&vm.name).exists() {
            bail!("a VM named {} exists already", vm.name);
        }
        vm.created = crate::containers::unix_now();
        vm.desired = "stopped".into();
        for d in &mut vm.disks {
            if d.file.is_empty() {
                d.file = format!("{}.qcow2", d.id);
            }
        }
        self.normalize(&mut vm)?;
        let dir = self.dir(&vm.name);
        std::fs::create_dir_all(&dir)?;
        let r = (|| -> Result<()> {
            for d in &vm.disks {
                let p = self.disk_path(&vm, d);
                if !p.exists() {
                    let gib = sizes.get(&d.id).copied().unwrap_or(32);
                    if gib == 0 || gib > 64 * 1024 {
                        bail!("disk {}: size 1 GiB to 64 TiB", d.id);
                    }
                    qemu_img(&["create", "-q", "-f", "qcow2", "-o", "lazy_refcounts=on,extended_l2=on,cluster_size=128k", &p.to_string_lossy(), &format!("{gib}G")])?;
                }
            }
            if vm.firmware == "uefi" {
                let vars = if vm.secure_boot { firmware_file(&["OVMF_VARS_4M.ms.fd", "OVMF_VARS.ms.fd"]) } else { firmware_file(&["OVMF_VARS_4M.fd", "OVMF_VARS.fd"]) }.context("UEFI firmware (ovmf) isn't installed")?;
                std::fs::copy(vars, dir.join("efivars.fd"))?;
            }
            self.save(&vm)
        })();
        if r.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        r.map(|_| vm)
    }

    /// Change settings (VM stopped, except name-independent things like comment/labels).
    pub fn update(&self, mut vm: Vm) -> Result<Vm> {
        let old = self.get(&vm.name)?;
        vm.created = old.created;
        vm.desired = old.desired.clone();
        vm.uuid = old.uuid.clone();
        self.normalize(&mut vm)?;
        let running = self.status(&vm.name).state != "stopped";
        let hw = |v: &Vm| {
            let mut v = v.clone();
            v.comment.clear();
            v.labels.clear();
            v.autostart = false;
            v.desired.clear();
            v
        };
        if running && hw(&old) != hw(&vm) {
            bail!("stop the VM to change its hardware");
        }
        for d in &vm.disks {
            if !self.disk_path(&vm, d).exists() {
                bail!("disk {}: {} doesn't exist (add new disks with the disk action)", d.id, self.disk_path(&vm, d).display());
            }
        }
        if vm.firmware == "uefi" && !self.dir(&vm.name).join("efivars.fd").exists() {
            let vars = firmware_file(&["OVMF_VARS_4M.ms.fd", "OVMF_VARS_4M.fd"]).context("UEFI firmware (ovmf) isn't installed")?;
            std::fs::copy(vars, self.dir(&vm.name).join("efivars.fd"))?;
        }
        self.save(&vm)?;
        Ok(vm)
    }

    pub fn add_disk(&self, name: &str, mut d: Disk, gib: u64) -> Result<Vm> {
        let mut vm = self.get(name)?;
        if self.status(name).state != "stopped" {
            bail!("stop the VM to add a disk");
        }
        if d.file.is_empty() {
            d.file = format!("{}.qcow2", d.id);
        }
        let p = self.disk_path(&vm, &d);
        if !p.exists() {
            if gib == 0 || gib > 64 * 1024 {
                bail!("size 1 GiB to 64 TiB");
            }
            qemu_img(&["create", "-q", "-f", "qcow2", "-o", "lazy_refcounts=on,extended_l2=on,cluster_size=128k", &p.to_string_lossy(), &format!("{gib}G")])?;
        }
        vm.disks.push(d);
        if let Err(e) = self.normalize(&mut vm) {
            bail!("{e}");
        }
        self.save(&vm)?;
        Ok(vm)
    }

    pub fn resize_disk(&self, name: &str, disk: &str, gib: u64) -> Result<()> {
        let vm = self.get(name)?;
        let d = vm.disks.iter().find(|d| d.id == disk).context("no such disk")?;
        if self.status(name).state != "stopped" {
            self.qmp(name, "block_resize", json!({"device": format!("drive-{}", d.id), "size": gib << 30}))?;
            return Ok(());
        }
        qemu_img(&["resize", "-q", &self.disk_path(&vm, d).to_string_lossy(), &format!("{gib}G")])
    }

    /// Remove a VM and its files (its disk files inside its directory; outside disks stay).
    pub fn remove(&self, net: &og_net::Net, name: &str) -> Result<()> {
        self.get(name)?;
        if self.status(name).state != "stopped" {
            bail!("stop the VM first");
        }
        let _ = net.detach(&owner(name));
        let _ = std::fs::remove_dir_all(self.rundir(name));
        std::fs::remove_dir_all(self.dir(name))?;
        Ok(())
    }

    pub fn set_desired(&self, name: &str, desired: &str) -> Result<()> {
        let mut vm = self.get(name)?;
        vm.desired = desired.into();
        self.save(&vm)
    }

    fn pid(&self, name: &str, file: &str) -> Option<i32> {
        let pid: i32 = std::fs::read_to_string(self.rundir(name).join(file)).ok()?.trim().parse().ok()?;
        // alive, and really ours (pid files outlive reboots in /run only if /run isn't tmpfs)
        let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let tag = format!("guest={name}");
        (cmd.split(|b| *b == 0).any(|a| a.starts_with(tag.as_bytes())) || file != "qemu.pid").then_some(pid)
    }

    pub fn status(&self, name: &str) -> Status {
        let Some(pid) = self.pid(name, "qemu.pid") else { return Status { state: "stopped".into(), pid: None, qmp: None } };
        let q = self.qmp(name, "query-status", json!({})).ok().and_then(|v| v["status"].as_str().map(String::from));
        let state = match q.as_deref() {
            Some("running") => "running",
            Some("paused") | Some("suspended") => "paused",
            Some("shutdown") => "stopped",
            Some("prelaunch") | Some("inmigrate") => "starting",
            Some(_) => "running",
            None => "running",
        };
        Status { state: state.into(), pid: Some(pid), qmp: q }
    }

    /// Send one QMP command and return its "return" value.
    pub fn qmp(&self, name: &str, cmd: &str, args: Value) -> Result<Value> {
        let s = UnixStream::connect(self.rundir(name).join("qmp.sock")).context("VM monitor not reachable")?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut w = s.try_clone()?;
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line)?; // greeting
        let mut send = |v: Value| -> Result<Value> {
            writeln!(w, "{v}")?;
            loop {
                line.clear();
                if r.read_line(&mut line)? == 0 {
                    bail!("VM monitor closed");
                }
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                if let Some(e) = v.get("error") {
                    bail!("{}", e["desc"].as_str().unwrap_or("QMP error"));
                }
                if let Some(ret) = v.get("return") {
                    return Ok(ret.clone());
                }
                // events: skip
            }
        };
        send(json!({"execute": "qmp_capabilities"}))?;
        let mut c = json!({"execute": cmd});
        if args.as_object().is_some_and(|o| !o.is_empty()) {
            c["arguments"] = args;
        }
        send(c)
    }

    /// The QEMU command line for a VM (with network devices prepared by the caller).
    pub fn command(&self, vm: &Vm, taps: &[(String, NetDev)]) -> Result<Vec<String>> {
        let dir = self.dir(&vm.name);
        let run = self.rundir(&vm.name);
        let mut a: Vec<String> = vec![];
        let mut push = |s: &[&str]| a.extend(s.iter().map(|x| x.to_string()));
        push(&["-name", &format!("guest={},debug-threads=on", vm.name)]);
        push(&["-uuid", &vm.uuid]);
        let mut machine = "q35,accel=kvm,vmport=off,hpet=off".to_string();
        if vm.secure_boot {
            machine.push_str(",smm=on");
        }
        if vm.realistic {
            machine.push_str(",x-oem-id=ALASKA,x-oem-table-id=A M I");
        }
        push(&["-machine", &machine]);
        let mut cpu = "host".to_string();
        if vm.realistic {
            cpu.push_str(",kvm=off,-hypervisor");
        } else if vm.os == "windows" {
            cpu.push_str(",hv_relaxed,hv_spinlocks=0x1fff,hv_vapic,hv_time,hv_vpindex,hv_synic,hv_stimer,hv_reset,hv_frequencies,hv_tlbflush,hv_ipi,hv_runtime");
        }
        push(&["-cpu", &cpu]);
        push(&["-smp", &format!("{},sockets=1,cores={},threads=1", vm.cores, vm.cores)]);
        push(&["-m", &format!("{}M", vm.memory)]);
        push(&["-nodefaults", "-no-user-config", "-daemonize"]);
        push(&["-pidfile", &run.join("qemu.pid").to_string_lossy()]);
        push(&["-qmp", &format!("unix:{},server=on,wait=off", qv(&run.join("qmp.sock").to_string_lossy()))]);
        push(&["-rtc", if vm.os == "windows" { "base=localtime,driftfix=slew" } else { "base=utc" }]);
        push(&["-global", "kvm-pit.lost_tick_policy=delay"]);
        push(&["-boot", "menu=on,splash-time=2000"]);
        if !vm.kernel.is_empty() {
            push(&["-kernel", &vm.kernel]);
            if !vm.initrd.is_empty() {
                push(&["-initrd", &vm.initrd]);
            }
            if !vm.append.is_empty() {
                push(&["-append", &vm.append]);
            }
        }
        if vm.firmware == "uefi" {
            let code = if vm.secure_boot { firmware_file(&["OVMF_CODE_4M.secboot.fd", "OVMF_CODE_4M.ms.fd", "OVMF_CODE.secboot.fd"]) } else { firmware_file(&["OVMF_CODE_4M.fd", "OVMF_CODE.fd"]) }.context("UEFI firmware (ovmf) isn't installed")?;
            push(&["-drive", &format!("if=pflash,format=raw,unit=0,readonly=on,file={}", qv(&code.to_string_lossy()))]);
            push(&["-drive", &format!("if=pflash,format=raw,unit=1,file={}", qv(&dir.join("efivars.fd").to_string_lossy()))]);
            if vm.secure_boot {
                push(&["-global", "driver=cfi.pflash01,property=secure,value=on"]);
            }
        }
        if vm.tpm {
            push(&["-chardev", &format!("socket,id=chrtpm,path={}", qv(&run.join("swtpm.sock").to_string_lossy()))]);
            push(&["-tpmdev", "emulator,id=tpm0,chardev=chrtpm"]);
            push(&["-device", "tpm-crb,tpmdev=tpm0"]);
        }
        // boot order
        let bootidx = |id: &str| vm.boot.iter().position(|b| b == id).map(|i| format!(",bootindex={}", i + 1)).unwrap_or_default();
        let mut sata_port = 0;
        let mut need_scsi = false;
        for d in &vm.disks {
            let path = self.disk_path(vm, d);
            let fmt = if path.extension().is_some_and(|e| e == "qcow2") { "qcow2" } else { "raw" };
            let cache = if d.cache == "writeback" { "cache.direct=off" } else { "cache.direct=on,aio=native" };
            let discard = if d.discard { ",discard=unmap,detect-zeroes=unmap" } else { "" };
            push(&["-blockdev", &format!("driver=file,filename={},node-name=file-{id},{cache}{discard}", qv(&path.to_string_lossy()), id = d.id)]);
            push(&["-blockdev", &format!("driver={fmt},file=file-{id},node-name=drive-{id}{discard}", id = d.id)]);
            let serial = format!(",serial={}", qv(&d.serial));
            let dev = match d.bus.as_str() {
                "virtio" => format!("virtio-blk-pci,drive=drive-{id},id={id}{serial}{}", bootidx(&d.id), id = d.id),
                "nvme" => format!("nvme,drive=drive-{id},id={id}{serial}{}", bootidx(&d.id), id = d.id),
                "scsi" => {
                    need_scsi = true;
                    format!("scsi-hd,bus=scsi0.0,drive=drive-{id},id={id}{serial}{}{}", if d.ssd { ",rotation_rate=1" } else { "" }, bootidx(&d.id), id = d.id)
                }
                _ => {
                    let port = sata_port;
                    sata_port += 1;
                    let model = if d.model.is_empty() { String::new() } else { format!(",model={}", qv(&d.model)) };
                    format!("ide-hd,bus=ide.{port},drive=drive-{id},id={id}{serial}{model}{}{}", if d.ssd { ",rotation_rate=1" } else { "" }, bootidx(&d.id), id = d.id)
                }
            };
            push(&["-device", &dev]);
        }
        if need_scsi {
            push(&["-device", "virtio-scsi-pci,id=scsi0"]);
        }
        for (i, iso) in vm.cdroms.iter().enumerate() {
            let port = sata_port;
            sata_port += 1;
            if port > 5 {
                bail!("too many SATA devices (6 at most)");
            }
            let id = format!("cdrom{i}");
            if iso.is_empty() {
                push(&["-device", &format!("ide-cd,bus=ide.{port},id={id}{}", bootidx(&id))]);
            } else {
                let p = self.iso_path(iso);
                if !p.exists() {
                    bail!("ISO {iso} not found");
                }
                push(&["-blockdev", &format!("driver=file,filename={},node-name=file-{id},read-only=on", qv(&p.to_string_lossy()))]);
                push(&["-blockdev", &format!("driver=raw,file=file-{id},node-name=drive-{id},read-only=on")]);
                let model = if vm.realistic { ",model=HL-DT-ST DVDRAM GH24NSD1" } else { "" };
                push(&["-device", &format!("ide-cd,bus=ide.{port},drive=drive-{id},id={id}{model}{}", bootidx(&id))]);
            }
        }
        if sata_port > 6 {
            bail!("too many SATA devices (6 at most)");
        }
        for (nic, dev) in vm.nics.iter().zip(taps.iter().map(|t| &t.1)) {
            let netdev = match dev {
                NetDev::Tap(ifname) => format!("tap,id=nd-{},ifname={},script=no,downscript=no,vhost=on", nic.id, ifname),
                NetDev::Fd(fd) => format!("tap,id=nd-{},fd={fd},vhost=on", nic.id),
            };
            push(&["-netdev", &netdev]);
            let model = match nic.model.as_str() {
                "e1000e" => "e1000e",
                "rtl8139" => "rtl8139",
                _ => "virtio-net-pci",
            };
            push(&["-device", &format!("{model},netdev=nd-{id},id={id},mac={}{}", nic.mac, bootidx(&nic.id), id = nic.id)]);
        }
        match vm.vga.as_str() {
            "none" => push(&["-vga", "none"]),
            "virtio" => push(&["-vga", "none", "-device", "virtio-vga"]),
            "qxl" => push(&["-vga", "none", "-device", "qxl-vga"]),
            _ => push(&["-vga", "none", "-device", "VGA"]),
        }
        if vm.vga != "none" {
            push(&["-vnc", &format!("unix:{}", qv(&run.join("vnc.sock").to_string_lossy()))]);
        } else {
            push(&["-display", "none"]);
        }
        // realistic: a NEC controller (QEMU's own xHCI has a Red Hat PCI id)
        push(&["-device", if vm.realistic { "nec-usb-xhci,id=xhci" } else { "qemu-xhci,id=xhci" }, "-device", "usb-tablet,bus=xhci.0"]);
        for u in &vm.usb {
            let (v, p) = u.split_once(':').unwrap();
            push(&["-device", &format!("usb-host,bus=xhci.0,vendorid=0x{v},productid=0x{p}")]);
        }
        for (i, p) in vm.hostpci.iter().enumerate() {
            let primary = i == 0 && vm.vga == "none";
            push(&["-device", &format!("vfio-pci,host={p},id=hostpci{i}{}", if primary { ",x-vga=on" } else { "" })]);
        }
        if vm.balloon > 0 && !vm.realistic {
            push(&["-device", "virtio-balloon-pci,id=balloon0,free-page-reporting=on"]);
        }
        if !vm.realistic {
            push(&["-device", "virtio-rng-pci"]);
        }
        if vm.agent && !vm.realistic {
            push(&["-chardev", &format!("socket,id=qga0,path={},server=on,wait=off", qv(&run.join("qga.sock").to_string_lossy()))]);
            push(&["-device", "virtio-serial-pci,id=vserial0", "-device", "virtserialport,bus=vserial0.0,chardev=qga0,name=org.qemu.guest_agent.0"]);
        }
        push(&["-chardev", &format!("socket,id=ser0,path={},server=on,wait=off,logfile={},logappend=on", qv(&run.join("serial.sock").to_string_lossy()), qv(&dir.join("console.log").to_string_lossy()))]);
        push(&["-serial", "chardev:ser0"]);
        let sm = &vm.smbios;
        let kv = |pairs: &[(&str, &str)]| pairs.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}={}", qv(v))).collect::<Vec<_>>().join(",");
        let t0 = kv(&[("vendor", &sm.bios_vendor), ("version", &sm.bios_version), ("date", &sm.bios_date)]);
        if !t0.is_empty() {
            push(&["-smbios", &format!("type=0,{t0}")]);
        }
        let t1 = kv(&[("manufacturer", &sm.manufacturer), ("product", &sm.product), ("version", &sm.version), ("serial", &sm.serial), ("sku", &sm.sku), ("family", &sm.family)]);
        if !t1.is_empty() {
            push(&["-smbios", &format!("type=1,{t1}")]);
        }
        let t2 = kv(&[("manufacturer", &sm.board_manufacturer), ("product", &sm.board_product), ("version", &sm.board_version), ("serial", &sm.board_serial)]);
        if !t2.is_empty() {
            push(&["-smbios", &format!("type=2,{t2}")]);
        }
        Ok(a)
    }
}

pub enum NetDev {
    Tap(String),
    /// an open macvtap /dev/tapN, passed to QEMU as this fd number
    Fd(i32),
}

/// og-net owner id of a VM (IPAM, taps, published ports).
pub fn owner(name: &str) -> String {
    format!("vm{}", &short_hash(name))
}

fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")[..10].to_string()
}

fn firmware_file(names: &[&str]) -> Option<PathBuf> {
    for d in ["/usr/share/OVMF", "/usr/share/ovmf", "/usr/share/edk2/ovmf", "/usr/share/qemu"] {
        for n in names {
            let p = Path::new(d).join(n);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

fn qemu_img(args: &[&str]) -> Result<()> {
    let out = Command::new("qemu-img").args(args).output().context("running qemu-img (is qemu-utils installed?)")?;
    if !out.status.success() {
        bail!("qemu-img {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn qemu_binary() -> Option<PathBuf> {
    ["/usr/bin/qemu-system-x86_64", "/usr/libexec/qemu-kvm", "/usr/local/bin/qemu-system-x86_64"].iter().map(PathBuf::from).find(|p| p.exists())
}

/// Host facts for the UI: can we run VMs, which NICs and PCI devices are there.
pub fn host_info() -> Value {
    let nics: Vec<String> = std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "lo" && !n.starts_with("og") && !n.starts_with("veth") && !n.starts_with("tap") && Path::new("/sys/class/net").join(n).join("device").exists() || n.starts_with("br") || n.starts_with("vmbr"))
        .collect();
    let mut pci = vec![];
    for e in std::fs::read_dir("/sys/bus/pci/devices").into_iter().flatten().flatten() {
        let p = e.path();
        let rd = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
        let class = rd("class");
        // GPUs, audio, USB controllers, NVMe, network: the usual passthrough candidates
        if ["0x03", "0x0403", "0x0c03", "0x0108", "0x02"].iter().any(|c| class.starts_with(c)) {
            let group = std::fs::read_link(p.join("iommu_group")).ok().and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()));
            let driver = std::fs::read_link(p.join("driver")).ok().and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()));
            pci.push(json!({"address": e.file_name().to_string_lossy(), "class": class, "vendor": rd("vendor"), "device": rd("device"), "iommu_group": group, "driver": driver}));
        }
    }
    json!({
        "kvm": Path::new("/dev/kvm").exists(),
        "qemu": qemu_binary().is_some(),
        "ovmf": firmware_file(&["OVMF_CODE_4M.fd", "OVMF_CODE.fd"]).is_some(),
        "swtpm": Path::new("/usr/bin/swtpm").exists(),
        "iommu": std::fs::read_dir("/sys/kernel/iommu_groups").map(|d| d.count() > 0).unwrap_or(false),
        "nics": nics,
        "pci": pci,
    })
}
