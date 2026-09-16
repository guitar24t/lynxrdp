#!/bin/sh
# Build .deb and .rpm packages for the server (and the Linux client).
# Usage: packaging/package-server.sh <amd64|arm64>
# Requires: nfpm and gpg on PATH, release binaries in target/release.
#
# The package version is the Cargo version plus, when LYNXRDP_RELEASE_TAG
# names a candidate (v0.1.0-rc.26), that candidate as nfpm's prerelease, so
# the package is 0.1.0~rc.26-1 rather than 0.1.0-1. dpkg and rpm both rank a
# tilde below the bare version, which is what lets the repository offer each
# candidate over the one before it and the final 0.1.0 over all of them.
# Without the tilde every candidate was 0.1.0-1 and a package manager could
# not tell them apart. A tag whose base disagrees with Cargo.toml fails here
# rather than producing a package whose version lies.
set -eu
ARCH="${1:-amd64}"
cd "$(dirname "$0")/.."
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
PRERELEASE=""
TAG="${LYNXRDP_RELEASE_TAG:-}"
if [ -n "$TAG" ]; then
    bare="${TAG#v}"
    base="${bare%%-*}"
    if [ "$base" != "$VERSION" ]; then
        echo "package-server.sh: release tag $TAG does not match the Cargo version $VERSION" >&2
        exit 1
    fi
    case "$bare" in
        *-*) PRERELEASE="${bare#*-}" ;;
    esac
fi
export VERSION PRERELEASE ARCH
# The apt keyring the server package installs is the committed ASCII key in
# binary form. Deriving it here rather than committing both forms is what
# keeps them from ever disagreeing.
mkdir -p target/packaging dist
gpg --batch --yes --dearmor --output target/packaging/lynxrdp-archive-keyring.gpg \
    packaging/keys/lynxrdp-packages.asc
for cfg in packaging/nfpm-server.yaml packaging/nfpm-client.yaml; do
    for fmt in deb rpm; do
        nfpm package -f "$cfg" -p "$fmt" -t dist/
    done
done
ls -la dist
