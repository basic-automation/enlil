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
# Printed once the kernel's own heap serves allocations with the firmware
# gone — proves the switching allocator + first post-ExitBootServices alloc.
HEAP_LINE="alloc test ok"
# Printed once the kernel loads its own IDT and takes a breakpoint through it
# — proves interrupt vectoring under enlil's own control.
IDT_LINE="int3 self-test ok"
# Printed once the kernel's bare-metal spinlock passes its acquire/hold-rejects/
# release self-test — the mutual exclusion for shared state under SMP.
SPINLOCK_LINE="spinlock acquire/release self-test ok"
# Printed once the kernel enables the local APIC in x2APIC mode and reads its
# ID — proves interrupt-controller bring-up under enlil's own control.
APIC_LINE="x2APIC enabled"
# Printed once the kernel installs its per-CPU block as the GS-base TLS pointer
# and gs:[0] reads it back — the foundation for SMP per-core data.
PERCPU_LINE="GS-base TLS installed"
# Printed once the kernel arms a one-shot LAPIC timer, enables interrupts, and
# its handler runs — the interrupt-preemption clock every scheduler needs.
TIMER_LINE="LAPIC timer fired"
# Printed once the kernel calibrates the TSC against the fixed-rate PIT — the
# monotonic time source the bare-metal kernel runs on.
TSC_LINE="TSC calibrated"
# Printed once the kernel walks the firmware ACPI tables (RSDP→XSDT→MADT) and
# reports the table + enabled-CPU counts — real hardware discovery on metal.
ACPI_LINE="acpi: discovered"
# Printed once the kernel finds the MCFG and reports the PCIe ECAM base + bus
# range — the window for extended config space + full multi-bus PCI topology.
ECAM_LINE="PCIe ECAM base"
# Printed once the kernel enumerates PCI bus 0 via the legacy config mechanism
# (0xCF8/0xCFC), reading the host bridge identity + present-function count.
PCI_LINE="functions on bus 0"
# Printed once the kernel classifies bus-0 functions by PCI base class — the
# storage/network/display inventory driver bring-up + passthrough consume.
PCI_CLASS_LINE="pci: classes"
# Printed once the kernel enumerates the full PCIe topology through the ECAM
# window (all buses, extended config space) — the mechanism passthrough needs.
ECAM_SCAN_LINE="ECAM scan"
# Printed once the kernel builds its own identity page tables and reloads CR3
# off the firmware's — reaching this line proves the map covers the kernel.
PAGING_LINE="off firmware page tables"
# Printed once the kernel turns on the CPU virtualization extension (SVM on
# this AMD host) — the enable gate for running a guest with VMRUN.
SVM_LINE="svm: enabled"
# Printed once the kernel allocates + programs the host state-save area into
# VM_HSAVE_PA (read back) — the last CPU-state step before VMRUN.
HSAVE_LINE="host-save area at"
# Printed once the kernel assembles a full VMRUN-ready guest — a HLT code page,
# a nested page table identity-mapping the low GiB, and a VMCB programmed to
# enter it (nested-CR3 read back) — everything VMRUN takes but the instruction.
VMCB_LINE="vmcb VMRUN-ready"
# Printed once the kernel actually runs that guest with VMRUN and the guest's
# first instruction (HLT) takes a #VMEXIT back into the hypervisor — the second
# live-boot sub-milestone: bare-metal enlil running a guest under nested SVM.
VMRUN_LINE="guest #VMEXIT HLT"
# Printed once the kernel drives the guest through the real #VMEXIT dispatch
# loop: the guest runs CPUID (intercepted, skipped by the loop) then HLT, so it
# executes more than one instruction with exits routed through the HAL model.
DISPATCH_LINE="dispatch loop runs a multi-instruction guest"
# Printed once the guest's OUT is decoded + emulated through the IOIO #VMEXIT
# path and the captured byte read back — the exit-handling path proven end to
# end (guest OUT 0x80 = 0x42).
IOIO_LINE="IOIO exit-handling path end to end"
# Printed once enlil answers the guest's intercepted CPUID: the guest reads
# leaf-0 EBX (delivered via the GPR shell) and OUTs its low byte, which matches
# the EBX enlil emulated — proving CPUID exit emulation + the shell's host→guest
# delivery. (The shell's guest→exit→guest path is covered by an earlier run's
# BX-carry proof + the GuestGprs layout test.)
CPUID_LINE="CPUID exit emulated"
# Printed once the guest reads CPUID.1:ECX[31] after enlil's stealth and sees 0
# — the in-guest proof that enlil hides the hypervisor-present bit.
STEALTH_LINE="CPUID stealth verified in-guest"
# Printed once enlil answers the guest's intercepted RDMSR: the guest reads the
# intercepted MSR (enlil injects a sentinel, delivered via the GPR shell) and
# OUTs its low byte, which matches the sentinel — proving MSR exit emulation.
MSR_LINE="MSR exit emulated"
# Printed once the guest WRMSRs a value then RDMSRs it back: enlil shadows the
# write (never touching hardware) and returns it, proving per-guest MSR-state
# virtualization.
MSRW_LINE="MSR write virtualized"
# Printed once the guest reads an unmapped GPA, enlil catches the nested page
# fault, demand-maps a page with a sentinel, and resumes — the guest reads the
# sentinel back, proving NPF handling (the basis for demand paging / MMIO).
NPF_LINE="nested page fault handled"
# Printed once the guest runs a native arithmetic loop (a taken branch, no
# #VMEXIT until the OUT) and produces the correct sum — proving near-native
# guest execution under enlil.
COMPUTE_LINE="near-native execution"
# Printed once the guest reads a byte through its GS segment, whose base VMRUN
# never loads — only the run shell's VMLOAD does. Proves the VMSAVE/VMLOAD
# extended-state (FS/GS/TR/LDTR + SYSENTER) swap around VMRUN.
VMLOAD_LINE="VMSAVE/VMLOAD extended-state swap works"
# Printed once the guest issues a VMMCALL hypercall, enlil services it and
# writes the result into guest RAX, and the guest OUTs it — the paravirt
# enlil<->guest hypercall channel.
VMMCALL_LINE="hypercall channel works"
# Printed once enlil arms the VMCB EVENTINJ field, VMRUN injects an interrupt
# before the guest's first instruction, and the guest's real-mode IVT vectors
# it to a handler whose OUT enlil captures — proving event injection.
EVENTINJ_LINE="event injection works"
# Printed once a 64-bit long-mode guest (paging on, CR3 walking its own tables
# through the NPT, L-bit code segment) runs to its OUT — the mode a real
# x86-64 OS boots in.
LONGMODE_LINE="long-mode guest works"
# Printed once the kernel draws to the GOP framebuffer and reads a pixel back
# — proves the framebuffer I/O backend is wired with the firmware gone.
GOP_LINE="framebuffer draw ok"
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
    if grep -q "$GOP_LINE" "$SERIAL_LOG" 2>/dev/null; then
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
    && grep -q "$KERNEL_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$HEAP_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$IDT_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$SPINLOCK_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$APIC_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PERCPU_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$TIMER_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$TSC_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$ACPI_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$ECAM_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PCI_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PCI_CLASS_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$ECAM_SCAN_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PAGING_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$SVM_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$HSAVE_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$VMCB_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$VMRUN_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$DISPATCH_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$IOIO_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$CPUID_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$STEALTH_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$MSR_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$MSRW_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$NPF_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$COMPUTE_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$VMLOAD_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$VMMCALL_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$EVENTINJ_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$LONGMODE_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$GOP_LINE" "$SERIAL_LOG" 2>/dev/null; then
    echo "PASS: banner, kernel memory, heap, IDT, spinlock, x2APIC, per-CPU GS-base TLS, LAPIC timer, TSC calibration, ACPI discovery, PCI enumeration, own page tables, SVM enable + host-save + VMRUN-ready guest + VMRUN to #VMEXIT(HLT) + multi-instruction dispatch loop + IOIO exit-handling + CPUID emulation + in-guest stealth + MSR read/write emulation + NPF demand-paging + native compute loop + VMSAVE/VMLOAD extended-state swap + VMMCALL hypercall + event injection + 64-bit long-mode guest, and GOP draw observed on serial"
    exit 0
fi

echo "FAIL: not all of banner/kernel-line/heap/idt/apic/svm/hsave/vmcb/vmrun/dispatch/ioio/cpuid/stealth/msr/msrw/npf/compute/gop observed (qemu rc=$QEMU_RC)"
exit 1
