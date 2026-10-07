#!/usr/bin/env bash
# Build a dd-ready USB disk image carrying the enlil UEFI payload on a FAT ESP.
#
# This is the reproducible image assembly for 1.0's non-destructive USB boot:
# a 64 MiB disk image with a DOS partition table, a single EFI System
# partition (type 0xEF) formatted FAT32, and the freshly built enlil-boot
# payload placed as the default UEFI boot application /EFI/BOOT/BOOTX64.EFI.
# The placed file is read back out of the image and byte-compared against the
# built .efi, so a green run proves the layout, not just the tool exit codes.
#
# Unlike scripts/make-signed-iso.sh (milestone-gated: Secure Boot signing +
# hybrid ISO for physical-hardware test events), this script is unsigned and
# needs only cargo + mtools (+ python3 for the partition table): it is the
# everyday "make me a bootable stick" path.
#
# Usage: scripts/make-esp-image.sh [output-dir]
# Outputs: <out>/esp-usb.img (dd this to a USB stick), <out>/BOOTX64.EFI (the
#          exact payload placed on the image, for inspection).
#
# Write to USB and boot:
#   sudo dd if=<out>/esp-usb.img of=/dev/sdX bs=4M conv=fsync status=progress
#   (sdX = the USB stick; this overwrites it) then pick the USB stick in the
#   target's firmware boot menu. Secure Boot must be OFF (or enroll your own
#   key and sign BOOTX64.EFI yourself); the target needs an AMD CPU with SVM.
#
# Environment overrides:
#   ENLIL_EFI_FILE     use this .efi instead of building enlil-boot (release)
#   ENLIL_TOOLS_PREFIX user-space prefix for mtools (default ~/enlil-tools/root,
#                      see scripts/install-iso-tools.sh)
#
# Exit codes: 0 image built + verified; 1 a step failed; 2 a tool missing.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${1:-$REPO_ROOT/dist}"
TOOLS_PREFIX="${ENLIL_TOOLS_PREFIX:-$HOME/enlil-tools/root}"
IMG_SIZE_MB=64
PART_START_LBA=2048

# mtools is commonly a user-space install (see scripts/install-iso-tools.sh);
# prepend that prefix if present.
if [ -d "$TOOLS_PREFIX/usr/bin" ]; then
    export PATH="$TOOLS_PREFIX/usr/bin:$PATH"
fi

for t in cargo dd python3 mformat mcopy mmd; do
    command -v "$t" >/dev/null 2>&1 || {
        echo "MISSING TOOL: $t — mtools via scripts/install-iso-tools.sh (or: apt install mtools)" >&2
        exit 2
    }
done

mkdir -p "$OUTDIR"

echo "==> [1/3] UEFI payload"
if [ -n "${ENLIL_EFI_FILE:-}" ]; then
    EFI="$ENLIL_EFI_FILE"
    echo "    using prebuilt $EFI"
else
    cargo build -p enlil-boot --release --target x86_64-unknown-uefi \
        --manifest-path "$REPO_ROOT/Cargo.toml"
    EFI="$REPO_ROOT/target/x86_64-unknown-uefi/release/enlil-boot.efi"
fi
[ -f "$EFI" ] || { echo "EFI payload missing: $EFI" >&2; exit 1; }
cp "$EFI" "$OUTDIR/BOOTX64.EFI"
echo "    $(wc -c < "$EFI") bytes -> $OUTDIR/BOOTX64.EFI"

echo "==> [2/3] disk image: DOS MBR + EFI System partition, FAT32 ESP"
IMG="$OUTDIR/esp-usb.img"
dd if=/dev/zero of="$IMG" bs=1M count="$IMG_SIZE_MB" status=none
python3 - "$IMG" "$PART_START_LBA" <<'EOF'
import struct, sys
img, start = sys.argv[1], int(sys.argv[2])
with open(img, 'rb') as f:
    total = len(f.read()) // 512
# One bootable EFI System partition (type 0xEF). The disk signature is fixed
# ("ENLI") so re-runs of this script produce the same partition table.
entry = struct.pack('<B3sB3sII',
                    0x80, b'\x20\x21\x00',       # bootable, start CHS
                    0xEF, b'\xFE\xFF\xFF',       # type EFI System, end CHS
                    start, total - start)
with open(img, 'r+b') as f:
    f.seek(0x1B8); f.write(struct.pack('<I', 0x454E4C49))
    f.seek(0x1BE); f.write(entry)
    f.seek(0x1FE); f.write(b'\x55\xAA')
EOF
PART="$IMG@@$((PART_START_LBA * 512))"
mformat -i "$PART" -F -v ENLIL ::
mmd -i "$PART" ::/EFI ::/EFI/BOOT
mcopy -i "$PART" "$EFI" ::/EFI/BOOT/BOOTX64.EFI

echo "==> [3/3] verify: /EFI/BOOT/BOOTX64.EFI present and byte-identical"
mdir -i "$PART" ::/EFI/BOOT | grep -q "BOOTX64  EFI" \
    || { echo "verify FAILED: /EFI/BOOT/BOOTX64.EFI not on the ESP" >&2; exit 1; }
mcopy -i "$PART" ::/EFI/BOOT/BOOTX64.EFI - | cmp -s - "$EFI" \
    || { echo "verify FAILED: image copy differs from $EFI" >&2; exit 1; }
echo "    /EFI/BOOT/BOOTX64.EFI on the FAT32 ESP, byte-identical to the build"

echo
echo "OK: USB disk image ready: $IMG"
echo "    write it with: sudo dd if=$IMG of=/dev/sdX bs=4M conv=fsync status=progress"
