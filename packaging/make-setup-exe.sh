#!/bin/bash
# Build the Windows installer around an already-built lynxrdp.exe.
#
#   packaging/make-setup-exe.sh <lynxrdp.exe> <output-directory>
#
# Needs makensis (NSIS). The GitHub windows runners have it; elsewhere it is
# `choco install nsis` on Windows or `apt install nsis` on Linux, which also
# builds Windows installers.
#
# The installer is not code-signed, so SmartScreen will warn the first time it
# runs. docs/INSTALL.md says what a user sees and what to click.
set -euo pipefail
EXE="$1"; OUT="$2"
cd "$(dirname "$0")/.."
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"

command -v makensis >/dev/null || { echo "makensis not found; see the header of this script" >&2; exit 1; }
[ -f "$EXE" ] || { echo "no such file: $EXE" >&2; exit 1; }

mkdir -p "$OUT"
OUT_ABS="$(cd "$OUT" && pwd)"
# Absolute, because makensis resolves relative paths against the .nsi.
EXE_ABS="$(cd "$(dirname "$EXE")" && pwd)/$(basename "$EXE")"

SETUP="$OUT_ABS/lynxrdp-${VERSION}-windows-x86_64-setup.exe"
makensis -V2 \
    "-DVERSION=$VERSION" \
    "-DEXEPATH=$EXE_ABS" \
    "-DOUTFILE=$SETUP" \
    packaging/windows/lynxrdp.nsi

echo "built $SETUP"
ls -l "$SETUP"
