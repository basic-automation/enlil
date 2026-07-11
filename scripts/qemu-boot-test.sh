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
#      (install with: sudo apt-get install -y qemu-system-x86 ovmf)
#
# Usage: scripts/qemu-boot-test.sh [output-dir]
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${1:-$(mktemp -d)}"
BANNER="enlil kernel alive"
TIMEOUT_SECS=60

mkdir -p "$OUTDIR"

# --- Prerequisite probe (honest SKIP rather than a fake pass) ---------------
QEMU="$(command -v qemu-system-x86_64 || true)"
OVMF=""
for cand in \
    /usr/share/OVMF/OVMF_CODE.fd \
    /usr/share/OVMF/OVMF.fd \
    /usr/share/ovmf/OVMF.fd \
    /usr/share/qemu/OVMF.fd \
    /usr/share/edk2-ovmf/x64/OVMF_CODE.fd; do
    if [ -f "$cand" ]; then OVMF="$cand"; break; fi
done

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
    -bios "$OVMF"
    -drive "format=raw,file=fat:rw:$ESP"
    -serial "file:$SERIAL_LOG"
    -display none
    -no-reboot
)
# Nested-virt acceleration when /dev/kvm is available, so the payload's own
# VMX/SVM backend (Phase 6.2) can run a nested guest under QEMU.
if [ -w /dev/kvm ]; then
    QEMU_ARGS+=(-enable-kvm -cpu "host,+vmx")
else
    QEMU_ARGS+=(-cpu max)
fi

timeout "$TIMEOUT_SECS" "$QEMU" "${QEMU_ARGS[@]}"
QEMU_RC=$?

echo "==> serial output:"
sed 's/^/    /' "$SERIAL_LOG" 2>/dev/null || true

if grep -q "$BANNER" "$SERIAL_LOG" 2>/dev/null; then
    echo "PASS: observed banner \"$BANNER\" on serial"
    exit 0
fi

echo "FAIL: banner \"$BANNER\" not observed (qemu rc=$QEMU_RC)"
exit 1
