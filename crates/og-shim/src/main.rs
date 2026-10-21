//! og-shim — one per container, single-threaded on purpose (libcontainer forks).
//!
//! Lifecycle: become a child subreaper so the container's init ends up as our child, create
//! and start the container from its bundle, wait for it, forward SIGTERM/SIGINT/SIGHUP to it,
//! then record the exit status and clean up libcontainer's state (which frees the cgroup).
//! Because the shim, not og-hostd, is the container's parent, restarting og-hostd never
//! takes running containers down with it.

use std::fs::OpenOptions;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{bail, Context, Result};
use clap::Parser;
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::syscall::syscall::SyscallType;
use serde::Serialize;

#[derive(Parser)]
#[command(name = "og-shim", about = "OG-Host per-container supervisor")]
struct Args {
    /// Container id (also the libcontainer state directory name).
    #[arg(long)]
    id: String,
    /// OCI bundle directory (holds config.json).
    #[arg(long)]
    bundle: PathBuf,
    /// libcontainer state root.
    #[arg(long, default_value = "/run/og-host/runtime")]
    state_root: PathBuf,
    /// File receiving the container's stdout and stderr.
    #[arg(long)]
    log: PathBuf,
    /// Where to write {"pid":..} once started and {"exit_code":..} at the end.
    #[arg(long)]
    status: PathBuf,
}

#[derive(Serialize)]
struct Status {
    state: &'static str,
    pid: Option<i32>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    started_at: Option<u64>,
    finished_at: Option<u64>,
    error: Option<String>,
}

static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// (read, write) pipe; the read end is close-on-exec so the container never inherits it.
fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        bail!("pipe2: {}", std::io::Error::last_os_error());
    }
    // the write end must survive exec into the container; libcontainer dup2()s it onto 1/2
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Host uid/gid that the container's root maps to, from the bundle's user-namespace mappings.
fn mapped_root(bundle: &std::path::Path) -> Option<(u32, u32)> {
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(bundle.join("config.json")).ok()?).ok()?;
    let host = |k: &str| -> Option<u32> {
        cfg["linux"][k].as_array()?.iter().find(|m| m["containerID"].as_u64() == Some(0))?["hostID"].as_u64().map(|v| v as u32)
    };
    Some((host("uidMappings")?, host("gidMappings")?))
}

extern "C" fn on_signal(sig: libc::c_int) {
    PENDING_SIGNAL.store(sig, Ordering::SeqCst);
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn write_status(path: &PathBuf, s: &Status) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(&args) {
        write_status(&args.status, &Status { state: "failed", pid: None, exit_code: None, signal: None, started_at: None, finished_at: Some(now()), error: Some(format!("{e:#}")) });
        eprintln!("og-shim: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<()> {
    // our orphaned descendants (the container's init) are re-parented to us
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        bail!("prctl(PR_SET_CHILD_SUBREAPER): {}", std::io::Error::last_os_error());
    }
    std::fs::create_dir_all(&args.state_root)?;
    let mut log = OpenOptions::new().create(true).append(true).open(&args.log).with_context(|| format!("opening {}", args.log.display()))?;

    // stdout/stderr go through a pipe, not the log file: inside a user namespace the container
    // re-opens /dev/stdout (/proc/self/fd/1) with its *mapped* uid, so the pipe must belong to
    // the container's root (what runc does too). The shim copies the pipe into the log.
    let (read_end, write_end) = pipe()?;
    if let Some((uid, gid)) = mapped_root(&args.bundle) {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fchown(write_end.as_raw_fd(), uid, gid) } != 0 {
            bail!("fchown stdio pipe: {}", std::io::Error::last_os_error());
        }
    }
    let out: OwnedFd = write_end.try_clone()?;
    let err: OwnedFd = write_end;

    let mut container = ContainerBuilder::new(args.id.clone(), SyscallType::default())
        .with_root_path(&args.state_root)
        .context("state root")?
        .with_stdout(out)
        .with_stderr(err)
        .validate_id()
        .context("container id")?
        .as_init(&args.bundle)
        .with_systemd(false)
        .with_detach(true)
        .build()
        .context("creating container")?;
    container.start().context("starting container")?;
    // copy container output to the log until every writer (the container's processes) is gone;
    // threads are safe now, libcontainer's forking is done
    let copier = std::thread::spawn(move || {
        let mut r = std::fs::File::from(read_end);
        let _ = std::io::copy(&mut r, &mut log);
    });
    let pid = container.pid().map(|p| p.as_raw()).context("container has no pid after start")?;
    let started_at = now();
    write_status(&args.status, &Status { state: "running", pid: Some(pid), exit_code: None, signal: None, started_at: Some(started_at), finished_at: None, error: None });

    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }

    // reap until the container's init exits; forward signals we receive meanwhile
    let (exit_code, signal) = loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(-1, &mut status, 0) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                let s = PENDING_SIGNAL.swap(0, Ordering::SeqCst);
                if s != 0 {
                    unsafe { libc::kill(pid, s) };
                }
                continue;
            }
            if e.raw_os_error() == Some(libc::ECHILD) {
                break (None, None); // nothing left to wait for
            }
            bail!("waitpid: {e}");
        }
        if r == pid {
            if libc::WIFEXITED(status) {
                break (Some(libc::WEXITSTATUS(status)), None);
            }
            if libc::WIFSIGNALED(status) {
                let s = libc::WTERMSIG(status);
                break (Some(128 + s), Some(s));
            }
        }
        // some other orphan was reaped; keep waiting for the container's init
    };

    let _ = copier.join();
    // libcontainer state + cgroup cleanup
    let _ = container.refresh_status();
    if let Err(e) = container.delete(true) {
        eprintln!("og-shim: cleanup of {}: {e}", args.id);
    }
    write_status(&args.status, &Status { state: "exited", pid: Some(pid), exit_code, signal, started_at: Some(started_at), finished_at: Some(now()), error: None });
    Ok(())
}
