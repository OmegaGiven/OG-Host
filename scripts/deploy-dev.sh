#!/bin/sh
# Build (Debian container) and install og-hostd + og-shim on the dev VM; restart it.
set -eu
cd "$(dirname "$0")/.."
HOST="${OG_DEV:-omegagiven@192.168.0.214}"
./scripts/build-debian.sh >/dev/null
scp -q target-debian/release/og-hostd target-debian/release/og-shim "$HOST":og-host/target/release/
ssh "$HOST" 'sudo install og-host/target/release/og-hostd og-host/target/release/og-shim /usr/local/bin/ && sudo systemctl restart og-hostd && sleep 2 && systemctl is-active og-hostd && og-hostd --version'
