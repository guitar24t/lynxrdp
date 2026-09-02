#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi
case "$1" in
    purge)
        rm -f /etc/pam.d/lynxrdp
        rm -rf /run/lynxrdp /var/log/lynxrdp
        ;;
esac
exit 0
