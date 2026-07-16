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
        bring_up_apic(&serial);
        bring_up_virtualization(&serial);
        bring_up_framebuffer(&serial, handoff);

        park()
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
                        serial.write_str(", guest hlt at ");
                        serial.write_str(format_u64_hex(entry, &mut b));
                        serial.write_str(", vmcb ");
                        serial.write_str(format_u64_hex(vmcb, &mut c));
                        serial.write_str(")\n");
                        // Drive the guest through the real #VMEXIT dispatch
                        // loop — the guest runs CPUID (intercepted, skipped by
                        // the loop) then HLT, so it executes more than one
                        // instruction and exits are routed through the HAL's
                        // arch-neutral model (LOCKED PRINCIPLE 2).
                        // SAFETY: SVM is enabled, VM_HSAVE_PA is programmed, and
                        // `vmcb` is a VMRUN-ready VMCB from program_boot_vmcb.
                        let run = unsafe { crate::svm::run_boot_guest_loop(vmcb) };
                        report_guest_run(serial, &run);
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
    /// On a clean `HLT` after skipping the intercepted `CPUID`, this proves the
    /// loop re-`VMRUN`ed the guest through more than one instruction. The
    /// success line keeps the substring the QEMU+OVMF harness asserts nightly.
    fn report_guest_run(serial: &SerialPort, run: &crate::svm::GuestRunOutcome) {
        use crate::svm::RunStop;
        let mut vr = [0u8; 20];
        let mut cp = [0u8; 20];
        match run.stop {
            RunStop::Halted => {
                serial.write_str("enlil kernel: svm: guest #VMEXIT HLT after ");
                serial.write_str(format_u64(u64::from(run.vmruns), &mut vr));
                serial.write_str(" VMRUNs (");
                serial.write_str(format_u64(u64::from(run.cpuid_exits), &mut cp));
                serial
                    .write_str(" cpuid skipped) — dispatch loop runs a multi-instruction guest\n");
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

    /// Enable the local APIC in x2APIC mode and report its ID — the interrupt
    /// hardware the LAPIC timer / IPIs / MSI routing build on.
    fn bring_up_apic(serial: &SerialPort) {
        match crate::apic::enable_x2apic() {
            Some(id) => {
                let mut buf = [0u8; 20];
                serial.write_str("enlil kernel: apic: x2APIC enabled, id ");
                serial.write_str(format_u64(u64::from(id), &mut buf));
                serial.write_str("\n");
            }
            None => serial.write_str("enlil kernel: apic: x2APIC unavailable\n"),
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
