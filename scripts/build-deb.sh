#!/usr/bin/env bash
# Build the enlil-bridge-agent .deb with dpkg-deb (no root required).
# Usage: build-deb.sh <version> <outdir>
set -euo pipefail

VERSION="$1"
OUTDIR="$2"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKGDIR="$ROOT/bridge-agent/packaging/deb"
BIN="$ROOT/target/release/bridge-agent"

if [ ! -x "$BIN" ]; then
    echo "error: $BIN not found; run 'cargo build --release -p bridge-agent' first" >&2
    exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
D="$STAGE/enlil-bridge-agent_${VERSION}_amd64"
mkdir -p "$D/DEBIAN" "$D/usr/sbin" "$D/etc/enlil" "$D/lib/systemd/system"

cp "$BIN" "$D/usr/sbin/enlil-bridge-agent"
cp "$PKGDIR/bridge-agent.toml" "$D/etc/enlil/bridge-agent.toml"
cp "$PKGDIR/enlil-bridge-agent.service" "$D/lib/systemd/system/enlil-bridge-agent.service"
sed "s/@VERSION@/${VERSION}/g" "$PKGDIR/control" > "$D/DEBIAN/control"
cp "$PKGDIR/postinst" "$D/DEBIAN/postinst"
cp "$PKGDIR/prerm" "$D/DEBIAN/prerm"
chmod 755 "$D/DEBIAN/postinst" "$D/DEBIAN/prerm"
printf '/etc/enlil/bridge-agent.toml\n' > "$D/DEBIAN/conffiles"

mkdir -p "$OUTDIR"
# dpkg-deb is strict about modes: dirs 755, conffiles 644, scripts 755.
find "$D" -type d -exec chmod 755 {} +
chmod 644 "$D/DEBIAN/control" "$D/DEBIAN/conffiles"
chmod 755 "$D/DEBIAN/postinst" "$D/DEBIAN/prerm"
chmod 755 "$D/usr/sbin/enlil-bridge-agent"
chmod 644 "$D/etc/enlil/bridge-agent.toml" "$D/lib/systemd/system/enlil-bridge-agent.service"
dpkg-deb --build "$D" "$OUTDIR/enlil-bridge-agent_${VERSION}_amd64.deb"
echo "wrote $OUTDIR/enlil-bridge-agent_${VERSION}_amd64.deb"
