//! Per-VM firewalls: what may reach each VM. One nftables table (`bridge og_vm_fw`) filters
//! frames going into VM taps, on OG-Host networks and host bridges alike (routed traffic
//! leaves the host through the bridge too, so the output hook is covered as well). Rebuilt
//! from the running VMs whenever one starts, stops or changes its rules.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::firewall::Rule;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VmFirewall {
    pub enabled: bool,
    /// what isn't allowed by a rule: drop | accept
    pub policy: String,
    pub rules: Vec<Rule>,
}

impl Default for VmFirewall {
    fn default() -> Self {
        VmFirewall { enabled: false, policy: "drop".into(), rules: vec![] }
    }
}

impl VmFirewall {
    pub fn validate(&mut self) -> Result<()> {
        if !["drop", "accept"].contains(&self.policy.as_str()) {
            anyhow::bail!("VM firewall policy is drop or accept");
        }
        // the host firewall's rule checks (ports, sources, comments)
        let mut c = crate::firewall::Config { enabled: true, policy: self.policy.clone(), rules: self.rules.clone(), ..Default::default() };
        c.validate()?;
        self.rules = c.rules;
        for r in &mut self.rules {
            r.iface.clear();
        }
        Ok(())
    }
}

/// The table for these (tap, firewall) pairs.
pub fn script(vms: &[(String, VmFirewall)]) -> String {
    let mut s = String::from("add table bridge og_vm_fw\ndelete table bridge og_vm_fw\n");
    let on: Vec<&(String, VmFirewall)> = vms.iter().filter(|(_, f)| f.enabled).collect();
    if on.is_empty() {
        return s;
    }
    s.push_str("table bridge og_vm_fw {\n");
    for (i, (_, f)) in on.iter().enumerate() {
        s.push_str(&format!("  chain vm{i} {{\n    ct state established,related accept\n    ct state invalid drop\n    ether type arp accept\n    meta l4proto ipv6-icmp accept\n    udp sport 67 udp dport 68 accept\n    udp sport 547 udp dport 546 accept\n"));
        for r in f.rules.iter().filter(|r| r.enabled) {
            let mut m = vec![];
            if !r.source.trim().is_empty() {
                let v6 = r.source.contains(':');
                m.push(format!("{} saddr {}", if v6 { "ip6" } else { "ip" }, r.source.trim()));
            }
            let ports = r.dport.split(',').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(", ");
            match r.proto.as_str() {
                "tcp" | "udp" => m.push(if ports.is_empty() { format!("meta l4proto {}", r.proto) } else { format!("{} dport {{ {ports} }}", r.proto) }),
                "tcp/udp" => m.push(if ports.is_empty() { "meta l4proto { tcp, udp }".into() } else { format!("meta l4proto {{ tcp, udp }} th dport {{ {ports} }}") }),
                "icmp" => m.push("meta l4proto { icmp, ipv6-icmp }".into()),
                _ => {}
            }
            let act = if r.action == "reject" { "reject" } else { r.action.as_str() };
            let c = if r.comment.is_empty() { String::new() } else { format!(" comment \"{}\"", r.comment) };
            s.push_str(&format!("    {} {act}{c}\n", m.join(" ")));
        }
        s.push_str(&format!("    {}\n  }}\n", if f.policy == "accept" { "accept" } else { "drop" }));
    }
    for hook in ["forward", "output"] {
        s.push_str(&format!("  chain {hook} {{\n    type filter hook {hook} priority filter; policy accept;\n"));
        for (i, (tap, _)) in on.iter().enumerate() {
            s.push_str(&format!("    oifname \"{tap}\" jump vm{i}\n"));
        }
        s.push_str("  }\n");
    }
    s.push_str("}\n");
    s
}

pub fn apply(vms: &[(String, VmFirewall)]) -> Result<()> {
    use std::io::Write;
    let sc = script(vms);
    let mut child = std::process::Command::new("nft").args(["-f", "-"]).stdin(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn()?;
    child.stdin.take().unwrap().write_all(sc.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!("VM firewall: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
