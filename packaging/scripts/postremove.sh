#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi
# Upgrades pass "upgrade" (deb) or "1" (rpm) and match nothing below.
case "$1" in
    remove|purge|0)
        # Nothing listens there any more: preremove ended the daemon and every
        # desktop it had started, so what remains is dead control sockets.
        rm -rf /run/lynxrdp
        ;;
esac
case "$1" in
    purge)
        # dpkg's purge is the request to forget the package entirely, edited
        # PAM file and session logs included. rpm has no equivalent: an erase
        # leaves /var/log/lynxrdp to the administrator, as packages leave
        # their logs, and retires the PAM file from preremove, where the
        # templates it is compared against still exist.
        rm -f /etc/pam.d/lynxrdp
        rm -rf /var/log/lynxrdp
        ;;
esac
exit 0
