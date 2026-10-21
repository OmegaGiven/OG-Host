//! Sandboxes: a user namespace + a network namespace it owns, pinned as bind-mounted files.
//!
//! Why: the kernel only lets a process join a network namespace if it has CAP_SYS_ADMIN in the
//! user namespace that *owns* it. A container in its own user namespace therefore can't join a
//! netns the host created. So we create the user namespace first (with the container id
//! mapping), create the netns from inside it, and let containers join both. Host root still
//! administers the netns (it owns the parent user namespace), so og-net can wire veths into it.
//! Containers sharing a sandbox (compose `network_mode: service:x`) share both namespaces.
//!
//! Nothing keeps running: the helper child exits once the namespaces are pinned.

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[derive(clap::Args)]
pub struct SandboxArgs {
    /// Directory to pin the namespaces in (gets `user` and `net` files).
    #[arg(long)]
    pub dir: PathBuf,
    /// Also pin the netns here (e.g. /run/netns/og-<id>) so `ip -n` can address it.
    #[arg(long)]
    pub netns_link: Option<PathBuf>,
    /// Host uid/gid the sandbox's id 0 maps to.
    #[arg(long, default_value_t = 100_000)]
    pub host_start: u32,
    /// Number of ids mapped.
    #[arg(long, default_value_t = 65_536)]
    pub size: u32,
}

fn pipe() -> Result<(std::fs::File, std::fs::File)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        bail!("pipe2: {}", std::io::Error::last_os_error());
    }
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok((std::fs::File::from(r), std::fs::File::from(w)))
}

fn cstr(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).context("path contains NUL")
}

fn bind(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !dst.exists() {
        std::fs::write(dst, b"")?;
    }
    let (s, d) = (cstr(src)?, cstr(dst)?);
    if unsafe { libc::mount(s.as_ptr(), d.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) } != 0 {
        bail!("bind {} -> {}: {}", src.display(), dst.display(), std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn create(a: &SandboxArgs) -> Result<()> {
    std::fs::create_dir_all(&a.dir)?;
    let (mut child_rx, mut parent_tx) = pipe()?; // parent -> child
    let (mut parent_rx, mut child_tx) = pipe()?; // child -> parent

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", std::io::Error::last_os_error());
    }
    if pid == 0 {
        // ---- child: only async-signal-safe-ish syscalls from here, then _exit ----
        drop(parent_tx);
        drop(parent_rx);
        let mut b = [0u8; 1];
        let code = (|| -> i32 {
            if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
                return 10;
            }
            if child_tx.write_all(b"u").is_err() || child_rx.read_exact(&mut b).is_err() {
                return 11; // parent writes our id maps, then lets us continue
            }
            if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
                return 12;
            }
            if child_tx.write_all(b"n").is_err() || child_rx.read_exact(&mut b).is_err() {
                return 13; // parent pins both namespaces, then lets us exit
            }
            0
        })();
        unsafe { libc::_exit(code) };
    }

    // ---- parent ----
    drop(child_rx);
    drop(child_tx);
    let fail = |msg: String| -> Result<()> {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let mut st = 0;
        unsafe { libc::waitpid(pid, &mut st, 0) };
        bail!(msg)
    };
    let mut b = [0u8; 1];
    if parent_rx.read_exact(&mut b).is_err() {
        return fail("sandbox helper failed to create a user namespace".into());
    }
    let map = format!("0 {} {}\n", a.host_start, a.size);
    for f in ["uid_map", "gid_map"] {
        if let Err(e) = std::fs::write(format!("/proc/{pid}/{f}"), &map) {
            return fail(format!("writing {f}: {e}"));
        }
    }
    parent_tx.write_all(b"m")?;
    if parent_rx.read_exact(&mut b).is_err() {
        return fail("sandbox helper failed to create a network namespace".into());
    }
    let user = a.dir.join("user");
    let net = a.dir.join("net");
    let pinned = (|| -> Result<()> {
        bind(Path::new(&format!("/proc/{pid}/ns/user")), &user)?;
        bind(Path::new(&format!("/proc/{pid}/ns/net")), &net)?;
        if let Some(link) = &a.netns_link {
            bind(Path::new(&format!("/proc/{pid}/ns/net")), link)?;
        }
        Ok(())
    })();
    if let Err(e) = pinned {
        return fail(format!("{e:#}"));
    }
    parent_tx.write_all(b"x")?;
    let mut st = 0;
    unsafe { libc::waitpid(pid, &mut st, 0) };
    if !(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0) {
        bail!("sandbox helper exited with status {st}");
    }
    println!("{}", a.dir.display());
    Ok(())
}
