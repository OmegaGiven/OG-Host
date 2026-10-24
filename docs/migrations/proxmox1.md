# Migrating proxmox1 to OG-Host

Inventory taken 2026-10-23/24. Usage history archived first (see "Baseline").

## The machine

| | |
|---|---|
| Board / CPU | ASUS ROG STRIX B650E-F, Ryzen 9 7900X (12c/24t) |
| RAM | 2 x 32 GB DDR5 at 4800 MT/s (rated 6000: EXPO off). 52 of 62 GB used, 3 GB swap in use |
| NVMe | Samsung 990 EVO Plus 4 TB: Proxmox root 96 G + thin pool 3.5 T (1.9 T used) — **the only disk the install touches** |
| HDDs (passed to go) | Seagate 16 TB (`ata-ST16000NT001-3LV101_K3S06HLY`, go `/mnt/disk1`, 12/15 T used), Seagate 12 TB (`ata-ST12000VN0008-3MH101_ZZ30N287-part1`, go `/mnt/disk2`, 0.7/11 T used); mergerfs → go `/mnt/media` |
| Idle | WD Red 2 TB `sda` (one 931 G partition, no filesystem signature, healthy), WD Black SN850/SN810 NVMe `6d:00.0` (bound to vfio-pci, no VM uses it) |
| GPU | RTX 5080 `01:00.0/.1` → station (vfio-pci at boot) |
| Network | one NIC, bridge `vmbr0` 192.168.0.129/24, tailscale on the host |
| Boot args | `amd_iommu=on iommu=pt vfio-pci.ids=15b7:5011,10de:2c02,10de:22e9` |
| Backups | **none configured on the host** (no vzdump jobs) |

## Guests

| ID | Name | State | CPU / RAM | Disk | What it is / does |
|---|---|---|---|---|---|
| 100 | SERVICES (`go`) | running, autostart order 1 | 6c / 8 GB (swapping, 4/5 GB swap) | 300 G (80 %) + both HDDs raw | Main services: Immich, Jellyfin (native), Nextcloud, Navidrome, Audiobookshelf, Kavita, DroppedNeedle, Firefly, life, og-vault, OG Paper, OG Office x2, omegagiven.com, monmmo, Graphite, Caddy, cloudflared, OmegaEnv, mcp-tailnet, go-links (`go.service`), Samba, **NFS export of /mnt/media** (og-host2 depends on it), NAS sync timers, backups (life/firefly) |
| 101 | EXIT-NODE | running | 2c / 2 GB | 20 G | Tailscale exit node only (`exit`, 100.81.18.1) |
| 104 | DOWNLOAD-VM | running | 2c / 6 GB | 20 G | **Retired** 2026-10-23 (everything moved to og-host2); only leftover configs |
| 105 | GPU-STATION (`station`) | running | 12c / 38 GB, RTX 5080 + USB 258a:010c | 2 T (72 %) | Game servers (Pterodactyl panel + Wings: Palworld, Necesse, Factorio, Bedrock, Minecraft Create; sleep/wake proxies), Wolf (Moonlight), Ollama, OpenClaw + Signal + OmegaEnv relay, life worker + tika, llama-server, searxng, chromium-cdp, audiocomic/bead-render/og-poster. Hidden from anti-cheat (`kvm=off,-hypervisor`, copied SMBIOS). Quirk: 5080 can fall off the bus after a VM restart |
| 120 | og-host-dev | running, no autostart | 4c / 4 GB | 40 G | Old OG-Host development VM (superseded by og-host2 + bench VMs) |
| 103 | dev | stopped | 2c / 6 GB | 128 G (45 G data) | Debian; config unchanged since 2026-05-15 |
| 106 | Windows | stopped | 2c / 4 GB | 100 G | Win11 (installer ISO attached), last changed 2026-07-14 |
| CT 102 | authentik-sso | stopped | 2c / 4 GB | 20 G | SSO; offline on the tailnet for 144 days |
| CT 110 | docker | stopped, protected | 2c / 2 GB | 32 G | community-scripts Docker LXC |

Host itself runs only tailscale, omega-agent, smartd.

## Baseline (done)

All usage, log and capacity history is archived before anything changes, in two independent
copies with identical checksums:

- og-host2 `/var/lib/og-host/archive/proxmox1-baseline-2026-10-23/`
- station-sway `~/archive/proxmox1-baseline-2026-10-23/`

Contents: Proxmox RRD history (host, every VM/CT, both storages; hour→decade, AVERAGE+MAX; data
back to 2026-03-14) as JSON plus the raw rrdcached files; the host journal, `/var/log/pve` task
logs and syslog; all guest configs and `/etc/pve`; SMART for every disk; current status
snapshots; OmegaEnv's metrics database (all hosts, since 2026-10-11) and its logs for proxmox,
go, station and download. **Refresh it the day of the switch** (same commands; the archive
README in the og-host2 copy lists them) so the last weeks are in.

## Decisions for the user

1. **Stopped guests and leftovers:** dev, Windows, authentik-sso, docker CT, og-host-dev,
   DOWNLOAD-VM — import, archive (backup kept, not imported), or drop?
2. **Idle drives:** the WD Black NVMe and the 2 TB WD Red: what's on them, and can they become
   OG-Host's backup datastore (RAID1 of the two? the RAID-for-backups wish)?
3. **go:** move it as a VM first and containerize later (recommended, lowest risk for the daily
   services), or straight to host containers?
4. **Window:** on-site (USB install) or remote (kexec install like og-host2)? Planned downtime
   for go's services: about 2 hours; station: about 3–4 hours.
5. **BIOS visit** (if on site): turn EXPO on for the DDR5 (6000), and check the result with the
   boot memtest.

## OG-Host work needed first

- **GPU passthrough on real hardware:** implemented, never tested on metal. Test on og-host2 by
  passing its RTX 2060 to a VM (with a reserved-at-boot vfio binding).
- **Reserve devices for VMs at boot:** a node setting that writes the vfio-pci ids + IOMMU
  kernel args (today Proxmox has them in its own cmdline/modprobe files).
- **VM start order and delay** (go before everything that mounts its NFS).
- **GPU reset hook** for the 5080's "fallen off the bus" quirk (remove + rescan before start).
- **Raw disk passthrough** check (`file: /dev/disk/by-id/...`) and `realistic` mode with station's
  copied SMBIOS + `-cpu host,kvm=off,-hypervisor` + USB passthrough.
- **Import of the backup format we choose** (`og-hostd import-vma` exists and was verified).

## Plan

**Phase 0 — prepare (no downtime)**
1. Build and test the OG-Host items above (og-host2 for GPU passthrough).
2. Backups of every guest we keep, to a place the install won't touch and the NAS sync can't
   clobber: `vzdump --stdout | zstd` onto the 12 TB disk (9.7 T free), into a folder that is
   first added to the exclusions of all three sync jobs (go push, go pull, NAS push-to-go), named
   `*.partial` until complete then renamed; `zstd -t` each; test-import go's backup on og-host2.
   Roughly 1.3–1.6 TB compressed for all guests.
3. Refresh the baseline archive; write down go's and station's tailnet IPs/MACs (keep them, so
   nothing else needs reconfiguring).

**Phase 1 — install (downtime starts)**
4. Shut down guests cleanly (station first, then go). Install OG-Host on the 990 EVO Plus only
   (disk picked by its by-id name; the HDDs are never touched). Same hostname/IP or new name
   `og-host1`.
5. Kernel args for IOMMU/vfio; tailscale up (approve link); join og-host2 as a cluster node.

**Phase 2 — bring services back**
6. **go:** import as a VM (same MAC → same LAN IP and tailnet node), both HDDs passed through raw
   as before, autostart first. Check: Immich, Jellyfin, Nextcloud, Firefly, OmegaEnv, NFS export
   (og-host2's MeTube/liberate/libby/audiobooks write there), Caddy sites.
7. **exit node:** either import the VM or make the OG-Host host itself the exit node (drop a VM).
8. **station:** import as a VM, 5080 + USB passthrough, realistic mode with its SMBIOS; check
   anti-cheat games, Wolf pairing, Pterodactyl game servers (never `docker stop` them).

**Phase 3 — get the efficiency (over the following weeks)**
9. Move go's services out of the VM into OG-Host containers one at a time (media disks mounted
   on the host with mergerfs; NFS + Samba served by the host), then retire the go VM: that's
   where most of the RAM goes back (8 GB VM that swaps → shared host RAM, as on og-host2).
10. Sleep-when-idle for heavy idle services; GPU power caps; zram.
11. After a week, compare with the baseline (`bench/results/`): host RAM, CPU, power, per-service.

**Rollback:** until Phase 3 starts, the backups + a Proxmox ISO restore everything (reinstall
Proxmox, `qmrestore` each backup). Keep the backups at least until go is fully containerized.
