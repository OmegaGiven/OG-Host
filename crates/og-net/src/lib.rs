//! og-net: container networking for OG-Host.
//!
//! - Every container gets a named network namespace (`/run/netns/og-<id>`), created before the
//!   container starts; the runtime joins it. Sharing a network (`network_mode: service:x`) is
//!   just joining another container's namespace.
//! - A network is a Linux bridge (`og0` for `default`) with a /24 (by default) from which
//!   [`ipam`] hands out addresses. Containers connect through a veth pair.
//! - nftables table `ip og_host`: outbound masquerade, Docker-style port publishing (DNAT,
//!   including from the host itself and 127.0.0.1), hairpin NAT, and isolation between
//!   different networks (a bridge may not open connections into another OG-Host bridge). Every per-container rule carries the comment `og:<id>`.
//!
//! Kernel configuration goes through `ip` and `nft` for now (behind this API); the plan is to
//! swap in native netlink without changing callers.

pub mod ipam;

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

pub use ipam::Subnet;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("no free addresses left in {0}")]
    Exhausted(String),
    #[error("`{cmd}` failed: {stderr}")]
    Command { cmd: String, stderr: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

pub const TABLE: &str = "og_host";
pub const DEFAULT_NETWORK: &str = "default";
pub const DEFAULT_BRIDGE: &str = "og0";
pub const DEFAULT_SUBNET: &str = "10.89.0.0/24";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    pub bridge: String,
    pub subnet: Subnet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// `[host_ip:]host_port:container_port[/proto]`, as in `docker run -p` and compose `ports:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMap {
    pub host_ip: Option<Ipv4Addr>,
    pub host_port: u16,
    pub container_port: u16,
    pub proto: Proto,
}

impl PortMap {
    pub fn parse(s: &str) -> Result<Self> {
        let (spec, proto) = match s.rsplit_once('/') {
            Some((a, "tcp")) => (a, Proto::Tcp),
            Some((a, "udp")) => (a, Proto::Udp),
            Some((_, p)) => return Err(Error::Invalid(format!("unsupported protocol {p:?} in {s:?}"))),
            None => (s, Proto::Tcp),
        };
        let parts: Vec<&str> = spec.split(':').collect();
        let port = |p: &str| p.parse::<u16>().ok().filter(|v| *v != 0).ok_or_else(|| Error::Invalid(format!("bad port {p:?} in {s:?}")));
        let (host_ip, host_port, container_port) = match parts.as_slice() {
            [c] => (None, port(c)?, port(c)?),
            [h, c] => (None, port(h)?, port(c)?),
            [ip, h, c] => (Some(ip.parse().map_err(|_| Error::Invalid(format!("bad host address {ip:?}")))?), port(h)?, port(c)?),
            _ => return Err(Error::Invalid(format!("bad port mapping {s:?}"))),
        };
        Ok(PortMap { host_ip, host_port, container_port, proto })
    }
}

/// What a container got: where its namespace is and its address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub owner: String,
    pub netns: PathBuf,
    pub network: Option<String>,
    pub ip: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub prefix: Option<u8>,
    pub host_veth: Option<String>,
    pub ports: Vec<PortMap>,
}

fn run(args: &[&str]) -> Result<String> {
    let out = Command::new(args[0]).args(&args[1..]).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(Error::Command { cmd: args.join(" "), stderr: String::from_utf8_lossy(&out.stderr).trim().to_string() });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Apply an nft script atomically (`nft -f -`): all of it or nothing.
fn nft_apply(script: &str) -> Result<()> {
    let mut child = Command::new("nft").args(["-f", "-"]).stdin(Stdio::piped()).stderr(Stdio::piped()).stdout(Stdio::null()).spawn()?;
    child.stdin.take().expect("stdin").write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Command { cmd: format!("nft -f - <<<{script:?}"), stderr: String::from_utf8_lossy(&out.stderr).trim().to_string() });
    }
    Ok(())
}

fn write_sysctl(path: &str, value: &str) -> Result<()> {
    std::fs::write(path, value).map_err(|e| Error::Invalid(format!("{path}: {e}")))
}

/// Name of the network namespace for `owner` (also its file under /run/netns).
pub fn netns_name(owner: &str) -> String {
    ifname("og-", owner)
}

/// Names are kernel-limited to 15 bytes.
fn ifname(prefix: &str, id: &str) -> String {
    let mut s = format!("{prefix}{id}");
    s.truncate(15);
    s
}

/// Docker-style locally administered MAC derived from the IP (02:42:a.b.c.d).
/// Unicast `xx:xx:xx:xx:xx:xx`.
pub fn valid_mac(m: &str) -> bool {
    let parts: Vec<&str> = m.split(':').collect();
    parts.len() == 6
        && parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
        && u8::from_str_radix(parts[0], 16).is_ok_and(|b| b & 1 == 0)
}

fn mac_for(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("02:42:{:02x}:{:02x}:{:02x}:{:02x}", o[0], o[1], o[2], o[3])
}

pub struct Net {
    /// Where network definitions and IPAM state live, e.g. /var/lib/og-host/networks.
    pub state_dir: PathBuf,
}

impl Net {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Net { state_dir: state_dir.into() }
    }

    fn def_path(&self, name: &str) -> PathBuf {
        self.state_dir.join(format!("{name}.net.json"))
    }

    fn ipam_path(&self, name: &str) -> PathBuf {
        self.state_dir.join(format!("{name}.ipam.json"))
    }

    pub fn get(&self, name: &str) -> Result<Option<Network>> {
        match std::fs::read(self.def_path(name)) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list(&self) -> Result<Vec<Network>> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.state_dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".net.json") {
                    out.push(serde_json::from_slice(&std::fs::read(e.path())?)?);
                }
            }
        }
        Ok(out)
    }

    /// Define a network (idempotent) and make sure its bridge and NAT rules exist.
    pub fn ensure(&self, name: &str, subnet: Option<&str>) -> Result<Network> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
            return Err(Error::Invalid(format!("invalid network name {name:?}")));
        }
        let net = match self.get(name)? {
            Some(n) => n,
            None => {
                let subnet = match subnet {
                    Some(s) => Subnet::parse(s)?,
                    None if name == DEFAULT_NETWORK => Subnet::parse(DEFAULT_SUBNET)?,
                    None => self.next_free_subnet()?,
                };
                if let Some(clash) = self.list()?.into_iter().find(|n| n.subnet.contains(subnet.base) || subnet.contains(n.subnet.base)) {
                    return Err(Error::Invalid(format!("{subnet} overlaps network {:?} ({})", clash.name, clash.subnet)));
                }
                let bridge = if name == DEFAULT_NETWORK { DEFAULT_BRIDGE.to_string() } else { ifname("og-", &short_hash(name)) };
                let n = Network { name: name.to_string(), bridge, subnet };
                std::fs::create_dir_all(&self.state_dir)?;
                std::fs::write(self.def_path(name), serde_json::to_vec_pretty(&n)?)?;
                n
            }
        };
        self.ensure_kernel_state(&net)?;
        Ok(net)
    }

    /// 10.89.N.0/24 for the first N not used yet.
    fn next_free_subnet(&self) -> Result<Subnet> {
        let used: Vec<Subnet> = self.list()?.into_iter().map(|n| n.subnet).collect();
        for n in 1..=255u8 {
            let s = Subnet::parse(&format!("10.89.{n}.0/24"))?;
            if !used.iter().any(|u| u.contains(s.base) || s.contains(u.base)) {
                return Ok(s);
            }
        }
        Err(Error::Exhausted("10.89.0.0/16".into()))
    }

    fn ensure_kernel_state(&self, net: &Network) -> Result<()> {
        let exists = Path::new("/sys/class/net").join(&net.bridge).exists();
        if !exists {
            run(&["ip", "link", "add", &net.bridge, "type", "bridge"])?;
            run(&["ip", "addr", "add", &format!("{}/{}", net.subnet.gateway(), net.subnet.prefix), "dev", &net.bridge])?;
        }
        run(&["ip", "link", "set", &net.bridge, "up"])?;
        write_sysctl("/proc/sys/net/ipv4/ip_forward", "1")?;
        // lets DNAT'd connections from 127.0.0.1 be routed to the bridge
        write_sysctl(&format!("/proc/sys/net/ipv4/conf/{}/route_localnet", net.bridge), "1")?;
        self.ensure_table()?;
        let tag = format!("og-net:{}", net.name);
        if !self.table_has_comment(&tag)? {
            let s = net.subnet;
            let b = &net.bridge;
            nft_apply(&format!(
                "add rule ip {TABLE} postrouting ip saddr {s} oifname != \"{b}\" masquerade comment \"{tag}\"\n\
                 add rule ip {TABLE} postrouting ip saddr {s} ip daddr {s} ct status dnat masquerade comment \"{tag}\"\n\
                 add rule ip {TABLE} postrouting ip saddr 127.0.0.0/8 ip daddr {s} masquerade comment \"{tag}\"\n\
                 add rule ip {TABLE} forward iifname \"{b}\" oifname \"og*\" oifname != \"{b}\" ct state new drop comment \"{tag}\"\n"
            ))?;
        }
        Ok(())
    }

    /// Base table + chains. The forward chain keeps different OG-Host networks apart.
    fn ensure_table(&self) -> Result<()> {
        nft_apply(&format!(
            "table ip {TABLE} {{\n\
               chain prerouting {{ type nat hook prerouting priority dstnat; policy accept; }}\n\
               chain output {{ type nat hook output priority -100; policy accept; }}\n\
               chain postrouting {{ type nat hook postrouting priority srcnat; policy accept; }}\n\
               chain forward {{ type filter hook forward priority filter; policy accept; }}\n\
             }}\n"
        ))?;
        Ok(())
    }

    fn table_rules(&self) -> Result<Vec<(u64, String, String)>> {
        let out = run(&["nft", "-j", "-a", "list", "table", "ip", TABLE])?;
        let v: serde_json::Value = serde_json::from_str(&out)?;
        let mut rules = Vec::new();
        for item in v["nftables"].as_array().into_iter().flatten() {
            if let Some(r) = item.get("rule") {
                if let (Some(h), Some(chain)) = (r["handle"].as_u64(), r["chain"].as_str()) {
                    rules.push((h, chain.to_string(), r["comment"].as_str().unwrap_or("").to_string()));
                }
            }
        }
        Ok(rules)
    }

    fn table_has_comment(&self, c: &str) -> Result<bool> {
        Ok(self.table_rules()?.iter().any(|(_, _, com)| com == c))
    }

    /// Remove a network: its NAT/isolation rules, bridge and state. Refused while in use.
    pub fn remove(&self, name: &str) -> Result<()> {
        let Some(net) = self.get(name)? else { return Ok(()) };
        if let Ok(text) = std::fs::read_to_string(self.ipam_path(name)) {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if v["allocations"].as_object().is_some_and(|a| !a.is_empty()) {
                return Err(Error::Invalid(format!("network {name:?} still has containers attached")));
            }
        }
        let tag = format!("og-net:{name}");
        if let Ok(rules) = self.table_rules() {
            let mut script = String::new();
            for (h, chain, c) in rules {
                if c == tag {
                    let _ = writeln!(script, "delete rule ip {TABLE} {chain} handle {h}");
                }
            }
            if !script.is_empty() {
                nft_apply(&script)?;
            }
        }
        if Path::new("/sys/class/net").join(&net.bridge).exists() {
            run(&["ip", "link", "del", &net.bridge])?;
        }
        let _ = std::fs::remove_file(self.ipam_path(name));
        std::fs::remove_file(self.def_path(name))?;
        Ok(())
    }

    /// A namespace with only loopback (up). Used for `network: none` and as the first step of attach.
    pub fn create_netns(&self, owner: &str) -> Result<PathBuf> {
        let name = ifname("og-", owner);
        let path = PathBuf::from("/run/netns").join(&name);
        if !path.exists() {
            run(&["ip", "netns", "add", &name])?;
        }
        run(&["ip", "-n", &name, "link", "set", "lo", "up"])?;
        Ok(path)
    }

    /// Create a namespace for `owner`, plug it into `network`, publish `ports`.
    pub fn attach(&self, owner: &str, network: &str, ports: &[PortMap]) -> Result<Attachment> {
        self.attach_mac(owner, network, ports, None)
    }

    /// Like [`attach`](Self::attach), with a chosen MAC address for the container's interface.
    pub fn attach_mac(&self, owner: &str, network: &str, ports: &[PortMap], mac: Option<&str>) -> Result<Attachment> {
        if let Some(m) = mac {
            if !valid_mac(m) {
                return Err(Error::Invalid(format!("bad MAC address {m:?}")));
            }
        }
        let net = self.ensure(network, None)?;
        let netns = self.create_netns(owner)?;
        let ns_name = netns.file_name().unwrap().to_string_lossy().to_string();
        let ip = ipam::allocate(&self.ipam_path(&net.name), &net.subnet, owner)?;
        let veth = ifname("ogv", owner);
        let result = (|| -> Result<()> {
            if !Path::new("/sys/class/net").join(&veth).exists() {
                run(&["ip", "link", "add", &veth, "type", "veth", "peer", "name", "eth0", "netns", &ns_name])?;
            }
            run(&["ip", "link", "set", &veth, "master", &net.bridge, "up"])?;
            let mac = mac.map(str::to_string).unwrap_or_else(|| mac_for(ip));
            run(&["ip", "-n", &ns_name, "link", "set", "eth0", "address", &mac])?;
            let _ = run(&["ip", "-n", &ns_name, "addr", "flush", "dev", "eth0"]);
            run(&["ip", "-n", &ns_name, "addr", "add", &format!("{ip}/{}", net.subnet.prefix), "dev", "eth0"])?;
            run(&["ip", "-n", &ns_name, "link", "set", "eth0", "up"])?;
            run(&["ip", "-n", &ns_name, "route", "replace", "default", "via", &net.subnet.gateway().to_string()])?;
            self.publish(owner, ip, ports)?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = self.detach(owner); // don't leave half-built plumbing behind
            return Err(e);
        }
        Ok(Attachment {
            owner: owner.to_string(),
            netns,
            network: Some(net.name),
            ip: Some(ip),
            gateway: Some(net.subnet.gateway()),
            prefix: Some(net.subnet.prefix),
            host_veth: Some(veth),
            ports: ports.to_vec(),
        })
    }

    /// Delete the rules tagged for `owner` (used before re-publishing, and on detach).
    fn unpublish(&self, owner: &str) -> Result<()> {
        let tag = format!("og:{owner}");
        if let Ok(rules) = self.table_rules() {
            let mut script = String::new();
            for (h, chain, c) in rules {
                if c == tag {
                    let _ = writeln!(script, "delete rule ip {TABLE} {chain} handle {h}");
                }
            }
            if !script.is_empty() {
                nft_apply(&script)?;
            }
        }
        Ok(())
    }

    /// Drop only the namespace link (for rebuilding a sandbox after a reboot).
    pub fn detach_netns_only(&self, owner: &str) -> Result<()> {
        let ns_name = ifname("og-", owner);
        if Path::new("/run/netns").join(&ns_name).exists() {
            run(&["ip", "netns", "del", &ns_name])?;
        }
        Ok(())
    }

    fn publish(&self, owner: &str, ip: Ipv4Addr, ports: &[PortMap]) -> Result<()> {
        self.unpublish(owner)?; // idempotent: re-attaching never duplicates rules
        if ports.is_empty() {
            return Ok(());
        }
        let tag = format!("og:{owner}");
        let mut script = String::new();
        for p in ports {
            let proto = p.proto.as_str();
            let dst = match p.host_ip {
                Some(h) => format!("ip daddr {h}"),
                None => "fib daddr type local".to_string(),
            };
            for chain in ["prerouting", "output"] {
                let _ = writeln!(
                    script,
                    "add rule ip {TABLE} {chain} {dst} {proto} dport {} dnat to {ip}:{} comment \"{tag}\"",
                    p.host_port, p.container_port
                );
            }
        }
        nft_apply(&script)
    }

    /// Remove everything `owner` had: rules, veth, namespace, address. Safe to call twice.
    pub fn detach(&self, owner: &str) -> Result<()> {
        self.unpublish(owner)?;
        let veth = ifname("ogv", owner);
        if Path::new("/sys/class/net").join(&veth).exists() {
            let _ = run(&["ip", "link", "del", &veth]);
        }
        let ns_name = ifname("og-", owner);
        if Path::new("/run/netns").join(&ns_name).exists() {
            run(&["ip", "netns", "del", &ns_name])?;
        }
        for n in self.list()? {
            ipam::release(&self.ipam_path(&n.name), owner)?;
        }
        Ok(())
    }
}

fn short_hash(s: &str) -> String {
    // FNV-1a, enough to make stable distinct bridge names
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")[..10].to_string()
}

/// /etc/resolv.conf for containers: the host's real upstream servers. Loopback resolvers
/// (systemd-resolved's 127.0.0.53, dnsmasq) are unreachable from a container, so they're
/// swapped for the upstreams systemd-resolved itself uses.
pub fn container_resolv_conf() -> String {
    let parse = |text: &str| -> (Vec<String>, Vec<String>) {
        let mut ns = Vec::new();
        let mut other = Vec::new();
        for l in text.lines() {
            let t = l.trim();
            if let Some(ip) = t.strip_prefix("nameserver") {
                let ip = ip.trim();
                if !(ip.starts_with("127.") || ip == "::1") {
                    ns.push(ip.to_string());
                }
            } else if t.starts_with("search") || t.starts_with("options") {
                other.push(t.to_string());
            }
        }
        (ns, other)
    };
    let (mut ns, mut other) = parse(&std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default());
    if ns.is_empty() {
        (ns, other) = parse(&std::fs::read_to_string("/run/systemd/resolve/resolv.conf").unwrap_or_default());
    }
    if ns.is_empty() {
        ns.push("1.1.1.1".into());
        ns.push("9.9.9.9".into());
    }
    let mut out = String::from("# generated by OG-Host\n");
    for n in ns.iter().take(3) {
        let _ = writeln!(out, "nameserver {n}");
    }
    for o in other.iter().filter(|o| !o.contains("trust-ad")) {
        let _ = writeln!(out, "{o}");
    }
    out
}

/// /etc/hosts for a container: localhost, itself, plus extra (name, ip) entries.
pub fn container_hosts(hostname: &str, ip: Option<Ipv4Addr>, extra: &[(String, String)]) -> String {
    let mut out = String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    if let Some(ip) = ip {
        let _ = writeln!(out, "{ip}\t{hostname}");
    } else {
        let _ = writeln!(out, "127.0.1.1\t{hostname}");
    }
    for (name, addr) in extra {
        let _ = writeln!(out, "{addr}\t{name}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_mappings() {
        assert_eq!(PortMap::parse("8080:80").unwrap(), PortMap { host_ip: None, host_port: 8080, container_port: 80, proto: Proto::Tcp });
        assert_eq!(PortMap::parse("53:53/udp").unwrap().proto, Proto::Udp);
        assert_eq!(PortMap::parse("127.0.0.1:9000:9000").unwrap().host_ip, Some(Ipv4Addr::LOCALHOST));
        assert_eq!(PortMap::parse("443").unwrap().host_port, 443);
        for bad in ["0:80", "80:99999", "a:b", "1.2.3:80:80", "80/sctp", "1:2:3:4"] {
            assert!(PortMap::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn interface_names_fit_the_kernel_limit() {
        let id = "0123456789abcdef0123456789abcdef";
        assert!(ifname("ogv", id).len() <= 15 && ifname("og-", id).len() <= 15);
        assert_eq!(mac_for(Ipv4Addr::new(10, 89, 0, 2)), "02:42:0a:59:00:02");
        assert_ne!(short_hash("media"), short_hash("download"));
    }

    #[test]
    fn hosts_file() {
        let h = container_hosts("web", Some(Ipv4Addr::new(10, 89, 0, 5)), &[("db".into(), "10.89.0.6".into())]);
        assert!(h.contains("10.89.0.5\tweb") && h.contains("10.89.0.6\tdb") && h.starts_with("127.0.0.1\tlocalhost"));
    }
}
