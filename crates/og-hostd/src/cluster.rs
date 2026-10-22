//! Clusters: several OG-Host servers, at one site or many, managed as one.
//!
//! Nodes talk over mutual TLS on the cluster port (default 8007): a cluster CA (made by
//! `create`, copied to every node) signs each node's certificate, and a node is known by its
//! certificate's fingerprint in the node list. Joining takes a one-time token from a member:
//! it carries that member's address, the CA's fingerprint (so the joiner can trust it before
//! it has the CA) and a secret.
//!
//! Shared between nodes: the node list and the accounts/permissions (access.json), versioned
//! (newest wins) and pushed on change plus every minute. Not shared: containers, VMs,
//! datastores, jobs — each node runs its own, and any node's UI manages any other through
//! `/v1/nodes/<node>/...` (proxied with the caller's identity; permissions are checked on the
//! node that does the work). Cross-site backups: sync jobs copy a datastore's backups to a
//! datastore on another node, sending only the chunks it lacks (encrypted ones stay sealed).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use og_access::Who;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;
use tokio_rustls::rustls;

use crate::serve::Daemon;

pub const DEFAULT_PORT: u16 = 8007;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NodeEntry {
    pub name: String,
    /// host:port others reach its cluster port at
    pub addr: String,
    pub site: String,
    /// sha256 of its certificate (DER), hex
    pub fingerprint: String,
    pub joined: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ClusterState {
    pub name: String,
    pub id: String,
    pub ca_cn: String,
    pub nodes: Vec<NodeEntry>,
    pub version: u64,
    pub origin: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PeerStatus {
    pub online: bool,
    pub last_seen: u64,
    pub info: Value,
    pub error: String,
}

/// The caller of a request that came from another node (inserted only by the cluster port,
/// after the peer's certificate checked out).
#[derive(Clone)]
pub struct ClusterUser(pub Who);

/// The node a cluster-port request came from (None: no or unknown client certificate).
#[derive(Clone)]
struct Peer(Option<String>);

#[derive(Default, Serialize, Deserialize, Clone)]
struct DocVersion {
    version: u64,
    origin: String,
}

pub struct Cluster {
    dir: PathBuf,
    root: PathBuf,
    pub me: String,
    pub port: u16,
    state: RwLock<Option<ClusterState>>,
    pub peers: Mutex<HashMap<String, PeerStatus>>,
    tokens: Mutex<HashMap<String, Instant>>,
    client: RwLock<Option<reqwest::Client>>,
    /// the main API router (with auth), for requests proxied from other nodes
    pub app: OnceLock<Router>,
    listening: Mutex<bool>,
    access_doc: Mutex<(DocVersion, Option<(u64, std::time::SystemTime)>)>,
}

fn now() -> u64 {
    crate::containers::unix_now()
}

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(b))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let c = rustls_pemfile::certs(&mut pem.as_bytes()).next().context("no certificate")??;
    Ok(c.to_vec())
}

fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut b = vec![0u8; n];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    hex::encode(b)
}

fn write_secret(p: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = p.with_extension("tmp");
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?.write_all(text.as_bytes())?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

fn file_stamp(p: &Path) -> Option<(u64, std::time::SystemTime)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().and_then(|m| Some((m.ino(), m.modified().ok()?)))
}

fn valid_node_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 63 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !s.starts_with('-')
}

fn valid_addr(s: &str) -> bool {
    s.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok() && h.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c)))
}

/// CA params rebuilt from its name (rcgen signs with params + key; the DN must match).
fn ca_params(cn: &str) -> Result<rcgen::CertificateParams> {
    let mut p = rcgen::CertificateParams::new(Vec::<String>::new())?;
    p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    p.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign, rcgen::KeyUsagePurpose::DigitalSignature];
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name.push(rcgen::DnType::CommonName, cn);
    p.distinguished_name.push(rcgen::DnType::OrganizationName, "OG-Host");
    p.not_after = rcgen::date_time_ymd(2046, 1, 1);
    Ok(p)
}

/// A node certificate signed by the cluster CA: (cert pem, key pem, fingerprint).
fn node_cert(ca_cn: &str, ca_key_pem: &str, name: &str, addr: &str) -> Result<(String, String, String)> {
    let host = addr.rsplit_once(':').map(|(h, _)| h.trim_matches(['[', ']'])).unwrap_or(addr);
    let mut names = vec![name.to_string()];
    if host != name {
        names.push(host.to_string());
    }
    let mut p = rcgen::CertificateParams::new(names)?;
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name.push(rcgen::DnType::CommonName, name);
    p.distinguished_name.push(rcgen::DnType::OrganizationName, "OG-Host");
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth, rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    p.not_after = rcgen::date_time_ymd(2046, 1, 1);
    let ca_key = rcgen::KeyPair::from_pem(ca_key_pem)?;
    let issuer = ca_params(ca_cn)?.self_signed(&ca_key)?;
    let key = rcgen::KeyPair::generate()?;
    let cert = p.signed_by(&key, &issuer, &ca_key)?;
    let fp = sha256_hex(cert.der());
    Ok((cert.pem(), key.serialize_pem(), fp))
}

impl Cluster {
    pub fn open(root: &Path, port: u16) -> Cluster {
        let dir = root.join("cluster");
        let state: Option<ClusterState> = std::fs::read(dir.join("cluster.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let access_doc: DocVersion = std::fs::read(dir.join("access.version")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let c = Cluster {
            dir,
            root: root.to_path_buf(),
            me: crate::tls::hostname(),
            port,
            state: RwLock::new(state),
            peers: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            client: RwLock::new(None),
            app: OnceLock::new(),
            listening: Mutex::new(false),
            access_doc: Mutex::new((access_doc, file_stamp(&root.join("access").join("access.json")))),
        };
        if c.state().is_some() {
            if let Err(e) = c.build_client() {
                eprintln!("og-hostd: cluster client: {e:#}");
            }
        }
        c
    }

    pub fn state(&self) -> Option<ClusterState> {
        self.state.read().unwrap().clone()
    }

    fn save_state(&self, s: &ClusterState) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let p = self.dir.join("cluster.json");
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
        std::fs::rename(tmp, p)?;
        *self.state.write().unwrap() = Some(s.clone());
        Ok(())
    }

    fn read(&self, f: &str) -> Result<String> {
        std::fs::read_to_string(self.dir.join(f)).with_context(|| format!("cluster file {f} missing"))
    }

    fn build_client(&self) -> Result<()> {
        let ca = reqwest::Certificate::from_pem(self.read("ca.pem")?.as_bytes())?;
        let ident = reqwest::Identity::from_pem(format!("{}\n{}", self.read("node.pem")?, self.read("node.key")?).as_bytes())?;
        let c = reqwest::Client::builder().use_rustls_tls().tls_built_in_root_certs(false).add_root_certificate(ca).identity(ident).connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(600)).build()?;
        *self.client.write().unwrap() = Some(c);
        Ok(())
    }

    fn client(&self) -> Result<reqwest::Client> {
        self.client.read().unwrap().clone().context("this server isn't in a cluster")
    }

    pub fn node_pub(&self, name: &str) -> Result<NodeEntry> {
        self.node(name)
    }

    pub fn client_pub(&self) -> Result<reqwest::Client> {
        self.client()
    }

    fn node(&self, name: &str) -> Result<NodeEntry> {
        self.state().context("this server isn't in a cluster")?.nodes.into_iter().find(|n| n.name == name).with_context(|| format!("no node {name:?} in the cluster"))
    }

    /// Make a new one-node cluster.
    pub fn create(&self, name: &str, addr: &str, site: &str) -> Result<()> {
        if self.state().is_some() {
            bail!("this server is in a cluster already");
        }
        if !valid_node_name(name) || !valid_addr(addr) {
            bail!("cluster names are letters, digits and -; the address looks like 192.168.1.10:8007");
        }
        std::fs::create_dir_all(&self.dir)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let cn = format!("OG-Host cluster {name} {}", &random_hex(4));
        let ca_key = rcgen::KeyPair::generate()?;
        let ca = ca_params(&cn)?.self_signed(&ca_key)?;
        write_secret(&self.dir.join("ca.key"), &ca_key.serialize_pem())?;
        std::fs::write(self.dir.join("ca.pem"), ca.pem())?;
        let (cert, key, fp) = node_cert(&cn, &ca_key.serialize_pem(), &self.me, addr)?;
        std::fs::write(self.dir.join("node.pem"), cert)?;
        write_secret(&self.dir.join("node.key"), &key)?;
        let st = ClusterState { name: name.into(), id: random_hex(8), ca_cn: cn, nodes: vec![NodeEntry { name: self.me.clone(), addr: addr.into(), site: site.into(), fingerprint: fp, joined: now() }], version: 1, origin: self.me.clone() };
        self.save_state(&st)?;
        self.build_client()?;
        Ok(())
    }

    /// A one-time join token (valid `ttl`).
    pub fn token(&self, ttl: Duration) -> Result<String> {
        let st = self.state().context("make a cluster first")?;
        let me = st.nodes.iter().find(|n| n.name == self.me).context("this node isn't in its own node list")?;
        let fp = sha256_hex(&pem_to_der(&self.read("ca.pem")?)?);
        let secret = random_hex(24);
        let mut t = self.tokens.lock().unwrap();
        t.retain(|_, d| *d > Instant::now());
        t.insert(sha256_hex(secret.as_bytes()), Instant::now() + ttl);
        let body = json!({"c": st.name, "a": me.addr, "f": fp, "s": secret});
        Ok(format!("OGJ1.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body.to_string())))
    }

    /// Join the cluster a token is from. This server's accounts are replaced by the cluster's.
    pub async fn join(&self, token: &str, addr: &str, site: &str) -> Result<()> {
        if self.state().is_some() {
            bail!("this server is in a cluster already");
        }
        if !valid_addr(addr) {
            bail!("give this server's address as others will reach it, e.g. 192.168.1.11:8007");
        }
        let raw = token.trim().strip_prefix("OGJ1.").context("that isn't a join token")?;
        let t: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw).context("damaged token")?)?;
        let (peer, fp, secret) = (t["a"].as_str().context("token")?, t["f"].as_str().context("token")?, t["s"].as_str().context("token")?);
        // fetch the CA without trusting anything, then check it against the token
        let insecure = reqwest::Client::builder().use_rustls_tls().danger_accept_invalid_certs(true).timeout(Duration::from_secs(15)).build()?;
        let ca_pem = insecure.get(format!("https://{peer}/cluster/v1/ca")).send().await.with_context(|| format!("can't reach {peer}"))?.error_for_status()?.text().await?;
        if sha256_hex(&pem_to_der(&ca_pem)?) != fp {
            bail!("the cluster's certificate doesn't match the token: not joining (someone in between?)");
        }
        let c = reqwest::Client::builder().use_rustls_tls().tls_built_in_root_certs(false).add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes())?).timeout(Duration::from_secs(30)).build()?;
        let r = c.post(format!("https://{peer}/cluster/v1/join")).json(&json!({"secret": secret, "name": self.me, "addr": addr, "site": site})).send().await?;
        if !r.status().is_success() {
            let e: Value = r.json().await.unwrap_or_default();
            bail!("{}", e["error"].as_str().unwrap_or("join refused"));
        }
        let p: Value = r.json().await?;
        std::fs::create_dir_all(&self.dir)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::write(self.dir.join("ca.pem"), p["ca_pem"].as_str().context("ca")?)?;
        write_secret(&self.dir.join("ca.key"), p["ca_key"].as_str().context("ca key")?)?;
        std::fs::write(self.dir.join("node.pem"), p["cert"].as_str().context("cert")?)?;
        write_secret(&self.dir.join("node.key"), p["key"].as_str().context("key")?)?;
        if let Some(a) = p.get("access") {
            // a new member takes the cluster's accounts, whatever version it had
            self.apply_access(a, true)?;
        }
        let st: ClusterState = serde_json::from_value(p["state"].clone())?;
        self.save_state(&st)?;
        self.build_client()?;
        Ok(())
    }

    /// On a member: admit a node (one-time secret), sign its certificate.
    fn admit(&self, secret: &str, name: &str, addr: &str, site: &str) -> Result<Value> {
        let ok = {
            let mut t = self.tokens.lock().unwrap();
            t.retain(|_, d| *d > Instant::now());
            t.remove(&sha256_hex(secret.as_bytes())).is_some()
        };
        if !ok {
            bail!("the join token is wrong, used or expired");
        }
        if !valid_node_name(name) || !valid_addr(addr) {
            bail!("bad node name or address");
        }
        let mut st = self.state().context("not in a cluster")?;
        if st.nodes.iter().any(|n| n.name == name) {
            bail!("a node named {name} is in the cluster already (remove it first, or rename this server)");
        }
        let ca_key = self.read("ca.key")?;
        let (cert, key, fp) = node_cert(&st.ca_cn, &ca_key, name, addr)?;
        st.nodes.push(NodeEntry { name: name.into(), addr: addr.into(), site: site.into(), fingerprint: fp, joined: now() });
        st.version += 1;
        st.origin = self.me.clone();
        self.save_state(&st)?;
        {
            let mut cur = self.access_doc.lock().unwrap();
            if cur.0.version == 0 {
                cur.0 = DocVersion { version: 1, origin: self.me.clone() };
                let _ = std::fs::write(self.dir.join("access.version"), serde_json::to_vec(&cur.0).unwrap_or_default());
            }
        }
        Ok(json!({"state": st, "ca_pem": self.read("ca.pem")?, "ca_key": ca_key, "cert": cert, "key": key, "access": self.access_doc()}))
    }

    pub fn remove_node(&self, name: &str) -> Result<ClusterState> {
        let mut st = self.state().context("not in a cluster")?;
        if name == self.me {
            bail!("a node can't remove itself; remove it from another node, or leave");
        }
        let n = st.nodes.len();
        st.nodes.retain(|x| x.name != name);
        if st.nodes.len() == n {
            bail!("no node {name}");
        }
        st.version += 1;
        st.origin = self.me.clone();
        self.save_state(&st)?;
        self.peers.lock().unwrap().remove(name);
        Ok(st)
    }

    /// Forget the cluster on this node (others should remove it too).
    pub fn leave(&self) -> Result<()> {
        if self.state().is_none() {
            bail!("not in a cluster");
        }
        let old = self.dir.with_file_name(format!("cluster.left-{}", now()));
        std::fs::rename(&self.dir, old)?;
        *self.state.write().unwrap() = None;
        *self.client.write().unwrap() = None;
        self.peers.lock().unwrap().clear();
        Ok(())
    }

    // ---- shared documents ----

    fn access_doc(&self) -> Value {
        let content = std::fs::read_to_string(self.root.join("access").join("access.json")).unwrap_or_default();
        let v = self.access_doc.lock().unwrap().0.clone();
        json!({"version": v.version, "origin": v.origin, "content": content})
    }

    fn apply_access(&self, doc: &Value, force: bool) -> Result<bool> {
        let (version, origin) = (doc["version"].as_u64().unwrap_or(0), doc["origin"].as_str().unwrap_or("").to_string());
        let content = doc["content"].as_str().unwrap_or("");
        if content.is_empty() {
            return Ok(false);
        }
        let mut cur = self.access_doc.lock().unwrap();
        if !force && (version, origin.as_str()) <= (cur.0.version, cur.0.origin.as_str()) {
            return Ok(false);
        }
        serde_json::from_str::<Value>(content).context("bad accounts document")?;
        let p = self.root.join("access").join("access.json");
        std::fs::create_dir_all(p.parent().unwrap())?;
        write_secret(&p, content)?;
        cur.0 = DocVersion { version, origin };
        cur.1 = file_stamp(&p);
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.dir.join("access.version"), serde_json::to_vec(&cur.0)?)?;
        Ok(true)
    }

    fn apply_state(&self, s: ClusterState) -> Result<bool> {
        let cur = self.state().context("not in a cluster")?;
        if s.id != cur.id || (s.version, s.origin.as_str()) <= (cur.version, cur.origin.as_str()) {
            return Ok(false);
        }
        if !s.nodes.iter().any(|n| n.name == self.me) {
            eprintln!("og-hostd: this node was removed from cluster {}", s.name);
        }
        self.save_state(&s)?;
        Ok(true)
    }

    /// Local accounts changed since the last push? Then it's a new version of ours.
    fn access_changed(&self) -> bool {
        let p = self.root.join("access").join("access.json");
        let st = file_stamp(&p);
        let mut cur = self.access_doc.lock().unwrap();
        if st.is_some() && st != cur.1 {
            cur.1 = st;
            cur.0 = DocVersion { version: cur.0.version + 1, origin: self.me.clone() };
            let _ = std::fs::create_dir_all(&self.dir);
            let _ = std::fs::write(self.dir.join("access.version"), serde_json::to_vec(&cur.0).unwrap_or_default());
            return true;
        }
        false
    }

    fn others(&self) -> Vec<NodeEntry> {
        self.state().map(|s| s.nodes.into_iter().filter(|n| n.name != self.me).collect()).unwrap_or_default()
    }

    async fn push_all(&self) {
        let Ok(c) = self.client() else { return };
        let body = json!({"state": self.state(), "access": self.access_doc()});
        for n in self.others() {
            let _ = c.post(format!("https://{}/cluster/v1/sync", n.addr)).json(&body).send().await;
        }
    }
}

/// Call a /v1 path on another node as `who` (JSON in, JSON out).
pub async fn call_node(d: &Daemon, node: &str, who: &Who, method: reqwest::Method, path: &str) -> Result<Value> {
    let n = d.cluster.node(node)?;
    let c = d.cluster.client()?;
    let r = c.request(method, format!("https://{}/cluster/v1/proxy{path}", n.addr)).header("x-og-user", who_header(who)).json(&json!({})).send().await.with_context(|| format!("{node} isn't reachable"))?;
    let st = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !st.is_success() {
        bail!("{node}: {}", v["error"].as_str().unwrap_or(st.as_str()));
    }
    Ok(v)
}

fn who_header(w: &Who) -> String {
    match w {
        Who::User(u) => format!("user {u}"),
        Who::Token(t, u) => format!("token {t} {u}"),
        Who::System => "system".into(),
    }
}

fn parse_who(h: &str) -> Option<Who> {
    let mut p = h.split(' ');
    match (p.next()?, p.next(), p.next()) {
        ("user", Some(u), None) => Some(Who::User(u.into())),
        ("token", Some(t), Some(u)) => Some(Who::Token(t.into(), u.into())),
        ("system", None, None) => Some(Who::System),
        _ => None,
    }
}

/// This node's status for the others (and the UI).
fn local_status(d: &Daemon) -> Value {
    let (load, mem_total, mem_avail) = {
        let l = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let load: f64 = l.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let mi = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let kb = |k: &str| mi.lines().find_map(|x| x.strip_prefix(k).and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok())).unwrap_or(0) * 1024;
        (load, kb("MemTotal:"), kb("MemAvailable:"))
    };
    let cts = d.node.list().unwrap_or_default();
    let vms = d.vms.list();
    let running_vms = vms.iter().filter(|v| d.vms.status(&v.name).state != "stopped").count();
    json!({
        "name": crate::tls::hostname(), "version": env!("CARGO_PKG_VERSION"), "time": now(),
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), "load": load, "mem_total": mem_total, "mem_available": mem_avail,
        "containers": cts.len(), "containers_running": cts.iter().filter(|(_, s)| s.state == "running").count(),
        "vms": vms.len(), "vms_running": running_vms,
        "uptime": std::fs::read_to_string("/proc/uptime").ok().and_then(|u| u.split('.').next().and_then(|s| s.parse::<u64>().ok())).unwrap_or(0),
        "stores": d.storage.list().iter().filter(|s| s.enabled).map(|s| s.id.clone()).collect::<Vec<_>>(),
    })
}

// ---- the cluster port ----

pub async fn listen(d: Arc<Daemon>) -> Result<()> {
    let cl = &d.cluster;
    {
        let mut l = cl.listening.lock().unwrap();
        if *l || cl.state().is_none() {
            return Ok(());
        }
        *l = true;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let ca_der = pem_to_der(&cl.read("ca.pem")?)?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(ca_der))?;
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).allow_unauthenticated().build()?;
    let certs = rustls_pemfile::certs(&mut cl.read("node.pem")?.as_bytes()).collect::<std::result::Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut cl.read("node.key")?.as_bytes())?.context("node key")?;
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_client_cert_verifier(verifier).with_single_cert(certs, key)?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let cfg = Arc::new(cfg);
    let addr = format!("0.0.0.0:{}", cl.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    eprintln!("og-hostd: cluster port on {addr}");
    let router = cluster_router(d.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, peer_addr)) = listener.accept().await else { continue };
            let cfg = cfg.clone();
            let router = router.clone();
            let d = d.clone();
            tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
                let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await else { return };
                let fp = tls.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| sha256_hex(c.as_ref()));
                let peer = fp.and_then(|f| d.cluster.state().and_then(|s| s.nodes.into_iter().find(|n| n.fingerprint == f).map(|n| n.name)));
                let svc = router.layer(Extension(Peer(peer))).layer(Extension(axum::extract::ConnectInfo::<SocketAddr>(peer_addr)));
                let svc = hyper_util::service::TowerToHyperService::new(svc);
                let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new()).serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), svc).await;
            });
        }
    });
    Ok(())
}

type D = State<Arc<Daemon>>;

fn err(code: StatusCode, msg: impl std::fmt::Display) -> Response {
    (code, Json(json!({"error": msg.to_string()}))).into_response()
}

fn need_peer(p: &Peer) -> Result<String, Response> {
    p.0.clone().ok_or_else(|| err(StatusCode::UNAUTHORIZED, "not a member of this cluster"))
}

fn cluster_router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/cluster/v1/ca", get(ca))
        .route("/cluster/v1/join", post(join_req))
        .route("/cluster/v1/status", get(status))
        .route("/cluster/v1/sync", post(sync))
        .route("/cluster/v1/proxy/{*rest}", any(proxied))
        .route("/cluster/v1/store/{ds}/snapshots", get(remote_snaps))
        .route("/cluster/v1/store/{ds}/missing", post(remote_missing))
        .route("/cluster/v1/store/{ds}/chunk/{id}", put(remote_chunk))
        .route("/cluster/v1/store/{ds}/snapshot", put(remote_snapshot))
        .route("/cluster/v1/store/{ds}/prune", post(remote_prune))
        .route("/cluster/v1/vm/prepare", post(vm_prepare))
        .route("/cluster/v1/vm/{name}/finish", post(vm_finish))
        .route("/cluster/v1/vm/{name}/abort", post(vm_abort))
        .with_state(d)
}

async fn ca(State(d): D) -> Response {
    match d.cluster.read("ca.pem") {
        Ok(p) => p.into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize)]
struct JoinReq {
    secret: String,
    name: String,
    addr: String,
    #[serde(default)]
    site: String,
}

async fn join_req(State(d): D, Json(b): Json<JoinReq>) -> Response {
    match d.cluster.admit(&b.secret, &b.name, &b.addr, &b.site) {
        Ok(v) => {
            d.access.audit("system", &b.addr, "cluster join", &b.name, true);
            let d2 = d.clone();
            tokio::spawn(async move { d2.cluster.push_all().await });
            Json(v).into_response()
        }
        Err(e) => {
            d.access.audit("system", &b.addr, "cluster join", &b.name, false);
            err(StatusCode::FORBIDDEN, e)
        }
    }
}

async fn status(State(d): D, Extension(p): Extension<Peer>) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    let d2 = d.clone();
    Json(tokio::task::spawn_blocking(move || local_status(&d2)).await.unwrap_or_default()).into_response()
}

async fn sync(State(d): D, Extension(p): Extension<Peer>, Json(b): Json<Value>) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    if let Ok(s) = serde_json::from_value::<ClusterState>(b["state"].clone()) {
        let _ = d.cluster.apply_state(s);
    }
    if let Some(a) = b.get("access") {
        if let Ok(true) = d.cluster.apply_access(a, false) {
            eprintln!("og-hostd: accounts updated from {}", p.0.as_deref().unwrap_or("?"));
        }
    }
    Json(json!({"ok": true})).into_response()
}

/// A request from another node on behalf of one of its users: run it through our own API
/// (and its permission checks) as that user.
async fn proxied(State(d): D, Extension(p): Extension<Peer>, mut req: Request<Body>) -> Response {
    let peer = match need_peer(&p) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let Some(who) = req.headers().get("x-og-user").and_then(|v| v.to_str().ok()).and_then(parse_who) else { return err(StatusCode::BAD_REQUEST, "who is asking?") };
    let pq = req.uri().path_and_query().map(|x| x.as_str().to_string()).unwrap_or_default();
    let rest = pq.strip_prefix("/cluster/v1/proxy").unwrap_or("/");
    if !rest.starts_with("/v1/") && !rest.starts_with("/api/") {
        return err(StatusCode::NOT_FOUND, "only API paths");
    }
    *req.uri_mut() = match rest.parse() {
        Ok(u) => u,
        Err(_) => return err(StatusCode::BAD_REQUEST, "bad path"),
    };
    req.headers_mut().remove("x-og-user");
    req.headers_mut().remove(axum::http::header::COOKIE);
    req.headers_mut().remove(axum::http::header::AUTHORIZATION);
    // a fresh start for the API's router (this route's path parameters must not leak in)
    let conn = req.extensions().get::<axum::extract::ConnectInfo<SocketAddr>>().cloned();
    *req.extensions_mut() = Default::default();
    if let Some(c) = conn {
        req.extensions_mut().insert(c);
    }
    req.extensions_mut().insert(ClusterUser(who));
    req.headers_mut().insert("x-og-via", axum::http::HeaderValue::from_str(&peer).unwrap_or(axum::http::HeaderValue::from_static("peer")));
    let Some(app) = d.cluster.app.get().cloned() else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    use tower::ServiceExt;
    match app.oneshot(req).await {
        Ok(r) => r.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ---- VMs arriving from another node (migration) ----

async fn vm_prepare(st: D, Extension(p): Extension<Peer>, b: Json<crate::migrate::Prepare>) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    crate::migrate::prepare(st, b).await
}

async fn vm_finish(st: D, Extension(p): Extension<Peer>, n: UrlPath<String>, b: Json<crate::migrate::Finish>) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    crate::migrate::finish(st, n, b).await
}

async fn vm_abort(st: D, Extension(p): Extension<Peer>, n: UrlPath<String>) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    crate::migrate::abort(st, n).await
}

// ---- datastores at other sites (sync jobs push here) ----

fn repo_of(d: &Daemon, ds: &str) -> Result<og_backup::Repo, Response> {
    d.storage.repo(ds).map(|(_, r)| r).map_err(|e| err(StatusCode::NOT_FOUND, format!("{e:#}")))
}

async fn remote_snaps(State(d): D, Extension(p): Extension<Peer>, UrlPath(ds): UrlPath<String>) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    let repo = match repo_of(&d, &ds) {
        Ok(r) => r,
        Err(r) => return r,
    };
    Json(json!(repo.snapshots().iter().map(|m| m.id()).collect::<Vec<_>>())).into_response()
}

#[derive(Deserialize)]
struct Ids {
    ids: Vec<String>,
}

async fn remote_missing(State(d): D, Extension(p): Extension<Peer>, UrlPath(ds): UrlPath<String>, Json(b): Json<Ids>) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    let repo = match repo_of(&d, &ds) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let missing: Vec<&String> = b.ids.iter().filter(|h| hex::decode(h).ok().and_then(|v| <og_backup::Id>::try_from(v).ok()).is_none_or(|id| !repo.has_chunk(&id))).collect();
    Json(json!({"missing": missing})).into_response()
}

async fn remote_chunk(State(d): D, Extension(p): Extension<Peer>, UrlPath((ds, id)): UrlPath<(String, String)>, body: axum::body::Bytes) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    let repo = match repo_of(&d, &ds) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let Some(id) = hex::decode(&id).ok().and_then(|v| <og_backup::Id>::try_from(v).ok()) else { return err(StatusCode::BAD_REQUEST, "bad chunk id") };
    match repo.put_raw(&id, &body) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct SnapPut {
    manifest: og_backup::Manifest,
    /// archive name -> index file, base64
    indexes: HashMap<String, String>,
}

async fn remote_snapshot(State(d): D, Extension(p): Extension<Peer>, UrlPath(ds): UrlPath<String>, Json(b): Json<SnapPut>) -> Response {
    let peer = match need_peer(&p) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let repo = match repo_of(&d, &ds) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let mut idx = vec![];
    for (k, v) in &b.indexes {
        match base64::engine::general_purpose::STANDARD.decode(v) {
            Ok(bytes) => idx.push((k.clone(), bytes)),
            Err(_) => return err(StatusCode::BAD_REQUEST, "bad index"),
        }
    }
    let mut m = b.manifest;
    if m.notes.is_empty() {
        m.notes = format!("from {peer}");
    }
    match repo.import_snapshot(&m, &idx) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct PruneReq {
    group: String,
    keep: og_backup::prune::Keep,
}

async fn remote_prune(State(d): D, Extension(p): Extension<Peer>, UrlPath(ds): UrlPath<String>, Json(b): Json<PruneReq>) -> Response {
    if let Err(r) = need_peer(&p) {
        return r;
    }
    let repo = match repo_of(&d, &ds) {
        Ok(r) => r,
        Err(r) => return r,
    };
    match crate::backup::prune(&repo, &b.group, &b.keep, false) {
        Ok(g) => Json(json!({"removed": g})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

// ---- background: peer status, config sync ----

pub async fn background(d: Arc<Daemon>) {
    let mut tick: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        tick += 1;
        if d.cluster.state().is_none() {
            continue;
        }
        let _ = listen(d.clone()).await;
        if d.cluster.access_changed() || tick % 20 == 0 {
            d.cluster.push_all().await;
        }
        if tick % 3 == 0 {
            let Ok(c) = d.cluster.client() else { continue };
            for n in d.cluster.others() {
                let r = c.get(format!("https://{}/cluster/v1/status", n.addr)).timeout(Duration::from_secs(5)).send().await;
                let result: Result<Value, String> = match r {
                    Ok(r) if r.status().is_success() => Ok(r.json().await.unwrap_or_default()),
                    Ok(r) => Err(format!("HTTP {}", r.status())),
                    Err(x) => Err(if x.is_connect() || x.is_timeout() { "unreachable".into() } else { x.to_string() }),
                };
                let mut peers = d.cluster.peers.lock().unwrap();
                let e = peers.entry(n.name.clone()).or_default();
                match result {
                    Ok(info) => {
                        e.online = true;
                        e.last_seen = now();
                        e.info = info;
                        e.error.clear();
                    }
                    Err(msg) => {
                        e.online = false;
                        e.error = msg;
                    }
                }
            }
        }
    }
}

// ---- the UI's API (/v1/cluster, /v1/nodes) ----

pub fn api_router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/v1/cluster", get(overview))
        .route("/v1/cluster/create", post(create))
        .route("/v1/cluster/token", post(token))
        .route("/v1/cluster/join", post(join))
        .route("/v1/cluster/leave", post(leave))
        .route("/v1/cluster/nodes/{name}", delete(remove))
        .route("/v1/cluster/sync-jobs", get(sync_jobs).post(sync_job_put))
        .route("/v1/cluster/sync-jobs/{id}", delete(sync_job_delete))
        .route("/v1/cluster/sync-jobs/{id}/run", post(sync_job_run))
        .route("/v1/nodes/{node}/{*rest}", any(to_node))
        .with_state(d)
}

async fn overview(State(d): D) -> Json<Value> {
    let st = d.cluster.state();
    let peers = d.cluster.peers.lock().unwrap().clone();
    let d2 = d.clone();
    let me_status = tokio::task::spawn_blocking(move || local_status(&d2)).await.unwrap_or_default();
    let nodes: Vec<Value> = match &st {
        Some(s) => s
            .nodes
            .iter()
            .map(|n| {
                if n.name == d.cluster.me {
                    json!({"name": n.name, "addr": n.addr, "site": n.site, "joined": n.joined, "local": true, "online": true, "info": me_status})
                } else {
                    let p = peers.get(&n.name).cloned().unwrap_or_default();
                    json!({"name": n.name, "addr": n.addr, "site": n.site, "joined": n.joined, "local": false, "online": p.online, "last_seen": p.last_seen, "info": p.info, "error": p.error})
                }
            })
            .collect(),
        None => vec![json!({"name": d.cluster.me, "local": true, "online": true, "info": me_status})],
    };
    Json(json!({"cluster": st.map(|s| json!({"name": s.name, "id": s.id, "version": s.version})), "me": d.cluster.me, "port": d.cluster.port, "nodes": nodes, "suggested_addr": suggested_addr(d.cluster.port)}))
}

fn suggested_addr(port: u16) -> String {
    let out = std::process::Command::new("ip").args(["-4", "-o", "route", "get", "1.1.1.1"]).output().ok();
    let src = out.and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().skip_while(|w| *w != "src").nth(1).map(String::from));
    format!("{}:{port}", src.unwrap_or_else(|| "127.0.0.1".into()))
}

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    #[serde(default)]
    addr: String,
    #[serde(default)]
    site: String,
}

async fn create(State(d): D, Extension(w): Extension<Who>, Json(b): Json<CreateReq>) -> Response {
    let addr = if b.addr.is_empty() { suggested_addr(d.cluster.port) } else { b.addr };
    match d.cluster.create(&b.name, &addr, &b.site) {
        Ok(()) => {
            d.access.audit(&w.label(), "", "cluster create", &b.name, true);
            let _ = listen(d.clone()).await;
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TokenReq {
    minutes: u64,
}

async fn token(State(d): D, Json(b): Json<TokenReq>) -> Response {
    let ttl = Duration::from_secs(b.minutes.clamp(5, 24 * 60) * 60);
    match d.cluster.token(ttl) {
        Ok(t) => Json(json!({"token": t, "expires_in": ttl.as_secs()})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct JoinUiReq {
    token: String,
    #[serde(default)]
    addr: String,
    #[serde(default)]
    site: String,
}

async fn join(State(d): D, Extension(w): Extension<Who>, Json(b): Json<JoinUiReq>) -> Response {
    let addr = if b.addr.is_empty() { suggested_addr(d.cluster.port) } else { b.addr };
    match d.cluster.join(&b.token, &addr, &b.site).await {
        Ok(()) => {
            d.access.audit(&w.label(), "", "cluster joined", "", true);
            let _ = listen(d.clone()).await;
            Json(json!({"ok": true, "note": "accounts now come from the cluster: sign in again with a cluster account"})).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn leave(State(d): D) -> Response {
    match d.cluster.leave() {
        Ok(()) => Json(json!({"ok": true, "note": "restart og-hostd to close the cluster port"})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn remove(State(d): D, UrlPath(name): UrlPath<String>) -> Response {
    match d.cluster.remove_node(&name) {
        Ok(_) => {
            let d2 = d.clone();
            tokio::spawn(async move { d2.cluster.push_all().await });
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

/// `/v1/nodes/<node>/<api path>`: the same API on another node, as the caller.
async fn to_node(State(d): D, Extension(w): Extension<Who>, UrlPath((node, rest)): UrlPath<(String, String)>, req: Request<Body>) -> Response {
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let path = format!("/{rest}{query}");
    if !path.starts_with("/v1/") && !path.starts_with("/api/") {
        return err(StatusCode::NOT_FOUND, "only API paths");
    }
    if node == d.cluster.me {
        // ourselves: straight into our own API
        let Some(app) = d.cluster.app.get().cloned() else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
        let (mut parts, body) = req.into_parts();
        parts.uri = match path.parse() {
            Ok(u) => u,
            Err(_) => return err(StatusCode::BAD_REQUEST, "bad path"),
        };
        let conn = parts.extensions.get::<axum::extract::ConnectInfo<SocketAddr>>().cloned();
        parts.extensions = Default::default();
        if let Some(c) = conn {
            parts.extensions.insert(c);
        }
        let mut r = Request::from_parts(parts, body);
        r.extensions_mut().insert(ClusterUser(w));
        use tower::ServiceExt;
        return app.oneshot(r).await.map(|r| r.into_response()).unwrap_or_else(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    let n = match d.cluster.node(&node) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    let c = match d.cluster.client() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let method = req.method().clone();
    let ctype = req.headers().get(axum::http::header::CONTENT_TYPE).cloned();
    let body = reqwest::Body::wrap_stream(req.into_body().into_data_stream());
    let mut rq = c.request(method, format!("https://{}/cluster/v1/proxy{path}", n.addr)).header("x-og-user", who_header(&w)).body(body);
    if let Some(ct) = ctype {
        rq = rq.header(reqwest::header::CONTENT_TYPE, ct);
    }
    match rq.send().await {
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut hm = HeaderMap::new();
            for k in [reqwest::header::CONTENT_TYPE, reqwest::header::CONTENT_DISPOSITION, reqwest::header::CACHE_CONTROL] {
                if let Some(v) = r.headers().get(&k) {
                    if let Ok(v) = axum::http::HeaderValue::from_bytes(v.as_bytes()) {
                        hm.insert(axum::http::HeaderName::from_bytes(k.as_str().as_bytes()).unwrap(), v);
                    }
                }
            }
            (status, hm, Body::from_stream(r.bytes_stream())).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("{node} isn't reachable: {e}")),
    }
}

// ---- sync jobs: copy backups to a datastore on another node ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncJob {
    pub id: String,
    pub enabled: bool,
    pub schedule: String,
    /// local datastore
    pub store: String,
    pub node: String,
    pub remote_store: String,
    /// backup groups to copy (ct/web, stack/immich…); empty = all
    pub groups: Vec<String>,
    /// retention on the remote side (empty = keep all there)
    pub keep: og_backup::prune::Keep,
    pub comment: String,
}

impl Default for SyncJob {
    fn default() -> Self {
        SyncJob { id: String::new(), enabled: true, schedule: "daily 04:00".into(), store: "local".into(), node: String::new(), remote_store: "local".into(), groups: vec![], keep: Default::default(), comment: String::new() }
    }
}

pub fn sync_jobs_list(root: &Path) -> Vec<SyncJob> {
    std::fs::read(root.join("sync-jobs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_sync_jobs(root: &Path, j: &[SyncJob]) -> Result<()> {
    let p = root.join("sync-jobs.json");
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(j)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

async fn sync_jobs(State(d): D) -> Json<Value> {
    let now = now();
    Json(json!(sync_jobs_list(&d.root).into_iter().map(|j| {
        let next = crate::schedule::Schedule::parse(&j.schedule).ok().and_then(|s| s.next_after(now)).filter(|_| j.enabled);
        json!({"job": j, "next": next})
    }).collect::<Vec<_>>()))
}

async fn sync_job_put(State(d): D, Json(j): Json<SyncJob>) -> Response {
    let r = (|| -> Result<()> {
        if !crate::storage::valid_id(&j.id) {
            bail!("job names are letters, digits, - and _");
        }
        crate::schedule::Schedule::parse(&j.schedule)?;
        d.storage.get(&j.store)?;
        d.cluster.node(&j.node)?;
        if j.node == d.cluster.me {
            bail!("pick another node");
        }
        if !crate::storage::valid_id(&j.remote_store) {
            bail!("bad remote datastore name");
        }
        let mut all = sync_jobs_list(&d.root);
        match all.iter_mut().find(|x| x.id == j.id) {
            Some(x) => *x = j,
            None => all.push(j),
        }
        save_sync_jobs(&d.root, &all)
    })();
    match r {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn sync_job_delete(State(d): D, UrlPath(id): UrlPath<String>) -> Response {
    let all: Vec<SyncJob> = sync_jobs_list(&d.root).into_iter().filter(|j| j.id != id).collect();
    match save_sync_jobs(&d.root, &all) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn sync_job_run(State(d): D, Extension(w): Extension<Who>, UrlPath(id): UrlPath<String>) -> Response {
    let Some(j) = sync_jobs_list(&d.root).into_iter().find(|j| j.id == id) else { return err(StatusCode::NOT_FOUND, "no such job") };
    match start_sync(&d, j, &w.label()) {
        Ok(t) => Json(json!({"task": t})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

pub fn start_sync(d: &Arc<Daemon>, j: SyncJob, user: &str) -> Result<String> {
    if d.tasks.busy("sync", &j.id) {
        bail!("sync job {} is running already", j.id);
    }
    let d2 = d.clone();
    let id = j.id.clone();
    d.tasks.spawn("sync", &id, user, move |t| {
        let h = tokio::runtime::Handle::current();
        h.block_on(run_sync(&d2, t, &j))
    })
}

async fn run_sync(d: &Daemon, t: &crate::tasks::Task, j: &SyncJob) -> Result<()> {
    let (_, repo) = d.storage.repo(&j.store)?;
    let n = d.cluster.node(&j.node)?;
    let c = d.cluster.client()?;
    let base = format!("https://{}/cluster/v1/store/{}", n.addr, j.remote_store);
    t.log(format!("copying backups from {} to {} on {}", j.store, j.remote_store, j.node));
    let r = c.get(format!("{base}/snapshots")).send().await.with_context(|| format!("{} isn't reachable", j.node))?;
    if !r.status().is_success() {
        let v: Value = r.json().await.unwrap_or_default();
        bail!("{}: {}", j.node, v["error"].as_str().unwrap_or("datastore not available"));
    }
    let there: std::collections::HashSet<String> = r.json::<Vec<String>>().await?.into_iter().collect();
    let wanted: Vec<og_backup::Manifest> = repo.snapshots().into_iter().filter(|m| j.groups.is_empty() || j.groups.contains(&m.group)).collect();
    // only what the remote retention would keep (else it's sent and pruned again every run)
    let mut keep = std::collections::HashSet::new();
    let mut by_group: std::collections::BTreeMap<&str, Vec<&og_backup::Manifest>> = Default::default();
    for m in &wanted {
        by_group.entry(&m.group).or_default().push(m);
    }
    for list in by_group.values() {
        let plan = og_backup::prune::plan(&list.iter().map(|m| (m.time, m.protected)).collect::<Vec<_>>(), &j.keep);
        for (m, k) in list.iter().zip(plan) {
            if k {
                keep.insert(m.id());
            }
        }
    }
    let todo: Vec<og_backup::Manifest> = wanted.iter().filter(|m| keep.contains(&m.id()) && !there.contains(&m.id())).cloned().collect();
    t.log(format!("{} backup(s) to copy, {} already there", todo.len(), there.len()));
    let (mut chunks, mut bytes) = (0u64, 0u64);
    let mut groups = std::collections::BTreeSet::new();
    // oldest first, so dedup works in time order
    for m in todo.iter().rev() {
        t.check()?;
        let ids = repo.chunk_ids(m)?;
        let hexes: Vec<String> = ids.iter().map(og_backup::id_hex).collect();
        let mut missing: Vec<String> = vec![];
        for part in hexes.chunks(5000) {
            let r: Value = c.post(format!("{base}/missing")).json(&json!({"ids": part})).send().await?.error_for_status()?.json().await?;
            missing.extend(r["missing"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)));
        }
        for h in &missing {
            t.check()?;
            let id: og_backup::Id = hex::decode(h)?.try_into().map_err(|_| anyhow::anyhow!("bad id"))?;
            let raw = repo.raw_chunk(&id)?;
            bytes += raw.len() as u64;
            chunks += 1;
            let r = c.put(format!("{base}/chunk/{h}")).body(raw).send().await?;
            if !r.status().is_success() {
                bail!("sending chunk {h}: HTTP {}", r.status());
            }
        }
        let idx: HashMap<String, String> = repo.index_files(m)?.into_iter().map(|(k, v)| (k, base64::engine::general_purpose::STANDARD.encode(v))).collect();
        let r = c.put(format!("{base}/snapshot")).json(&json!({"manifest": m, "indexes": idx})).send().await?;
        if !r.status().is_success() {
            let v: Value = r.json().await.unwrap_or_default();
            bail!("{}: {}", m.id(), v["error"].as_str().unwrap_or("refused"));
        }
        groups.insert(m.group.clone());
        t.log(format!("  {} ({} new chunks)", m.id(), missing.len()));
    }
    if !j.keep.is_empty() {
        for g in repo.snapshots().into_iter().map(|m| m.group).filter(|g| j.groups.is_empty() || j.groups.contains(g)).collect::<std::collections::BTreeSet<_>>() {
            if let Ok(r) = c.post(format!("{base}/prune")).json(&json!({"group": g, "keep": j.keep})).send().await {
                if let Ok(v) = r.json::<Value>().await {
                    let n = v["removed"].as_array().map(|a| a.len()).unwrap_or(0);
                    if n > 0 {
                        t.log(format!("  pruned {n} old backup(s) of {g} on {}", j.node));
                    }
                }
            }
        }
    }
    t.log(format!("done: {} backup(s), {chunks} chunks ({:.1} MiB) sent", todo.len(), bytes as f64 / 1048576.0));
    Ok(())
}
