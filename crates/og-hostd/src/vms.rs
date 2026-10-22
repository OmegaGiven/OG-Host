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
    /// file name in the VM's directory, or an absolute path (image file or block device);
    /// empty when the disk is a pool volume
    pub file: String,
    /// storage pool volume `pool:name` (new VMs: "pool" alone makes one there)
    pub volume: String,
    /// none | writeback
    pub cache: String,
    pub discard: bool,
    pub ssd: bool,
    pub serial: String,
    pub model: String,
}

impl Default for Disk {
    fn default() -> Self {
        Disk { id: "disk0".into(), bus: "virtio".into(), file: String::new(), volume: String::new(), cache: "none".into(), discard: true, ssd: true, serial: String::new(), model: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Nic {
    pub id: String,
    /// virtio | e1000e | rtl8139
    pub model: String,
    /// og-net network name, `bridge:<host bridge>` or `lan:<host nic>` (macvtap)
    pub network: String,
    /// VLAN tag on a VLAN-aware host bridge
    pub vlan: Option<u16>,
    pub mac: String,
    /// published ports `host:guest[/udp]` (og-net networks only)
    pub ports: Vec<String>,
}

impl Default for Nic {
    fn default() -> Self {
        Nic { id: "net0".into(), model: "virtio".into(), network: og_net::DEFAULT_NETWORK.into(), vlan: None, mac: String::new(), ports: vec![] }
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
    /// a template: never started, cloned into new VMs (its disks then stay unchanged)
    pub template: bool,
    /// what may reach the VM (tap NICs: OG-Host networks and host bridges)
    pub firewall: crate::vmfw::VmFirewall,
    /// cloud-init settings (a NoCloud seed disc is made at every start)
    pub cloudinit: Option<CloudInit>,
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
            template: false,
            firewall: Default::default(),
            cloudinit: None,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CloudInit {
    pub user: String,
    /// SHA-512 crypt hash (set from a plain password by the API; never stored in plain)
    pub password_hash: String,
    pub ssh_keys: Vec<String>,
    /// "dhcp" or "static"
    pub ip: String,
    /// static: 192.168.1.50/24
    pub address: String,
    pub gateway: String,
    pub dns: Vec<String>,
    /// extra #cloud-config YAML merged in (packages, runcmd, …)
    pub extra: String,
    pub upgrade: bool,
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
    pub pools: crate::pools::Pools,
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
        Vms { root: root.join("vms"), run: PathBuf::from("/run/og-host/vms"), pools: crate::pools::Pools::new(root) }
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
        if !d.volume.is_empty() {
            return self.pools.path(&d.volume).unwrap_or_else(|_| PathBuf::from("/nonexistent"));
        }
        if d.file.starts_with('/') { PathBuf::from(&d.file) } else { self.dir(&vm.name).join(&d.file) }
    }

    /// A pool volume for a VM's disk (`pool` alone, or empty = the local pool).
    fn new_volume(&self, vm: &str, d: &Disk) -> String {
        let pool = if d.volume.is_empty() { "local" } else { d.volume.trim_end_matches(':') };
        let p = self.pools.get(pool).map(|p| p.kind).unwrap_or_default();
        format!("{pool}:vm-{vm}-{}{}", d.id, if p == "dir" { ".qcow2" } else { "" })
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
            if d.volume.is_empty() && (d.file.is_empty() || d.file.contains("..") || (!d.file.starts_with('/') && d.file.contains('/'))) {
                bail!("disk {}: bad file", d.id);
            }
            if !d.volume.is_empty() {
                self.pools.split(&d.volume)?;
                d.file.clear();
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
            if let Some(br) = n.network.strip_prefix("bridge:") {
                if br.is_empty() || !Path::new("/sys/class/net").join(br).join("bridge").exists() {
                    bail!("no bridge {br:?} on this server (Network → Bridges)");
                }
                if !n.ports.is_empty() {
                    bail!("published ports only work on OG-Host networks (on a bridge the VM has its own LAN address)");
                }
            } else if n.vlan.is_some() {
                bail!("VLAN tags need a host bridge (bridge:<name>)");
            }
            if n.vlan.is_some_and(|v| v == 0 || v > 4094) {
                bail!("VLAN ids are 1-4094");
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
        vm.firewall.validate()?;
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
        if !valid_name(&vm.name) {
            bail!("VM names are lower-case letters, digits and - (up to 40)");
        }
        // new disks: a volume in the asked pool (default: local)
        let mut fresh = vec![];
        for d in &mut vm.disks {
            if d.file.is_empty() && !d.volume.contains(':') || d.volume.ends_with(':') {
                d.volume = self.new_volume(&vm.name, d);
                fresh.push(d.id.clone());
            }
        }
        self.normalize(&mut vm)?;
        let dir = self.dir(&vm.name);
        std::fs::create_dir_all(&dir)?;
        let mut made: Vec<String> = vec![];
        let r = (|| -> Result<()> {
            for d in &vm.disks {
                if fresh.contains(&d.id) && !self.pools.exists(&d.volume) {
                    let gib = sizes.get(&d.id).copied().unwrap_or(32);
                    self.pools.create(&d.volume, gib).with_context(|| format!("disk {}", d.id))?;
                    made.push(d.volume.clone());
                } else if !self.disk_path(&vm, d).exists() {
                    bail!("disk {}: {} doesn't exist", d.id, self.disk_path(&vm, d).display());
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
            for v in made {
                let _ = self.pools.delete(&v);
            }
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
            v.firewall = Default::default();
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
        if d.file.is_empty() && !d.volume.contains(':') || d.volume.ends_with(':') {
            d.volume = self.new_volume(&vm.name, &d);
            self.pools.create(&d.volume, gib)?;
        } else if !self.disk_path(&vm, &d).exists() {
            bail!("{} doesn't exist", self.disk_path(&vm, &d).display());
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
        if !d.volume.is_empty() {
            return self.pools.resize(&d.volume, gib);
        }
        qemu_img(&["resize", "-q", &self.disk_path(&vm, d).to_string_lossy(), &format!("{gib}G")])
    }

    /// Remove a VM and its files (its disk files inside its directory; outside disks stay).
    pub fn remove(&self, net: &og_net::Net, name: &str) -> Result<()> {
        self.get(name)?;
        if self.status(name).state != "stopped" {
            bail!("stop the VM first");
        }
        let vm = self.get(name)?;
        let _ = net.detach(&owner(name));
        for i in 0..vm.nics.len() {
            let _ = net.detach(&crate::vmrun::nic_owner(name, i));
        }
        let _ = std::fs::remove_dir_all(self.rundir(name));
        // its own volumes (vm-<name>-…); volumes it only uses stay
        for d in &vm.disks {
            if d.volume.split(':').nth(1).is_some_and(|n| n.starts_with(&format!("vm-{name}-"))) {
                if let Err(e) = self.pools.delete(&d.volume) {
                    eprintln!("og-hostd: removing {}: {e:#}", d.volume);
                }
            }
        }
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
        if vm.disks.iter().any(|d| d.bus == "scsi") {
            push(&["-device", "virtio-scsi-pci,id=scsi0"]);
        }
        for d in &vm.disks {
            let path = self.disk_path(vm, d);
            let fmt = if path.extension().is_some_and(|e| e == "qcow2") { "qcow2" } else { "raw" };
            let cache = if d.cache == "writeback" { "cache.direct=off" } else { "cache.direct=on,aio=native" };
            let discard = if d.discard { ",discard=unmap,detect-zeroes=unmap" } else { "" };
            let proto = if std::fs::metadata(&path).is_ok_and(|m| { use std::os::unix::fs::FileTypeExt; m.file_type().is_block_device() }) { "host_device" } else { "file" };
            push(&["-blockdev", &format!("driver={proto},filename={},node-name=file-{id},{cache}{discard}", qv(&path.to_string_lossy()), id = d.id)]);
            push(&["-blockdev", &format!("driver={fmt},file=file-{id},node-name=drive-{id}{discard}", id = d.id)]);
            let serial = format!(",serial={}", qv(&d.serial));
            let dev = match d.bus.as_str() {
                "virtio" => format!("virtio-blk-pci,drive=drive-{id},id={id}{serial}{}", bootidx(&d.id), id = d.id),
                "nvme" => format!("nvme,drive=drive-{id},id={id}{serial}{}", bootidx(&d.id), id = d.id),
                "scsi" => {
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
        if vm.cloudinit.is_some() {
            let seed = self.dir(&vm.name).join("cloudinit.iso");
            if seed.exists() {
                if sata_port > 5 {
                    bail!("too many SATA devices for the cloud-init disc (6 at most)");
                }
                push(&["-blockdev", &format!("driver=file,filename={},node-name=file-cidata,read-only=on", qv(&seed.to_string_lossy()))]);
                push(&["-blockdev", "driver=raw,file=file-cidata,node-name=drive-cidata,read-only=on"]);
                push(&["-device", &format!("ide-cd,bus=ide.{sata_port},drive=drive-cidata,id=cidata")]);
                sata_port += 1;
            }
        }
        if sata_port > 6 {
            bail!("too many SATA devices (6 at most)");
        }
        // in-kernel virtio-net backend when there is one (faster), else QEMU's own
        if !Path::new("/dev/vhost-net").exists() {
            let _ = Command::new("modprobe").arg("vhost_net").status();
        }
        let vhost = if Path::new("/dev/vhost-net").exists() { "on" } else { "off" };
        for (nic, dev) in vm.nics.iter().zip(taps.iter().map(|t| &t.1)) {
            let netdev = match dev {
                NetDev::Tap(ifname) => format!("tap,id=nd-{},ifname={},script=no,downscript=no,vhost={vhost}", nic.id, ifname),
                NetDev::Fd(fd) => format!("tap,id=nd-{},fd={fd},vhost={vhost}", nic.id),
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

impl Vms {
    /// Write the NoCloud seed disc (user-data, meta-data, network-config) for a VM.
    pub fn cloudinit_seed(&self, vm: &Vm) -> Result<()> {
        let Some(c) = &vm.cloudinit else { return Ok(()) };
        let dir = self.dir(&vm.name);
        let tmp = dir.join(".cidata");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let yaml_str = |s: &str| serde_json::to_string(s).unwrap_or_default(); // JSON strings are valid YAML
        let mut ud = String::from("#cloud-config\n");
        ud.push_str(&format!("hostname: {}\nfqdn: {}\nmanage_etc_hosts: true\n", vm.name, vm.name));
        if !c.user.is_empty() {
            ud.push_str(&format!("users:\n  - name: {}\n    groups: [sudo, wheel, adm]\n    shell: /bin/bash\n    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n    lock_passwd: {}\n", yaml_str(&c.user), c.password_hash.is_empty()));
            if !c.password_hash.is_empty() {
                ud.push_str(&format!("    passwd: {}\n", yaml_str(&c.password_hash)));
            }
            if !c.ssh_keys.is_empty() {
                ud.push_str("    ssh_authorized_keys:\n");
                for k in &c.ssh_keys {
                    ud.push_str(&format!("      - {}\n", yaml_str(k.trim())));
                }
            }
        }
        ud.push_str(&format!("ssh_pwauth: {}\n", !c.password_hash.is_empty()));
        if c.upgrade {
            ud.push_str("package_update: true\npackage_upgrade: true\n");
        }
        if vm.agent {
            ud.push_str("packages: [qemu-guest-agent]\nruncmd:\n  - [systemctl, enable, --now, qemu-guest-agent]\n");
        }
        if !c.extra.trim().is_empty() {
            ud.push_str("# extra\n");
            ud.push_str(c.extra.trim_start_matches("#cloud-config").trim());
            ud.push('\n');
        }
        std::fs::write(tmp.join("user-data"), &ud)?;
        // a new instance id whenever the settings change, so cloud-init applies them again
        let id = {
            use sha2::Digest;
            let h = sha2::Sha256::digest(format!("{}{ud}{:?}{}", vm.uuid, c, vm.nics.iter().map(|n| n.mac.clone()).collect::<Vec<_>>().join(",")));
            format!("og-{}", hex::encode(&h[..6]))
        };
        std::fs::write(tmp.join("meta-data"), format!("instance-id: {id}\nlocal-hostname: {}\n", vm.name))?;
        let mut net = String::from("version: 2\nethernets:\n");
        for (i, n) in vm.nics.iter().enumerate() {
            net.push_str(&format!("  nic{i}:\n    match:\n      macaddress: \"{}\"\n", n.mac));
            if i == 0 && c.ip == "static" && !c.address.is_empty() {
                net.push_str(&format!("    addresses: [{}]\n", yaml_str(&c.address)));
                if !c.gateway.is_empty() {
                    net.push_str(&format!("    routes:\n      - to: default\n        via: {}\n", yaml_str(&c.gateway)));
                }
                if !c.dns.is_empty() {
                    net.push_str(&format!("    nameservers:\n      addresses: [{}]\n", c.dns.iter().map(|d| yaml_str(d)).collect::<Vec<_>>().join(", ")));
                }
            } else {
                net.push_str("    dhcp4: true\n");
            }
        }
        std::fs::write(tmp.join("network-config"), net)?;
        let iso = dir.join("cloudinit.iso");
        let tool = ["genisoimage", "mkisofs", "xorrisofs"].iter().find(|t| which(t)).context("cloud-init needs genisoimage or xorriso installed")?;
        let mut cmd = Command::new(tool);
        cmd.args(["-quiet", "-output"]).arg(&iso).args(["-volid", "cidata", "-joliet", "-rock"]).arg(tmp.join("user-data")).arg(tmp.join("meta-data")).arg(tmp.join("network-config"));
        let out = cmd.output()?;
        let _ = std::fs::remove_dir_all(&tmp);
        if !out.status.success() {
            bail!("making the cloud-init disc: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    /// Turn a (stopped) VM into a template: it can't start, its disks become read-only.
    pub fn make_template(&self, name: &str) -> Result<Vm> {
        let mut vm = self.get(name)?;
        if self.status(name).state != "stopped" {
            bail!("stop the VM first");
        }
        vm.template = true;
        vm.autostart = false;
        for d in &vm.disks {
            let p = self.disk_path(&vm, d);
            if p.is_file() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444))?;
            }
        }
        self.save(&vm)?;
        Ok(vm)
    }

    /// A new VM from an existing one (or template). `linked`: disks share the template's data
    /// (copy-on-write; only for templates, same pool). New identity: uuid, MACs, serials, TPM.
    pub fn clone_vm(&self, src: &str, dst: &str, linked: bool, pool: Option<&str>) -> Result<Vm> {
        let s = self.get(src)?;
        if !valid_name(dst) {
            bail!("VM names are lower-case letters, digits and - (up to 40)");
        }
        if self.dir(dst).exists() {
            bail!("a VM named {dst} exists already");
        }
        if linked && !s.template {
            bail!("linked clones need a template (a full clone works from any VM)");
        }
        if !s.template && self.status(src).state != "stopped" {
            bail!("stop {src} first (or make it a template)");
        }
        let mut vm = s.clone();
        vm.name = dst.into();
        vm.uuid.clear();
        vm.template = false;
        vm.desired = "stopped".into();
        vm.created = crate::containers::unix_now();
        vm.comment = format!("{} of {src}", if linked { "linked clone" } else { "clone" });
        for n in &mut vm.nics {
            n.mac.clear();
            n.ports.clear();
        }
        let dir = self.dir(dst);
        std::fs::create_dir_all(&dir)?;
        let mut made = vec![];
        let r = (|| -> Result<()> {
            for (i, d) in s.disks.iter().enumerate() {
                let target_pool = pool.map(String::from).unwrap_or_else(|| d.volume.split(':').next().filter(|p| !p.is_empty()).unwrap_or("local").to_string());
                let kind = self.pools.get(&target_pool)?.kind;
                let vol = format!("{target_pool}:vm-{dst}-{}{}", d.id, if kind == "dir" { ".qcow2" } else { "" });
                let srcvol = if d.volume.is_empty() {
                    // a disk file of the old kind: import it into the pool
                    self.pools.import(&self.disk_path(&s, d), &vol, None)?;
                    made.push(vol.clone());
                    vm.disks[i].volume = vol;
                    vm.disks[i].file.clear();
                    vm.disks[i].serial.clear();
                    continue;
                } else {
                    d.volume.clone()
                };
                self.pools.clone_vol(&srcvol, &vol, linked && target_pool == srcvol.split(':').next().unwrap_or(""))?;
                made.push(vol.clone());
                vm.disks[i].volume = vol;
                vm.disks[i].serial.clear();
            }
            if s.firmware == "uefi" && self.dir(src).join("efivars.fd").exists() {
                std::fs::copy(self.dir(src).join("efivars.fd"), dir.join("efivars.fd"))?;
            }
            // a fresh TPM (a copy would be the same machine to anything that checks)
            self.normalize(&mut vm)?;
            self.save(&vm)
        })();
        if r.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            for v in made {
                let _ = self.pools.delete(&v);
            }
        }
        r.map(|_| vm)
    }
}

fn which(t: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(t).is_file()))
}

/// SHA-512 crypt of a password for cloud-init.
pub fn hash_password(pw: &str) -> Result<String> {
    if pw.chars().count() < 8 {
        bail!("use at least 8 characters");
    }
    sha_crypt::sha512_simple(pw, &sha_crypt::Sha512Params::default()).map_err(|e| anyhow::anyhow!("hashing: {e:?}"))
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
