//! x86-64 4-level page-table construction (host page tables, item 1.3).
//!
//! The hypervisor needs identity-mapped long-mode page tables in several
//! places: the guest-boot trampoline that enters a 64-bit payload, and the
//! bare-metal host's own address space (item 6.2). Both hand-rolled the PML4 →
//! PDPT → PD chain inline; this module is the reusable, tested primitive.
//!
//! It builds an **identity map** (guest/host virtual address == physical
//! address) two ways: [`build_identity_map_2mib`] uses 2 MiB huge pages (three
//! levels — PML4 → PDPT → PD huge-leaves — the fast path for a large straight
//! map), while [`build_identity_map_4kib`] uses the full four levels for
//! fine-grained 4 KiB pages and supports **guard pages** (left not-present so a
//! touch faults). The tables are written into a caller-provided memory image at
//! a caller-chosen base, and the returned [`PageTableLayout`] carries the `cr3`
//! value (the PML4 physical address) to load.
//!
//! Everything here is pure address arithmetic over a byte buffer — no backend
//! detail — so it serves both the Linux/KVM dev path and the bare-metal target.

/// Page-table entry flag bits (shared by PML4E / PDPTE / PDE / PTE).
pub mod flags {
    /// The entry maps/points to something (bit 0).
    pub const PRESENT: u64 = 1 << 0;
    /// Writes are allowed (bit 1).
    pub const WRITABLE: u64 = 1 << 1;
    /// User-mode access is allowed (bit 2).
    pub const USER: u64 = 1 << 2;
    /// Page-level write-through (bit 3).
    pub const WRITE_THROUGH: u64 = 1 << 3;
    /// Page-level cache disable (bit 4).
    pub const NO_CACHE: u64 = 1 << 4;
    /// Accessed (bit 5) — set by hardware.
    pub const ACCESSED: u64 = 1 << 5;
    /// Dirty (bit 6) — set by hardware on write.
    pub const DIRTY: u64 = 1 << 6;
    /// Page Size — a PDE/PDPTE with this set is a huge-page leaf (bit 7).
    pub const HUGE_PAGE: u64 = 1 << 7;
    /// Global — not flushed on a CR3 reload (bit 8).
    pub const GLOBAL: u64 = 1 << 8;
    /// No-execute (bit 63) — requires EFER.NXE.
    pub const NO_EXECUTE: u64 = 1 << 63;
}

/// A 4-KiB page-table frame.
const PAGE_SIZE: u64 = 4096;
/// A 4-KiB page-table frame, as a `usize` (for byte-offset arithmetic).
const PAGE_BYTES: usize = 4096;
/// Entries per table (each 8 bytes → 512 per 4-KiB table).
const TABLE_ENTRIES: u64 = 512;
/// A 2-MiB huge page.
const HUGE_2MIB: u64 = 2 * 1024 * 1024;
/// Physical-address bits [51:12] of a table pointer (4-KiB aligned).
const TABLE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Physical-address bits [51:21] of a 2-MiB huge-page frame.
const HUGE_2MIB_ADDR_MASK: u64 = 0x000F_FFFF_FFE0_0000;

/// The PML4 index (bits 47:39) of a virtual address.
#[must_use]
pub const fn pml4_index(virt: u64) -> usize {
    ((virt >> 39) & 0x1FF) as usize
}

/// The PDPT index (bits 38:30) of a virtual address.
#[must_use]
pub const fn pdpt_index(virt: u64) -> usize {
    ((virt >> 30) & 0x1FF) as usize
}

/// The page-directory index (bits 29:21) of a virtual address.
#[must_use]
pub const fn pd_index(virt: u64) -> usize {
    ((virt >> 21) & 0x1FF) as usize
}

/// The page-table index (bits 20:12) of a virtual address.
#[must_use]
pub const fn pt_index(virt: u64) -> usize {
    ((virt >> 12) & 0x1FF) as usize
}

/// A page-table entry pointing at the next-level table at `next_phys` (which
/// must be 4-KiB aligned; low bits are masked off) with `flags`.
#[must_use]
pub const fn table_entry(next_phys: u64, flags: u64) -> u64 {
    (next_phys & TABLE_ADDR_MASK) | flags
}

/// A 2-MiB huge-page leaf entry mapping `phys` (2-MiB aligned; low bits masked)
/// with `flags`; the [`HUGE_PAGE`](flags::HUGE_PAGE) bit is set for you.
#[must_use]
pub const fn huge_2mib_entry(phys: u64, flags: u64) -> u64 {
    (phys & HUGE_2MIB_ADDR_MASK) | flags | flags::HUGE_PAGE
}

/// A 4-KiB page-table entry (PTE) mapping `phys` (4-KiB aligned; low bits
/// masked) with `flags`.
///
/// Unlike [`huge_2mib_entry`] this is a leaf without the
/// [`HUGE_PAGE`](flags::HUGE_PAGE) bit — the terminal level of a 4-KiB map.
#[must_use]
pub const fn pte_entry(phys: u64, flags: u64) -> u64 {
    (phys & TABLE_ADDR_MASK) | flags
}

/// Where a built page-table hierarchy lives and what to load into `CR3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageTableLayout {
    /// The value to load into `CR3` — the PML4's physical (guest) address.
    pub cr3: u64,
    /// Number of 4-KiB tables written (e.g. PML4 + PDPT + the PDs for a 2 MiB
    /// map, or PML4 + PDPT + PD + PT for a 4 KiB map).
    pub table_count: usize,
    /// Total bytes the tables occupy (`table_count * 4096`).
    pub bytes: usize,
}

/// Why building an identity map failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingError {
    /// `bytes_to_map` was zero — nothing to map.
    EmptyRegion,
    /// `tables_base_gpa` was not 4-KiB aligned, so a table pointer would decode
    /// to the wrong address.
    UnalignedBase,
    /// The map needs more than one PDPT's worth of page directories (> 512 GiB),
    /// which this single-PML4-entry builder does not lay out yet.
    MapTooLarge {
        /// The number of page directories the map would need.
        num_pd: u64,
    },
    /// A 4-KiB map requested more pages than a single page table holds (> 512,
    /// i.e. > 2 MiB), which the single-PT 4-KiB builder does not lay out yet.
    TooManyPages {
        /// The number of 4-KiB pages requested.
        requested: u64,
    },
    /// A guard-page index fell outside the mapped range.
    GuardOutOfRange {
        /// The offending page index.
        index: u64,
    },
    /// The tables would run past the end of the memory image.
    TablesExceedMemory {
        /// Byte offset one past the last table byte.
        needed: usize,
        /// The memory image length.
        have: usize,
    },
}

/// Build an identity map of `[0, bytes_to_map)` using 2 MiB huge pages.
///
/// Writes the PML4/PDPT/PD tables into `mem` at `tables_base_gpa`. `leaf_flags`
/// are OR'd into each 2 MiB leaf entry — the builder always sets `PRESENT` on
/// leaves and `PRESENT | WRITABLE` on the intermediate table pointers — so a
/// caller typically passes `flags::WRITABLE` (plus `flags::NO_CACHE` for an
/// MMIO identity map). The table region is zeroed first, so entries the map
/// does not reach read back not-present.
///
/// The tables are laid out contiguously: PML4 at `tables_base_gpa`, the PDPT
/// next, then one PD per gibibyte of mapped range. `mem` is indexed by
/// guest-physical address (based at 0), so `tables_base_gpa` must point at real
/// backing bytes in `mem`.
///
/// # Errors
/// - [`PagingError::EmptyRegion`] if `bytes_to_map == 0`;
/// - [`PagingError::UnalignedBase`] if `tables_base_gpa` is not 4-KiB aligned;
/// - [`PagingError::MapTooLarge`] if the map exceeds 512 GiB (needs more than
///   one PDPT / PML4 entry);
/// - [`PagingError::TablesExceedMemory`] if the tables do not fit in `mem`.
pub fn build_identity_map_2mib(
    mem: &mut [u8],
    tables_base_gpa: u64,
    bytes_to_map: u64,
    leaf_flags: u64,
) -> Result<PageTableLayout, PagingError> {
    if bytes_to_map == 0 {
        return Err(PagingError::EmptyRegion);
    }
    if tables_base_gpa & (PAGE_SIZE - 1) != 0 {
        return Err(PagingError::UnalignedBase);
    }

    let num_2mib = bytes_to_map.div_ceil(HUGE_2MIB);
    let num_pd = num_2mib.div_ceil(TABLE_ENTRIES);
    // One PDPT (512 entries) points at up to 512 PDs → 512 GiB; beyond that we
    // would need more than one PML4 entry.
    if num_pd > TABLE_ENTRIES {
        return Err(PagingError::MapTooLarge { num_pd });
    }
    // Both counts are now ≤ 512, so the conversions never hit the fallback.
    let num_pd = usize::try_from(num_pd).unwrap_or(usize::MAX);
    let num_2mib = usize::try_from(num_2mib).unwrap_or(usize::MAX);

    let table_count = 2 + num_pd; // PML4 + PDPT + PDs
    let (base, region_usize, end) = table_region(tables_base_gpa, table_count, mem.len())?;
    // Unmapped entries must read back not-present.
    mem[base..end].fill(0);

    let table_flags = flags::PRESENT | flags::WRITABLE;
    let pdpt_gpa = tables_base_gpa + PAGE_SIZE;
    let first_pd_gpa = tables_base_gpa + 2 * PAGE_SIZE;
    let pdpt_off = base + PAGE_BYTES;
    let first_pd_off = base + 2 * PAGE_BYTES;

    // PML4[0] → PDPT (a ≤512 GiB identity map lives entirely under PML4[0]).
    write_entry(mem, base, table_entry(pdpt_gpa, table_flags));

    // PDPT[k] → PD[k], one PD per gibibyte.
    for k in 0..num_pd {
        let pd_gpa = first_pd_gpa + (k as u64) * PAGE_SIZE;
        write_entry(mem, pdpt_off + k * 8, table_entry(pd_gpa, table_flags));
    }

    // PD[k][j] → the (k*512 + j)-th 2 MiB physical frame.
    let leaf = leaf_flags | flags::PRESENT;
    for page in 0..num_2mib {
        let k = page / 512;
        let j = page % 512;
        let pd_off = first_pd_off + k * PAGE_BYTES;
        let phys = (page as u64) * HUGE_2MIB;
        write_entry(mem, pd_off + j * 8, huge_2mib_entry(phys, leaf));
    }

    Ok(PageTableLayout {
        cr3: tables_base_gpa,
        table_count,
        bytes: region_usize,
    })
}

/// Validate that `table_count` 4-KiB tables based at `tables_base_gpa` fit in a
/// `mem_len`-byte image, returning `(base, region_bytes, end)` as `usize`
/// offsets, or [`PagingError::TablesExceedMemory`] if they do not.
fn table_region(
    tables_base_gpa: u64,
    table_count: usize,
    mem_len: usize,
) -> Result<(usize, usize, usize), PagingError> {
    let too_big = || PagingError::TablesExceedMemory {
        needed: usize::MAX,
        have: mem_len,
    };
    let base = usize::try_from(tables_base_gpa).map_err(|_| too_big())?;
    let region = table_count.checked_mul(PAGE_BYTES).ok_or_else(too_big)?;
    let end = base.checked_add(region).ok_or_else(too_big)?;
    if end > mem_len {
        return Err(PagingError::TablesExceedMemory {
            needed: end,
            have: mem_len,
        });
    }
    Ok((base, region, end))
}

/// Build a 4-KiB-granular identity map of the first `num_pages` pages, with
/// guard pages.
///
/// Writes the PML4/PDPT/PD/PT tables into `mem` at `tables_base_gpa` mapping
/// `[0, num_pages * 4 KiB)`, and leaves each page in `guard_pages` **not
/// present** so a touch faults — the guard-page half of item 1.3's mmap-like
/// semantics. Unlike [`build_identity_map_2mib`] this uses the full four levels
/// (PML4 → PDPT → PD → PT). This first slice covers up to one page table (512
/// pages / 2 MiB) — enough for a guarded stack or a small fine-grained region;
/// larger 4-KiB maps (multiple PTs) are a later slice. `leaf_flags` are OR'd
/// into each mapped page (with `PRESENT` added); guard pages are written zero.
///
/// # Errors
/// - [`PagingError::EmptyRegion`] if `num_pages == 0`;
/// - [`PagingError::UnalignedBase`] if `tables_base_gpa` is not 4-KiB aligned;
/// - [`PagingError::TooManyPages`] if `num_pages > 512`;
/// - [`PagingError::GuardOutOfRange`] if a guard index is `>= num_pages`;
/// - [`PagingError::TablesExceedMemory`] if the four tables do not fit in `mem`.
pub fn build_identity_map_4kib(
    mem: &mut [u8],
    tables_base_gpa: u64,
    num_pages: u64,
    leaf_flags: u64,
    guard_pages: &[u64],
) -> Result<PageTableLayout, PagingError> {
    // PML4 + PDPT + PD + PT.
    const TABLE_COUNT: usize = 4;

    if num_pages == 0 {
        return Err(PagingError::EmptyRegion);
    }
    if tables_base_gpa & (PAGE_SIZE - 1) != 0 {
        return Err(PagingError::UnalignedBase);
    }
    if num_pages > TABLE_ENTRIES {
        return Err(PagingError::TooManyPages {
            requested: num_pages,
        });
    }
    for &g in guard_pages {
        if g >= num_pages {
            return Err(PagingError::GuardOutOfRange { index: g });
        }
    }
    // num_pages ≤ 512 here, so the conversion never hits the fallback.
    let num_pages = usize::try_from(num_pages).unwrap_or(usize::MAX);

    let (base, region_usize, end) = table_region(tables_base_gpa, TABLE_COUNT, mem.len())?;
    mem[base..end].fill(0);

    let table_flags = flags::PRESENT | flags::WRITABLE;
    let pdpt_gpa = tables_base_gpa + PAGE_SIZE;
    let page_dir_gpa = tables_base_gpa + 2 * PAGE_SIZE;
    let page_table_gpa = tables_base_gpa + 3 * PAGE_SIZE;
    let pt_off = base + 3 * PAGE_BYTES;

    write_entry(mem, base, table_entry(pdpt_gpa, table_flags));
    write_entry(
        mem,
        base + PAGE_BYTES,
        table_entry(page_dir_gpa, table_flags),
    );
    write_entry(
        mem,
        base + 2 * PAGE_BYTES,
        table_entry(page_table_gpa, table_flags),
    );

    let leaf = leaf_flags | flags::PRESENT;
    for page in 0..num_pages {
        if guard_pages.contains(&(page as u64)) {
            continue; // leave the PTE zero → not present → guard page
        }
        let phys = (page as u64) * PAGE_SIZE;
        write_entry(mem, pt_off + page * 8, pte_entry(phys, leaf));
    }

    Ok(PageTableLayout {
        cr3: tables_base_gpa,
        table_count: TABLE_COUNT,
        bytes: region_usize,
    })
}

/// Build a 4-KiB-granular identity map of the first `num_pages` pages spanning
/// **any number of page tables** (up to one PDPT / 512 GiB), with guard pages.
///
/// This is the multi-table generalization of [`build_identity_map_4kib`]: where
/// that primitive stops at a single page table (512 pages / 2 MiB), this lays
/// out one PT per 512 pages and one PD per 512 PTs, so it covers a full 4-KiB
/// identity map up to a single PDPT's reach (512 PDs × 512 PTs × 512 pages =
/// 512 GiB). The table region is laid out contiguously as PML4, PDPT, the PDs,
/// then the PTs. Each page in `guard_pages` is left **not present** so a touch
/// faults; `leaf_flags` are OR'd into every mapped leaf (with `PRESENT` added).
///
/// Use this for a fine-grained region larger than 2 MiB (a big guarded arena, a
/// 4-KiB host mapping); prefer [`build_identity_map_2mib`] when 2 MiB huge-page
/// granularity is acceptable, since it needs far fewer tables.
///
/// # Errors
/// - [`PagingError::EmptyRegion`] if `num_pages == 0`;
/// - [`PagingError::UnalignedBase`] if `tables_base_gpa` is not 4-KiB aligned;
/// - [`PagingError::MapTooLarge`] if the map needs more than one PDPT (> 512
///   GiB, i.e. more than 512 page directories);
/// - [`PagingError::GuardOutOfRange`] if a guard index is `>= num_pages`;
/// - [`PagingError::TablesExceedMemory`] if the tables do not fit in `mem`.
pub fn build_identity_map_4kib_multi(
    mem: &mut [u8],
    tables_base_gpa: u64,
    num_pages: u64,
    leaf_flags: u64,
    guard_pages: &[u64],
) -> Result<PageTableLayout, PagingError> {
    if num_pages == 0 {
        return Err(PagingError::EmptyRegion);
    }
    if tables_base_gpa & (PAGE_SIZE - 1) != 0 {
        return Err(PagingError::UnalignedBase);
    }
    let num_page_tables = num_pages.div_ceil(TABLE_ENTRIES);
    let num_page_dirs = num_page_tables.div_ceil(TABLE_ENTRIES);
    // One PDPT (512 entries) points at up to 512 PDs → 512 GiB; beyond that we
    // would need more than one PML4 entry.
    if num_page_dirs > TABLE_ENTRIES {
        return Err(PagingError::MapTooLarge {
            num_pd: num_page_dirs,
        });
    }
    for &g in guard_pages {
        if g >= num_pages {
            return Err(PagingError::GuardOutOfRange { index: g });
        }
    }
    // All three counts are now ≤ their table bounds, so the conversions never
    // hit the fallback.
    let num_page_tables = usize::try_from(num_page_tables).unwrap_or(usize::MAX);
    let num_page_dirs = usize::try_from(num_page_dirs).unwrap_or(usize::MAX);
    let num_pages = usize::try_from(num_pages).unwrap_or(usize::MAX);

    let table_count = 2 + num_page_dirs + num_page_tables; // PML4 + PDPT + PDs + PTs
    let (base, region_usize, end) = table_region(tables_base_gpa, table_count, mem.len())?;
    // Unmapped/guard entries must read back not-present.
    mem[base..end].fill(0);

    let table_flags = flags::PRESENT | flags::WRITABLE;
    let pdpt_gpa = tables_base_gpa + PAGE_SIZE;
    let first_dir_gpa = tables_base_gpa + 2 * PAGE_SIZE;
    let first_leaf_gpa = first_dir_gpa + (num_page_dirs as u64) * PAGE_SIZE;
    let pdpt_off = base + PAGE_BYTES;
    let first_dir_off = base + 2 * PAGE_BYTES;
    let first_leaf_off = first_dir_off + num_page_dirs * PAGE_BYTES;

    // PML4[0] → PDPT (a ≤512 GiB identity map lives entirely under PML4[0]).
    write_entry(mem, base, table_entry(pdpt_gpa, table_flags));

    // PDPT[k] → PD[k], one PD per 512 page tables (512 GiB).
    for k in 0..num_page_dirs {
        let dir_gpa = first_dir_gpa + (k as u64) * PAGE_SIZE;
        write_entry(mem, pdpt_off + k * 8, table_entry(dir_gpa, table_flags));
    }

    // PD entries: the t-th page table (global index) lives in PD t/512 at slot
    // t%512.
    for t in 0..num_page_tables {
        let leaf_gpa = first_leaf_gpa + (t as u64) * PAGE_SIZE;
        let dir_off = first_dir_off + (t / 512) * PAGE_BYTES;
        write_entry(
            mem,
            dir_off + (t % 512) * 8,
            table_entry(leaf_gpa, table_flags),
        );
    }

    // PT leaves: page p lives in PT p/512 at slot p%512, identity-mapped unless
    // it is a guard page (left zero → not present).
    let leaf = leaf_flags | flags::PRESENT;
    for page in 0..num_pages {
        if guard_pages.contains(&(page as u64)) {
            continue;
        }
        let leaf_off = first_leaf_off + (page / 512) * PAGE_BYTES;
        let phys = (page as u64) * PAGE_SIZE;
        write_entry(mem, leaf_off + (page % 512) * 8, pte_entry(phys, leaf));
    }

    Ok(PageTableLayout {
        cr3: tables_base_gpa,
        table_count,
        bytes: region_usize,
    })
}

/// Write a little-endian `u64` entry at byte offset `off` in `mem`. The caller
/// has validated the whole table region fits in `mem`.
fn write_entry(mem: &mut [u8], off: usize, entry: u64) {
    mem[off..off + 8].copy_from_slice(&entry.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read the little-endian `u64` entry at `table_gpa + index*8`.
    fn read_entry(mem: &[u8], table_gpa: u64, index: usize) -> u64 {
        let off = usize::try_from(table_gpa).unwrap() + index * 8;
        u64::from_le_bytes(mem[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn index_helpers_split_a_virtual_address() {
        // 0x0000_7F3B_C025_1000: PML4=0xFE, PDPT=0xEF, PD=0x1E01>>? compute below.
        let v = (0x0FDu64 << 39) | (0x1AB << 30) | (0x0C7 << 21) | (0x155 << 12);
        assert_eq!(pml4_index(v), 0x0FD);
        assert_eq!(pdpt_index(v), 0x1AB);
        assert_eq!(pd_index(v), 0x0C7);
        assert_eq!(pt_index(v), 0x155);
    }

    #[test]
    fn maps_two_2mib_pages_into_one_pd() {
        let base = 0x1_0000u64;
        let mut mem = vec![0u8; 0x1_4000];
        // 4 MiB → two 2 MiB pages, one PD.
        let layout =
            build_identity_map_2mib(&mut mem, base, 4 * 1024 * 1024, flags::WRITABLE).unwrap();
        assert_eq!(layout.cr3, base);
        assert_eq!(layout.table_count, 3); // PML4 + PDPT + 1 PD
        assert_eq!(layout.bytes, 3 * 4096);

        let pml4 = base;
        let pdpt = base + 0x1000;
        let pd = base + 0x2000;
        // PML4[0] → PDPT, present+writable.
        assert_eq!(
            read_entry(&mem, pml4, 0),
            (pdpt & TABLE_ADDR_MASK) | flags::PRESENT | flags::WRITABLE
        );
        // PDPT[0] → PD.
        assert_eq!(
            read_entry(&mem, pdpt, 0),
            (pd & TABLE_ADDR_MASK) | flags::PRESENT | flags::WRITABLE
        );
        // PD[0] and PD[1] are 2 MiB huge leaves at 0 and 2 MiB.
        assert_eq!(
            read_entry(&mem, pd, 0),
            flags::HUGE_PAGE | flags::PRESENT | flags::WRITABLE
        );
        assert_eq!(
            read_entry(&mem, pd, 1),
            HUGE_2MIB | flags::HUGE_PAGE | flags::PRESENT | flags::WRITABLE
        );
        // PD[2] is untouched (not present).
        assert_eq!(read_entry(&mem, pd, 2) & flags::PRESENT, 0);
    }

    #[test]
    fn exactly_one_gib_fills_a_single_pd() {
        let base = 0u64;
        let mut mem = vec![0u8; 0x4000];
        let layout = build_identity_map_2mib(&mut mem, base, 1024 * 1024 * 1024, 0).unwrap();
        assert_eq!(layout.table_count, 3); // one PD holds all 512 entries
        let pd = base + 0x2000;
        // Last 2 MiB leaf maps 511 * 2 MiB.
        assert_eq!(
            read_entry(&mem, pd, 511),
            huge_2mib_entry(511 * HUGE_2MIB, flags::PRESENT)
        );
    }

    #[test]
    fn crossing_a_gib_boundary_allocates_a_second_pd() {
        let base = 0x1_0000u64;
        let mut mem = vec![0u8; 0x2_0000];
        // 1 GiB + 2 MiB → 513 huge pages → two PDs.
        let bytes = 1024 * 1024 * 1024 + HUGE_2MIB;
        let layout = build_identity_map_2mib(&mut mem, base, bytes, flags::WRITABLE).unwrap();
        assert_eq!(layout.table_count, 4); // PML4 + PDPT + 2 PDs

        let pdpt = base + 0x1000;
        let pd0 = base + 0x2000;
        let pd1 = base + 0x3000;
        assert_eq!(
            read_entry(&mem, pdpt, 1),
            table_entry(pd1, flags::PRESENT | flags::WRITABLE)
        );
        // pd0 is full (last entry present); pd1[0] maps the 513th page at 1 GiB.
        assert_eq!(read_entry(&mem, pd0, 511) & flags::PRESENT, flags::PRESENT);
        assert_eq!(
            read_entry(&mem, pd1, 0),
            huge_2mib_entry(1024 * 1024 * 1024, flags::PRESENT | flags::WRITABLE)
        );
        assert_eq!(read_entry(&mem, pd1, 1) & flags::PRESENT, 0);
    }

    #[test]
    fn leaf_flags_are_applied_to_huge_pages() {
        let mut mem = vec![0u8; 0x4000];
        let layout =
            build_identity_map_2mib(&mut mem, 0, HUGE_2MIB, flags::NO_CACHE | flags::WRITABLE)
                .unwrap();
        let pd = layout.cr3 + 0x2000;
        let entry = read_entry(&mem, pd, 0);
        assert_eq!(entry & flags::NO_CACHE, flags::NO_CACHE);
        assert_eq!(entry & flags::WRITABLE, flags::WRITABLE);
        assert_eq!(entry & flags::PRESENT, flags::PRESENT);
        assert_eq!(entry & flags::HUGE_PAGE, flags::HUGE_PAGE);
    }

    #[test]
    fn maps_4kib_pages_with_a_guard_hole() {
        let base = 0x1_0000u64;
        let mut mem = vec![0u8; 0x1_5000];
        // 8 pages, with page 3 left as a guard (not present).
        let layout = build_identity_map_4kib(&mut mem, base, 8, flags::WRITABLE, &[3]).unwrap();
        assert_eq!(layout.cr3, base);
        assert_eq!(layout.table_count, 4); // PML4 + PDPT + PD + PT

        let pml4 = base;
        let pdpt = base + 0x1000;
        let pd = base + 0x2000;
        let pt = base + 0x3000;
        assert_eq!(
            read_entry(&mem, pml4, 0),
            table_entry(pdpt, flags::PRESENT | flags::WRITABLE)
        );
        assert_eq!(
            read_entry(&mem, pd, 0),
            table_entry(pt, flags::PRESENT | flags::WRITABLE)
        );
        // Page 0 and 2 are present 4-KiB leaves at their identity address.
        assert_eq!(
            read_entry(&mem, pt, 0),
            pte_entry(0, flags::PRESENT | flags::WRITABLE)
        );
        assert_eq!(
            read_entry(&mem, pt, 2),
            pte_entry(2 * 4096, flags::PRESENT | flags::WRITABLE)
        );
        // The guard page (3) is not present.
        assert_eq!(read_entry(&mem, pt, 3), 0);
        assert_eq!(read_entry(&mem, pt, 3) & flags::PRESENT, 0);
        // Pages beyond the mapped 8 are also not present.
        assert_eq!(read_entry(&mem, pt, 8) & flags::PRESENT, 0);
    }

    #[test]
    fn rejects_bad_4kib_inputs() {
        let mut mem = vec![0u8; 0x5000];
        assert_eq!(
            build_identity_map_4kib(&mut mem, 0, 0, 0, &[]),
            Err(PagingError::EmptyRegion)
        );
        assert_eq!(
            build_identity_map_4kib(&mut mem, 0x100, 4, 0, &[]),
            Err(PagingError::UnalignedBase)
        );
        assert!(matches!(
            build_identity_map_4kib(&mut mem, 0, 513, 0, &[]),
            Err(PagingError::TooManyPages { requested: 513 })
        ));
        assert!(matches!(
            build_identity_map_4kib(&mut mem, 0, 4, 0, &[9]),
            Err(PagingError::GuardOutOfRange { index: 9 })
        ));
        let mut tiny = vec![0u8; 0x2000];
        assert!(matches!(
            build_identity_map_4kib(&mut tiny, 0, 4, 0, &[]),
            Err(PagingError::TablesExceedMemory { .. })
        ));
    }

    #[test]
    fn rejects_bad_inputs() {
        let mut mem = vec![0u8; 0x4000];
        assert_eq!(
            build_identity_map_2mib(&mut mem, 0, 0, 0),
            Err(PagingError::EmptyRegion)
        );
        assert_eq!(
            build_identity_map_2mib(&mut mem, 0x800, HUGE_2MIB, 0),
            Err(PagingError::UnalignedBase)
        );
        // Tables do not fit: a tiny image cannot hold even PML4+PDPT+PD.
        let mut tiny = vec![0u8; 0x1000];
        assert!(matches!(
            build_identity_map_2mib(&mut tiny, 0, HUGE_2MIB, 0),
            Err(PagingError::TablesExceedMemory { .. })
        ));
        // > 512 GiB needs more than one PDPT.
        let mut big = vec![0u8; 0x1000];
        assert!(matches!(
            build_identity_map_2mib(&mut big, 0, 513 * 1024 * 1024 * 1024, 0),
            Err(PagingError::MapTooLarge { .. })
        ));
    }

    #[test]
    fn multi_pt_spans_more_than_one_page_table() {
        // 600 pages (> 512) needs two PTs under one PD.
        let base = 0x1_0000u64;
        // table_count = PML4 + PDPT + 1 PD + 2 PTs = 5 → 5 * 0x1000, plus base.
        let mut mem = vec![0u8; 0x2_0000];
        let layout =
            build_identity_map_4kib_multi(&mut mem, base, 600, flags::WRITABLE, &[3, 550]).unwrap();
        assert_eq!(layout.cr3, base);
        assert_eq!(layout.table_count, 5); // PML4 + PDPT + 1 PD + 2 PT

        let pml4 = base;
        let pdpt = base + 0x1000;
        let pd = base + 0x2000;
        let pt0 = base + 0x3000; // first page table (pages 0..512)
        let pt1 = base + 0x4000; // second page table (pages 512..600)

        assert_eq!(
            read_entry(&mem, pml4, 0),
            table_entry(pdpt, flags::PRESENT | flags::WRITABLE)
        );
        assert_eq!(
            read_entry(&mem, pdpt, 0),
            table_entry(pd, flags::PRESENT | flags::WRITABLE)
        );
        // PD[0] → PT0, PD[1] → PT1.
        assert_eq!(
            read_entry(&mem, pd, 0),
            table_entry(pt0, flags::PRESENT | flags::WRITABLE)
        );
        assert_eq!(
            read_entry(&mem, pd, 1),
            table_entry(pt1, flags::PRESENT | flags::WRITABLE)
        );
        // Page 0 in PT0[0], page 511 in PT0[511], page 512 in PT1[0].
        assert_eq!(
            read_entry(&mem, pt0, 0),
            pte_entry(0, flags::PRESENT | flags::WRITABLE)
        );
        assert_eq!(
            read_entry(&mem, pt0, 511),
            pte_entry(511 * 4096, flags::PRESENT | flags::WRITABLE)
        );
        assert_eq!(
            read_entry(&mem, pt1, 0),
            pte_entry(512 * 4096, flags::PRESENT | flags::WRITABLE)
        );
        // Page 599 (last) present in PT1[87]; page 600 (unmapped) not present.
        assert_eq!(
            read_entry(&mem, pt1, 87),
            pte_entry(599 * 4096, flags::PRESENT | flags::WRITABLE)
        );
        assert_eq!(read_entry(&mem, pt1, 88) & flags::PRESENT, 0);
        // Guard pages: 3 (in PT0) and 550 (in PT1[38]) are not present.
        assert_eq!(read_entry(&mem, pt0, 3), 0);
        assert_eq!(read_entry(&mem, pt1, 38), 0);
    }

    #[test]
    fn multi_pt_matches_single_pt_for_small_maps() {
        // For ≤ 512 pages the multi builder must produce the same 4 tables and
        // the same entries as the single-PT primitive.
        let base = 0x1_0000u64;
        let mut a = vec![0u8; 0x1_5000];
        let mut b = vec![0u8; 0x1_5000];
        let la = build_identity_map_4kib(&mut a, base, 8, flags::WRITABLE, &[3]).unwrap();
        let lb = build_identity_map_4kib_multi(&mut b, base, 8, flags::WRITABLE, &[3]).unwrap();
        assert_eq!(la, lb);
        assert_eq!(a, b);
    }

    #[test]
    fn multi_pt_rejects_bad_inputs() {
        let mut mem = vec![0u8; 0x8000];
        assert_eq!(
            build_identity_map_4kib_multi(&mut mem, 0, 0, 0, &[]),
            Err(PagingError::EmptyRegion)
        );
        assert_eq!(
            build_identity_map_4kib_multi(&mut mem, 0x100, 4, 0, &[]),
            Err(PagingError::UnalignedBase)
        );
        assert!(matches!(
            build_identity_map_4kib_multi(&mut mem, 0, 600, 0, &[601]),
            Err(PagingError::GuardOutOfRange { index: 601 })
        ));
        // > 512 GiB (> 512 PDs) needs more than one PDPT: 513 PDs' worth of
        // pages = 513 * 512 * 512.
        let mut tiny = vec![0u8; 0x1000];
        assert!(matches!(
            build_identity_map_4kib_multi(&mut tiny, 0, 513 * 512 * 512, 0, &[]),
            Err(PagingError::MapTooLarge { .. })
        ));
        // Tables do not fit.
        let mut small = vec![0u8; 0x2000];
        assert!(matches!(
            build_identity_map_4kib_multi(&mut small, 0, 600, 0, &[]),
            Err(PagingError::TablesExceedMemory { .. })
        ));
    }
}
