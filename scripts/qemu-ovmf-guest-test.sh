#!/usr/bin/env bash
# Boot a guest from UEFI firmware (OVMF) on the bare-metal enlil kernel under
# QEMU+KVM.
#
# Builds enlil-boot (release, x86_64-unknown-uefi), stages it on a virtual ESP
# together with an OVMF code + variable-store pair and a config.toml that asks
# for `[guest.ovmf] boot = "firmware"`, boots QEMU+OVMF with nested SVM, and
# waits for the guest firmware's own console output - relayed by the hypervisor
# as "[ovmf] ..." lines on serial. Windows boots the same way (its boot manager
# lives on an ESP), so this is the path both of the user's OSes need.
#
# Exit codes: 0 the expected guest output appeared; 1 it did not; 2 SKIPPED (no
# /dev/kvm, qemu or OVMF).
#
# Environment:
#   ENLIL_OVMF_EXPECT   text that must appear in the guest console (default:
#                       "BdsDxe: No bootable option" - OVMF ran PEI and DXE and
#                       reached its boot manager; distro OVMF has no built-in
#                       shell, so with no disk this is where it ends up)
#   ENLIL_OVMF_GUEST_CODE / ENLIL_OVMF_GUEST_VARS  the firmware the *guest* runs
#                       (default: the same distro OVMF the outer VM uses)
#   ENLIL_OVMF_ESP_DIR  extra files copied into the guest-visible FAT image,
#                       if a guest ESP (EFI/BOOT/BOOTX64.EFI ...) should exist
#   ENLIL_QEMU, ENLIL_OVMF_CODE, ENLIL_OVMF_VARS   as scripts/qemu-boot-test.sh
#   ENLIL_OVMF_TIMEOUT  seconds to wait (default 180)
#
# Usage: scripts/qemu-ovmf-guest-test.sh [output-dir]
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${1:-$(mktemp -d)}"
EXPECT="${ENLIL_OVMF_EXPECT:-BdsDxe: No bootable option}"
TIMEOUT_SECS="${ENLIL_OVMF_TIMEOUT:-180}"
GUEST_ID="ovmf"

mkdir -p "$OUTDIR"

if [ ! -w /dev/kvm ]; then
    echo "SKIP: /dev/kvm is not writable (a real guest needs nested SVM)"
    exit 2
fi
QEMU="${ENLIL_QEMU:-$(command -v qemu-system-x86_64 || true)}"
OVMF="${ENLIL_OVMF_CODE:-}"
if [ -z "$OVMF" ]; then
    for cand in /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
                /usr/share/edk2/x64/OVMF_CODE.4m.fd /usr/share/edk2-ovmf/x64/OVMF_CODE.fd; do
        [ -f "$cand" ] && { OVMF="$cand"; break; }
    done
fi
OVMF_VARS="${ENLIL_OVMF_VARS:-}"
if [ -z "$OVMF_VARS" ]; then
    for cand in "${OVMF/OVMF_CODE/OVMF_VARS}" "${OVMF/CODE/VARS}"; do
        [ -f "$cand" ] && { OVMF_VARS="$cand"; break; }
    done
fi
if [ -z "$QEMU" ] || [ -z "$OVMF" ] || [ -z "$OVMF_VARS" ]; then
    echo "SKIP: qemu=${QEMU:-missing} ovmf=${OVMF:-missing} vars=${OVMF_VARS:-missing}"
    exit 2
fi
GUEST_CODE="${ENLIL_OVMF_GUEST_CODE:-$OVMF}"
GUEST_VARS="${ENLIL_OVMF_GUEST_VARS:-$OVMF_VARS}"

echo "==> building enlil-boot (release, x86_64-unknown-uefi)"
cargo build -p enlil-boot --release --target x86_64-unknown-uefi \
    --manifest-path "$REPO_ROOT/Cargo.toml" || { echo "FAIL: build"; exit 1; }
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
EFI="$TARGET_DIR/x86_64-unknown-uefi/release/enlil-boot.efi"
[ -f "$EFI" ] || { echo "FAIL: missing $EFI"; exit 1; }

ESP="$OUTDIR/esp"
rm -rf "$ESP"
mkdir -p "$ESP/EFI/BOOT" "$ESP/EFI/enlil/firmware/$GUEST_ID"
cp "$EFI" "$ESP/EFI/BOOT/BOOTX64.EFI"
printf '[guest.%s]\nboot = "firmware"\n' "$GUEST_ID" > "$ESP/EFI/enlil/config.toml"
cp "$GUEST_CODE" "$ESP/EFI/enlil/firmware/$GUEST_ID/OVMF_CODE.fd"
cp "$GUEST_VARS" "$ESP/EFI/enlil/firmware/$GUEST_ID/OVMF_VARS.fd"
[ -f "${ENLIL_OVMF_DISK_TXT:-/nonexistent}" ] && cp "$ENLIL_OVMF_DISK_TXT" "$ESP/EFI/enlil/firmware/$GUEST_ID/disk.txt"

SERIAL_LOG="$OUTDIR/serial.log"
cp "$OVMF_VARS" "$OUTDIR/OVMF_VARS.fd"
: > "$SERIAL_LOG"
EXTRA=()
[ -n "${ENLIL_QEMU_EXTRA:-}" ] && read -r -a EXTRA <<< "$ENLIL_QEMU_EXTRA"
echo "==> booting under QEMU+KVM (timeout ${TIMEOUT_SECS}s, waiting for: $EXPECT)"
"$QEMU" \
    -machine q35 -smp 2 -m 2G \
    -enable-kvm -cpu "host,+svm" \
    -drive "format=raw,file=fat:rw:$ESP" \
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF" \
    -drive "if=pflash,format=raw,file=$OUTDIR/OVMF_VARS.fd" \
    "${EXTRA[@]}" \
    -serial "file:$SERIAL_LOG" -display none -no-reboot &
QEMU_PID=$!

result=1
for _ in $(seq "$TIMEOUT_SECS"); do
    if grep -aqF "$EXPECT" "$SERIAL_LOG" 2>/dev/null \
        && grep -aF "$EXPECT" "$SERIAL_LOG" | grep -aq "^\[ovmf\]"; then result=0; break; fi
    if grep -aq "ovmf: guest stopped" "$SERIAL_LOG" 2>/dev/null; then sleep 1; break; fi
    kill -0 "$QEMU_PID" 2>/dev/null || break
    sleep 1
done
kill "$QEMU_PID" 2>/dev/null; wait "$QEMU_PID" 2>/dev/null

echo "---- guest console (lines relayed by the hypervisor) ----"
grep -a "^\[ovmf\]" "$SERIAL_LOG" | cut -c1-200 | tail -60
echo "---- hypervisor lines ----"
grep -a "enlil kernel: ovmf" "$SERIAL_LOG" | cut -c1-230 | tail -${ENLIL_OVMF_TAIL:-70}
if [ "$result" -eq 0 ]; then
    echo "PASS: the guest firmware printed: $EXPECT"
else
    echo "FAIL: the guest did not print: $EXPECT"
fi
echo "serial log: $SERIAL_LOG"
exit "$result"
