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

/// How much physical address space the kernel identity-maps: the low 4 GiB.
///
/// Covers this workstation/QEMU config end to end — RAM, the ACPI tables, and
/// the GOP framebuffer (at 2 GiB under q35) all live below 4 GiB. A follow-up
/// should derive the exact span from the memory map + framebuffer base for
/// hosts with RAM or MMIO above 4 GiB.
pub const IDENTITY_MAP_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Whole GiB an [`IDENTITY_MAP_BYTES`]-style byte count spans (for reporting).
#[must_use]
pub const fn map_gib(bytes: u64) -> u64 {
    bytes / (1024 * 1024 * 1024)
}

#[cfg(target_os = "uefi")]
pub use hw::install_identity_map;

#[cfg(target_os = "uefi")]
mod hw {
    use super::IDENTITY_MAP_BYTES;
    use alloc::alloc::{Layout, alloc_zeroed};
    use enlil_hal::npt::build_identity_npt_2mib;

    /// A page-table buffer for the low-4 GiB 2 MiB map: PML4 + PDPT + 4 PDs = 6
    /// pages; 8 gives headroom. 4 KiB-aligned so it is a valid table root.
    #[repr(C, align(4096))]
    struct PageTables([u8; 8 * 4096]);

    /// Build the kernel's own identity page tables and load them into `CR3`,
    /// returning the installed table root (`CR3` value), or `None` if the table
    /// buffer could not be allocated or built.
    ///
    /// The buffer is leaked: the page tables must live for the kernel's
    /// lifetime. The kernel runs identity-mapped, so the allocation's virtual
    /// address is its physical address — both the builder's `phys_base` and the
    /// `CR3` value.
    ///
    /// # Safety
    ///
    /// Loading `CR3` replaces the active address space. The caller must ensure
    /// the map covers everything the kernel touches next (its code, stack, heap,
    /// the ACPI region, and the framebuffer) — [`IDENTITY_MAP_BYTES`] does for a
    /// sub-4 GiB layout. A short map would fault on the first unmapped access.
    #[must_use]
    pub unsafe fn install_identity_map() -> Option<u64> {
        // SAFETY: PageTables is a nonzero, 4 KiB-aligned block; alloc_zeroed
        // yields it zeroed or null.
        let raw = unsafe { alloc_zeroed(Layout::new::<PageTables>()) };
        if raw.is_null() {
            return None;
        }
        let phys_base = raw as u64;
        // SAFETY: raw owns the PageTables bytes; it is leaked below so the slice
        // never outlives the allocation.
        let buf =
            unsafe { core::slice::from_raw_parts_mut(raw, core::mem::size_of::<PageTables>()) };
        let cr3 = build_identity_npt_2mib(buf, phys_base, IDENTITY_MAP_BYTES)
            .ok()?
            .ncr3;

        // SAFETY: cr3 is a freshly built identity map covering IDENTITY_MAP_BYTES
        // of physical memory — the kernel's code/stack/heap/ACPI/framebuffer all
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
    fn identity_map_covers_the_low_four_gib() {
        assert_eq!(map_gib(IDENTITY_MAP_BYTES), 4);
    }

    #[test]
    fn map_gib_rounds_down() {
        assert_eq!(map_gib(0), 0);
        assert_eq!(map_gib(1024 * 1024 * 1024 + 1), 1);
        assert_eq!(map_gib(2 * 1024 * 1024 * 1024), 2);
    }
}
