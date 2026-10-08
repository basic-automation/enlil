#!/usr/bin/env bash
# Build the enlil-bridge-agent .msi (genuine MSI via msibuild + IDT tables).
# Usage: build-msi.sh <version> <outdir>
#
# The Windows binary is cross-compiled with the x86_64-pc-windows-gnu target.
# It needs a Windows-capable linker: either zig on PATH (used as
# `zig cc -target x86_64-windows-gnu`) or a mingw-w64 cross-gcc. Set ZIG to
# the zig binary when it is not on PATH.
set -euo pipefail

VERSION="$1"
OUTDIR="$2"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v msibuild >/dev/null; then
    echo "error: msibuild not found (install the 'msitools' package)" >&2
    exit 1
fi
if ! command -v gcab >/dev/null; then
    echo "error: gcab not found (install the 'gcab' package)" >&2
    exit 1
fi

# Pick a Windows linker.
LINKER=""
if [ -n "${ZIG:-}" ] && [ -x "$ZIG" ]; then
    LINKER="$ZIG cc -target x86_64-windows-gnu"
elif command -v zig >/dev/null; then
    LINKER="zig cc -target x86_64-windows-gnu"
elif command -v x86_64-w64-mingw32-gcc >/dev/null; then
    LINKER="x86_64-w64-mingw32-gcc"
fi
if [ -z "$LINKER" ]; then
    echo "error: no Windows linker found (need zig or x86_64-w64-mingw32-gcc)" >&2
    exit 1
fi

export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$LINKER"
rustup target add x86_64-pc-windows-gnu >/dev/null 2>&1 || true
cargo build --release --target x86_64-pc-windows-gnu -p bridge-agent

EXE="$ROOT/target/x86_64-pc-windows-gnu/release/bridge-agent.exe"
CFG="$ROOT/bridge-agent/packaging/windows/bridge-agent-windows.toml"
python3 "$ROOT/scripts/build-msi.py" "$VERSION" "$EXE" "$CFG" "$OUTDIR"
