#!/bin/bash
# Download the server packages the repository serves: the .deb and .rpm
# assets of the newest releases, walking back from the most recent one and
# stopping at the first tag whose packages carry a tag-derived version.
#
# Usage: packaging/collect-repo-packages.sh <dest-dir> <first-tag> [<count>]
#
# Releases are taken in the order GitHub lists them (newest first), never by
# comparing tag strings: `sort -V` puts v0.1.0 before v0.1.0-rc.1, the
# opposite of what the tags mean, and the tilde ordering that makes the
# packages themselves sort correctly lives in dpkg and rpm, not in the tags.
#
# <first-tag> exists because every release before it shipped packages
# versioned 0.1.0-1, which apt and dnf would rank above every 0.1.0~rc.N-1;
# serving them would make the package manager "upgrade" back to an old build.
# Needs the gh CLI with a token that can read releases.
set -euo pipefail

[ $# -ge 2 ] || { echo "usage: $0 <dest-dir> <first-tag> [<count>]" >&2; exit 2; }
DEST="$1"; FIRST="$2"; COUNT="${3:-5}"
mkdir -p "$DEST"

taken=0
found_first=0
while IFS= read -r tag; do
    [ -n "$tag" ] || continue
    if [ "$taken" -lt "$COUNT" ]; then
        echo "collecting $tag"
        gh release download "$tag" --dir "$DEST" --clobber \
            --pattern 'lynxrdp-server*.deb' --pattern 'lynxrdp-server*.rpm'
        taken=$((taken + 1))
    fi
    if [ "$tag" = "$FIRST" ]; then
        found_first=1
        break
    fi
done < <(gh release list --limit 100 --exclude-drafts --json tagName --jq '.[].tagName')

if [ "$found_first" -ne 1 ]; then
    echo "$0: the first repository release $FIRST is not among the releases; refusing to serve older packages" >&2
    exit 1
fi
[ "$taken" -gt 0 ] || { echo "$0: no releases collected" >&2; exit 1; }
echo "collected the packages of $taken release(s):"
ls -la "$DEST"
