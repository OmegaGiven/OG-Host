//! Resolve Docker-style `User` strings against the container's own /etc/passwd and /etc/group.
//! Files are read through `og_oci::unpack::secure_resolve`, so a symlinked /etc/passwd inside
//! an image can't make us read host files.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
    pub additional_gids: Vec<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("user {0:?} not found in the image's /etc/passwd")]
    NoUser(String),
    #[error("group {0:?} not found in the image's /etc/group")]
    NoGroup(String),
}

fn read_in_root(root: &Path, rel: &str) -> String {
    og_oci::unpack::secure_resolve(root, Path::new(rel))
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

struct PasswdEntry {
    name: String,
    uid: u32,
    gid: u32,
}

fn passwd(root: &Path) -> Vec<PasswdEntry> {
    read_in_root(root, "etc/passwd")
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 4).then(|| Some(PasswdEntry { name: f[0].into(), uid: f[2].parse().ok()?, gid: f[3].parse().ok()? }))?
        })
        .collect()
}

/// (name, gid, members)
fn groups(root: &Path) -> Vec<(String, u32, Vec<String>)> {
    read_in_root(root, "etc/group")
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 3 {
                return None;
            }
            let members = f.get(3).map(|m| m.split(',').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default();
            Some((f[0].to_string(), f[2].parse().ok()?, members))
        })
        .collect()
}

/// `None`/empty = root. Numeric ids are accepted even when absent from /etc/passwd (like Docker).
pub fn resolve(root: &Path, spec: Option<&str>) -> Result<Ids, UserError> {
    let Some(spec) = spec.filter(|s| !s.is_empty()) else {
        return Ok(Ids { uid: 0, gid: 0, additional_gids: vec![] });
    };
    let (u, g) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let pw = passwd(root);
    let (uid, name, mut gid) = match u.parse::<u32>() {
        Ok(n) => {
            let e = pw.iter().find(|e| e.uid == n);
            (n, e.map(|e| e.name.clone()), e.map(|e| e.gid).unwrap_or(n))
        }
        Err(_) => {
            let e = pw.iter().find(|e| e.name == u).ok_or_else(|| UserError::NoUser(u.to_string()))?;
            (e.uid, Some(e.name.clone()), e.gid)
        }
    };
    let gr = groups(root);
    if let Some(g) = g {
        gid = match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => gr.iter().find(|(n, _, _)| n == g).map(|(_, id, _)| *id).ok_or_else(|| UserError::NoGroup(g.to_string()))?,
        };
    }
    let additional_gids = match &name {
        Some(n) => gr.iter().filter(|(_, id, m)| m.contains(n) && *id != gid).map(|(_, id, _)| *id).collect(),
        None => vec![],
    };
    Ok(Ids { uid, gid, additional_gids })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_with(passwd: &str, group: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("etc")).unwrap();
        std::fs::write(d.path().join("etc/passwd"), passwd).unwrap();
        std::fs::write(d.path().join("etc/group"), group).unwrap();
        d
    }

    #[test]
    fn resolves_names_numbers_and_groups() {
        let r = root_with("root:x:0:0::/root:/bin/sh\nnginx:x:101:101::/var/cache/nginx:/sbin/nologin\n", "root:x:0:\nnginx:x:101:\nwww-data:x:82:nginx\n");
        assert_eq!(resolve(r.path(), None).unwrap(), Ids { uid: 0, gid: 0, additional_gids: vec![] });
        assert_eq!(resolve(r.path(), Some("nginx")).unwrap(), Ids { uid: 101, gid: 101, additional_gids: vec![82] });
        assert_eq!(resolve(r.path(), Some("1000:1000")).unwrap(), Ids { uid: 1000, gid: 1000, additional_gids: vec![] });
        assert_eq!(resolve(r.path(), Some("nginx:www-data")).unwrap().gid, 82);
        assert!(resolve(r.path(), Some("nobody-here")).is_err());
        assert!(resolve(r.path(), Some("nginx:nogroup")).is_err());
    }

    #[test]
    fn symlinked_passwd_stays_inside_root() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("etc")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", d.path().join("etc/passwd")).unwrap();
        // resolves to <root>/etc/passwd -> itself (a loop inside the root), never the host file
        assert!(resolve(d.path(), Some("root")).is_err());
    }
}
