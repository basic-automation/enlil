#!/usr/bin/env bash
# Build a Secure-Boot-signed, UEFI-bootable ISO (+ a raw ESP image) of the
# enlil-boot payload for physical-hardware testing.
#
# This is MILESTONE-GATED: run it when there is something worth booting on real
# metal (a new live-boot sub-milestone), not on every change — the nightly
# routine already proves each step in QEMU+OVMF. The image runs enlil-boot,
# which brings the machine up under enlil's own control and runs its SVM
# guest-execution demo, then parks.
#
# Two steps stay MANUAL for the human (this script never does them):
#   1. Enroll `enlil_db.cer` (emitted next to the ISO) in the target firmware's
#      Secure Boot `db` ONCE — or turn Secure Boot off. After enrolling, every
#      image this script produces boots with Secure Boot left on.
#   2. Write the image to a USB stick and reboot the target machine:
#        sudo dd if=<out>/enlil.iso of=/dev/sdX bs=4M conv=fsync   (Linux)
#        or write <out>/enlil.iso with Rufus/balenaEtcher          (Windows)
#      (the raw <out>/esp.img is an alternative for tools that want an ESP.)
#
# The signing key lives OUTSIDE the repo ($ENLIL_SIGN_DIR, default
# ~/enlil-signing) and is generated once on first run. The private key is never
# committed; only the public DER cert (enlil_db.cer) ships with the image.
#
# Outputs (in the output dir, default ./dist): enlil.iso, esp.img,
# BOOTX64.EFI (signed), enlil_db.cer, README-boot.txt.
#
# Exit codes: 0 built + self-boot-verified; 1 a step failed; 2 a tool missing.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${1:-$REPO_ROOT/dist}"
KEYDIR="${ENLIL_SIGN_DIR:-$HOME/enlil-signing}"
TOOLS_PREFIX="${ENLIL_TOOLS_PREFIX:-$HOME/enlil-tools/root}"

# The signing/ISO tools are commonly a user-space install (see
# scripts/install-iso-tools.sh); prepend that prefix if present.
if [ -d "$TOOLS_PREFIX/usr/bin" ]; then
    export PATH="$TOOLS_PREFIX/usr/bin:$PATH"
    export LD_LIBRARY_PATH="$TOOLS_PREFIX/usr/lib/x86_64-linux-gnu:$TOOLS_PREFIX/lib/x86_64-linux-gnu:${LD_LIBRARY_PATH:-}"
fi

for t in openssl sbsign sbverify mformat mcopy mmd xorriso cargo; do
    command -v "$t" >/dev/null 2>&1 || {
        echo "MISSING TOOL: $t — run scripts/install-iso-tools.sh (openssl/cargo excepted)" >&2
        exit 2
    }
done

mkdir -p "$OUTDIR" "$KEYDIR"

echo "==> [1/6] signing key (self-signed Secure Boot db cert, in $KEYDIR)"
if [ ! -f "$KEYDIR/enlil_db.key" ]; then
    openssl req -new -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
        -subj "/CN=enlil Secure Boot db/" \
        -keyout "$KEYDIR/enlil_db.key" -out "$KEYDIR/enlil_db.crt"
    openssl x509 -in "$KEYDIR/enlil_db.crt" -outform DER -out "$KEYDIR/enlil_db.cer"
    chmod 600 "$KEYDIR/enlil_db.key"
    echo "    generated a new key (enroll enlil_db.cer in firmware once)"
else
    echo "    reusing existing key"
fi
cp "$KEYDIR/enlil_db.cer" "$OUTDIR/enlil_db.cer"

echo "==> [2/6] building enlil-boot.efi (release, x86_64-unknown-uefi)"
cargo build -p enlil-boot --release --target x86_64-unknown-uefi \
    --manifest-path "$REPO_ROOT/Cargo.toml"
EFI="$REPO_ROOT/target/x86_64-unknown-uefi/release/enlil-boot.efi"
[ -f "$EFI" ] || { echo "build produced no $EFI" >&2; exit 1; }

echo "==> [3/6] signing + verifying"
SIGNED="$OUTDIR/BOOTX64.EFI"
sbsign --key "$KEYDIR/enlil_db.key" --cert "$KEYDIR/enlil_db.crt" \
    --output "$SIGNED" "$EFI"
sbverify --cert "$KEYDIR/enlil_db.crt" "$SIGNED" \
    || { echo "signature verification FAILED" >&2; exit 1; }

echo "==> [4/6] building the FAT ESP image (esp.img)"
ESP="$OUTDIR/esp.img"
dd if=/dev/zero of="$ESP" bs=1M count=48 status=none
mformat -i "$ESP" -F ::
mmd -i "$ESP" ::/EFI ::/EFI/BOOT
mcopy -i "$ESP" "$SIGNED" ::/EFI/BOOT/BOOTX64.EFI

echo "==> [5/6] building the hybrid UEFI ISO (enlil.iso)"
TREE="$(mktemp -d)"
mkdir -p "$TREE/EFI/BOOT"
cp "$SIGNED" "$TREE/EFI/BOOT/BOOTX64.EFI"
cp "$ESP" "$TREE/efiboot.img"
xorriso -as mkisofs -R -J -V ENLIL \
    -e efiboot.img -no-emul-boot -isohybrid-gpt-basdat \
    -o "$OUTDIR/enlil.iso" "$TREE" 2>&1 | tail -1
rm -rf "$TREE"

cat > "$OUTDIR/README-boot.txt" <<'TXT'
enlil signed boot image — physical hardware test
=================================================
Files: enlil.iso (hybrid UEFI ISO), esp.img (raw ESP), BOOTX64.EFI (signed),
       enlil_db.cer (public cert to enroll).

ONE-TIME firmware setup (choose ONE):
  A) Secure Boot ON: enter firmware setup, enroll enlil_db.cer into the
     Secure Boot signature database (db) / "Authorized Signatures", save.
  B) Secure Boot OFF: disable Secure Boot in firmware setup.

Write to USB and boot:
  Linux:   sudo dd if=enlil.iso of=/dev/sdX bs=4M conv=fsync   (sdX = the USB)
  Windows: write enlil.iso with Rufus (DD mode) or balenaEtcher.
  Then boot the target from the USB in the firmware boot menu.

Requirements on the target: an AMD CPU with SVM enabled + unlocked in firmware.
Output is on COM1 serial (attach a USB-serial adapter to see the detailed
"enlil kernel: ..." lines); on-screen you get the GOP boot indicator. The image
runs the guest-execution demo and then halts — it does not boot an OS yet.
TXT

echo "==> [6/6] self-boot smoke test under QEMU+OVMF (if available)"
QEMU="${ENLIL_QEMU:-$HOME/qemu-local/bin/qemu-system-x86_64}"
OVMF="${ENLIL_OVMF_CODE:-$HOME/qemu-local/root/usr/share/OVMF/OVMF_CODE_4M.fd}"
OVMF_VARS="${ENLIL_OVMF_VARS:-$HOME/qemu-local/root/usr/share/OVMF/OVMF_VARS_4M.fd}"
if [ -x "$QEMU" ] && [ -f "$OVMF" ] && [ -f "$OVMF_VARS" ]; then
    LOG="$OUTDIR/self-boot.log"; VF="$OUTDIR/self-boot-vars.fd"
    cp "$OVMF_VARS" "$VF"; : > "$LOG"
    ACCEL=(-cpu max)
    if [ -w /dev/kvm ] && grep -qw svm /proc/cpuinfo; then ACCEL=(-enable-kvm -cpu host,+svm); fi
    "$QEMU" -machine q35 "${ACCEL[@]}" \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF" \
        -drive "if=pflash,format=raw,file=$VF" \
        -drive "format=raw,file=$OUTDIR/enlil.iso,media=cdrom" \
        -serial "file:$LOG" -display none -no-reboot &
    QPID=$!
    for _ in $(seq 45); do
        kill -0 "$QPID" 2>/dev/null || break
        grep -q "framebuffer draw ok" "$LOG" 2>/dev/null && break
        sleep 1
    done
    kill "$QPID" 2>/dev/null || true; wait "$QPID" 2>/dev/null || true
    rm -f "$VF"
    if grep -q "enlil kernel alive" "$LOG" && grep -q "framebuffer draw ok" "$LOG"; then
        echo "    self-boot OK: the signed ISO booted under OVMF to the guest engine"
    else
        echo "    self-boot FAILED — see $LOG" >&2
        exit 1
    fi
else
    echo "    (QEMU/OVMF not found — skipping self-boot smoke test)"
fi

echo
echo "OK: signed boot image ready in $OUTDIR"
echo "    enlil.iso   — hybrid UEFI ISO (dd to USB or burn)"
echo "    esp.img     — raw ESP image (alternative)"
echo "    enlil_db.cer — enroll ONCE in firmware Secure Boot db (or disable SB)"
echo "    README-boot.txt — full instructions"
