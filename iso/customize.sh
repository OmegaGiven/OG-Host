#!/bin/sh
# mmdebstrap customize hook: turn the Debian base into OG-Host. $1 = the root file system.
set -eu
R=$1
install -m 0755 /src/target-debian/release/og-hostd /src/target-debian/release/og-shim "$R/usr/local/bin/"
install -m 0644 /src/packaging/og-hostd.service /src/packaging/og-hostd.socket "$R/etc/systemd/system/"
# BuildKit (fetched and checked by build.sh): buildkitd, buildctl and its runc
tar -xzf /out/cache/buildkit-*.linux-amd64.tar.gz -C "$R/usr/local" bin/buildkitd bin/buildctl bin/buildkit-runc
install -m 0644 /src/packaging/og-buildkitd.service "$R/etc/systemd/system/og-buildkitd.service"
cp -r /src/iso/overlay/. "$R/"
# LVM: only OG-Host's own volume group activates by itself (install system and installed
# system); volume groups on other disks (an old Proxmox install, someone else's array) stay
# inactive unless asked for, so nothing touches them on its own
sed -i 's|^\(\s*\)# auto_activation_volume_list = \[ "vg1".*|\1auto_activation_volume_list = [ "og" ]|' "$R/etc/lvm/lvm.conf"
grep -q '^\s*auto_activation_volume_list = \[ "og" \]' "$R/etc/lvm/lvm.conf"

# apt: Debian trixie with security and point-release updates
rm -f "$R/etc/apt/sources.list"
cat > "$R/etc/apt/sources.list.d/debian.sources" <<SRC
Types: deb
URIs: http://deb.debian.org/debian
Suites: trixie trixie-updates
Components: main contrib non-free-firmware
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg

Types: deb
URIs: http://security.debian.org/debian-security
Suites: trixie-security
Components: main contrib non-free-firmware
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
SRC

# a newer kernel than trixie's 6.12 (backports; Proxmox ships a 7.x kernel too): VM-to-VM
# networking measured ~10 % faster with it
cat > "$R/etc/apt/sources.list.d/backports.sources" <<SRC
Types: deb
URIs: http://deb.debian.org/debian
Suites: trixie-backports
Components: main contrib non-free-firmware
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
SRC
cat > "$R/etc/apt/preferences.d/og-kernel" <<PREF
Package: linux-image-amd64 linux-image-*-amd64 linux-headers-amd64 linux-headers-*-amd64 linux-kbuild-* linux-base
Pin: release n=trixie-backports
Pin-Priority: 600
PREF
chroot "$R" apt-get -qq update
old=$(chroot "$R" dpkg-query -W -f '${Package}\n' 'linux-image-6*' 2>/dev/null || true)
DEBIAN_FRONTEND=noninteractive chroot "$R" apt-get -qq install -y -t trixie-backports linux-image-amd64 >/dev/null
[ -n "$old" ] && DEBIAN_FRONTEND=noninteractive chroot "$R" apt-get -qq purge -y $old >/dev/null
rm -f "$R"/boot/*.old

echo og-host > "$R/etc/hostname"
printf '127.0.0.1\tlocalhost\n127.0.1.1\tog-host\n::1\tlocalhost ip6-localhost ip6-loopback\n' > "$R/etc/hosts"
sed -i 's/^# *\(en_US.UTF-8\)/\1/' "$R/etc/locale.gen"
chroot "$R" locale-gen >/dev/null
echo 'LANG=C.UTF-8' > "$R/etc/default/locale"
ln -sf /run/systemd/resolve/stub-resolv.conf "$R/etc/resolv.conf"
# NFS v4 needs no portmapper: rpcbind stays off (port 111); an NFS v3 mount still starts it
chroot "$R" systemctl disable rpcbind.service rpcbind.socket >/dev/null 2>&1 || true
chroot "$R" systemctl enable systemd-networkd systemd-resolved systemd-timesyncd ssh og-hostd.socket og-hostd og-buildkitd og-installer og-live-shell >/dev/null 2>&1
# the install medium's root is locked; the installer sets a password
chroot "$R" passwd -l root >/dev/null
# fresh identity per install (and per live boot)
: > "$R/etc/machine-id"
rm -f "$R"/etc/ssh/ssh_host_* "$R/var/lib/dbus/machine-id"
chroot "$R" update-initramfs -u -k all >/dev/null
chroot "$R" apt-get clean
rm -rf "$R"/var/lib/apt/lists/* "$R"/var/cache/apt/*.bin "$R"/tmp/* "$R"/var/log/*.log
