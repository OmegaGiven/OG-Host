//! A small DHCPv4 server for VMs on OG-Host networks: it answers only MAC addresses it has a
//! lease for (the VM's address from the network's IPAM), on that network's bridge. Containers
//! get their address directly; this is just how a VM's own OS learns it.

use std::collections::HashMap;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::FromRawFd;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, Clone)]
pub struct Lease {
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    pub dns: Vec<Ipv4Addr>,
    pub hostname: String,
    pub bridge: String,
}

#[derive(Default)]
pub struct Dhcp {
    leases: Arc<RwLock<HashMap<[u8; 6], Lease>>>,
    bridges: Mutex<HashMap<String, ()>>,
}

fn parse_mac(m: &str) -> Option<[u8; 6]> {
    let v: Vec<u8> = m.split(':').filter_map(|p| u8::from_str_radix(p, 16).ok()).collect();
    v.try_into().ok()
}

impl Dhcp {
    pub fn set(&self, mac: &str, lease: Lease) {
        let Some(m) = parse_mac(mac) else { return };
        let bridge = lease.bridge.clone();
        self.leases.write().unwrap().insert(m, lease);
        self.serve(&bridge);
    }

    pub fn remove(&self, mac: &str) {
        if let Some(m) = parse_mac(mac) {
            self.leases.write().unwrap().remove(&m);
        }
    }

    /// The address a VM's NIC was given.
    pub fn ip_of(&self, mac: &str) -> Option<Ipv4Addr> {
        parse_mac(mac).and_then(|m| self.leases.read().unwrap().get(&m).map(|l| l.ip))
    }

    /// Listen on a bridge (once).
    fn serve(&self, bridge: &str) {
        let mut b = self.bridges.lock().unwrap();
        if b.contains_key(bridge) {
            return;
        }
        let sock = match bind(bridge) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("og-hostd: DHCP on {bridge}: {e}");
                return;
            }
        };
        b.insert(bridge.to_string(), ());
        let leases = self.leases.clone();
        let br = bridge.to_string();
        std::thread::Builder::new()
            .name(format!("dhcp-{br}"))
            .spawn(move || {
                let mut buf = [0u8; 1500];
                loop {
                    let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
                    if let Some(reply) = answer(&buf[..n], &leases.read().unwrap(), &br) {
                        let _ = sock.send_to(&reply, "255.255.255.255:68");
                    }
                }
            })
            .ok();
    }
}

fn bind(bridge: &str) -> std::io::Result<UdpSocket> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let one: libc::c_int = 1;
        let sz = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, &one as *const _ as *const libc::c_void, sz);
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_BROADCAST, &one as *const _ as *const libc::c_void, sz);
        let name = std::ffi::CString::new(bridge).unwrap();
        if libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_BINDTODEVICE, name.as_ptr() as *const libc::c_void, bridge.len() as libc::socklen_t + 1) != 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        let addr = libc::sockaddr_in { sin_family: libc::AF_INET as u16, sin_port: 67u16.to_be(), sin_addr: libc::in_addr { s_addr: 0 }, sin_zero: [0; 8] };
        if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t) != 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(UdpSocket::from_raw_fd(fd))
    }
}

const COOKIE: [u8; 4] = [99, 130, 83, 99];

fn options(p: &[u8]) -> HashMap<u8, &[u8]> {
    let mut o = HashMap::new();
    let mut i = 240;
    while i < p.len() {
        let code = p[i];
        if code == 255 {
            break;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        if i + 1 >= p.len() {
            break;
        }
        let len = p[i + 1] as usize;
        if i + 2 + len > p.len() {
            break;
        }
        o.insert(code, &p[i + 2..i + 2 + len]);
        i += 2 + len;
    }
    o
}

/// Build the reply to one request, if it's for a MAC we know on this bridge.
pub fn answer(p: &[u8], leases: &HashMap<[u8; 6], Lease>, bridge: &str) -> Option<Vec<u8>> {
    if p.len() < 240 || p[0] != 1 || p[1] != 1 || p[2] != 6 || p[236..240] != COOKIE {
        return None;
    }
    let mac: [u8; 6] = p[28..34].try_into().ok()?;
    let l = leases.get(&mac).filter(|l| l.bridge == bridge)?;
    let o = options(p);
    let kind = *o.get(&53)?.first()?;
    let reply_type = match kind {
        1 => 2, // DISCOVER -> OFFER
        3 => {
            // REQUEST -> ACK, or NAK when it asks for another address
            let asked = o.get(&50).and_then(|v| <[u8; 4]>::try_from(*v).ok()).map(Ipv4Addr::from).or_else(|| {
                let ci = Ipv4Addr::new(p[12], p[13], p[14], p[15]);
                (!ci.is_unspecified()).then_some(ci)
            });
            if asked.is_some_and(|a| a != l.ip) {
                6
            } else {
                5
            }
        }
        8 => 5, // INFORM -> ACK
        _ => return None,
    };
    let mut r = vec![0u8; 240];
    r[0] = 2;
    r[1] = 1;
    r[2] = 6;
    r[4..8].copy_from_slice(&p[4..8]); // xid
    r[10..12].copy_from_slice(&p[10..12]); // flags
    if reply_type != 6 && kind != 8 {
        r[16..20].copy_from_slice(&l.ip.octets()); // yiaddr
    }
    r[20..24].copy_from_slice(&l.gateway.octets()); // siaddr
    r[28..44].copy_from_slice(&p[28..44]); // chaddr
    r[236..240].copy_from_slice(&COOKIE);
    let mut opt = |code: u8, v: &[u8]| {
        r.push(code);
        r.push(v.len() as u8);
        r.extend_from_slice(v);
    };
    opt(53, &[reply_type]);
    opt(54, &l.gateway.octets());
    if reply_type != 6 {
        let mask = if l.prefix == 0 { 0 } else { u32::MAX << (32 - l.prefix) };
        opt(51, &86400u32.to_be_bytes());
        opt(58, &43200u32.to_be_bytes());
        opt(59, &75600u32.to_be_bytes());
        opt(1, &mask.to_be_bytes());
        opt(3, &l.gateway.octets());
        let dns: Vec<u8> = l.dns.iter().take(3).flat_map(|d| d.octets()).collect();
        if !dns.is_empty() {
            opt(6, &dns);
        }
        let h = l.hostname.as_bytes();
        if !h.is_empty() && h.len() < 64 {
            opt(12, h);
        }
        opt(26, &1500u16.to_be_bytes());
    }
    r.push(255);
    while r.len() < 300 {
        r.push(0);
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: u8, mac: [u8; 6], asked: Option<Ipv4Addr>) -> Vec<u8> {
        let mut p = vec![0u8; 240];
        p[0] = 1;
        p[1] = 1;
        p[2] = 6;
        p[4..8].copy_from_slice(&[1, 2, 3, 4]);
        p[28..34].copy_from_slice(&mac);
        p[236..240].copy_from_slice(&COOKIE);
        p.extend_from_slice(&[53, 1, kind]);
        if let Some(a) = asked {
            p.push(50);
            p.push(4);
            p.extend_from_slice(&a.octets());
        }
        p.push(255);
        p
    }

    #[test]
    fn offer_ack_nak() {
        let mac = [0x52, 0x54, 0, 1, 2, 3];
        let mut leases = HashMap::new();
        leases.insert(mac, Lease { ip: Ipv4Addr::new(10, 89, 0, 7), prefix: 24, gateway: Ipv4Addr::new(10, 89, 0, 1), dns: vec![Ipv4Addr::new(1, 1, 1, 1)], hostname: "web".into(), bridge: "og0".into() });
        let offer = answer(&req(1, mac, None), &leases, "og0").unwrap();
        assert_eq!(offer[0], 2);
        assert_eq!(&offer[16..20], &[10, 89, 0, 7]);
        assert_eq!(options(&offer)[&53], &[2]);
        assert_eq!(options(&offer)[&1], &[255, 255, 255, 0]);
        let ack = answer(&req(3, mac, Some(Ipv4Addr::new(10, 89, 0, 7))), &leases, "og0").unwrap();
        assert_eq!(options(&ack)[&53], &[5]);
        let nak = answer(&req(3, mac, Some(Ipv4Addr::new(10, 89, 0, 9))), &leases, "og0").unwrap();
        assert_eq!(options(&nak)[&53], &[6]);
        assert!(answer(&req(1, [2, 0, 0, 0, 0, 1], None), &leases, "og0").is_none());
        assert!(answer(&req(1, mac, None), &leases, "og1").is_none());
    }
}
