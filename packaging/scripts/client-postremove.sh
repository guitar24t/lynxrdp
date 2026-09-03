#!/bin/sh
# Drop the entry and icon from the caches again. Same reasoning as
# client-postinstall.sh: best effort, never fatal.
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || :
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || :
fi
exit 0
