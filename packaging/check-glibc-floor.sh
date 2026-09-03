#!/bin/sh
# Fail if a binary needs a glibc newer than the oldest supported target.
#
# glibc is backward compatible, not forward: a binary built against 2.39
# will not start on 2.34, while one built against 2.34 runs fine on 2.39.
# RHEL 9 ships the oldest glibc we target (2.34), so that is the ceiling
# for what any shipped binary may require.
#
# This matters because the failure is silent until run time and total: the
# dynamic loader refuses the binary outright with
#   version `GLIBC_2.39' not found (required by /usr/bin/lynxrdpd)
# A weak symbol does not save us -- the *version reference* is what the
# loader enforces, and it is not weak. Rust's std pulls in pidfd_spawnp
# and pidfd_getpid at GLIBC_2.39 when it is built on a host that has them,
# which is exactly how this slipped through the first time.
#
# Usage: packaging/check-glibc-floor.sh <binary>...
set -eu
MAX="${LYNXRDP_MAX_GLIBC:-2.34}"
status=0

for bin in "$@"; do
    if [ ! -e "$bin" ]; then
        echo "check-glibc-floor: $bin does not exist" >&2
        status=1
        continue
    fi
    # Every glibc version this binary references, newest last.
    versions="$(objdump -T "$bin" 2>/dev/null \
        | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -u -V || true)"
    if [ -z "$versions" ]; then
        echo "ok    $bin (no versioned glibc references)"
        continue
    fi
    highest="$(printf '%s\n' "$versions" | tail -1)"
    # sort -V puts the larger last; if that is still MAX, nothing exceeds it.
    if [ "$(printf '%s\n%s\n' "$highest" "$MAX" | sort -V | tail -1)" = "$MAX" ]; then
        echo "ok    $bin (needs at most GLIBC_$highest)"
    else
        too_new="$(printf '%s\n' "$versions" | while read -r v; do
            [ "$(printf '%s\n%s\n' "$v" "$MAX" | sort -V | tail -1)" != "$MAX" ] && echo "$v"
        done | tr '\n' ' ')"
        echo "FAIL  $bin needs GLIBC_$highest but the floor is GLIBC_$MAX" >&2
        echo "      versions above the floor: $too_new" >&2
        echo "      symbols responsible:" >&2
        objdump -T "$bin" 2>/dev/null | grep -E "GLIBC_($(printf '%s' "$too_new" \
            | sed 's/ *$//; s/\./\\./g; s/ /|/g'))\)" \
            | awk '{print "        " $NF, $(NF-1)}' | sort -u >&2
        status=1
    fi
done

exit $status
