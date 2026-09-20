#!/bin/sh
# Install the LynxRDP server on a Debian- or RHEL-family host: set up the
# signed package repository, install lynxrdp-server through the package
# manager, enable the service and check that it is really running.
#
#   curl -fsSL https://guitar24t.github.io/lynxrdp/install.sh | sudo sh
#
# Every check that can refuse the install runs before the first change, so a
# host that is turned down is left exactly as it was found. The one that
# surprises people is the desktop check: the package pulls in Xvfb but no
# desktop environment, and a server without one serves every user a bare
# xterm. That is a working install that looks broken, so it is refused up
# front with the command that fixes it. --no-desktop-check is for hosts whose
# users bring their own ~/.lynxrdp/session or ~/.xsession.
#
# POSIX sh on purpose: this is piped into whatever /bin/sh the host has, and
# on Debian and Ubuntu that is dash.
set -eu

SITE="https://guitar24t.github.io/lynxrdp"
# The key every index and package is signed with. It is fetched from the same
# site as this script, so pinning it here is not a second opinion about who
# published it; it catches a truncated or swapped download, nothing more.
FINGERPRINT="26F85CC72E5FFF1C64C789739B3301E96C9BCF7F"
PACKAGE="lynxrdp-server"
SERVICE="lynxrdpd"
PORT=3390

# The session launcher's list, in its order. tools/check-startwm.py fails if
# this and packaging/startwm.sh ever disagree, because a desktop the launcher
# would start but this script does not know would be refused for no reason.
DESKTOPS="startxfce4 xfce4-session startplasma-x11 mate-session cinnamon-session gnome-session lxqt-session startlxde lxsession budgie-desktop i3 openbox-session icewm-session fluxbox"

CHECK_DESKTOP=1
for arg in "$@"; do
    case "$arg" in
        --no-desktop-check) CHECK_DESKTOP=0 ;;
        -h|--help)
            sed -n '2,18p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
            echo "usage: install.sh [--no-desktop-check]"
            exit 0
            ;;
        *) echo "install.sh: unknown option $arg" >&2; exit 2 ;;
    esac
done

say() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf '\nerror: %s\n' "$1" >&2; shift; for line in "$@"; do printf '       %s\n' "$line" >&2; done; exit 1; }

# ---------------------------------------------------------------- checks
# Nothing below this line and above "changes" modifies the host.

[ "$(id -u)" -eq 0 ] || die "this installer must run as root." \
    "Run it again with sudo:" \
    "  curl -fsSL $SITE/install.sh | sudo sh"

[ -r /etc/os-release ] || die "cannot identify this system: /etc/os-release is missing."
# shellcheck disable=SC1091
. /etc/os-release
DISTRO="${PRETTY_NAME:-${ID:-unknown}}"
FAMILY=""
for id in ${ID:-} ${ID_LIKE:-}; do
    case "$id" in
        debian|ubuntu) FAMILY=apt; break ;;
        rhel|centos|fedora) FAMILY=dnf; break ;;
    esac
done
case "$FAMILY" in
    apt) command -v apt-get >/dev/null 2>&1 || die "$DISTRO looks like a Debian-family system but has no apt-get." ;;
    dnf) command -v dnf >/dev/null 2>&1 || die "$DISTRO looks like a RHEL-family system but has no dnf." ;;
    *) die "$DISTRO is not a supported distribution." \
        "Packages exist for the Debian family (Debian, Ubuntu and derivatives)" \
        "and the RHEL family (RHEL, AlmaLinux, Rocky, CentOS Stream, Fedora)." \
        "Elsewhere, build from source: https://github.com/guitar24t/lynxrdp" ;;
esac

case "$(uname -m)" in
    x86_64|amd64|aarch64|arm64) ;;
    *) die "there is no package for the $(uname -m) architecture." \
        "Packages are built for x86_64 and aarch64 only." ;;
esac

# The packages are linked against the RHEL 9 glibc (2.34); an older libc
# would install the package and then fail to start the daemon.
if command -v getconf >/dev/null 2>&1; then
    glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
    if [ -n "$glibc" ]; then
        major="${glibc%%.*}"; minor="${glibc#*.}"; minor="${minor%%.*}"
        if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 34 ]; }; then
            die "$DISTRO has glibc $glibc; the packages need 2.34 or newer." \
                "That means Ubuntu 22.04, Debian 12, RHEL 9 or later."
        fi
    fi
fi

found_desktop=""
for candidate in $DESKTOPS; do
    if command -v "$candidate" >/dev/null 2>&1; then
        found_desktop="$candidate"
        break
    fi
done
if [ -z "$found_desktop" ] && [ "$CHECK_DESKTOP" -eq 1 ]; then
    case "$FAMILY" in
        apt)
            die "no desktop environment is installed, so nothing has been changed." \
                "LynxRDP serves each user a desktop session and installs none itself;" \
                "without one every user would get a bare xterm. Install a desktop first," \
                "then run this installer again. Xfce is light and works well remotely:" \
                "" \
                "  sudo apt update" \
                "  sudo apt install -y xfce4 xfce4-session dbus-x11" \
                "" \
                "Any of these works instead: GNOME (ubuntu-desktop-minimal on Ubuntu," \
                "task-gnome-desktop on Debian), KDE Plasma (kde-plasma-desktop), MATE" \
                "(mate-desktop-environment), Cinnamon, LXQt, LXDE or Budgie." \
                "" \
                "If your users start their own session from ~/.lynxrdp/session or" \
                "~/.xsession, skip this check with: install.sh --no-desktop-check"
            ;;
        dnf)
            if [ "${ID:-}" = "fedora" ]; then
                hint1="  sudo dnf install -y @xfce-desktop-environment"
                hint2="GNOME (@workstation-product-environment) and KDE Plasma"
                hint3="(@kde-desktop-environment) work as well."
            else
                hint1="  sudo dnf install -y epel-release && sudo dnf groupinstall -y Xfce"
                hint2="GNOME needs no extra repository:"
                hint3="  sudo dnf groupinstall -y \"Server with GUI\""
            fi
            die "no desktop environment is installed, so nothing has been changed." \
                "LynxRDP serves each user a desktop session and installs none itself;" \
                "without one every user would get a bare xterm. Install a desktop first," \
                "then run this installer again. Xfce is light and works well remotely:" \
                "" \
                "$hint1" \
                "" \
                "$hint2" \
                "$hint3" \
                "" \
                "If your users start their own session from ~/.lynxrdp/session or" \
                "~/.xsession, skip this check with: install.sh --no-desktop-check"
            ;;
    esac
fi

[ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1 || \
    die "this host is not running systemd, which the $SERVICE service needs." \
        "In a container or chroot, install the package by hand and start" \
        "/usr/bin/lynxrdpd yourself."

say "$DISTRO on $(uname -m); desktop: ${found_desktop:-not checked}"

# ---------------------------------------------------------------- changes

fetch() {
    # fetch <url> <destination>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    say "installing curl"
    case "$FAMILY" in
        apt) apt-get update -q >/dev/null 2>&1 && DEBIAN_FRONTEND=noninteractive apt-get install -y -q curl ca-certificates >/dev/null 2>&1 ;;
        dnf) dnf -y -q install curl >/dev/null 2>&1 ;;
    esac
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# The package manager's transcript is kept and shown only when it fails: a
# successful install is fifty lines of dependencies nobody needs to read, and
# a failed one is exactly what they need.
quietly() {
    if ! "$@" >"$tmp/pm.log" 2>&1; then
        cat "$tmp/pm.log" >&2
        die "the package manager failed; its output is above." "command: $*"
    fi
}

say "setting up the package repository ($SITE)"
fetch "$SITE/lynxrdp-packages.asc" "$tmp/key.asc"
if command -v gpg >/dev/null 2>&1; then
    got="$(gpg --batch --show-keys --with-colons "$tmp/key.asc" 2>/dev/null | awk -F: '/^fpr/ { print $10; exit }')"
    [ "$got" = "$FINGERPRINT" ] || die "the downloaded signing key is not the expected one." \
        "expected $FINGERPRINT" "got      ${got:-nothing readable}"
fi

case "$FAMILY" in
    apt)
        # The site serves the keyring in the binary form Signed-By expects,
        # so a host without gpg can still be set up.
        fetch "$SITE/lynxrdp-archive-keyring.gpg" "$tmp/keyring.gpg"
        fetch "$SITE/lynxrdp.sources" "$tmp/lynxrdp.sources"
        install -D -m 0644 "$tmp/keyring.gpg" /usr/share/keyrings/lynxrdp-archive-keyring.gpg
        # A host that already has the package has the package's own copy of
        # the definition, possibly edited by its administrator; leave it.
        [ -e /etc/apt/sources.list.d/lynxrdp.sources ] || \
            install -D -m 0644 "$tmp/lynxrdp.sources" /etc/apt/sources.list.d/lynxrdp.sources
        # Refresh this source alone: an unrelated broken repository on the
        # host is not a reason to fail here.
        apt-get update -q \
            -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/lynxrdp.sources \
            -o Dir::Etc::sourceparts=- -o APT::Get::List-Cleanup=0 >/dev/null
        ;;
    dnf)
        fetch "$SITE/lynxrdp.repo" "$tmp/lynxrdp.repo"
        # As above: never replace a definition that is already there.
        [ -e /etc/yum.repos.d/lynxrdp.repo ] || \
            install -D -m 0644 "$tmp/lynxrdp.repo" /etc/yum.repos.d/lynxrdp.repo
        ;;
esac

# Packages from before versions came from the release tag are all 0.1.0-1,
# which every package manager ranks above 0.1.0~rc.N. Such a host has to be
# moved onto the repository once, and "install" alone would call it current.
installed=""
case "$FAMILY" in
    apt) installed="$(dpkg-query -W -f='${Version}' "$PACKAGE" 2>/dev/null || true)" ;;
    dnf) installed="$(rpm -q --qf '%{VERSION}-%{RELEASE}' "$PACKAGE" 2>/dev/null || true)"
         case "$installed" in *"not installed"*) installed="" ;; esac ;;
esac

say "installing $PACKAGE"
case "$FAMILY" in
    apt)
        export DEBIAN_FRONTEND=noninteractive
        if [ "$installed" = "0.1.0-1" ]; then
            newest="$(apt-cache madison "$PACKAGE" | awk '{print $3}' | head -1)"
            say "moving the pre-repository build 0.1.0-1 onto $newest"
            quietly apt-get install -y -q --allow-downgrades "$PACKAGE=$newest"
        else
            quietly apt-get install -y -q "$PACKAGE"
        fi
        ;;
    dnf)
        if [ "$installed" = "0.1.0-1" ]; then
            say "moving the pre-repository build 0.1.0-1 onto the repository"
            quietly dnf -y -q downgrade "$PACKAGE"
        else
            quietly dnf -y -q install "$PACKAGE"
        fi
        ;;
esac

say "enabling and starting $SERVICE"
systemctl daemon-reload
systemctl enable --now "$SERVICE" >/dev/null 2>&1 || true

# ---------------------------------------------------------------- verify
# "enable --now" returning is not the daemon running: it can start and die on
# a bad configuration a moment later, so the state is sampled for a while.
ok=0
i=0
while [ "$i" -lt 20 ]; do
    if systemctl is-active --quiet "$SERVICE"; then
        ok=$((ok + 1))
        [ "$ok" -ge 3 ] && break
    else
        ok=0
    fi
    i=$((i + 1))
    sleep 0.5
done
if [ "$ok" -lt 3 ]; then
    systemctl --no-pager --lines=0 status "$SERVICE" >&2 || true
    journalctl --no-pager -u "$SERVICE" -n 25 >&2 2>/dev/null || true
    die "$SERVICE was installed but is not staying up; its last log lines are above."
fi
systemctl is-enabled --quiet "$SERVICE" || warn "$SERVICE is running but not enabled at boot."

listening=unknown
if command -v ss >/dev/null 2>&1; then
    if ss -H -ltn "sport = :$PORT" 2>/dev/null | grep -q .; then
        listening=yes
        if ss -H -ltn "sport = :$PORT" | awk '{print $4}' | grep -qvE '^(127\.|\[::1\])'; then
            die "$SERVICE is listening on a non-loopback address, which it must never do."
        fi
    else
        listening=no
    fi
fi
[ "$listening" = "no" ] && warn "$SERVICE is active but nothing listens on port $PORT; check listen.port in /etc/lynxrdp/lynxrdp.toml."

case "$FAMILY" in
    apt) version="$(dpkg-query -W -f='${Version}' "$PACKAGE")" ;;
    dnf) version="$(rpm -q --qf '%{VERSION}-%{RELEASE}' "$PACKAGE")" ;;
esac

# Clients reach the daemon only through an SSH port forward, so a host
# without sshd has a server nobody can connect to.
if ! systemctl is-active --quiet ssh 2>/dev/null && ! systemctl is-active --quiet sshd 2>/dev/null; then
    case "$FAMILY" in
        apt) warn "no SSH server is running, and clients connect only through SSH: sudo apt install -y openssh-server" ;;
        dnf) warn "no SSH server is running, and clients connect only through SSH: sudo dnf install -y openssh-server && sudo systemctl enable --now sshd" ;;
    esac
fi

# "localhost" is what a host with no configured name answers, and it is the
# one address that is wrong from every client.
host="$(hostname -f 2>/dev/null || hostname)"
case "$host" in localhost|localhost.*|"") host="<this-host>" ;; esac

cat <<EOF

LynxRDP server $version is installed and running.
  service : $SERVICE (active, enabled at boot)
  listens : 127.0.0.1:$PORT (loopback only; clients arrive through SSH)
  desktop : ${found_desktop:-whatever each user's ~/.lynxrdp/session starts}
  config  : /etc/lynxrdp/lynxrdp.toml
  updates : through the package manager, from $SITE

Connect from a LynxRDP client with:  lynxrdp <user>@$host
EOF
