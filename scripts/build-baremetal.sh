#!/usr/bin/env bash
# Build enlil's bare-metal-ready crates for the custom x86_64-unknown-enlil
# kernel target (os=none, no_std + alloc), rebuilding core/alloc from source
# with -Z build-std.
#
# Why a script instead of .cargo/config.toml: `build-std` under
# `[unstable]` in a root .cargo/config.toml applies to EVERY cargo
# invocation, which would force the Linux/Windows dev host to rebuild std
# from source on ordinary `cargo build` too. Encoding the flags here keeps
# the host dev builds fast while giving a one-command bare-metal build.
#
# Exit codes: 0 all listed crates built for the bare-metal target; 1 a build
# failed.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="$REPO_ROOT/x86_64-unknown-enlil.json"

# Crates that are no_std + alloc clean and compile for the bare-metal kernel
# target today. As more of the core graph is ported (enlil-platform's
# baremetal backend, enlil-core, …) add them here — each addition is a
# critical-path increment proven by this script staying green.
BAREMETAL_CRATES=(enlil-hal)

FLAGS=(-Z build-std=core,alloc -Z json-target-spec)

rc=0
for crate in "${BAREMETAL_CRATES[@]}"; do
    echo "==> building $crate for x86_64-unknown-enlil"
    if cargo build -p "$crate" --target "$TARGET" "${FLAGS[@]}" \
            --manifest-path "$REPO_ROOT/Cargo.toml"; then
        echo "OK: $crate"
    else
        echo "FAIL: $crate did not build for x86_64-unknown-enlil"
        rc=1
    fi
done

if [ "$rc" -eq 0 ]; then
    echo "ALL bare-metal crates built for x86_64-unknown-enlil"
fi
exit "$rc"
