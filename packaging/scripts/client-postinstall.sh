#!/bin/sh
# Refresh the desktop caches so the new entry and icon appear without a
# logout. Both tools are optional -- a headless install may not have either --
# so neither failure is allowed to fail the package.
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || :
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || :
fi
exit 0
