#!/usr/bin/env bash
# Build a DEBUG OVMF whose DEBUG() log goes to the QEMU debug port 0x402.
#
# A guest firmware that is silent in release builds (distro OVMF) can be debugged
# on enlil with this one: the hypervisor relays port 0x402 as "[ovmf-dbg] ..."
# lines (see scripts/qemu-ovmf-guest-test.sh, which prints them). It found why
# OVMF ignored the virtio disk (a PCI subsystem id below 0x40, and an interrupt
# that did not follow the PIIX3 routing). The build also includes a UEFI shell.
#
# Nothing here touches the enlil repository: edk2 and acpica are cloned into
# $ENLIL_EDK2_DIR (default ~/enlil-edk2, about 1 GB) and built there.
#
# Environment:
#   ENLIL_EDK2_DIR        where to clone and build (default ~/enlil-edk2)
#   ENLIL_EDK2_TAG        edk2 release (default edk2-stable202608, what Arch ships)
#   ENLIL_OVMF_DEBUG_MASK PcdDebugPrintErrorLevel (default 0x8040004F: errors,
#                         warnings, info, load and verbose; 0xFFFFFFFF floods the
#                         log so much a guest never reaches PCI in time)
#
# Needs: git, gcc, make, nasm, python3, uuid headers. Builds iasl from source.
# Prints the CODE/VARS paths to pass as ENLIL_OVMF_GUEST_CODE / ENLIL_OVMF_GUEST_VARS.
set -euo pipefail

DIR="${ENLIL_EDK2_DIR:-$HOME/enlil-edk2}"
TAG="${ENLIL_EDK2_TAG:-edk2-stable202608}"
MASK="${ENLIL_OVMF_DEBUG_MASK:-0x8040004F}"
mkdir -p "$DIR/tmp"
cd "$DIR"
# Keep compiler temp files off a small /tmp.
export TMPDIR="$DIR/tmp"

for tool in git gcc make nasm python3; do
    command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 2; }
done

[ -d acpica ] || git clone --depth 1 https://github.com/acpica/acpica acpica
[ -x acpica/generate/unix/bin/iasl ] || make -C acpica -j"$(nproc)" iasl
export PATH="$DIR/acpica/generate/unix/bin:$PATH"

[ -d edk2 ] || git clone --depth 1 --branch "$TAG" https://github.com/tianocore/edk2 edk2
cd edk2
# Top-level submodules only: the nested ones (OpenSSL's fuzz corpora, krb5, ...)
# are test-only and make a recursive fetch take hours.
git submodule update --init --depth 1
export PYTHON_COMMAND=python3
[ -x BaseTools/Source/C/bin/GenFw ] || make -C BaseTools -j"$(nproc)"

sed -i "s/PcdDebugPrintErrorLevel|0x[0-9A-Fa-f]*/PcdDebugPrintErrorLevel|$MASK/" OvmfPkg/OvmfPkgX64.dsc

set +u
# shellcheck disable=SC1091
. ./edksetup.sh
set -u
build -a X64 -t GCC -b DEBUG -p OvmfPkg/OvmfPkgX64.dsc -n "$(nproc)" \
    -D FD_SIZE_4MB -D SECURE_BOOT_ENABLE=FALSE -D TPM2_ENABLE=FALSE \
    -D NETWORK_ENABLE=FALSE -D SMM_REQUIRE=FALSE -D BUILD_SHELL=TRUE

OUT="$PWD/Build/OvmfX64/DEBUG_GCC/FV"
echo
echo "built:"
ls -l "$OUT/OVMF_CODE.fd" "$OUT/OVMF_VARS.fd"
echo
echo "use it as the guest firmware:"
echo "  ENLIL_OVMF_GUEST_CODE=$OUT/OVMF_CODE.fd ENLIL_OVMF_GUEST_VARS=$OUT/OVMF_VARS.fd \\"
echo "      scripts/qemu-ovmf-guest-test.sh"
