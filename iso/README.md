# OG-Host install ISO

    ./iso/build.sh        # -> iso/out/og-host-<version>-amd64.iso  (~475 MB)

Needs Docker (the build runs in a privileged `debian:trixie` container: mmdebstrap, squashfs,
grub-mkrescue) and builds og-hostd + og-shim first (`scripts/build-debian.sh`). The ISO is
hybrid: it boots from CD/DVD or a USB stick, on UEFI or BIOS.

## What's on it

A Debian 13 (trixie) system with the Linux kernel, firmware, QEMU/KVM, OVMF, swtpm, LVM thin,
nftables, systemd-networkd/resolved, OpenSSH, and OG-Host (`/usr/local/bin/og-hostd`, `og-shim`).
The same image is the live installer and, copied to disk, the installed system. Files added on
top of Debian are in `overlay/`; `customize.sh` sets it up, `inside.sh` makes the ISO.

## Installing

Boot it; the installer starts on the console (tty2 has a root shell while installing). It asks:

- the disk (everything on it is erased) and the system's size; the rest of the disk becomes the
  LVM thin pool `og/data`, storage pool `local-thin`, for VM disks (0 = whole disk for the system)
- ext4 or xfs, server name, network card, DHCP or a static address, time zone
- the first web account (an Administrator), root's password, optionally root's SSH key

Then it restarts into OG-Host; the console shows `https://<address>:8006`.

Layout (GPT): 1 MiB BIOS boot, 512 MiB EFI system partition, the system, LVM. GRUB is installed
for UEFI (also at the removable fallback path, so no NVRAM entry is needed) and for BIOS, so the
disk boots either way. Grub's menu also goes to the first serial port.

## Unattended

Pick "answers from a drive labelled OG-AUTO" in the boot menu (or put `og.auto=label` on the
kernel command line, or `og.auto=/path/to/file`). The installer reads `og-auto.cfg` from a drive
(USB stick, FAT or ext4) labelled `OG-AUTO`:

    disk = auto              # or /dev/nvme0n1; auto = the first disk of 16 GB or more
    root_gb = 32             # 0 = whole disk for the system, no thin pool
    fs = ext4                # or xfs
    hostname = lab1
    net = dhcp               # or static, with address = 192.168.1.20/24, gateway, dns = 1.1.1.1 9.9.9.9
    iface = enp3s0           # default: the first wired card
    timezone = Europe/Berlin
    admin_user = admin
    admin_password = ...     # 10 characters or more
    root_password = ...
    ssh_key = ssh-ed25519 AAAA...
    reboot = yes

Make such a drive image: `truncate -s 4M a.img; mkfs.vfat -n OG-AUTO a.img; mcopy -i a.img og-auto.cfg ::`.
The progress also goes to the kernel log (a serial console shows it).

## Testing it

Unattended install into a QEMU VM, then boots on UEFI (q35) and BIOS (i440fx): see the commands
in the commit that added this, or roughly:

    qemu-system-x86_64 -enable-kvm -m 3072 -machine q35 -drive if=pflash,... OVMF \
      -drive file=disk.qcow2,if=virtio -drive file=a.img,format=raw,if=virtio \
      -cdrom og-host-….iso    # choose the OG-AUTO menu entry
