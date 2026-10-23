//! Sleep when idle, wake on request: containers labelled `og.sleep-after: <duration>` are
//! stopped after that long without activity and started again by the first connection to one of
//! their published ports. Idle containers give back their RAM and GPU memory (a loaded Stable
//! Diffusion holds ~8 GB of RAM and ~7 GB of VRAM doing nothing).
//!
//! How: og-hostd listens on every published TCP port of such a container itself. While the
//! container runs, the port's DNAT rules send traffic straight to it and the listener sees
//! nothing (no proxying cost). When it sleeps the rules are removed, so connections reach the
//! listener: it starts the container (which publishes the rules again), waits until the app
//! accepts connections, and relays that first connection.
//!
//! Activity = network traffic into/out of the container above a trickle (health checks don't
//! count) or CPU use; a GPU job always shows as CPU too.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::serve::Daemon;

pub const LABEL: &str = "og.sleep-after";
/// Below this much traffic per check (both directions), a container counts as idle.
const TRICKLE_BYTES: u64 = 256 * 1024;
/// Average CPU (percent of one core) below which it counts as idle.
const IDLE_CPU: f64 = 3.0;
const CHECK: Duration = Duration::from_secs(30);
/// How long a waking connection waits for the app to accept connections.
const WAKE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
struct Track {
    last_active: Option<Instant>,
    prev: Option<(u64, u64, Instant)>, // (cpu usec, net bytes, when)
}

static TRACK: Mutex<Option<HashMap<String, Track>>> = Mutex::new(None);
/// Ports og-hostd listens on, so each is bound once: (ip, port) -> container id.
static LISTENING: Mutex<Option<HashMap<(Ipv4Addr, u16), String>>> = Mutex::new(None);
/// One wake at a time per container.
static WAKING: Mutex<Option<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = Mutex::new(None);

pub fn sleep_after(labels: &std::collections::BTreeMap<String, String>) -> Option<Duration> {
    labels.get(LABEL).and_then(|v| crate::brain::parse_duration(v)).filter(|d| *d >= Duration::from_secs(60))
}

fn usage(d: &Daemon, m: &crate::containers::Meta) -> (u64, u64) {
    let cg = std::path::PathBuf::from(crate::brain::CGROUP_ROOT).join(&m.name);
    let cpu = std::fs::read_to_string(cg.join("cpu.stat"))
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec ").and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0);
    // network: the host side of its veth, or of the container whose network it shares
    let holder = m.network_mode.strip_prefix("container:").and_then(|o| d.node.find(o).ok());
    let net = holder.as_ref().unwrap_or(m).net.as_ref().and_then(|a| a.host_veth.clone());
    let bytes = net
        .map(|v| {
            let s = std::path::PathBuf::from("/sys/class/net").join(v).join("statistics");
            let r = |f: &str| std::fs::read_to_string(s.join(f)).ok().and_then(|x| x.trim().parse::<u64>().ok()).unwrap_or(0);
            r("rx_bytes") + r("tx_bytes")
        })
        .unwrap_or(0);
    (cpu, bytes)
}

/// Every 30 s: note activity of running sleepers; put the idle ones to sleep.
fn check(d: &Arc<Daemon>) {
    let Ok(list) = d.node.list() else { return };
    let mut track = TRACK.lock().unwrap();
    let track = track.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    for (m, st) in &list {
        let Some(after) = sleep_after(&m.labels) else { continue };
        if st.state != "running" {
            track.remove(&m.id);
            continue;
        }
        let t = track.entry(m.id.clone()).or_default();
        let (cpu, net) = usage(d, m);
        let active = match t.prev {
            Some((pc, pn, at)) => {
                let secs = now.duration_since(at).as_secs_f64().max(1.0);
                let cpu_pct = cpu.saturating_sub(pc) as f64 / 1e4 / secs;
                net.saturating_sub(pn) > TRICKLE_BYTES || cpu_pct > IDLE_CPU
            }
            None => true, // just started (or og-hostd just started): give it a full period
        };
        t.prev = Some((cpu, net, now));
        if active || t.last_active.is_none() {
            t.last_active = Some(now);
        }
        if t.last_active.is_some_and(|a| now.duration_since(a) >= after) {
            track.remove(&m.id);
            if let Err(e) = put_to_sleep(d, &m.id) {
                eprintln!("og-hostd: {}: going to sleep failed: {e:#}", m.name);
            } else {
                eprintln!("og-hostd: {} idle for {} min: sleeping (wakes on its next request)", m.name, after.as_secs() / 60);
            }
        }
    }
    drop(list);
}

fn put_to_sleep(d: &Daemon, id: &str) -> anyhow::Result<()> {
    d.node.stop(id, 20)?;
    d.node.set_desired(id, "sleeping")?;
    d.node.net.unpublish_ports(id)?;
    Ok(())
}

pub fn is_sleeping(d: &Daemon, id: &str) -> bool {
    d.node.find(id).is_ok_and(|m| m.desired == "sleeping")
}

/// Listen on every published TCP port of every sleeper (once per port).
fn ensure_listeners(d: &Arc<Daemon>) {
    let Ok(list) = d.node.list() else { return };
    for (m, _) in list {
        if sleep_after(&m.labels).is_none() {
            continue;
        }
        let Some(a) = m.net.as_ref() else { continue };
        for p in &a.ports {
            if p.proto != og_net::Proto::Tcp {
                continue;
            }
            let ip = p.host_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
            let key = (ip, p.host_port);
            {
                let mut l = LISTENING.lock().unwrap();
                let l = l.get_or_insert_with(HashMap::new);
                if l.get(&key) == Some(&m.id) {
                    continue;
                }
                l.insert(key, m.id.clone());
            }
            let (d2, id, cport) = (d.clone(), m.id.clone(), p.container_port);
            tokio::spawn(async move {
                if let Err(e) = listen(d2, id.clone(), SocketAddr::from((ip, key.1)), cport).await {
                    eprintln!("og-hostd: wake listener on {ip}:{}: {e:#}", key.1);
                    if let Some(l) = LISTENING.lock().unwrap().as_mut() {
                        l.remove(&key);
                    }
                }
            });
        }
    }
}

async fn listen(d: Arc<Daemon>, id: String, addr: SocketAddr, container_port: u16) -> anyhow::Result<()> {
    let sock = tokio::net::TcpSocket::new_v4()?;
    sock.set_reuseaddr(true)?;
    sock.bind(addr)?;
    let l = sock.listen(256)?;
    loop {
        let (conn, _) = l.accept().await?;
        // the container could have been removed or relabelled meanwhile
        if !LISTENING.lock().unwrap().as_ref().is_some_and(|m| m.get(&(match addr.ip() { std::net::IpAddr::V4(v) => v, _ => Ipv4Addr::UNSPECIFIED }, addr.port())) == Some(&id)) {
            return Ok(());
        }
        let (d2, id2) = (d.clone(), id.clone());
        tokio::spawn(async move {
            if let Err(e) = wake_and_relay(d2, id2, conn, container_port).await {
                eprintln!("og-hostd: waking connection: {e:#}");
            }
        });
    }
}

async fn wake_and_relay(d: Arc<Daemon>, id: String, mut conn: tokio::net::TcpStream, port: u16) -> anyhow::Result<()> {
    let lock = WAKING.lock().unwrap().get_or_insert_with(HashMap::new).entry(id.clone()).or_default().clone();
    {
        let _g = lock.lock().await;
        if d.node.status(&id).state != "running" {
            let (d2, id2) = (d.clone(), id.clone());
            let name = d.node.find(&id).map(|m| m.name).unwrap_or_default();
            eprintln!("og-hostd: {name}: waking up for a request");
            tokio::task::spawn_blocking(move || d2.node.start(&id2)).await??;
        }
    }
    let ip = d.node.find(&id)?.net.and_then(|a| a.ip).ok_or_else(|| anyhow::anyhow!("no address"))?;
    let t0 = Instant::now();
    let mut up = loop {
        match tokio::net::TcpStream::connect((ip, port)).await {
            Ok(s) => break s,
            Err(_) if t0.elapsed() < WAKE_TIMEOUT => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => anyhow::bail!("{ip}:{port} didn't come up in {} s: {e}", WAKE_TIMEOUT.as_secs()),
        }
    };
    tokio::io::copy_bidirectional(&mut conn, &mut up).await?;
    Ok(())
}

pub async fn run(d: Arc<Daemon>) {
    loop {
        let d2 = d.clone();
        let _ = tokio::task::spawn_blocking(move || check(&d2)).await;
        ensure_listeners(&d);
        tokio::time::sleep(CHECK).await;
    }
}
