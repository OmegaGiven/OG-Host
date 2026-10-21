//! og-oci: everything OG-Host needs to get a Docker/OCI image onto disk safely.
//!
//! - [`reference`]: parse and normalise image references.
//! - [`registry`]: talk to OCI/Docker registries (anonymous bearer tokens, multi-arch indexes).
//! - [`store`]: content-addressed blob store; every byte is checked against its digest.
//! - [`unpack`]: apply layer tarballs to a root filesystem, with whiteouts, refusing any
//!   entry that would land outside the root.

pub mod reference;
pub mod registry;
pub mod store;
pub mod unpack;

pub use reference::Reference;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid image reference: {0:?}")]
    BadReference(String),
    #[error("registry error: {0}")]
    Registry(String),
    #[error("digest mismatch for {expected}: got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("unsafe layer entry {0:?}: path escapes the root filesystem")]
    UnsafePath(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// `sha256:<hex>` digest of a byte slice.
pub fn sha256_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// Validate a digest string and return its hex part. Only sha256 is accepted.
pub fn digest_hex(digest: &str) -> Result<&str> {
    match digest.strip_prefix("sha256:") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) => Ok(h),
        _ => Err(Error::Unsupported(format!("digest {digest:?} (only lowercase sha256 is accepted)"))),
    }
}
