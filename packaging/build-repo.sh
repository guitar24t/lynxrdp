#!/bin/bash
# Build the APT and RPM repositories that GitHub Pages serves, from a
# directory of .deb and .rpm files, signing everything with one GPG key.
#
# Usage: packaging/build-repo.sh <packages-dir> <site-dir> --key <fingerprint>
#                                [--public-key <armored-file>]
#
# The result is a static tree: <site>/apt is a Debian archive (pool, dists,
# InRelease), <site>/rpm/el9/<arch> a yum repository (repodata with a
# detached signature on repomd.xml, every package signed), and the public key
# sits at the root in both forms so a host can set the repository up by hand.
#
# --public-key is the ASCII key published on the site and checked against the
# signing key; it defaults to the committed one the server packages install,
# and the fingerprints must agree, because a repository signed with one key
# while the packages hand out another would verify on no machine. CI passes a
# throwaway pair to prove the layout without the real secret.
#
# Needs dpkg-dev, apt-utils, createrepo-c, rpm (for rpmsign) and gnupg. It
# runs on the Ubuntu release runner; the RHEL side is checked by installing
# from the result in an AlmaLinux container, not by building on one.
set -euo pipefail

usage() { echo "usage: $0 <packages-dir> <site-dir> --key <fingerprint> [--public-key <file>]" >&2; exit 2; }

[ $# -ge 2 ] || usage
PACKAGES="$1"; SITE="$2"; shift 2
KEY=""; PUBLIC=""
while [ $# -gt 0 ]; do
    case "$1" in
        --key) KEY="${2:-}"; shift 2 ;;
        --public-key) PUBLIC="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[ -n "$KEY" ] || usage
cd "$(dirname "$0")/.."
PUBLIC="${PUBLIC:-packaging/keys/lynxrdp-packages.asc}"
SITE_URL="https://guitar24t.github.io/lynxrdp"

for tool in dpkg-scanpackages dpkg-deb apt-ftparchive createrepo_c gpg rpm; do
    command -v "$tool" >/dev/null || { echo "$0: $tool is not installed" >&2; exit 1; }
done
# Ubuntu ships the signing entry point as rpmsign; older rpm builds only
# have the rpm --addsign spelling. rpm's default %__gpg names a binary Ubuntu
# does not install, so the one on PATH is passed explicitly.
GPG_BIN="$(command -v gpg)"
if command -v rpmsign >/dev/null; then
    sign_rpm() { rpmsign --define "__gpg $GPG_BIN" --define "_gpg_name $KEY" --addsign "$@"; }
else
    sign_rpm() { rpm --define "__gpg $GPG_BIN" --define "_gpg_name $KEY" --addsign "$@"; }
fi

fingerprint_of() {
    gpg --batch --show-keys --with-colons "$1" | awk -F: '/^fpr/ { print $10; exit }'
}
published="$(fingerprint_of "$PUBLIC")"
if [ "$published" != "$KEY" ]; then
    echo "$0: the signing key $KEY is not the key published in $PUBLIC ($published)" >&2
    exit 1
fi
gpg --batch --list-secret-keys "$KEY" >/dev/null 2>&1 \
    || { echo "$0: no secret key $KEY in the GnuPG keyring" >&2; exit 1; }

# Signatures are made with SHA-256 explicitly: RHEL 9's crypto policy rejects
# SHA-1 signatures outright, and gpg's default depends on its version.
gpg_sign() { gpg --batch --yes --digest-algo SHA256 --local-user "$KEY" "$@"; }

shopt -s nullglob
debs=("$PACKAGES"/*.deb)
rpms=("$PACKAGES"/*.rpm)
[ ${#debs[@]} -gt 0 ] || { echo "$0: no .deb in $PACKAGES" >&2; exit 1; }
[ ${#rpms[@]} -gt 0 ] || { echo "$0: no .rpm in $PACKAGES" >&2; exit 1; }

rm -rf "$SITE"
mkdir -p "$SITE"

# ---- APT ------------------------------------------------------------------
# A conventional pool so several versions of a package can sit side by side;
# --multiversion below lists them all, and apt picks the highest.
apt="$SITE/apt"
mkdir -p "$apt/dists/stable/main"
declare -A deb_arches=()
for deb in "${debs[@]}"; do
    name="$(dpkg-deb -f "$deb" Package)"
    arch="$(dpkg-deb -f "$deb" Architecture)"
    deb_arches["$arch"]=1
    dir="$apt/pool/main/${name:0:1}/$name"
    mkdir -p "$dir"
    cp "$deb" "$dir/"
done
for arch in "${!deb_arches[@]}"; do
    mkdir -p "$apt/dists/stable/main/binary-$arch"
    # Run from the archive root so Filename fields are relative to it, which
    # is what apt joins onto the URI.
    (cd "$apt" && dpkg-scanpackages --multiversion --arch "$arch" pool /dev/null 2>/dev/null) \
        > "$apt/dists/stable/main/binary-$arch/Packages"
    gzip -9 -k -f "$apt/dists/stable/main/binary-$arch/Packages"
done
arch_list="$(printf '%s ' "${!deb_arches[@]}" | sed 's/ $//')"
(cd "$apt" && apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=LynxRDP \
    -o APT::FTPArchive::Release::Label=LynxRDP \
    -o APT::FTPArchive::Release::Suite=stable \
    -o APT::FTPArchive::Release::Codename=stable \
    -o "APT::FTPArchive::Release::Architectures=$arch_list" \
    -o APT::FTPArchive::Release::Components=main \
    -o "APT::FTPArchive::Release::Description=LynxRDP server packages" \
    release dists/stable) > "$apt/dists/stable/Release"
# Both signature forms: apt prefers InRelease and falls back to Release.gpg.
gpg_sign --clearsign --output "$apt/dists/stable/InRelease" "$apt/dists/stable/Release"
gpg_sign --detach-sign --armor --output "$apt/dists/stable/Release.gpg" "$apt/dists/stable/Release"

# ---- RPM ------------------------------------------------------------------
# One repository per architecture, which is what $basearch in the .repo file
# selects. The packages are signed in place after copying so the originals
# (release assets) are left as they were published.
for rpm in "${rpms[@]}"; do
    arch="$(rpm -qp --qf '%{ARCH}' "$rpm" 2>/dev/null)"
    dir="$SITE/rpm/el9/$arch"
    mkdir -p "$dir"
    cp "$rpm" "$dir/"
    sign_rpm "$dir/$(basename "$rpm")" >/dev/null
done
for dir in "$SITE"/rpm/el9/*/; do
    createrepo_c --quiet "$dir"
    gpg_sign --detach-sign --armor --output "$dir/repodata/repomd.xml.asc" "$dir/repodata/repomd.xml"
done

# ---- Keys, definitions, front page ----------------------------------------
cp "$PUBLIC" "$SITE/lynxrdp-packages.asc"
gpg --batch --yes --dearmor --output "$SITE/lynxrdp-archive-keyring.gpg" "$PUBLIC"
cp packaging/repo/lynxrdp.sources "$SITE/lynxrdp.sources"
# The packaged .repo trusts the key file the package installed; the copy on
# the site is for hosts that start from the repository, so it fetches the
# key from the site instead.
sed "s#^gpgkey=.*#gpgkey=$SITE_URL/lynxrdp-packages.asc#" packaging/repo/lynxrdp.repo > "$SITE/lynxrdp.repo"
sed -e "s#@FINGERPRINT@#$(echo "$KEY" | sed 's/..../& /g; s/ $//')#" \
    packaging/repo/index.html > "$SITE/index.html"
# Not a Jekyll site: nothing here needs processing, and Jekyll would drop
# files it considers special.
: > "$SITE/.nojekyll"

echo "repository built in $SITE:"
find "$SITE" -type f | sort | sed 's/^/  /'
