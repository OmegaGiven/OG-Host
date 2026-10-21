//! og-hostd — OG-Host node daemon. For now: image pull + unpack, the first engine milestone.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use og_oci::{registry::Registry, store::Store, unpack, Reference};

#[derive(Parser)]
#[command(name = "og-hostd", version, about = "OG-Host node daemon")]
struct Cli {
    /// State directory (images, containers, volumes).
    #[arg(long, default_value = "/var/lib/og-host", global = true)]
    root: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pull an image (all layers verified by digest) into the local store.
    Pull { image: String },
    /// Build a root filesystem for a pulled image in DEST.
    Unpack {
        image: String,
        dest: PathBuf,
        /// Apply file ownership from the image (needs root).
        #[arg(long)]
        owners: bool,
    },
}

/// refs/<registry>/<repository>/<tag-or-digest> -> manifest digest
fn ref_path(root: &std::path::Path, r: &Reference) -> PathBuf {
    root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let store = Store::open(cli.root.join("content")).await?;
    match cli.cmd {
        Cmd::Pull { image } => {
            let r = Reference::parse(&image)?;
            let t0 = std::time::Instant::now();
            let img = Registry::new().pull(&r, &store).await.with_context(|| format!("pulling {r}"))?;
            let p = ref_path(&cli.root, &r);
            tokio::fs::create_dir_all(p.parent().unwrap()).await?;
            tokio::fs::write(&p, &img.manifest_digest).await?;
            let total: u64 = img.manifest.layers.iter().map(|l| l.size).sum();
            println!(
                "{r}\n  manifest {}\n  {} layers, {:.1} MB, {:.1}s",
                img.manifest_digest,
                img.manifest.layers.len(),
                total as f64 / 1e6,
                t0.elapsed().as_secs_f64()
            );
        }
        Cmd::Unpack { image, dest, owners } => {
            let r = Reference::parse(&image)?;
            let digest = tokio::fs::read_to_string(ref_path(&cli.root, &r))
                .await
                .with_context(|| format!("{r} is not pulled; run `og-hostd pull {image}` first"))?;
            let manifest: og_oci::registry::Manifest = serde_json::from_slice(&store.read(digest.trim()).await?)?;
            std::fs::create_dir_all(&dest)?;
            let opts = unpack::UnpackOptions { preserve_owner: owners, id_offset: 0 };
            let t0 = std::time::Instant::now();
            for layer in &manifest.layers {
                let path = store.blob_path(&layer.digest)?;
                let (dest, opts) = (dest.clone(), opts.clone());
                let stats = tokio::task::spawn_blocking(move || unpack::apply_layer(&dest, unpack::open_layer(&path)?, &opts)).await??;
                println!("  {} {:?}", &layer.digest[..19], stats);
            }
            println!("unpacked {r} into {} in {:.1}s", dest.display(), t0.elapsed().as_secs_f64());
        }
    }
    Ok(())
}
