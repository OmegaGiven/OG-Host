//! Minimal OCI distribution client: manifests (incl. multi-arch indexes), blobs, and
//! anonymous bearer-token auth as used by Docker Hub, GHCR, lscr.io and friends.

use std::collections::HashMap;

use futures_util::StreamExt;
use reqwest::{header, StatusCode};
use serde::{Deserialize, Serialize};

use crate::{reference::Reference, sha256_digest, store::Store, Error, Result};

pub const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MT_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MT_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// Biggest manifest/config we accept; real ones are a few KB.
const MAX_JSON_BYTES: usize = 4 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

/// A resolved, single-platform image.
#[derive(Debug, Clone)]
pub struct ResolvedImage {
    pub reference: Reference,
    /// Digest of the platform-specific manifest (what the image *is*).
    pub manifest_digest: String,
    pub manifest: Manifest,
}

/// The current machine's platform in OCI terms.
pub fn host_platform() -> Platform {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    };
    Platform { architecture: architecture.into(), os: "linux".into(), variant: None }
}

pub struct Registry {
    http: reqwest::Client,
    /// Bearer tokens per (host, repository).
    tokens: HashMap<(String, String), String>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("og-host/", env!("CARGO_PKG_VERSION")))
            .https_only(false) // plain http allowed only for localhost registries, checked in url()
            .build()
            .expect("http client");
        Registry { http, tokens: HashMap::new() }
    }

    fn url(r: &Reference, path: &str) -> String {
        let host = r.api_host();
        let insecure = host.starts_with("localhost") || host.starts_with("127.0.0.1");
        format!("{}://{}/v2/{}/{}", if insecure { "http" } else { "https" }, host, r.repository, path)
    }

    /// GET with auth: on 401, follow the WWW-Authenticate challenge for an anonymous token.
    async fn get(&mut self, r: &Reference, path: &str, accept: &str) -> Result<reqwest::Response> {
        let url = Self::url(r, path);
        let key = (r.api_host().to_string(), r.repository.clone());
        for attempt in 0..2 {
            let mut req = self.http.get(&url).header(header::ACCEPT, accept);
            if let Some(t) = self.tokens.get(&key) {
                req = req.bearer_auth(t);
            }
            let resp = req.send().await?;
            if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                let challenge = resp
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| Error::Registry(format!("{url}: 401 without an auth challenge")))?
                    .to_string();
                let token = self.fetch_token(&challenge, r).await?;
                self.tokens.insert(key.clone(), token);
                continue;
            }
            if !resp.status().is_success() {
                return Err(Error::Registry(format!("{url}: HTTP {}", resp.status())));
            }
            return Ok(resp);
        }
        unreachable!()
    }

    async fn fetch_token(&self, challenge: &str, r: &Reference) -> Result<String> {
        let params = parse_challenge(challenge)
            .ok_or_else(|| Error::Unsupported(format!("auth challenge {challenge:?}")))?;
        let realm = params.get("realm").ok_or_else(|| Error::Registry("auth challenge without realm".into()))?;
        if !realm.starts_with("https://") {
            return Err(Error::Registry(format!("refusing non-https token realm {realm}")));
        }
        let scope = params.get("scope").cloned().unwrap_or_else(|| format!("repository:{}:pull", r.repository));
        let mut req = self.http.get(realm.as_str()).query(&[("scope", scope.as_str())]);
        if let Some(service) = params.get("service") {
            req = req.query(&[("service", service.as_str())]);
        }
        #[derive(Deserialize)]
        struct Tok {
            token: Option<String>,
            access_token: Option<String>,
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(Error::Registry(format!("token endpoint: HTTP {}", resp.status())));
        }
        let t: Tok = resp.json().await?;
        t.token.or(t.access_token).ok_or_else(|| Error::Registry("token response had no token".into()))
    }

    async fn get_json_bytes(&mut self, r: &Reference, path: &str, accept: &str) -> Result<(String, Vec<u8>)> {
        let resp = self.get(r, path, accept).await?;
        let media_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if resp.content_length().unwrap_or(0) as usize > MAX_JSON_BYTES {
            return Err(Error::Registry("manifest too large".into()));
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > MAX_JSON_BYTES {
            return Err(Error::Registry("manifest too large".into()));
        }
        Ok((media_type, bytes.to_vec()))
    }

    /// Resolve a reference to a single-platform manifest, verifying digests on the way.
    pub async fn resolve(&mut self, r: &Reference, store: &Store) -> Result<ResolvedImage> {
        let accept = [MT_OCI_INDEX, MT_DOCKER_LIST, MT_OCI_MANIFEST, MT_DOCKER_MANIFEST].join(", ");
        let (mut media_type, mut bytes) = self.get_json_bytes(r, &format!("manifests/{}", r.manifest_ref()), &accept).await?;
        let mut digest = sha256_digest(&bytes);
        if let Some(pinned) = &r.digest {
            if &digest != pinned {
                return Err(Error::DigestMismatch { expected: pinned.clone(), actual: digest });
            }
        }
        if media_type.is_empty() {
            media_type = sniff_media_type(&bytes);
        }
        if media_type == MT_OCI_INDEX || media_type == MT_DOCKER_LIST {
            let index: Index = serde_json::from_slice(&bytes)?;
            let want = host_platform();
            let chosen = index
                .manifests
                .iter()
                .find(|m| m.platform.as_ref().is_some_and(|p| p.os == want.os && p.architecture == want.architecture))
                .ok_or_else(|| Error::Unsupported(format!("{r} has no {}/{} image", want.os, want.architecture)))?
                .clone();
            let (mt, b) = self.get_json_bytes(r, &format!("manifests/{}", chosen.digest), &accept).await?;
            let actual = sha256_digest(&b);
            if actual != chosen.digest {
                return Err(Error::DigestMismatch { expected: chosen.digest, actual });
            }
            media_type = if mt.is_empty() { sniff_media_type(&b) } else { mt };
            bytes = b;
            digest = actual;
        }
        if media_type != MT_OCI_MANIFEST && media_type != MT_DOCKER_MANIFEST {
            return Err(Error::Unsupported(format!("manifest type {media_type:?}")));
        }
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        store.put(&digest, &bytes).await?;
        Ok(ResolvedImage { reference: r.clone(), manifest_digest: digest, manifest })
    }

    /// Download a blob into the store unless it is already there. Verified while streaming.
    pub async fn fetch_blob(&mut self, r: &Reference, d: &Descriptor, store: &Store) -> Result<()> {
        if store.has(&d.digest).await? {
            return Ok(());
        }
        let resp = self.get(r, &format!("blobs/{}", d.digest), "*/*").await?;
        let mut w = store.writer(&d.digest).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if w.written() + chunk.len() as u64 > d.size {
                return Err(Error::Registry(format!("blob {} is larger than its declared {} bytes", d.digest, d.size)));
            }
            w.write(&chunk).await?;
        }
        if w.written() != d.size {
            return Err(Error::Registry(format!("blob {}: got {} bytes, expected {}", d.digest, w.written(), d.size)));
        }
        w.finish().await?;
        Ok(())
    }

    /// Resolve, then fetch the config and every layer. Returns the resolved image.
    pub async fn pull(&mut self, r: &Reference, store: &Store) -> Result<ResolvedImage> {
        let img = self.resolve(r, store).await?;
        self.fetch_blob(r, &img.manifest.config, store).await?;
        for layer in &img.manifest.layers {
            tracing::info!(digest = %layer.digest, size = layer.size, "layer");
            self.fetch_blob(r, layer, store).await?;
        }
        Ok(img)
    }
}

fn sniff_media_type(bytes: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "mediaType", default)]
        media_type: String,
        manifests: Option<serde_json::Value>,
    }
    match serde_json::from_slice::<Probe>(bytes) {
        Ok(p) if !p.media_type.is_empty() => p.media_type,
        Ok(p) if p.manifests.is_some() => MT_OCI_INDEX.into(),
        _ => MT_OCI_MANIFEST.into(),
    }
}

/// Parse `Bearer realm="...",service="...",scope="..."`.
fn parse_challenge(h: &str) -> Option<HashMap<String, String>> {
    let rest = h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer "))?;
    let mut out = HashMap::new();
    let mut s = rest.trim();
    while !s.is_empty() {
        let (k, after) = s.split_once('=')?;
        let after = after.strip_prefix('"')?;
        let (v, tail) = after.split_once('"')?;
        out.insert(k.trim().to_string(), v.to_string());
        s = tail.trim_start_matches(',').trim();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_hub_challenge() {
        let c = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#;
        let m = parse_challenge(c).unwrap();
        assert_eq!(m["realm"], "https://auth.docker.io/token");
        assert_eq!(m["service"], "registry.docker.io");
        assert_eq!(m["scope"], "repository:library/alpine:pull");
        assert!(parse_challenge("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn sniffs_index_without_media_type() {
        assert_eq!(sniff_media_type(br#"{"manifests":[]}"#), MT_OCI_INDEX);
        assert_eq!(sniff_media_type(br#"{"mediaType":"application/vnd.docker.distribution.manifest.v2+json"}"#), MT_DOCKER_MANIFEST);
    }
}

/// The parts of an OCI image config that matter for running it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub config: RunConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RunConfig {
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub exposed_ports: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub volumes: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub labels: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub stop_signal: Option<String>,
}
