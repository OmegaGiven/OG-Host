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

mod sandbox;

#[derive(Parser)]
#[command(name = "og-shim", about = "OG-Host per-container supervisor")]
enum Cli {
    /// Create, start and supervise one container.
    Run(Args),
    /// Run an extra process inside a running container (docker exec): joins its namespaces and
    /// cgroup and keeps its capability bounding set, seccomp filter and no_new_privs. Uses our
    /// stdin/stdout/stderr; exits with the process's exit code.
    Exec(ExecArgs),
    /// Create a sandbox: a user namespace with the given id mapping and a network namespace
    /// owned by it, pinned as files so containers can join both later.
    Sandbox(sandbox::SandboxArgs),
}

#[derive(clap::Args)]
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
    /// Host uid:gid of the container's root (owner of its stdio pipe). Needed when the
    /// container joins a sandbox's user namespace, as its config then has no id mappings.
    #[arg(long)]
    stdio_owner: Option<String>,
}

#[derive(clap::Args)]
struct ExecArgs {
    #[arg(long)]
    id: String,
    #[arg(long, default_value = "/run/og-host/runtime")]
    state_root: PathBuf,
    /// uid:gid inside the container (default: root).
    #[arg(long)]
    user: Option<String>,
    #[arg(long)]
    additional_gids: Vec<u32>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// KEY=VALUE, repeatable; added to the container's environment.
    #[arg(long)]
    env: Vec<String>,
    /// Host uid:gid of the container's root; our stdio pipes are handed to it (see run).
    #[arg(long)]
    stdio_owner: Option<String>,
    #[arg(trailing_var_arg = true, required = true)]
    argv: Vec<String>,
}

#[derive(Serialize)]
struct Status {
    state: &'static str,
    pid: Option<i32>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    oom_killed: bool,
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
    // OG_SHIM_DEBUG=1 (inherited from og-hostd): the runtime's own diagnostics go to shim.log
    if std::env::var_os("OG_SHIM_DEBUG").is_some() {
        let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false).with_env_filter(tracing_subscriber::EnvFilter::new("debug")).try_init();
    }
    // containers and exec'd processes must not inherit our supplementary groups (root's gid 0
    // when og-hostd runs as root): the runtime only sets groups that a container asks for
    if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
        eprintln!("og-shim: clearing supplementary groups: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }
    let args = match Cli::parse() {
        Cli::Run(a) => a,
        Cli::Sandbox(a) => {
            if let Err(e) = sandbox::create(&a) {
                eprintln!("og-shim sandbox: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        Cli::Exec(a) => match exec(&a) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("og-shim exec: {e:#}");
                std::process::exit(126);
            }
        },
    };
    if let Err(e) = run(&args) {
        write_status(&args.status, &Status { state: "failed", pid: None, exit_code: None, signal: None, oom_killed: false, started_at: None, finished_at: Some(now()), error: Some(format!("{e:#}")) });
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
    let owner = args
        .stdio_owner
        .as_deref()
        .and_then(|o| o.split_once(':'))
        .and_then(|(u, g)| Some((u.parse().ok()?, g.parse().ok()?)))
        .or_else(|| mapped_root(&args.bundle));
    if let Some((uid, gid)) = owner {
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
    write_status(&args.status, &Status { state: "running", pid: Some(pid), exit_code: None, signal: None, oom_killed: false, started_at: Some(started_at), finished_at: None, error: None });

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
    let oom_killed = oom_kills(&args.bundle) > 0;
    // libcontainer state + cgroup cleanup
    let _ = container.refresh_status();
    if let Err(e) = container.delete(true) {
        eprintln!("og-shim: cleanup of {}: {e}", args.id);
    }
    write_status(&args.status, &Status { state: "exited", pid: Some(pid), exit_code, signal, oom_killed, started_at: Some(started_at), finished_at: Some(now()), error: None });
    Ok(())
}

/// OOM kills in the container's cgroup (memory.events), read before the cgroup is removed.
fn oom_kills(bundle: &std::path::Path) -> u64 {
    let cfg: Option<serde_json::Value> = std::fs::read(bundle.join("config.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let Some(cg) = cfg.as_ref().and_then(|c| c["linux"]["cgroupsPath"].as_str().map(String::from)) else { return 0 };
    let path = PathBuf::from("/sys/fs/cgroup").join(cg.trim_start_matches('/')).join("memory.events");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("oom_kill ").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(0)
}

fn parse_pair(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

fn exec(args: &ExecArgs) -> Result<i32> {
    use std::os::fd::FromRawFd;
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        bail!("prctl(PR_SET_CHILD_SUBREAPER): {}", std::io::Error::last_os_error());
    }
    // only pipes/sockets are handed over; never chown a real file such as /dev/null
    if let Some((uid, gid)) = args.stdio_owner.as_deref().and_then(parse_pair) {
        for fd in 0..3 {
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut st) } == 0 {
                let kind = st.st_mode & libc::S_IFMT;
                if kind == libc::S_IFIFO || kind == libc::S_IFSOCK {
                    unsafe { libc::fchown(fd, uid, gid) };
                }
            }
        }
    }
    let dup = |fd: i32| -> Result<OwnedFd> {
        let n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if n < 0 {
            bail!("dup fd {fd}: {}", std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(n) })
    };
    let (uid, gid) = match args.user.as_deref() {
        Some(u) => parse_pair(u).context("--user must be uid:gid")?,
        None => (0, 0),
    };
    let env = args.env.iter().filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let (stdin, stdout, stderr) = (dup(0)?, dup(1)?, dup(2)?);
    let pid = ContainerBuilder::new(args.id.clone(), SyscallType::default())
        .with_root_path(&args.state_root)
        .context("state root")?
        .with_stdin(stdin)
        .with_stdout(stdout)
        .with_stderr(stderr)
        .as_tenant()
        .with_container_args(args.argv.clone())
        .with_env(env)
        .with_cwd(args.cwd.clone())
        .with_user(Some(uid))
        .with_group(Some(gid))
        .with_additional_gids(args.additional_gids.clone())
        .with_detach(true)
        .build()
        .context("exec in container")?
        .as_raw();
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
    loop {
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
            bail!("waitpid: {e}");
        }
        if r == pid {
            if libc::WIFEXITED(status) {
                return Ok(libc::WEXITSTATUS(status));
            }
            if libc::WIFSIGNALED(status) {
                return Ok(128 + libc::WTERMSIG(status));
            }
        }
    }
}
