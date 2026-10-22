//! The host's own networking: bridges (VMs and containers straight on the LAN, VLAN-aware if
//! wanted), bonds and VLANs, written as systemd-networkd files (`/etc/systemd/network/05-og-*`,
//! ahead of anything else there) and applied with commit-confirm: changes go back by
//! themselves after 90 s unless confirmed, so a wrong setting can't strand the server.
//! A bridge takes its first port's MAC, so DHCP keeps handing out the same address.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CONFIRM_SECS: u64 = 90;
const NETDIR: &str = "/etc/systemd/network";
const PREFIX: &str = "05-og-";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Addr {
    /// dhcp | static | none
    pub mode: String,
    /// static: 192.168.1.10/24
    pub cidr: String,
    pub gateway: String,
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Bridge {
    pub name: String,
    pub ports: Vec<String>,
    pub vlan_aware: bool,
    /// VLANs allowed on the ports when VLAN-aware (empty = 2-4094)
    pub vids: Vec<u16>,
    pub addr: Addr,
    pub mtu: u32,
    pub comment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Bond {
    pub name: String,
    pub slaves: Vec<String>,
    /// 802.3ad | active-backup | balance-alb | balance-xor | balance-rr
    pub mode: String,
    pub mtu: u32,
    /// address on the bond itself (if it isn't a bridge port)
    pub addr: Addr,
    pub comment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Vlan {
    pub name: String,
    pub parent: String,
    pub id: u16,
    pub addr: Addr,
    pub comment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub bridges: Vec<Bridge>,
    pub bonds: Vec<Bond>,
    pub vlans: Vec<Vlan>,
}

fn valid_if(s: &str) -> bool {
    !s.is_empty() && s.len() <= 15 && s.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) && !s.starts_with("og")
}

fn physical(n: &str) -> bool {
    Path::new("/sys/class/net").join(n).join("device").exists()
}

/// The DHCP identity a NIC's current lease uses (systemd-networkd lease file), as the
/// [DHCPv4] lines that make another interface (its bridge) ask the same way.
fn dhcp_identity(n: &str) -> String {
    let Some(idx) = std::fs::read_to_string(Path::new("/sys/class/net").join(n).join("ifindex")).ok() else { return "ClientIdentifier=mac\n".into() };
    let lease = std::fs::read_to_string(format!("/run/systemd/netif/leases/{}", idx.trim())).unwrap_or_default();
    let id = lease.lines().find_map(|l| l.strip_prefix("CLIENTID=")).unwrap_or("").to_string();
    // ff + IAID (4 bytes) + DUID: RFC 4361; 01 + MAC: the MAC
    if id.len() > 10 && id.starts_with("ff") {
        if let Ok(iaid) = u32::from_str_radix(&id[2..10], 16) {
            return format!("ClientIdentifier=duid\nIAID={iaid}\nDUIDType=raw\nDUIDRawData={}\n", id[10..].as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(":"));
        }
    }
    "ClientIdentifier=mac\n".into()
}

fn mac_of(n: &str) -> Option<String> {
    std::fs::read_to_string(Path::new("/sys/class/net").join(n).join("address")).ok().map(|s| s.trim().to_string()).filter(|m| m.len() == 17 && m != "00:00:00:00:00:00")
}

fn check_addr(a: &mut Addr, what: &str) -> Result<()> {
    if a.mode.is_empty() {
        a.mode = "none".into();
    }
    match a.mode.as_str() {
        "dhcp" | "none" => Ok(()),
        "static" => {
            let (ip, p) = a.cidr.split_once('/').with_context(|| format!("{what}: give the address as 192.168.1.10/24"))?;
            ip.parse::<std::net::IpAddr>().with_context(|| format!("{what}: bad address"))?;
            p.parse::<u8>().ok().filter(|p| *p <= 128).with_context(|| format!("{what}: bad prefix"))?;
            if !a.gateway.is_empty() {
                a.gateway.parse::<std::net::IpAddr>().with_context(|| format!("{what}: bad gateway"))?;
            }
            for d in &a.dns {
                d.parse::<std::net::IpAddr>().with_context(|| format!("{what}: bad DNS server {d}"))?;
            }
            Ok(())
        }
        _ => bail!("{what}: address is dhcp, static or none"),
    }
}

impl Config {
    pub fn validate(&mut self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        let mut used = std::collections::HashSet::new();
        for b in &mut self.bonds {
            if !valid_if(&b.name) || !names.insert(b.name.clone()) {
                bail!("bad or repeated name {}", b.name);
            }
            if !["802.3ad", "active-backup", "balance-alb", "balance-xor", "balance-rr", "balance-tlb", "broadcast"].contains(&b.mode.as_str()) {
                bail!("{}: bond mode is 802.3ad, active-backup, balance-alb, …", b.name);
            }
            if b.slaves.is_empty() {
                bail!("{}: a bond needs network cards", b.name);
            }
            for s in &b.slaves {
                if !physical(s) || !used.insert(s.clone()) {
                    bail!("{}: {s} isn't a free network card", b.name);
                }
            }
            check_addr(&mut b.addr, &b.name)?;
        }
        for v in &mut self.vlans {
            if !valid_if(&v.name) || !names.insert(v.name.clone()) {
                bail!("bad or repeated name {}", v.name);
            }
            if v.id == 0 || v.id > 4094 {
                bail!("{}: VLAN ids are 1-4094", v.name);
            }
            if !(physical(&v.parent) || self.bonds.iter().any(|b| b.name == v.parent)) {
                bail!("{}: no network card or bond {}", v.name, v.parent);
            }
            check_addr(&mut v.addr, &v.name)?;
        }
        for b in &mut self.bridges {
            if !valid_if(&b.name) || !names.insert(b.name.clone()) {
                bail!("bad or repeated name {}", b.name);
            }
            for p in &b.ports {
                let ok = physical(p) || self.bonds.iter().any(|x| &x.name == p) || self.vlans.iter().any(|x| &x.name == p);
                if !ok || !used.insert(p.clone()) {
                    bail!("{}: {p} isn't a free network card, bond or VLAN", b.name);
                }
            }
            if b.vids.iter().any(|v| *v == 0 || *v > 4094) {
                bail!("{}: VLAN ids are 1-4094", b.name);
            }
            if b.mtu != 0 && !(576..=9216).contains(&b.mtu) {
                bail!("{}: MTU 576-9216", b.name);
            }
            check_addr(&mut b.addr, &b.name)?;
            b.comment = b.comment.chars().filter(|c| !c.is_control()).take(100).collect();
        }
        Ok(())
    }

    /// networkd files: (file name, contents).
    pub fn files(&self) -> Vec<(String, String)> {
        let mut f = vec![];
        let addr = |a: &Addr| -> String {
            let mut s = String::new();
            match a.mode.as_str() {
                "dhcp" => s.push_str("DHCP=ipv4\nIPv6AcceptRA=yes\n"),
                "static" => {
                    s.push_str(&format!("Address={}\n", a.cidr));
                    if !a.gateway.is_empty() {
                        s.push_str(&format!("Gateway={}\n", a.gateway));
                    }
                    for d in &a.dns {
                        s.push_str(&format!("DNS={d}\n"));
                    }
                }
                _ => s.push_str("LinkLocalAddressing=no\nIPv6AcceptRA=no\n"),
            }
            s
        };
        let mtu = |m: u32| if m > 0 { format!("[Link]\nMTUBytes={m}\n") } else { String::new() };
        for b in &self.bonds {
            f.push((format!("{PREFIX}10-{}.netdev", b.name), format!("# OG-Host\n[NetDev]\nName={}\nKind=bond\n{}\n[Bond]\nMode={}\nMIIMonitorSec=100ms\n{}", b.name, if b.mtu > 0 { format!("MTUBytes={}\n", b.mtu) } else { String::new() }, b.mode, if b.mode == "802.3ad" { "LACPTransmitRate=fast\nTransmitHashPolicy=layer3+4\n" } else { "" })));
            for s in &b.slaves {
                f.push((format!("{PREFIX}20-{s}.network"), format!("# OG-Host\n[Match]\nName={s}\n\n[Network]\nBond={}\n", b.name)));
            }
            if !self.bridges.iter().any(|x| x.ports.contains(&b.name)) {
                let vl: String = self.vlans.iter().filter(|v| v.parent == b.name).map(|v| format!("VLAN={}\n", v.name)).collect();
                f.push((format!("{PREFIX}30-{}.network", b.name), format!("# OG-Host\n[Match]\nName={}\n\n[Network]\n{}{vl}", b.name, addr(&b.addr))));
            }
        }
        for v in &self.vlans {
            f.push((format!("{PREFIX}10-{}.netdev", v.name), format!("# OG-Host\n[NetDev]\nName={}\nKind=vlan\n\n[VLAN]\nId={}\n", v.name, v.id)));
            if !self.bridges.iter().any(|x| x.ports.contains(&v.name)) {
                f.push((format!("{PREFIX}30-{}.network", v.name), format!("# OG-Host\n[Match]\nName={}\n\n[Network]\n{}", v.name, addr(&v.addr))));
            }
        }
        // a physical VLAN parent that isn't a bond or a bridge port still needs VLAN= lines
        for p in self.vlans.iter().map(|v| v.parent.clone()).collect::<std::collections::BTreeSet<_>>() {
            let is_bond = self.bonds.iter().any(|b| b.name == p);
            let in_bridge = self.bridges.iter().any(|b| b.ports.contains(&p));
            if !is_bond && !in_bridge {
                let vl: String = self.vlans.iter().filter(|v| v.parent == p).map(|v| format!("VLAN={}\n", v.name)).collect();
                f.push((format!("{PREFIX}25-{p}.network"), format!("# OG-Host\n[Match]\nName={p}\n\n[Network]\nDHCP=ipv4\n{vl}")));
            }
        }
        for b in &self.bridges {
            let first_mac = b.ports.first().and_then(|p| mac_of(p));
            let mut nd = format!("# OG-Host\n[NetDev]\nName={}\nKind=bridge\n", b.name);
            if let Some(m) = &first_mac {
                nd.push_str(&format!("MACAddress={m}\n"));
            }
            if b.mtu > 0 {
                nd.push_str(&format!("MTUBytes={}\n", b.mtu));
            }
            nd.push_str(&format!("\n[Bridge]\nSTP=no\nForwardDelaySec=0\nVLANFiltering={}\n", if b.vlan_aware { "yes" } else { "no" }));
            if b.vlan_aware {
                nd.push_str("DefaultPVID=1\n");
            }
            f.push((format!("{PREFIX}10-{}.netdev", b.name), nd));
            let vids = if b.vids.is_empty() { "VLAN=2-4094\n".to_string() } else { b.vids.iter().map(|v| format!("VLAN={v}\n")).collect() };
            for p in &b.ports {
                let mut s = format!("# OG-Host\n[Match]\nName={p}\n\n[Network]\nBridge={}\n", b.name);
                // a VLAN parent that's a bridge port keeps its VLANs
                for v in self.vlans.iter().filter(|v| &v.parent == p) {
                    s.push_str(&format!("VLAN={}\n", v.name));
                }
                if b.vlan_aware {
                    s.push_str(&format!("\n[BridgeVLAN]\nPVID=1\nEgressUntagged=1\n\n[BridgeVLAN]\n{vids}"));
                }
                s.push_str(&mtu(b.mtu));
                f.push((format!("{PREFIX}20-{p}.network"), s));
            }
            let mut s = format!("# OG-Host\n[Match]\nName={}\n\n[Network]\n{}ConfigureWithoutCarrier=yes\n", b.name, addr(&b.addr));
            if b.addr.mode == "dhcp" {
                // ask DHCP exactly as the first port did, so the address stays the same
                let ident = b.ports.first().map(|p| dhcp_identity(p)).unwrap_or_default();
                s.push_str(&format!("\n[DHCPv4]\n{ident}"));
            }
            if b.vlan_aware {
                s.push_str(&format!("\n[BridgeVLAN]\nPVID=1\nEgressUntagged=1\n\n[BridgeVLAN]\n{vids}"));
            }
            f.push((format!("{PREFIX}30-{}.network", b.name), s));
        }
        f
    }

    fn interfaces(&self) -> Vec<String> {
        let mut v: Vec<String> = vec![];
        for b in &self.bonds {
            v.push(b.name.clone());
            v.extend(b.slaves.iter().cloned());
        }
        for x in &self.vlans {
            v.push(x.name.clone());
            v.push(x.parent.clone());
        }
        for b in &self.bridges {
            v.push(b.name.clone());
            v.extend(b.ports.iter().cloned());
        }
        v.sort();
        v.dedup();
        v
    }
}

fn current_files() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = std::fs::read_dir(NETDIR)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(PREFIX).then(|| Some((n, std::fs::read_to_string(e.path()).ok()?))).flatten()
        })
        .collect();
    v.sort();
    v
}

fn write_files(files: &[(String, String)]) -> Result<()> {
    for (n, _) in current_files() {
        std::fs::remove_file(Path::new(NETDIR).join(n))?;
    }
    std::fs::create_dir_all(NETDIR)?;
    for (n, c) in files {
        std::fs::write(Path::new(NETDIR).join(n), c)?;
    }
    Ok(())
}

fn networkd_apply(ifaces: &[String], removed_virtual: &[String]) -> Result<()> {
    let st = Command::new("networkctl").arg("reload").status().context("running networkctl (OG-Host's networking needs systemd-networkd)")?;
    if !st.success() {
        bail!("networkctl reload failed");
    }
    // virtual devices no longer configured: delete them (networkd leaves them)
    for n in removed_virtual {
        let _ = Command::new("ip").args(["link", "del", n]).status();
    }
    let existing: Vec<&String> = ifaces.iter().filter(|n| Path::new("/sys/class/net").join(n).exists()).collect();
    if !existing.is_empty() {
        let _ = Command::new("networkctl").arg("reconfigure").args(existing).status();
    }
    Ok(())
}

struct Pending {
    previous: Vec<(String, String)>,
    prev_cfg: Config,
    deadline: Instant,
    generation: u64,
}

pub struct HostNet {
    file: PathBuf,
    state: Mutex<(Option<Pending>, u64)>,
}

impl HostNet {
    pub fn open(root: &Path) -> HostNet {
        HostNet { file: root.join("hostnet.json"), state: Mutex::new((None, 0)) }
    }

    pub fn config(&self) -> Config {
        std::fs::read(&self.file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn status(&self) -> Value {
        let st = self.state.lock().unwrap();
        let left = st.0.as_ref().map(|p| p.deadline.saturating_duration_since(Instant::now()).as_secs());
        // what the system has now
        let mut links = vec![];
        for e in std::fs::read_dir("/sys/class/net").into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n == "lo" || n.starts_with("og") || n.starts_with("veth") || n.starts_with("tap") {
                continue;
            }
            let p = e.path();
            let rd = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
            let kind = if p.join("bridge").exists() { "bridge" } else if p.join("bonding").exists() { "bond" } else if physical(&n) { "nic" } else { "virtual" };
            let master = std::fs::read_link(p.join("master")).ok().and_then(|l| l.file_name().map(|x| x.to_string_lossy().into_owned()));
            let addrs = Command::new("ip").args(["-o", "addr", "show", "dev", &n]).output().map(|o| String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.split_whitespace().skip_while(|w| *w != "inet" && *w != "inet6").nth(1).map(String::from)).filter(|a| !a.starts_with("fe80")).collect::<Vec<_>>()).unwrap_or_default();
            links.push(json!({"name": n, "kind": kind, "mac": rd("address"), "state": rd("operstate"), "speed": rd("speed").parse::<i64>().ok().filter(|s| *s > 0), "mtu": rd("mtu").parse::<u32>().ok(), "master": master, "addresses": addrs}));
        }
        links.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let networkd = Command::new("systemctl").args(["is-active", "--quiet", "systemd-networkd"]).status().is_ok_and(|s| s.success());
        json!({"config": self.config(), "links": links, "pending": left.map(|s| json!({"seconds_left": s})), "networkd": networkd, "files": self.config().files().iter().map(|(n, c)| json!({"name": n, "content": c})).collect::<Vec<_>>()})
    }

    pub fn apply(&self, mut new: Config) -> Result<u64> {
        new.validate()?;
        if !Command::new("systemctl").args(["is-active", "--quiet", "systemd-networkd"]).status().is_ok_and(|s| s.success()) {
            bail!("this server's network isn't run by systemd-networkd; OG-Host manages host networking through it (the OG-Host OS uses it)");
        }
        let mut st = self.state.lock().unwrap();
        let (previous, prev_cfg) = match st.0.take() {
            Some(p) => (p.previous, p.prev_cfg),
            None => (current_files(), self.config()),
        };
        let mut ifaces = new.interfaces();
        ifaces.extend(prev_cfg.interfaces());
        let removed: Vec<String> = prev_cfg.bridges.iter().map(|b| b.name.clone()).chain(prev_cfg.bonds.iter().map(|b| b.name.clone())).chain(prev_cfg.vlans.iter().map(|v| v.name.clone())).filter(|n| !new.interfaces().contains(n)).collect();
        write_files(&new.files())?;
        std::fs::write(&self.file, serde_json::to_vec_pretty(&new)?)?;
        networkd_apply(&ifaces, &removed)?;
        st.1 += 1;
        let generation = st.1;
        st.0 = Some(Pending { previous, prev_cfg, deadline: Instant::now() + Duration::from_secs(CONFIRM_SECS), generation });
        Ok(generation)
    }

    pub fn confirm(&self) -> Result<()> {
        if self.state.lock().unwrap().0.take().is_none() {
            bail!("nothing to confirm");
        }
        Ok(())
    }

    pub fn rollback(&self, generation: Option<u64>) -> Result<bool> {
        let mut st = self.state.lock().unwrap();
        let Some(p) = st.0.as_ref() else { return Ok(false) };
        if generation.is_some_and(|g| g != p.generation) {
            return Ok(false);
        }
        let p = st.0.take().unwrap();
        let cur = self.config();
        let mut ifaces = cur.interfaces();
        ifaces.extend(p.prev_cfg.interfaces());
        let removed: Vec<String> = cur.bridges.iter().map(|b| b.name.clone()).chain(cur.bonds.iter().map(|b| b.name.clone())).chain(cur.vlans.iter().map(|v| v.name.clone())).filter(|n| !p.prev_cfg.interfaces().contains(n)).collect();
        write_files(&p.previous)?;
        std::fs::write(&self.file, serde_json::to_vec_pretty(&p.prev_cfg)?)?;
        // ports leave the bridge before it goes, so they get their own config back
        for n in &removed {
            let _ = Command::new("ip").args(["link", "set", n, "down"]).status();
        }
        networkd_apply(&ifaces, &removed)?;
        Ok(true)
    }
}

/// Plug a VM's tap into a host bridge, on a VLAN if the bridge is VLAN-aware.
pub fn attach_tap(tap: &str, bridge: &str, vlan: Option<u16>) -> Result<()> {
    let run = |args: &[&str]| -> Result<()> {
        let out = Command::new(args[0]).args(&args[1..]).output()?;
        if !out.status.success() {
            bail!("{}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    };
    if !Path::new("/sys/class/net").join(bridge).join("bridge").exists() {
        bail!("no bridge {bridge} on this server");
    }
    if !Path::new("/sys/class/net").join(tap).exists() {
        run(&["ip", "tuntap", "add", "dev", tap, "mode", "tap", "vnet_hdr"])?;
    }
    run(&["ip", "link", "set", tap, "master", bridge, "up"])?;
    let filtering = std::fs::read_to_string(Path::new("/sys/class/net").join(bridge).join("bridge/vlan_filtering")).map(|s| s.trim() == "1").unwrap_or(false);
    if filtering {
        if let Some(v) = vlan {
            let _ = run(&["bridge", "vlan", "del", "dev", tap, "vid", "1"]);
            run(&["bridge", "vlan", "add", "dev", tap, "vid", &v.to_string(), "pvid", "untagged"])?;
        }
    } else if vlan.is_some() {
        bail!("bridge {bridge} isn't VLAN-aware: turn that on to put VMs on VLANs");
    }
    Ok(())
}
