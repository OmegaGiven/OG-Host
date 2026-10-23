#!/usr/bin/env python3
"""report.py DIR TAG — a markdown table from DIR/{pve,og,incus,deb}-TAG.txt (RESULT lines),
DIR/boot-TAG.txt and DIR/{pve,og}-cluster-TAG.txt. Best value per row in bold (lower is better
unless the unit says otherwise). Every number comes from the files; nothing is typed in."""
import re, sys, statistics, pathlib
d, tag = pathlib.Path(sys.argv[1]), sys.argv[2]
plats = [("pve", "Proxmox VE 9.2"), ("og", "OG-Host"), ("incus", "Incus 7.5"), ("deb", "Debian 13 + Docker + libvirt")]
vals = {p: {} for p, _ in plats}
units = {}
for p, _ in plats:
    for f in [d / f"{p}-{tag}.txt", d / f"{p}-cluster-{tag}.txt"]:
        if f.exists():
            for l in f.read_text().splitlines():
                m = re.match(r"RESULT (\S+) (\S+) ?(.*)", l)
                if m and re.match(r"^-?[0-9.]+$", m.group(2)):
                    vals[p][m.group(1)] = float(m.group(2)); units[m.group(1)] = m.group(3).strip()
b = d / f"boot-{tag}.txt"
if b.exists():
    per = {}
    for l in b.read_text().splitlines():
        m = re.match(r"(\S+) boot_to_ready_s (\S+)", l)
        if m:
            per.setdefault(m.group(1), []).append(float(m.group(2)))
    for p, v in per.items():
        if p in vals:
            vals[p]["boot_to_ready_s"] = statistics.median(v); units["boot_to_ready_s"] = "s"
higher = lambda k, u: any(x in u for x in ["iops", "MB/s", "Gbit", "/100"]) or k.startswith("vm_density_1g") or k.endswith("_saved_mb")
keys = []
for p, _ in plats:
    for k in vals[p]:
        if k not in keys:
            keys.append(k)
print("| test | " + " | ".join(n for _, n in plats) + " |")
print("|---|" + "---:|" * len(plats))
for k in keys:
    u = units.get(k, "")
    row = [vals[p].get(k) for p, _ in plats]
    have = [x for x in row if x is not None]
    best = (max(have) if higher(k, u) else min(have)) if len(have) > 1 and not k.startswith(("tcp_ports", "packages")) else None
    cells = []
    for x in row:
        if x is None:
            cells.append("–")
        else:
            s = f"{x:g}"
            cells.append(f"**{s}**" if best is not None and x == best else s)
    print(f"| {k} ({u}) | " + " | ".join(cells) + " |")
