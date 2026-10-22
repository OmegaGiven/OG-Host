//! Apply image layers (tar, tar+gzip, tar+zstd) to a root filesystem directory.
//!
//! Security: every entry path is resolved with chroot semantics (`secure_join`): `..` can't
//! climb above the root, and symlinks met along the way (absolute or relative) are resolved
//! *inside* the root. Final components are never followed, so a symlink planted by one layer
//! can't redirect a later write outside the root. Files are opened with `O_NOFOLLOW`.
//!
//! Whiteouts (`.wh.<name>`) delete from lower layers; an opaque marker (`.wh..wh..opq`) clears
//! a directory's lower-layer contents.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE: &str = ".wh..wh..opq";
const MAX_SYMLINK_HOPS: usize = 255;

/// Options for applying layers.
#[derive(Debug, Clone, Default)]
pub struct UnpackOptions {
    /// Apply file ownership from the archive (requires root, or a user namespace).
    pub preserve_owner: bool,
    /// Add this to every uid/gid (user-namespace remapping, e.g. 100000).
    pub id_offset: u32,
    /// The layer goes into an empty directory of its own, to be stacked with overlayfs:
    /// whiteouts become overlayfs whiteouts (a 0:0 character device; `trusted.overlay.opaque`
    /// on directories) instead of deleting files (needs root).
    pub overlay: bool,
}

/// Open a layer blob, detecting gzip / zstd / plain tar from its magic bytes.
pub fn open_layer(path: &Path) -> Result<Box<dyn Read>> {
    let mut f = fs::File::open(path)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    let f = fs::File::open(path)?;
    let r: Box<dyn Read> = if n >= 2 && magic[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::MultiGzDecoder::new(io::BufReader::new(f)))
    } else if n == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Box::new(zstd::stream::read::Decoder::new(f)?)
    } else {
        Box::new(io::BufReader::new(f))
    };
    Ok(r)
}

/// Normalise an archive path into plain components, rejecting anything that tries to
/// escape (`..`) once leading `/` and `./` are stripped.
fn clean_rel(name: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in name.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(Error::UnsafePath(name.display().to_string()));
            }
        }
    }
    Ok(out)
}

/// Resolve `rel` under `root` as if `root` were `/`: symlinks in intermediate components
/// are followed but confined to `root`; the final component is not followed (right for
/// writes, which replace whatever is there).
pub fn secure_join(root: &Path, rel: &Path) -> Result<PathBuf> {
    resolve_in_root(root, rel, false)
}

/// Like [`secure_join`] but also follows a symlink in the final component, still confined to
/// `root`. Use this for reads (e.g. an image's /etc/passwd), never for writes.
pub fn secure_resolve(root: &Path, rel: &Path) -> Result<PathBuf> {
    resolve_in_root(root, rel, true)
}

fn resolve_in_root(root: &Path, rel: &Path, follow_last: bool) -> Result<PathBuf> {
    let mut parts: Vec<std::ffi::OsString> = rel.components().filter_map(|c| match c {
        Component::Normal(p) => Some(p.to_os_string()),
        _ => None,
    }).collect();
    parts.reverse(); // stack: pop from the end
    let mut current = PathBuf::new(); // relative to root, always clean
    let mut hops = 0;
    while let Some(part) = parts.pop() {
        if part == ".." {
            current.pop();
            continue;
        }
        let candidate = current.join(&part);
        let is_last = parts.is_empty();
        match fs::symlink_metadata(root.join(&candidate)) {
            Ok(m) if m.file_type().is_symlink() && (!is_last || follow_last) => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(Error::UnsafePath(format!("{}: too many symlinks", rel.display())));
                }
                let target = fs::read_link(root.join(&candidate))?;
                // push the target's components (in order) ahead of what's left
                let mut t: Vec<std::ffi::OsString> = Vec::new();
                for c in target.components() {
                    match c {
                        Component::RootDir => current = PathBuf::new(),
                        Component::ParentDir => t.push("..".into()),
                        Component::Normal(p) => t.push(p.to_os_string()),
                        _ => {}
                    }
                }
                for p in t.into_iter().rev() {
                    parts.push(p);
                }
            }
            _ => current = candidate,
        }
    }
    Ok(root.join(current))
}

fn cstr(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

fn set_xattr(p: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let (c, n) = (cstr(p)?, std::ffi::CString::new(name).unwrap());
    if unsafe { libc::lsetxattr(c.as_ptr(), n.as_ptr(), value.as_ptr().cast(), value.len(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An overlayfs whiteout: a character device 0:0.
fn mknod_whiteout(p: &Path) -> io::Result<()> {
    let c = cstr(p)?;
    if unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR | 0o000, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Apply one layer to `root`. `root` must exist.
pub fn apply_layer<R: Read>(root: &Path, layer: R, opts: &UnpackOptions) -> Result<LayerStats> {
    let mut archive = tar::Archive::new(layer);
    let mut created_here: HashSet<PathBuf> = HashSet::new();
    let mut stats = LayerStats::default();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.into_owned();
        let rel = clean_rel(&raw)?;
        if rel.as_os_str().is_empty() {
            continue; // the root itself
        }
        let file_name = rel.file_name().unwrap_or_default().to_string_lossy().to_string();
        let parent_rel = rel.parent().unwrap_or(Path::new("")).to_path_buf();

        // --- whiteouts ---
        if file_name == OPAQUE && opts.overlay {
            let dir = secure_join(root, &parent_rel)?;
            fs::create_dir_all(&dir)?;
            set_xattr(&dir, "trusted.overlay.opaque", b"y")?;
            stats.whiteouts += 1;
            continue;
        }
        if let Some(target) = file_name.strip_prefix(WHITEOUT_PREFIX).filter(|_| opts.overlay) {
            let at = secure_join(root, &parent_rel.join(target))?;
            if let Some(p) = at.parent() {
                fs::create_dir_all(p)?;
            }
            remove_any(&at)?;
            mknod_whiteout(&at)?;
            stats.whiteouts += 1;
            continue;
        }
        if file_name == OPAQUE {
            let dir = secure_join(root, &parent_rel)?;
            if let Ok(rd) = fs::read_dir(&dir) {
                for child in rd.flatten() {
                    let child_rel = parent_rel.join(child.file_name());
                    if !created_here.contains(&child_rel) {
                        remove_any(&child.path())?;
                    }
                }
            }
            stats.whiteouts += 1;
            continue;
        }
        if let Some(target) = file_name.strip_prefix(WHITEOUT_PREFIX) {
            let victim = secure_join(root, &parent_rel.join(target))?;
            remove_any(&victim)?;
            stats.whiteouts += 1;
            continue;
        }

        let dest = secure_join(root, &rel)?;
        if !dest.starts_with(root) {
            return Err(Error::UnsafePath(raw.display().to_string()));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let header = entry.header().clone();
        let mode = header.mode().unwrap_or(0o644) & 0o7777;
        let kind = header.entry_type();
        // anything already at the destination (other than a directory we merge into) is replaced
        if !(kind.is_dir() && fs::symlink_metadata(&dest).map(|m| m.is_dir()).unwrap_or(false)) {
            remove_any(&dest)?;
        }
        match kind {
            k if k.is_dir() => {
                fs::create_dir_all(&dest)?;
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode))?;
            }
            k if k.is_file() || k == tar::EntryType::Continuous => {
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&dest)?;
                stats.bytes += io::copy(&mut entry, &mut f)?;
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode))?; // umask-proof
            }
            k if k.is_symlink() => {
                let target = header.link_name()?.ok_or_else(|| Error::UnsafePath(raw.display().to_string()))?;
                // the symlink itself may point anywhere: it's resolved inside the container's root
                std::os::unix::fs::symlink(&*target, &dest)?;
            }
            k if k.is_hard_link() => {
                let target = header.link_name()?.ok_or_else(|| Error::UnsafePath(raw.display().to_string()))?;
                let src = secure_join(root, &clean_rel(&target)?)?;
                if !src.starts_with(root) {
                    return Err(Error::UnsafePath(target.display().to_string()));
                }
                fs::hard_link(&src, &dest)?;
            }
            k if k.is_character_special() || k.is_block_special() || k.is_fifo() => {
                if !make_node(&dest, &header, mode)? {
                    stats.skipped_devices += 1;
                    continue;
                }
            }
            _ => {
                // pax/gnu metadata entries are consumed by the tar crate; anything else is ignored
                stats.skipped_other += 1;
                continue;
            }
        }
        if opts.preserve_owner {
            let uid = header.uid().unwrap_or(0) as u32 + opts.id_offset;
            let gid = header.gid().unwrap_or(0) as u32 + opts.id_offset;
            std::os::unix::fs::lchown(&dest, Some(uid), Some(gid))?;
        }
        created_here.insert(rel);
        stats.entries += 1;
    }
    Ok(stats)
}

/// Device nodes and FIFOs need privileges; returns false (skipped) when not permitted.
fn make_node(dest: &Path, header: &tar::Header, mode: u32) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let kind = header.entry_type();
    let fmt = if kind.is_character_special() { libc::S_IFCHR } else if kind.is_block_special() { libc::S_IFBLK } else { libc::S_IFIFO };
    let dev = libc::makedev(
        header.device_major().ok().flatten().unwrap_or(0),
        header.device_minor().ok().flatten().unwrap_or(0),
    );
    let c = std::ffi::CString::new(dest.as_os_str().as_bytes()).map_err(|_| Error::UnsafePath(dest.display().to_string()))?;
    let rc = unsafe { libc::mknod(c.as_ptr(), fmt | mode as libc::mode_t, dev) };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EPERM) {
        tracing::debug!(path = %dest.display(), "skipping device node (no privilege)");
        Ok(false)
    } else {
        Err(err.into())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LayerStats {
    pub entries: u64,
    pub bytes: u64,
    pub whiteouts: u64,
    pub skipped_devices: u64,
    pub skipped_other: u64,
}

/// Is this file name a whiteout marker (for callers listing layer contents)?
pub fn is_whiteout(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with(WHITEOUT_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tar in memory from (path, kind, data/link) tuples.
    enum E<'a> {
        Dir(&'a str),
        File(&'a str, &'a [u8]),
        Sym(&'a str, &'a str),
        Hard(&'a str, &'a str),
    }

    fn tar_of(entries: &[E]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for e in entries {
            let mut h = tar::Header::new_gnu();
            match e {
                E::Dir(p) => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_mode(0o755);
                    h.set_size(0);
                    b.append_data(&mut h, p, &[][..]).unwrap();
                }
                E::File(p, d) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_mode(0o644);
                    h.set_size(d.len() as u64);
                    b.append_data(&mut h, p, *d).unwrap();
                }
                E::Sym(p, t) => {
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_size(0);
                    b.append_link(&mut h, p, t).unwrap();
                }
                E::Hard(p, t) => {
                    h.set_entry_type(tar::EntryType::Link);
                    h.set_size(0);
                    b.append_link(&mut h, p, t).unwrap();
                }
            }
        }
        b.into_inner().unwrap()
    }

    /// Like tar_of, but writes a raw (unsafe) name straight into the header, bypassing the
    /// tar crate's own path checks, to simulate a malicious archive.
    fn tar_raw_file(name: &str, data: &[u8]) -> Vec<u8> {
        let mut h = tar::Header::new_old();
        h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(data.len() as u64);
        h.set_cksum();
        let mut out = h.as_bytes().to_vec();
        out.extend_from_slice(data);
        out.resize(512 + data.len().div_ceil(512) * 512, 0);
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    fn apply(root: &Path, t: Vec<u8>) -> Result<LayerStats> {
        apply_layer(root, &t[..], &UnpackOptions::default())
    }

    #[test]
    fn basic_layers_and_whiteouts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        apply(root, tar_of(&[E::Dir("etc"), E::File("etc/a", b"1"), E::File("etc/b", b"2"), E::Dir("var/x"), E::File("var/x/old", b"o")])).unwrap();
        // layer 2: delete etc/a, make var/x opaque but add a new file in it
        apply(root, tar_of(&[E::File("etc/.wh.a", b""), E::File("var/x/.wh..wh..opq", b""), E::File("var/x/new", b"n")])).unwrap();
        assert!(!root.join("etc/a").exists());
        assert_eq!(fs::read(root.join("etc/b")).unwrap(), b"2");
        assert!(!root.join("var/x/old").exists());
        assert_eq!(fs::read(root.join("var/x/new")).unwrap(), b"n");
    }

    #[test]
    fn opaque_marker_after_same_layer_entries_keeps_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        apply(root, tar_of(&[E::Dir("d"), E::File("d/lower", b"l")])).unwrap();
        apply(root, tar_of(&[E::File("d/upper", b"u"), E::File("d/.wh..wh..opq", b"")])).unwrap();
        assert!(!root.join("d/lower").exists());
        assert!(root.join("d/upper").exists());
    }

    #[test]
    fn rejects_dotdot_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let err = apply(&root, tar_raw_file("../escaped", b"x")).unwrap_err();
        assert!(matches!(err, Error::UnsafePath(_)), "{err:?}");
        assert!(!dir.path().join("escaped").exists());
    }

    #[test]
    fn symlink_cannot_redirect_writes_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        // layer 1 plants an absolute symlink and a relative one that climbs out
        apply(&root, tar_of(&[E::Sym("abs", outside.to_str().unwrap()), E::Sym("rel", "../../../../outside")])).unwrap();
        // layer 2 writes "through" them
        apply(&root, tar_of(&[E::File("abs/pwned", b"x"), E::File("rel/pwned", b"y")])).unwrap();
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "nothing may be written outside the root");
        // both writes were confined to the root
        assert!(root.join(outside.strip_prefix("/").unwrap()).join("pwned").exists());
        assert!(root.join("outside/pwned").exists());
    }

    #[test]
    fn final_symlink_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("target-file");
        fs::write(&outside, b"keep").unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        apply(&root, tar_of(&[E::Sym("f", outside.to_str().unwrap())])).unwrap();
        apply(&root, tar_of(&[E::File("f", b"new")])).unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        assert_eq!(fs::read(root.join("f")).unwrap(), b"new");
    }

    #[test]
    fn hardlink_target_stays_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        apply(&root, tar_of(&[E::File("a", b"data"), E::Hard("b", "a")])).unwrap();
        assert_eq!(fs::read(root.join("b")).unwrap(), b"data");
    }

    #[test]
    fn secure_join_handles_dir_symlinks_like_a_chroot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink("usr/lib", root.join("lib")).unwrap();
        std::os::unix::fs::symlink("/usr/lib", root.join("lib64")).unwrap();
        assert_eq!(secure_join(root, Path::new("lib/x.so")).unwrap(), root.join("usr/lib/x.so"));
        assert_eq!(secure_join(root, Path::new("lib64/x.so")).unwrap(), root.join("usr/lib/x.so"));
        assert_eq!(secure_join(root, Path::new("../../../etc/passwd")).unwrap(), root.join("etc/passwd"));
        // reads may follow a final symlink, but only inside the root
        std::os::unix::fs::symlink("/etc/hostname", root.join("h")).unwrap();
        assert_eq!(secure_join(root, Path::new("h")).unwrap(), root.join("h"));
        assert_eq!(secure_resolve(root, Path::new("h")).unwrap(), root.join("etc/hostname"));
        // symlink loop is cut off, not followed forever
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        assert!(secure_join(root, Path::new("loop/x")).is_err());
    }
}
