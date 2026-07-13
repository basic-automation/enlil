#!/usr/bin/env bash
# QEMU + OVMF headless boot proof for the enlil UEFI payload (Phase 6.1).
#
# Builds enlil-boot for x86_64-unknown-uefi, drops it as the default UEFI
# boot application (EFI/BOOT/BOOTX64.EFI) on a virtual FAT ESP, boots it under
# QEMU with OVMF firmware, and asserts the payload's serial banner. This is the
# nightly boot proof for a bare-metal increment — a build step is proven by
# BUILDING for the uefi target, and a boot step by BOOTING here and matching
# the expected serial output. It never touches the workstation's boot config.
#
# Exit codes:
#   0  boot proof passed (banner observed on serial)
#   1  build or boot FAILED (banner not observed)
#   2  SKIPPED: qemu-system-x86_64 and/or OVMF firmware not installed
#      (install with: sudo apt-get install -y qemu-system-x86 ovmf, or point
#      ENLIL_QEMU / ENLIL_OVMF_CODE at an unpacked user-space install)
#
# Environment overrides:
#   ENLIL_QEMU       path to qemu-system-x86_64 (or a wrapper script)
#   ENLIL_OVMF_CODE  path to the OVMF code image (OVMF.fd or OVMF_CODE*.fd)
#   ENLIL_OVMF_VARS  path to the matching OVMF vars template; when set (or
#                    auto-derived from a CODE_4M image) firmware is loaded as
#                    split pflash instead of legacy -bios
#
# Usage: scripts/qemu-boot-test.sh [output-dir]
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${1:-$(mktemp -d)}"
BANNER="enlil kernel alive"
# Printed by kernel_entry after the UEFI stage hands off control — proves the
# payload→kernel transition and the kernel's walk of the handed-off memory map.
KERNEL_LINE="enlil kernel: memory:"
TIMEOUT_SECS=60

mkdir -p "$OUTDIR"

# --- Prerequisite probe (honest SKIP rather than a fake pass) ---------------
QEMU="${ENLIL_QEMU:-}"
if [ -z "$QEMU" ]; then
    for cand in \
        "$(command -v qemu-system-x86_64 || true)" \
        "$HOME/qemu-local/bin/qemu-system-x86_64"; do
        if [ -n "$cand" ] && [ -x "$cand" ]; then QEMU="$cand"; break; fi
    done
fi
OVMF="${ENLIL_OVMF_CODE:-}"
if [ -z "$OVMF" ]; then
    for cand in \
        /usr/share/OVMF/OVMF_CODE.fd \
        /usr/share/OVMF/OVMF_CODE_4M.fd \
        /usr/share/OVMF/OVMF.fd \
        /usr/share/ovmf/OVMF.fd \
        /usr/share/qemu/OVMF.fd \
        /usr/share/edk2-ovmf/x64/OVMF_CODE.fd \
        "$HOME/qemu-local/root/usr/share/OVMF/OVMF_CODE_4M.fd"; do
        if [ -f "$cand" ]; then OVMF="$cand"; break; fi
    done
fi
# A CODE-only image needs its VARS template (split pflash); a combined
# OVMF.fd boots via legacy -bios with no vars file.
OVMF_VARS="${ENLIL_OVMF_VARS:-}"
if [ -z "$OVMF_VARS" ] && [[ "$OVMF" == *OVMF_CODE*.fd ]]; then
    cand="${OVMF/OVMF_CODE/OVMF_VARS}"
    # e.g. OVMF_CODE_4M.fd -> OVMF_VARS_4M.fd alongside it
    if [ -f "$cand" ]; then OVMF_VARS="$cand"; fi
fi

if [ -z "$QEMU" ] || [ -z "$OVMF" ]; then
    echo "SKIP: qemu-system-x86_64=${QEMU:-missing} ovmf=${OVMF:-missing}"
    echo "SKIP: install with: sudo apt-get install -y qemu-system-x86 ovmf"
    exit 2
fi

# --- Build the UEFI payload -------------------------------------------------
echo "==> building enlil-boot for x86_64-unknown-uefi"
if ! cargo build -p enlil-boot --target x86_64-unknown-uefi \
        --manifest-path "$REPO_ROOT/Cargo.toml"; then
    echo "FAIL: enlil-boot did not build for x86_64-unknown-uefi"
    exit 1
fi
EFI="$REPO_ROOT/target/x86_64-unknown-uefi/debug/enlil-boot.efi"
if [ ! -f "$EFI" ]; then
    echo "FAIL: expected artifact missing: $EFI"
    exit 1
fi

# --- Lay out the ESP (QEMU virtual-FAT, no image build / no root needed) ----
ESP="$OUTDIR/esp"
rm -rf "$ESP"
mkdir -p "$ESP/EFI/BOOT"
cp "$EFI" "$ESP/EFI/BOOT/BOOTX64.EFI"

# --- Boot headless under QEMU + OVMF, capture serial ------------------------
SERIAL_LOG="$OUTDIR/serial.log"
echo "==> booting under QEMU (OVMF=$OVMF, timeout=${TIMEOUT_SECS}s)"
QEMU_ARGS=(
    -machine q35
    -drive "format=raw,file=fat:rw:$ESP"
    -serial "file:$SERIAL_LOG"
    -display none
    -no-reboot
)
if [ -n "$OVMF_VARS" ]; then
    # Split CODE/VARS firmware: pflash, with a writable per-run vars copy.
    cp "$OVMF_VARS" "$OUTDIR/OVMF_VARS.fd"
    QEMU_ARGS+=(
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF"
        -drive "if=pflash,format=raw,file=$OUTDIR/OVMF_VARS.fd"
    )
else
    QEMU_ARGS+=(-bios "$OVMF")
fi
# Nested-virt acceleration when /dev/kvm is available, so the payload's own
# VMX/SVM backend (Phase 6.2) can run a nested guest under QEMU. Expose the
# virt extension the host actually has (Intel VMX or AMD SVM).
if [ -w /dev/kvm ]; then
    if grep -qw svm /proc/cpuinfo; then
        QEMU_ARGS+=(-enable-kvm -cpu "host,+svm")
    else
        QEMU_ARGS+=(-enable-kvm -cpu "host,+vmx")
    fi
else
    QEMU_ARGS+=(-cpu max)
fi

# The payload halts (hlt loop) after its banner, so QEMU never exits on its
# own: run it in the background and poll the serial log, killing QEMU as soon
# as the banner (or the timeout) arrives instead of always burning the full
# timeout.
: > "$SERIAL_LOG"
"$QEMU" "${QEMU_ARGS[@]}" &
QEMU_PID=$!
QEMU_RC=0
for _ in $(seq "$TIMEOUT_SECS"); do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        wait "$QEMU_PID"
        QEMU_RC=$?
        break
    fi
    if grep -q "$KERNEL_LINE" "$SERIAL_LOG" 2>/dev/null; then
        break
    fi
    sleep 1
done
if kill -0 "$QEMU_PID" 2>/dev/null; then
    kill "$QEMU_PID" 2>/dev/null
    wait "$QEMU_PID" 2>/dev/null
fi

echo "==> serial output:"
sed 's/^/    /' "$SERIAL_LOG" 2>/dev/null || true

if grep -q "$BANNER" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$KERNEL_LINE" "$SERIAL_LOG" 2>/dev/null; then
    echo "PASS: observed banner \"$BANNER\" and kernel line \"$KERNEL_LINE\" on serial"
    exit 0
fi

echo "FAIL: banner \"$BANNER\" + kernel line \"$KERNEL_LINE\" not both observed (qemu rc=$QEMU_RC)"
exit 1
