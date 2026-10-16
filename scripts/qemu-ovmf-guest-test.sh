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
# The outer firmware boots only the enlil ESP (bootindex=0): the guest's disk
# holds a bootable ESP of its own, and must not be picked up by the outer VM.
#
# With ENLIL_OVMF_LINUX=1 the guest also gets a virtual disk holding an ESP with
# Limine (the user's real bootloader) and the test Linux kernel + initramfs:
# OVMF boots Limine from the disk, Limine boots Linux, and init's markers come
# back through the guest serial console - the same path Windows' boot manager
# takes on its own ESP.
#
# Environment:
#   ENLIL_OVMF_LINUX    1: boot Linux from the guest's virtual disk (see above;
#                       expects "ENLIL-GUEST-TICK 3" unless ENLIL_OVMF_EXPECT is set)
#   ENLIL_OVMF_DISK_MODEL  virtio (default) or nvme: the controller the guest's
#                       virtual disk appears on (Windows needs nvme)
#   ENLIL_OVMF_CMDLINE  the guest kernel command line, replacing the legacy-PIC one
#                       (acpi=off noapic nolapic) the direct-boot guest uses
#   ENLIL_OVMF_EXPECT   text that must appear in the guest console (default:
#                       "BdsDxe: No bootable option" - OVMF ran PEI and DXE and
#                       reached its boot manager; distro OVMF has no built-in
#                       shell, so with no disk this is where it ends up)
#   ENLIL_OVMF_GUEST_CODE / ENLIL_OVMF_GUEST_VARS  the firmware the *guest* runs
#                       (default: the same distro OVMF the outer VM uses; a DEBUG
#                       build from scripts/build-ovmf-debug.sh logs on port 0x402,
#                       shown here as "[ovmf-dbg]" lines)
#   ENLIL_OVMF_ESP_DIR  extra files copied into the guest-visible FAT image,
#                       if a guest ESP (EFI/BOOT/BOOTX64.EFI ...) should exist
#   ENLIL_QEMU, ENLIL_OVMF_CODE, ENLIL_OVMF_VARS   as scripts/qemu-boot-test.sh
#   ENLIL_OVMF_TIMEOUT  seconds to wait (default 180)
#
# Usage: scripts/qemu-ovmf-guest-test.sh [output-dir]
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${1:-$(mktemp -d)}"
LINUX="${ENLIL_OVMF_LINUX:-0}"
if [ "$LINUX" = 1 ]; then
    EXPECT="${ENLIL_OVMF_EXPECT:-ENLIL-GUEST-TICK 3}"
else
    EXPECT="${ENLIL_OVMF_EXPECT:-BdsDxe: No bootable option}"
fi
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
DISK_ARGS=()
if [ "$LINUX" = 1 ]; then
    LIMINE_EFI="${ENLIL_LIMINE_EFI:-/usr/share/limine/BOOTX64.EFI}"
    [ -f "$LIMINE_EFI" ] || { echo "SKIP: no Limine EFI binary at $LIMINE_EFI"; exit 2; }
    command -v qemu-img >/dev/null || { echo "SKIP: qemu-img not installed"; exit 2; }
    echo "==> building the guest assets"
    GUEST_DIR="$OUTDIR/guest"
    bash "$REPO_ROOT/scripts/build-test-guest.sh" "$GUEST_DIR" >/dev/null || {
        rc=$?
        [ "$rc" -eq 2 ] && exit 2
        exit 1
    }
    # The guest's ESP: Limine as the removable-media bootloader, configured the
    # way a real install would be (protocol linux, kernel + initramfs by path).
    GESP="$OUTDIR/guest-esp"
    rm -rf "$GESP"
    mkdir -p "$GESP/EFI/BOOT"
    cp "$LIMINE_EFI" "$GESP/EFI/BOOT/BOOTX64.EFI"
    cp "$GUEST_DIR/vmlinuz" "$GUEST_DIR/initrd.img" "$GESP/"
    {
        echo "timeout: 0"
        echo "serial: yes"
        echo ""
        echo "/Enlil test guest"
        echo "    protocol: linux"
        echo "    path: boot():/vmlinuz"
        echo "    cmdline: ${ENLIL_OVMF_CMDLINE:-$(tr -d '\n' < "$GUEST_DIR/cmdline.txt")} ${ENLIL_OVMF_CMDLINE_EXTRA:-}"
        echo "    module_path: boot():/initrd.img"
    } > "$GESP/limine.conf"
    # vvfat builds the FAT image from the directory with no root and no mtools.
    qemu-img convert -f vvfat -O raw "fat:$GESP" "$OUTDIR/guest-disk.img" || { echo "FAIL: vvfat"; exit 1; }
    printf 'nvme-serial = enlilvdisk\nmodel = %s\nallow = all rw\n' "${ENLIL_OVMF_DISK_MODEL:-virtio}" \
        > "$ESP/EFI/enlil/firmware/$GUEST_ID/disk.txt"
    DISK_ARGS=(-drive "file=$OUTDIR/guest-disk.img,if=none,id=vd0,format=raw,cache=writethrough"
               -device "nvme,drive=vd0,serial=enlilvdisk")
fi

SERIAL_LOG="$OUTDIR/serial.log"
cp "$OVMF_VARS" "$OUTDIR/OVMF_VARS.fd"
: > "$SERIAL_LOG"
EXTRA=()
[ -n "${ENLIL_QEMU_EXTRA:-}" ] && read -r -a EXTRA <<< "$ENLIL_QEMU_EXTRA"
echo "==> booting under QEMU+KVM (timeout ${TIMEOUT_SECS}s, waiting for: $EXPECT)"
"$QEMU" \
    -machine q35 -smp 2 -m 2G \
    -enable-kvm -cpu "host,+svm" \
    -drive "format=raw,file=fat:rw:$ESP,if=none,id=espdisk" \
    -device ahci,id=ahci -device "ide-hd,drive=espdisk,bus=ahci.0,bootindex=0" \
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF" \
    -drive "if=pflash,format=raw,file=$OUTDIR/OVMF_VARS.fd" \
    "${DISK_ARGS[@]}" \
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
if grep -aq "^\[ovmf-dbg\]" "$SERIAL_LOG"; then
    echo "---- firmware debug log (last ${ENLIL_OVMF_DBG_TAIL:-40} lines) ----"
    grep -a "^\[ovmf-dbg\]" "$SERIAL_LOG" | cut -c1-200 | tail -${ENLIL_OVMF_DBG_TAIL:-40}
fi
echo "---- hypervisor lines ----"
grep -a "enlil kernel: ovmf" "$SERIAL_LOG" | cut -c1-230 | tail -${ENLIL_OVMF_TAIL:-70}
# The guest's CPUID leaf 0 must carry a real vendor string: a mis-compiled host
# CPUID once returned EBX = 9 for every leaf, and the guest saw no vendor at all.
if ! grep -aq "cpuid leaf 0x0\.0x0 -> 0x[0-9a-f]* 0x\(68747541\|756e6547\) " "$SERIAL_LOG"; then
    echo "FAIL: the guest's CPUID leaf 0 has no vendor string (host CPUID broken?)"
    result=1
fi
if [ "$result" -eq 0 ]; then
    echo "PASS: the guest firmware printed: $EXPECT"
else
    echo "FAIL: the guest did not print: $EXPECT"
fi
echo "serial log: $SERIAL_LOG"
exit "$result"
