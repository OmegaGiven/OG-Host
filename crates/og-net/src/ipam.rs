//! IPv4 address management: one JSON state file per network, guarded by an exclusive flock so
//! concurrent `run`s never hand out the same address.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// An IPv4 subnet like 10.89.0.0/24.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subnet {
    pub base: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    pub fn parse(s: &str) -> Result<Self> {
        let (ip, p) = s.split_once('/').ok_or_else(|| Error::Invalid(format!("subnet {s:?} needs /prefix")))?;
        let base: Ipv4Addr = ip.parse().map_err(|_| Error::Invalid(format!("bad subnet address {ip:?}")))?;
        let prefix: u8 = p.parse().map_err(|_| Error::Invalid(format!("bad prefix {p:?}")))?;
        if !(8..=30).contains(&prefix) {
            return Err(Error::Invalid(format!("prefix /{prefix} out of range (8-30)")));
        }
        let mask = u32::MAX << (32 - prefix);
        Ok(Subnet { base: Ipv4Addr::from(u32::from(base) & mask), prefix })
    }

    fn first(&self) -> u32 {
        u32::from(self.base)
    }

    fn size(&self) -> u32 {
        1u32 << (32 - self.prefix)
    }

    /// .1 of the subnet: the bridge's address and the containers' default route.
    pub fn gateway(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.first() + 1)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        let v = u32::from(ip);
        v >= self.first() && v < self.first() + self.size()
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.base, self.prefix)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// ip -> owner (container id)
    allocations: BTreeMap<Ipv4Addr, String>,
}

/// Run `f` on the state file while holding an exclusive lock on it.
fn with_state<T>(path: &Path, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut state: State = if buf.trim().is_empty() { State::default() } else { serde_json::from_str(&buf)? };
    let out = f(&mut state)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&serde_json::to_vec_pretty(&state)?)?;
    file.sync_all()?;
    Ok(out) // lock released when `file` drops
}

/// Allocate the lowest free address (skipping network, gateway and broadcast).
pub fn allocate(state_file: &Path, subnet: &Subnet, owner: &str) -> Result<Ipv4Addr> {
    with_state(state_file, |s| {
        if let Some((ip, _)) = s.allocations.iter().find(|(_, o)| o.as_str() == owner) {
            return Ok(*ip); // idempotent per owner
        }
        let (first, size) = (subnet.first(), subnet.size());
        for off in 2..size - 1 {
            let ip = Ipv4Addr::from(first + off);
            if !s.allocations.contains_key(&ip) {
                s.allocations.insert(ip, owner.to_string());
                return Ok(ip);
            }
        }
        Err(Error::Exhausted(subnet.to_string()))
    })
}

pub fn release(state_file: &Path, owner: &str) -> Result<()> {
    with_state(state_file, |s| {
        s.allocations.retain(|_, o| o != owner);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_parsing_and_gateway() {
        let s = Subnet::parse("10.89.0.77/24").unwrap();
        assert_eq!(s.to_string(), "10.89.0.0/24");
        assert_eq!(s.gateway(), Ipv4Addr::new(10, 89, 0, 1));
        assert!(s.contains("10.89.0.200".parse().unwrap()) && !s.contains("10.89.1.1".parse().unwrap()));
        assert!(Subnet::parse("10.0.0.0").is_err() && Subnet::parse("10.0.0.0/31").is_err());
    }

    #[test]
    fn allocates_lowest_free_reuses_and_exhausts() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("net.json");
        let s = Subnet::parse("10.0.0.0/29").unwrap(); // usable: .2 .. .6
        assert_eq!(allocate(&f, &s, "a").unwrap(), Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(allocate(&f, &s, "b").unwrap(), Ipv4Addr::new(10, 0, 0, 3));
        assert_eq!(allocate(&f, &s, "a").unwrap(), Ipv4Addr::new(10, 0, 0, 2), "same owner, same address");
        release(&f, "a").unwrap();
        assert_eq!(allocate(&f, &s, "c").unwrap(), Ipv4Addr::new(10, 0, 0, 2), "freed address is reused");
        for o in ["d", "e", "f"] {
            allocate(&f, &s, o).unwrap();
        }
        assert!(matches!(allocate(&f, &s, "g"), Err(Error::Exhausted(_))));
    }

    #[test]
    fn concurrent_allocations_never_collide() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("net.json");
        let s = Subnet::parse("10.1.0.0/24").unwrap();
        let handles: Vec<_> = (0..32)
            .map(|i| {
                let f = f.clone();
                std::thread::spawn(move || allocate(&f, &s, &format!("c{i}")).unwrap())
            })
            .collect();
        let mut ips: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        ips.sort();
        ips.dedup();
        assert_eq!(ips.len(), 32);
    }
}
