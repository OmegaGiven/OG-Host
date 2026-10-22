# OG-Host vs Proxmox VE — 2026-10-22

**Setup.** Both on station-sway (Ryzen, 62 GB, NVMe), each in an identical QEMU/KVM VM: 4 vCPU
pinned to host cores 8–11, 8 GB RAM, 64 GB virtio disk (qcow2, cache=none, io_uring), UEFI,
user-mode network. Only one of them running at a time. Fresh boot, 2 minutes idle, then
`bench/bench.sh` (3 runs per test, medians). Guests are nested (L2), so absolute numbers are
lower than on bare metal; both pay the same.

- **Proxmox VE 9.2-1** (official ISO, automated install: ext4, 16 GB root, no swap, rest
  `local-lvm` thin pool), kernel 7.0.2-pve, its defaults. Containers = Proxmox 9's OCI
  application containers (LXC, unprivileged, 1 GB thin-LV root each).
- **OG-Host 0.2.3** (our ISO, unattended install: same layout), Debian kernel 6.12.
  *Baseline* = as shipped on the ISO; *tuned* = this commit (disk I/O threads + io_uring, an
  always-on balloon with free-page reporting, parallel full copies, parallel backup/restore).
- Same workloads: `nginx:alpine` from Docker Hub; VMs = Debian 13 genericcloud image as a
  template with cloud-init (1 vCPU, 1 GB), linked/full clones on the thin pool.

Raw output: `pve-pass3.txt` / `og-pass3.txt` (main run), `og-pass2.txt` (OG baseline),
`pve-pass2.txt`; `pve.txt`/`og.txt` = first run (script bugs, partial).

| | Proxmox VE 9.2 | OG-Host baseline | OG-Host tuned |
|---|---:|---:|---:|
| **Install** (unattended, to first prompt off) | 128 s¹ | 36 s | |
| **Boot** to web UI answering | 16.2 s | 10.3 s | |
| Disk used after install | 4.6 GB | 1.4 GB | |
| Packages | 742 | 304 | |
| RAM used, idle, nothing running | 1.4 GB | 132 MB | 125 MB |
| CPU busy, idle | 0.4–4 % | 0.02 % | 0.02 % |
| Processes / running as root | 53 / 41 | 22 / 17 | |
| Listening TCP ports (not localhost) | 6 | 7 | |
| **Container** create → serving HTTP | 5.7 s (4.1–5.7) | 0.40 s | 0.40 s |
| 30 containers → all serving | 191 s (121–191) | 13.5 s | 13.7 s |
| Remove the 30 | 110 s (75–110) | 5.7 s | 6.1 s |
| **VM** linked clone | 1.9 s | 0.23 s | 0.46 s |
| Full clone, 8 GB disk | 7.2 s | 4.7 s | **2.2 s** |
| VM start → SSH answering | 14.3 s | 14.1 s | 15.6 s |
| Host RAM per idle 1 GB guest | **600 MB** | 754 MB | 692 MB |
| Guest disk, 4k random read (fio, QD32) | 57 k IOPS | 102 k | 88 k |
| 4k random write | 53 k IOPS | 105 k | 100 k |
| 1M sequential read | 1437 MB/s | 1551 | 1834 |
| 1M sequential write | 311 MB/s | 344 | 307 |
| VM ↔ VM (iperf3, same host) | **13.6 Gbit/s** | 11.0 | 10.9 |
| VM ↔ VM ping | 0.31 ms | 0.30 | 0.34 |
| Host → VM | 27.4 Gbit/s | 28.9 | 27.4 |
| **Backup** stopped VM (8 GB disk, ~2.5 GB used) | **9.9 s** | 22.9 s | 15.5 s |
| Backup size | 1374 MB | 1342 MB² | |
| Restore | **10.2 s** | 18.5 s | 12.4 s |
| Second backup, nothing changed | **9.9 s** | 17.3 s | 14.9 s |

¹ includes Proxmox's 10 s boot-menu countdown; OG's run booted the installer kernel directly.
² store size after one backup (the tuned run's store already held the baseline's chunks).

## Reading it

- **OG-Host is lighter**: a tenth of the idle RAM, a third of the disk, idle CPU near zero, fewer
  root processes. It installs and boots faster.
- **Containers are OG-Host's big lead**: ~14× faster to start and remove. Proxmox's OCI support
  turns each image into an LXC container with its own thin LV + ext4 (made per container);
  OG-Host runs OCI containers natively on overlay storage.
- **VMs are close**: same QEMU/KVM underneath. OG-Host wins clones and random disk I/O
  (virtio-blk with an I/O thread vs Proxmox's virtio-scsi default), ties boot and host→VM network.
- **Where Proxmox still wins**: VM↔VM network (~20 % more; Proxmox's 7.0 kernel vs Debian's 6.12
  is the first suspect), host RAM per idle VM (~90 MB less; OG's VMs boot UEFI with a standard
  VGA by default, the Proxmox template had SeaBIOS and a serial console — not yet an
  apples-to-apples comparison), and backups (vzdump's plain full dump vs OG's chunked,
  deduplicated store; parallelizing chunking brought OG from 2.3× to 1.5× slower, and an
  unchanged second backup is still a full read — skipping unallocated thin-pool regions is next).

## Not covered yet

Bare metal; ZFS; GPU; memory pressure (who survives, does Postgres keep its latency); live
migration and HA timings (need two nodes each); update + rollback; security scans (Lynis, CVE
scan); Proxmox Backup Server (incremental, would change the backup rows); Docker-in-LXC on
Proxmox (the way many people actually run containers there).

Per-container memory isn't reported: the before/after host-memory delta came out negative on
Proxmox (unreliable for LXC), so it needs per-cgroup accounting on both.
