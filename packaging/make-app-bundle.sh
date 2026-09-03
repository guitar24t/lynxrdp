#!/bin/bash
# Build LynxRDP.app around an already-built client binary.
#
#   packaging/make-app-bundle.sh <binary-path> <output-directory>
#
# The result is a normal macOS application: double-clicking it opens the
# connection manager, it gets a Dock icon and a menu bar, and Finder shows it
# as one item. The command line still works -- the binary inside the bundle is
# the same one, at LynxRDP.app/Contents/MacOS/lynxrdp.
#
# The bundle is not signed or notarised; "The installers are not signed" in the
# README says what Gatekeeper does about that and how a user gets past it.
set -euo pipefail
BIN="$1"; OUT="$2"
cd "$(dirname "$0")/.."
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"

APP="$OUT/LynxRDP.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# macOS `cp` clones rather than copies, and cloning a freshly linked Mach-O
# occasionally fails with EIO on Apple Silicon. Same fallback as
# package-client.sh.
if ! cp "$BIN" "$APP/Contents/MacOS/lynxrdp"; then
    echo "note: cp failed; retrying with a stream copy" >&2
    sleep 1
    cat "$BIN" > "$APP/Contents/MacOS/lynxrdp"
fi
chmod +x "$APP/Contents/MacOS/lynxrdp"
cp assets/lynxrdp.icns "$APP/Contents/Resources/lynxrdp.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>LynxRDP</string>
    <key>CFBundleDisplayName</key>
    <string>LynxRDP</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.guitar24t.lynxrdp</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleExecutable</key>
    <string>lynxrdp</string>
    <key>CFBundleIconFile</key>
    <string>lynxrdp</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <!-- The window is drawn entirely by the application; without this the
         title bar renders in the light appearance even in dark mode. -->
    <key>NSRequiresAquaSystemAppearance</key>
    <false/>
    <key>NSHumanReadableCopyright</key>
    <string>MIT licensed</string>
</dict>
</plist>
PLIST

# PkgInfo is legacy but Finder still reads it, and it costs eight bytes.
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "built $APP"
find "$APP" -type f | sort
