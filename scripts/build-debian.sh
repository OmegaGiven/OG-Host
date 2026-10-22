#!/bin/sh
# Release build of og-hostd + og-shim against Debian 13 (trixie) glibc, in a container,
# so the binaries run on OG-Host's Debian base. Output: target-debian/release/.
set -eu
cd "$(dirname "$0")/.."
IMG=og-host-builder:trixie
if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  docker build -q -t "$IMG" - <<'DOCKERFILE'
FROM rust:1-trixie
RUN apt-get update && apt-get install -y --no-install-recommends clang libelf-dev zlib1g-dev libseccomp-dev pkg-config && rm -rf /var/lib/apt/lists/*
DOCKERFILE
fi
docker run --rm -u "$(id -u):$(id -g)" -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/src/target-debian \
  -v "$PWD":/src -v og-host-cargo:/cargo -w /src "$IMG" \
  cargo build --release --bin og-hostd --bin og-shim "$@"
ls -la target-debian/release/og-hostd target-debian/release/og-shim
