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

/// Spare 4 KiB pages appended to the kernel's page-table buffer, available as
/// page tables for later 2 MiB → 4 KiB splits ([`HostMap::split_to_4kib`]).
///
/// Each split of a 2 MiB region into 4 KiB pages consumes exactly one.
pub const SPARE_TABLE_PAGES: u64 = 8;

#[cfg(target_os = "uefi")]
pub use hw::{HostMap, install_identity_map};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{MAP_MAX_BYTES, SPARE_TABLE_PAGES};
    use alloc::alloc::{Layout, alloc_zeroed};
    use enlil_hal::npt::{
        HUGE_2MIB, build_identity_npt_2mib_with_4kib_window, split_npt_2mib_leaf, translate_npt,
    };

    /// Table pages the kernel's identity map needs: PML4 + PDPT + one PD per GiB
    /// (`ceil(span / 1 GiB)`) + one PT for the low 2 MiB slot, which is rendered
    /// at 4 KiB granularity so virtual address 0 (the null page) can be a guard.
    const fn table_pages(span_bytes: u64) -> u64 {
        let num_2mib = span_bytes.div_ceil(2 * 1024 * 1024);
        let num_pd = num_2mib.div_ceil(512);
        2 + num_pd + 1
    }

    /// Build the kernel's own identity page tables covering `span_bytes` and
    /// load them into `CR3`, leaving **virtual address 0 (the null page)
    /// unmapped** as a guard.
    ///
    /// The bulk of the map is cheap 2 MiB huge pages; only the low 2 MiB slot is
    /// rendered at 4 KiB granularity (via
    /// [`build_identity_npt_2mib_with_4kib_window`]) so page 0 can be left
    /// not-present. A stray kernel null-pointer dereference then takes a `#PF`
    /// into the kernel's own handler (already on its IST stack) instead of
    /// silently reading or corrupting physical page 0. The 64-bit kernel never
    /// touches VA 0 — its code, stack, heap, the ACPI region, and the
    /// framebuffer all live well above the first 2 MiB — so the guard is
    /// invisible to normal execution (reaching the post-`CR3` steps is itself
    /// the proof).
    ///
    /// Returns the installed table root (`CR3` value), or `None` if the buffer
    /// could not be allocated or the map built. The buffer is sized to exactly
    /// the tables the span needs and leaked (the page tables must live for the
    /// kernel's lifetime). The kernel runs identity-mapped, so the allocation's
    /// virtual address is its physical address — both the builder's `phys_base`
    /// and the `CR3` value.
    ///
    /// # Safety
    ///
    /// Loading `CR3` replaces the active address space. The caller must ensure
    /// `span_bytes` covers everything the kernel touches next (its code, stack,
    /// heap, the ACPI region, the framebuffer) — pass
    /// [`required_map_bytes`](super::required_map_bytes) — and that nothing the
    /// kernel touches lives in the guarded null page `[0, 4 KiB)`. A short map,
    /// or a live access to the null page, would fault.
    #[must_use]
    pub unsafe fn install_identity_map(span_bytes: u64) -> Option<HostMap> {
        let span = span_bytes.min(MAP_MAX_BYTES);
        // Spare pages ride along so a region can later be refined to 4 KiB
        // without a second allocation (the tables must stay one contiguous
        // buffer for the phys_base-relative walk).
        let pages = usize::try_from(table_pages(span).checked_add(SPARE_TABLE_PAGES)?).ok()?;
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
        // Render the low 2 MiB slot (index 0) at 4 KiB with page 0 a guard; the
        // rest of the span stays 2 MiB huge pages. span >= 4 GiB (the map floor)
        // so slot 0 is always within range.
        let layout_info =
            build_identity_npt_2mib_with_4kib_window(buf, phys_base, span, 0, &[0]).ok()?;
        let cr3 = layout_info.ncr3;

        // SAFETY: cr3 is a freshly built identity map covering `span` bytes of
        // physical memory — the kernel's code/stack/heap/ACPI/framebuffer all
        // lie within it and above the guarded null page, so execution continues
        // seamlessly after the reload.
        unsafe { load_cr3(cr3) };
        Some(HostMap {
            cr3,
            buf,
            next_spare_pa: phys_base + (layout_info.table_count as u64) * 4096,
            spare_end_pa: phys_base + layout.size() as u64,
        })
    }

    /// Load `CR3`, which also flushes every non-global TLB entry.
    ///
    /// # Safety
    ///
    /// `cr3` must be a page-table root mapping everything the kernel touches
    /// next, or the very next instruction fetch faults.
    unsafe fn load_cr3(cr3: u64) {
        // SAFETY: the caller guarantees cr3 maps the running kernel.
        unsafe {
            core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
        }
    }

    /// The kernel's own live host page tables, retained so regions can be
    /// refined after the initial `CR3` install.
    ///
    /// The buffer is leaked (the tables live for the kernel's lifetime), so the
    /// borrow is `'static` and the kernel runs identity-mapped — a table's
    /// virtual address is its physical address.
    pub struct HostMap {
        /// The installed table root (the live `CR3` value).
        pub cr3: u64,
        /// The whole table buffer; `buf[0]` is physical [`Self::cr3`].
        buf: &'static mut [u8],
        /// Physical address of the next unused spare page-table page.
        next_spare_pa: u64,
        /// One past the last byte of the buffer.
        spare_end_pa: u64,
    }

    impl HostMap {
        /// Spare page-table pages still available for a split.
        #[must_use]
        pub const fn spare_pages_left(&self) -> u64 {
            (self.spare_end_pa - self.next_spare_pa) / 4096
        }

        /// Refine the 2 MiB region containing `addr` from one huge page to 512
        /// 4 KiB pages, leaving each address in `guards` **not present** so a
        /// touch of it faults.
        ///
        /// The mapping is otherwise preserved exactly, so the kernel keeps
        /// running across the change. Consumes one spare page-table page and
        /// reloads `CR3` to flush the stale huge-page TLB entry.
        ///
        /// Returns the 2 MiB-aligned base of the split region, or `None` if no
        /// spare page remains or the region is not a huge-page leaf.
        ///
        /// # Safety
        ///
        /// Every address in `guards` must be one the kernel will never touch —
        /// unmapping a live page faults on the next access. Guarding a page
        /// inside a region the caller owns exclusively is the safe use.
        #[must_use]
        pub unsafe fn split_to_4kib(&mut self, addr: u64, guards: &[u64]) -> Option<u64> {
            if self.spare_pages_left() == 0 {
                return None;
            }
            let base = addr & !(HUGE_2MIB - 1);
            let pt_pa = self.next_spare_pa;
            split_npt_2mib_leaf(self.buf, self.cr3, base, pt_pa, guards).ok()?;
            self.next_spare_pa += 4096;
            // The CPU may still hold the 2 MiB translation; a CR3 reload drops it.
            // SAFETY: the split reproduced every mapping except the guards, which
            // the caller guarantees are untouched — the kernel stays mapped.
            unsafe { load_cr3(self.cr3) };
            Some(base)
        }

        /// Resolve `addr` through the live tables, or `None` if it is unmapped.
        ///
        /// Reads the same tables the CPU walks, so it answers whether an address
        /// would fault — the way a guard page is proven absent without touching
        /// it (touching it is exactly what must fault).
        #[must_use]
        pub fn translate(&self, addr: u64) -> Option<u64> {
            translate_npt(self.buf, self.cr3, addr)
        }
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
