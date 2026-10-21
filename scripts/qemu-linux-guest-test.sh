#!/usr/bin/env bash
# Boot a REAL Linux guest on the bare-metal enlil kernel under QEMU+KVM.
#
# Builds enlil-boot (release, x86_64-unknown-uefi), stages it on a virtual ESP
# together with a Linux kernel + initramfs (scripts/build-test-guest.sh), boots
# QEMU+OVMF with nested SVM, and waits for the guest's own console output —
# relayed by the hypervisor as "[linux] ..." lines on serial.
#
# Exit codes: 0 the expected guest output appeared; 1 it did not; 2 SKIPPED
# (no /dev/kvm, qemu, OVMF, or guest assets).
#
# Environment:
#   ENLIL_LINUX_EXPECT  text that must appear in the guest console
#                       (default: "ENLIL-GUEST-INIT-OK", i.e. init ran to the end)
#   ENLIL_GUEST_KERNEL  guest kernel (see build-test-guest.sh)
#   ENLIL_QEMU, ENLIL_OVMF_CODE, ENLIL_OVMF_VARS   as scripts/qemu-boot-test.sh
#   ENLIL_LINUX_TIMEOUT seconds to wait (default 240)
#
# Usage: scripts/qemu-linux-guest-test.sh [output-dir]
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${1:-$(mktemp -d)}"
EXPECT="${ENLIL_LINUX_EXPECT:-ENLIL-GUEST-INIT-OK}"
TIMEOUT_SECS="${ENLIL_LINUX_TIMEOUT:-240}"

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

echo "==> building the guest assets"
GUEST_DIR="$OUTDIR/guest"
bash "$REPO_ROOT/scripts/build-test-guest.sh" "$GUEST_DIR" || {
    rc=$?
    [ "$rc" -eq 2 ] && exit 2
    exit 1
}

echo "==> building enlil-boot (release, x86_64-unknown-uefi)"
cargo build -p enlil-boot --release --target x86_64-unknown-uefi \
    --manifest-path "$REPO_ROOT/Cargo.toml" || { echo "FAIL: build"; exit 1; }
# The workspace may redirect cargo's target dir (build.target-dir); ask cargo
# rather than assuming $REPO_ROOT/target, which could hold a stale binary.
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
EFI="$TARGET_DIR/x86_64-unknown-uefi/release/enlil-boot.efi"
[ -f "$EFI" ] || { echo "FAIL: missing $EFI"; exit 1; }

ESP="$OUTDIR/esp"
rm -rf "$ESP"
mkdir -p "$ESP/EFI/BOOT" "$ESP/EFI/enlil/linux"
cp "$EFI" "$ESP/EFI/BOOT/BOOTX64.EFI"
cp "$GUEST_DIR/vmlinuz" "$GUEST_DIR/initrd.img" "$GUEST_DIR/cmdline.txt" "$ESP/EFI/enlil/linux/"

SERIAL_LOG="$OUTDIR/serial.log"
cp "$OVMF_VARS" "$OUTDIR/OVMF_VARS.fd"
: > "$SERIAL_LOG"
echo "==> booting under QEMU+KVM (timeout ${TIMEOUT_SECS}s, waiting for: $EXPECT)"
"$QEMU" \
    -machine q35 -smp 2 -m 2G \
    -enable-kvm -cpu "host,+svm" \
    -drive "format=raw,file=fat:rw:$ESP" \
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF" \
    -drive "if=pflash,format=raw,file=$OUTDIR/OVMF_VARS.fd" \
    -serial "file:$SERIAL_LOG" -display none -no-reboot &
QEMU_PID=$!

result=1
for _ in $(seq "$TIMEOUT_SECS"); do
    if grep -qF "$EXPECT" "$SERIAL_LOG" 2>/dev/null; then result=0; break; fi
    # The runner reports "guest stopped" when the guest ends for any reason.
    if grep -q "linux: guest stopped" "$SERIAL_LOG" 2>/dev/null; then sleep 1; break; fi
    kill -0 "$QEMU_PID" 2>/dev/null || break
    sleep 1
done
kill "$QEMU_PID" 2>/dev/null; wait "$QEMU_PID" 2>/dev/null

echo "---- guest console (lines relayed by the hypervisor) ----"
grep -a "^\[linux\]" "$SERIAL_LOG" | cut -c1-200 | tail -60
echo "---- hypervisor lines ----"
grep -a "enlil kernel: linux:" "$SERIAL_LOG" | cut -c1-220 | tail -20
# Transparency: the guest must believe it runs on real hardware. Linux logs
# "Hypervisor detected" whenever CPUID or DMI says otherwise.
if [ "$result" -eq 0 ] && grep -aq "Hypervisor detected" "$SERIAL_LOG"; then
    echo "FAIL: the guest detected a hypervisor (CPUID/stealth regression)"
    exit 1
fi
if [ "$result" -eq 0 ]; then
    if grep -aq "Booting paravirtualized kernel on bare hardware" "$SERIAL_LOG"; then
        echo "stealth: the guest believes it runs on bare hardware"
    fi
    echo "info: $(grep -ac 'unchecked MSR access error' "$SERIAL_LOG") unchecked-MSR faults in the guest log"
    echo "PASS: the Linux guest printed: $EXPECT"
    exit 0
fi
echo "FAIL: the guest did not print: $EXPECT (serial log: $SERIAL_LOG)"
exit 1
