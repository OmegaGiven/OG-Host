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

/// The profile for a container holding `caps`.
pub fn default_profile(caps: &[Capability]) -> LinuxSeccomp {
    let has = |c: &[Capability]| c.iter().any(|x| caps.contains(x));
    let mut names: Vec<&str> = Vec::new();
    for (calls, lift) in RULES {
        if lift.is_empty() || !has(lift) {
            names.extend_from_slice(calls);
        }
    }
    let mut syscalls: Vec<Value> = vec![json!({ "names": names, "action": "SCMP_ACT_ERRNO", "errnoRet": EPERM })];
    if !caps.contains(&Capability::SysAdmin) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_lift_rules() {
        let names = |p: &LinuxSeccomp| -> Vec<String> { p.syscalls().as_ref().unwrap()[0].names().clone() };
        let base = default_profile(&[Capability::Chown]);
        let n = names(&base);
        for s in ["mount", "unshare", "bpf", "keyctl", "io_uring_setup", "kexec_load"] {
            assert!(n.iter().any(|x| x == s), "{s} should be refused");
        }
        assert!(base.syscalls().as_ref().unwrap().iter().any(|r| r.names() == &["clone3".to_string()]));
        let admin = default_profile(&[Capability::SysAdmin]);
        let n = names(&admin);
        assert!(!n.iter().any(|x| x == "mount" || x == "unshare" || x == "bpf"));
        assert!(n.iter().any(|x| x == "keyctl")); // never lifted
        assert!(!admin.syscalls().as_ref().unwrap().iter().any(|r| r.names() == &["clone3".to_string()]));
    }
}
