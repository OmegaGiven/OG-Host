//! Host firewall: what may reach this server (the input hook), in its own nftables table
//! `inet og_host_fw`, separate from og-net's container NAT/isolation table.
//!
//! Always allowed: replies to our own connections, loopback, IPv6 neighbour discovery, DHCP
//! replies, and SSH plus the web UI ports from the management networks (default: anywhere),
//! so a rule list can't lock the admin out. Then the user's rules, first match wins, then the
//! default policy. Optional: containers reaching the host, ping.
//!
//! Changing it is commit-confirm: the new rules apply at once and roll back after 60 s
//! unless confirmed. Before applying, the rules are evaluated for the caller's own address
//! and a change that would cut them off is refused unless forced. The confirmed config is in
//! `<root>/firewall.json` and applied at start.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const TABLE: &str = "og_host_fw";
pub const CONFIRM_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    /// drop | reject | accept
    pub policy: String,
    pub allow_ping: bool,
    /// containers on og-net bridges may reach services on the host
    pub allow_containers: bool,
    /// CIDRs (or addresses) that always reach SSH and the UI; empty = anywhere
    pub management: Vec<String>,
    pub rules: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Config { enabled: false, policy: "drop".into(), allow_ping: true, allow_containers: true, management: vec![], rules: vec![] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Rule {
    pub enabled: bool,
    /// accept | drop | reject
    pub action: String,
    /// tcp | udp | tcp/udp | icmp | "" (any)
    pub proto: String,
    /// "22", "80,443", "8000-8100" (tcp/udp only)
    pub dport: String,
    /// address or CIDR, IPv4 or IPv6; "" = anywhere
    pub source: String,
    /// incoming interface, e.g. eth0 or tailscale0 ("*" wildcard at the end); "" = any
    pub iface: String,
    pub comment: String,
}

impl Default for Rule {
    fn default() -> Self {
        Rule { enabled: true, action: "accept".into(), proto: "tcp".into(), dport: String::new(), source: String::new(), iface: String::new(), comment: String::new() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Net {
    addr: IpAddr,
    prefix: u8,
}

impl Net {
    fn parse(s: &str) -> Result<Net> {
        let (a, p) = s.split_once('/').map(|(a, p)| (a, Some(p))).unwrap_or((s, None));
        let addr: IpAddr = a.trim().parse().with_context(|| format!("{s:?} is not an address or CIDR"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.trim().parse::<u8>().ok().filter(|p| *p <= max).with_context(|| format!("bad prefix in {s:?}"))?,
            None => max,
        };
        Ok(Net { addr, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => {
                let m = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                u32::from(n) & m == u32::from(i) & m
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let m = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                u128::from(n) & m == u128::from(i) & m
            }
            _ => false,
        }
    }

    fn nft(&self) -> String {
        format!("{} saddr {}/{}", if self.addr.is_ipv4() { "ip" } else { "ip6" }, self.addr, self.prefix)
    }
}

fn ports(s: &str) -> Result<Vec<(u16, u16)>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b): (u16, u16) = (a.trim().parse().with_context(|| format!("bad port {part:?}"))?, b.trim().parse().with_context(|| format!("bad port {part:?}"))?);
        if a == 0 || a > b {
            bail!("bad port range {part:?}");
        }
        out.push((a, b));
    }
    Ok(out)
}

fn ports_nft(p: &[(u16, u16)]) -> String {
    let items: Vec<String> = p.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}-{b}") }).collect();
    format!("{{ {} }}", items.join(", "))
}

fn valid_iface(s: &str) -> bool {
    !s.is_empty() && s.len() <= 15 && s.trim_end_matches('*').chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) && s.matches('*').count() <= 1 && (!s.contains('*') || s.ends_with('*'))
}

fn clean_comment(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\' || *c == ' ').take(64).collect()
}

impl Config {
    pub fn validate(&mut self) -> Result<()> {
        if !["drop", "reject", "accept"].contains(&self.policy.as_str()) {
            bail!("the default policy is drop, reject or accept");
        }
        for m in &self.management {
            Net::parse(m)?;
        }
        if self.rules.len() > 500 {
            bail!("at most 500 rules");
        }
        for (i, r) in self.rules.iter_mut().enumerate() {
            let n = i + 1;
            if !["accept", "drop", "reject"].contains(&r.action.as_str()) {
                bail!("rule {n}: action is accept, drop or reject");
            }
            if !["tcp", "udp", "tcp/udp", "icmp", ""].contains(&r.proto.as_str()) {
                bail!("rule {n}: protocol is tcp, udp, tcp/udp, icmp or any");
            }
            let p = ports(&r.dport).with_context(|| format!("rule {n}"))?;
            if !p.is_empty() && !["tcp", "udp", "tcp/udp"].contains(&r.proto.as_str()) {
                bail!("rule {n}: ports need tcp or udp");
            }
            if !r.source.trim().is_empty() {
                Net::parse(r.source.trim()).with_context(|| format!("rule {n}"))?;
            }
            if !r.iface.is_empty() && !valid_iface(&r.iface) {
                bail!("rule {n}: bad interface name {:?}", r.iface);
            }
            r.comment = clean_comment(&r.comment);
        }
        Ok(())
    }

    /// The nft script that replaces the table (atomically).
    pub fn script(&self, mgmt_ports: &[u16]) -> String {
        let mut s = format!("add table inet {TABLE}\ndelete table inet {TABLE}\n");
        if !self.enabled {
            return s;
        }
        let policy = if self.policy == "accept" { "accept" } else { "drop" };
        let mut l: Vec<String> = vec![
            "ct state established,related accept".into(),
            "ct state invalid drop".into(),
            "iif lo accept".into(),
            "meta l4proto ipv6-icmp icmpv6 type { nd-neighbor-solicit, nd-neighbor-advert, nd-router-advert, nd-router-solicit, packet-too-big, destination-unreachable, time-exceeded, parameter-problem } accept".into(),
            "udp dport { 68, 546 } accept comment \"dhcp\"".into(),
            // VMs asking OG-Host's DHCP for their address
            "iifname \"og*\" udp dport 67 accept comment \"vm dhcp\"".into(),
        ];
        if self.allow_containers {
            l.push("iifname \"og*\" accept comment \"containers\"".into());
        }
        if self.allow_ping {
            l.push("meta l4proto icmp icmp type { echo-request, destination-unreachable, time-exceeded } accept".into());
            l.push("meta l4proto ipv6-icmp icmpv6 type echo-request accept".into());
        }
        let mp: Vec<(u16, u16)> = mgmt_ports.iter().map(|p| (*p, *p)).collect();
        if self.management.is_empty() {
            l.push(format!("tcp dport {} accept comment \"management\"", ports_nft(&mp)));
        } else {
            for m in &self.management {
                if let Ok(n) = Net::parse(m) {
                    l.push(format!("{} tcp dport {} accept comment \"management\"", n.nft(), ports_nft(&mp)));
                }
            }
        }
        for r in self.rules.iter().filter(|r| r.enabled) {
            let mut m = Vec::new();
            if !r.iface.is_empty() {
                m.push(format!("iifname \"{}\"", r.iface));
            }
            if let Ok(n) = Net::parse(r.source.trim()) {
                if !r.source.trim().is_empty() {
                    m.push(n.nft());
                }
            }
            let p = ports(&r.dport).unwrap_or_default();
            match r.proto.as_str() {
                "tcp" | "udp" => m.push(if p.is_empty() { format!("meta l4proto {}", r.proto) } else { format!("{} dport {}", r.proto, ports_nft(&p)) }),
                "tcp/udp" => m.push(if p.is_empty() { "meta l4proto { tcp, udp }".into() } else { format!("meta l4proto {{ tcp, udp }} th dport {}", ports_nft(&p)) }),
                "icmp" => m.push("meta l4proto { icmp, ipv6-icmp }".into()),
                _ => {}
            }
            let act = if r.action == "reject" { "reject with icmpx admin-prohibited" } else { r.action.as_str() };
            let c = if r.comment.is_empty() { String::new() } else { format!(" comment \"{}\"", r.comment) };
            l.push(format!("{} {act}{c}", m.join(" ")).trim().to_string());
        }
        if self.policy == "reject" {
            l.push("meta l4proto tcp reject with tcp reset".into());
            l.push("reject with icmpx admin-prohibited".into());
        }
        s.push_str(&format!("table inet {TABLE} {{\n  chain input {{\n    type filter hook input priority filter; policy {policy};\n"));
        for x in l {
            s.push_str("    ");
            s.push_str(&x);
            s.push('\n');
        }
        s.push_str("  }\n}\n");
        s
    }

    /// Would a new TCP connection from `ip` to `port` be let in? (Interface rules are taken
    /// at their worst: an accept on an interface doesn't count, a drop does.)
    pub fn admits(&self, ip: IpAddr, port: u16, mgmt_ports: &[u16]) -> bool {
        if !self.enabled || ip.is_loopback() {
            return true;
        }
        if mgmt_ports.contains(&port) && (self.management.is_empty() || self.management.iter().filter_map(|m| Net::parse(m).ok()).any(|n| n.contains(ip))) {
            return true;
        }
        for r in self.rules.iter().filter(|r| r.enabled) {
            if !["tcp", "tcp/udp", ""].contains(&r.proto.as_str()) {
                continue;
            }
            let p = ports(&r.dport).unwrap_or_default();
            if !p.is_empty() && !p.iter().any(|(a, b)| (*a..=*b).contains(&port)) {
                continue;
            }
            if !r.source.trim().is_empty() && !Net::parse(r.source.trim()).is_ok_and(|n| n.contains(ip)) {
                continue;
            }
            if !r.iface.is_empty() && r.action == "accept" {
                continue;
            }
            return r.action == "accept";
        }
        self.policy == "accept"
    }
}

fn nft_apply(script: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("nft").args(["-f", "-"]).stdin(Stdio::piped()).stderr(Stdio::piped()).stdout(Stdio::null()).spawn().context("running nft")?;
    child.stdin.take().context("stdin")?.write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("nft: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

struct Pending {
    previous: Config,
    deadline: Instant,
    generation: u64,
}

pub struct Firewall {
    file: PathBuf,
    pub mgmt_ports: Vec<u16>,
    state: Mutex<(Config, Option<Pending>, u64)>,
}

impl Firewall {
    pub fn open(root: &Path, mut mgmt_ports: Vec<u16>) -> Firewall {
        mgmt_ports.push(22);
        mgmt_ports.sort_unstable();
        mgmt_ports.dedup();
        let file = root.join("firewall.json");
        let cfg: Config = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let fw = Firewall { file, mgmt_ports, state: Mutex::new((cfg.clone(), None, 0)) };
        if cfg.enabled {
            match nft_apply(&cfg.script(&fw.mgmt_ports)) {
                Ok(()) => eprintln!("og-hostd: firewall on ({} rules, policy {})", cfg.rules.len(), cfg.policy),
                Err(e) => eprintln!("og-hostd: firewall could not be applied: {e:#}"),
            }
        }
        fw
    }

    pub fn status(&self) -> serde_json::Value {
        let st = self.state.lock().unwrap();
        let left = st.1.as_ref().map(|p| p.deadline.saturating_duration_since(Instant::now()).as_secs());
        serde_json::json!({"config": st.0, "pending": left.map(|s| serde_json::json!({"seconds_left": s})), "management_ports": self.mgmt_ports, "script": st.0.script(&self.mgmt_ports)})
    }

    /// Apply `new` now; it reverts by itself unless [`confirm`]ed. Returns the generation to
    /// hand the rollback timer.
    pub fn apply(&self, mut new: Config, caller: Option<IpAddr>, caller_port: u16, force: bool) -> Result<u64> {
        new.validate()?;
        if let Some(ip) = caller {
            if !force && !new.admits(ip, caller_port, &self.mgmt_ports) {
                bail!("these rules would block you ({ip} to port {caller_port}): add your network under management or a rule that lets it in");
            }
        }
        let mut st = self.state.lock().unwrap();
        nft_apply(&new.script(&self.mgmt_ports))?;
        let previous = match st.1.take() {
            Some(p) => p.previous, // a second change before confirming keeps the last good one
            None => st.0.clone(),
        };
        st.2 += 1;
        let generation = st.2;
        st.0 = new;
        st.1 = Some(Pending { previous, deadline: Instant::now() + Duration::from_secs(CONFIRM_SECS), generation });
        Ok(generation)
    }

    pub fn confirm(&self) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if st.1.take().is_none() {
            bail!("nothing to confirm");
        }
        save(&self.file, &st.0)
    }

    /// Put back the last confirmed rules (timer or by hand). `generation` = only if still that change.
    pub fn rollback(&self, generation: Option<u64>) -> Result<bool> {
        let mut st = self.state.lock().unwrap();
        let Some(p) = st.1.as_ref() else { return Ok(false) };
        if generation.is_some_and(|g| g != p.generation) {
            return Ok(false);
        }
        let prev = st.1.take().unwrap().previous;
        nft_apply(&prev.script(&self.mgmt_ports))?;
        st.0 = prev;
        Ok(true)
    }
}

fn save(file: &Path, cfg: &Config) -> Result<()> {
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::rename(tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn script_and_lockout_check() {
        let mut c = Config { enabled: true, management: vec!["192.168.0.0/24".into()], ..Default::default() };
        c.rules.push(Rule { proto: "tcp".into(), dport: "80,443,9000-9100".into(), source: "10.0.0.0/8".into(), comment: "web \"x\"".into(), ..Default::default() });
        c.rules.push(Rule { action: "drop".into(), proto: "".into(), source: "10.6.6.6".into(), ..Default::default() });
        c.validate().unwrap();
        let s = c.script(&[22, 8006]);
        assert!(s.contains("ip saddr 192.168.0.0/24 tcp dport { 22, 8006 } accept"));
        assert!(s.contains("ip saddr 10.0.0.0/8 tcp dport { 80, 443, 9000-9100 } accept comment \"web x\""));
        assert!(s.contains("policy drop;"));
        let m = [22, 8006];
        assert!(c.admits(ip("192.168.0.50"), 8006, &m));
        assert!(!c.admits(ip("100.64.0.9"), 8006, &m));
        assert!(c.admits(ip("10.1.2.3"), 443, &m));
        assert!(!c.admits(ip("10.1.2.3"), 8006, &m));
        assert!(c.admits(ip("::ffff:192.168.0.7"), 22, &m));
        let mut bad = c.clone();
        bad.rules[0].iface = "eth0\" accept".into();
        assert!(bad.validate().is_err());
        bad.rules[0].iface = "eth0".into();
        bad.rules[0].proto = "icmp".into();
        assert!(bad.validate().is_err());
        assert!(Net::parse("fd00::/8").unwrap().contains(ip("fd12::1")));
        assert_eq!(Config::default().script(&m), "add table inet og_host_fw\ndelete table inet og_host_fw\n");
    }
}
