//! Default seccomp profile.
//!
//! Modeled on the syscalls Docker's default profile refuses, as a deny list over a default
//! allow: each rule shuts a piece of kernel attack surface that containers don't need, and is
//! lifted when the container is granted the capability that guards it (as Docker does).
//! The x86 and x32 ABIs are filtered too, so 32-bit syscalls can't route around the rules
//! (and 32-bit programs, e.g. Steam, still run).
//!
//! Errors are EPERM, except clone3, which returns ENOSYS so libc falls back to clone, where
//! the namespace flags can be inspected (clone3 passes them in memory, out of seccomp's reach).

use oci_spec::runtime::{Capability, LinuxSeccomp};
use serde_json::{json, Value};

const EPERM: u32 = 1;
const ENOSYS: u32 = 38;

/// (syscalls, capabilities any of which lifts the rule; empty = always refused)
const RULES: &[(&[&str], &[Capability])] = &[
    (&["acct"], &[Capability::SysPacct]),
    (&["add_key", "request_key", "keyctl"], &[]),
    (&["bpf"], &[Capability::Bpf, Capability::SysAdmin]),
    (&["clock_adjtime", "clock_settime", "settimeofday", "stime", "adjtimex"], &[Capability::SysTime]),
    (&["create_module", "init_module", "finit_module", "delete_module", "query_module", "get_kernel_syms"], &[Capability::SysModule]),
    (&["kexec_load", "kexec_file_load", "reboot"], &[Capability::SysBoot]),
    (&["iopl", "ioperm"], &[Capability::SysRawio]),
    (&["kcmp", "process_madvise"], &[Capability::SysPtrace]),
    (&["lookup_dcookie", "quotactl", "quotactl_fd", "swapon", "swapoff", "fanotify_init", "name_to_handle_at", "nfsservctl"], &[Capability::SysAdmin]),
    (&["mount", "umount", "umount2", "fsopen", "fsconfig", "fsmount", "fspick", "move_mount", "open_tree", "mount_setattr", "pivot_root"], &[Capability::SysAdmin]),
    (&["unshare", "setns"], &[Capability::SysAdmin]),
    (&["open_by_handle_at"], &[Capability::DacReadSearch]),
    (&["perf_event_open"], &[Capability::Perfmon, Capability::SysAdmin]),
    (&["syslog"], &[Capability::Syslog]),
    (&["move_pages"], &[Capability::SysNice]),
    (&["vhangup"], &[Capability::SysTtyConfig]),
    (&["userfaultfd"], &[Capability::SysPtrace]),
    (&["io_uring_setup", "io_uring_enter", "io_uring_register"], &[]),
    (&["sysfs", "_sysctl", "ustat", "uselib", "vm86", "vm86old"], &[]),
];

/// Namespace flags refused in clone(2) without CAP_SYS_ADMIN.
const CLONE_NS_FLAGS: &[u64] = &[
    0x0000_0080, // CLONE_NEWTIME
    0x0002_0000, // CLONE_NEWNS
    0x0200_0000, // CLONE_NEWCGROUP
    0x0400_0000, // CLONE_NEWUTS
    0x0800_0000, // CLONE_NEWIPC
    0x1000_0000, // CLONE_NEWUSER
    0x2000_0000, // CLONE_NEWPID
    0x4000_0000, // CLONE_NEWNET
];

fn architectures() -> Vec<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => vec!["SCMP_ARCH_X86_64", "SCMP_ARCH_X86", "SCMP_ARCH_X32"],
        "aarch64" => vec!["SCMP_ARCH_AARCH64", "SCMP_ARCH_ARM"],
        _ => vec![],
    }
}

/// Syscalls a sandbox inside the container needs to build its own namespaces (bubblewrap,
/// Steam's pressure-vessel, browsers' sandboxes): allowed with `nested`. Inside our user
/// namespace they act on namespaces owned by the container's unprivileged root.
const NESTED: &[&str] = &["unshare", "setns", "mount", "umount", "umount2", "pivot_root", "fsopen", "fsconfig", "fsmount", "fspick", "move_mount", "open_tree", "mount_setattr"];

/// The profile for a container holding `caps`; `nested` allows nested namespaces (desktop profile).
pub fn default_profile(caps: &[Capability], nested: bool) -> LinuxSeccomp {
    let has = |c: &[Capability]| c.iter().any(|x| caps.contains(x));
    let mut names: Vec<&str> = Vec::new();
    for (calls, lift) in RULES {
        if lift.is_empty() || !has(lift) {
            names.extend(calls.iter().filter(|c| !(nested && NESTED.contains(c))));
        }
    }
    let mut syscalls: Vec<Value> = vec![json!({ "names": names, "action": "SCMP_ACT_ERRNO", "errnoRet": EPERM })];
    if !caps.contains(&Capability::SysAdmin) && !nested {
        // clone's flags are its first argument on x86_64/aarch64 (s390x differs; not a target)
        for f in CLONE_NS_FLAGS {
            syscalls.push(json!({
                "names": ["clone"],
                "action": "SCMP_ACT_ERRNO",
                "errnoRet": EPERM,
                "args": [{ "index": 0, "value": f, "valueTwo": f, "op": "SCMP_CMP_MASKED_EQ" }],
            }));
        }
        syscalls.push(json!({ "names": ["clone3"], "action": "SCMP_ACT_ERRNO", "errnoRet": ENOSYS }));
    }
    serde_json::from_value(json!({
        "defaultAction": "SCMP_ACT_ALLOW",
        "architectures": architectures(),
        "syscalls": syscalls,
    }))
    .expect("static seccomp profile is valid")
}

/// A profile in Docker's seccomp JSON format (what `--security-opt seccomp=profile.json` takes)
/// for a container holding `caps`: entries are kept or dropped by their `includes` / `excludes`
/// (capabilities, architectures, minimum kernel) the way Docker does, and the architectures
/// come from `archMap` for this machine when the profile doesn't list them.
pub fn from_docker(p: &Value, caps: &[Capability]) -> Result<LinuxSeccomp, String> {
    let host_arch = architectures().first().copied().unwrap_or("");
    let cap_names: Vec<String> = caps.iter().filter_map(|c| serde_json::to_value(c).ok()?.as_str().map(String::from)).collect();
    let kernel = kernel_version();
    let arches: Vec<Value> = match p.get("architectures").and_then(Value::as_array) {
        Some(a) if !a.is_empty() => a.clone(),
        _ => p.get("archMap").and_then(Value::as_array).into_iter().flatten().filter(|m| m["architecture"] == host_arch).flat_map(|m| {
            std::iter::once(m["architecture"].clone()).chain(m["subArchitectures"].as_array().cloned().unwrap_or_default())
        }).collect(),
    };
    let short_arch = |a: &str| a.trim_start_matches("SCMP_ARCH_").to_ascii_lowercase();
    let host_short = match std::env::consts::ARCH { "x86_64" => "amd64", "aarch64" => "arm64", a => a };
    let arch_matches = |list: &Value| list.as_array().is_some_and(|l| l.iter().filter_map(Value::as_str).any(|a| a == host_short || short_arch(a) == short_arch(host_arch)));
    let mut syscalls = Vec::new();
    for e in p.get("syscalls").and_then(Value::as_array).into_iter().flatten() {
        let inc = &e["includes"];
        let exc = &e["excludes"];
        let list = |v: &Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect() };
        if !list(&inc["caps"]).iter().all(|c| cap_names.contains(c)) {
            continue;
        }
        if inc.get("arches").is_some_and(|a| a.as_array().is_some_and(|l| !l.is_empty())) && !arch_matches(&inc["arches"]) {
            continue;
        }
        if let Some(min) = inc["minKernel"].as_str() {
            if kernel < parse_version(min) {
                continue;
            }
        }
        if list(&exc["caps"]).iter().any(|c| cap_names.contains(c)) || arch_matches(&exc["arches"]) {
            continue;
        }
        let mut names = list(&e["names"]);
        if let Some(n) = e["name"].as_str() {
            names.push(n.to_string());
        }
        if names.is_empty() {
            continue;
        }
        let mut r = json!({ "names": names, "action": e["action"] });
        for k in ["errnoRet", "args"] {
            if !e[k].is_null() {
                r[k] = e[k].clone();
            }
        }
        syscalls.push(r);
    }
    let mut out = json!({ "defaultAction": p.get("defaultAction").cloned().unwrap_or(json!("SCMP_ACT_ERRNO")), "architectures": arches, "syscalls": syscalls });
    if let Some(d) = p.get("defaultErrnoRet") {
        out["defaultErrnoRet"] = d.clone();
    }
    if let Some(f) = p.get("flags") {
        out["flags"] = f.clone();
    }
    serde_json::from_value(out).map_err(|e| e.to_string())
}

fn parse_version(v: &str) -> (u32, u32) {
    let mut it = v.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).map(|x| x.parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

fn kernel_version() -> (u32, u32) {
    parse_version(&std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_profile() {
        let p = json!({
            "defaultAction": "SCMP_ACT_ERRNO", "defaultErrnoRet": 1,
            "archMap": [{ "architecture": "SCMP_ARCH_X86_64", "subArchitectures": ["SCMP_ARCH_X86", "SCMP_ARCH_X32"] },
                        { "architecture": "SCMP_ARCH_AARCH64", "subArchitectures": ["SCMP_ARCH_ARM"] }],
            "syscalls": [
                { "names": ["read", "write", "exit_group"], "action": "SCMP_ACT_ALLOW" },
                { "names": ["mount"], "action": "SCMP_ACT_ALLOW", "includes": { "caps": ["CAP_SYS_ADMIN"] } },
                { "names": ["chown"], "action": "SCMP_ACT_ALLOW", "excludes": { "caps": ["CAP_SYS_ADMIN"] } },
                { "names": ["personality"], "action": "SCMP_ACT_ALLOW", "args": [{ "index": 0, "value": 0, "op": "SCMP_CMP_EQ" }] }
            ]
        });
        let s = from_docker(&p, &[Capability::Chown]).unwrap();
        let names: Vec<String> = s.syscalls().as_ref().unwrap().iter().flat_map(|r| r.names().clone()).collect();
        assert!(names.contains(&"read".into()) && names.contains(&"chown".into()) && names.contains(&"personality".into()));
        assert!(!names.contains(&"mount".into()));
        if std::env::consts::ARCH == "x86_64" {
            assert_eq!(s.architectures().as_ref().unwrap().len(), 3);
        }
        let admin = from_docker(&p, &[Capability::SysAdmin]).unwrap();
        let names: Vec<String> = admin.syscalls().as_ref().unwrap().iter().flat_map(|r| r.names().clone()).collect();
        assert!(names.contains(&"mount".into()) && !names.contains(&"chown".into()));
    }

    #[test]
    fn caps_lift_rules() {
        let names = |p: &LinuxSeccomp| -> Vec<String> { p.syscalls().as_ref().unwrap()[0].names().clone() };
        let base = default_profile(&[Capability::Chown], false);
        let n = names(&base);
        for s in ["mount", "unshare", "bpf", "keyctl", "io_uring_setup", "kexec_load"] {
            assert!(n.iter().any(|x| x == s), "{s} should be refused");
        }
        assert!(base.syscalls().as_ref().unwrap().iter().any(|r| r.names() == &["clone3".to_string()]));
        let admin = default_profile(&[Capability::SysAdmin], false);
        let n = names(&admin);
        assert!(!n.iter().any(|x| x == "mount" || x == "unshare" || x == "bpf"));
        assert!(n.iter().any(|x| x == "keyctl")); // never lifted
        let desk = default_profile(&[Capability::Chown], true);
        let n = names(&desk);
        assert!(!n.iter().any(|x| x == "unshare" || x == "mount" || x == "setns"));
        assert!(n.iter().any(|x| x == "bpf" || x == "keyctl" || x == "kexec_load"));
        assert!(!desk.syscalls().as_ref().unwrap().iter().any(|r| r.names() == &["clone3".to_string()]));
        assert!(!admin.syscalls().as_ref().unwrap().iter().any(|r| r.names() == &["clone3".to_string()]));
    }
}
