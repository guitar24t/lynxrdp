#!/bin/sh
# LynxRDP desktop session launcher.
#
# Runs inside the user's session with DISPLAY and XAUTHORITY set. It starts
# the user's preferred desktop; when the desktop exits the session ends.
#
# Order of preference:
#   1. ~/.lynxrdp/session   (executable script or command line)
#   2. ~/.xsession          (executable)
#   3. $LYNXRDP_DESKTOP     (a command, e.g. exported from /etc/environment)
#   4. The first desktop found from the list below
#   5. xterm

if [ -r /etc/profile ]; then . /etc/profile; fi
if [ -r "$HOME/.profile" ]; then . "$HOME/.profile"; fi

export XDG_SESSION_TYPE=x11
unset WAYLAND_DISPLAY

# Some desktops need a D-Bus session bus; start one if none is available.
if [ -z "$DBUS_SESSION_BUS_ADDRESS" ] && command -v dbus-launch >/dev/null 2>&1; then
    eval "$(dbus-launch --sh-syntax --exit-with-session)"
    export DBUS_SESSION_BUS_ADDRESS
fi

if command -v xrdb >/dev/null 2>&1 && [ -r "$HOME/.Xresources" ]; then
    xrdb -merge "$HOME/.Xresources"
fi

run() {
    echo "lynxrdp: starting desktop: $*" >&2
    exec "$@"
}

if [ -x "$HOME/.lynxrdp/session" ]; then
    run "$HOME/.lynxrdp/session"
elif [ -r "$HOME/.lynxrdp/session" ]; then
    run /bin/sh -c "$(cat "$HOME/.lynxrdp/session")"
fi
if [ -x "$HOME/.xsession" ]; then
    run "$HOME/.xsession"
fi
if [ -n "$LYNXRDP_DESKTOP" ]; then
    run /bin/sh -c "$LYNXRDP_DESKTOP"
fi

# Match GNOME's system data search path without letting spaces or glob
# characters in a directory change the lookup. A subshell keeps IFS and
# globbing unchanged for the desktop we ultimately exec.
has_ubuntu_session() (
    IFS=:
    set -f
    for session_data_dir in ${XDG_DATA_DIRS:-/usr/local/share:/usr/share}; do
        case "$session_data_dir" in
            /*)
                if [ -r "$session_data_dir/gnome-session/sessions/ubuntu.session" ]; then
                    return 0
                fi
                ;;
        esac
    done
    return 1
)

for candidate in \
    "startxfce4" \
    "xfce4-session" \
    "startplasma-x11" \
    "mate-session" \
    "cinnamon-session" \
    "gnome-session" \
    "lxqt-session" \
    "startlxde" \
    "lxsession" \
    "budgie-desktop" \
    "i3" \
    "openbox-session" \
    "icewm-session" \
    "fluxbox"; do
    if command -v "$candidate" >/dev/null 2>&1; then
        case "$candidate" in
            gnome-session)
                # Ubuntu's defaults (including its installed wallpapers) are
                # keyed to ubuntu:GNOME. Generic GNOME selects Adwaita images
                # Ubuntu need not install. Use its own Xorg session identity
                # only where that session exists; RHEL keeps generic GNOME.
                if has_ubuntu_session; then
                    export XDG_SESSION_DESKTOP=ubuntu XDG_CURRENT_DESKTOP=ubuntu:GNOME
                    export GNOME_SHELL_SESSION_MODE=ubuntu
                    run gnome-session --session=ubuntu
                fi
                export XDG_SESSION_DESKTOP=gnome XDG_CURRENT_DESKTOP=GNOME
                ;;
            startxfce4|xfce4-session) export XDG_SESSION_DESKTOP=xfce XDG_CURRENT_DESKTOP=XFCE ;;
            startplasma-x11) export XDG_SESSION_DESKTOP=plasma XDG_CURRENT_DESKTOP=KDE ;;
            mate-session) export XDG_SESSION_DESKTOP=mate XDG_CURRENT_DESKTOP=MATE ;;
            cinnamon-session) export XDG_SESSION_DESKTOP=cinnamon XDG_CURRENT_DESKTOP=X-Cinnamon ;;
        esac
        run "$candidate"
    fi
done

if command -v xterm >/dev/null 2>&1; then
    echo "lynxrdp: no desktop environment found; starting xterm" >&2
    run xterm -geometry 120x40+20+20 -title "LynxRDP: no desktop environment installed"
fi

echo "lynxrdp: no desktop environment or xterm found" >&2
exit 1
