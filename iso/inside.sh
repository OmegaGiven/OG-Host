#!/bin/sh
# Runs in the ISO builder container (see build.sh): /src is the source tree, /out the output.
set -eu
W=/build
R=$W/rootfs
ISO=$W/iso
rm -rf "$W" && mkdir -p "$R" "$ISO/live" "$ISO/boot/grub"

PKGS="systemd,systemd-sysv,systemd-resolved,systemd-timesyncd,dbus,udev,kmod,
linux-image-amd64,initramfs-tools,live-boot,live-boot-initramfs-tools,
grub-efi-amd64-bin,grub-pc-bin,grub2-common,efibootmgr,amd64-microcode,intel-microcode,
firmware-linux-free,firmware-misc-nonfree,firmware-realtek,firmware-bnx2,firmware-bnx2x,
qemu-system-x86,qemu-utils,ovmf,swtpm,swtpm-tools,genisoimage,libseccomp2,
nftables,iproute2,openssh-server,openssh-client,curl,ca-certificates,ethtool,iputils-ping,bridge-utils,
lvm2,thin-provisioning-tools,e2fsprogs,xfsprogs,dosfstools,squashfs-tools,fdisk,parted,nfs-common,cifs-utils,
pciutils,usbutils,smartmontools,nvme-cli,lsof,
zstd,xz-utils,less,vim-tiny,sudo,bash-completion,locales,console-setup,keyboard-configuration,tzdata,apt-transport-https"
PKGS=$(echo "$PKGS" | tr -d '\n ')

mmdebstrap --variant=minbase --components=main,contrib,non-free-firmware --include="$PKGS" \
  --aptopt='APT::Install-Recommends "false"' \
  --setup-hook='mkdir -p "$1/var/cache/apt/archives" && mount --bind /var/cache/apt/archives "$1/var/cache/apt/archives"' \
  --customize-hook='umount "$1/var/cache/apt/archives" || true' \
  --customize-hook='sh /src/iso/customize.sh "$1"' \
  trixie "$R" http://deb.debian.org/debian

# boot files for the live system, then the image (/boot stays in it: the installer copies it)
cp "$R"/boot/vmlinuz-* "$ISO/live/vmlinuz"
cp "$R"/boot/initrd.img-* "$ISO/live/initrd.img"
mksquashfs "$R" "$ISO/live/filesystem.squashfs" -comp zstd -Xcompression-level 19 -b 1M -noappend -quiet -wildcards \
  -e "proc/*" "sys/*" "dev/*" "run/*" "tmp/*"
echo "OG-Host $VERSION" > "$ISO/og-host.txt"

cat > "$ISO/boot/grub/grub.cfg" <<CFG
set timeout=8
set default=0
insmod all_video
insmod gfxterm
serial --unit=0 --speed=115200
terminal_input console serial
terminal_output console serial
set menu_color_normal=light-gray/black
set menu_color_highlight=black/light-gray

menuentry "Install OG-Host $VERSION" {
  linux /live/vmlinuz boot=live og.installer quiet splash=no
  initrd /live/initrd.img
}
menuentry "Install OG-Host $VERSION (answers from a drive labelled OG-AUTO)" {
  linux /live/vmlinuz boot=live og.installer og.auto=label
  initrd /live/initrd.img
}
menuentry "Install OG-Host $VERSION (serial console)" {
  linux /live/vmlinuz boot=live og.installer console=tty1 console=ttyS0,115200n8
  initrd /live/initrd.img
}
menuentry "Install OG-Host $VERSION (safe graphics)" {
  linux /live/vmlinuz boot=live og.installer nomodeset
  initrd /live/initrd.img
}
if [ "\$grub_platform" = "efi" ]; then
  menuentry "UEFI firmware settings" { fwsetup }
fi
CFG

grub-mkrescue -o "/out/og-host-$VERSION-amd64.iso" "$ISO" -- -volid "OGHOST" 2>&1 | grep -v '^xorriso.*: UPDATE' || true
test -s "/out/og-host-$VERSION-amd64.iso"
chown "$OWNER" "/out/og-host-$VERSION-amd64.iso"
