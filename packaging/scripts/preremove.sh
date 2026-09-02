#!/bin/sh
set -e
# On removal (not upgrade) stop and disable the daemon. Upgrades pass "upgrade"
# (deb) or "1" (rpm).
case "$1" in
    remove|purge|0)
        if [ -d /run/systemd/system ]; then
            systemctl disable --now lynxrdpd.service >/dev/null 2>&1 || true
        fi
        ;;
esac
exit 0
