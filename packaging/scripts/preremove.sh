#!/bin/sh
set -e
# Upgrades pass "upgrade" (deb) or "1" (rpm) and must change nothing here:
# the new package's postinstall restarts the daemon, and the running desktops
# survive that restart because the unit uses KillMode=process.
#
# On a real removal that same KillMode is a trap. "systemctl stop" ends the
# daemon alone; every supervisor, lynxrdp-session, X server and desktop it
# started stays up in the unit's cgroup with its binaries about to be deleted,
# unreachable (its control socket goes with the runtime directory) and, with
# the default idle_timeout_secs = 0, never ending on its own. The daemon would
# end them itself if it ran with --stop-sessions-on-exit, but the unit
# deliberately does not, so that they survive upgrades. So removal ends them
# here: stop the daemon the normal way, then signal what is left. systemctl
# kill still reaches an inactive unit's cgroup for as long as anything
# remains in it.
unit=lynxrdpd.service

leftovers() {
    # Empty once the cgroup is gone, which happens as soon as its last process
    # exits. Asked of systemd rather than read under /sys/fs/cgroup so the
    # layout of that tree is not this script's concern.
    systemctl show -p ControlGroup --value "$unit" 2>/dev/null
}

end_sessions() {
    # SIGTERM, not SIGKILL: the supervisor closes the PAM (logind) session
    # only from its SIGTERM handler, and lynxrdp-session takes its desktop and
    # X server down in order from its own. Whatever ignores it for ten seconds
    # is killed, which is also what the daemon does to a session that will
    # not stop when asked.
    systemctl kill --kill-who=all --signal=SIGTERM "$unit" >/dev/null 2>&1 || true
    i=0
    while [ "$i" -lt 10 ] && [ -n "$(leftovers)" ]; do
        sleep 1
        i=$((i + 1))
    done
    if [ -n "$(leftovers)" ]; then
        systemctl kill --kill-who=all --signal=SIGKILL "$unit" >/dev/null 2>&1 || true
    fi
}

retire_pam_file() {
    # /etc/pam.d/lynxrdp is not in the manifest -- postinstall copies in one
    # of two templates, whichever matches the distribution -- so rpm does not
    # remove it, and rpm has no purge to ask for that later. Apply rpm's own
    # rule for a config file by hand: still identical to a template, remove
    # it; edited by the administrator, keep it as .rpmsave. This runs here
    # rather than in postremove because the templates are gone by then.
    if [ ! -e /etc/pam.d/lynxrdp ]; then
        return 0
    fi
    installed="$(cksum < /etc/pam.d/lynxrdp)"
    for template in /usr/share/lynxrdp/pam/lynxrdp.debian /usr/share/lynxrdp/pam/lynxrdp.rhel; do
        if [ -e "$template" ] && [ "$(cksum < "$template")" = "$installed" ]; then
            rm -f /etc/pam.d/lynxrdp
            return 0
        fi
    done
    mv -f /etc/pam.d/lynxrdp /etc/pam.d/lynxrdp.rpmsave
    echo "warning: /etc/pam.d/lynxrdp saved as /etc/pam.d/lynxrdp.rpmsave" >&2
}

case "$1" in
    remove|purge|0)
        if [ -d /run/systemd/system ]; then
            systemctl disable --now "$unit" >/dev/null 2>&1 || true
            end_sessions
        fi
        ;;
esac
case "$1" in
    0)
        retire_pam_file
        ;;
esac
exit 0
