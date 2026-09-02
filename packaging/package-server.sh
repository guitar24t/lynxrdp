#!/bin/sh
# Build .deb and .rpm packages for the server (and the Linux client).
# Usage: packaging/package-server.sh <amd64|arm64>
# Requires: nfpm on PATH, release binaries in target/release.
set -eu
ARCH="${1:-amd64}"
cd "$(dirname "$0")/.."
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
export VERSION ARCH
mkdir -p dist
for cfg in packaging/nfpm-server.yaml packaging/nfpm-client.yaml; do
    for fmt in deb rpm; do
        nfpm package -f "$cfg" -p "$fmt" -t dist/
    done
done
ls -la dist
