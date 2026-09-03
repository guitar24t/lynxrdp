#!/bin/bash
# Build the Windows installer around an already-built lynxrdp.exe.
#
#   packaging/make-setup-exe.sh <lynxrdp.exe> <output-directory>
#
# Needs makensis (NSIS): `choco install nsis` on Windows, `apt install nsis` on
# Linux (which builds Windows installers just as well), `brew install nsis` on
# macOS. The Windows installer puts it in Program Files rather than on PATH, so
# we look there too.
#
# The installer is not code-signed, so SmartScreen will warn the first time it
# runs; "The installers are not signed" in the README says what a user sees
# and what to click.
set -euo pipefail
EXE="$1"; OUT="$2"
cd "$(dirname "$0")/.."
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"

# The Windows NSIS installer does not put makensis on PATH, so fall back to the
# two places it installs to before giving up.
MAKENSIS="$(command -v makensis || true)"
for candidate in "/c/Program Files (x86)/NSIS/makensis.exe" "/c/Program Files/NSIS/makensis.exe"; do
    if [ -z "$MAKENSIS" ] && [ -x "$candidate" ]; then
        MAKENSIS="$candidate"
    fi
done
[ -n "$MAKENSIS" ] || { echo "makensis not found; see the header of this script" >&2; exit 1; }
[ -f "$EXE" ] || { echo "no such file: $EXE" >&2; exit 1; }

mkdir -p "$OUT"
OUT_ABS="$(cd "$OUT" && pwd)"
# Absolute, because makensis resolves relative paths against the .nsi.
EXE_ABS="$(cd "$(dirname "$EXE")" && pwd)/$(basename "$EXE")"

SETUP="$OUT_ABS/lynxrdp-${VERSION}-windows-x86_64-setup.exe"

# NSIS wants native Windows paths. Under Git Bash the MSYS layer rewrites
# /d/a/... to D:/a/... on its way to makensis.exe, and NSIS reads those forward
# slashes literally -- "no files found" on a file that is plainly there. cygpath
# gives us the D:\a\... form it actually wants. Off Windows there is no cygpath
# and the POSIX path is already native.
towin() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi
}

"$MAKENSIS" -V2 \
    "-DVERSION=$VERSION" \
    "-DEXEPATH=$(towin "$EXE_ABS")" \
    "-DOUTFILE=$(towin "$SETUP")" \
    packaging/windows/lynxrdp.nsi

echo "built $SETUP"
ls -l "$SETUP"
