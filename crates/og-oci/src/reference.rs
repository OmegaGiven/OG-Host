//! Image references: `nginx`, `nginx:1.27`, `library/nginx`, `ghcr.io/owner/app:tag`,
//! `registry:5000/app@sha256:...`. Normalised the way Docker does it.

use std::fmt;

use crate::Error;

pub const DOCKER_HUB: &str = "docker.io";
/// Docker Hub's API host differs from its name in references.
pub const DOCKER_HUB_API: &str = "registry-1.docker.io";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Registry host as written in references (`docker.io`, `ghcr.io`, `host:5000`).
    pub registry: String,
    /// Repository path (`library/nginx`, `owner/app`).
    pub repository: String,
    /// Tag, when no digest pins the image (`latest` by default).
    pub tag: Option<String>,
    /// `sha256:<hex>` when the reference pins a digest.
    pub digest: Option<String>,
}

impl Reference {
    pub fn parse(input: &str) -> Result<Self, Error> {
        let s = input.trim();
        if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::BadReference(input.to_string()));
        }
        let (rest, digest) = match s.split_once('@') {
            Some((r, d)) => {
                let hex = d.strip_prefix("sha256:").ok_or_else(|| Error::BadReference(input.into()))?;
                if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(Error::BadReference(input.into()));
                }
                (r, Some(d.to_ascii_lowercase()))
            }
            None => (s, None),
        };
        // A tag is the part after the last ':' that comes after the last '/'
        // (so `host:5000/app` keeps its port).
        let last_slash = rest.rfind('/').map(|i| i + 1).unwrap_or(0);
        let (name, tag) = match rest[last_slash..].rfind(':') {
            Some(i) => (&rest[..last_slash + i], Some(rest[last_slash + i + 1..].to_string())),
            None => (rest, None),
        };
        if let Some(t) = &tag {
            let ok = !t.is_empty() && t.len() <= 128
                && t.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
                && !t.starts_with(['.', '-']);
            if !ok {
                return Err(Error::BadReference(input.into()));
            }
        }
        // The first component is a registry when it looks like a host.
        let (registry, repo) = match name.split_once('/') {
            Some((first, more)) if first.contains('.') || first.contains(':') || first == "localhost" => {
                (first.to_ascii_lowercase(), more.to_string())
            }
            _ => (DOCKER_HUB.to_string(), name.to_string()),
        };
        // registry host: dot-separated labels of [a-z0-9-] (no empty labels, so no ".."), optional :port
        let (host, port) = match registry.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (registry.as_str(), None),
        };
        let host_ok = !host.is_empty()
            && host.split('.').all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !l.starts_with('-'));
        let port_ok = port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.chars().all(|c| c.is_ascii_digit()));
        if !host_ok || !port_ok {
            return Err(Error::BadReference(input.into()));
        }
        let repository = if registry == DOCKER_HUB && !repo.contains('/') { format!("library/{repo}") } else { repo };
        let valid_component = |c: &str| {
            !c.is_empty()
                && c.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || "._-".contains(ch))
                && c.starts_with(|ch: char| ch.is_ascii_alphanumeric())
        };
        if !repository.split('/').all(valid_component) {
            return Err(Error::BadReference(input.into()));
        }
        let tag = if digest.is_none() { Some(tag.unwrap_or_else(|| "latest".into())) } else { tag };
        Ok(Reference { registry, repository, tag, digest })
    }

    /// Host to talk to for the registry API.
    pub fn api_host(&self) -> &str {
        if self.registry == DOCKER_HUB { DOCKER_HUB_API } else { &self.registry }
    }

    /// What to ask the manifest endpoint for: the digest if pinned, else the tag.
    pub fn manifest_ref(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Reference {
        Reference::parse(s).unwrap()
    }

    #[test]
    fn docker_hub_short_names() {
        let r = p("nginx");
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()), ("docker.io", "library/nginx", Some("latest")));
        assert_eq!(r.api_host(), "registry-1.docker.io");
        assert_eq!(p("nginx:1.27-alpine").tag.as_deref(), Some("1.27-alpine"));
        assert_eq!(p("linuxserver/sonarr").repository, "linuxserver/sonarr");
    }

    #[test]
    fn other_registries_and_ports() {
        let r = p("ghcr.io/immich-app/immich-server:v2");
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()), ("ghcr.io", "immich-app/immich-server", Some("v2")));
        let r = p("localhost:5000/app");
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()), ("localhost:5000", "app", Some("latest")));
        let r = p("lscr.io/linuxserver/chromium:latest");
        assert_eq!(r.api_host(), "lscr.io");
    }

    #[test]
    fn digests() {
        let d = "sha256:".to_string() + &"a".repeat(64);
        let r = p(&format!("alpine@{d}"));
        assert_eq!((r.tag.as_deref(), r.digest.as_deref()), (None, Some(d.as_str())));
        assert_eq!(r.manifest_ref(), d);
        assert!(Reference::parse("alpine@sha256:abc").is_err());
    }

    #[test]
    fn rejects_bad_input() {
        for bad in ["", "Nginx", "a b", "nginx:", "nginx:-x", "../etc", "repo//x", "x:y:z/..", "a\nb", "evil..com/x", ".host.io/x", "host.io:99999999/x", "host.io:ab/x"] {
            assert!(Reference::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn display_round_trip() {
        assert_eq!(p("nginx").to_string(), "docker.io/library/nginx:latest");
        assert_eq!(p("ghcr.io/a/b:c").to_string(), "ghcr.io/a/b:c");
    }
}
