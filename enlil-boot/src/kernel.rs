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
    }
    summary
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

#[cfg(target_os = "uefi")]
pub use hw::kernel_entry;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{MemorySummary, format_u64, summarize_memory_map};
    use crate::handoff::BootHandoff;
    use crate::serial::SerialPort;

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
        } else {
            serial.write_str("enlil kernel: memory: NO MAP in handoff\n");
        }

        park()
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
    fn formats_decimals_without_alloc() {
        let mut buf = [0u8; 20];
        assert_eq!(format_u64(0, &mut buf), "0");
        assert_eq!(format_u64(9, &mut buf), "9");
        assert_eq!(format_u64(4096, &mut buf), "4096");
        assert_eq!(format_u64(u64::MAX, &mut buf), "18446744073709551615");
    }
}
