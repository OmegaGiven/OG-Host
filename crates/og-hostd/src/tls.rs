//! Built-in HTTPS for the web UI and API (like Proxmox's :8006).
//!
//! `--listen-tls ADDR` serves the same router over TLS. The certificate is `--tls-cert` /
//! `--tls-key` (PEM; e.g. from `tailscale cert`), or else a self-signed one generated on
//! first start in `<root>/tls/` (valid for the host name, `localhost` and the host's
//! addresses). Reloaded from disk when the files change, so a renewed cert needs no restart.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use tokio_rustls::rustls;

/// Marker in request extensions: the request came in over TLS (cookies get `Secure`).
#[derive(Clone, Copy)]
pub struct Secure;

pub struct CertFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// The configured files, or a self-signed pair under `<root>/tls/`, created if missing.
pub fn cert_files(root: &Path, cert: Option<PathBuf>, key: Option<PathBuf>) -> Result<CertFiles> {
    if let (Some(cert), Some(key)) = (cert, key) {
        return Ok(CertFiles { cert, key });
    }
    let dir = root.join("tls");
    let files = CertFiles { cert: dir.join("cert.pem"), key: dir.join("key.pem") };
    if files.cert.exists() && files.key.exists() {
        return Ok(files);
    }
    std::fs::create_dir_all(&dir)?;
    let host = hostname();
    let mut names = vec![host.clone(), "localhost".to_string()];
    names.extend(local_addresses());
    let mut params = rcgen::CertificateParams::new(names.clone()).context("certificate names")?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, format!("OG-Host {host}"));
    params.distinguished_name.push(rcgen::DnType::OrganizationName, "OG-Host");
    params.not_after = rcgen::date_time_ymd(2036, 1, 1);
    let key = rcgen::KeyPair::generate().context("generating a key")?;
    let cert = params.self_signed(&key).context("signing the certificate")?;
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&files.key)?.write_all(key.serialize_pem().as_bytes())?;
    }
    std::fs::write(&files.cert, cert.pem())?;
    eprintln!("og-hostd: created a self-signed certificate for {} in {}", names.join(", "), dir.display());
    Ok(files)
}

fn hostname() -> String {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut uts) };
    unsafe { std::ffi::CStr::from_ptr(uts.nodename.as_ptr()) }.to_string_lossy().into_owned()
}

/// IPv4 addresses of the host's interfaces (for the certificate's SANs).
fn local_addresses() -> Vec<String> {
    let out = std::process::Command::new("ip").args(["-4", "-o", "addr", "show", "scope", "global"]).output();
    let Ok(out) = out else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().skip_while(|w| *w != "inet").nth(1).and_then(|a| a.split('/').next()).map(String::from))
        .filter(|a| !a.starts_with("10.89.")) // container bridges
        .collect()
}

fn load_config(files: &CertFiles) -> Result<Arc<rustls::ServerConfig>> {
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(&files.cert).with_context(|| format!("{}", files.cert.display()))?))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("reading the certificate")?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(&files.key).with_context(|| format!("{}", files.key.display()))?))
        .context("reading the key")?
        .context("no private key in the key file")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate and key don't match")?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Serve `router` over TLS on `addr` until the process ends.
pub async fn serve(addr: &str, files: CertFiles, router: axum::Router) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    let current = Arc::new(Mutex::new((load_config(&files)?, mtime(&files.cert), mtime(&files.key))));
    eprintln!("og-hostd: web UI and API on https://{addr} (certificate {})", files.cert.display());
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("og-hostd: tls accept: {e}");
                continue;
            }
        };
        // pick up a renewed certificate
        let cfg = {
            let mut c = current.lock().unwrap();
            let (cm, km) = (mtime(&files.cert), mtime(&files.key));
            if cm != c.1 || km != c.2 {
                match load_config(&files) {
                    Ok(n) => *c = (n, cm, km),
                    Err(e) => eprintln!("og-hostd: keeping the old certificate: {e:#}"),
                }
            }
            c.0.clone()
        };
        let router = router.clone();
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
            let Ok(Ok(tls)) = tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(tcp)).await else { return };
            let svc = router.layer(axum::Extension(axum::extract::ConnectInfo::<SocketAddr>(peer))).layer(axum::Extension(Secure));
            let svc = hyper_util::service::TowerToHyperService::new(svc);
            let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), svc)
                .await;
        });
    }
}
