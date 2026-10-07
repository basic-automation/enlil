#!/usr/bin/env bash
# Lay out the Enlil USB ESP tree with per-guest virtual UEFI firmware (Phase 0.3).
#
# Guests get their own virtual UEFI: the stick carries an OVMF CODE + VARS pair
# per guest, staged here from each guest's `[guest.<id>.firmware]` config
# block. The host payload path (BOOTX64.EFI) was already scripted; this adds
# the guest-firmware half of the ESP.
#
# Layout produced (default ./dist/esp):
#   EFI/BOOT/BOOTX64.EFI                     host UEFI payload (enlil-boot)
#   EFI/enlil/config.toml                    hypervisor config (copy)
#   EFI/enlil/firmware/<guest>/OVMF_CODE.fd  per-guest firmware, read-only code
#   EFI/enlil/firmware/<guest>/OVMF_VARS.fd  per-guest variable-store template
#   manifest.txt                             sha256 + sizes + placeholder flags
#
# The path contract (file names, guest-id rules) is shared with
# enlil-boot::firmware; guest ids become FAT directory names, so ids outside
# [A-Za-z0-9_-] (max 32 chars) are rejected before anything is staged.
#
# Firmware sources, in order per guest: the literal config path (relative paths
# resolve against the config file's directory), then $ENLIL_OVMF_CODE /
# $ENLIL_OVMF_VARS, then well-known system locations (/usr/share/OVMF,
# ~/qemu-local). With --allow-placeholders a missing image is replaced by a
# clearly-marked placeholder file (recorded in manifest.txt); without it the
# script fails loudly rather than staging a stick that cannot boot guests.
#
# The VARS file staged here is a TEMPLATE: at guest launch the hypervisor must
# copy it to a writable per-boot vars file. It is never mapped writable in
# place and never shared between running guests.
#
# Usage: scripts/make-guest-esp.sh [--config PATH] [--out DIR] [--allow-placeholders]
#
# Exit codes: 0 staged + verified; 1 a step failed; 2 a tool is missing.
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="$REPO_ROOT/examples/guests-uefi-firmware.toml"
OUTDIR="$REPO_ROOT/dist/esp"
ALLOW_PLACEHOLDERS=0

while [ $# -gt 0 ]; do
    case "$1" in
        --config) CONFIG="$2"; shift 2 ;;
        --out) OUTDIR="$2"; shift 2 ;;
        --allow-placeholders) ALLOW_PLACEHOLDERS=1; shift ;;
        -h | --help)
            sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

for t in cargo sha256sum; do
    command -v "$t" >/dev/null 2>&1 || {
        echo "MISSING TOOL: $t" >&2
        exit 2
    }
done

[ -n "$OUTDIR" ] && [ "$OUTDIR" != "/" ] || { echo "refusing to stage into '$OUTDIR'" >&2; exit 2; }
[ -f "$CONFIG" ] || { echo "config not found: $CONFIG" >&2; exit 1; }
CONFIG_DIR="$(cd "$(dirname "$CONFIG")" && pwd)"

echo "==> reading firmware table from $CONFIG"
TABLE="$(cargo run -q -p enlil-config --example dump-firmware \
    --manifest-path "$REPO_ROOT/Cargo.toml" -- "$CONFIG")" || {
    echo "FAIL: config load/validation failed (see above)" >&2
    exit 1
}
GUESTS="$(printf '%s\n' "$TABLE" | grep -c . || true)"
echo "    guests with firmware: $GUESTS"

rm -rf "$OUTDIR"
mkdir -p "$OUTDIR/EFI/BOOT" "$OUTDIR/EFI/enlil"

echo "==> [1/4] host payload -> EFI/BOOT/BOOTX64.EFI"
EFI_BUILD="$REPO_ROOT/target/x86_64-unknown-uefi/release/enlil-boot.efi"
if cargo build -q -p enlil-boot --release --target x86_64-unknown-uefi \
    --manifest-path "$REPO_ROOT/Cargo.toml" 2>/dev/null \
    && [ -f "$EFI_BUILD" ]; then
    cp "$EFI_BUILD" "$OUTDIR/EFI/BOOT/BOOTX64.EFI"
    echo "    fresh x86_64-unknown-uefi release build"
else
    echo "    (uefi-target build unavailable — falling back)"
    if [ -f "$REPO_ROOT/dist/BOOTX64.EFI" ]; then
        cp "$REPO_ROOT/dist/BOOTX64.EFI" "$OUTDIR/EFI/BOOT/BOOTX64.EFI"
        echo "    WARNING: reusing $REPO_ROOT/dist/BOOTX64.EFI (not rebuilt)"
    else
        echo "FAIL: no BOOTX64.EFI (build failed and dist/BOOTX64.EFI missing)" >&2
        exit 1
    fi
fi

echo "==> [2/4] config -> EFI/enlil/config.toml"
cp "$CONFIG" "$OUTDIR/EFI/enlil/config.toml"

# Resolve one firmware image: literal path (config-relative), then the
# ENLIL_OVMF_* override, then well-known install locations.
# Prints the resolved path, or nothing when unresolvable.
resolve_firmware() { # kind(literal "code"|"vars") path
    local kind="$1" literal="$2" cand=""
    for cand in \
        "$literal" \
        "$CONFIG_DIR/$literal"; do
        [ -f "$cand" ] && { printf '%s' "$cand"; return 0; }
    done
    local envvar="ENLIL_OVMF_CODE"
    [ "$kind" = "vars" ] && envvar="ENLIL_OVMF_VARS"
    cand="${!envvar:-}"
    [ -n "$cand" ] && [ -f "$cand" ] && { printf '%s' "$cand"; return 0; }
    local stem="OVMF_CODE"
    [ "$kind" = "vars" ] && stem="OVMF_VARS"
    for cand in \
        "/usr/share/OVMF/${stem}_4M.fd" \
        "/usr/share/OVMF/${stem}.fd" \
        "$HOME/qemu-local/root/usr/share/OVMF/${stem}_4M.fd"; do
        [ -f "$cand" ] && { printf '%s' "$cand"; return 0; }
    done
    return 1
}

PLACEHOLDERS=0
echo "==> [3/4] per-guest firmware -> EFI/enlil/firmware/<guest>/"
while IFS=$'\t' read -r id code vars; do
    [ -n "$id" ] || continue
    dest="$OUTDIR/EFI/enlil/firmware/$id"
    mkdir -p "$dest"
    for kind in code vars; do
        literal=""; [ "$kind" = "code" ] && literal="$code" || literal="$vars"
        fname="OVMF_CODE.fd"; [ "$kind" = "vars" ] && fname="OVMF_VARS.fd"
        if src="$(resolve_firmware "$kind" "$literal")"; then
            cp "$src" "$dest/$fname"
            echo "    $id/$fname <- $src"
        elif [ "$ALLOW_PLACEHOLDERS" = 1 ]; then
            cat > "$dest/$fname" <<EOF
ENLIL-PLACEHOLDER-FIRMWARE
kind=$fname
guest=$id
This is NOT real firmware. It was staged because no $fname image was found;
a stick carrying it cannot boot a UEFI guest. Install OVMF (or point
ENLIL_OVMF_CODE / ENLIL_OVMF_VARS at real images) and re-run
scripts/make-guest-esp.sh without --allow-placeholders.
EOF
            PLACEHOLDERS=$((PLACEHOLDERS + 1))
            echo "    $id/$fname <- PLACEHOLDER (no $kind image found)"
        else
            echo "FAIL: guest '$id': no $kind firmware image found for '$literal'" >&2
            echo "      (set ENLIL_OVMF_CODE / ENLIL_OVMF_VARS, install OVMF, or re-run with --allow-placeholders)" >&2
            exit 1
        fi
    done
done <<< "$TABLE"

echo "==> [4/4] manifest + verification"
: > "$OUTDIR/manifest.txt"
{
    echo "# Enlil ESP manifest — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "# config: $CONFIG"
    echo "# placeholders: $PLACEHOLDERS"
} >> "$OUTDIR/manifest.txt"
(cd "$OUTDIR" && find EFI -type f | sort | while read -r f; do
    sum="$(sha256sum "$f" | cut -d' ' -f1)"
    size="$(stat -c%s "$f")"
    ph="real"
    head -1 "$f" | grep -q "^ENLIL-PLACEHOLDER-FIRMWARE$" && ph="placeholder"
    printf '%s  %s  %s  %s\n' "$sum" "$size" "$ph" "$f" >> manifest.txt
done)

# Verify the staged tree: payload + config present, every firmware guest has
# both images, and a guest's code and vars are never the same file/content.
[ -s "$OUTDIR/EFI/BOOT/BOOTX64.EFI" ] || { echo "FAIL: BOOTX64.EFI missing/empty" >&2; exit 1; }
[ -f "$OUTDIR/EFI/enlil/config.toml" ] || { echo "FAIL: config.toml missing" >&2; exit 1; }
while IFS=$'\t' read -r id _code _vars; do
    [ -n "$id" ] || continue
    d="$OUTDIR/EFI/enlil/firmware/$id"
    for f in OVMF_CODE.fd OVMF_VARS.fd; do
        [ -s "$d/$f" ] || { echo "FAIL: $id/$f missing/empty" >&2; exit 1; }
    done
    if cmp -s "$d/OVMF_CODE.fd" "$d/OVMF_VARS.fd"; then
        echo "FAIL: $id: OVMF_CODE.fd and OVMF_VARS.fd are identical — vars would corrupt code" >&2
        exit 1
    fi
done <<< "$TABLE"

echo
echo "OK: ESP staged in $OUTDIR"
find "$OUTDIR/EFI" -type f | sort | sed 's/^/    /'
echo "    manifest: $OUTDIR/manifest.txt"
if [ "$PLACEHOLDERS" -gt 0 ]; then
    echo "    WARNING: $PLACEHOLDERS placeholder firmware file(s) staged — not bootable guests"
fi
