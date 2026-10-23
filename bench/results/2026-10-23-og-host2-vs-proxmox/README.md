# proxmoxtwo (Proxmox VE) vs og-host2 (OG-Host): same machine, real services — 2026-10-23

Same hardware (Ryzen 7 2700X, 32 GB, RTX 2060 + RTX 3070). Measured by the OmegaEnv agent
(Proxmox: host + guest agents, `omegaenv-hosts-snapshot.json`) and by OG-Host itself.

**Proxmox window:** only 3 minutes are usable (00:36:34–00:39:44, 20 samples, before the backups
started), so treat its CPU figure as indicative. The machine was wiped right after.

| | Proxmox VE 9 (4 VMs) | OG-Host 0.3 (containers) |
|---|---|---|
| host CPU, idle | 0.7 % median (3 min) | 0.4 % (30 s, after fixing the log follower bug below) |
| host RAM used | **29.0 GB** of 33.6 (86 %) | **11.5 GB** of 33.6 |
| RAM still available | ~4.6 GB | **20.5 GB** |
| … of which Stable Diffusion | asleep (VM used 1.2 GB) | loaded: 8.3 GB (model kept in RAM) |
| Proxmox host alone (VMs stopped, 01:39) | 2.7 GB | — |
| GPU idle power | 2060 9.4 W, 3070 19.8 W | 2060 9.5 W, 3070 19.1 W |

What ran: Proxmox — pmx2-apps (Mealie, Stirling PDF, mirror, OG Office bhb, og-vault/admin),
pmx2-music-ai (Karaoke Maker, koboldcpp), pmx2-media-ai (og-rpg-chat, SD/ComfyUI on demand),
pmx2-ci-runner (idle). OG-Host — the same apps as containers except the CI runner and ComfyUI,
plus the liberate stack; Stable Diffusion is always loaded instead of woken on demand.

Why the RAM gap: each VM holds its whole allocation (12.9 + 6.4 + 8.6 + 4.3 GB) although the
guests used only ~6.6 GB inside; containers use what the apps use. Without the always-loaded
SD model, the same services take ~3.2 GB on OG-Host.

CPU is a wash at idle (both under 1 %). During the first hours on OG-Host the CPU sat at ~7 %:
that was the OmegaEnv log follower spinning on OG-Host's Docker logs endpoint (follow resent the
log tail forever; fixed in d9d25ee), not the AI apps.
