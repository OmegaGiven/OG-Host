//! Image builds through BuildKit (`og-buildkitd.service`, unix:///run/og-host/buildkit.sock).
//! BuildKit writes an OCI image tarball; it's imported into OG-Host's store (every blob
//! verified) and tagged, so built images are handled exactly like pulled ones.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub const BUILDKIT_ADDR: &str = "unix:///run/og-host/buildkit.sock";

pub struct BuildSpec {
    pub context: PathBuf,
    /// Dockerfile path; relative paths are inside the context.
    pub dockerfile: Option<String>,
    pub tag: String,
    pub args: BTreeMap<String, String>,
    pub target: Option<String>,
}

/// Build and tag; returns the manifest digest. Progress goes to stderr.
pub fn build(root: &Path, spec: &BuildSpec) -> Result<String> {
    build_with(root, spec, false).map(|(d, _)| d)
}

/// Like [`build`], optionally capturing BuildKit's progress output instead of printing it.
pub fn build_with(root: &Path, spec: &BuildSpec, capture: bool) -> Result<(String, String)> {
    let context = std::fs::canonicalize(&spec.context).with_context(|| format!("build context {}", spec.context.display()))?;
    let df = spec.dockerfile.clone().unwrap_or_else(|| "Dockerfile".into());
    let df_path = if Path::new(&df).is_absolute() { PathBuf::from(&df) } else { context.join(&df) };
    if !df_path.is_file() {
        bail!("no Dockerfile at {}", df_path.display());
    }
    let r = og_oci::Reference::parse(&spec.tag).with_context(|| format!("tag {:?}", spec.tag))?;
    let tmp_dir = root.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let out = tmp_dir.join(format!("build-{}-{}.tar", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()));

    let mut cmd = Command::new("buildctl");
    cmd.arg("--addr").arg(BUILDKIT_ADDR).arg("build").arg("--frontend").arg("dockerfile.v0")
        .arg("--local").arg(format!("context={}", context.display()))
        .arg("--local").arg(format!("dockerfile={}", df_path.parent().unwrap().display()))
        .arg("--opt").arg(format!("filename={}", df_path.file_name().unwrap().to_string_lossy()))
        .arg("--progress").arg("plain");
    for (k, v) in &spec.args {
        cmd.arg("--opt").arg(format!("build-arg:{k}={v}"));
    }
    if let Some(t) = &spec.target {
        cmd.arg("--opt").arg(format!("target={t}"));
    }
    cmd.arg("--output").arg(format!("type=oci,dest={}", out.display()));
    let t0 = std::time::Instant::now();
    let (status, log) = if capture {
        let o = cmd.output().context("running buildctl (is og-buildkitd running?)")?;
        let mut log = String::from_utf8_lossy(&o.stdout).into_owned();
        log.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status, log)
    } else {
        (cmd.status().context("running buildctl (is og-buildkitd running?)")?, String::new())
    };
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        let tail: Vec<&str> = log.lines().rev().take(15).collect();
        bail!("build of {} failed (buildctl exit {status}){}", spec.tag, if capture { format!(":\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n")) } else { String::new() });
    }
    let digest = og_oci::store::import_oci_tar(&root.join("content"), &out).context("importing the built image")?;
    let _ = std::fs::remove_file(&out);
    let ref_file = root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_"));
    std::fs::create_dir_all(ref_file.parent().unwrap())?;
    std::fs::write(&ref_file, &digest)?;
    eprintln!("built {} ({}) in {:.1}s", r, &digest[..19], t0.elapsed().as_secs_f64());
    Ok((digest, log))
}

/// Is this tag present locally?
pub fn have_image(root: &Path, tag: &str) -> bool {
    og_oci::Reference::parse(tag)
        .map(|r| root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_")).exists())
        .unwrap_or(false)
}
