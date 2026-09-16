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
# Traversable, not readable: the optional Unix listening socket lives at
# /run/lynxrdp/lynxrdp.sock and the user's own sshd process has to search its
# way to it. At 0700 every forward to the documented path failed with EACCES
# before the daemon saw a connection, and nothing logged it. The daemon keeps
# /run/lynxrdp/sessions underneath at 0700 itself, so that is not what this
# bit opens. lynxrdpd.tmpfiles and RuntimeDirectoryMode in the unit say the
# same; all three have to agree, or the next boot or restart undoes this.
chmod 0711 /run/lynxrdp
chmod 0700 /var/log/lynxrdp
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    # dpkg runs "postinst configure <old-version>" for a fresh install and an
    # upgrade alike; only the second argument, empty the first time, tells
    # them apart. rpm passes a count instead: 1 on the first install, 2 or
    # more on an upgrade. Taking the fresh-install branch on an upgrade is
    # not harmless: "enable --now" does nothing to a running unit, so the old
    # daemon stays in memory with the old policy, and because it starts every
    # session through /proc/self/exe -- which reads "(deleted)" once the file
    # has been replaced -- it can no longer start any. It would also re-enable
    # a unit the administrator had deliberately disabled.
    upgrade=no
    case "$1" in
        configure)
            if [ -n "$2" ]; then
                upgrade=yes
            fi
            ;;
        1|"")
            ;;
        *)
            upgrade=yes
            ;;
    esac
    if [ "$upgrade" = yes ]; then
        # Restart the daemon only; running sessions are preserved because
        # the unit uses KillMode=process. try-restart leaves a stopped or
        # disabled unit exactly as it was.
        systemctl try-restart lynxrdpd.service >/dev/null 2>&1 || true
    else
        systemctl enable --now lynxrdpd.service >/dev/null 2>&1 || true
    fi
fi
exit 0
