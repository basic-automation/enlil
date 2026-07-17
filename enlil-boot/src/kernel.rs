//! Kernel entry: the first enlil code that owns the machine (Phase 6.1→6.2).
//!
//! After `ExitBootServices()` the UEFI stage jumps here with the
//! [`BootHandoff`](crate::handoff::BootHandoff) it collected. This module is
//! the seam between the firmware payload and the enlil kernel proper: once
//! Phase 1.2 wires the `x86_64-unknown-enlil` std build, `kernel_entry`
//! becomes the call into `enlil-platform`'s `baremetal_init` and the full
//! hypervisor graph; today it proves the transition by consuming the handoff
//! with no firmware services left — walking the raw UEFI memory descriptor
//! array and reporting the machine's usable RAM on the serial console.
//!
//! The descriptor walk and number formatting are pure (`no_std`, no `alloc`
//! — the firmware allocator is gone by the time they run) and are unit-tested
//! on the dev host; only the serial output and the `hlt` park are gated to
//! the firmware target.

/// EFI memory types that `enlil-platform`'s `MemoryMap::from_uefi` treats as
/// usable RAM: loader code/data (1, 2), boot-services code/data (3, 4), and
/// conventional memory (7). Kept in sync with that mapping so the boot-time
/// summary agrees with the map the kernel later builds.
const USABLE_EFI_TYPES: [u32; 5] = [1, 2, 3, 4, 7];

/// EFI conventional memory (type 7) — the only type safe to claim for the
/// bootstrap kernel heap. The other usable types still hold live boot state
/// when the kernel enters: the loaded image (1, 2), and the boot stack plus
/// the handed-off memory-map buffer (3, 4).
const EFI_CONVENTIONAL: u32 = 7;

// Field offsets inside an EFI_MEMORY_DESCRIPTOR (UEFI spec §7.2): Type is a
// u32 at +0, PhysicalStart a u64 at +8, NumberOfPages a u64 at +24. The
// firmware-reported descriptor stride may exceed the struct size, which is
// why the walk uses the handoff's descriptor size rather than a Rust struct.
const DESC_TYPE_OFFSET: usize = 0;
const DESC_PHYS_START_OFFSET: usize = 8;
const DESC_NUM_PAGES_OFFSET: usize = 24;
/// Minimum bytes a descriptor must span to contain the fields we read.
const DESC_MIN_SIZE: usize = DESC_NUM_PAGES_OFFSET + 8;

/// Bytes per UEFI page (4 KiB, fixed by the spec).
const UEFI_PAGE_SIZE: u64 = 4096;

/// What the kernel learned from walking the handed-off UEFI memory map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemorySummary {
    /// Total descriptors in the map (whatever their type).
    pub descriptors: usize,
    /// Descriptors of a usable-RAM type.
    pub usable_regions: usize,
    /// Total usable bytes.
    pub usable_bytes: u64,
    /// Size of the largest single usable region in bytes.
    pub largest_usable_bytes: u64,
    /// One past the highest usable physical address (0 if none).
    pub highest_usable_end: u64,
    /// Base of the largest conventional-memory (type 7) region — where the
    /// bootstrap kernel heap goes (see [`EFI_CONVENTIONAL`]).
    pub largest_conventional_base: u64,
    /// Size of that largest conventional region in bytes.
    pub largest_conventional_bytes: u64,
}

impl MemorySummary {
    /// Total usable memory in whole MiB (rounded down).
    #[must_use]
    pub const fn usable_mib(&self) -> u64 {
        self.usable_bytes / (1024 * 1024)
    }

    /// Largest usable region in whole MiB (rounded down).
    #[must_use]
    pub const fn largest_usable_mib(&self) -> u64 {
        self.largest_usable_bytes / (1024 * 1024)
    }
}

/// Read a little-endian `u32` at `offset`, or `None` past the end.
fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(raw.try_into().ok()?))
}

/// Read a little-endian `u64` at `offset`, or `None` past the end.
fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    let raw = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(raw.try_into().ok()?))
}

/// Walk a raw UEFI memory descriptor array (as handed off by
/// `ExitBootServices()`) and summarize the machine's usable RAM.
///
/// `descriptor_size` is the firmware-reported stride between descriptors,
/// which may exceed the descriptor struct itself. A stride too small to hold
/// the fields we read, or an empty map, yields an all-zero summary rather
/// than a panic — the caller decides how to treat a degenerate map.
#[must_use]
pub fn summarize_memory_map(bytes: &[u8], descriptor_size: usize) -> MemorySummary {
    let mut summary = MemorySummary::default();
    if descriptor_size < DESC_MIN_SIZE {
        return summary;
    }

    for desc in bytes.chunks_exact(descriptor_size) {
        let Some(efi_type) = read_u32_le(desc, DESC_TYPE_OFFSET) else {
            break;
        };
        let Some(phys_start) = read_u64_le(desc, DESC_PHYS_START_OFFSET) else {
            break;
        };
        let Some(pages) = read_u64_le(desc, DESC_NUM_PAGES_OFFSET) else {
            break;
        };
        summary.descriptors += 1;

        if pages == 0 || !USABLE_EFI_TYPES.contains(&efi_type) {
            continue;
        }
        let size = pages.saturating_mul(UEFI_PAGE_SIZE);
        summary.usable_regions += 1;
        summary.usable_bytes = summary.usable_bytes.saturating_add(size);
        summary.largest_usable_bytes = summary.largest_usable_bytes.max(size);
        summary.highest_usable_end = summary
            .highest_usable_end
            .max(phys_start.saturating_add(size));
        if efi_type == EFI_CONVENTIONAL && size > summary.largest_conventional_bytes {
            summary.largest_conventional_base = phys_start;
            summary.largest_conventional_bytes = size;
        }
    }
    summary
}

/// Cap on the bootstrap kernel heap.
///
/// Enough for the kernel's own structures while leaving the bulk of a large
/// region free for the later carve into guest RAM / DMA windows (Phase 1.3's
/// `plan_hypervisor_regions`).
pub const BOOT_HEAP_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Below this a region is too small to be worth installing as the heap.
pub const BOOT_HEAP_MIN_BYTES: u64 = 1024 * 1024;

/// How much of a conventional region to claim for the bootstrap heap.
///
/// Returns the whole region up to [`BOOT_HEAP_MAX_BYTES`], or nothing (0)
/// when the region is under [`BOOT_HEAP_MIN_BYTES`].
#[must_use]
pub const fn boot_heap_size(region_bytes: u64) -> u64 {
    if region_bytes < BOOT_HEAP_MIN_BYTES {
        return 0;
    }
    if region_bytes > BOOT_HEAP_MAX_BYTES {
        BOOT_HEAP_MAX_BYTES
    } else {
        region_bytes
    }
}

/// Format `value` as decimal into `buf`, returning the used suffix.
///
/// `no_std`- and alloc-free (the firmware allocator is gone when the kernel
/// formats its first lines). A 20-byte buffer always fits a `u64`.
pub fn format_u64(value: u64, buf: &mut [u8; 20]) -> &str {
    let mut cursor = buf.len();
    let mut rest = value;
    loop {
        cursor -= 1;
        buf[cursor] = b'0' + u8::try_from(rest % 10).unwrap_or(0);
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    // The bytes just written are ASCII digits, so this cannot fail.
    core::str::from_utf8(&buf[cursor..]).unwrap_or("?")
}

/// Format `value` as `0x`-prefixed lowercase hex into `buf`, returning the
/// used suffix. Alloc-free like [`format_u64`]; 18 bytes always fit.
pub fn format_u64_hex(value: u64, buf: &mut [u8; 18]) -> &str {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut cursor = buf.len();
    let mut rest = value;
    loop {
        cursor -= 1;
        buf[cursor] = DIGITS[usize::try_from(rest & 0xF).unwrap_or(0)];
        rest >>= 4;
        if rest == 0 {
            break;
        }
    }
    cursor -= 1;
    buf[cursor] = b'x';
    cursor -= 1;
    buf[cursor] = b'0';
    // The bytes just written are ASCII, so this cannot fail.
    core::str::from_utf8(&buf[cursor..]).unwrap_or("?")
}

#[cfg(target_os = "uefi")]
pub use hw::kernel_entry;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{MemorySummary, boot_heap_size, format_u64, format_u64_hex, summarize_memory_map};
    use crate::allocator::install_kernel_heap;
    use crate::handoff::BootHandoff;
    use crate::serial::SerialPort;
    use alloc::vec::Vec;

    /// The enlil kernel entry point.
    ///
    /// Called by the UEFI stage after `ExitBootServices()` with the collected
    /// [`BootHandoff`]; never returns. Owns the machine from here: brings up
    /// its own serial console, consumes the handed-off memory map, and parks
    /// the CPU. As Phase 6.2 lands this grows into `baremetal_init` +
    /// hypervisor bring-up.
    pub fn kernel_entry(handoff: &BootHandoff) -> ! {
        let serial = SerialPort::com1();
        serial.write_str("enlil kernel: entered via BootHandoff\n");

        if handoff.memory_map_base != 0 && handoff.memory_map_len != 0 {
            // SAFETY: the UEFI stage recorded the base/extent of the final
            // memory map it received from ExitBootServices() and forgot the
            // owning buffer, so the array is live, unaliased, and unmodified.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    handoff.memory_map_base as *const u8,
                    handoff.memory_map_len,
                )
            };
            let summary = summarize_memory_map(bytes, handoff.memory_descriptor_size);
            report_memory(&serial, &summary);
            bring_up_heap(&serial, &summary);
        } else {
            serial.write_str("enlil kernel: memory: NO MAP in handoff\n");
        }

        bring_up_interrupts(&serial);
        bring_up_locks(&serial);
        let apic_id = bring_up_apic(&serial);
        bring_up_percpu(&serial, apic_id.unwrap_or(0));
        bring_up_timer(&serial);
        bring_up_time(&serial);
        bring_up_acpi(&serial, handoff);
        bring_up_pci(&serial);
        bring_up_paging(&serial);
        bring_up_virtualization(&serial);
        bring_up_framebuffer(&serial, handoff);

        park()
    }

    /// Calibrate the TSC against the PIT and report its frequency — the
    /// monotonic time source the bare-metal kernel runs on (ROADMAP 6.2).
    ///
    /// Runs with interrupts masked (the boot path has not enabled them since
    /// `bring_up_timer` re-masked) so nothing perturbs the calibration window.
    fn bring_up_time(serial: &SerialPort) {
        use crate::tsc::hz_to_mhz_rounded;
        match crate::tsc::calibrate_tsc_hz() {
            Some(hz) if hz > 0 => {
                let mut buf = [0u8; 20];
                serial.write_str("enlil kernel: time: TSC calibrated ");
                serial.write_str(format_u64(hz_to_mhz_rounded(hz), &mut buf));
                serial.write_str(" MHz (via PIT) — monotonic clock live\n");
            }
            _ => serial.write_str("enlil kernel: time: TSC calibration FAILED (PIT silent)\n"),
        }
    }

    /// Discover physical hardware from the firmware ACPI tables and report it.
    ///
    /// Walks the handed-off RSDP → XSDT → MADT (the minimal `no_std` walker in
    /// [`crate::acpi`], since the full `enlil-devices` readers are `std`-only)
    /// and reports the table count and enabled-CPU count — the first real
    /// hardware discovery from firmware tables on bare metal (ROADMAP 6.3).
    fn bring_up_acpi(serial: &SerialPort, handoff: &BootHandoff) {
        match crate::acpi::discover(handoff.acpi_rsdp) {
            Some(summary) => {
                let mut t = [0u8; 20];
                let mut c = [0u8; 20];
                serial.write_str("enlil kernel: acpi: discovered ");
                serial.write_str(format_u64(summary.tables as u64, &mut t));
                serial.write_str(" tables, ");
                serial.write_str(format_u64(u64::from(summary.enabled_cpus), &mut c));
                serial.write_str(" enabled CPUs (MADT)\n");
            }
            None => serial.write_str("enlil kernel: acpi: no valid RSDP in handoff\n"),
        }
    }

    /// Install the kernel's own identity page tables and switch `CR3` off the
    /// firmware's (which live in reclaimable boot-services memory) — host
    /// page-table management (ROADMAP 6.2).
    ///
    /// Reaching the report line proves the map is correct: the kernel's code,
    /// stack, heap, ACPI region, and framebuffer are all covered, or the `CR3`
    /// reload would have faulted. Every later step runs on these tables.
    fn bring_up_paging(serial: &SerialPort) {
        // SAFETY: install_identity_map builds a low-4-GiB identity map that
        // covers everything the kernel touches next (sub-4-GiB layout), so the
        // CR3 reload continues execution seamlessly.
        match unsafe { crate::paging::install_identity_map() } {
            Some(cr3) => {
                let mut buf = [0u8; 18];
                serial.write_str("enlil kernel: paging: own identity tables installed, CR3=");
                serial.write_str(format_u64_hex(cr3, &mut buf));
                serial.write_str(" — off firmware page tables\n");
            }
            None => serial.write_str("enlil kernel: paging: page-table build FAILED\n"),
        }
    }

    /// Enumerate PCI bus 0 via the legacy config mechanism and report it — the
    /// device discovery every passthrough/IOMMU step builds on (ROADMAP 6.3).
    ///
    /// Reads the host bridge (00:00.0) identity as a proof the config reads
    /// reach real hardware and counts the present functions on bus 0.
    fn bring_up_pci(serial: &SerialPort) {
        let scan = crate::pci::scan_bus0();
        let mut ven = [0u8; 18];
        let mut dev = [0u8; 18];
        let mut fns = [0u8; 20];
        serial.write_str("enlil kernel: pci: host bridge ");
        serial.write_str(format_u64_hex(u64::from(scan.host_vendor), &mut ven));
        serial.write_str(":");
        serial.write_str(format_u64_hex(u64::from(scan.host_device), &mut dev));
        serial.write_str(", ");
        serial.write_str(format_u64(u64::from(scan.functions), &mut fns));
        serial.write_str(" functions on bus 0\n");
        // Device classes discovered — the input to driver bring-up + passthrough.
        let mut sto = [0u8; 20];
        let mut net = [0u8; 20];
        let mut dsp = [0u8; 20];
        serial.write_str("enlil kernel: pci: classes ");
        serial.write_str(format_u64(u64::from(scan.storage), &mut sto));
        serial.write_str(" storage, ");
        serial.write_str(format_u64(u64::from(scan.network), &mut net));
        serial.write_str(" network, ");
        serial.write_str(format_u64(u64::from(scan.display), &mut dsp));
        serial.write_str(" display\n");
    }

    /// Arm the LAPIC timer once and prove it fires an interrupt into the kernel.
    ///
    /// The interrupt-preemption clock every later scheduler needs (ROADMAP 6.2).
    /// Installs the timer handler at [`TIMER_VECTOR`], arms a one-shot count,
    /// enables interrupts, and waits (bounded) for the tick — then disables
    /// interrupts again and reports. A bounded wait means a timer that never
    /// fires is reported, not a hang.
    fn bring_up_timer(serial: &SerialPort) {
        /// The IDT vector the LAPIC timer delivers on (above the 0x20 legacy
        /// range, clear of exceptions).
        const TIMER_VECTOR: u8 = 0x40;
        /// One-shot countdown (divide-by-16). Small enough to fire fast under
        /// QEMU, large enough not to fire before interrupts are enabled.
        const TIMER_COUNT: u32 = 0x0010_0000;
        /// Cap on the wait spins so a non-firing timer is reported, not hung.
        const WAIT_SPINS: u32 = 200_000_000;

        crate::idt::install_timer_gate(TIMER_VECTOR);
        let before = crate::idt::timer_ticks();
        crate::apic::arm_oneshot_timer(TIMER_VECTOR, TIMER_COUNT);
        // SAFETY: the timer gate is installed and the handler signals EOI; sti
        // only enables delivery of the interrupt we just armed.
        unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };
        let mut spun = 0u32;
        while crate::idt::timer_ticks() == before && spun < WAIT_SPINS {
            spun += 1;
            core::hint::spin_loop();
        }
        // SAFETY: re-mask interrupts before continuing the single-threaded boot.
        unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)) };

        if crate::idt::timer_ticks() > before {
            serial.write_str("enlil kernel: apic: LAPIC timer fired — preemption clock live\n");
        } else {
            serial.write_str("enlil kernel: apic: LAPIC timer did NOT fire\n");
        }
    }

    /// Turn on the CPU's virtualization extension (AMD SVM) so the kernel can
    /// later run a guest with `VMRUN` — the second live-boot sub-milestone's
    /// enable gate.
    fn bring_up_virtualization(serial: &SerialPort) {
        use crate::svm::SvmStatus;
        match crate::svm::enable_svm() {
            SvmStatus::Available => {
                serial.write_str("enlil kernel: svm: enabled (EFER.SVME set)\n");
                // Program the host state-save area VMRUN requires.
                match crate::svm::program_host_save_area() {
                    Some(pa) => {
                        let mut buf = [0u8; 18];
                        serial.write_str("enlil kernel: svm: host-save area at ");
                        serial.write_str(format_u64_hex(pa, &mut buf));
                        serial.write_str("\n");
                    }
                    None => serial.write_str("enlil kernel: svm: host-save area FAILED\n"),
                }
                // Assemble a full VMRUN-ready guest (code page + NPT + VMCB)
                // through the enlil-hal layer — everything VMRUN takes but the
                // instruction itself.
                match crate::svm::program_boot_vmcb() {
                    Some((vmcb, ncr3, entry)) => {
                        let mut a = [0u8; 18];
                        let mut b = [0u8; 18];
                        let mut c = [0u8; 18];
                        serial.write_str("enlil kernel: svm: vmcb VMRUN-ready (nCR3 ");
                        serial.write_str(format_u64_hex(ncr3, &mut a));
                        serial.write_str(", guest code at GPA ");
                        serial.write_str(format_u64_hex(entry, &mut b));
                        serial.write_str(", vmcb ");
                        serial.write_str(format_u64_hex(vmcb, &mut c));
                        serial.write_str(")\n");
                        // Drive the guest through the real #VMEXIT dispatch loop
                        // — the guest (in its own isolated RAM at GPA 0) runs
                        // CPUID/RDMSR/OUT/HLT, all routed through the HAL's
                        // arch-neutral model (LOCKED PRINCIPLE 2).
                        // SAFETY: SVM is enabled, VM_HSAVE_PA is programmed, and
                        // `vmcb` is a VMRUN-ready VMCB from program_boot_vmcb.
                        let run = unsafe { crate::svm::run_boot_guest_loop(vmcb) };
                        report_guest_run(serial, &run);
                        // Second guest: prove EVENTINJ delivers an injected
                        // interrupt through the guest's real-mode IVT.
                        run_event_inj_guest(serial);
                        // Third guest: prove a 64-bit long-mode guest runs (the
                        // mode a real OS boots in).
                        run_long_mode_guest(serial);
                    }
                    None => serial.write_str("enlil kernel: svm: vmcb VMRUN-ready FAILED\n"),
                }
            }
            SvmStatus::Unsupported => serial.write_str("enlil kernel: svm: not supported by CPU\n"),
            SvmStatus::DisabledByFirmware => {
                serial.write_str("enlil kernel: svm: disabled by firmware (VM_CR locked)\n");
            }
        }
    }

    /// Report what the guest #VMEXIT dispatch loop observed.
    ///
    /// On a clean `HLT`, this proves the loop ran the guest through more than
    /// one instruction and answered each intercepted instruction: CPUID and
    /// RDMSR emulated, their results delivered via the GPR shell, and both
    /// captured through the guest's port writes. The success lines keep the
    /// substrings the QEMU+OVMF harness asserts nightly.
    fn report_guest_run(serial: &SerialPort, run: &crate::svm::GuestRunOutcome) {
        use crate::svm::{
            GUEST_COMPUTE_PORT, GUEST_COMPUTE_SUM, GUEST_GS_PORT, GUEST_GS_SENTINEL,
            GUEST_HV_BIT_PORT, GUEST_IO_PORT, GUEST_MSR_PORT, GUEST_MSR_SENTINEL, GUEST_MSR_W_PORT,
            GUEST_MSR_W_VALUE, GUEST_NPF_PORT, GUEST_NPF_SENTINEL, GUEST_VMMCALL_PORT,
            GUEST_VMMCALL_RESULT, RunStop,
        };
        let mut vr = [0u8; 20];
        let mut cp = [0u8; 20];
        match run.stop {
            RunStop::Halted => {
                serial.write_str("enlil kernel: svm: guest #VMEXIT HLT after ");
                serial.write_str(format_u64(u64::from(run.vmruns), &mut vr));
                serial.write_str(" VMRUNs (");
                serial.write_str(format_u64(u64::from(run.cpuid_exits), &mut cp));
                serial
                    .write_str(" cpuid emulated) — dispatch loop runs a multi-instruction guest\n");
                // The emulated port write proves the IOIO exit path end to end.
                if let Some((port, data)) = run.last_io_out() {
                    let mut p = [0u8; 18];
                    let mut d = [0u8; 18];
                    serial.write_str("enlil kernel: svm: guest OUT port ");
                    serial.write_str(format_u64_hex(u64::from(port), &mut p));
                    serial.write_str(" = ");
                    serial.write_str(format_u64_hex(u64::from(data), &mut d));
                    serial.write_str(" (emulated) — IOIO exit-handling path end to end\n");
                }
                // The guest's OUT to the CPUID port carried the EBX enlil
                // emulated for leaf 0 (delivered via the GPR shell) — a match
                // proves the CPUID exit was answered and reached the guest.
                if let (Some(ebx), Some(out)) =
                    (run.cpuid_leaf0_ebx, run.io_out_to(u16::from(GUEST_IO_PORT)))
                    && out == ebx & 0xFF
                {
                    serial.write_str(
                        "enlil kernel: svm: guest CPUID leaf 0 answered by enlil (vendor byte via GPR shell) — CPUID exit emulated\n",
                    );
                }
                // The guest read CPUID.1:ECX[31] (hypervisor-present) after
                // enlil's stealth and OUT it — a 0 proves the guest cannot see
                // it runs under enlil (LOCKED PRINCIPLE 1), in-guest.
                if let Some(out) = run.io_out_to(u16::from(GUEST_HV_BIT_PORT))
                    && out == 0
                {
                    serial.write_str(
                        "enlil kernel: svm: guest sees hypervisor-present bit clear — CPUID stealth verified in-guest\n",
                    );
                }
                // The guest's OUT to the MSR port carried the sentinel enlil
                // injected for the intercepted RDMSR — a match proves the MSR
                // exit was answered and the spoofed value reached the guest.
                if let Some(out) = run.io_out_to(u16::from(GUEST_MSR_PORT))
                    && out == GUEST_MSR_SENTINEL & 0xFF
                {
                    serial.write_str(
                        "enlil kernel: svm: guest RDMSR answered by enlil (sentinel via GPR shell) — MSR exit emulated\n",
                    );
                }
                // The guest WRMSR'd a value then RDMSR'd it back; enlil shadowed
                // the write (never reaching hardware) and returned it — a match
                // proves per-guest MSR-state virtualization.
                if let Some(out) = run.io_out_to(u16::from(GUEST_MSR_W_PORT))
                    && out == u32::from(GUEST_MSR_W_VALUE)
                {
                    serial.write_str(
                        "enlil kernel: svm: guest WRMSR shadowed + read back by enlil — MSR write virtualized\n",
                    );
                }
                // The guest read an unmapped GPA; enlil caught the NPF,
                // demand-mapped a page with a sentinel, and resumed. A matching
                // OUT proves the fault was handled and the mapped page reached
                // the guest — the basis for demand paging and MMIO.
                if run.npf_exits > 0
                    && let Some(out) = run.io_out_to(u16::from(GUEST_NPF_PORT))
                    && out == u32::from(GUEST_NPF_SENTINEL)
                {
                    serial.write_str(
                        "enlil kernel: svm: guest NPF demand-mapped by enlil — nested page fault handled\n",
                    );
                }
                // The guest ran a native arithmetic loop (a taken branch, no
                // #VMEXIT) and OUT the correct sum — proving it executes real
                // code at native speed under enlil.
                if let Some(out) = run.io_out_to(u16::from(GUEST_COMPUTE_PORT))
                    && out == u32::from(GUEST_COMPUTE_SUM)
                {
                    serial.write_str(
                        "enlil kernel: svm: guest native loop computed the right sum — near-native execution\n",
                    );
                }
                // The guest issued a VMMCALL hypercall; enlil serviced it and
                // wrote the result into guest RAX, which the guest OUT'd. A
                // match proves the paravirt enlil↔guest hypercall channel.
                if run.vmmcall_exits > 0
                    && let Some(out) = run.io_out_to(u16::from(GUEST_VMMCALL_PORT))
                    && out == u32::from(GUEST_VMMCALL_RESULT)
                {
                    serial.write_str(
                        "enlil kernel: svm: guest VMMCALL serviced by enlil (result via RAX) — hypercall channel works\n",
                    );
                }
                // The guest read a byte through its GS segment, whose base VMRUN
                // never loads — only the run shell's VMLOAD does. A matching OUT
                // proves the VMSAVE/VMLOAD extended-state swap loaded the guest's
                // FS/GS/TR/LDTR before entry (a guest using segmentation is safe).
                if let Some(out) = run.io_out_to(u16::from(GUEST_GS_PORT))
                    && out == u32::from(GUEST_GS_SENTINEL)
                {
                    serial.write_str(
                        "enlil kernel: svm: guest GS-relative read via VMLOAD'd base — VMSAVE/VMLOAD extended-state swap works\n",
                    );
                }
            }
            RunStop::ShutDown => serial.write_str("enlil kernel: svm: guest SHUTDOWN\n"),
            RunStop::Invalid => serial.write_str("enlil kernel: svm: guest INVALID state\n"),
            RunStop::IterationCap => {
                serial.write_str("enlil kernel: svm: guest hit VMRUN cap (runaway)\n");
            }
            RunStop::Unhandled => {
                let mut e = [0u8; 18];
                serial.write_str("enlil kernel: svm: guest #VMEXIT unhandled code ");
                serial.write_str(format_u64_hex(run.final_exit, &mut e));
                serial.write_str("\n");
            }
        }
    }

    /// Build and run the event-injection guest, reporting whether the injected
    /// interrupt was delivered and handled.
    ///
    /// enlil arms the VMCB `EVENTINJ` field so `VMRUN` injects
    /// [`GUEST_EVENT_VECTOR`](crate::svm::GUEST_EVENT_VECTOR) before the guest's
    /// first instruction; the guest's real-mode IVT vectors it to a handler that
    /// `OUT`s [`GUEST_EVENT_SENTINEL`](crate::svm::GUEST_EVENT_SENTINEL). A
    /// matching `OUT` in the run's I/O record proves the injection was delivered
    /// and handled — the enabling step for virtual-timer and virtual-device
    /// interrupts (ROADMAP 6.2).
    fn run_event_inj_guest(serial: &SerialPort) {
        use crate::svm::{GUEST_EVENT_PORT, GUEST_EVENT_SENTINEL};
        match crate::svm::program_event_inj_vmcb() {
            Some((vmcb, _handler)) => {
                // SAFETY: SVM is enabled, VM_HSAVE_PA is programmed, and `vmcb`
                // is a VMRUN-ready VMCB from program_event_inj_vmcb.
                let run = unsafe { crate::svm::run_boot_guest_loop(vmcb) };
                if run.io_out_to(u16::from(GUEST_EVENT_PORT))
                    == Some(u32::from(GUEST_EVENT_SENTINEL))
                {
                    serial.write_str(
                        "enlil kernel: svm: injected interrupt vectored to guest handler — event injection works\n",
                    );
                } else {
                    serial.write_str(
                        "enlil kernel: svm: event injection NOT observed (guest took the bare-HLT path)\n",
                    );
                }
            }
            None => serial.write_str("enlil kernel: svm: event-inj vmcb build FAILED\n"),
        }
    }

    /// Build and run a 64-bit long-mode guest, reporting whether it executed.
    ///
    /// enlil programs a VMCB for long mode (paging on, `CR3` walking the guest's
    /// own identity page tables, an `L`-bit code segment) and runs a guest whose
    /// code `OUT`s [`GUEST_LM_SENTINEL`](crate::svm::GUEST_LM_SENTINEL). A
    /// matching `OUT` proves a guest ran in the mode a real x86-64 OS boots in;
    /// a `VMEXIT_INVALID` stop instead means a long-mode consistency check
    /// failed (ROADMAP 6.2).
    fn run_long_mode_guest(serial: &SerialPort) {
        use crate::svm::{GUEST_LM_PORT, GUEST_LM_SENTINEL};
        match crate::svm::program_long_mode_vmcb() {
            Some((vmcb, _cr3)) => {
                // SAFETY: SVM is enabled, VM_HSAVE_PA is programmed, and `vmcb`
                // is a VMRUN-ready long-mode VMCB from program_long_mode_vmcb.
                let run = unsafe { crate::svm::run_boot_guest_loop(vmcb) };
                if run.io_out_to(u16::from(GUEST_LM_PORT)) == Some(u32::from(GUEST_LM_SENTINEL)) {
                    serial.write_str(
                        "enlil kernel: svm: 64-bit long-mode guest ran to its OUT — long-mode guest works\n",
                    );
                } else {
                    let mut e = [0u8; 18];
                    serial.write_str(
                        "enlil kernel: svm: long-mode guest did NOT reach its OUT (final exit ",
                    );
                    serial.write_str(format_u64_hex(run.final_exit, &mut e));
                    serial.write_str(")\n");
                }
            }
            None => serial.write_str("enlil kernel: svm: long-mode vmcb build FAILED\n"),
        }
    }

    /// Enable the local APIC in x2APIC mode and report its ID — the interrupt
    /// hardware the LAPIC timer / IPIs / MSI routing build on. Returns the APIC
    /// id (or `None` if x2APIC is unavailable) for the per-CPU block.
    fn bring_up_apic(serial: &SerialPort) -> Option<u32> {
        crate::apic::enable_x2apic().map_or_else(
            || {
                serial.write_str("enlil kernel: apic: x2APIC unavailable\n");
                None
            },
            |id| {
                let mut buf = [0u8; 20];
                serial.write_str("enlil kernel: apic: x2APIC enabled, id ");
                serial.write_str(format_u64(u64::from(id), &mut buf));
                serial.write_str("\n");
                Some(id)
            },
        )
    }

    /// Self-test the bare-metal spinlock primitive — the mutual exclusion the
    /// kernel guards shared state with once SMP brings up more cores (6.2).
    fn bring_up_locks(serial: &SerialPort) {
        if crate::spinlock::selftest() {
            serial.write_str("enlil kernel: sync: spinlock acquire/release self-test ok\n");
        } else {
            serial.write_str("enlil kernel: sync: spinlock self-test FAILED\n");
        }
    }

    /// Install this CPU's per-CPU data block as the `GS`-base TLS pointer and
    /// prove `gs:[0]` reads it back — the foundation for SMP per-core data
    /// (run queue, current vCPU) reached through `GS` (ROADMAP 6.2).
    fn bring_up_percpu(serial: &SerialPort, apic_id: u32) {
        if crate::percpu::install_and_selftest(apic_id) {
            serial.write_str("enlil kernel: percpu: GS-base TLS installed, gs:[0] self-test ok\n");
        } else {
            serial.write_str("enlil kernel: percpu: GS-base TLS self-test FAILED\n");
        }
    }

    /// Draw the boot indicator on the GOP framebuffer (if the firmware handed
    /// one over) and self-test that the kernel can drive it.
    fn bring_up_framebuffer(serial: &SerialPort, handoff: &BootHandoff) {
        match &handoff.framebuffer {
            Some(fb) if crate::framebuffer::draw_and_selftest(fb) => {
                serial.write_str("enlil kernel: gop: framebuffer draw ok\n");
            }
            Some(_) => serial.write_str("enlil kernel: gop: framebuffer draw FAILED\n"),
            None => serial.write_str("enlil kernel: gop: no framebuffer in handoff\n"),
        }
    }

    /// Install the kernel's own IDT (replacing the firmware's, whose handlers
    /// live in soon-to-be-reclaimed boot-services memory) and prove it
    /// vectors by taking a breakpoint.
    fn bring_up_interrupts(serial: &SerialPort) {
        if crate::idt::init_and_selftest() {
            serial.write_str("enlil kernel: idt: installed, int3 self-test ok\n");
        } else {
            serial.write_str("enlil kernel: idt: self-test FAILED\n");
        }
    }

    /// Install the kernel heap in the largest conventional region and prove
    /// dynamic allocation works with the firmware gone.
    fn bring_up_heap(serial: &SerialPort, summary: &MemorySummary) {
        let size = boot_heap_size(summary.largest_conventional_bytes);
        if size == 0 {
            serial.write_str("enlil kernel: heap: NO conventional region large enough\n");
            return;
        }
        let base = summary.largest_conventional_base;
        let (Ok(base_usize), Ok(size_usize)) = (usize::try_from(base), usize::try_from(size))
        else {
            serial.write_str("enlil kernel: heap: region beyond addressable range\n");
            return;
        };
        // SAFETY: the span is conventional memory (nothing of the firmware,
        // image, stack, or handoff lives there), sized within the region, and
        // this runs once on the single boot CPU.
        unsafe {
            install_kernel_heap(
                core::ptr::with_exposed_provenance_mut(base_usize),
                size_usize,
            );
        }

        // First dynamic allocation with no firmware alive: grow a Vec across
        // a few reallocations and check the contents survived.
        let mut probe: Vec<u8> = Vec::new();
        for i in 0..4096usize {
            probe.push(u8::try_from(i % 251).unwrap_or(0));
        }
        let intact = probe
            .iter()
            .enumerate()
            .all(|(i, &b)| usize::from(b) == i % 251);

        let mut dec = [0u8; 20];
        let mut hex = [0u8; 18];
        serial.write_str("enlil kernel: heap: ");
        serial.write_str(format_u64(size / (1024 * 1024), &mut dec));
        serial.write_str(" MiB at ");
        serial.write_str(format_u64_hex(base, &mut hex));
        if intact {
            serial.write_str(", alloc test ok\n");
        } else {
            serial.write_str(", alloc test FAILED\n");
        }
    }

    /// Emit the memory summary the QEMU+OVMF harness asserts on.
    fn report_memory(serial: &SerialPort, summary: &MemorySummary) {
        let mut buf = [0u8; 20];
        serial.write_str("enlil kernel: memory: ");
        serial.write_str(format_u64(
            u64::try_from(summary.descriptors).unwrap_or(u64::MAX),
            &mut buf,
        ));
        serial.write_str(" descriptors, ");
        serial.write_str(format_u64(
            u64::try_from(summary.usable_regions).unwrap_or(u64::MAX),
            &mut buf,
        ));
        serial.write_str(" usable regions, ");
        serial.write_str(format_u64(summary.usable_mib(), &mut buf));
        serial.write_str(" MiB usable (largest ");
        serial.write_str(format_u64(summary.largest_usable_mib(), &mut buf));
        serial.write_str(" MiB)\n");
    }

    /// Halt the CPU forever (interrupts stay off; `hlt` retires on any
    /// machine-generated wakeup and loops straight back).
    fn park() -> ! {
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one raw descriptor with the spec layout inside `stride` bytes.
    fn desc(efi_type: u32, phys_start: u64, pages: u64, stride: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; stride];
        bytes[DESC_TYPE_OFFSET..DESC_TYPE_OFFSET + 4].copy_from_slice(&efi_type.to_le_bytes());
        bytes[DESC_PHYS_START_OFFSET..DESC_PHYS_START_OFFSET + 8]
            .copy_from_slice(&phys_start.to_le_bytes());
        bytes[DESC_NUM_PAGES_OFFSET..DESC_NUM_PAGES_OFFSET + 8]
            .copy_from_slice(&pages.to_le_bytes());
        bytes
    }

    fn map(descriptors: &[Vec<u8>]) -> Vec<u8> {
        descriptors.concat()
    }

    #[test]
    fn summarizes_usable_and_reserved_regions() {
        // 48-byte stride like real firmware (struct is 40, stride padded).
        let stride = 48;
        let bytes = map(&[
            desc(7, 0x10_0000, 256, stride),  // conventional: 1 MiB
            desc(0, 0xE_0000, 16, stride),    // reserved: not usable
            desc(2, 0x20_0000, 512, stride),  // loader data: 2 MiB
            desc(11, 0xFEC0_0000, 1, stride), // MMIO: not usable
        ]);
        let summary = summarize_memory_map(&bytes, stride);
        assert_eq!(summary.descriptors, 4);
        assert_eq!(summary.usable_regions, 2);
        assert_eq!(summary.usable_bytes, (256 + 512) * 4096);
        assert_eq!(summary.largest_usable_bytes, 512 * 4096);
        assert_eq!(summary.highest_usable_end, 0x20_0000 + 512 * 4096);
        assert_eq!(summary.usable_mib(), 3);
        assert_eq!(summary.largest_usable_mib(), 2);
    }

    #[test]
    fn honors_firmware_stride_larger_than_struct() {
        // A future firmware may pad descriptors: same fields, 64-byte stride.
        let bytes = map(&[desc(7, 0, 1024, 64), desc(7, 0x100_0000, 1024, 64)]);
        let summary = summarize_memory_map(&bytes, 64);
        assert_eq!(summary.descriptors, 2);
        assert_eq!(summary.usable_regions, 2);
        assert_eq!(summary.usable_bytes, 2 * 1024 * 4096);
    }

    #[test]
    fn zero_page_descriptors_are_not_usable_regions() {
        let bytes = map(&[desc(7, 0x1000, 0, 48)]);
        let summary = summarize_memory_map(&bytes, 48);
        assert_eq!(summary.descriptors, 1);
        assert_eq!(summary.usable_regions, 0);
        assert_eq!(summary.usable_bytes, 0);
    }

    #[test]
    fn degenerate_stride_yields_empty_summary() {
        let bytes = map(&[desc(7, 0, 16, 48)]);
        // Stride smaller than the fields we must read → refuse to guess.
        assert_eq!(summarize_memory_map(&bytes, 16), MemorySummary::default());
        // Zero stride must not loop forever.
        assert_eq!(summarize_memory_map(&bytes, 0), MemorySummary::default());
    }

    #[test]
    fn trailing_partial_descriptor_is_ignored() {
        let stride = 48;
        let mut bytes = map(&[desc(7, 0, 16, stride)]);
        bytes.extend_from_slice(&[0u8; 24]); // torn tail
        let summary = summarize_memory_map(&bytes, stride);
        assert_eq!(summary.descriptors, 1);
        assert_eq!(summary.usable_regions, 1);
    }

    #[test]
    fn tracks_largest_conventional_region_for_the_heap() {
        let stride = 48;
        let bytes = map(&[
            desc(2, 0x10_0000, 1024, stride), // loader data: usable, NOT heap-safe
            desc(7, 0x100_0000, 256, stride), // conventional: 1 MiB
            desc(7, 0x800_0000, 768, stride), // conventional: 3 MiB — largest
        ]);
        let summary = summarize_memory_map(&bytes, stride);
        assert_eq!(summary.usable_regions, 3);
        // The 4 MiB loader-data region must NOT be picked for the heap even
        // though it is the largest usable region: it holds live boot state.
        assert_eq!(summary.largest_conventional_base, 0x800_0000);
        assert_eq!(summary.largest_conventional_bytes, 768 * 4096);
    }

    #[test]
    fn boot_heap_size_caps_and_floors() {
        assert_eq!(boot_heap_size(0), 0);
        assert_eq!(boot_heap_size(BOOT_HEAP_MIN_BYTES - 1), 0);
        assert_eq!(boot_heap_size(BOOT_HEAP_MIN_BYTES), BOOT_HEAP_MIN_BYTES);
        assert_eq!(boot_heap_size(8 * 1024 * 1024), 8 * 1024 * 1024);
        assert_eq!(boot_heap_size(u64::MAX), BOOT_HEAP_MAX_BYTES);
    }

    #[test]
    fn formats_hex_without_alloc() {
        let mut buf = [0u8; 18];
        assert_eq!(format_u64_hex(0, &mut buf), "0x0");
        assert_eq!(format_u64_hex(0x800_0000, &mut buf), "0x8000000");
        assert_eq!(format_u64_hex(u64::MAX, &mut buf), "0xffffffffffffffff");
    }

    #[test]
    fn formats_decimals_without_alloc() {
        let mut buf = [0u8; 20];
        assert_eq!(format_u64(0, &mut buf), "0");
        assert_eq!(format_u64(9, &mut buf), "9");
        assert_eq!(format_u64(4096, &mut buf), "4096");
        assert_eq!(format_u64(u64::MAX, &mut buf), "18446744073709551615");
    }
}
