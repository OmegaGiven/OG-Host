# OG-Host vs Proxmox VE vs Incus vs self-built Debian — 2026-10-22

All numbers in [table.md](table.md) are generated from the raw `RESULT` lines in this folder
by `bench/report.py`; nothing is typed in by hand. Best value per row in bold.

## Setup (identical for all four)

- One QEMU/KVM VM per platform on station-sway: 4 vCPU pinned to host threads 8–11, 8 GB RAM,
  64 GB virtio disk (qcow2, cache=none, io_uring), UEFI, user-mode network plus a private
  cluster LAN. **Only the platform under test runs** (fresh boot, 2 minutes idle first).
- Same workloads everywhere: `nginx:alpine`, `networkstatic/iperf3`, `polinux/stress` from
  Docker Hub; VM guests are Debian 13 cloud images with cloud-init (1 vCPU, 1 GB, 8 GB disk).
- 3 runs per timed test, median reported; every VM operation is followed by a check that the
  guest answers SSH (that check is what exposed the live-migration bug fixed in `67c32ed`).

| platform | version | how it was set up |
|---|---|---|
| Proxmox VE | 9.2.21, kernel 7.0 | official ISO, automated install (ext4, local-lvm thin), updated; PBS 4.2 installed (stopped except for its own test); Docker-in-LXC as a 3rd container option |
| OG-Host | 0.2.3 + this week's fixes, kernel 7.2 | our ISO, unattended install |
| Incus | 7.5.1 (Zabbly stable), kernel 6.12 | Debian 13 + Incus, btrfs pool (recommended over the auto-chosen dir pool), OCI images from Docker Hub, Incus's own `images:debian/13/cloud` for VMs |
| Debian "DIY" | Debian 13, kernel 6.12 | Docker (docker.io) + libvirt/virt-install, qcow2 files, default NAT network; backups by zstd/tar scripts as people do by hand |

Proxmox and OG-Host ran as members of a two-node cluster (partner powered off for the
single-node tests), Incus and Debian standalone.

## What's solid (big, repeatable gaps)

- **Footprint:** OG-Host idles at 171 MB RAM (Debian 210, Incus 242, Proxmox 1399), 2.2 GB disk
  (Proxmox 9.5 incl. PBS), the fewest processes / root processes, best lynis score (66).
- **Boot to management ready:** OG-Host 9.7 s, Debian 19.2, Incus 21.0. Proxmox 96 s *as a
  cluster member whose partner is down* (pveproxy waits ~80 s for quorum; standalone it was
  16.2 s earlier the same day) — a real 2-node-homelab behaviour, but not a like-for-like boot.
- **Containers:** create → serving 0.18 s (Debian Docker 0.30, Docker-in-LXC 0.23, Incus 3.1,
  Proxmox LXC 3.9); 30 containers 6.0 s (Docker-in-LXC 6.8, Docker 8.4, Incus 95, Proxmox 120);
  5.3 MB per container (cgroup + supervisor; Proxmox 9.7, Docker 10.9, Incus 17).
- **Container backup/restore** (500 MB volume): 0.9 s / 0.9 s; Proxmox 2.2 / 4.1, Docker+tar
  15 / 4.8, Incus 20 / 6.9.
- **VMs:** linked clone 0.44 s; 4k random IOPS 141k read / 48k write (others 39–60k / 21–26k);
  idle RAM per 1 GB guest 361 MB with KSM (Debian 439, Proxmox 593, Incus 643); 16 guests in
  3.9 GB of host RAM (others 7.3–7.7 GB).
- **Live migration** (8 GB local thin disk, 1 GB RAM, over the same emulated LAN), all 6 runs
  verified: OG-Host 38.4 s median, Proxmox 104.5 s.
- **HA:** OG-Host fails a VM over after a power cut: running again after 136 s, SSH after 146 s
  (from a replica ≤ 1 min old). Proxmox HA can't do this for VMs on local LVM-thin (needs shared
  storage or ZFS replication plus a third vote). Incus/Debian: no built-in HA.

## Where OG-Host is behind or level

- **Migration downtime:** 79.5 ms median vs Proxmox 31.5 ms.
- **VM backups vs the DIY script:** zstd of the qcow2 file is 1.8 s backup / 3.8 s restore vs
  OG-Host 4.3 / 11.1 (Proxmox vzdump 11.3 / 5.7, PBS 9.4 / 6.3). The DIY way has no dedup, no
  incremental, no encryption, and needs the VM stopped — but it's faster on this disk. Restore
  times varied a lot between runs for every platform (OG-Host measured 3.3 s in another run).
- **Removing 30 containers:** 3.0 s vs Docker 0.8 s (Proxmox 77, Incus 26).
- **Fresh VM memory** (30 s after boot, before KSM merges): 687 MB vs Debian 444.
- **VM↔VM network:** 12.6 Gbit/s vs Debian 13.9 / Proxmox 13.6 / Incus 12.0 — within this
  machine's run-to-run noise (±10–20 %), so call it level.
- **Full VM copy:** Incus 0.38 s (a btrfs copy-on-write, no data copied) and Debian 1.8 s vs
  OG-Host 2.9 s.

## Rows that don't compare (shown, but don't read them as wins)

- **Sequential disk (fio 1M):** Incus and Debian 13–15 GB/s read are the host's page cache —
  their default disk caching (btrfs / libvirt qcow2) doesn't bypass it like Proxmox's and
  OG-Host's `cache=none`. Their sequential writes (~400 MB/s) are also cached; Proxmox and
  OG-Host write ~95 MB/s into freshly allocated thin-LVM blocks.
- **density_mem_per_container_mb:** a host-wide before/after delta; it reads 0 for Proxmox's LXC
  (doesn't capture it). Use `ctr_mem_cgroup_plus_supervisor_mb`.
- **API latency:** Proxmox and OG-Host over HTTPS with a token; Incus over its local socket;
  Debian has no API (`virsh list` timed). All ~10–20 ms.
- **Backup sizes:** Incus uses gzip on export (smaller, much slower); others zstd / dedup.

## Trust

- Nested virtualisation on a desktop that was also running other VMs: identical tests varied
  10–50 % between runs. Treat differences under ~25 % as ties; the big gaps above are 2–40×.
- Each platform was run with its defaults (plus the obvious best choices noted above); nobody
  tuned Proxmox, Incus or the Debian setup the way OG-Host was tuned during this work.
- Not covered: bare metal, ZFS, GPU passthrough, Ceph/shared storage, Incus clustering,
  long-running soak tests, upgrades.

`og-cluster-f4-invalid.txt` is the first OG-Host cluster run: migrations failed on a reset
connection and the VM ended up stopped, so the later "migrations" were offline copies; fixed
(connection reuse + resume on failure) and rerun as `og-cluster-f4.txt`.
