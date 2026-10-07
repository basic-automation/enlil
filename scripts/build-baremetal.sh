#!/usr/bin/env bash
# Build enlil's bare-metal-ready crates for the custom x86_64-unknown-enlil
# kernel target (os=none, no_std + alloc), rebuilding core/alloc from source
# with -Z build-std.
#
# Why a script instead of .cargo/config.toml (DECISION, T-1.1 2026-10-07,
# recorded in ROADMAP.md Phase 1.2): `build-std` under `[unstable]` in a root
# .cargo/config.toml applies to EVERY cargo invocation, which would force the
# Linux/Windows dev host to rebuild std from source on ordinary `cargo build`
# too. Encoding the flags here keeps the host dev builds fast while giving a
# one-command bare-metal build. Deliberately NOT revisited per roadmap churn —
# any future .cargo/config.toml wiring must first justify the host-build cost.
#
# Exit codes: 0 all listed crates built for the bare-metal target; 1 a build
# failed.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="$REPO_ROOT/x86_64-unknown-enlil.json"

# Crates that are no_std + alloc clean and compile for the bare-metal kernel
# target today. As more of the core graph is ported (enlil-core, …) add them
# here — each addition is a critical-path increment proven by this script
# staying green.
#
# BAREMETAL_EXTRA_FLAGS carries per-crate extra cargo flags, index-aligned
# with BAREMETAL_CRATES: enlil-platform needs its bare-metal backend feature
# selected (and default features off — platform-linux gates the remaining
# std-only bits such as async_rt::epoll; io and async_rt themselves are
# no_std-clean since T-1.2/T-1.3).
BAREMETAL_CRATES=(enlil-hal enlil-platform)
BAREMETAL_EXTRA_FLAGS=(
    ""                                                # enlil-hal: no_std + alloc, no features at all
    "--no-default-features --features platform-baremetal" # enlil-platform: no_std + alloc backend
)

if [ "${#BAREMETAL_CRATES[@]}" -ne "${#BAREMETAL_EXTRA_FLAGS[@]}" ]; then
    echo "BUG: BAREMETAL_CRATES and BAREMETAL_EXTRA_FLAGS are out of sync" >&2
    exit 1
fi

FLAGS=(-Z build-std=core,alloc -Z json-target-spec)

rc=0
for i in "${!BAREMETAL_CRATES[@]}"; do
    crate="${BAREMETAL_CRATES[$i]}"
    echo "==> building $crate for x86_64-unknown-enlil"
    # Word-split the per-crate extra flags (empty for crates that need none).
    # shellcheck disable=SC2086
    if cargo build -p "$crate" --target "$TARGET" ${BAREMETAL_EXTRA_FLAGS[$i]} "${FLAGS[@]}" \
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
