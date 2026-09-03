#!/bin/bash
# Bundle a release client binary into dist/ as a tarball (Unix) or zip (Windows).
# Usage: packaging/package-client.sh <target-triple> <name> <binary-file-name>
set -euo pipefail
TARGET="$1"; NAME="$2"; BIN="$3"
cd "$(dirname "$0")/.."
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
SRC="target/${TARGET}/release/${BIN}"
[ -f "$SRC" ] || SRC="target/release/${BIN}"
mkdir -p dist stage
STAGE="stage/lynxrdp-${VERSION}-${NAME}"
rm -rf "$STAGE"; mkdir -p "$STAGE"
# macOS cp clones the file rather than copying it, and cloning a freshly
# linked Mach-O occasionally fails with EIO on Apple Silicon. Fall back to a
# stream copy, which uses ordinary reads and writes and has no clone path.
if ! cp "$SRC" "$STAGE/$BIN"; then
    echo "note: cp failed; retrying $SRC with a stream copy" >&2
    sleep 1
    cat "$SRC" > "$STAGE/$BIN"
fi
chmod +x "$STAGE/$BIN"
cp README.md LICENSE "$STAGE/" 2>/dev/null || true
case "$NAME" in
    windows-*)
        (cd stage && 7z a -tzip "../dist/lynxrdp-${VERSION}-${NAME}.zip" "$(basename "$STAGE")" >/dev/null)
        ;;
    *)
        tar -C stage -czf "dist/lynxrdp-${VERSION}-${NAME}.tar.gz" "$(basename "$STAGE")"
        ;;
esac
rm -rf stage
ls -la dist
