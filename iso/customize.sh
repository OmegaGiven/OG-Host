#!/bin/sh
# mmdebstrap customize hook: turn the Debian base into OG-Host. $1 = the root file system.
set -eu
R=$1
install -m 0755 /src/target-debian/release/og-hostd /src/target-debian/release/og-shim "$R/usr/local/bin/"
install -m 0644 /src/packaging/og-hostd.service "$R/etc/systemd/system/og-hostd.service"
cp -r /src/iso/overlay/. "$R/"

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

echo og-host > "$R/etc/hostname"
printf '127.0.0.1\tlocalhost\n127.0.1.1\tog-host\n::1\tlocalhost ip6-localhost ip6-loopback\n' > "$R/etc/hosts"
sed -i 's/^# *\(en_US.UTF-8\)/\1/' "$R/etc/locale.gen"
chroot "$R" locale-gen >/dev/null
echo 'LANG=C.UTF-8' > "$R/etc/default/locale"
ln -sf /run/systemd/resolve/stub-resolv.conf "$R/etc/resolv.conf"
chroot "$R" systemctl enable systemd-networkd systemd-resolved systemd-timesyncd ssh og-hostd og-installer og-live-shell >/dev/null 2>&1
# the install medium's root is locked; the installer sets a password
chroot "$R" passwd -l root >/dev/null
# fresh identity per install (and per live boot)
: > "$R/etc/machine-id"
rm -f "$R"/etc/ssh/ssh_host_* "$R/var/lib/dbus/machine-id"
chroot "$R" update-initramfs -u -k all >/dev/null
rm -rf "$R"/var/lib/apt/lists/* "$R"/var/cache/apt/*.bin "$R"/tmp/* "$R"/var/log/*.log
