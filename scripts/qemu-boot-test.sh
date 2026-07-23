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
# Printed once the kernel drives enlil-platform's BareMetalScheduler (linked in
# now that the crate is no_std under platform-baremetal — the Phase 1.2 payoff):
# four priority-ordered tasks run Critical->Low on real hardware, their closures
# heap-boxed via the kernel's own allocator across the crate boundary.
SCHED_LINE="BareMetalScheduler ran 4 tasks"
# Printed once the kernel builds enlil-platform's MemoryMap from the real
# firmware descriptor array (cross-checked against its own alloc-free summary)
# and carves the hypervisor's DMA/heap/per-guest regions — roadmap 1.3 wiring
# the boot payload's real UEFI map into the platform region carver.
MEMPLAN_LINE="enlil-platform MemoryMap from"
HEAP_RESERVE_LINE="all carved regions clear the live heap"
GUEST_RAM_LINE="guest RAM from the planned region"
# Printed once the kernel feeds its PIT-calibrated TSC frequency to
# enlil-platform's time backend and measures a busy-sleep with the platform
# Instant/Duration — the third no_std enlil-platform module proven on hardware.
PLATFORM_TIME_LINE="enlil-platform Instant measured"
# Printed once the kernel loads its own IDT and takes a breakpoint through it
# — proves interrupt vectoring under enlil's own control.
IDT_LINE="int3 self-test ok"
# Printed once the kernel loads its own GDT + TSS, points #DF at an IST stack,
# and a self-test interrupt confirms the IST switch — fault-handling robustness.
GDT_LINE="IST self-test ok"
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
# Printed once the kernel arms the LAPIC timer in TSC-deadline mode (an absolute
# TSC deadline, not a divided count-down) and its handler runs — the precise
# preemption clock a scheduler quantizes on.
DEADLINE_LINE="TSC-deadline timer fired"
# Printed once the kernel calibrates the TSC against the fixed-rate PIT — the
# monotonic time source the bare-metal kernel runs on.
TSC_LINE="TSC calibrated"
# Printed once the kernel reads a monotonic ns clock across a busy-sleep over
# the calibrated TSC — the bare-metal time source timeouts + scheduler read.
MONOTONIC_LINE="monotonic clock advanced"
# Printed once the kernel arms the TSC-deadline timer at an ns deadline derived
# from the calibrated clock and the monotonic clock confirms it fired on time —
# ns-precise preemption, exactly how the scheduler arms a quantum.
NS_DEADLINE_LINE="ns-precise preemption"
# Printed once the kernel walks the firmware ACPI tables (RSDP→XSDT→MADT) and
# reports the table + enabled-CPU counts — real hardware discovery on metal.
ACPI_LINE="acpi: discovered"
# Printed once the kernel enumerates the enabled processors' APIC IDs from the
# MADT — the AP inventory SMP bring-up (INIT-SIPI-SIPI) targets. Booted with
# -smp 2 so this is a real multi-CPU inventory.
APIC_INVENTORY_LINE="SMP AP inventory"
# Printed once the kernel finds a conventional page below 1 MiB for the AP
# startup trampoline (a SIPI can only start an AP page-aligned in low memory) and
# proves it writable — found from the firmware map, not a hardcoded address.
AP_TRAMPOLINE_LINE="AP trampoline page at"
# Printed once the BSP wakes the application processors with INIT-SIPI-SIPI and
# each one reports in by bumping the trampoline's counter — the machine's other
# CPUs are running enlil's code. Booted -smp 2, so there is exactly one AP.
SMP_LINE="APs started via INIT-SIPI-SIPI"
# Printed once the kernel finds the MCFG and reports the PCIe ECAM base + bus
# range — the window for extended config space + full multi-bus PCI topology.
ECAM_LINE="PCIe ECAM base"
# Printed once the kernel classifies the firmware IOMMU (DMAR=VT-d / IVRS=AMD-Vi
# / none) — Phase 6.4's prerequisite. The VM is given an emulated intel-iommu, so
# this reports real VT-d rather than "none".
IOMMU_LINE="acpi: IOMMU Intel VT-d"
# Printed once the kernel parses the DMAR body and reports each DMA-remapping
# hardware unit's register block — the addresses per-guest DMA isolation is
# programmed through (ROADMAP 6.4). Read from a real firmware DMAR, not a stub.
DMAR_LINE="DMA-remapping unit(s):"
# Printed once the kernel decodes that unit's device-scope entries — which
# hardware the IOMMU governs, and so what can be placed in a per-guest DMA
# domain. A device no unit covers cannot be isolated for passthrough.
DMAR_SCOPE_LINE="scoped to"
# Printed once the kernel enumerates PCI bus 0 via the legacy config mechanism
# (0xCF8/0xCFC), reading the host bridge identity + present-function count.
PCI_LINE="functions on bus 0"
# Printed once the kernel classifies bus-0 functions by PCI base class — the
# storage/network/display inventory driver bring-up + passthrough consume.
PCI_CLASS_LINE="pci: classes"
# Printed once the kernel enumerates the full PCIe topology through the ECAM
# window (all buses, extended config space) — the mechanism passthrough needs.
ECAM_SCAN_LINE="ECAM scan"
# Printed once the kernel reads the first memory BAR on bus 0 — the MMIO window
# a driver / passthrough claims. Read-only decode (base + 32/64-bit), no sizing.
PCI_BAR_LINE="pci: BAR"
# Printed once the kernel builds its own identity page tables and reloads CR3
# off the firmware's — reaching this line proves the map covers the kernel.
PAGING_LINE="off firmware page tables"
SPLIT_LINE="split to 4 KiB pages"
# Printed once the kernel allocates its own stack and unmaps the page below it —
# an overflow now takes a diagnosable #PF instead of corrupting its neighbour.
STACK_LINE="unmapped — overflow faults"
# Printed from the continuation AFTER the permanent switch onto that stack, with
# the RSP it is actually running on: the rest of bring-up (ECAM, the SVM guests,
# the framebuffer) runs on kernel-owned memory, not the firmware's stack.
STACK_SWITCH_LINE="kernel bring-up continues at RSP"
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
# Printed once enlil traps a guest's own #UD (invalid-opcode fault), arms it via
# the VMCB exception-intercept bitmap, and re-injects it into the guest's own
# real-mode IVT handler — exception virtualization (trap + re-deliver a guest fault).
UD_LINE="exception interception works"
# Printed once a guest handles an injected interrupt AND resumes past it: enlil
# injects an interrupt, the guest's IVT handler runs and IRETs, and the guest
# continues — the full inject → handle → IRET → resume cycle a timer tick needs.
IRQ_LINE="interrupt round-trip works"
# Printed once enlil write-protects a guest's NPT leaf, traps the guest's store
# as a present+write nested page fault, grants the write, and the store then
# completes — the dirty-tracking / copy-on-write primitive (live migration).
WP_LINE="NPT dirty-tracking works"
# Printed once the kernel draws to the GOP framebuffer and reads a pixel back
# — proves the framebuffer I/O backend is wired with the firmware gone.
GOP_LINE="framebuffer draw ok"
# Printed once the kernel draws its 8x8 text banner over the framebuffer and
# verifies the blit — visible on-screen output without a serial cable.
GOP_TEXT_LINE="text console banner drawn"
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
    # Two CPUs so the MADT carries a real AP inventory (the kernel enumerates
    # the enabled APIC IDs); the AP stays in wait-for-SIPI until SMP bring-up.
    -smp 2
    # An emulated Intel VT-d IOMMU, so the firmware publishes a real DMAR table
    # with real DMA-remapping units for the kernel to parse (ROADMAP 6.4). QEMU
    # emulates VT-d regardless of the host CPU vendor, so this works on this AMD
    # workstation; enlil's own SVM path is unaffected.
    -device intel-iommu
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
    && grep -q "$SCHED_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$MEMPLAN_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$HEAP_RESERVE_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$GUEST_RAM_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PLATFORM_TIME_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$IDT_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$SPINLOCK_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$GDT_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$APIC_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PERCPU_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$TIMER_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$DEADLINE_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$TSC_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$MONOTONIC_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$NS_DEADLINE_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$ACPI_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$APIC_INVENTORY_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$AP_TRAMPOLINE_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$SMP_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$ECAM_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$IOMMU_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$DMAR_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$DMAR_SCOPE_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PCI_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PCI_CLASS_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$ECAM_SCAN_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PCI_BAR_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$PAGING_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$SPLIT_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$STACK_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$STACK_SWITCH_LINE" "$SERIAL_LOG" 2>/dev/null \
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
    && grep -q "$UD_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$IRQ_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$WP_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$GOP_LINE" "$SERIAL_LOG" 2>/dev/null \
    && grep -q "$GOP_TEXT_LINE" "$SERIAL_LOG" 2>/dev/null; then
    echo "PASS: banner, kernel memory, heap, enlil-platform scheduler, IDT, spinlock, x2APIC, per-CPU GS-base TLS, LAPIC timer, LAPIC TSC-deadline timer, TSC calibration, ACPI discovery incl. real VT-d DMAR remapping units + device scopes, SMP AP bring-up (INIT-SIPI-SIPI), PCI enumeration, own page tables + 4 KiB split + kernel-owned guarded stack (bring-up continues on it), SVM enable + host-save + VMRUN-ready guest + VMRUN to #VMEXIT(HLT) + multi-instruction dispatch loop + IOIO exit-handling + CPUID emulation + in-guest stealth + MSR read/write emulation + NPF demand-paging + native compute loop + VMSAVE/VMLOAD extended-state swap + VMMCALL hypercall + event injection + 64-bit long-mode guest + guest-exception interception (#UD trap + re-inject) + interrupt round-trip (inject/handle/IRET/resume) + NPT write-protection dirty-tracking (trap+grant a guest store), and GOP draw observed on serial"
    exit 0
fi

echo "FAIL: not all of banner/kernel-line/heap/idt/apic/svm/hsave/vmcb/vmrun/dispatch/ioio/cpuid/stealth/msr/msrw/npf/compute/gop observed (qemu rc=$QEMU_RC)"
exit 1
