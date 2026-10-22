//! Desktop profile (`og.profile=desktop`, DESIGN.md §4.9): game containers that look like a
//! real PC rather than a container or VM. This module picks the identity — hostname,
//! machine-id, NIC MAC, DMI/SMBIOS — that `Node::run` puts in place; the runtime side (no
//! masked /proc, nested namespaces) is `RunOptions::desktop`.

use std::collections::BTreeMap;

/// Realtek, the most common onboard NIC vendor on desktop boards.
const DESKTOP_OUI: &str = "00:e0:4c";

/// DMI fields shown to the container (readable by anyone on real hardware).
pub const DMI_FIELDS: &[&str] = &[
    "sys_vendor", "product_name", "product_version", "product_family", "board_vendor", "board_name",
    "board_version", "bios_vendor", "bios_version", "bios_date", "chassis_vendor", "chassis_type",
];

/// What a self-built desktop's firmware typically reports.
const GENERIC_DMI: &[(&str, &str)] = &[
    ("sys_vendor", "To Be Filled By O.E.M."),
    ("product_name", "To Be Filled By O.E.M."),
    ("product_version", "To Be Filled By O.E.M."),
    ("product_family", "To Be Filled By O.E.M."),
    ("board_vendor", "ASRock"),
    ("board_name", "B550M Pro4"),
    ("board_version", ""),
    ("bios_vendor", "American Megatrends International, LLC."),
    ("bios_version", "P3.20"),
    ("bios_date", "09/27/2023"),
    ("chassis_vendor", "To Be Filled By O.E.M."),
    ("chassis_type", "3"),
];

const VIRTUAL_MARKERS: &[&str] = &["qemu", "kvm", "vmware", "virtualbox", "innotek", "xen", "bochs", "parallels", "virtual machine", "standard pc", "hvm domu", "cloud hypervisor"];

pub fn is_desktop(labels: &BTreeMap<String, String>) -> bool {
    labels.get("og.profile").is_some_and(|p| p.trim().eq_ignore_ascii_case("desktop"))
}

/// `og.hostname`, else keep an explicit hostname, else `DESKTOP-XXXXXXX` (Windows style).
pub fn hostname(labels: &BTreeMap<String, String>, id: &str, current: &str, name: &str) -> String {
    if let Some(h) = labels.get("og.hostname").filter(|h| valid_hostname(h)) {
        return h.clone();
    }
    let generated = current == name || current.is_empty() || (current.len() == 12 && current.chars().all(|c| c.is_ascii_hexdigit()));
    if !generated {
        return current.to_string();
    }
    format!("DESKTOP-{}", id.chars().filter(|c| c.is_ascii_alphanumeric()).take(7).collect::<String>().to_ascii_uppercase())
}

fn valid_hostname(h: &str) -> bool {
    !h.is_empty() && h.len() <= 63 && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !h.starts_with('-')
}

/// `og.mac`, else a stable vendor-prefixed MAC for desktop containers; None = network default.
pub fn mac(labels: &BTreeMap<String, String>, id: &str) -> Option<String> {
    if let Some(m) = labels.get("og.mac") {
        return Some(m.to_ascii_lowercase());
    }
    if !is_desktop(labels) {
        return None;
    }
    let hex: Vec<char> = id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let b = |i: usize| -> String { hex.get(i * 2..i * 2 + 2).map(|s| s.iter().collect::<String>()).unwrap_or_else(|| "00".into()) };
    Some(format!("{DESKTOP_OUI}:{}:{}:{}", b(0), b(1), b(2)).to_ascii_lowercase())
}

/// Machine-id: stable per container, 32 lowercase hex.
pub fn machine_id(id: &str) -> String {
    let mut s: String = id.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_ascii_lowercase();
    while s.len() < 32 {
        s.push('0');
    }
    s.truncate(32);
    s
}

fn host_dmi(field: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/class/dmi/id/{field}")).ok().map(|s| s.trim().to_string())
}

/// Does the host's own DMI say it's a VM?
pub fn host_is_virtual() -> bool {
    ["sys_vendor", "product_name", "board_vendor", "bios_vendor"]
        .iter()
        .filter_map(|f| host_dmi(f))
        .any(|v| VIRTUAL_MARKERS.iter().any(|m| v.to_ascii_lowercase().contains(m)))
}

/// DMI values to show over the host's: `og.dmi.<field>` labels, plus generic desktop values
/// for every field when the host itself is virtual. Empty = show the host's (bare metal).
pub fn dmi_overrides(labels: &BTreeMap<String, String>, host_virtual: bool) -> Vec<(String, String)> {
    DMI_FIELDS
        .iter()
        .filter_map(|f| {
            let label = labels.get(&format!("og.dmi.{f}")).cloned();
            let generic = host_virtual.then(|| GENERIC_DMI.iter().find(|(k, _)| k == f).map(|(_, v)| v.to_string())).flatten();
            label.or(generic).map(|v| (f.to_string(), v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn identity() {
        let id = "f770e9a05a07d2d301d66ae38940c50e";
        let d = l(&[("og.profile", "desktop")]);
        assert!(is_desktop(&d) && !is_desktop(&l(&[])));
        assert_eq!(hostname(&d, id, "steam", "steam"), "DESKTOP-F770E9A");
        assert_eq!(hostname(&d, id, "f770e9a05a07", "x"), "DESKTOP-F770E9A");
        assert_eq!(hostname(&d, id, "gamebox", "x"), "gamebox");
        assert_eq!(hostname(&l(&[("og.hostname", "omega-pc")]), id, "a", "a"), "omega-pc");
        assert_eq!(mac(&d, id).unwrap(), "00:e0:4c:f7:70:e9");
        assert!(og_net::valid_mac(&mac(&d, id).unwrap()));
        assert_eq!(mac(&l(&[]), id), None);
        assert_eq!(machine_id(id), id);
        assert_eq!(dmi_overrides(&d, false), vec![]);
        let v = dmi_overrides(&l(&[("og.dmi.board_name", "X670E")]), true);
        assert!(v.contains(&("board_name".into(), "X670E".into())) && v.contains(&("chassis_type".into(), "3".into())));
    }
}
