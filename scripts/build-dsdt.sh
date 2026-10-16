#!/usr/bin/env bash
# Compile the guest machine description (enlil-boot/src/acpi/dsdt.asl) to AML.
#
# The compiled dsdt.aml is committed so building enlil needs no ACPI compiler;
# rerun this after editing the ASL. Needs iasl (ACPICA): from your distro, or the
# one scripts/build-ovmf-debug.sh builds under $ENLIL_EDK2_DIR/acpica.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IASL="${IASL:-$(command -v iasl || true)}"
[ -z "$IASL" ] && IASL="${ENLIL_EDK2_DIR:-$HOME/enlil-edk2}/acpica/generate/unix/bin/iasl"
[ -x "$IASL" ] || { echo "iasl not found (set IASL=/path/to/iasl)" >&2; exit 2; }
DIR="$REPO_ROOT/enlil-boot/src/acpi"
"$IASL" -we -p "$DIR/dsdt" "$DIR/dsdt.asl"
ls -l "$DIR/dsdt.aml"
