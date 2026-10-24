# Migrating proxmox1 to OG-Host (`og-host1`)

Plan v2, 2026-10-24, from the inventory of 2026-10-23/24 and the user's decisions.
Principle: **the media drives are never written to by the migration**, and services come back
as **containers on OG-Host**, not VMs. No VM survives the move.

## Hardware and what happens to it

| Part | Today | After |
|---|---|---|
| Samsung 990 EVO Plus 4 TB NVMe | Proxmox + all VM disks | **wiped**: OG-Host root, state, apps |
| Seagate 16 TB + 12 TB (mergerfs `/mnt/media`, 13 of 26 TB used) | passed raw into the go VM | **untouched**; mounted on the og-host1 host (same mergerfs pool, same `/mnt/media` path) |
| RTX 5080 | passed to station VM | **freed** for og-host1 containers (og-play streaming, AI models) |
| WD Black SN850 NVMe (`6d:00.0`, vfio-bound, unused) and WD Red 2 TB (`sda`, no filesystem found) | idle | candidate mirrored (RAID1) backup datastore, once the user confirms they hold nothing |
| Ryzen 9 7900X, 64 GB DDR5 at 4800 (rated 6000) | | same; EXPO on at the BIOS visit |

## Baseline (done)

All Proxmox usage/log/capacity history (RRD from 2026-03-14, journals, task logs, configs,
SMART, OmegaEnv metrics + logs) is in two verified copies: og-host2
`/var/lib/og-host/archive/proxmox1-baseline-2026-10-23/` and station-sway
`~/archive/proxmox1-baseline-2026-10-23/`. Refresh it on switch day.

## Service by service

Where the data lives today decides the work: data already on the media drives just stays;
anything on the NVMe (VM disks) must be backed up before the wipe.

### Bring back as containers on og-host1

| Service | Data on media (stays) | Data on NVMe (back up, restore) | Notes |
|---|---|---|---|
| **Immich** | library `/mnt/media/immich-library` (+ its nightly DB dumps in `…/backups`) | postgres `~/immich/postgres` (1.1 GB), ML model cache (0.8 GB, re-downloadable), compose + `.env` | restore from a fresh `pg_dumpall` taken after the services stop (not just the nightly dump); same Immich version first, upgrade later |
| **Jellyfin** | the media | `/var/lib/jellyfin` (20 GB: library DB, metadata, images), `/etc/jellyfin`, `/var/cache` (skip) | native install today → official container with the same data/config dirs; library paths stay `/mnt/media/...` |
| **Audiobookshelf** | `/mnt/media/BOOKS` | `/opt/audiobookshelf/{config,metadata}` (85 MB) | |
| **OG Paper** | | `~/og-paper-server/data` (45 MB) + image | |
| **monmmo** | ROMs per monmmo-deploy memory | docker volume `deploy_og-pokemmo-data` (170 MB) + `~/monmmo` (1.4 GB: assets/data/deploy) | rebuild image per the monmmo-deploy memory |
| **Firefly III** | `/mnt/media/APP-DATA/firefly/{pgdata,upload}` | compose + `.env`, `~/firefly/simplefin-sync` (+ its timer), `ogfin` | pg data already on media; still take a `pg_dump` before stopping |
| **life** (DB + API + gather) | `/mnt/media/APP-DATA/life/{pgdata,blobs,inbox,backups}` | `~/life/deploy` (compose, **secrets/**), `life-backup.timer` | life worker/tika/memos ran on station: see "Decide" |
| **NAS sync jobs** | | `~/sync-scripts/` (push/pull + status server), their user timers, `~/sync-logs`; **NAS side:** `nas-push-to-go` cron + scripts on antietam-vault | **very important**; come back as host systemd timers on og-host1. Fix while at it: `--update` on the NAS→go job and the in-progress-file problem (memory go-nas-sync-hazard). Paths stay `/mnt/media` so the scripts barely change. **Disable both directions for the whole migration window** |

### Move to og-host2 (containers, media over NFS)

| Service | Media | Back up from go |
|---|---|---|
| **Kavita** | `/mnt/media/BOOKS/Comics` (read via NFS) | `~/kavita/config` (2.4 GB) |
| **Navidrome** | `/mnt/media/MUSIC` | `~/navidrome/data` (146 MB) |
| **DroppedNeedle** | `/mnt/media/MUSIC`, `/mnt/media/downloads/soulseek` | `~/droppedneedle/{config,cache}` (7 MB) |

These can move **before** the switch (og-host2 already mounts go's `/mnt/media`): less to do on
the day. Their NFS mounts then point at og-host1 after it.

### Back up only (not brought back)

| What | Back up |
|---|---|
| **Nextcloud** (a new drive suite comes later) | `~/nextcloud-docker` (1.9 GB: app, config), `mysqldump` of the DB, users' files and notes (its data dir; `my_shared_media` is just `/mnt/media`, don't copy that). Export notes as files too |
| **OG Note** (`og-vault`) | volume `og-vault-deploy_og-vault-data` (13 MB) + `notes-public/` |
| **Pterodactyl** (game servers, not restarted yet) | panel DB `~/pterodactyl/panel/data/database` (+ `mysqldump`), panel data, Wings config, **server volumes `/var/lib/pterodactyl/volumes` (7.5 GB: Palworld, Necesse, Factorio, Bedrock, Create worlds)**, the wake/lazymc proxy units |
| **OpenClaw** (a better AI system replaces it) | `~/.openclaw` (116 MB: agents, **skills**: firefly-report, homelab-map/-monitor/-triage, news-digest, note-catalog; credentials, identity, MCP auth), `~/openclaw`, unit files |
| **Signal connectors** | `~/.local/share/signal-cli` (138 MB, the "Omega Lab" number's keys), `signal-omega` + `omegaenv-claw-relay` units, `libby-signal-relay` script + its allowed-recipient file |
| **AI models** (kept for later) | `~/ollama/ollama-data` (6 GB), `~/ai-companion/models` (16 GB), `~/ai-companion/llama-models` (19 GB, incl. hermes-qwen3.5-35b), life-ollama models |
| **AudioComic** (retired) | code is on GitHub (OmegaGiven/AudioComic, last push 2026-09-05) **but 6 local changes are uncommitted**: commit+push them; archive `~/comic-audiobook` (35 GB incl. venvs/outputs) and `~/audiocomic-secrets.env` |
| **OmegaEnv** (decommissioned: OG-Host UI + cluster homepage replace it) | `~/omegaenv` incl. `core/data` (tiles, watches, secrets, notify config), `/mnt/media/APP-DATA/omegaenv` (metrics.db, logs; already in the baseline) — for `og-hostd lab import` into the new UI |

### Let go

- **Wolf** (moving to og-play on OG-Host) and the station VM itself.
- **DOWNLOAD-VM** (retired yesterday), **og-host-dev** (old dev VM).

### Not covered by the notes — decide (my suggestion in brackets)

| Service | Where | Suggestion |
|---|---|---|
| Caddy (+ its TLS state) | go | [drop: tailscale serve on OG-Host replaces it; keep config in backup] |
| cloudflared (omegagiven.com tunnel) + `omegagiven-site` | go | [bring back as containers: public site] |
| OG Office dev + personal (`og-office`, `og-office-personal`) | go; data on media `APP-DATA/og-office*`, keys on NVMe | [bring back: keys/ must be backed up] |
| Graphite editor | go | [bring back, tiny static site] |
| mcp-tailnet (Claude phone/web tools via funnel) | go | [bring back as container] |
| go-links (`go.service`, `go/…`) | go | [bring back; later feeds the cluster homepage aliases] |
| Samba share + NFS export of `/mnt/media` | go | [bring back on the og-host1 host: og-host2's MeTube, liberate, libby, audiobooks write there] |
| Ollama / llama-server / searxng / chromium-cdp, life worker + tika + memos, bead-render, og-poster agent | station | [life worker: yes, as a container on the 5080; the rest: back up, decide with the new AI system] |
| EXIT-NODE VM | | [drop the VM; og-host1's own tailscale offers the exit node] |
| dev VM (45 GB, unused since May), Windows VM, authentik-sso, docker CT | | [archive disk images, don't bring back] |
| Alerts: libby + epic claimer + hardware alerts use station's Signal relay / OmegaEnv `/message` | | [until the new AI system: a small Signal relay container on og-host1 using the saved signal-cli data] |

## Naming and addresses

go (192.168.0.137) is referenced by og-host2's NFS mount, the NAS sync jobs, liberate's
`extra_hosts` and old bookmarks. Proposal: **og-host1 takes LAN IP 192.168.0.137** (so NFS
clients and the NAS keep working) with tailnet name `og-host1`; update the few `go` names.
Remove the old `go`, `station`, `exit`, `download`, `proxmox` nodes from the tailnet after.

## OG-Host work before the switch

1. **Cluster** og-host1 + og-host2 (exists: `og-hostd` cluster join; test on the bench pair again).
2. **Cluster homepage** that builds itself from the services on both nodes (tiles from compose
   projects: name, URL from `tailscale serve`/published ports, health, sleeping state, icons via
   `og.*` labels; go-links aliases) — the "auto front-end tiles" request — plus import of
   OmegaEnv's tiles/watches (`og-hostd lab import`).
3. **Alerts** without OmegaEnv: OG-Host notifications (hardware, integrity, failed containers)
   through the Signal relay container.
4. **Host-level mergerfs + NFS/Samba export** on og-host1 (a host service, set up at install).
5. **RAID1 backup datastore** on the two idle drives (the RAID-for-backups wish).
6. Nightly OG-Host backup jobs for the app data dirs (none exist on Proxmox today).

GPU/USB passthrough and VM import are **no longer needed** (no VMs come back).

## Backups: where

Not on the media drives (user rule), not on the NVMe (wiped). Two copies:
- **NAS** (antietam-vault, 9.4 TB free) in a folder **outside** `/srv/media` (the sync only covers
  `/srv/media`), e.g. `/srv/backup/proxmox1-2026-10-xx/`.
- **og-host2** `/var/lib/og-host/archive/` (545 GB free) for the app-level backups (small).

Two kinds:
- **App-level** (what gets restored): DB dumps taken with services stopped, config/data dirs,
  compose files, secrets, unit files — about **120 GB** with the AI models and AudioComic, ~25 GB
  without. Checksums recorded at the source, compared at both copies.
- **Whole VM images** (insurance, never restored unless something was missed): vzdump of go
  (300 G disk) and station (2 T disk, ~1.4 T used) + the small/stopped guests to the NAS only
  (~1–1.5 TB compressed), each written as `.partial` then renamed, `zstd -t` checked.

## Schedule

**Phase 0: prepare (no downtime, days before)**
1. Build items 1–4 above on og-host2 / bench.
2. Move Kavita, Navidrome, DroppedNeedle to og-host2; check they read the media over NFS.
3. Commit+push AudioComic's local changes; write the backup script (one run per service, listed
   above) and dry-run it.
4. Whole VM images to the NAS; check.

**Phase 1: switch day**
5. Disable the sync jobs (both go timers and the NAS cron). Pause og-host2's NFS writers
   (MeTube, liberate importer, libby, audiobooks).
6. Stop services on go and station; take the final app-level backups (DB dumps with everything
   stopped); verify both copies' checksums. Refresh the baseline archive.
7. Shut down all guests and the host. **If on site: unplug the two media drives (and the idle
   ones) before installing.** Install OG-Host on the 990 EVO Plus, picked by its by-id name.
8. Plug the drives back, mount the mergerfs pool read-only first and check it (file counts and a
   checksum sample against the NAS copy), then read-write.

**Phase 2: bring back (same day, in this order)**
9. Host: tailscale up, IP .137, NFS + Samba exports, join the cluster with og-host2, Signal relay.
10. Firefly, life, Immich, Jellyfin, Audiobookshelf (the daily ones), then OG Paper, monmmo,
    OG Office, omegagiven.com, mcp-tailnet, go-links, Graphite.
11. Re-enable og-host2's NFS writers; re-enable the sync jobs **last**, after a dry run
    (`RSYNC_EXTRA="-n -i"`) shows no unexpected deletions or overwrites.
12. og-play on the 5080; life worker on the 5080.

**Phase 3: after**
13. Cluster homepage live; OmegaEnv on go was the VM, so it's gone with it: confirm everything it
    did is covered (dashboard, alerts, logs, metrics).
14. A week later: compare with the baseline (`bench/results/`).
15. Keep the backups until everything has run cleanly for a month.

**Rollback:** the whole-VM images + a Proxmox ISO restore the old setup (reinstall Proxmox on the
NVMe, `qmrestore`); the media drives were never part of it.

## Open questions

1. The "decide" table above.
2. Do the WD Black NVMe and the WD Red hold anything? (If not → RAID1 backup datastore.)
3. On site (unplug drives, USB install, EXPO) or remote (kexec like og-host2)? On site is safer
   for the media drives.
4. Pterodactyl and OpenClaw backups: archive forever, or until the replacements exist?
