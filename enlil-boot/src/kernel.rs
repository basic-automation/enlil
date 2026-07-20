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

/// The ceiling for an AP trampoline page.
///
/// A `SIPI`'s vector byte encodes the AP's real-mode start address as
/// `vector << 12`, so a woken AP can only begin executing in the low 1 MiB,
/// page-aligned (SDM Vol. 3 §9.4.4). Wherever the trampoline lands, it must be
/// below this.
pub const AP_TRAMPOLINE_LIMIT: u64 = 1024 * 1024;

/// Lowest address an AP trampoline may use.
///
/// The real-mode IVT (0–0x400) and the BIOS data area (0x400–0x500) live at the
/// bottom of memory; page 0 is also where a null-pointer write lands, which
/// should keep faulting. Start above all of it.
pub const AP_TRAMPOLINE_MIN: u64 = 0x1000;

/// Find a page below 1 MiB that an AP trampoline can be placed in.
///
/// SMP bring-up sends `INIT`-`SIPI`-`SIPI` to each application processor, and
/// the `SIPI` vector can only point at a page-aligned address in the low 1 MiB —
/// so before an AP can be started at all, the kernel needs a page *there* that
/// the firmware says is genuinely free. Hard-coding a traditional address (0x8000
/// and friends) is a guess; under UEFI the low megabyte's layout is the
/// firmware's business, and a wrong guess means the AP executes garbage and
/// triple-faults.
///
/// Scans the raw firmware descriptor array for [`EFI_CONVENTIONAL`] memory —
/// not merely `Usable`-mapped, since loader and boot-services regions still hold
/// live boot state — and returns the **highest** qualifying page in
/// `[AP_TRAMPOLINE_MIN, AP_TRAMPOLINE_LIMIT)`, keeping away from the very bottom
/// of memory where legacy structures cluster. Alloc-free, so it runs before any
/// `MemoryMap` exists. `None` if the firmware left no conventional page down
/// there.
#[must_use]
pub fn find_ap_trampoline_page(bytes: &[u8], descriptor_size: usize) -> Option<u64> {
    if descriptor_size < DESC_MIN_SIZE {
        return None;
    }
    let mut best: Option<u64> = None;
    for desc in bytes.chunks_exact(descriptor_size) {
        let (Some(efi_type), Some(phys_start), Some(pages)) = (
            read_u32_le(desc, DESC_TYPE_OFFSET),
            read_u64_le(desc, DESC_PHYS_START_OFFSET),
            read_u64_le(desc, DESC_NUM_PAGES_OFFSET),
        ) else {
            break;
        };
        if pages == 0 || efi_type != EFI_CONVENTIONAL {
            continue;
        }
        let region_end = phys_start.saturating_add(pages.saturating_mul(UEFI_PAGE_SIZE));
        // Clip the region to the window a SIPI vector can reach.
        let start = phys_start.max(AP_TRAMPOLINE_MIN);
        let end = region_end.min(AP_TRAMPOLINE_LIMIT);
        if start >= end || end - start < UEFI_PAGE_SIZE {
            continue;
        }
        // The highest whole page fully inside the clipped span.
        let page = (end - UEFI_PAGE_SIZE) & !(UEFI_PAGE_SIZE - 1);
        if page >= start {
            best = Some(best.map_or(page, |b| b.max(page)));
        }
    }
    best
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

/// The planned guest-RAM window: a bump allocator over the region
/// `plan_hypervisor_regions` carved for guests.
///
/// Guest RAM must **not** come from the kernel heap: the heap is where the
/// hypervisor's own structures live, and a guest with a nested mapping onto it
/// could reach them. The planner carves a span for exactly this purpose
/// (LOCKED PRINCIPLE 5 — isolation), retyped `Reserved` in the map so nothing
/// else claims it; this hands out disjoint slices of it, one per guest.
///
/// Empty until [`kernel::bring_up_memory_plan`](kernel) publishes the plan.
pub mod guest_ram {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Next free system-physical address inside the carved window.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    /// One past the last byte of the carved window (0 = no window).
    static END: AtomicU64 = AtomicU64::new(0);

    /// Publish the carved guest-RAM window so [`take`] can hand out slices.
    pub fn publish(base: u64, size: u64) {
        NEXT.store(base, Ordering::Release);
        END.store(base.saturating_add(size), Ordering::Release);
    }

    /// Bytes still available in the carved window.
    #[must_use]
    pub fn remaining() -> u64 {
        END.load(Ordering::Acquire)
            .saturating_sub(NEXT.load(Ordering::Acquire))
    }

    /// Take a slice of the carved window for one guest.
    ///
    /// Returns the `size`-byte, `align`-aligned slice's system-physical base, or
    /// `None` if no window was published or too little of it is left (the caller
    /// then falls back to the heap).
    ///
    /// `align` must be a power of two. Slices are disjoint and never reused, so
    /// one guest's RAM can never alias another's.
    #[must_use]
    pub fn take(size: u64, align: u64) -> Option<u64> {
        let end = END.load(Ordering::Acquire);
        if end == 0 || size == 0 || !align.is_power_of_two() {
            return None;
        }
        let mut cur = NEXT.load(Ordering::Acquire);
        loop {
            let base = cur.checked_add(align - 1)? & !(align - 1);
            let next = base.checked_add(size)?;
            if next > end {
                return None;
            }
            match NEXT.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(base),
                Err(actual) => cur = actual,
            }
        }
    }
}

/// The low-memory page reserved for the AP startup trampoline.
///
/// Found from the firmware map at boot ([`find_ap_trampoline_page`]) and kept
/// here because the `SIPI` that starts an application processor encodes its
/// start address as a vector byte — the page has to be known before any AP can
/// be woken.
pub mod ap_trampoline {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// The claimed page's physical address (0 = none claimed).
    static PAGE: AtomicU64 = AtomicU64::new(0);

    /// Record the page claimed for the trampoline.
    pub fn publish(page: u64) {
        PAGE.store(page, Ordering::Release);
    }

    /// The claimed trampoline page, if one was found.
    #[must_use]
    pub fn page() -> Option<u64> {
        match PAGE.load(Ordering::Acquire) {
            0 => None,
            p => Some(p),
        }
    }

    /// The `SIPI` vector byte that starts an AP at the claimed page.
    ///
    /// A `SIPI`'s vector is the start address shifted right by 12, so this is
    /// `None` unless a page was claimed and it lies in the reachable low 1 MiB.
    #[must_use]
    pub fn sipi_vector() -> Option<u8> {
        u8::try_from(page()? >> 12).ok()
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
    use super::{
        DESC_MIN_SIZE, DESC_NUM_PAGES_OFFSET, DESC_PHYS_START_OFFSET, DESC_TYPE_OFFSET,
        MemorySummary, boot_heap_size, format_u64, format_u64_hex, read_u32_le, read_u64_le,
        summarize_memory_map,
    };
    use crate::allocator::install_kernel_heap;
    use crate::handoff::BootHandoff;
    use crate::serial::SerialPort;
    use alloc::alloc::Layout;
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

        let mut highest_usable_end = 0u64;
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
            highest_usable_end = summary.highest_usable_end;
            report_memory(&serial, &summary);
            let heap_span = bring_up_heap(&serial, &summary);
            // The heap is live, so enlil-platform's heap-backed primitives work.
            bring_up_scheduler(&serial);
            bring_up_memory_plan(
                &serial,
                bytes,
                handoff.memory_descriptor_size,
                &summary,
                heap_span,
            );
            bring_up_ap_trampoline_page(&serial, bytes, handoff.memory_descriptor_size);
        } else {
            serial.write_str("enlil kernel: memory: NO MAP in handoff\n");
        }

        bring_up_interrupts(&serial);
        bring_up_locks(&serial);
        let apic_id = bring_up_apic(&serial);
        bring_up_percpu(&serial, apic_id.unwrap_or(0));
        bring_up_gdt(&serial);
        bring_up_timer(&serial);
        bring_up_deadline_timer(&serial);
        if let Some(hz) = bring_up_time(&serial) {
            bring_up_deadline_ns(&serial, hz);
            bring_up_platform_time(&serial, hz);
        }
        let ecam = bring_up_acpi(&serial, handoff);
        bring_up_pci(&serial);
        let map = bring_up_paging(&serial, handoff, highest_usable_end);

        // Everything from here runs on a stack the kernel owns, with a guard
        // page below it — off the firmware's unguarded boot-services stack. The
        // continuation takes its inputs from statics because nothing on this
        // stack survives the switch.
        publish_continuation(handoff, ecam);
        if let Some(mut map) = map
            && let Some(stack) = bring_up_guarded_stack(&serial, &mut map)
        {
            // SAFETY: `stack` came from allocate_guarded_stack — a live, mapped,
            // exclusively-owned region with a 16-byte-aligned top — and nothing
            // on the current stack is needed again: the continuation diverges,
            // and its inputs were published to statics above. `handoff` points
            // into firmware memory, not this stack.
            unsafe { crate::stack::switch_to_guarded_stack(stack, kernel_main_on_own_stack) };
        }

        // No guarded stack (no map, no spare table page, or the guard would not
        // take) — carry on where we are rather than not booting at all.
        serial.write_str("enlil kernel: stack: continuing on the firmware stack\n");
        kernel_main_on_own_stack()
    }

    /// Where [`kernel_entry`] leaves the continuation's inputs, since the switch
    /// to the kernel stack abandons everything on the old one.
    static HANDOFF_PTR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    /// The ECAM base discovered from the MCFG, plus 1 (0 = none discovered).
    static ECAM_BASE_PLUS1: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    /// The last bus in the ECAM window.
    static ECAM_END_BUS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    /// Publish the continuation's inputs before the stack switch.
    fn publish_continuation(handoff: &BootHandoff, ecam: Option<(u64, u8)>) {
        use core::sync::atomic::Ordering;
        HANDOFF_PTR.store(core::ptr::from_ref(handoff) as u64, Ordering::Release);
        if let Some((base, end_bus)) = ecam {
            ECAM_BASE_PLUS1.store(base.saturating_add(1), Ordering::Release);
            ECAM_END_BUS.store(u64::from(end_bus), Ordering::Release);
        }
    }

    /// The rest of the kernel's bring-up, running on the kernel-owned stack.
    ///
    /// Reached by [`switch_to_guarded_stack`](crate::stack::switch_to_guarded_stack)
    /// (or called directly if no guarded stack could be built). Reports the `RSP`
    /// it is actually running on, then finishes bring-up and parks.
    extern "C" fn kernel_main_on_own_stack() -> ! {
        use core::sync::atomic::Ordering;

        let serial = SerialPort::com1();
        report_running_stack(&serial);

        let handoff_ptr = HANDOFF_PTR.load(Ordering::Acquire);
        let handoff = (handoff_ptr != 0).then(|| {
            // SAFETY: kernel_entry published a pointer to the BootHandoff it was
            // called with, which lives in firmware memory (not on either stack)
            // and outlives the kernel.
            unsafe {
                &*(core::ptr::with_exposed_provenance::<BootHandoff>(
                    usize::try_from(handoff_ptr).unwrap_or(0),
                ))
            }
        });

        // ECAM reads need the identity map installed earlier (window < 4 GiB).
        let ecam_base = ECAM_BASE_PLUS1.load(Ordering::Acquire);
        if ecam_base != 0 {
            let end_bus = u8::try_from(ECAM_END_BUS.load(Ordering::Acquire)).unwrap_or(u8::MAX);
            bring_up_ecam(&serial, ecam_base - 1, end_bus);
        }
        bring_up_virtualization(&serial);
        if let Some(handoff) = handoff {
            bring_up_framebuffer(&serial, handoff);
        }

        park()
    }

    /// Report which stack the kernel is actually running on now.
    ///
    /// Read after the switch, from the running `RSP` itself — so it reflects
    /// reality rather than what was intended.
    fn report_running_stack(serial: &SerialPort) {
        let rsp = crate::stack::current_rsp();
        let mut r = [0u8; 18];
        serial.write_str("enlil kernel: stack: kernel bring-up continues at RSP ");
        serial.write_str(format_u64_hex(rsp, &mut r));
        serial.write_str("\n");
    }

    /// Calibrate the TSC against the PIT and report its frequency — the
    /// monotonic time source the bare-metal kernel runs on (ROADMAP 6.2).
    ///
    /// Runs with interrupts masked (the boot path has not enabled them since
    /// `bring_up_timer` re-masked) so nothing perturbs the calibration window.
    fn bring_up_time(serial: &SerialPort) -> Option<u64> {
        use crate::tsc::hz_to_mhz_rounded;
        match crate::tsc::calibrate_tsc_hz() {
            Some(hz) if hz > 0 => {
                let mut buf = [0u8; 20];
                serial.write_str("enlil kernel: time: TSC calibrated ");
                serial.write_str(format_u64(hz_to_mhz_rounded(hz), &mut buf));
                serial.write_str(" MHz (via PIT) — monotonic clock live\n");
                bring_up_monotonic(serial, hz);
                Some(hz)
            }
            _ => {
                serial.write_str("enlil kernel: time: TSC calibration FAILED (PIT silent)\n");
                None
            }
        }
    }

    /// Arm the LAPIC TSC-deadline timer at a **precise nanosecond deadline**
    /// derived from the calibrated clock, and measure — via the monotonic clock
    /// — that it fired on time. This is exactly how the scheduler arms a quantum
    /// ("preempt me in N µs"): a real-time deadline, not a raw tick count.
    ///
    /// Converts the requested ns to a TSC-tick offset with the calibrated `hz`
    /// ([`ns_to_ticks`]), arms the deadline timer, waits (bounded) for the tick,
    /// then checks the elapsed ns (measured independently across the wait) lands
    /// in a wide plausibility band — proving the ns→deadline→fire→measure path
    /// end to end (ROADMAP 6.2).
    fn bring_up_deadline_ns(serial: &SerialPort, hz: u64) {
        use crate::tsc::{ns_to_ticks, read_tsc, ticks_to_ns};
        /// The scheduler-quantum-sized deadline to arm and measure (2 ms).
        const DEADLINE_NS: u64 = 2_000_000;
        /// Wide acceptance band — only a grossly wrong clock/timer fails.
        const MIN_NS: u64 = DEADLINE_NS / 2;
        const MAX_NS: u64 = DEADLINE_NS * 20;
        /// Cap on the wait spins so a non-firing timer is reported, not hung.
        const WAIT_SPINS: u32 = 200_000_000;
        /// Same vector + handler as the other timer tests.
        const TIMER_VECTOR: u8 = 0x40;

        if !crate::apic::tsc_deadline_available() {
            return; // reported already by bring_up_deadline_timer
        }
        crate::idt::install_timer_gate(TIMER_VECTOR);
        let before = crate::idt::timer_ticks();
        let t0 = read_tsc();
        crate::apic::arm_tsc_deadline_timer(TIMER_VECTOR, ns_to_ticks(DEADLINE_NS, hz));
        // SAFETY: the timer gate is installed and the handler signals EOI.
        unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };
        let mut spun = 0u32;
        while crate::idt::timer_ticks() == before && spun < WAIT_SPINS {
            spun += 1;
            core::hint::spin_loop();
        }
        // SAFETY: re-mask interrupts before continuing the single-threaded boot.
        unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)) };
        let elapsed = ticks_to_ns(read_tsc().wrapping_sub(t0), hz);

        if crate::idt::timer_ticks() > before && (MIN_NS..=MAX_NS).contains(&elapsed) {
            let mut buf = [0u8; 20];
            serial.write_str("enlil kernel: apic: TSC-deadline fired after ");
            serial.write_str(format_u64(elapsed / 1000, &mut buf));
            serial.write_str(" us for a 2 ms ns-deadline — ns-precise preemption\n");
        } else {
            serial.write_str("enlil kernel: apic: ns-precise TSC-deadline self-test FAILED\n");
        }
    }

    /// Prove the calibrated TSC drives a monotonic ns clock + busy-sleep — the
    /// bare-metal time source timeouts and the scheduler read (ROADMAP 6.2).
    ///
    /// Reads the clock, busy-sleeps a fixed interval, reads again, and checks
    /// the elapsed ns is monotonic and lands in a wide plausibility band around
    /// the request (so a mis-scaled clock or a broken `rdtsc` is caught, without
    /// flaking on the emulator's timing jitter). Interrupts are masked here, so
    /// nothing perturbs the sleep.
    fn bring_up_monotonic(serial: &SerialPort, hz: u64) {
        use crate::tsc::{read_tsc, ticks_to_ns};
        /// The interval to sleep and measure (5 ms).
        const SLEEP_NS: u64 = 5_000_000;
        /// Accept anything from ~half to 20x the request — a loose band that
        /// only fails on a grossly mis-scaled clock, not emulator jitter.
        const MIN_NS: u64 = SLEEP_NS / 2;
        const MAX_NS: u64 = SLEEP_NS * 20;

        let t0 = read_tsc();
        crate::tsc::busy_sleep_ns(hz, SLEEP_NS);
        let t1 = read_tsc();
        let elapsed = ticks_to_ns(t1.wrapping_sub(t0), hz);
        if t1 != t0 && (MIN_NS..=MAX_NS).contains(&elapsed) {
            let mut buf = [0u8; 20];
            serial.write_str("enlil kernel: time: monotonic clock advanced ");
            serial.write_str(format_u64(elapsed / 1000, &mut buf));
            serial.write_str(" us over a 5 ms busy-sleep — TSC clock live\n");
        } else {
            serial.write_str("enlil kernel: time: monotonic clock self-test FAILED\n");
        }
    }

    /// Prove enlil-platform's time backend (`Instant` + calibrated-TSC
    /// `Duration`) runs on real hardware — the third `no_std` enlil-platform
    /// module driven from the live boot path, after the scheduler and the
    /// `MemoryMap` carver.
    ///
    /// Feeds the kernel's PIT-calibrated TSC frequency to `enlil_platform::time`,
    /// then times a fixed busy-sleep with the platform `Instant::now()` /
    /// `elapsed()` and checks the elapsed `Duration` lands in a wide plausibility
    /// band (so a mis-scaled clock is caught without flaking on emulator jitter).
    fn bring_up_platform_time(serial: &SerialPort, hz: u64) {
        use enlil_platform::time::{Instant, set_tsc_frequency};

        /// Interval to sleep and measure (5 ms).
        const SLEEP_NS: u64 = 5_000_000;
        /// Accept ~half to 20x the request — only a grossly wrong clock fails.
        const MIN_MS: u128 = 2;
        const MAX_MS: u128 = 100;

        set_tsc_frequency(hz);
        let start = Instant::now();
        crate::tsc::busy_sleep_ns(hz, SLEEP_NS);
        let ms = start.elapsed().as_millis();
        if (MIN_MS..=MAX_MS).contains(&ms) {
            let mut buf = [0u8; 20];
            serial.write_str("enlil kernel: time: enlil-platform Instant measured ");
            serial.write_str(format_u64(u64::try_from(ms).unwrap_or(u64::MAX), &mut buf));
            serial.write_str(" ms over a 5 ms sleep — platform time backend live\n");
        } else {
            serial.write_str("enlil kernel: time: enlil-platform Instant self-test FAILED\n");
        }
    }

    /// Discover physical hardware from the firmware ACPI tables and report it.
    ///
    /// Walks the handed-off RSDP → XSDT → MADT (the minimal `no_std` walker in
    /// [`crate::acpi`], since the full `enlil-devices` readers are `std`-only)
    /// and reports the table count and enabled-CPU count — the first real
    /// hardware discovery from firmware tables on bare metal (ROADMAP 6.3).
    fn bring_up_acpi(serial: &SerialPort, handoff: &BootHandoff) -> Option<(u64, u8)> {
        let summary = crate::acpi::discover(handoff.acpi_rsdp)?;
        let mut t = [0u8; 20];
        let mut c = [0u8; 20];
        serial.write_str("enlil kernel: acpi: discovered ");
        serial.write_str(format_u64(summary.tables as u64, &mut t));
        serial.write_str(" tables, ");
        serial.write_str(format_u64(u64::from(summary.enabled_cpus), &mut c));
        serial.write_str(" enabled CPUs (MADT)\n");
        // The AP inventory SMP bring-up (INIT-SIPI-SIPI) targets: the enabled
        // processors' APIC IDs, BSP included.
        serial.write_str("enlil kernel: acpi: APIC IDs");
        for i in 0..summary.apic_id_count {
            let mut id = [0u8; 20];
            serial.write_str(" ");
            serial.write_str(format_u64(u64::from(summary.apic_ids[i]), &mut id));
        }
        serial.write_str(" (SMP AP inventory)\n");
        serial.write_str("enlil kernel: acpi: IOMMU ");
        serial.write_str(summary.iommu.name());
        serial.write_str(" (DMAR/IVRS)\n");
        if summary.ecam_base == 0 {
            return None;
        }
        let mut e = [0u8; 18];
        let mut b = [0u8; 20];
        serial.write_str("enlil kernel: acpi: PCIe ECAM base ");
        serial.write_str(format_u64_hex(summary.ecam_base, &mut e));
        serial.write_str(", buses 0-");
        serial.write_str(format_u64(u64::from(summary.ecam_end_bus), &mut b));
        serial.write_str(" (MCFG)\n");
        Some((summary.ecam_base, summary.ecam_end_bus))
    }

    /// Enumerate the full PCI Express topology through the `ECAM` window (all
    /// buses, extended config space) — reads MMIO the kernel's identity map
    /// covers (the `ECAM` window is below 4 GiB), the mechanism passthrough
    /// needs.
    fn bring_up_ecam(serial: &SerialPort, ecam_base: u64, end_bus: u8) {
        // SAFETY: ecam_base is the firmware ECAM base from the MCFG and its
        // window is identity-mapped by bring_up_paging (installed above).
        let scan = unsafe { crate::pci::scan_ecam(ecam_base, end_bus) };
        let mut f = [0u8; 20];
        let mut b = [0u8; 20];
        serial.write_str("enlil kernel: pci: ECAM scan ");
        serial.write_str(format_u64(u64::from(scan.functions), &mut f));
        serial.write_str(" functions across ");
        serial.write_str(format_u64(u64::from(scan.buses_in_use), &mut b));
        serial.write_str(" buses\n");
    }

    /// Install the kernel's own identity page tables and switch `CR3` off the
    /// firmware's (which live in reclaimable boot-services memory) — host
    /// page-table management (ROADMAP 6.2).
    ///
    /// Reaching the report line proves the map is correct: the kernel's code,
    /// stack, heap, ACPI region, and framebuffer are all covered, or the `CR3`
    /// reload would have faulted. Every later step runs on these tables.
    ///
    /// The span is derived from the real memory map (highest usable RAM) and the
    /// framebuffer, floored at 4 GiB — so it stays correct on hosts with RAM or
    /// MMIO above 4 GiB, not just this QEMU layout.
    fn bring_up_paging(
        serial: &SerialPort,
        handoff: &BootHandoff,
        highest_usable_end: u64,
    ) -> Option<crate::paging::HostMap> {
        let (fb_base, fb_size) = handoff
            .framebuffer
            .map_or((0, 0), |fb| (fb.base, fb.size_bytes()));
        let span = crate::paging::required_map_bytes(highest_usable_end, fb_base, fb_size);
        // SAFETY: `span` covers the highest usable RAM and the framebuffer
        // (floored at 4 GiB), so the CR3 reload continues execution seamlessly.
        match unsafe { crate::paging::install_identity_map(span) } {
            Some(mut map) => {
                let mut c = [0u8; 18];
                let mut g = [0u8; 20];
                serial.write_str("enlil kernel: paging: own identity tables (");
                serial.write_str(format_u64(crate::paging::map_gib(span), &mut g));
                serial.write_str(" GiB) installed, CR3=");
                serial.write_str(format_u64_hex(map.cr3, &mut c));
                serial.write_str(" — off firmware page tables\n");
                prove_4kib_split(serial, &mut map);
                Some(map)
            }
            None => {
                serial.write_str("enlil kernel: paging: page-table build FAILED\n");
                None
            }
        }
    }

    /// Claim the low-memory page an AP trampoline will be placed in, and prove
    /// it is writable on real hardware (ROADMAP 6.2, SMP).
    ///
    /// Bringing up the application processors starts with `INIT`-`SIPI`-`SIPI`,
    /// and a `SIPI` can only start an AP at a page-aligned address in the low
    /// 1 MiB — so the first unbuilt piece is a page *there* that the firmware
    /// agrees is free. This finds one from the real firmware map rather than
    /// guessing a traditional address (a wrong guess means the AP executes
    /// garbage and triple-faults, which is the open risk on this item), records
    /// it for the bring-up step, and writes/reads a sentinel through it so the
    /// page is known good before an AP is ever pointed at it.
    ///
    /// Sending the IPIs and running a real trampoline is the next slice.
    fn bring_up_ap_trampoline_page(serial: &SerialPort, bytes: &[u8], descriptor_size: usize) {
        let Some(page) = crate::kernel::find_ap_trampoline_page(bytes, descriptor_size) else {
            serial.write_str(
                "enlil kernel: smp: NO conventional page below 1 MiB for an AP trampoline\n",
            );
            return;
        };

        // Prove the page is real, writable memory before an AP runs from it.
        let ptr: *mut u64 =
            core::ptr::with_exposed_provenance_mut(usize::try_from(page).unwrap_or(0));
        const SENTINEL: u64 = 0x5350_1AB0_0757_2A11;
        // SAFETY: `page` is a whole 4 KiB page the firmware map reports as
        // conventional (free) memory, identity-mapped like all low memory, and
        // claimed here for the kernel's exclusive use.
        let readback = unsafe {
            ptr.write_volatile(SENTINEL);
            ptr.read_volatile()
        };
        if readback != SENTINEL {
            serial.write_str("enlil kernel: smp: AP trampoline page NOT WRITABLE\n");
            return;
        }
        // SAFETY: same page, exclusively owned; leave it zeroed for the
        // trampoline that will be copied in.
        unsafe { ptr.write_volatile(0) };

        crate::kernel::ap_trampoline::publish(page);
        let mut p = [0u8; 18];
        let mut v = [0u8; 20];
        serial.write_str("enlil kernel: smp: AP trampoline page at ");
        serial.write_str(format_u64_hex(page, &mut p));
        serial.write_str(" (SIPI vector ");
        serial.write_str(format_u64(page >> 12, &mut v));
        serial.write_str("), writable — reachable by a SIPI\n");
    }

    /// Build the kernel's own stack with a guard page (ROADMAP 6.2).
    ///
    /// Until now the kernel has run on the firmware's stack: it lives in
    /// boot-services memory the kernel means to reclaim, and it has no guard, so
    /// an overflow corrupts whatever is below it silently. This allocates a stack
    /// the kernel owns and unmaps the page below it via the 4 KiB split.
    ///
    /// Returns the stack for the caller to switch onto, or `None` (reported on
    /// serial) if it could not be built — including when the guard page did not
    /// actually come out unmapped, which is checked by translating it through the
    /// live tables rather than touching it (touching it is what must fault).
    fn bring_up_guarded_stack(
        serial: &SerialPort,
        map: &mut crate::paging::HostMap,
    ) -> Option<crate::stack::GuardedStack> {
        let Some(stack) = crate::stack::allocate_guarded_stack(map) else {
            serial.write_str("enlil kernel: stack: guarded stack setup FAILED\n");
            return None;
        };

        let mut b = [0u8; 18];
        let mut g = [0u8; 18];
        let mut k = [0u8; 20];
        serial.write_str("enlil kernel: stack: kernel-owned stack at ");
        serial.write_str(format_u64_hex(stack.base, &mut b));
        serial.write_str(" (");
        serial.write_str(format_u64(crate::stack::usable_bytes() / 1024, &mut k));
        serial.write_str(" KiB usable), guard page ");
        serial.write_str(format_u64_hex(crate::stack::guard_page(stack.base), &mut g));
        serial.write_str(" unmapped — overflow faults\n");
        Some(stack)
    }

    /// Refine one 2 MiB region of the live host map down to 4 KiB pages and
    /// prove on real hardware that the split preserved the mapping (ROADMAP 6.2).
    ///
    /// The bulk of the host map stays huge-page-granular (few tables, few TLB
    /// entries); this is the mixed-granularity step that lets a chosen region be
    /// treated page by page — what a stack guard page needs (leave one page
    /// absent so an overflow faults instead of silently corrupting its
    /// neighbour) and what fine-grained MMIO trapping needs.
    ///
    /// The region split is a private 2 MiB-aligned heap block, so no page of it
    /// is reachable from anywhere else and guarding inside it is safe. The proof
    /// is a write/read-back through the freshly split leaves after the `CR3`
    /// reload: the data reaches the same physical frame it did as a huge page.
    fn prove_4kib_split(serial: &SerialPort, map: &mut crate::paging::HostMap) {
        const TWO_MIB: usize = 2 * 1024 * 1024;
        // A private, 2 MiB-aligned, 2 MiB-long block: exactly one huge leaf.
        let Ok(layout) = Layout::from_size_align(TWO_MIB, TWO_MIB) else {
            return;
        };
        // SAFETY: layout has a nonzero size; alloc returns a block or null.
        let raw = unsafe { alloc::alloc::alloc(layout) };
        if raw.is_null() {
            serial.write_str("enlil kernel: paging: 4 KiB split — no 2 MiB block\n");
            return;
        }
        let region = raw as u64;
        // Write a sentinel through the huge-page mapping first...
        const SENTINEL: u64 = 0xE117_5717_0BE1_5EAFu64;
        let probe = raw.wrapping_add(0x3_000).cast::<u64>();
        // SAFETY: probe is inside the 2 MiB block this call owns and is
        // 8-byte-aligned (the block is 2 MiB-aligned, the offset a page).
        unsafe { probe.write_volatile(SENTINEL) };

        // Guard the last page of the private block — an unmapped page a stack
        // placed here would fault on instead of running off the end.
        let guard = region + (TWO_MIB as u64) - 4096;
        // SAFETY: `guard` is the last page of the block allocated just above,
        // which nothing else references and this kernel never touches.
        let Some(base) = (unsafe { map.split_to_4kib(region, &[guard]) }) else {
            serial.write_str("enlil kernel: paging: 4 KiB split FAILED\n");
            return;
        };
        // ...and read it back through the 4 KiB leaves, after the CR3 reload.
        // SAFETY: same page, still mapped — it is not the guard page.
        let seen = unsafe { probe.read_volatile() };
        let mut b = [0u8; 18];
        let mut gu = [0u8; 18];
        let mut sp = [0u8; 20];
        if seen == SENTINEL {
            serial.write_str("enlil kernel: paging: 2 MiB at ");
            serial.write_str(format_u64_hex(base, &mut b));
            serial.write_str(" split to 4 KiB pages, guard page at ");
            serial.write_str(format_u64_hex(guard, &mut gu));
            serial.write_str(" unmapped, mapping preserved (");
            serial.write_str(format_u64(map.spare_pages_left(), &mut sp));
            serial.write_str(" spare tables left)\n");
        } else {
            serial.write_str("enlil kernel: paging: 4 KiB split LOST THE MAPPING\n");
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
        serial.write_str(" display, ");
        let mut mx = [0u8; 20];
        serial.write_str(format_u64(u64::from(scan.msix_capable), &mut mx));
        serial.write_str(" MSI-X-capable\n");

        // The first memory BAR on bus 0 — the MMIO window a driver / passthrough
        // claims (read-only; no BAR sizing yet).
        let bar = crate::pci::first_memory_bar();
        if bar.base != 0 {
            let mut d = [0u8; 20];
            let mut f = [0u8; 20];
            let mut idx = [0u8; 20];
            let mut base = [0u8; 18];
            serial.write_str("enlil kernel: pci: BAR mem window at 00:");
            serial.write_str(format_u64(u64::from(bar.device), &mut d));
            serial.write_str(".");
            serial.write_str(format_u64(u64::from(bar.function), &mut f));
            serial.write_str(" BAR");
            serial.write_str(format_u64(u64::from(bar.index), &mut idx));
            serial.write_str(" base ");
            serial.write_str(format_u64_hex(bar.base, &mut base));
            let mut size = [0u8; 18];
            serial.write_str(" size ");
            serial.write_str(format_u64_hex(bar.size, &mut size));
            serial.write_str(if bar.is_64 {
                " (64-bit)\n"
            } else {
                " (32-bit)\n"
            });
        } else {
            serial.write_str("enlil kernel: pci: BAR scan — no memory BAR on bus 0\n");
        }
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

    /// Arm the LAPIC timer in **TSC-deadline** mode and prove it fires — the
    /// precise-preemption clock a scheduler quantizes on (ROADMAP 6.2).
    ///
    /// Where [`bring_up_timer`] proves the divided count-down path, this proves
    /// the absolute-deadline path: the next tick is a TSC value, not a divided
    /// count, so preemption lands at a precise point on the monotonic clock. If
    /// the CPU lacks the mode (`CPUID.1:ECX[24]` clear) it is reported, not
    /// treated as a failure. Runs the same bounded sti/wait/cli dance as the
    /// one-shot test, reusing the timer gate + tick counter at [`TIMER_VECTOR`].
    fn bring_up_deadline_timer(serial: &SerialPort) {
        /// Same vector + handler as the one-shot timer test (installed here in
        /// case ordering ever changes); the handler counts ticks + signals EOI.
        const TIMER_VECTOR: u8 = 0x40;
        /// TSC ticks until the deadline. At multi-GHz this is a few ms — long
        /// enough not to fire before `sti`, short enough to land in the wait.
        const DEADLINE_OFFSET: u64 = 50_000_000;
        /// Cap on the wait spins so a non-firing timer is reported, not hung.
        const WAIT_SPINS: u32 = 200_000_000;

        if !crate::apic::tsc_deadline_available() {
            serial.write_str(
                "enlil kernel: apic: TSC-deadline timer mode unavailable (CPUID.1:ECX[24] clear)\n",
            );
            return;
        }
        crate::idt::install_timer_gate(TIMER_VECTOR);
        let before = crate::idt::timer_ticks();
        crate::apic::arm_tsc_deadline_timer(TIMER_VECTOR, DEADLINE_OFFSET);
        // SAFETY: the timer gate is installed and the handler signals EOI; sti
        // only enables delivery of the deadline interrupt we just armed.
        unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };
        let mut spun = 0u32;
        while crate::idt::timer_ticks() == before && spun < WAIT_SPINS {
            spun += 1;
            core::hint::spin_loop();
        }
        // SAFETY: re-mask interrupts before continuing the single-threaded boot.
        unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)) };

        if crate::idt::timer_ticks() > before {
            serial.write_str(
                "enlil kernel: apic: LAPIC TSC-deadline timer fired — precise preemption clock live\n",
            );
        } else {
            serial.write_str("enlil kernel: apic: LAPIC TSC-deadline timer did NOT fire\n");
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
                        report_guest_ram_source(serial);
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

    /// Install the kernel's own GDT + TSS with an IST stack and self-test that
    /// an IST-routed interrupt switches to it — so a fault (e.g. a kernel-stack
    /// overflow) runs on a good stack instead of triple-faulting (6.2).
    fn bring_up_gdt(serial: &SerialPort) {
        if crate::gdt::install_and_selftest() {
            serial.write_str(
                "enlil kernel: gdt: own GDT+TSS loaded, #DF/#PF/#GP on IST1/2/3, IST self-test ok\n",
            );
        } else {
            serial.write_str("enlil kernel: gdt: GDT+TSS/IST self-test FAILED\n");
        }
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
                // Draw the on-screen text banner over the framebuffer and
                // verify the 8x8 text console blits correctly (visible output
                // without a serial cable — the console the service VM will use).
                if crate::framebuffer::draw_text_banner(fb) {
                    serial.write_str("enlil kernel: gop: text console banner drawn + verified\n");
                } else {
                    serial.write_str("enlil kernel: gop: text console self-test FAILED\n");
                }
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
    ///
    /// Returns the installed heap's `(base, size)` so later planning can mark it
    /// in use ([`MemoryMap::reserve_span`]) — the heap is claimed before any
    /// `MemoryMap` exists, so nothing else knows that memory is spoken for.
    fn bring_up_heap(serial: &SerialPort, summary: &MemorySummary) -> Option<(u64, u64)> {
        let size = boot_heap_size(summary.largest_conventional_bytes);
        if size == 0 {
            serial.write_str("enlil kernel: heap: NO conventional region large enough\n");
            return None;
        }
        let base = summary.largest_conventional_base;
        let (Ok(base_usize), Ok(size_usize)) = (usize::try_from(base), usize::try_from(size))
        else {
            serial.write_str("enlil kernel: heap: region beyond addressable range\n");
            return None;
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
        Some((base, size))
    }

    /// Run enlil-platform's bare-metal work-stealing scheduler on real hardware
    /// — the Phase 1.2 payoff.
    ///
    /// The boot kernel now LINKS `enlil-platform` (`no_std` under
    /// `platform-baremetal`) and drives its `BareMetalScheduler` instead of
    /// re-deriving a scheduler, proving both the crate linkage and roadmap 6.2's
    /// "per-CPU work-stealing scheduler as the bare-metal threading backend".
    ///
    /// Submits four tasks out of priority order — each closure heap-boxed via the
    /// kernel's own global allocator, so this also exercises `enlil-platform`'s
    /// `alloc` use across the crate boundary — then drains the queue and checks
    /// all four ran in `Critical`→`Low` priority order (the run queue re-orders
    /// them), the scheduling invariant that lets a vCPU task pre-empt housekeeping.
    fn bring_up_scheduler(serial: &SerialPort) {
        use core::sync::atomic::{AtomicUsize, Ordering};
        use enlil_platform::threading::{BareMetalScheduler, CpuAffinity, Priority, Task};

        /// Priority discriminant each task records as it runs, in run order.
        static ORDER: [AtomicUsize; 4] = [
            AtomicUsize::new(usize::MAX),
            AtomicUsize::new(usize::MAX),
            AtomicUsize::new(usize::MAX),
            AtomicUsize::new(usize::MAX),
        ];
        /// Next free slot in `ORDER`.
        static CURSOR: AtomicUsize = AtomicUsize::new(0);

        let sched = BareMetalScheduler::new(1);
        // Submitted out of priority order; a correct run queue dispatches them
        // Critical(0) → High(1) → Normal(2) → Low(3) regardless.
        let prios = [
            Priority::Low,
            Priority::Critical,
            Priority::Normal,
            Priority::High,
        ];
        for prio in prios {
            let task = Task::new("boot-sched-probe", prio, CpuAffinity::Any, move || {
                let slot = CURSOR.fetch_add(1, Ordering::Relaxed);
                if let Some(cell) = ORDER.get(slot) {
                    cell.store(prio as usize, Ordering::Relaxed);
                }
            });
            if sched.submit(task).is_err() {
                serial.write_str("enlil kernel: sched: submit FAILED\n");
                return;
            }
        }
        let mut ran = 0u32;
        while sched.run_one(0) {
            ran += 1;
        }
        // A correct priority dispatch records discriminant `i` in slot `i`.
        let ordered = ORDER
            .iter()
            .enumerate()
            .all(|(i, cell)| cell.load(Ordering::Relaxed) == i);
        if ran == 4 && ordered {
            serial.write_str(
                "enlil kernel: sched: enlil-platform BareMetalScheduler ran 4 tasks in Critical->Low order — Phase 1.2 linkage + bare-metal scheduler live\n",
            );
        } else {
            serial.write_str("enlil kernel: sched: scheduler self-test FAILED\n");
        }
    }

    /// Build enlil-platform's `MemoryMap` from the real firmware map and carve
    /// the hypervisor's physical regions — roadmap 1.3 "wire the boot payload to
    /// collect the real UEFI map and feed it".
    ///
    /// Rebuilds the typed `UefiMemoryDescriptor` array from the same raw firmware
    /// bytes the kernel's own alloc-free [`summarize_memory_map`] walked, feeds it
    /// to [`MemoryMap::from_uefi`], and cross-checks the two independent readers
    /// agree on total usable RAM. Then runs `plan_hypervisor_regions` to carve a
    /// low DMA window, the hypervisor heap, and one disjoint per-guest RAM span
    /// (LOCKED PRINCIPLE 5 — isolation), reporting their bases. Conservative sizes
    /// so the plan fits QEMU's default RAM as well as real hardware.
    fn bring_up_memory_plan(
        serial: &SerialPort,
        bytes: &[u8],
        descriptor_size: usize,
        summary: &MemorySummary,
        heap_span: Option<(u64, u64)>,
    ) {
        use enlil_platform::memory::map::{MemoryMap, MemoryPlanRequest, UefiMemoryDescriptor};

        const MIB: u64 = 1024 * 1024;

        let mut descriptors: Vec<UefiMemoryDescriptor> = Vec::new();
        if descriptor_size >= DESC_MIN_SIZE {
            for desc in bytes.chunks_exact(descriptor_size) {
                let (Some(kind), Some(phys_start), Some(page_count)) = (
                    read_u32_le(desc, DESC_TYPE_OFFSET),
                    read_u64_le(desc, DESC_PHYS_START_OFFSET),
                    read_u64_le(desc, DESC_NUM_PAGES_OFFSET),
                ) else {
                    break;
                };
                descriptors.push(UefiMemoryDescriptor {
                    kind,
                    phys_start,
                    page_count,
                });
            }
        }

        let mut map = MemoryMap::from_uefi(&descriptors);
        let total_usable = map.total_usable();
        let largest = map.largest_usable().map_or(0, |r| r.size);
        // Two independent readers of the same firmware map must agree.
        let agrees = total_usable == summary.usable_bytes;

        let mut dcnt = [0u8; 20];
        let mut umib = [0u8; 20];
        let mut lmib = [0u8; 20];
        serial.write_str("enlil kernel: memplan: enlil-platform MemoryMap from ");
        serial.write_str(format_u64(
            u64::try_from(descriptors.len()).unwrap_or(u64::MAX),
            &mut dcnt,
        ));
        serial.write_str(" UEFI descriptors — ");
        serial.write_str(format_u64(total_usable / MIB, &mut umib));
        serial.write_str(" MiB usable (largest ");
        serial.write_str(format_u64(largest / MIB, &mut lmib));
        serial.write_str(if agrees {
            " MiB), agrees with kernel summary\n"
        } else {
            " MiB), DISAGREES with kernel summary\n"
        });

        // The bootstrap heap was claimed from the raw firmware map before any
        // MemoryMap existed, so the map still calls that span usable. Mark it in
        // use or the plan below will carve guest RAM straight out of the memory
        // the hypervisor is allocating from — silent corruption the moment a
        // guest writes to its RAM.
        if let Some((heap_base, heap_size)) = heap_span {
            let mut hb = [0u8; 18];
            let mut he = [0u8; 18];
            serial.write_str(if map.reserve_span(heap_base, heap_size) {
                "enlil kernel: memplan: reserved live heap "
            } else {
                "enlil kernel: memplan: live heap NOT reservable "
            });
            serial.write_str(format_u64_hex(heap_base, &mut hb));
            serial.write_str("-");
            serial.write_str(format_u64_hex(heap_base + heap_size, &mut he));
            serial.write_str(" — planner cannot carve over it\n");
        }

        let req = MemoryPlanRequest {
            heap_size: 8 * MIB,
            dma_size: 2 * MIB,
            guest_ram_sizes: alloc::vec![16 * MIB],
            align: 2 * MIB,
        };
        match map.plan_hypervisor_regions(&req) {
            Some(regions) => {
                let mut dma_buf = [0u8; 18];
                let mut heap_buf = [0u8; 18];
                let mut guest_buf = [0u8; 18];
                serial.write_str("enlil kernel: memplan: carved DMA@");
                serial.write_str(format_u64_hex(regions.dma.base.as_u64(), &mut dma_buf));
                serial.write_str(" heap@");
                serial.write_str(format_u64_hex(regions.heap.base.as_u64(), &mut heap_buf));
                serial.write_str(" guest0@");
                serial.write_str(format_u64_hex(
                    regions.guest_ram.first().map_or(0, |r| r.base.as_u64()),
                    &mut guest_buf,
                ));
                serial.write_str(" — disjoint per-guest RAM (LOCKED PRINCIPLE 5)\n");

                // Every carved region must now clear the live heap. Checking it
                // here is what makes the reservation above a proof rather than a
                // claim: before it, guest0 landed inside the heap.
                if let Some((heap_base, heap_size)) = heap_span {
                    let heap_end = heap_base + heap_size;
                    let clears =
                        |base: u64, size: u64| base + size <= heap_base || base >= heap_end;
                    let all_clear = clears(regions.dma.base.as_u64(), regions.dma.size)
                        && clears(regions.heap.base.as_u64(), regions.heap.size)
                        && regions
                            .guest_ram
                            .iter()
                            .all(|r| clears(r.base.as_u64(), r.size));
                    serial.write_str(if all_clear {
                        "enlil kernel: memplan: all carved regions clear the live heap — no overlap\n"
                    } else {
                        "enlil kernel: memplan: CARVED REGION OVERLAPS THE LIVE HEAP\n"
                    });

                    // Publish the guest window only once it is known disjoint
                    // from the heap — otherwise guests would be handed the
                    // hypervisor's own allocator memory.
                    if all_clear {
                        if let Some(g0) = regions.guest_ram.first() {
                            crate::kernel::guest_ram::publish(g0.base.as_u64(), g0.size);
                        }
                    }
                }
            }
            None => {
                serial.write_str("enlil kernel: memplan: region plan did not fit the usable map\n");
            }
        }
    }

    /// Report where the SVM guests' RAM came from — the planned, heap-disjoint
    /// guest region, or the kernel heap as a fallback (ROADMAP 1.3 / 6.2).
    ///
    /// A guest backed by heap memory could reach the hypervisor's own allocator
    /// through its nested mapping; backed by the carved region it cannot
    /// (LOCKED PRINCIPLE 5). The remaining byte count is the proof the guest
    /// actually consumed planned RAM rather than falling back.
    fn report_guest_ram_source(serial: &SerialPort) {
        const MIB: u64 = 1024 * 1024;
        let left = crate::kernel::guest_ram::remaining();
        if left == 0 {
            serial.write_str(
                "enlil kernel: svm: guest RAM from the kernel heap (no planned region)\n",
            );
            return;
        }
        let mut m = [0u8; 20];
        serial.write_str("enlil kernel: svm: guest RAM from the planned region, ");
        serial.write_str(format_u64(left / MIB, &mut m));
        serial.write_str(" MiB left — guests cannot reach the hypervisor heap\n");
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
    fn ap_trampoline_page_comes_from_conventional_low_memory() {
        let stride = 48;
        let bytes = map(&[
            desc(7, 0x1000, 8, stride),      // conventional 0x1000-0x9000
            desc(7, 0x10_0000, 256, stride), // conventional, but above 1 MiB
            desc(2, 0x2_0000, 16, stride),   // loader data below 1 MiB: not free
        ]);
        // The highest whole page inside the low conventional span.
        assert_eq!(find_ap_trampoline_page(&bytes, stride), Some(0x8000));
    }

    #[test]
    fn ap_trampoline_page_is_clipped_to_the_sipi_reachable_window() {
        let stride = 48;
        // A region straddling 1 MiB: only the part below the limit is usable,
        // and the chosen page must lie entirely within it.
        let bytes = map(&[desc(7, 0xF_0000, 64, stride)]); // 0xF0000-0x130000
        let page = find_ap_trampoline_page(&bytes, stride).unwrap();
        assert_eq!(page, 0xF_F000);
        assert!(page >= AP_TRAMPOLINE_MIN);
        assert!(page + 4096 <= AP_TRAMPOLINE_LIMIT, "past a SIPI's reach");
        assert_eq!(page % 4096, 0, "a SIPI vector is a page number");
    }

    #[test]
    fn ap_trampoline_page_avoids_the_ivt_and_bios_data_area() {
        let stride = 48;
        // Conventional memory starting at 0: the IVT/BDA at the bottom is
        // skipped, so nothing below AP_TRAMPOLINE_MIN is ever chosen.
        let bytes = map(&[desc(7, 0, 1, stride)]); // only page 0
        assert_eq!(find_ap_trampoline_page(&bytes, stride), None);
    }

    #[test]
    fn ap_trampoline_page_absent_when_no_low_conventional_memory() {
        let stride = 48;
        let bytes = map(&[
            desc(7, 0x10_0000, 256, stride), // all conventional memory is high
            desc(0, 0x1000, 8, stride),      // low memory is reserved
        ]);
        assert_eq!(find_ap_trampoline_page(&bytes, stride), None);
        // A malformed stride is refused rather than misread.
        assert_eq!(find_ap_trampoline_page(&bytes, 4), None);
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
