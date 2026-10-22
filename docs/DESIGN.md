# OG-Host — design

OG-Host is a headless, web-managed server operating system for the homelab. It replaces
Proxmox VE + Docker-inside-VMs + OmegaEnv with one system we own end to end: a minimal
image-based host, our own container engine (Docker/OCI compatible), occasional VMs, a
resource "brain" that moves memory and GPUs to where they are needed, and a Proxmox-style
web UI (OmegaEnv, rewritten in Rust).

Status: design, 2026-10-21. Nothing built yet.

## 1. Why

Measured on the current lab (OmegaEnv, Oct 2026):

| | Allocated | Actually used |
|---|---|---|
| go (VM) | 8 GB | ~10 GB needed, 5+ GB in swap |
| station (VM, RTX 5080) | 39 GB | ~18 GB |
| vm-3070 (VM, RTX 3070, RAM pinned by passthrough) | 12 GB | ~1 GB most of the time |
| vm-2060 (VM, RTX 2060, pinned) | 6 GB | ~2.6 GB |
| pmx2-ci-runner | 8 GB | ~0.6 GB |
| download | 6 GB | ~1.5 GB |
| pmx2-apps | 4 GB | ~1.6 GB |
| **Total** | **~86 GB of ~94 GB** | **~35 GB** |

The waste comes from per-VM memory reservations, worst for GPU VMs (VFIO pins all guest
RAM). Almost every workload is already a Docker container inside a VM, so the VM layer is
mostly overhead. Existing open-source hosts don't fill three gaps that matter here:

1. Nothing moves memory between workloads based on pressure.
2. Nothing manages Compose stacks, system containers and VMs together in one UI.
3. Nothing schedules consumer GPUs (VRAM, the 8-session NVENC limit, gaming first) across cards.

## 2. Goals and non-goals

**Goals**
- Fast, efficient, secure. Rust everywhere on the host side.
- Headless: no desktop or GPU console needed to run it. Managed through a web UI + API.
- Containers first; VMs only when needed (Windows, odd jobs).
- Runs existing Docker images and the compose files we already have, unchanged.
- Speaks enough of the Docker Engine API that tools which drive Docker keep working
  (Wolf, Pterodactyl Wings, `docker` and `docker compose` CLIs).
- Memory and GPUs shared and prioritised, idle apps stopped until used.
- Image-based, atomic A/B updates with one-click rollback.
- Fully open source, no subscriptions or nags.

**Non-goals (for now)**
- Writing a kernel or hypervisor (we use Linux, KVM, and an existing VMM).
- Automatic failover between hosts (needs a third quorum vote; revisit later).
- Running Kubernetes.
- A desktop or local graphics stack.

## 3. Architecture

```
            browser ── HTTPS (tailnet) ──┐
                                         ▼
 ┌──────────────────────────── og-host node ─────────────────────────────┐
 │  og-hostd (Rust, one binary, systemd)                                  │
 │   ├─ api        REST + WebSocket (UI, CLI, consoles, logs, metrics)    │
 │   ├─ docker-api Docker Engine API subset on /run/og-host/docker.sock   │
 │   ├─ engine     images, containers, compose stacks, builds             │
 │   ├─ net        bridges, nftables, WireGuard, per-stack networks       │
 │   ├─ store      ZFS datasets for volumes/images, snapshots, send/recv  │
 │   ├─ gpu        device inventory, CDI injection, VRAM-aware scheduler  │
 │   ├─ vm         Cloud Hypervisor / QEMU, VFIO, browser console         │
 │   ├─ brain      memory policy (cgroup v2 + PSI), scale-to-zero         │
 │   └─ telemetry  metrics, logs, alerts (OmegaEnv's monitor, in Rust)    │
 │  low-level runtime: youki (Rust OCI runtime) as a library              │
 │  Linux kernel · KVM · ZFS · NVIDIA driver + container toolkit          │
 │  base image: minimal Debian 13 packages, read-only root, A/B slots     │
 └────────────────────────────────────────────────────────────────────────┘
        ▲ node-to-node mTLS (cluster view, moves, backups) ▼
                        second og-host node
```

One daemon per node. Any node's UI shows the whole cluster (two nodes today).

## 4. Components

### 4.1 Base image
- Built with mkosi from Debian 13 packages: kernel, ZFS, NVIDIA driver (built per kernel
  in CI, signed), nftables, WireGuard, Tailscale, og-hostd. No package manager on the
  running host.
- Root filesystem read-only (dm-verity), two system slots (A/B), config and state on a
  separate ZFS pool. Update = download image, write the idle slot, reboot; failed boot
  falls back automatically. Rollback from the UI.
- First-boot: web setup page on the LAN to pick disks, network, admin account, and join
  the tailnet.
- Reuse the OG-OS CI pattern: build in CI, boot-test in VMs on every commit.

### 4.2 Container engine
- **Images**: OCI distribution client (Docker Hub, GHCR, private registries), layer
  storage on ZFS (one dataset per layer, clones for container root filesystems; overlayfs
  fallback). Content-addressed, verified digests, garbage collection.
- **Builds**: 15+ of our services build from a Dockerfile. MVP shells out to BuildKit
  (buildkitd in a container, rootless) behind our API; a native Dockerfile builder is a
  later milestone.
- **Containers**: namespaces, cgroup v2, mounts, users, capabilities, seccomp, devices,
  restart policies, health checks, log capture, all via youki's libcontainer.
- **Compose**: native parser for the compose spec. MVP scope is what our stacks use
  (survey of 70+ services, Oct 2026): `image`, `build`, `ports`, `volumes` (bind + named),
  `environment`, `env_file`, `restart`, `depends_on`, `healthcheck`, `command`,
  `entrypoint`, `user`, `working_dir`, `hostname`, `extra_hosts`, `dns`, `devices`,
  `cap_add` (NET_ADMIN), `shm_size`, `mem_limit`, `security_opt`, `networks`,
  `network_mode: host` and `network_mode: service:<name>` (VPN sidecar pattern),
  `deploy.resources` with NVIDIA GPU reservations, top-level `volumes`/`networks`/`name`.
- **Docker Engine API**: a socket that speaks the subset Wolf, Pterodactyl Wings,
  OmegaEnv's old agent and the docker/compose CLIs call (containers, images, networks,
  volumes, exec, logs, events, attach). Each client gets a scoped token: Wolf may only
  manage its own session containers, Wings only game servers. This is a big security
  improvement over today's all-powerful docker.sock mounts.

### 4.3 Networking
- One Linux bridge per stack by default, nftables for port publishing and isolation,
  IPv4/IPv6.
- `service:` network sharing and VPN-only egress (download stack through WireGuard;
  leak check built in, as OmegaEnv does today).
- Tailscale on the host; optional per-app tailnet names via `tailscale serve`.
- Built-in reverse proxy with automatic HTTPS (replaces Caddy for tailnet sites).

### 4.4 Storage
- ZFS pool(s) per node. Every volume is a dataset: quotas, compression, snapshots.
- Scheduled snapshots, `zfs send` to the NAS (antietam-vault) for off-site backup;
  database-aware hooks (pg_dump before snapshot) for Postgres containers.
- Media pool (mergerfs today) mounted read-only or read-write per stack.

### 4.5 GPUs
- Inventory per node (model, VRAM, NVENC/NVDEC, temperature, power), CDI device injection.
- Scheduler: workloads declare a GPU class (gaming, interactive, batch) and VRAM need. The
  scheduler places batch jobs on whichever card has room, keeps NVENC sessions under the
  GeForce limit of 8 per system, and lets gaming take priority (pause or move batch work,
  the way og-gpu-mode does by hand today).
- Power cap and persistence mode per card as policy (gpu-guard's job today).

### 4.6 VMs
- Cloud Hypervisor (Rust) for Linux guests, QEMU for Windows and passthrough edge cases.
- virtio-mem / free page reporting so idle VMs return memory where the guest supports it.
- Browser console (noVNC/serial over WebSocket), snapshots on ZFS zvols.
- The OG-OS CI runner becomes a container with `/dev/kvm` instead of a VM.
- VMs stay first-class for what containers can't do: Windows (builds, Windows-only apps and
  games), other kernels, untrusted code that needs a hardware boundary.
- "Looks like a real PC" profile for VMs too (Windows games/anti-cheat check for VMs): hide
  the KVM signature (`kvm=off`, custom `hv_vendor_id`, no hypervisor CPUID bit where the
  guest allows), host SMBIOS/DMI passed through or a realistic custom one, real-looking disk
  model/serial and NIC vendor (OUI) instead of QEMU/virtio names. Trade-off: hiding Hyper-V
  enlightenments costs Windows guest performance, so it is opt-in per VM.

### 4.7 The brain (resource policy)
- Every workload gets a priority class: **critical** (Jellyfin, Immich, databases,
  OmegaEnv), **normal**, **batch** (AI jobs, QA browsers, encodes), **idle-stop**.
- Enforced with cgroup v2 `memory.low` (protect critical), `memory.high` (throttle batch
  under pressure), `memory.max` (hard caps), plus CPU weights.
- Watches PSI (pressure stall information) per node and per workload. Under pressure it
  squeezes batch first, then stops idle-stop apps, then alerts. Never OOM-kills critical.
- Scale-to-zero: idle-stop apps are stopped after N minutes without traffic; the built-in
  proxy holds the first request, starts the app, then forwards it.
- zram for compressed swap; KSM for VMs.
- Implemented (og-hostd `brain.rs`, every 5 s; status at `GET /v1/brain`). Labels:
  `og.priority` (critical | normal | batch | idle-stop), `og.memory-low` (extra floor for
  critical), `og.idle-after` (default 30m), `og.gpu-class` (gaming | interactive | batch),
  `og.gpu-preempt` (stop | pause), `og.vram` (placement / admission).
  Pressure is measured on what the *protected* workloads feel (PSI of host services and
  non-batch containers, plus MemAvailable), so a throttled batch job doesn't count its own
  stall. Order: squeeze batch `memory.high` -> sleep idle-stop (severe) -> evict the biggest
  batch job once squeezing stops freeing memory (without swap that is immediate) -> alert.
  Tested on a 1.8 GB VM: a 1.1 GB batch job is throttled then evicted before the kernel OOM
  killer runs; critical/normal containers untouched. Containers report `OOMKilled`.

### 4.9 "Looks like a real PC" (desktop profile)
Applies to containers and VMs alike (VM side in §4.6). Game containers (Wolf sessions) should not look like containers or VMs to games and
anti-cheat. Containers start ahead here: the game sees the real CPU (no hypervisor CPUID
bit), real GPU and real devices. `og.profile=desktop` removes the remaining tells:
- hostname like a desktop (`og.hostname`, default `DESKTOP-XXXXXXX`), a stable
  `/etc/machine-id`, a NIC MAC with a real vendor prefix instead of a random 02:xx one;
- DMI/SMBIOS (`/sys/class/dmi/id`): the host's own when it is bare metal; when the host is
  itself a VM, generic desktop-board values (or `og.dmi.*` labels) over the QEMU ones;
- no container markers (`/.dockerenv`, `/run/.containerenv`, `container=`); cgroup
  namespace so `/proc/self/cgroup` is `0::/`;
- nested namespaces allowed (seccomp) and `/proc` left fully visible (no masked paths) so
  Steam's pressure-vessel / bubblewrap works inside. Safe enough because the container root is
  an unprivileged host user (user namespace); documented trade-off vs the default profile.
- Wolf creates session containers through its scoped socket; the scope adds the profile and
  `og.gpu-class=gaming` to every container it creates (`--scope-label`).
Known remaining tells: the host kernel version string, `/proc/self/mountinfo` paths. A
resource view (LXCFS-style /proc/meminfo, cpuinfo) is planned for capped non-desktop
containers; it can't be used with the desktop profile because overmounting /proc breaks
nested /proc mounts (bubblewrap).

### 4.10 Resource caps
Per container, set at create time and changeable live (`docker update`, `POST
/v1/containers/{id}/resources`, the UI): memory max/high/low, CPU quota (`cpus`), CPU
pinning (`cpuset`), CPU weight, pids, disk I/O bandwidth and IOPS per device. Presets:
`minimal` (a quarter core, lowest weight, memory held at its current size) and `restore`
(back to what it was created with). Changes are written to the cgroup at once and kept in
the bundle so they survive restarts.

### 4.8 Web UI and API
- OmegaEnv becomes the UI: Home (apps), Monitor, Explore (metrics), Logs, Watch, Apps
  (stacks + catalog), plus new Hosts / Containers / VMs / Storage / GPUs / Updates pages.
- Backend rewritten in Rust (axum + tokio); metrics and logs keep SQLite + files.
  Frontend stays plain HTML/JS served by the daemon (no build step, fast).
- Approval queue for changes proposed by the AI assistant, as today.
- CLI `og` talks to the same API.
- Implemented (first cut): og-hostd serves the UI itself on its `--listen` address — no CDN,
  fonts or third-party JS; assets compiled into the binary, gzipped once, ETag-cached
  (about 35 KB over the wire). Pages: Overview (live CPU/memory/network/disk, history
  15 m/1 h/24 h, GPUs with VRAM/NVENC, busiest containers, brain activity), Containers
  (live stats, start/stop/restart/kill/remove, minimize/restore, detail drawer with live
  logs, resource sliders, inspect; new-container form with priority, GPU and desktop
  profile), Stacks (managed in `<root>/stacks/<name>/compose.yaml` or discovered by label;
  editor with diff, rollback on invalid compose; up/down/restart/pull), Images, Storage
  (disks, volumes, networks), Logs, Ctrl-K palette; phone layout with a bottom tab bar.
- Live data: one sampler every 2 s → one serialized message → Server-Sent Events to every
  browser (`/v1/events`); history in memory (1 h at 2 s, 24 h at 1 min).
- Sign-in: `og-hostd passwd <user>` (argon2id). HttpOnly SameSite=Strict session cookie;
  sessions stored hashed and survive restarts; a password change or account removal ends
  existing sessions; changes need `X-OG-UI: 1` and a same-origin Origin; 5 failed sign-ins
  lock a client out for 10 minutes. Strict CSP. TLS: put `tailscale serve` (or another
  proxy) in front for now; built-in TLS is planned.
- The Docker Engine API is also mounted at `/v1/docker/` for signed-in clients.
- Not ported from OmegaEnv yet: multi-host Monitor (omega-agent), Watches, OpenClaw drawer
  and approval queue, the cross-host log store, app tiles/catalog.

## 5. Security model
- **Two privilege levels inside og-hostd**: the API and UI layer runs unprivileged and
  talks to a small privileged core over a local socket with a narrow, typed command set.
  The privileged core is the only code that touches namespaces, mounts, nftables or ZFS.
- **Containers rootless-by-default** (user namespaces); root-in-container maps to an
  unprivileged host user unless the stack opts out and an admin approves.
- Default seccomp profile, no new privileges, dropped capabilities; `privileged` and
  `docker.sock` access need explicit approval and are scoped (see 4.2).
- Optional VM-isolated containers (Cloud Hypervisor microVMs, like Kata) for risky
  workloads: public game servers, the AI agent with a shell.
- Auth: local accounts with passkeys / TOTP, sessions over HTTPS on the tailnet only by
  default, per-user roles, audit log of every change.
- Secrets stored encrypted on the state pool, injected at start, never shown in the UI.
- Signed images and updates; verified boot (dm-verity, Secure Boot where available).
- Supply chain: pinned dependencies, `cargo audit`/`cargo deny` in CI, SBOM per release.

## 6. Testing (the real cost)
- Unit and property tests for parsers (compose, Dockerfile, OCI manifests).
- Integration lab in CI: boot the image in VMs, deploy every real compose stack from the
  lab (sanitised), check health, ports, volumes, GPU (on the self-hosted runner).
- Security tests: container-escape suite, cgroup limit enforcement, nftables isolation,
  scoped docker-API tokens.
- Chaos tests: power loss mid-write, full disk, OOM pressure, failed update rollback.
- Docker API conformance: run Wolf, Wings and the docker/compose CLIs against og-hostd.

## 7. Roadmap
1. **Engine MVP** — og-hostd on a normal Debian 13 VM: pull images, run containers,
   compose stacks from our survey list, logs, restart policies, minimal REST API.
2. **Docker API** — enough for docker/compose CLIs, then Wolf and Wings.
3. **UI** — OmegaEnv backend ported to Rust inside og-hostd; containers/stacks pages.
4. **Brain** — priority classes, memory.low/high, PSI, scale-to-zero, GPU scheduler.
5. **Storage + networking** — ZFS volumes and snapshots, backups to the NAS, VPN networks.
6. **Base image** — mkosi build, A/B updates, first-boot setup, CI boot tests.
7. **VMs** — Cloud Hypervisor + QEMU, console, Windows (builds, Windows-only software),
   "looks like a real PC" option for game VMs.
8. **Own game streaming** — look into rewriting Wolf (Moonlight-compatible streaming,
   per-session game containers) for OG-Host: native use of the desktop profile, the GPU
   scheduler and the brain instead of driving a Docker socket. After the steps above.
9. **Pilot** — proxmoxtwo (after its RAM is tested/fenced), then the main host, one stack
   at a time.

## 8. Decisions log
- 2026-10-21: code name **OG-Host**; Rust; OmegaEnv becomes the UI; Debian 13 package
  base (not Arch); youki as the low-level OCI runtime; Docker Engine API compatibility is
  MVP (Wolf and Pterodactyl Wings drive Docker directly).

## 9. Open questions
- ZFS layer storage vs overlayfs as the default image store (performance, dedup).
- Cloud Hypervisor vs QEMU only (Windows + GPU passthrough maturity).
- How far to take the native Dockerfile builder vs keeping BuildKit.
- Licence (Apache-2.0 or MPL-2.0, as OG Paper uses).

## 10. Benchmark plan: OG-Host vs Proxmox VE

Two VMs side by side on the same physical host, identical resources (e.g. 4 vCPU, 8 GB RAM,
64 GB virtio disk on the same storage, same bridge), one running Proxmox VE 9 (workloads in
LXC and in Docker inside an LXC/VM, as people actually run them), one running OG-Host. The
same workloads on both, each test run three times, medians reported.

| Area | Measurement |
|---|---|
| Boot | power-on to "all workloads healthy" |
| Idle overhead | RAM and CPU used by the platform itself with zero workloads, and with the full set idle |
| Start latency | time to start one container / a 10-service compose stack, cold and warm cache |
| Density | how many identical small services (nginx, redis, a Node app) start before the first OOM or health failure |
| Memory pressure | fill memory with a batch job: does a protected service (Postgres) keep its latency? who gets killed? |
| Reclaim | after a burst, how fast and how much memory returns to the pool |
| Network | iperf3 container↔container, host→published port, published port from the LAN; latency p50/p99 |
| Disk | fio random/sequential inside a container volume; image pull + unpack time |
| GPU (on hardware) | two GPU workloads sharing one card vs passthrough; time-to-first-frame for Wolf |
| Updates | time to update the host and roll back a bad update; downtime per service |
| Security | open ports on the host, processes running as host root, container breakout test suite, Lynis/CIS-style host scan, CVE scan (grype/trivy) of the host image, default capability sets, user-namespace use |
| Footprint | install size, number of packages, lines of code on the privileged path |

Results go in `bench/results/<date>/` with the scripts that produced them, so every number can
be reproduced.
