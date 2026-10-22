//! Small JSON files in the lab directory, written atomically (tmp + rename).

use std::path::Path;

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub fn load<T: DeserializeOwned>(dir: &Path, name: &str) -> Option<T> {
    serde_json::from_slice(&std::fs::read(dir.join(name)).ok()?).ok()
}

fn write(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let p = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?.write_all(bytes)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// Save, logging (not failing) on error: callers are background state updates.
pub fn save<T: Serialize + ?Sized>(dir: &Path, name: &str, v: &T) {
    let r = serde_json::to_vec_pretty(v).map_err(anyhow::Error::from).and_then(|b| write(dir, name, &b, 0o644));
    if let Err(e) = r {
        eprintln!("og-lab: saving {name}: {e:#}");
    }
}

/// Save a file holding secrets (mode 0600).
pub fn save_private<T: Serialize + ?Sized>(dir: &Path, name: &str, v: &T) -> Result<()> {
    write(dir, name, &serde_json::to_vec_pretty(v)?, 0o600)
}
