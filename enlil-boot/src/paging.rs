//! Host page-table management for the enlil kernel (Phase 6.2).
//!
//! After `ExitBootServices()` the kernel runs on the firmware's page tables,
//! which live in boot-services memory it is free to reclaim. This module gives
//! the kernel its own: it builds a 2 MiB-huge-page identity map (reusing
//! `enlil-hal`'s page-table builder — the x86-64 host-paging and NPT formats
//! are identical) and loads it into `CR3`, so every later step — the SVM guests,
//! the framebuffer draw — runs on enlil's own tables, not the firmware's.
//!
//! Reaching the post-`CR3` report line is itself the proof the map is correct:
//! the kernel's code, stack, heap, the ACPI tables, and the framebuffer must
//! all be mapped, or the `mov cr3` would fault before the next instruction.

/// The floor on the identity map: always cover at least the low 4 GiB, where
/// the legacy MMIO holes (LAPIC, ECAM, ISA) and typical framebuffers live.
pub const MAP_FLOOR_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The cap on the identity map, bounding the page-table buffer. Hosts needing
/// more than 512 GiB mapped are a future slice (multi-`PDPT` roots).
pub const MAP_MAX_BYTES: u64 = 512 * 1024 * 1024 * 1024;

/// 2 MiB, the huge-page granularity the map is rounded up to.
const HUGE_2MIB: u64 = 2 * 1024 * 1024;

/// The physical span the kernel must identity-map so the `CR3` reload keeps
/// every live access valid.
///
/// The max of the highest usable RAM address and the framebuffer's end, floored
/// at [`MAP_FLOOR_BYTES`], rounded up to a 2 MiB huge page, capped at
/// [`MAP_MAX_BYTES`].
#[must_use]
pub const fn required_map_bytes(highest_usable_end: u64, fb_base: u64, fb_size: u64) -> u64 {
    let fb_end = fb_base.saturating_add(fb_size);
    let mut span = if highest_usable_end > fb_end {
        highest_usable_end
    } else {
        fb_end
    };
    if span < MAP_FLOOR_BYTES {
        span = MAP_FLOOR_BYTES;
    }
    // Round up to a 2 MiB huge page.
    span = span.saturating_add(HUGE_2MIB - 1) & !(HUGE_2MIB - 1);
    if span > MAP_MAX_BYTES {
        MAP_MAX_BYTES
    } else {
        span
    }
}

/// Whole GiB a byte count spans (for reporting).
#[must_use]
pub const fn map_gib(bytes: u64) -> u64 {
    bytes / (1024 * 1024 * 1024)
}

#[cfg(target_os = "uefi")]
pub use hw::install_identity_map;

#[cfg(target_os = "uefi")]
mod hw {
    use super::MAP_MAX_BYTES;
    use alloc::alloc::{Layout, alloc_zeroed};
    use enlil_hal::npt::build_identity_npt_2mib;

    /// Table pages a 2 MiB-huge-page identity map of `span_bytes` needs:
    /// PML4 + PDPT + one PD per GiB (`ceil(span / 1 GiB)`).
    const fn table_pages(span_bytes: u64) -> u64 {
        let num_2mib = span_bytes.div_ceil(2 * 1024 * 1024);
        let num_pd = num_2mib.div_ceil(512);
        2 + num_pd
    }

    /// Build the kernel's own identity page tables covering `span_bytes` and
    /// load them into `CR3`.
    ///
    /// Returns the installed table root (`CR3` value), or `None` if the buffer
    /// could not be allocated or the map built.
    /// The buffer is sized to exactly the tables the span needs and leaked (the
    /// page tables must live for the kernel's lifetime). The kernel runs
    /// identity-mapped, so the allocation's virtual address is its physical
    /// address — both the builder's `phys_base` and the `CR3` value.
    ///
    /// # Safety
    ///
    /// Loading `CR3` replaces the active address space. The caller must ensure
    /// `span_bytes` covers everything the kernel touches next (its code, stack,
    /// heap, the ACPI region, the framebuffer) — pass
    /// [`required_map_bytes`](super::required_map_bytes). A short map would fault
    /// on the first unmapped access.
    #[must_use]
    pub unsafe fn install_identity_map(span_bytes: u64) -> Option<u64> {
        let span = span_bytes.min(MAP_MAX_BYTES);
        let pages = usize::try_from(table_pages(span)).ok()?;
        let layout = Layout::from_size_align(pages.checked_mul(4096)?, 4096).ok()?;
        // SAFETY: layout is a nonzero, 4 KiB-aligned size; alloc_zeroed yields a
        // zeroed block or null.
        let raw = unsafe { alloc_zeroed(layout) };
        if raw.is_null() {
            return None;
        }
        let phys_base = raw as u64;
        // SAFETY: raw owns layout.size() bytes; it is leaked below so the slice
        // never outlives the allocation.
        let buf = unsafe { core::slice::from_raw_parts_mut(raw, layout.size()) };
        let cr3 = build_identity_npt_2mib(buf, phys_base, span).ok()?.ncr3;

        // SAFETY: cr3 is a freshly built identity map covering `span` bytes of
        // physical memory — the kernel's code/stack/heap/ACPI/framebuffer all
        // lie within it, so execution continues seamlessly after the reload.
        unsafe {
            core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
        }
        Some(cr3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_map_floors_at_four_gib() {
        // Small RAM + a sub-4-GiB framebuffer → the 4 GiB floor.
        assert_eq!(
            required_map_bytes(128 * 1024 * 1024, 0x8000_0000, 0x40_0000),
            MAP_FLOOR_BYTES
        );
    }

    #[test]
    fn required_map_covers_ram_above_the_floor() {
        // 8 GiB of RAM → the span grows to cover it (2 MiB-rounded).
        let eight_gib = 8u64 * 1024 * 1024 * 1024;
        assert_eq!(required_map_bytes(eight_gib, 0, 0), eight_gib);
    }

    #[test]
    fn required_map_covers_a_high_framebuffer() {
        // A framebuffer at 5 GiB pulls the span up past the floor.
        let fb_base = 5u64 * 1024 * 1024 * 1024;
        let span = required_map_bytes(1024 * 1024, fb_base, 0x80_0000);
        assert!(span >= fb_base + 0x80_0000, "span = {span}");
        assert_eq!(span % (2 * 1024 * 1024), 0); // 2 MiB-aligned
    }

    #[test]
    fn required_map_is_capped() {
        assert_eq!(required_map_bytes(u64::MAX, u64::MAX, 0), MAP_MAX_BYTES);
    }

    #[test]
    fn map_gib_rounds_down() {
        assert_eq!(map_gib(0), 0);
        assert_eq!(map_gib(1024 * 1024 * 1024 + 1), 1);
        assert_eq!(map_gib(2 * 1024 * 1024 * 1024), 2);
    }
}
