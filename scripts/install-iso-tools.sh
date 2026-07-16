#!/usr/bin/env bash
# Install the signing/ISO tools that scripts/make-signed-iso.sh needs, WITHOUT
# root, into a local prefix (default ~/enlil-tools/root): sbsigntool (sbsign/
# sbverify), mtools (mformat/mcopy/mmd), and xorriso — plus their not-already-
# installed dependencies. `openssl` and `cargo` are assumed already present.
#
# Uses `apt-get download` (no root) + `dpkg -x` into the prefix; the base libs
# (libc6/libssl3/libuuid1) are used from the system install. Add the prefix to
# your environment (make-signed-iso.sh does this automatically):
#   export PATH="$HOME/enlil-tools/root/usr/bin:$PATH"
#   export LD_LIBRARY_PATH="$HOME/enlil-tools/root/usr/lib/x86_64-linux-gnu:..."
set -uo pipefail

PREFIX="${ENLIL_TOOLS_PREFIX:-$HOME/enlil-tools/root}"
DEBDIR="$HOME/enlil-tools/deb"
mkdir -p "$PREFIX" "$DEBDIR"
cd "$DEBDIR"

TARGETS="sbsigntool mtools xorriso"

# Full recursive dependency closure, minus virtual + already-installed packages.
CLOSURE="$(apt-cache depends --recurse --no-recommends --no-suggests \
    --no-conflicts --no-breaks --no-replaces --no-enhances $TARGETS 2>/dev/null \
    | grep -v '^ ' | grep -v '^<' | sort -u)"

TO_GET=""
for p in $CLOSURE; do
    apt-cache policy "$p" 2>/dev/null | grep -q 'Candidate:' || continue  # virtual
    dpkg -s "$p" >/dev/null 2>&1 && continue                              # installed
    TO_GET="$TO_GET $p"
done

echo "==> downloading:$TO_GET"
# shellcheck disable=SC2086
[ -n "$TO_GET" ] && apt-get download $TO_GET

echo "==> extracting into $PREFIX"
n=0
for d in *.deb; do
    [ -e "$d" ] || continue
    dpkg -x "$d" "$PREFIX" 2>/dev/null && n=$((n + 1))
done
echo "    extracted $n package(s)"

echo "==> verifying tools resolve"
export PATH="$PREFIX/usr/bin:$PATH"
export LD_LIBRARY_PATH="$PREFIX/usr/lib/x86_64-linux-gnu:$PREFIX/lib/x86_64-linux-gnu:${LD_LIBRARY_PATH:-}"
rc=0
for t in sbsign sbverify mformat mcopy mmd xorriso; do
    if command -v "$t" >/dev/null 2>&1; then
        printf '    %-9s ok\n' "$t"
    else
        printf '    %-9s MISSING\n' "$t"
        rc=1
    fi
done
exit "$rc"
