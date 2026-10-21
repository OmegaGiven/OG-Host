//! ID-mapped bind mounts (Linux 5.12+): show a host directory to a user-namespaced container
//! with ownership translated through the container's id mapping.
//!
//! With container 0 -> host 100000, a file owned by host uid 1000 appears inside as uid 1000
//! and is written back as 1000. Container processes stay unprivileged on the host, yet bind
//! mounts behave as they do under Docker without user namespaces.
//!
//! The mapped mount is built on the host: clone the source tree (open_tree), attach the
//! sandbox's user namespace (mount_setattr MOUNT_ATTR_IDMAP), and pin it at a staging path
//! (move_mount). The container then bind-mounts the staging path; a bind of an id-mapped mount
//! keeps its mapping.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

const OPEN_TREE_CLONE: libc::c_uint = 1;
const OPEN_TREE_CLOEXEC: libc::c_uint = libc::O_CLOEXEC as libc::c_uint;
const MOUNT_ATTR_IDMAP: u64 = 0x0010_0000;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x0000_0004;

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

fn cstr(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).context("path contains NUL")
}

/// Create an id-mapped copy of `source` at `staging` using the user namespace file `userns`.
pub fn map_mount(source: &Path, staging: &Path, userns: &Path) -> Result<()> {
    let src = cstr(source)?;
    let tree = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            src.as_ptr(),
            OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | libc::AT_RECURSIVE as libc::c_uint,
        )
    };
    if tree < 0 {
        bail!("open_tree({}): {}", source.display(), std::io::Error::last_os_error());
    }
    let tree = unsafe { OwnedFd::from_raw_fd(tree as i32) };
    let ns = std::fs::File::open(userns).with_context(|| format!("opening {}", userns.display()))?;
    let attr = MountAttr { attr_set: MOUNT_ATTR_IDMAP, attr_clr: 0, propagation: 0, userns_fd: ns.as_raw_fd() as u64 };
    let empty = CString::new("").unwrap();
    let r = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            empty.as_ptr(),
            (libc::AT_EMPTY_PATH | libc::AT_RECURSIVE) as libc::c_uint,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if r < 0 {
        let e = std::io::Error::last_os_error();
        bail!(
            "id-mapping {} failed ({e}); its filesystem may not support id-mapped mounts (FUSE/NFS); \
             run this container without a user namespace or use a volume",
            source.display()
        );
    }
    // staging target must exist with the same type (dir or file)
    if let Some(p) = staging.parent() {
        std::fs::create_dir_all(p)?;
    }
    if source.is_dir() {
        std::fs::create_dir_all(staging)?;
    } else if !staging.exists() {
        std::fs::write(staging, b"")?;
    }
    let dst = cstr(staging)?;
    let r = unsafe {
        libc::syscall(libc::SYS_move_mount, tree.as_raw_fd(), empty.as_ptr(), libc::AT_FDCWD, dst.as_ptr(), MOVE_MOUNT_F_EMPTY_PATH)
    };
    if r < 0 {
        bail!("move_mount -> {}: {}", staging.display(), std::io::Error::last_os_error());
    }
    Ok(())
}

/// Unmount and remove every staging mount for a container.
pub fn cleanup(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p: PathBuf = e.path();
        if let Ok(c) = cstr(&p) {
            unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
        }
        let _ = std::fs::remove_dir(&p).or_else(|_| std::fs::remove_file(&p));
    }
    let _ = std::fs::remove_dir(dir);
}
