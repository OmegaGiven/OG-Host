# OG-Host

Headless, web-managed server OS for the homelab: our own Rust container engine
(Docker/OCI compatible), virtual machines, backups, clusters across sites, and a resource
brain that shares memory and GPUs where they're needed. Managed from the browser (OmegaEnv,
rewritten in Rust, is its UI).

Status: in development, running on test VMs. See [docs/DESIGN.md](docs/DESIGN.md) and the
[release notes](docs/releases/).

## What's in it

- **Containers**: own engine, Docker Engine API + Compose, BuildKit builds, GPUs (CDI), rootless by
  default, live resource caps, a "desktop" profile that looks like a real PC.
- **VMs**: QEMU/KVM with UEFI, Secure Boot and TPM 2.0, browser console (own VNC client),
  snapshots, PCI/USB passthrough, built-in DHCP on OG-Host networks or macvtap onto the LAN, and
  a "looks like a real PC" mode.
- **Backups**: deduplicated, compressed, optionally encrypted (og-backup); datastores on disk,
  NFS or SMB; jobs with retention; restore in place or to a folder; verify and GC.
- **Clusters**: nodes at any site over mutual TLS, shared accounts, manage every node from any
  node, sync jobs that copy backups to another site.
- **Accounts and security**: realms (local, PAM), roles, path permissions, API tokens,
  two-factor, audit log, host firewall with commit-confirm.
- **Updates**: signed releases from mirrors, automatic rollback, rolling cluster updates.
- **AI**: a built-in assistant on any OpenAI-compatible model server; it uses OG-Host's tools as
  the signed-in user and asks before changing anything.

## Install

On a Debian or Ubuntu server, as root, from a mirror:

    curl -fsSL https://<mirror>/install.sh | sh
    og-hostd passwd admin

then open `https://<server>:8006`.

From source: `cargo build --release`, install `target/release/og-hostd` and `og-shim` to
`/usr/local/bin`, and the units in `packaging/`.

## Releases

    og-hostd release keygen DIR                      # once: the signing key (keep it offline)
    og-hostd release make --version X --channel stable --notes NOTES.md \
        --key DIR/og-host-release.key --mirror MIRROR_DIR \
        --bin target/release/og-hostd --bin target/release/og-shim --url https://<mirror>
    og-hostd mirror sync https://<mirror> /srv/og-host-mirror   # a verified copy elsewhere

Licence: MPL-2.0.
