#!/bin/sh
set -e
# Install a PAM service file matching the distribution unless the admin
# already provided one.
if [ ! -e /etc/pam.d/lynxrdp ]; then
    if [ -e /etc/pam.d/common-session ]; then
        cp /usr/share/lynxrdp/pam/lynxrdp.debian /etc/pam.d/lynxrdp
    elif [ -e /etc/pam.d/system-auth ]; then
        cp /usr/share/lynxrdp/pam/lynxrdp.rhel /etc/pam.d/lynxrdp
    else
        echo "lynxrdp: could not detect the PAM layout; copy a file from /usr/share/lynxrdp/pam/ to /etc/pam.d/lynxrdp" >&2
    fi
fi
mkdir -p /run/lynxrdp /var/log/lynxrdp
chmod 0700 /run/lynxrdp /var/log/lynxrdp
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    if [ "$1" = "configure" ] || [ "$1" = "1" ] || [ -z "$1" ]; then
        # Fresh install: enable and start.
        systemctl enable --now lynxrdpd.service >/dev/null 2>&1 || true
    else
        # Upgrade: restart the daemon only; running sessions are preserved
        # because the unit uses KillMode=process.
        systemctl try-restart lynxrdpd.service >/dev/null 2>&1 || true
    fi
fi
exit 0
