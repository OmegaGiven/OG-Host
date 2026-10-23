#!/bin/sh
# Build the OG-Host install ISO (hybrid: UEFI and BIOS, CD/DVD or USB stick).
#   ./iso/build.sh            -> iso/out/og-host-<version>-amd64.iso
# Builds og-hostd + og-shim for Debian (scripts/build-debian.sh), then a Debian 13 (trixie)
# system with them in a privileged Docker container (mmdebstrap, squashfs, grub-mkrescue).
set -eu
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
./scripts/build-debian.sh >/dev/null
# BuildKit for `og-hostd build` / compose `build:` (pinned release, checked against its sha256)
BUILDKIT=v0.34.1 BUILDKIT_SHA=a59539daf547fe0958efe2d07e41c0a6944e1112a66d7e519ec1cf5b673ef15b
mkdir -p iso/out/cache
BK=iso/out/cache/buildkit-$BUILDKIT.linux-amd64.tar.gz
if ! echo "$BUILDKIT_SHA  $BK" | sha256sum -c --status 2>/dev/null; then
  curl -fsSL -o "$BK" "https://github.com/moby/buildkit/releases/download/$BUILDKIT/buildkit-$BUILDKIT.linux-amd64.tar.gz"
  echo "$BUILDKIT_SHA  $BK" | sha256sum -c --quiet
fi
IMG=og-host-isobuilder:trixie
if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  docker build -q -t "$IMG" - <<'DOCKERFILE'
FROM debian:trixie
RUN apt-get update && apt-get install -y --no-install-recommends mmdebstrap squashfs-tools xorriso mtools dosfstools \
    grub-pc-bin grub-efi-amd64-bin grub-common ca-certificates zstd && rm -rf /var/lib/apt/lists/*
DOCKERFILE
fi
mkdir -p iso/out
docker run --rm --privileged -e VERSION="$VERSION" -e OWNER="$(id -u):$(id -g)" \
  -v "$PWD":/src:ro -v "$PWD/iso/out":/out -v og-host-apt-cache:/var/cache/apt/archives \
  "$IMG" sh /src/iso/inside.sh
ls -la "iso/out/og-host-$VERSION-amd64.iso"
