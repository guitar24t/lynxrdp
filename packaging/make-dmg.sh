#!/bin/bash
# Build the macOS disk image around an already-built LynxRDP.app.
#
#   packaging/make-dmg.sh <LynxRDP.app> <output-directory> [arch-name]
#
# Opening the image shows the application next to an Applications shortcut,
# which is how a Mac user expects to install something.
#
# macOS only: hdiutil exists nowhere else. The image is neither signed nor
# notarised, so Gatekeeper will refuse the first launch; "The installers are
# not signed" in the README says how to get past that.
set -euo pipefail
APP="$1"; OUT="$2"; ARCH="${3:-macos-aarch64}"
cd "$(dirname "$0")/.."
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"

command -v hdiutil >/dev/null || { echo "hdiutil not found; this script only runs on macOS" >&2; exit 1; }
[ -d "$APP" ] || { echo "no such bundle: $APP" >&2; exit 1; }

mkdir -p "$OUT"
OUT_ABS="$(cd "$OUT" && pwd)"
DMG="$OUT_ABS/lynxrdp-${VERSION}-${ARCH}.dmg"

STAGE="$(mktemp -d)/LynxRDP"
mkdir -p "$STAGE"
trap 'rm -rf "$(dirname "$STAGE")"' EXIT
# -R: a bundle is a directory, and its executable bit has to survive.
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
cp LICENSE "$STAGE/LICENSE.txt"

rm -f "$DMG"
hdiutil create \
    -volname "LynxRDP ${VERSION}" \
    -srcfolder "$STAGE" \
    -fs HFS+ \
    -format UDZO \
    -ov \
    "$DMG"

echo "built $DMG"
ls -l "$DMG"
