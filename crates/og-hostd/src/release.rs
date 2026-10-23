//! Making releases and mirrors (for whoever publishes OG-Host):
//!
//! - `og-hostd release keygen DIR`: an ed25519 release key (`og-host-release.key`, keep it
//!   safe and offline) and its public half (`.pub` hex for og-hostd, `.pem` for openssl).
//! - `og-hostd release make ...`: pack the binaries, sign the channel, refresh the mirror's
//!   website and installer.
//! - `og-hostd mirror sync URL DIR`: copy a mirror (checking signatures and checksums) to
//!   serve it elsewhere.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ed25519_dalek::Signer;
use serde_json::{json, Value};
use sha2::Digest;

use crate::update::{Artifact, Manifest};

fn read_key(path: &Path) -> Result<ed25519_dalek::SigningKey> {
    let t = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let b: [u8; 32] = hex::decode(t.trim()).ok().and_then(|v| v.try_into().ok()).context("a release key file holds 64 hex characters")?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&b))
}

/// SubjectPublicKeyInfo PEM of an ed25519 key (what `openssl pkeyutl -verify` reads).
fn spki_pem(k: &ed25519_dalek::VerifyingKey) -> String {
    use base64::Engine as _;
    let mut der = hex::decode("302a300506032b6570032100").unwrap();
    der.extend_from_slice(k.as_bytes());
    format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", base64::engine::general_purpose::STANDARD.encode(der))
}

pub fn keygen(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let key = dir.join("og-host-release.key");
    if key.exists() {
        bail!("{} exists already", key.display());
    }
    let mut seed = [0u8; 32];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
    }
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&key)?.write_all(hex::encode(seed).as_bytes())?;
    }
    let vk = sk.verifying_key();
    std::fs::write(dir.join("og-host-release.pub"), format!("{}\n", hex::encode(vk.as_bytes())))?;
    std::fs::write(dir.join("og-host-release.pem"), spki_pem(&vk))?;
    println!("release key: {} (keep it safe; it signs every update)", key.display());
    println!("public key:  {} (key id {})", hex::encode(vk.as_bytes()), crate::update::key_id(&vk));
    Ok(())
}

/// Vouch for a key with a root key: add it (or renew it) in the mirror's `keys/delegations.json`
/// and re-sign the list with a higher version. `revoke`: drop it instead.
pub fn delegate(root_key: &Path, key: &str, name: &str, days: u64, channels: &[String], mirror: &Path, revoke: bool) -> Result<()> {
    let sk = read_key(root_key)?;
    let hexkey = if Path::new(key).exists() { std::fs::read_to_string(key)?.trim().to_string() } else { key.trim().to_string() };
    let b: [u8; 32] = hex::decode(&hexkey).ok().and_then(|v| v.try_into().ok()).context("the key is 64 hex characters (or a .pub file holding them)")?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&b)?;
    let dir = mirror.join("keys");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("delegations.json");
    let mut d: crate::update::Delegations = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    d.keys.retain(|x| x.key.trim() != hexkey);
    if !revoke {
        for c in channels {
            if !["stable", "beta", "dev"].contains(&c.as_str()) {
                bail!("channels are stable, beta, dev");
            }
        }
        d.keys.push(crate::update::Delegation { key: hexkey.clone(), name: name.into(), not_after: crate::containers::unix_now() + days * 86400, channels: channels.to_vec() });
    }
    d.version += 1;
    let rvk = sk.verifying_key();
    std::fs::write(dir.join("og-host-release.pub"), format!("{}\n", hex::encode(rvk.as_bytes())))?;
    std::fs::write(dir.join("og-host-release.pem"), spki_pem(&rvk))?;
    std::fs::create_dir_all(dir.join("delegated"))?;
    if !revoke {
        std::fs::write(dir.join("delegated").join(format!("{}.pem", crate::update::key_id(&vk))), spki_pem(&vk))?;
    } else {
        let _ = std::fs::remove_file(dir.join("delegated").join(format!("{}.pem", crate::update::key_id(&vk))));
    }
    let body = serde_json::to_vec_pretty(&d)?;
    std::fs::write(&path, &body)?;
    std::fs::write(path.with_extension("json.sig"), sign(&sk, &body))?;
    println!("{} key {} ({name}); key list version {}", if revoke { "revoked" } else { "vouched for" }, crate::update::key_id(&vk), d.version);
    Ok(())
}

fn sign(sk: &ed25519_dalek::SigningKey, data: &[u8]) -> String {
    format!("{}\n{}\n", hex::encode(sk.sign(data).to_bytes()), crate::update::key_id(&sk.verifying_key()))
}

fn valid_version(v: &str) -> bool {
    !v.is_empty() && v.len() < 40 && v.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)) && v.chars().next().is_some_and(|c| c.is_ascii_digit())
}

pub struct MakeOpts {
    pub version: String,
    pub channel: String,
    pub notes: Option<PathBuf>,
    pub key: PathBuf,
    pub mirror: PathBuf,
    pub bins: Vec<PathBuf>,
    pub arch: Option<String>,
    /// the mirror's public URL (written into install.sh)
    pub url: Option<String>,
}

pub fn make(o: &MakeOpts) -> Result<()> {
    if !valid_version(&o.version) {
        bail!("bad version {:?}", o.version);
    }
    if !["stable", "beta", "dev"].contains(&o.channel.as_str()) {
        bail!("channel is stable, beta or dev");
    }
    let sk = read_key(&o.key)?;
    let arch = o.arch.clone().unwrap_or_else(|| crate::update::arch().to_string());
    // the binary must say the version it's released as
    let main = o.bins.iter().find(|b| b.file_name().is_some_and(|n| n == "og-hostd")).context("--bin .../og-hostd is required")?;
    let out = std::process::Command::new(main).arg("--version").output().context("running the og-hostd to release")?;
    if !String::from_utf8_lossy(&out.stdout).contains(&o.version) {
        bail!("{} says {:?}, not {}", main.display(), String::from_utf8_lossy(&out.stdout).trim(), o.version);
    }
    let rel = format!("releases/{}/og-host-{}-{arch}.tar.zst", o.version, o.version);
    let dest = o.mirror.join(&rel);
    std::fs::create_dir_all(dest.parent().unwrap())?;
    {
        let f = std::fs::File::create(&dest)?;
        let z = zstd::stream::write::Encoder::new(f, 19)?.auto_finish();
        let mut t = tar::Builder::new(z);
        for b in &o.bins {
            let name = b.file_name().context("bad --bin")?.to_string_lossy().into_owned();
            let mut h = tar::Header::new_gnu();
            let data = std::fs::read(b)?;
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_mtime(crate::containers::unix_now());
            h.set_cksum();
            t.append_data(&mut h, format!("bin/{name}"), &data[..])?;
        }
        t.into_inner()?;
    }
    let bytes = std::fs::read(&dest)?;
    let art = Artifact { arch: arch.clone(), file: rel.clone(), sha256: hex::encode(sha2::Sha256::digest(&bytes)), size: bytes.len() as u64 };
    let notes = match &o.notes {
        Some(p) => std::fs::read_to_string(p)?,
        None => String::new(),
    };
    // a channel file: same version keeps the other architectures' builds
    let chan = o.mirror.join("channels").join(format!("{}.json", o.channel));
    std::fs::create_dir_all(chan.parent().unwrap())?;
    let mut m: Manifest = std::fs::read(&chan).ok().and_then(|b| serde_json::from_slice(&b).ok()).filter(|m: &Manifest| m.version == o.version).unwrap_or_default();
    m.format = 1;
    m.channel = o.channel.clone();
    m.version = o.version.clone();
    m.date = crate::containers::unix_now();
    if !notes.is_empty() || m.notes.is_empty() {
        m.notes = notes;
    }
    m.artifacts.retain(|a| a.arch != arch);
    m.artifacts.push(art.clone());
    let body = serde_json::to_vec_pretty(&m)?;
    std::fs::write(&chan, &body)?;
    std::fs::write(chan.with_extension("json.sig"), sign(&sk, &body))?;
    // release history for the website
    let idx_path = o.mirror.join("releases/index.json");
    let mut idx: Vec<Value> = std::fs::read(&idx_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    idx.retain(|r| r["version"] != json!(o.version));
    idx.push(json!({"version": m.version, "date": m.date, "channel": o.channel, "notes": m.notes, "artifacts": m.artifacts}));
    idx.sort_by(|a, b| crate::update::version_key(b["version"].as_str().unwrap_or("")).cmp(&crate::update::version_key(a["version"].as_str().unwrap_or(""))));
    let ib = serde_json::to_vec_pretty(&idx)?;
    std::fs::write(&idx_path, &ib)?;
    std::fs::write(idx_path.with_extension("json.sig"), sign(&sk, &ib))?;
    // the signer's public key: a release (root) key, or a delegated key (published for install.sh;
    // trusted only through the root-signed delegations.json)
    let vk = sk.verifying_key();
    let root_signed = crate::update::BUILTIN_KEYS.iter().any(|h| *h == hex::encode(vk.as_bytes()));
    let keys = o.mirror.join("keys");
    let kdir = if root_signed { keys.clone() } else { keys.join("delegated") };
    std::fs::create_dir_all(&kdir)?;
    if root_signed {
        std::fs::write(keys.join("og-host-release.pub"), format!("{}\n", hex::encode(vk.as_bytes())))?;
        std::fs::write(keys.join("og-host-release.pem"), spki_pem(&vk))?;
    } else {
        std::fs::write(kdir.join(format!("{}.pem", crate::update::key_id(&vk))), spki_pem(&vk))?;
    }
    std::fs::write(o.mirror.join("index.html"), SITE)?;
    std::fs::write(o.mirror.join("install.sh"), install_script(o.url.as_deref().unwrap_or("__MIRROR__")))?;
    std::fs::write(o.mirror.join("favicon.svg"), include_str!("../ui/favicon.svg"))?;
    println!("released {} ({arch}) on {}: {} ({:.1} MiB, sha256 {})", o.version, o.channel, rel, art.size as f64 / 1048576.0, &art.sha256[..16]);
    Ok(())
}

/// Copy a mirror, checking every channel's signature and every artifact's checksum.
pub async fn sync(root: &Path, from: &str, to: &Path) -> Result<()> {
    let base = from.trim_end_matches('/');
    let c = reqwest::Client::builder().use_rustls_tls().build()?;
    let get = |p: String| {
        let c = c.clone();
        async move { Ok::<_, anyhow::Error>(c.get(p).send().await?.error_for_status()?.bytes().await?) }
    };
    for f in ["index.html", "install.sh", "favicon.svg", "keys/og-host-release.pub", "keys/og-host-release.pem", "releases/index.json", "releases/index.json.sig"] {
        if let Ok(b) = get(format!("{base}/{f}")).await {
            let p = to.join(f);
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(p, b)?;
        }
    }
    for ch in ["stable", "beta", "dev"] {
        let Ok(body) = get(format!("{base}/channels/{ch}.json")).await else { continue };
        let sig = String::from_utf8_lossy(&get(format!("{base}/channels/{ch}.json.sig")).await?).into_owned();
        let key = crate::update::verify(root, &body, &sig).with_context(|| format!("channel {ch}"))?;
        let m: Manifest = serde_json::from_slice(&body)?;
        for a in &m.artifacts {
            if a.file.contains("..") || a.file.starts_with('/') {
                bail!("bad artifact path {}", a.file);
            }
            let dest = to.join(&a.file);
            if dest.exists() && hex::encode(sha2::Sha256::digest(std::fs::read(&dest)?)) == a.sha256 {
                continue;
            }
            let b = get(format!("{base}/{}", a.file)).await?;
            if hex::encode(sha2::Sha256::digest(&b)) != a.sha256 {
                bail!("{}: checksum mismatch", a.file);
            }
            std::fs::create_dir_all(dest.parent().unwrap())?;
            std::fs::write(&dest, b)?;
            println!("  {}", a.file);
        }
        let d = to.join("channels");
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join(format!("{ch}.json")), &body)?;
        std::fs::write(d.join(format!("{ch}.json.sig")), sig)?;
        println!("{ch}: {} (signed by {key})", m.version);
    }
    Ok(())
}

/// The mirror's web page: what OG-Host is, the current releases, how to install and verify.
const SITE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OG-Host downloads</title>
<meta name="description" content="OG-Host: a fast, open source server OS for containers and virtual machines, managed from the browser.">
<link rel="icon" href="favicon.svg" type="image/svg+xml">
<style>
:root { --bg: #0b0d12; --card: #12151c; --card-2: #171b24; --line: #232836; --text: #eef0e8; --text-2: #b3b8c4; --text-3: #7d8494; --accent: #9b8cff; --good: #4fd18b; --mono: ui-monospace, "JetBrains Mono", "SF Mono", Menlo, monospace; }
@media (prefers-color-scheme: light) { :root:not([data-theme="dark"]) { --bg: #f6f6f3; --card: #ffffff; --card-2: #f0f0ec; --line: #e2e2dc; --text: #16181d; --text-2: #4a4f5c; --text-3: #7a7f8a; --accent: #5b4bd6; --good: #1f8f55; } }
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--text); font: 15px/1.6 Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }
a { color: var(--accent); }
.wrap { max-width: 960px; margin: 0 auto; padding: 0 16px; }
header { padding: 40px 0 24px; display: flex; align-items: center; gap: 16px; }
.mark { width: 56px; height: 56px; border-radius: 14px; background: #16181d; color: #eef0e8; display: grid; place-items: center; line-height: 1; font-weight: 800; text-align: center; flex: none; box-shadow: 0 8px 30px -10px rgba(0,0,0,.6); }
.mark b { font-size: 20px; display: block; letter-spacing: .02em; } .mark small { font-size: 9.5px; letter-spacing: .18em; font-weight: 700; display: block; margin-top: 2px; }
h1 { margin: 0; font-size: 28px; letter-spacing: -.01em; } .tag { margin: 2px 0 0; color: var(--text-2); }
.hero { background: var(--card); border: 1px solid var(--line); border-radius: 18px; padding: 24px; display: grid; gap: 14px; }
.hero h2 { margin: 0; font-size: 20px; } .muted { color: var(--text-3); } .small { font-size: 13px; }
.row { display: flex; gap: 10px; flex-wrap: wrap; align-items: center; }
.btn { display: inline-flex; align-items: center; gap: 8px; background: var(--accent); color: #fff; padding: 10px 16px; border-radius: 10px; text-decoration: none; font-weight: 600; }
.btn.ghost { background: transparent; color: var(--text); border: 1px solid var(--line); }
pre { background: var(--card-2); border: 1px solid var(--line); border-radius: 10px; padding: 12px 14px; overflow-x: auto; font: 13px/1.5 var(--mono); margin: 0; }
.grid { display: grid; grid-template-columns: repeat(2, 1fr); gap: 12px; margin: 22px 0; }
@media (max-width: 640px) { .grid { grid-template-columns: 1fr; } }
.card { background: var(--card); border: 1px solid var(--line); border-radius: 14px; padding: 16px; }
.card h3 { margin: 0 0 6px; font-size: 15px; } .card p { margin: 0; color: var(--text-2); font-size: 14px; }
h2.sec { font-size: 13px; text-transform: uppercase; letter-spacing: .1em; color: var(--text-3); margin: 34px 0 10px; }
.rel { background: var(--card); border: 1px solid var(--line); border-radius: 14px; padding: 14px 16px; margin-bottom: 10px; }
.rel .head { display: flex; gap: 10px; align-items: baseline; flex-wrap: wrap; } .rel .head b { font-size: 16px; }
.chip { font-size: 11.5px; border: 1px solid var(--line); border-radius: 6px; padding: 0 6px; color: var(--text-2); }
.chip.good { color: var(--good); }
.notes { white-space: pre-wrap; color: var(--text-2); font-size: 14px; margin: 8px 0 0; }
.files { font: 12.5px var(--mono); color: var(--text-3); margin-top: 8px; word-break: break-all; }
footer { padding: 40px 0; color: var(--text-3); font-size: 13px; }
</style>
</head>
<body>
<div class="wrap">
<header><div class="mark"><b>OG</b><small>HOST</small></div><div><h1>OG-Host</h1><p class="tag">A fast, open source server OS for containers and virtual machines, run from your browser.</p></div></header>
<section class="hero">
  <h2 id="latest">Loading the latest release…</h2>
  <div class="row" id="dl"></div>
  <div><div class="muted small" style="margin-bottom:6px">Install on a Debian or Ubuntu server (as root):</div><pre id="cmd">curl -fsSL <span class="u"></span>/install.sh | sh</pre></div>
  <p class="muted small">Already running OG-Host? It updates itself from this mirror: <b>Settings → Updates</b>. Every release is signed; servers only install what the release key signed.</p>
</section>
<div class="grid">
  <div class="card"><h3>Containers, natively</h3><p>Its own engine: Docker and Compose compatible, rootless by default, GPUs, live resource caps.</p></div>
  <div class="card"><h3>Virtual machines</h3><p>KVM with UEFI, Secure Boot and TPM 2.0 for Windows 11, a console in the browser, and a mode that looks like a real PC.</p></div>
  <div class="card"><h3>Backups that dedupe</h3><p>Encrypted, deduplicated backups of stacks and VMs to disks or shares, copied to your other sites.</p></div>
  <div class="card"><h3>Clusters across sites</h3><p>Join servers at home and elsewhere: one UI, shared accounts with two-factor, mutual TLS between nodes.</p></div>
</div>
<h2 class="sec">Releases</h2>
<div id="rels"><p class="muted">…</p></div>
<h2 class="sec">Check a download</h2>
<pre>curl -fsSLO <span class="u"></span>/channels/stable.json
curl -fsSLO <span class="u"></span>/channels/stable.json.sig
curl -fsSLO <span class="u"></span>/keys/og-host-release.pem
head -1 stable.json.sig | xxd -r -p > stable.sig
openssl pkeyutl -verify -pubin -inkey og-host-release.pem -rawin -in stable.json -sigfile stable.sig
sha256sum og-host-*.tar.zst   # compare with "sha256" in stable.json</pre>
<p class="muted small">Release key: <span id="key" style="font-family:var(--mono)">…</span></p>
<h2 class="sec">Run a mirror</h2>
<p class="muted small">Any web server can serve a copy of this directory. To make one that checks every signature on the way: <code>og-hostd mirror sync <span class="u"></span> /srv/og-host-mirror</code>, then point your servers' update settings at it.</p>
<footer>OG-Host is free software (MPL-2.0) by OmegaGiven. No subscriptions, no nag screens.</footer>
</div>
<script>
const base = location.href.replace(/[#?].*$/, '').replace(/\/(index\.html)?$/, '');
document.querySelectorAll('.u').forEach((e) => (e.textContent = base));
const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const mb = (b) => `${(b / 1048576).toFixed(1)} MiB`;
fetch('keys/og-host-release.pub').then((r) => r.text()).then((t) => (document.getElementById('key').textContent = t.trim())).catch(() => {});
fetch('releases/index.json').then((r) => r.json()).then((list) => {
  const stable = list.find((r) => r.channel === 'stable') || list[0];
  if (stable) {
    document.getElementById('latest').innerHTML = `OG-Host ${esc(stable.version)} <span class="chip good">${esc(stable.channel)}</span> <span class="muted small">${new Date(stable.date * 1000).toLocaleDateString()}</span>`;
    document.getElementById('dl').innerHTML = stable.artifacts.map((a) => `<a class="btn" href="${esc(a.file)}">Download for ${esc(a.arch)} · ${mb(a.size)}</a>`).join('') + '<a class="btn ghost" href="#rels">Release notes</a>';
  }
  document.getElementById('rels').innerHTML = list.map((r) => `<div class="rel"><div class="head"><b>${esc(r.version)}</b><span class="chip">${esc(r.channel)}</span><span class="muted small">${new Date(r.date * 1000).toLocaleString()}</span></div>${r.notes ? `<div class="notes">${esc(r.notes)}</div>` : ''}<div class="files">${r.artifacts.map((a) => `<a href="${esc(a.file)}">${esc(a.file.split('/').pop())}</a> · sha256 ${esc(a.sha256)}`).join('<br>')}</div></div>`).join('') || '<p class="muted">No releases yet.</p>';
}).catch(() => { document.getElementById('latest').textContent = 'No releases yet'; });
</script>
</body>
</html>
"##;

/// `curl .../install.sh | sh`: get og-hostd from this mirror, check its signature (with
/// openssl, when there) and checksum, install it and its systemd unit.
const INSTALL: &str = r##"#!/bin/sh
# OG-Host installer: downloads the current release from this mirror, checks it, installs
# /usr/local/bin/og-hostd (+ og-shim) and starts og-hostd.service. Run as root.
set -eu
MIRROR="${OG_MIRROR:-__MIRROR__}"
CHANNEL="${OG_CHANNEL:-stable}"
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
ARCH=$(uname -m)
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
cd "$TMP"
fetch() { curl -fsSL "$MIRROR/$1" -o "$2"; }
fetch "channels/$CHANNEL.json" manifest.json
fetch "channels/$CHANNEL.json.sig" manifest.sig
fetch keys/og-host-release.pem root.pem
# verify FILE (signature SIG: hex line + key id line) with PEM
verify() { head -1 "$2" | xxd -r -p > sig.bin; openssl pkeyutl -verify -pubin -inkey "$1" -rawin -in "$3" -sigfile sig.bin >/dev/null 2>&1; }
if command -v openssl >/dev/null 2>&1 && command -v xxd >/dev/null 2>&1; then
  if verify root.pem manifest.sig manifest.json; then
    echo "signature ok (release key)"
  else
    # signed by a delegated (CI) key: the release key must vouch for it in keys/delegations.json
    KID=$(sed -n 2p manifest.sig | tr -cd '0-9a-f')
    fetch keys/delegations.json dl.json && fetch keys/delegations.json.sig dl.sig || { echo "signature check FAILED: not installing" >&2; exit 1; }
    verify root.pem dl.sig dl.json || { echo "the key list isn't signed by the release key: not installing" >&2; exit 1; }
    fetch "keys/delegated/$KID.pem" signer.pem
    HEX=$(openssl pkey -pubin -in signer.pem -outform DER | tail -c 32 | xxd -p -c 64)
    grep -q "\"$HEX\"" dl.json || { echo "the signing key isn't vouched for: not installing" >&2; exit 1; }
    verify signer.pem manifest.sig manifest.json || { echo "signature check FAILED: not installing" >&2; exit 1; }
    echo "signature ok (key $KID, vouched for by the release key)"
  fi
else
  echo "(openssl/xxd not found: skipping the signature check, using the checksum only)"
fi
# the artifact for this architecture: file and sha256 (no jq needed)
LINE=$(tr -d '\n ' < manifest.json | sed 's/},{/}\n{/g' | grep "\"arch\":\"$ARCH\"" || true)
[ -n "$LINE" ] || { echo "no OG-Host build for $ARCH on channel $CHANNEL" >&2; exit 1; }
FILE=$(echo "$LINE" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
SUM=$(echo "$LINE" | sed -n 's/.*"sha256":"\([^"]*\)".*/\1/p')
VER=$(tr -d '\n ' < manifest.json | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')
echo "OG-Host $VER for $ARCH"
fetch "$FILE" og-host.tar.zst
echo "$SUM  og-host.tar.zst" | sha256sum -c - >/dev/null || { echo "checksum FAILED" >&2; exit 1; }
command -v zstd >/dev/null 2>&1 || { apt-get install -y -q zstd >/dev/null 2>&1 || true; }
mkdir x && zstd -dc og-host.tar.zst | tar -x -C x
install -m 755 x/bin/og-hostd /usr/local/bin/og-hostd
[ -f x/bin/og-shim ] && install -m 755 x/bin/og-shim /usr/local/bin/og-shim
if command -v apt-get >/dev/null 2>&1; then
  DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends iproute2 nftables >/dev/null 2>&1 || true
fi
cat > /etc/systemd/system/og-hostd.service <<'UNIT'
[Unit]
Description=OG-Host node daemon (containers, VMs, networking, API)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/og-hostd serve --listen 127.0.0.1:8411 --listen-tls 0.0.0.0:8006
Restart=always
RestartSec=2
KillMode=process

[Install]
WantedBy=multi-user.target
UNIT
mkdir -p /var/lib/og-host
[ -f /var/lib/og-host/update.json ] || printf '{"channel":"%s","mirrors":["%s"],"auto":"install","schedule":"daily 03:15","system":true,"reboot":"auto"}\n' "$CHANNEL" "$MIRROR" > /var/lib/og-host/update.json
systemctl daemon-reload
systemctl enable --now og-hostd
echo
echo "OG-Host $VER is running. Make your first account:"
echo "  og-hostd passwd admin"
echo "then open https://$(hostname -I 2>/dev/null | cut -d' ' -f1):8006"
"##;

pub fn install_script(mirror_url: &str) -> String {
    INSTALL.replace("__MIRROR__", mirror_url)
}
