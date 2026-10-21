//! Content-addressed blob store: `<root>/blobs/sha256/<hex>`.
//!
//! Blobs are streamed to a temporary file, hashed while writing, and only renamed into
//! place when the digest matches. A blob in the store is therefore always what its name
//! says it is.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{digest_hex, Error, Result};

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// An in-progress blob write; call [`BlobWriter::finish`] to verify and commit it.
pub struct BlobWriter {
    expected: String,
    final_path: PathBuf,
    tmp_path: PathBuf,
    file: tokio::fs::File,
    hasher: Sha256,
    written: u64,
}

impl Store {
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        tokio::fs::create_dir_all(root.join("blobs/sha256")).await?;
        tokio::fs::create_dir_all(root.join("tmp")).await?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.root.join("blobs/sha256").join(digest_hex(digest)?))
    }

    pub async fn has(&self, digest: &str) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.blob_path(digest)?).await?)
    }

    /// Blocking read, for callers outside async code.
    pub fn read_sync(&self, digest: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.blob_path(digest)?)?)
    }

    pub async fn read(&self, digest: &str) -> Result<Vec<u8>> {
        Ok(tokio::fs::read(self.blob_path(digest)?).await?)
    }

    /// Store an in-memory blob (manifests, configs), verifying its digest.
    pub async fn put(&self, digest: &str, bytes: &[u8]) -> Result<PathBuf> {
        let mut w = self.writer(digest).await?;
        w.write(bytes).await?;
        w.finish().await
    }

    pub async fn writer(&self, digest: &str) -> Result<BlobWriter> {
        let final_path = self.blob_path(digest)?;
        let tmp_path = self.root.join("tmp").join(format!("{}.{}", digest_hex(digest)?, std::process::id()));
        let file = tokio::fs::File::create(&tmp_path).await?;
        Ok(BlobWriter { expected: digest.to_string(), final_path, tmp_path, file, hasher: Sha256::new(), written: 0 })
    }
}

impl BlobWriter {
    pub async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        self.hasher.update(chunk);
        self.written += chunk.len() as u64;
        self.file.write_all(chunk).await?;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Verify the digest, fsync and move the blob into place. On mismatch the temporary
    /// file is removed and nothing enters the store.
    pub async fn finish(mut self) -> Result<PathBuf> {
        self.file.flush().await?;
        self.file.sync_all().await?;
        let actual = format!("sha256:{}", hex::encode(self.hasher.finalize_reset()));
        if actual != self.expected {
            let _ = tokio::fs::remove_file(&self.tmp_path).await;
            return Err(Error::DigestMismatch { expected: self.expected, actual });
        }
        tokio::fs::rename(&self.tmp_path, &self.final_path).await?;
        Ok(self.final_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verified_put_and_rejected_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let data = b"hello og-host";
        let digest = crate::sha256_digest(data);
        store.put(&digest, data).await.unwrap();
        assert!(store.has(&digest).await.unwrap());
        assert_eq!(store.read(&digest).await.unwrap(), data);

        let wrong = crate::sha256_digest(b"something else");
        let err = store.put(&wrong, data).await.unwrap_err();
        assert!(matches!(err, Error::DigestMismatch { .. }));
        assert!(!store.has(&wrong).await.unwrap());
        // no stray temp files left behind
        assert_eq!(std::fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_path_tricks_in_digests() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        assert!(store.blob_path("sha256:../../etc/passwd").is_err());
        assert!(store.blob_path("md5:abc").is_err());
    }
}

/// Import an OCI image layout tarball (e.g. `buildctl --output type=oci`) into the store.
/// Every blob is hashed while copied and must match the digest it is filed under; returns the
/// digest of the (single-platform) image manifest.
pub fn import_oci_tar(store_root: &Path, tar_path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let blobs = store_root.join("blobs/sha256");
    std::fs::create_dir_all(&blobs)?;
    std::fs::create_dir_all(store_root.join("tmp"))?;
    let mut index: Option<Vec<u8>> = None;
    let mut archive = tar::Archive::new(std::fs::File::open(tar_path)?);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let path = path.trim_start_matches("./").to_string();
        if path == "index.json" {
            let mut b = Vec::new();
            entry.read_to_end(&mut b)?;
            index = Some(b);
            continue;
        }
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let Some(hexname) = path.strip_prefix("blobs/sha256/") else { continue };
        crate::digest_hex(&format!("sha256:{hexname}"))?; // rejects anything that isn't a clean digest
        let dst = blobs.join(hexname);
        if dst.exists() {
            continue;
        }
        let tmp = store_root.join("tmp").join(format!("{hexname}.import.{}", std::process::id()));
        let mut out = std::fs::File::create(&tmp)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        out.sync_all()?;
        let actual = hex::encode(h.finalize());
        if actual != hexname {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::DigestMismatch { expected: format!("sha256:{hexname}"), actual: format!("sha256:{actual}") });
        }
        std::fs::rename(&tmp, &dst)?;
    }
    let index = index.ok_or_else(|| Error::Unsupported("OCI tarball without index.json".into()))?;
    let v: serde_json::Value = serde_json::from_slice(&index)?;
    let first = v["manifests"][0].clone();
    let mut digest = first["digest"].as_str().ok_or_else(|| Error::Unsupported("empty OCI index".into()))?.to_string();
    // a nested index (multi-platform output): pick this machine's platform
    if first["mediaType"].as_str() == Some(crate::registry::MT_OCI_INDEX) {
        let inner: serde_json::Value = serde_json::from_slice(&std::fs::read(blobs.join(crate::digest_hex(&digest)?))?)?;
        let want = crate::registry::host_platform();
        digest = inner["manifests"]
            .as_array()
            .and_then(|m| m.iter().find(|d| d["platform"]["architecture"].as_str() == Some(&want.architecture)))
            .and_then(|d| d["digest"].as_str())
            .ok_or_else(|| Error::Unsupported("built image has no manifest for this platform".into()))?
            .to_string();
    }
    Ok(digest)
}
