#!/usr/bin/env bash
# Build all enlil-bridge-agent installers into dist/.
# Usage: package-bridge-agent.sh
#
# Produces:
#   dist/enlil-bridge-agent_<version>_amd64.deb   (dpkg-deb)
#   dist/enlil-bridge-agent-<version>-1.x86_64.rpm (native RPM writer)
#   dist/enlil-bridge-agent-<version>-x64.msi      (msibuild + IDT tables)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
OUTDIR="$ROOT/dist"

echo "==> enlil-bridge-agent $VERSION"
echo "==> building Linux release binary"
cargo build --release -p bridge-agent

mkdir -p "$OUTDIR"
echo "==> .deb"
bash "$ROOT/scripts/build-deb.sh" "$VERSION" "$OUTDIR"
echo "==> .rpm"
python3 "$ROOT/scripts/build-rpm.py" "$VERSION" "1" \
    "$ROOT/target/release/bridge-agent" "$OUTDIR"
echo "==> .msi"
bash "$ROOT/scripts/build-msi.sh" "$VERSION" "$OUTDIR"

echo "==> dist contents:"
ls -la "$OUTDIR" | grep -E "deb|rpm|msi" || true
