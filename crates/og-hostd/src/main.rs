//! og-hostd — OG-Host node daemon. Current milestone: images + containers from the CLI.

mod access_api;
mod ai;
mod auth;
mod backup;
mod backup_api;
mod brain;
mod build;
mod cluster;
mod compose;
mod containers;
mod desktop;
mod dhcp;
mod dockerapi;
mod firewall;
mod ha;
mod hostnet;
mod installer;
mod idmap;
mod labnode;
mod migrate;
mod pools;
mod resources;
mod schedule;
mod serve;
mod stacks;
mod stats;
mod sysupdate;
mod storage;
mod tasks;
mod release;
mod tls;
mod update;
mod vm_api;
mod vma;
mod vmrun;
mod vmbrain;
mod vmfw;
mod vms;
mod ui;

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
enum LabCmd {
    /// Copy an OmegaEnv data directory (apps, groups, checks, hosts, watches, queue, ...)
    /// into <root>/lab. With --env, its .env becomes config.json.
    Import {
        from: PathBuf,
        #[arg(long)]
        env: Option<PathBuf>,
        /// Also take its secrets (agent/API tokens), so existing agents keep working.
        #[arg(long)]
        with_secrets: bool,
    },
    /// Print the settings an omega-agent needs to report to this server.
    Agent { host: String },
}

#[derive(Subcommand)]
enum Cmd {
    /// Pull an image (all layers verified by digest) into the local store.
    Pull { image: String },
    /// Import a Proxmox VE backup (vzdump .vma / .vma.zst / .vma.gz, or - for stdin): its disks
    /// become volumes vm-NAME-<device> in POOL, its config is kept in <root>/imported/NAME/.
    ImportVma {
        file: PathBuf,
        #[arg(long, default_value = "local-thin")]
        pool: String,
        #[arg(long)]
        name: String,
    },
    /// Load images from a `docker save` / OCI archive (FILE, or - for stdin), like docker load.
    Load {
        file: PathBuf,
        /// tag it as this too (when the archive carries no name)
        #[arg(long)]
        tag: Option<String>,
    },
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
        /// GPUs: all, or a GPU index/UUID; or any CDI name (vendor.com/class=name). Repeatable.
        #[arg(long = "gpus")]
        gpus: Vec<String>,
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
    /// Remove a stopped container (`-f`: kill it first).
    Rm {
        container: String,
        /// kill it first if it runs (like docker rm -f)
        #[arg(short, long)]
        force: bool,
    },
    /// Create and start every service of a compose project.
    Up {
        #[arg(short = 'f', long = "file", default_value = "docker-compose.yml")]
        file: PathBuf,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        /// Rebuild images of services with `build:` even if they exist.
        #[arg(long)]
        build: bool,
    },
    /// Build an image from a Dockerfile (via BuildKit) and tag it.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(short = 'f', long = "file")]
        file: Option<String>,
        #[arg(long = "build-arg")]
        build_arg: Vec<String>,
        #[arg(long)]
        target: Option<String>,
        context: PathBuf,
    },
    /// Stop and remove a compose project's containers (volumes are kept unless --volumes).
    Down {
        #[arg(short = 'f', long = "file", default_value = "docker-compose.yml")]
        file: PathBuf,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        #[arg(long)]
        volumes: bool,
    },
    /// Create a web UI account or change its password (read from the terminal, or one line
    /// on stdin). `--remove` deletes the account.
    Passwd {
        /// name@local (default realm) or name@pam
        user: String,
        #[arg(long)]
        remove: bool,
        /// turn off the account's two-factor sign-in (lost phone)
        #[arg(long)]
        reset_tfa: bool,
    },
    /// Install OG-Host on a disk (on the install ISO's console). `--auto FILE` (or `og.auto=FILE`
    /// on the kernel command line) answers from a file instead of asking.
    Installer {
        #[arg(long)]
        auto: Option<PathBuf>,
    },
    /// Make releases (for publishers): keygen DIR, make ...
    Release {
        #[command(subcommand)]
        cmd: ReleaseCmd,
    },
    /// Copy an update mirror (checking signatures and checksums).
    Mirror {
        #[command(subcommand)]
        cmd: MirrorCmd,
    },
    /// Check for / install an OG-Host update from the configured mirrors.
    Update {
        /// install it too (else just check)
        #[arg(long)]
        install: bool,
    },
    /// (systemd ExecStartPre) count a start of an update on trial; roll back if it keeps failing.
    #[command(hide = true)]
    UpdateGuard,
    /// (timer after boot) roll back a whole-system update nothing confirmed.
    #[command(hide = true)]
    UpdateBootCheck,
    /// Lab control plane maintenance.
    Lab {
        #[command(subcommand)]
        cmd: LabCmd,
    },
    /// Run the node daemon: API, restart policies, health monitoring.
    Serve {
        /// Also listen on TCP (e.g. 127.0.0.1:8411); requires the bearer token.
        #[arg(long)]
        listen: Option<String>,
        /// Scoped Docker API socket: name:label=value -> /run/og-host/docker-<name>.sock,
        /// limited to containers carrying that label. Repeatable.
        #[arg(long = "docker-scope")]
        docker_scope: Vec<String>,
        /// Label put on every container a scope creates: name:key=value, e.g.
        /// wolf:og.profile=desktop. Repeatable.
        #[arg(long = "scope-label")]
        scope_label: Vec<String>,
        /// Run the lab control plane too (OmegaEnv: fleet monitor, app tiles, metrics,
        /// logs, watches, install queue, OpenClaw); data in <root>/lab.
        #[arg(long)]
        lab: bool,
        /// Also serve HTTPS on this address (e.g. 0.0.0.0:8006).
        #[arg(long)]
        listen_tls: Option<String>,
        /// Port for other cluster nodes (mutual TLS; only open once in a cluster).
        #[arg(long, default_value_t = cluster::DEFAULT_PORT)]
        cluster_port: u16,
        /// PEM certificate for --listen-tls (default: self-signed, created in <root>/tls).
        #[arg(long)]
        tls_cert: Option<PathBuf>,
        #[arg(long)]
        tls_key: Option<PathBuf>,
    },
    /// Start a stopped container.
    Start { container: String },
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

pub(crate) async fn ensure_pulled(root: &std::path::Path, store: &Store, image: &str, quiet: bool) -> Result<String> {
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

/// Import a `docker save` (OCI layout) archive and tag what it names: (name, manifest digest).
pub(crate) fn load_archive(root: &std::path::Path, file: &std::path::Path, tag: Option<&str>) -> Result<Vec<(String, String)>> {
    use std::io::Read;
    // from stdin: to a file first (the archive is read twice)
    let tmp;
    let path = if file.as_os_str() == "-" {
        tmp = root.join("content/tmp").join(format!("load-{}.tar", std::process::id()));
        std::fs::create_dir_all(tmp.parent().unwrap())?;
        std::io::copy(&mut std::io::stdin().lock(), &mut std::fs::File::create(&tmp)?)?;
        tmp.clone()
    } else {
        file.to_path_buf()
    };
    let digest = og_oci::store::import_oci_tar(&root.join("content"), &path).context("importing the archive")?;
    // names: docker's manifest.json RepoTags, OCI index annotations, --tag
    let mut names: Vec<String> = vec![];
    let mut ar = tar::Archive::new(std::fs::File::open(&path)?);
    for e in ar.entries()? {
        let mut e = e?;
        let p = e.path()?.to_string_lossy().trim_start_matches("./").to_string();
        if p == "manifest.json" || p == "index.json" {
            let mut b = vec![];
            e.read_to_end(&mut b)?;
            let v: serde_json::Value = serde_json::from_slice(&b).unwrap_or_default();
            if p == "manifest.json" {
                for m in v.as_array().into_iter().flatten() {
                    names.extend(m["RepoTags"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)));
                }
            } else {
                for m in v["manifests"].as_array().into_iter().flatten() {
                    for k in ["io.containerd.image.name", "org.opencontainers.image.ref.name"] {
                        if let Some(n) = m["annotations"][k].as_str().filter(|n| n.contains(':') || n.contains('/')) {
                            names.push(n.to_string());
                        }
                    }
                }
            }
        }
    }
    if file.as_os_str() == "-" {
        let _ = std::fs::remove_file(&path);
    }
    names.extend(tag.map(String::from));
    names.sort();
    names.dedup();
    if names.is_empty() {
        anyhow::bail!("the archive names no image: give --tag NAME");
    }
    let mut out = vec![];
    for n in names {
        let r = Reference::parse(&n)?;
        let p = ref_path(root, &r);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, &digest)?;
        out.push((n, digest.clone()));
    }
    Ok(out)
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

#[derive(Subcommand)]
enum ReleaseCmd {
    /// Make a release signing key in DIR.
    Keygen { dir: PathBuf },
    /// Vouch for a signing key (a CI key) with a root release key, on a mirror directory; or
    /// --revoke it. Servers accept releases it signs until it expires.
    Delegate {
        /// the root release key file
        #[arg(long)]
        root_key: PathBuf,
        /// the key to vouch for: hex, or its .pub file
        #[arg(long)]
        key: String,
        #[arg(long, default_value = "ci")]
        name: String,
        #[arg(long, default_value_t = 400)]
        days: u64,
        #[arg(long, value_delimiter = ',', default_value = "stable,beta,dev")]
        channels: Vec<String>,
        #[arg(long)]
        mirror: PathBuf,
        #[arg(long)]
        revoke: bool,
    },
    /// Pack binaries into a signed release on a mirror directory.
    Make {
        #[arg(long)]
        version: String,
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long)]
        notes: Option<PathBuf>,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        mirror: PathBuf,
        /// binaries to ship (og-hostd required; og-shim)
        #[arg(long = "bin", required = true)]
        bins: Vec<PathBuf>,
        #[arg(long)]
        arch: Option<String>,
        /// the mirror's public URL (for install.sh)
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Subcommand)]
enum MirrorCmd {
    /// Copy URL's releases into DIR.
    Sync { url: String, dir: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let cli = Cli::parse();
    // a CLI command piped into `head` etc. exits quietly on SIGPIPE; the daemon must not (a
    // peer closing a socket would kill it), so it keeps Rust's default of ignoring it
    if !matches!(cli.cmd, Cmd::Serve { .. }) {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    // publishing tools need no server state (and no root)
    match &cli.cmd {
        Cmd::UpdateGuard => {
            update::guard(&cli.root);
            return Ok(());
        }
        Cmd::UpdateBootCheck => {
            sysupdate::boot_check();
            return Ok(());
        }
        Cmd::Installer { auto } => return installer::run(auto.clone()),
        Cmd::Release { cmd: ReleaseCmd::Keygen { dir } } => return release::keygen(dir),
        Cmd::Release { cmd: ReleaseCmd::Delegate { root_key, key, name, days, channels, mirror, revoke } } => return release::delegate(root_key, key, name, *days, channels, mirror, *revoke),
        Cmd::Release { cmd: ReleaseCmd::Make { version, channel, notes, key, mirror, bins, arch, url } } => {
            return release::make(&release::MakeOpts { version: version.clone(), channel: channel.clone(), notes: notes.clone(), key: key.clone(), mirror: mirror.clone(), bins: bins.clone(), arch: arch.clone(), url: url.clone() })
        }
        _ => {}
    }
    let store = Store::open(cli.root.join("content")).await?;
    let node = containers::Node {
        root: cli.root.clone(),
        store: store.clone(),
        runtime_root: PathBuf::from("/run/og-host/runtime"),
        net: og_net::Net::new(cli.root.join("networks")),
    };
    match cli.cmd {
        Cmd::ImportVma { file, pool, name } => {
            let (root, file) = (cli.root.clone(), file.clone());
            tokio::task::spawn_blocking(move || vma::cli(&root, &file, &pool, &name)).await??;
        }
        Cmd::Load { file, tag } => {
            for (name, digest) in load_archive(&cli.root, &file, tag.as_deref())? {
                println!("loaded {name} ({digest})");
            }
        }
        Cmd::Pull { image } => {
            let r = Reference::parse(&image)?;
            let _ = tokio::fs::remove_file(ref_path(&cli.root, &r)).await; // explicit pull refreshes the tag
            ensure_pulled(&cli.root, &store, &image, false).await?;
        }
        Cmd::Unpack { image, dest, owners } => {
            let digest = ensure_pulled(&cli.root, &store, &image, true).await?;
            let manifest: og_oci::registry::Manifest = serde_json::from_slice(&store.read(&digest).await?)?;
            std::fs::create_dir_all(&dest)?;
            let opts = unpack::UnpackOptions { preserve_owner: owners, id_offset: 0, overlay: false };
            let t0 = std::time::Instant::now();
            for layer in &manifest.layers {
                let path = store.blob_path(&layer.digest)?;
                let (dest, opts) = (dest.clone(), opts.clone());
                let stats = tokio::task::spawn_blocking(move || unpack::apply_layer(&dest, unpack::open_layer(&path)?, &opts)).await??;
                println!("  {} {:?}", &layer.digest[..19], stats);
            }
            println!("unpacked {image} into {} in {:.1}s", dest.display(), t0.elapsed().as_secs_f64());
        }
        Cmd::Run { image, name, env, memory, user, entrypoint, network, publish, volume, gpus, command } => {
            let cdi: Vec<String> = gpus.iter().map(|g| if g.contains('=') { g.clone() } else { format!("nvidia.com/gpu={g}") }).collect();
            let binds = volume.iter().map(|v| containers::parse_bind(v)).collect::<Result<Vec<_>>>()?;
            let digest = ensure_pulled(&cli.root, &store, &image, true).await?;
            let memory = memory.map(|m| containers::parse_size(&m)).transpose()?;
            let ports = publish.iter().map(|p| og_net::PortMap::parse(p)).collect::<Result<Vec<_>, _>>()?;
            let choice = containers::NetChoice::parse(&network);
            let t0 = std::time::Instant::now();
            let meta = tokio::task::spawn_blocking(move || {
                node.run(&image, &digest, name.as_deref(), choice, ports, containers::Extra::default(), |o| {
                    o.env = env;
                    o.mounts = binds;
                    o.cdi_devices = cdi;
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
        Cmd::Up { file, project, build } => {
            let env: og_compose::interp::Vars = std::env::vars().collect();
            let p = og_compose::load(&file, project.as_deref(), &env)?;
            compose::up(&node, &cli.root, &store, &p, build).await?;
        }
        Cmd::Build { tag, file, build_arg, target, context } => {
            let args = build_arg.iter().filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
            let spec = build::BuildSpec { context, dockerfile: file, tag, args, target };
            let root = cli.root.clone();
            tokio::task::spawn_blocking(move || build::build(&root, &spec)).await??;
        }
        Cmd::Down { file, project, volumes } => {
            let env: og_compose::interp::Vars = std::env::vars().collect();
            let p = og_compose::load(&file, project.as_deref(), &env)?;
            let root = cli.root.clone();
            tokio::task::spawn_blocking(move || compose::down(&node, &root, &p, volumes)).await??;
        }
        Cmd::Passwd { user, remove, reset_tfa } => {
            let id = if user.contains('@') { user.clone() } else { format!("{user}@local") };
            og_access::parse_userid(&id)?;
            let access = og_access::Access::open(&cli.root);
            if remove {
                access.delete_user(&id)?;
                access.audit("system", "cli", "remove user", &id, true);
                println!("removed {id}");
            } else if reset_tfa {
                access.tfa_disable(&id)?;
                access.audit("system", "cli", "tfa reset", &id, true);
                println!("two-factor turned off for {id}");
            } else {
                let first = !access.has_users();
                if id.ends_with("@pam") {
                    access.upsert_user(&id, &serde_json::json!({}), None)?;
                } else {
                    let pw = read_password("New password: ")?;
                    if std::io::IsTerminal::is_terminal(&std::io::stdin()) && read_password("Again: ")? != pw {
                        anyhow::bail!("the passwords don't match");
                    }
                    access.upsert_user(&id, &serde_json::json!({}), Some(&pw))?;
                }
                if first {
                    access.set_acl(og_access::AclEntry { path: "/".into(), ugid: id.clone(), role: "Administrator".into(), propagate: true }, false)?;
                    println!("{id} is the first account: Administrator on /");
                }
                access.audit("system", "cli", "passwd", &id, true);
                println!("account {id} saved; sign in on the web UI");
            }
        }
        Cmd::Release { .. } | Cmd::UpdateGuard | Cmd::UpdateBootCheck | Cmd::Installer { .. } => unreachable!("handled before the store opens"),
        Cmd::Mirror { cmd } => match cmd {
            MirrorCmd::Sync { url, dir } => release::sync(&cli.root, &url, &dir).await?,
        },
        Cmd::Update { install } => {
            let a = update::check(&cli.root).await?;
            println!("installed {}, mirror {} has {} on {} (signed by {})", update::VERSION, a.mirror, a.manifest.version, a.manifest.channel, a.key);
            if install && a.newer {
                println!("use the web UI or POST /v1/update/install on a running server (it restarts the daemon)");
            }
        }
        Cmd::Lab { cmd } => match cmd {
            LabCmd::Import { from, env, with_secrets } => {
                let done = og_lab::import_omegaenv(&from, env.as_deref(), &cli.root.join("lab"), with_secrets)?;
                println!("imported into {}: {}", cli.root.join("lab").display(), done.join(", "));
                println!("metrics.db and the log directory are used where config.json points (copy them into {} to move them)", cli.root.join("lab").display());
            }
            LabCmd::Agent { host } => {
                std::fs::create_dir_all(cli.root.join("lab"))?;
                let lab = og_lab::Lab::open(&cli.root.join("lab")).await?;
                println!("# omega-agent settings for {host} (put in ~/.config/omegaenv/agent.env or the unit's EnvironmentFile)");
                println!("OMEGA_URL=https://<this-server>:8006");
                println!("OMEGA_TOKEN={}", lab.secrets.agent_token);
                println!("OMEGA_HOST={host}");
            }
        },
        Cmd::Serve { listen, docker_scope, scope_label, listen_tls, tls_cert, tls_key, lab, cluster_port } => {
            // an update on trial: count this start, roll back if it keeps failing
            update::startup_check(&cli.root);
            sysupdate::on_start(&cli.root);
            let tls = match listen_tls {
                Some(addr) => Some((addr, tls::cert_files(&cli.root, tls_cert, tls_key)?)),
                None => None,
            };
            serve::serve(node, cli.root.clone(), listen, docker_scope, scope_label, tls, lab, cluster_port).await?;
        }
        Cmd::Start { container } => {
            let m = node.find(&container)?;
            tokio::task::block_in_place(|| node.start(&m.id))?;
            println!("{} started", m.name);
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
        Cmd::Rm { container, force } => {
            let m = node.find(&container)?;
            if force {
                node.stop(&m.id, 0)?;
            }
            node.remove(&m.id)?;
            println!("removed {}", m.name);
        }
    }
    Ok(())
}

/// A password from the terminal without echo, or one line from stdin when not a terminal.
fn read_password(prompt: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_string());
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::tcgetattr(0, &mut t) };
    let old = t;
    t.c_lflag &= !libc::ECHO;
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    let mut line = String::new();
    let r = stdin.lock().read_line(&mut line);
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &old) };
    eprintln!();
    r?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
