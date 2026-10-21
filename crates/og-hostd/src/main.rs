//! og-hostd — OG-Host node daemon. Current milestone: images + containers from the CLI.

mod containers;
mod idmap;

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
    /// Create and start a container (pulls the image if needed).
    Run {
        image: String,
        #[arg(long)]
        name: Option<String>,
        /// KEY=value, repeatable.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Hard memory cap, e.g. 256m.
        #[arg(long)]
        memory: Option<String>,
        /// Run as this user (uid, uid:gid, name).
        #[arg(long)]
        user: Option<String>,
        /// Replace the image's entrypoint.
        #[arg(long)]
        entrypoint: Option<String>,
        /// default | <network> | none | host | container:<name>
        #[arg(long, default_value = "default")]
        network: String,
        /// Publish a port: [host_ip:]host_port:container_port[/udp], repeatable.
        #[arg(short = 'p', long = "publish")]
        publish: Vec<String>,
        /// Bind mount: /host/path:/container/path[:ro], repeatable.
        #[arg(short = 'v', long = "volume")]
        volume: Vec<String>,
        /// Command (replaces the image's CMD).
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// List containers.
    Ps,
    /// Print a container's output.
    Logs { container: String },
    /// Stop a container (SIGTERM, then SIGKILL after the timeout).
    Stop {
        container: String,
        #[arg(long, default_value_t = 10)]
        timeout: u64,
    },
    /// Remove a stopped container.
    Rm { container: String },
    /// Check a compose file: services in start order, what's used, and unsupported keys.
    /// Prints names and counts only, never values (compose files hold secrets).
    Config {
        #[arg(short = 'f', long = "file", default_value = "docker-compose.yml")]
        file: PathBuf,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
    },
}

/// refs/<registry>/<repository>/<tag-or-digest> -> manifest digest
fn ref_path(root: &std::path::Path, r: &Reference) -> PathBuf {
    root.join("refs").join(&r.registry).join(&r.repository).join(r.manifest_ref().replace(':', "_"))
}

async fn ensure_pulled(root: &std::path::Path, store: &Store, image: &str, quiet: bool) -> Result<String> {
    let r = Reference::parse(image)?;
    let p = ref_path(root, &r);
    if let Ok(d) = tokio::fs::read_to_string(&p).await {
        return Ok(d.trim().to_string());
    }
    let t0 = std::time::Instant::now();
    let img = Registry::new().pull(&r, store).await.with_context(|| format!("pulling {r}"))?;
    tokio::fs::create_dir_all(p.parent().unwrap()).await?;
    tokio::fs::write(&p, &img.manifest_digest).await?;
    if !quiet {
        let total: u64 = img.manifest.layers.iter().map(|l| l.size).sum();
        println!("{r}\n  manifest {}\n  {} layers, {:.1} MB, {:.1}s", img.manifest_digest, img.manifest.layers.len(), total as f64 / 1e6, t0.elapsed().as_secs_f64());
    }
    Ok(img.manifest_digest)
}

fn ago(t: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = now.saturating_sub(t);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let cli = Cli::parse();
    let store = Store::open(cli.root.join("content")).await?;
    let node = containers::Node {
        root: cli.root.clone(),
        store: store.clone(),
        runtime_root: PathBuf::from("/run/og-host/runtime"),
        net: og_net::Net::new(cli.root.join("networks")),
    };
    match cli.cmd {
        Cmd::Pull { image } => {
            let r = Reference::parse(&image)?;
            let _ = tokio::fs::remove_file(ref_path(&cli.root, &r)).await; // explicit pull refreshes the tag
            ensure_pulled(&cli.root, &store, &image, false).await?;
        }
        Cmd::Unpack { image, dest, owners } => {
            let digest = ensure_pulled(&cli.root, &store, &image, true).await?;
            let manifest: og_oci::registry::Manifest = serde_json::from_slice(&store.read(&digest).await?)?;
            std::fs::create_dir_all(&dest)?;
            let opts = unpack::UnpackOptions { preserve_owner: owners, id_offset: 0 };
            let t0 = std::time::Instant::now();
            for layer in &manifest.layers {
                let path = store.blob_path(&layer.digest)?;
                let (dest, opts) = (dest.clone(), opts.clone());
                let stats = tokio::task::spawn_blocking(move || unpack::apply_layer(&dest, unpack::open_layer(&path)?, &opts)).await??;
                println!("  {} {:?}", &layer.digest[..19], stats);
            }
            println!("unpacked {image} into {} in {:.1}s", dest.display(), t0.elapsed().as_secs_f64());
        }
        Cmd::Run { image, name, env, memory, user, entrypoint, network, publish, volume, command } => {
            let binds = volume.iter().map(|v| containers::parse_bind(v)).collect::<Result<Vec<_>>>()?;
            let digest = ensure_pulled(&cli.root, &store, &image, true).await?;
            let memory = memory.map(|m| containers::parse_size(&m)).transpose()?;
            let ports = publish.iter().map(|p| og_net::PortMap::parse(p)).collect::<Result<Vec<_>, _>>()?;
            let choice = containers::NetChoice::parse(&network);
            let t0 = std::time::Instant::now();
            let meta = tokio::task::spawn_blocking(move || {
                node.run(&image, &digest, name.as_deref(), choice, ports, |o| {
                    o.env = env;
                    o.mounts = binds;
                    o.memory_max = memory;
                    o.user = user;
                    if let Some(e) = entrypoint {
                        o.entrypoint = Some(vec![e]);
                    }
                    if !command.is_empty() {
                        o.command = Some(command);
                    }
                })
            })
            .await??;
            let ip = meta.net.as_ref().and_then(|a| a.ip).map(|i| format!(" ip {i}")).unwrap_or_default();
            println!("{} {} started in {:.2}s{ip}", meta.name, &meta.id[..12], t0.elapsed().as_secs_f64());
        }
        Cmd::Ps => {
            println!("{:<14} {:<16} {:<26} {:<10} {:<15} {:<22} {}", "ID", "NAME", "IMAGE", "STATE", "IP", "PORTS", "CREATED");
            for (m, s) in node.list()? {
                let ip = m.net.as_ref().and_then(|a| a.ip).map(|i| i.to_string()).unwrap_or_else(|| m.network_mode.clone());
                let ports = m.net.as_ref().map(|a| a.ports.iter().map(|p| format!("{}->{}", p.host_port, p.container_port)).collect::<Vec<_>>().join(",")).unwrap_or_default();
                let state = match (s.state.as_str(), s.exit_code) {
                    ("exited", Some(c)) => format!("exited({c})"),
                    (st, _) => st.to_string(),
                };
                println!("{:<14} {:<16} {:<26} {:<10} {:<15} {:<22} {}", &m.id[..12], m.name, m.image, state, ip, ports, ago(m.created_at));
            }
        }
        Cmd::Logs { container } => {
            let m = node.find(&container)?;
            print!("{}", String::from_utf8_lossy(&std::fs::read(node.log_path(&m.id)).unwrap_or_default()));
        }
        Cmd::Stop { container, timeout } => {
            let m = node.find(&container)?;
            let st = tokio::task::spawn_blocking(move || node.stop(&m.id, timeout)).await??;
            println!("{} {}", container, st.state);
        }
        Cmd::Config { file, project } => {
            let env: og_compose::interp::Vars = std::env::vars().collect();
            let p = og_compose::load(&file, project.as_deref(), &env)?;
            println!("project {} ({} services)", p.name, p.services.len());
            for s in &p.services {
                let mut feats = Vec::new();
                if s.build.is_some() { feats.push("build".to_string()); }
                if !s.ports.is_empty() { feats.push(format!("{} ports", s.ports.len())); }
                let (binds, vols) = s.mounts.iter().fold((0, 0), |(b, v), m| match m.kind {
                    og_compose::MountKind::Bind => (b + 1, v),
                    _ => (b, v + 1),
                });
                if binds > 0 { feats.push(format!("{binds} binds")); }
                if vols > 0 { feats.push(format!("{vols} volumes")); }
                if let Some(n) = &s.network_mode { feats.push(format!("network_mode {n}")); }
                if s.gpus { feats.push("gpu".into()); }
                if s.healthcheck.is_some() { feats.push("healthcheck".into()); }
                if !s.depends_on.is_empty() { feats.push(format!("after {}", s.depends_on.iter().map(|d| d.0.as_str()).collect::<Vec<_>>().join(","))); }
                if !s.cap_add.is_empty() { feats.push(format!("cap_add {}", s.cap_add.join(","))); }
                if !s.devices.is_empty() { feats.push(format!("{} devices", s.devices.len())); }
                if s.privileged { feats.push("PRIVILEGED".into()); }
                if s.mounts.iter().any(|m| m.source.ends_with("docker.sock")) { feats.push("DOCKER-SOCKET".into()); }
                println!("  {:<22} {:<46} {}", s.name, s.image.as_deref().unwrap_or("(build)"), feats.join(" · "));
            }
            for w in &p.warnings {
                println!("  warning: {w}");
            }
        }
        Cmd::Rm { container } => {
            let m = node.find(&container)?;
            node.remove(&m.id)?;
            println!("removed {}", m.name);
        }
    }
    Ok(())
}
