//! AMD SVM nested page tables (the `nCR3` a `VMRUN` guest translates its
//! guest-physical addresses through — AMD64 APM Vol. 2 §15.25).
//!
//! Nested page tables use the ordinary x86-64 long-mode page-table format, so
//! this is a plain PML4 → PDPT → PD identity map with 2 MiB huge-page leaves.
//! Every entry — leaf **and** intermediate pointer — sets `U/S = 1`: the nested
//! walk is privilege-checked, so a `U/S = 0` entry anywhere on the path would
//! fault a guest user-mode (CPL 3) access.
//!
//! This is the **bare-metal SVM backend's** copy of the identity-map builder:
//! it writes into a caller-provided physical buffer with no allocator, so it is
//! consumable by the `no_std` boot kernel that links `enlil-hal`. The host/KVM
//! dev path has the general analogue in
//! `enlil-platform::memory::paging::build_npt_identity_map_2mib`; the two
//! converge once Phase 1.2 lets the boot path link `enlil-platform` directly.

/// A 4 KiB page — the size and alignment of every nested page table.
pub const PAGE_SIZE: u64 = 4096;

/// A 2 MiB huge page — the leaf mapping granularity.
pub const HUGE_2MIB: u64 = 2 * 1024 * 1024;

/// The number of 8-byte entries in one 4 KiB table.
pub const TABLE_ENTRIES: u64 = 512;

/// Page-table entry flag bits (shared x86-64 long-mode format).
mod flags {
    /// Entry is present.
    pub const PRESENT: u64 = 1 << 0;
    /// Writable.
    pub const WRITABLE: u64 = 1 << 1;
    /// User-accessible — required on every NPT level (see module docs).
    pub const USER: u64 = 1 << 2;
    /// Maps a 2 MiB page directly (only meaningful in a PD entry).
    pub const HUGE_PAGE: u64 = 1 << 7;
}

/// Bits 51:12 of an entry hold the next table's / the mapped frame's physical
/// address.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Where a built nested-page-table hierarchy lives and what to load into the
/// VMCB's `NESTED_CR3` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NptLayout {
    /// The value to program into `NESTED_CR3` — the PML4's physical address.
    pub ncr3: u64,
    /// Number of 4 KiB tables written (PML4 + PDPT + the PDs).
    pub table_count: usize,
    /// Total bytes the tables occupy (`table_count * 4096`).
    pub bytes: usize,
}

/// Why building a nested map failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NptError {
    /// `bytes_to_map` was zero — nothing to map.
    EmptyRegion,
    /// `phys_base` was not 4 KiB-aligned, so an entry would decode to the
    /// wrong address.
    UnalignedBase,
    /// `spa_base` (the system-physical base the guest's GPA 0 maps to) was not
    /// 2 MiB-aligned, so a huge-page leaf would decode to the wrong frame.
    UnalignedSpaBase,
    /// The map needs more than one PDPT's worth of page directories (> 512 GiB),
    /// which this single-PML4-entry builder does not lay out.
    MapTooLarge {
        /// The number of page directories the map would need.
        num_pd: u64,
    },
    /// The tables would run past the end of the provided buffer.
    TablesExceedBuffer {
        /// Byte offset one past the last table byte.
        needed: usize,
        /// The buffer length.
        have: usize,
    },
    /// A `PML4`/`PDPT`/`PD` entry on the path to a demand-mapped leaf was
    /// absent, or pointed outside the table buffer ([`map_npt_2mib_leaf`]).
    IntermediateNotPresent,
    /// A requested guard-page index was `>= num_pages` — outside the mapped
    /// range ([`build_identity_npt_4kib`]).
    GuardPageOutOfRange {
        /// The offending guard-page index.
        index: u64,
        /// The number of pages the map covers.
        num_pages: u64,
    },
    /// A `PD` entry on the path to a demand-mapped 4 KiB leaf was a 2 MiB
    /// huge-page leaf, not a page-table pointer — the map is huge-page-granular
    /// there and cannot take a 4 KiB leaf without a split ([`map_npt_4kib_leaf`]).
    HugePageOnPath,
    /// The `PD` entry a split targeted was not a present 2 MiB huge-page leaf —
    /// there is nothing to split there ([`split_npt_2mib_leaf`]).
    NotAHugePageLeaf,
}

/// Build a 2 MiB-huge-page identity map of `[0, bytes_to_map)` for use as an
/// SVM guest's nested page table.
///
/// The tables are written into the **front** of `buf`, which the caller says
/// begins at physical address `phys_base` — so `buf[0]` is physical `phys_base`
/// (on the identity-mapped bare-metal kernel a heap allocation's virtual
/// address *is* its physical address, which is what the caller passes). The
/// intermediate pointers therefore encode `phys_base`-relative addresses, and
/// `phys_base` need not be zero (a dedicated heap buffer sits high in RAM). The
/// KVM/host case where the buffer is guest RAM based at GPA 0 is just
/// `phys_base == 0`.
///
/// Returns an [`NptLayout`] whose `ncr3` is the value to program into the
/// VMCB's `NESTED_CR3` field ([`svm::control::NESTED_CR3`](crate::svm::control))
/// — it equals `phys_base`, the PML4's physical address. The mapped region must
/// cover every guest-physical address the guest touches (its code, stack, and
/// any MMIO), which for a flat bring-up guest is the low range holding its code.
///
/// # Errors
///
/// - [`NptError::EmptyRegion`] if `bytes_to_map == 0`;
/// - [`NptError::UnalignedBase`] if `phys_base` is not 4 KiB-aligned;
/// - [`NptError::MapTooLarge`] if the map exceeds 512 GiB;
/// - [`NptError::TablesExceedBuffer`] if the tables do not fit in `buf`.
pub fn build_identity_npt_2mib(
    buf: &mut [u8],
    phys_base: u64,
    bytes_to_map: u64,
) -> Result<NptLayout, NptError> {
    // An identity map is the general builder with the guest's GPA 0 mapped to
    // system-physical 0.
    build_npt_2mib(buf, phys_base, 0, bytes_to_map)
}

/// Build a 2 MiB-huge-page nested page table mapping guest-physical
/// `[0, bytes_to_map)` onto **system-physical** `[spa_base, spa_base + …)` for
/// an SVM guest.
///
/// This is the general form of [`build_identity_npt_2mib`] (which is this with
/// `spa_base == 0`): it lets a guest see its RAM at GPA 0 while that RAM lives
/// at an arbitrary 2 MiB-aligned system-physical `spa_base` — the memory-
/// isolation model (LOCKED PRINCIPLE 5), where each guest's GPA space is a
/// disjoint window of host RAM rather than the hypervisor's own addresses.
///
/// The tables themselves are written into the front of `buf`, which begins at
/// physical `phys_base` (see [`build_identity_npt_2mib`] for the `phys_base`
/// contract); `spa_base` is independent — it is where the *mapped guest RAM*
/// lives, not where the tables live. Every level sets `U/S = 1` (see module
/// docs). Returns an [`NptLayout`] whose `ncr3` (`== phys_base`) is the VMCB
/// `NESTED_CR3` value.
///
/// # Errors
///
/// - [`NptError::EmptyRegion`] if `bytes_to_map == 0`;
/// - [`NptError::UnalignedBase`] if `phys_base` is not 4 KiB-aligned;
/// - [`NptError::UnalignedSpaBase`] if `spa_base` is not 2 MiB-aligned;
/// - [`NptError::MapTooLarge`] if the map exceeds 512 GiB;
/// - [`NptError::TablesExceedBuffer`] if the tables do not fit in `buf`.
pub fn build_npt_2mib(
    buf: &mut [u8],
    phys_base: u64,
    spa_base: u64,
    bytes_to_map: u64,
) -> Result<NptLayout, NptError> {
    if bytes_to_map == 0 {
        return Err(NptError::EmptyRegion);
    }
    if phys_base & (PAGE_SIZE - 1) != 0 {
        return Err(NptError::UnalignedBase);
    }
    if spa_base & (HUGE_2MIB - 1) != 0 {
        return Err(NptError::UnalignedSpaBase);
    }

    let num_2mib = bytes_to_map.div_ceil(HUGE_2MIB);
    let num_pd = num_2mib.div_ceil(TABLE_ENTRIES);
    if num_pd > TABLE_ENTRIES {
        return Err(NptError::MapTooLarge { num_pd });
    }
    // Both counts are ≤ 512 now, so the conversions never saturate.
    let num_pd = usize::try_from(num_pd).unwrap_or(usize::MAX);
    let num_2mib = usize::try_from(num_2mib).unwrap_or(usize::MAX);

    let table_count = 2 + num_pd; // PML4 + PDPT + PDs
    let region_bytes = table_count
        .checked_mul(4096)
        .ok_or(NptError::TablesExceedBuffer {
            needed: usize::MAX,
            have: buf.len(),
        })?;
    if region_bytes > buf.len() {
        return Err(NptError::TablesExceedBuffer {
            needed: region_bytes,
            have: buf.len(),
        });
    }
    // Tables occupy the front of buf; unmapped entries must read not-present.
    for byte in &mut buf[..region_bytes] {
        *byte = 0;
    }

    // NPT sets U/S on every level (see module docs). Buffer offsets are counted
    // from buf[0]; physical addresses are counted from phys_base.
    let table_flags = flags::PRESENT | flags::WRITABLE | flags::USER;
    let pdpt_pa = phys_base + PAGE_SIZE;
    let first_pd_pa = phys_base + 2 * PAGE_SIZE;
    let pdpt_off = 4096;
    let first_pd_off = 2 * 4096;

    // PML4[0] → PDPT (a ≤512 GiB map lives entirely under PML4[0]).
    write_entry(buf, 0, (pdpt_pa & ADDR_MASK) | table_flags);

    // PDPT[k] → PD[k], one PD per gibibyte.
    for k in 0..num_pd {
        let pd_pa = first_pd_pa + (k as u64) * PAGE_SIZE;
        write_entry(buf, pdpt_off + k * 8, (pd_pa & ADDR_MASK) | table_flags);
    }

    // PD[k][j] → the (k*512 + j)-th 2 MiB frame: guest-physical page*2 MiB maps
    // to system-physical spa_base + page*2 MiB (identity when spa_base == 0).
    let leaf_flags = flags::PRESENT | flags::WRITABLE | flags::USER | flags::HUGE_PAGE;
    for page in 0..num_2mib {
        let k = page / 512;
        let j = page % 512;
        let pd_off = first_pd_off + k * 4096;
        let phys = spa_base + (page as u64) * HUGE_2MIB;
        write_entry(buf, pd_off + j * 8, (phys & ADDR_MASK) | leaf_flags);
    }

    Ok(NptLayout {
        ncr3: phys_base,
        table_count,
        bytes: region_bytes,
    })
}

/// Map a single 2 MiB guest page — `gpa` → `spa` — into an NPT that already
/// exists (built by [`build_npt_2mib`]), by writing the leaf into its
/// page directory.
///
/// This is the demand-paging / MMIO-backing step: on a nested page fault the
/// backend allocates a frame and calls this to fill in the missing leaf, then
/// re-`VMRUN`s so the faulting instruction re-executes against the new mapping.
/// The covering `PML4`/`PDPT`/`PD` must already be present — they are for any
/// `gpa` within the reach of the originally-built map's tables (its `PDPT` spans
/// 512 GiB and its `PD` one gibibyte), so a leaf anywhere in the built `PD`'s
/// gibibyte just fills a not-present slot.
///
/// `buf`/`phys_base` are the same table buffer and physical base passed to
/// [`build_npt_2mib`]; the walk converts each level's physical pointer back to a
/// `buf` offset via `phys_base`.
///
/// # Errors
///
/// - [`NptError::UnalignedBase`] if `gpa` or `spa` is not 2 MiB-aligned;
/// - [`NptError::IntermediateNotPresent`] if the `PML4`/`PDPT`/`PD` entry on the
///   path is absent (the covering table was never built) or points outside
///   `buf` (below `phys_base` or past its end).
pub fn map_npt_2mib_leaf(
    buf: &mut [u8],
    phys_base: u64,
    gpa: u64,
    spa: u64,
) -> Result<(), NptError> {
    if gpa & (HUGE_2MIB - 1) != 0 || spa & (HUGE_2MIB - 1) != 0 {
        return Err(NptError::UnalignedBase);
    }
    let pml4_i = ((gpa >> 39) & 0x1FF) as usize;
    let pdpt_i = ((gpa >> 30) & 0x1FF) as usize;
    let pd_i = ((gpa >> 21) & 0x1FF) as usize;

    // PML4[pml4_i] → PDPT.
    let pdpt_off = child_offset(buf, phys_base, 0, pml4_i)?;
    // PDPT[pdpt_i] → PD.
    let pd_off = child_offset(buf, phys_base, pdpt_off, pdpt_i)?;
    // Write the 2 MiB leaf into PD[pd_i].
    let leaf_flags = flags::PRESENT | flags::WRITABLE | flags::USER | flags::HUGE_PAGE;
    write_entry(buf, pd_off + pd_i * 8, (spa & ADDR_MASK) | leaf_flags);
    Ok(())
}

/// Build a **4 KiB-granular** identity map of `num_pages` pages, leaving each
/// index in `guard_pages` **not present** so a touch of it faults.
///
/// Serves as a host page table or a fine-grained nested page table. Where
/// [`build_identity_npt_2mib`] maps at 2 MiB huge-page granularity, this
/// lays a full `PML4 → PDPT → PD → PT` hierarchy with 4 KiB leaves — the
/// granularity a guard page needs (a single unmapped 4 KiB page inside an
/// otherwise-mapped region) and the granularity fine-grained MMIO trapping
/// needs. It covers up to one page directory (512 page tables → 1 GiB), which
/// is ample for the small regions 4 KiB granularity is used for (stacks with
/// guard pages, MMIO windows); the bulk of a host map stays 2 MiB.
///
/// The tables are written into the front of `buf`, which begins at physical
/// `phys_base` (same contract as [`build_identity_npt_2mib`]); leaves identity-
/// map guest/host-physical `page * 4 KiB` to the same physical address. Every
/// level sets `U/S = 1` (see module docs), harmless for a host/CPL-0 walk and
/// required for a guest CPL-3 nested walk. Returns an [`NptLayout`] whose `ncr3`
/// (`== phys_base`) is the value to load into `CR3` (host) or `NESTED_CR3`
/// (guest).
///
/// # Errors
///
/// - [`NptError::EmptyRegion`] if `num_pages == 0`;
/// - [`NptError::UnalignedBase`] if `phys_base` is not 4 KiB-aligned;
/// - [`NptError::MapTooLarge`] if `num_pages` exceeds one PD (> 1 GiB);
/// - [`NptError::GuardPageOutOfRange`] if a guard index is `>= num_pages`;
/// - [`NptError::TablesExceedBuffer`] if the tables do not fit in `buf`.
pub fn build_identity_npt_4kib(
    buf: &mut [u8],
    phys_base: u64,
    num_pages: u64,
    guard_pages: &[u64],
) -> Result<NptLayout, NptError> {
    if num_pages == 0 {
        return Err(NptError::EmptyRegion);
    }
    if phys_base & (PAGE_SIZE - 1) != 0 {
        return Err(NptError::UnalignedBase);
    }
    for &g in guard_pages {
        if g >= num_pages {
            return Err(NptError::GuardPageOutOfRange {
                index: g,
                num_pages,
            });
        }
    }

    let num_pt = num_pages.div_ceil(TABLE_ENTRIES);
    if num_pt > TABLE_ENTRIES {
        return Err(NptError::MapTooLarge { num_pd: num_pt });
    }
    let num_pt = usize::try_from(num_pt).unwrap_or(usize::MAX);
    let num_pages_us = usize::try_from(num_pages).unwrap_or(usize::MAX);

    let table_count = 3 + num_pt; // PML4 + PDPT + PD + PTs
    let region_bytes = table_count
        .checked_mul(4096)
        .ok_or(NptError::TablesExceedBuffer {
            needed: usize::MAX,
            have: buf.len(),
        })?;
    if region_bytes > buf.len() {
        return Err(NptError::TablesExceedBuffer {
            needed: region_bytes,
            have: buf.len(),
        });
    }
    for byte in &mut buf[..region_bytes] {
        *byte = 0;
    }

    let table_flags = flags::PRESENT | flags::WRITABLE | flags::USER;
    let pdpt_off = 4096;
    let directory_off = 2 * 4096;
    let first_pt_off = 3 * 4096;
    let pdpt_pa = phys_base + PAGE_SIZE;
    let directory_pa = phys_base + 2 * PAGE_SIZE;
    let first_pt_pa = phys_base + 3 * PAGE_SIZE;

    // PML4[0] → PDPT, PDPT[0] → PD (a ≤1 GiB map lives under PML4[0]/PDPT[0]).
    write_entry(buf, 0, (pdpt_pa & ADDR_MASK) | table_flags);
    write_entry(buf, pdpt_off, (directory_pa & ADDR_MASK) | table_flags);

    // PD[k] → PT[k], one PT per 512 pages.
    for k in 0..num_pt {
        let pt_pa = first_pt_pa + (k as u64) * PAGE_SIZE;
        write_entry(
            buf,
            directory_off + k * 8,
            (pt_pa & ADDR_MASK) | table_flags,
        );
    }

    // PT[k][j] → the (k*512 + j)-th 4 KiB frame, identity-mapped — unless the
    // page is a guard page, left not-present (zeroed above).
    let leaf_flags = flags::PRESENT | flags::WRITABLE | flags::USER;
    for page in 0..num_pages_us {
        if guard_pages.contains(&(page as u64)) {
            continue;
        }
        let k = page / 512;
        let j = page % 512;
        let pt_off = first_pt_off + k * 4096;
        let phys = (page as u64) * PAGE_SIZE;
        write_entry(buf, pt_off + j * 8, (phys & ADDR_MASK) | leaf_flags);
    }

    Ok(NptLayout {
        ncr3: phys_base,
        table_count,
        bytes: region_bytes,
    })
}

/// Map a single **4 KiB** guest/host page — `gpa` → `spa` — into a 4 KiB-
/// granular map that already exists (built by [`build_identity_npt_4kib`]), by
/// writing the leaf into its page table.
///
/// The 4 KiB analogue of [`map_npt_2mib_leaf`]: the demand-paging / MMIO-backing
/// step at page granularity. On a nested page fault (or to back an MMIO page)
/// the backend fills the missing `PT` leaf and re-`VMRUN`s so the faulting
/// access re-executes against the new mapping. The covering `PML4`/`PDPT`/`PD`/
/// `PT` must already be present — they are for any `gpa` within the reach of the
/// originally-built map (its `PD` spans 1 GiB and each `PT` 2 MiB), so a leaf
/// anywhere in a built `PT`'s 2 MiB just fills a not-present slot (e.g. a page
/// left out as a guard).
///
/// `buf`/`phys_base` are the same table buffer and physical base passed to
/// [`build_identity_npt_4kib`].
///
/// # Errors
///
/// - [`NptError::UnalignedBase`] if `gpa` or `spa` is not 4 KiB-aligned;
/// - [`NptError::HugePageOnPath`] if the `PD` entry is a 2 MiB huge-page leaf
///   (the map is huge-page-granular there — a 4 KiB leaf needs a split);
/// - [`NptError::IntermediateNotPresent`] if a `PML4`/`PDPT`/`PD` entry on the
///   path is absent or points outside `buf`.
pub fn map_npt_4kib_leaf(
    buf: &mut [u8],
    phys_base: u64,
    gpa: u64,
    spa: u64,
) -> Result<(), NptError> {
    if gpa & (PAGE_SIZE - 1) != 0 || spa & (PAGE_SIZE - 1) != 0 {
        return Err(NptError::UnalignedBase);
    }
    let pml4_i = ((gpa >> 39) & 0x1FF) as usize;
    let pdpt_i = ((gpa >> 30) & 0x1FF) as usize;
    let pd_i = ((gpa >> 21) & 0x1FF) as usize;
    let leaf_i = ((gpa >> 12) & 0x1FF) as usize;

    // PML4[pml4_i] → PDPT → PD → PT, following present table pointers.
    let pdpt_off = child_offset(buf, phys_base, 0, pml4_i)?;
    let pd_off = child_offset(buf, phys_base, pdpt_off, pdpt_i)?;
    let leaf_table_off = child_offset(buf, phys_base, pd_off, pd_i)?;
    // Write the 4 KiB leaf into PT[leaf_i] (no HUGE_PAGE bit).
    let leaf_flags = flags::PRESENT | flags::WRITABLE | flags::USER;
    write_entry(
        buf,
        leaf_table_off + leaf_i * 8,
        (spa & ADDR_MASK) | leaf_flags,
    );
    Ok(())
}

/// Replace one 2 MiB huge-page leaf with a full 512-entry page table mapping the
/// same 2 MiB at **4 KiB granularity**, optionally leaving chosen pages absent.
///
/// This is the mixed-granularity step: a map built by [`build_npt_2mib`] stays
/// huge-page-granular in bulk (few tables, few TLB entries) while a single 2 MiB
/// region is refined to 4 KiB so individual pages inside it can be treated
/// separately — a stack guard page left not-present so an overflow faults
/// instead of silently corrupting the neighbouring allocation, or an MMIO page
/// carved out of otherwise-plain RAM for fine-grained trapping.
///
/// The 512 new leaves reproduce the huge page's mapping exactly — the same
/// system-physical frame, at the same offsets, carrying the huge leaf's own
/// flags (minus `HUGE_PAGE`) so cacheability and writability survive the split.
/// Every `gpa` in `guard_gpas` is instead left absent. Afterwards the `PD` entry
/// points at the new table, so [`map_npt_4kib_leaf`] can fill a guard slot back
/// in later.
///
/// `pt_pa` is the physical address of a spare 4 KiB page **inside `buf`** to use
/// as the new page table (the caller sizes its table buffer with room to spare);
/// it is zeroed before use. `buf`/`phys_base` are the same buffer and physical
/// base passed to [`build_npt_2mib`].
///
/// The caller must flush the TLB for the split region afterwards (reloading
/// `CR3`, or `invlpg` per page) — the CPU may still hold the stale huge-page
/// translation.
///
/// # Errors
///
/// - [`NptError::UnalignedBase`] if `gpa` is not 2 MiB-aligned, `pt_pa` is not
///   4 KiB-aligned, or a guard address is not 4 KiB-aligned;
/// - [`NptError::NotAHugePageLeaf`] if the `PD` entry is absent or is a table
///   pointer rather than a 2 MiB leaf;
/// - [`NptError::GuardPageOutOfRange`] if a guard address lies outside the
///   2 MiB region being split;
/// - [`NptError::TablesExceedBuffer`] if `pt_pa` does not lie fully within `buf`;
/// - [`NptError::IntermediateNotPresent`] if a `PML4`/`PDPT` entry on the path is
///   absent or points outside `buf`.
pub fn split_npt_2mib_leaf(
    buf: &mut [u8],
    phys_base: u64,
    gpa: u64,
    pt_pa: u64,
    guard_gpas: &[u64],
) -> Result<(), NptError> {
    if gpa & (HUGE_2MIB - 1) != 0 || pt_pa & (PAGE_SIZE - 1) != 0 {
        return Err(NptError::UnalignedBase);
    }
    // Guard addresses must be 4 KiB-aligned and inside the region being split.
    for &g in guard_gpas {
        if g & (PAGE_SIZE - 1) != 0 {
            return Err(NptError::UnalignedBase);
        }
        let index =
            g.checked_sub(gpa)
                .map(|d| d / PAGE_SIZE)
                .ok_or(NptError::GuardPageOutOfRange {
                    index: 0,
                    num_pages: TABLE_ENTRIES,
                })?;
        if index >= TABLE_ENTRIES {
            return Err(NptError::GuardPageOutOfRange {
                index,
                num_pages: TABLE_ENTRIES,
            });
        }
    }

    // The spare page must lie fully inside the table buffer.
    let new_table_off = pt_pa
        .checked_sub(phys_base)
        .and_then(|d| usize::try_from(d).ok())
        .ok_or(NptError::TablesExceedBuffer {
            needed: 0,
            have: buf.len(),
        })?;
    if new_table_off
        .checked_add(4096)
        .is_none_or(|end| end > buf.len())
    {
        return Err(NptError::TablesExceedBuffer {
            needed: new_table_off.saturating_add(4096),
            have: buf.len(),
        });
    }

    let pml4_i = ((gpa >> 39) & 0x1FF) as usize;
    let pdpt_i = ((gpa >> 30) & 0x1FF) as usize;
    let pd_i = ((gpa >> 21) & 0x1FF) as usize;

    // PML4[pml4_i] → PDPT → PD, following present table pointers.
    let pdpt_off = child_offset(buf, phys_base, 0, pml4_i)?;
    let pd_off = child_offset(buf, phys_base, pdpt_off, pdpt_i)?;

    // The entry being split must be a present 2 MiB leaf.
    let huge = read_entry(buf, pd_off + pd_i * 8)?;
    if huge & flags::PRESENT == 0 || huge & flags::HUGE_PAGE == 0 {
        return Err(NptError::NotAHugePageLeaf);
    }
    let spa_base = huge & ADDR_MASK;
    // Inherit the huge leaf's flags so cacheability/writability survive; the
    // HUGE_PAGE bit means "2 MiB frame" in a PD but "PAT" in a PT, so drop it.
    let leaf_flags = (huge & !ADDR_MASK) & !flags::HUGE_PAGE;

    // Zero the spare page, then lay 512 4 KiB leaves over the same 2 MiB.
    for byte in &mut buf[new_table_off..new_table_off + 4096] {
        *byte = 0;
    }
    for j in 0..TABLE_ENTRIES {
        let page_gpa = gpa + j * PAGE_SIZE;
        if guard_gpas.contains(&page_gpa) {
            continue; // left not-present: a touch faults
        }
        let phys = spa_base + j * PAGE_SIZE;
        let index = usize::try_from(j).unwrap_or(usize::MAX);
        write_entry(
            buf,
            new_table_off + index * 8,
            (phys & ADDR_MASK) | leaf_flags,
        );
    }

    // Re-point the PD entry at the new table (a pointer, so no HUGE_PAGE).
    let table_flags = flags::PRESENT | flags::WRITABLE | flags::USER;
    write_entry(buf, pd_off + pd_i * 8, (pt_pa & ADDR_MASK) | table_flags);
    Ok(())
}

/// Walk the tables and resolve `gpa` to the physical address it maps to, or
/// `None` if any level on the path is not present.
///
/// The read-only counterpart to the builders: it answers "is this address
/// actually mapped, and to what?" by following exactly the path the hardware
/// walker would, handling both a 2 MiB huge-page leaf in the `PD` and a 4 KiB
/// leaf in a `PT` (so it works before and after
/// [`split_npt_2mib_leaf`]), and carrying the offset within the page through.
///
/// This is how a guard page is *proven* rather than assumed: a guarded address
/// resolves to `None` while its neighbours resolve normally. It reads the same
/// in-memory tables the CPU walks, so it also serves as a self-check that a
/// builder wrote what it intended.
///
/// `buf`/`phys_base` are the table buffer and physical base the map was built
/// with.
#[must_use]
pub fn translate_npt(buf: &[u8], phys_base: u64, gpa: u64) -> Option<u64> {
    let pml4_i = ((gpa >> 39) & 0x1FF) as usize;
    let pdpt_i = ((gpa >> 30) & 0x1FF) as usize;
    let pd_i = ((gpa >> 21) & 0x1FF) as usize;
    let leaf_i = ((gpa >> 12) & 0x1FF) as usize;

    let pdpt_off = child_offset(buf, phys_base, 0, pml4_i).ok()?;
    let pd_off = child_offset(buf, phys_base, pdpt_off, pdpt_i).ok()?;

    // The PD entry is either a 2 MiB leaf or a pointer to a PT of 4 KiB leaves.
    let pd_entry = read_entry(buf, pd_off + pd_i * 8).ok()?;
    if pd_entry & flags::PRESENT == 0 {
        return None;
    }
    if pd_entry & flags::HUGE_PAGE != 0 {
        return Some((pd_entry & ADDR_MASK) | (gpa & (HUGE_2MIB - 1)));
    }

    let leaf_table_off = child_offset(buf, phys_base, pd_off, pd_i).ok()?;
    let leaf = read_entry(buf, leaf_table_off + leaf_i * 8).ok()?;
    if leaf & flags::PRESENT == 0 {
        return None;
    }
    Some((leaf & ADDR_MASK) | (gpa & (PAGE_SIZE - 1)))
}

/// Read the present table entry at `table_off + index*8` and return the `buf`
/// offset of the table it points at (its physical address minus `phys_base`).
fn child_offset(
    buf: &[u8],
    phys_base: u64,
    table_off: usize,
    index: usize,
) -> Result<usize, NptError> {
    let entry = read_entry(buf, table_off + index * 8)?;
    if entry & flags::PRESENT == 0 {
        return Err(NptError::IntermediateNotPresent);
    }
    // A table pointer must not be a huge-page leaf — descending into one would
    // treat a 2 MiB frame address as a table base.
    if entry & flags::HUGE_PAGE != 0 {
        return Err(NptError::HugePageOnPath);
    }
    let child_pa = entry & ADDR_MASK;
    let off = child_pa
        .checked_sub(phys_base)
        .and_then(|d| usize::try_from(d).ok())
        .ok_or(NptError::IntermediateNotPresent)?;
    // The child table must lie fully within the buffer.
    if off.checked_add(4096).is_none_or(|end| end > buf.len()) {
        return Err(NptError::IntermediateNotPresent);
    }
    Ok(off)
}

/// Read a little-endian `u64` page-table entry at byte `offset`, or an error if
/// it runs past `buf`.
fn read_entry(buf: &[u8], offset: usize) -> Result<u64, NptError> {
    let raw = buf
        .get(offset..offset + 8)
        .ok_or(NptError::IntermediateNotPresent)?;
    Ok(u64::from_le_bytes(raw.try_into().unwrap_or([0; 8])))
}

/// Write a little-endian `u64` page-table entry at byte `offset` in `buf`.
fn write_entry(buf: &mut [u8], offset: usize, entry: u64) {
    buf[offset..offset + 8].copy_from_slice(&entry.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_entry(buf: &[u8], table_off: usize, index: usize) -> u64 {
        let off = table_off + index * 8;
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[off..off + 8]);
        u64::from_le_bytes(b)
    }

    #[test]
    fn translate_resolves_huge_leaves_4kib_leaves_and_guard_pages() {
        let phys_base = 0x1_0000u64;
        let mut buf = alloc::vec![0u8; 4 * 4096];
        build_identity_npt_2mib(&mut buf, phys_base, 4 * 1024 * 1024).unwrap();

        // Through a 2 MiB huge leaf, offset within the page carried through.
        assert_eq!(translate_npt(&buf, phys_base, 0x1234), Some(0x1234));
        assert_eq!(
            translate_npt(&buf, phys_base, HUGE_2MIB + 0x99),
            Some(HUGE_2MIB + 0x99)
        );
        // Past the built map: nothing there.
        assert_eq!(translate_npt(&buf, phys_base, 8 * HUGE_2MIB), None);

        // After a split the same addresses resolve identically, except the guard.
        let guard = 0x7000u64;
        split_npt_2mib_leaf(&mut buf, phys_base, 0, phys_base + 3 * 4096, &[guard]).unwrap();
        assert_eq!(translate_npt(&buf, phys_base, 0x1234), Some(0x1234));
        assert_eq!(translate_npt(&buf, phys_base, guard), None, "guard mapped");
        assert_eq!(translate_npt(&buf, phys_base, guard + 0x40), None);
        // Its neighbours on both sides are still mapped.
        assert_eq!(translate_npt(&buf, phys_base, guard - 0x1000), Some(0x6000));
        assert_eq!(translate_npt(&buf, phys_base, guard + 0x1000), Some(0x8000));
    }

    #[test]
    fn split_replaces_a_huge_leaf_with_512_identical_4kib_leaves() {
        let phys_base = 0x1_0000u64;
        // 4 MiB map (PML4+PDPT+PD = 3 pages) plus a spare page for the new PT.
        let mut buf = alloc::vec![0u8; 4 * 4096];
        build_identity_npt_2mib(&mut buf, phys_base, 4 * 1024 * 1024).unwrap();
        let pt_pa = phys_base + 3 * 4096;

        // Split the second huge page (GPA 2 MiB), which maps SPA 2 MiB.
        split_npt_2mib_leaf(&mut buf, phys_base, HUGE_2MIB, pt_pa, &[]).unwrap();

        // PD[1] is now a table pointer at pt_pa, not a huge leaf.
        let pd_entry = read_entry(&buf, 0x2000, 1);
        assert_eq!(pd_entry & flags::HUGE_PAGE, 0, "still a huge leaf");
        assert_eq!(pd_entry & ADDR_MASK, pt_pa & ADDR_MASK);

        // All 512 leaves reproduce the huge page's mapping at 4 KiB granularity.
        let uwp = flags::PRESENT | flags::WRITABLE | flags::USER;
        for j in 0..512u64 {
            let leaf = read_entry(&buf, 3 * 4096, usize::try_from(j).unwrap());
            assert_eq!(leaf, (HUGE_2MIB + j * PAGE_SIZE) | uwp, "leaf {j}");
        }
        // PD[0]'s huge page is untouched — the bulk stays 2 MiB-granular.
        assert_ne!(read_entry(&buf, 0x2000, 0) & flags::HUGE_PAGE, 0);
    }

    #[test]
    fn split_leaves_guard_pages_absent_and_map_npt_4kib_leaf_fills_them() {
        let phys_base = 0x1_0000u64;
        let mut buf = alloc::vec![0u8; 4 * 4096];
        build_identity_npt_2mib(&mut buf, phys_base, 4 * 1024 * 1024).unwrap();
        let pt_pa = phys_base + 3 * 4096;

        // Guard the first and last page of the split region.
        let guards = [0u64, 511 * PAGE_SIZE];
        split_npt_2mib_leaf(&mut buf, phys_base, 0, pt_pa, &guards).unwrap();

        assert_eq!(read_entry(&buf, 3 * 4096, 0) & flags::PRESENT, 0);
        assert_eq!(read_entry(&buf, 3 * 4096, 511) & flags::PRESENT, 0);
        // A page between the guards is mapped.
        assert_ne!(read_entry(&buf, 3 * 4096, 1) & flags::PRESENT, 0);

        // The split makes the region 4 KiB-granular, so a guard slot can be
        // filled back in on demand (it used to hit HugePageOnPath).
        map_npt_4kib_leaf(&mut buf, phys_base, 0, 0x5000).unwrap();
        assert_eq!(
            read_entry(&buf, 3 * 4096, 0),
            0x5000 | flags::PRESENT | flags::WRITABLE | flags::USER
        );
    }

    #[test]
    fn split_inherits_the_huge_leafs_flags_without_the_huge_bit() {
        // Hand-build a PD whose leaf carries an extra flag (NO_CACHE, bit 4) to
        // prove cacheability survives the split.
        const NO_CACHE: u64 = 1 << 4;
        let phys_base = 0u64;
        let mut buf = alloc::vec![0u8; 4 * 4096];
        build_identity_npt_2mib(&mut buf, phys_base, 2 * 1024 * 1024).unwrap();
        let huge = read_entry(&buf, 0x2000, 0) | NO_CACHE;
        buf[0x2000..0x2008].copy_from_slice(&huge.to_le_bytes());

        split_npt_2mib_leaf(&mut buf, phys_base, 0, 3 * 4096, &[]).unwrap();

        let leaf = read_entry(&buf, 3 * 4096, 7);
        assert_ne!(leaf & NO_CACHE, 0, "NO_CACHE lost in the split");
        assert_eq!(leaf & flags::HUGE_PAGE, 0, "HUGE_PAGE means PAT in a PT");
        assert_eq!(leaf & ADDR_MASK, 7 * PAGE_SIZE);
    }

    #[test]
    fn split_rejects_bad_input() {
        let phys_base = 0u64;
        let mut buf = alloc::vec![0u8; 4 * 4096];
        build_identity_npt_2mib(&mut buf, phys_base, 4 * 1024 * 1024).unwrap();
        let pt_pa = 3 * 4096;

        // Not 2 MiB-aligned.
        assert_eq!(
            split_npt_2mib_leaf(&mut buf, phys_base, 0x1000, pt_pa, &[]),
            Err(NptError::UnalignedBase)
        );
        // Spare page not 4 KiB-aligned.
        assert_eq!(
            split_npt_2mib_leaf(&mut buf, phys_base, 0, pt_pa + 8, &[]),
            Err(NptError::UnalignedBase)
        );
        // Guard outside the 2 MiB region.
        assert_eq!(
            split_npt_2mib_leaf(&mut buf, phys_base, 0, pt_pa, &[HUGE_2MIB]),
            Err(NptError::GuardPageOutOfRange {
                index: 512,
                num_pages: 512,
            })
        );
        // Spare page past the end of the buffer.
        assert!(matches!(
            split_npt_2mib_leaf(&mut buf, phys_base, 0, 8 * 4096, &[]),
            Err(NptError::TablesExceedBuffer { .. })
        ));
        // Nothing to split: GPA 4 MiB is beyond the built map, so PD[2] is absent.
        assert_eq!(
            split_npt_2mib_leaf(&mut buf, phys_base, 4 * HUGE_2MIB, pt_pa, &[]),
            Err(NptError::NotAHugePageLeaf)
        );
        // Splitting the same leaf twice: the second sees a table pointer.
        split_npt_2mib_leaf(&mut buf, phys_base, 0, pt_pa, &[]).unwrap();
        assert_eq!(
            split_npt_2mib_leaf(&mut buf, phys_base, 0, pt_pa, &[]),
            Err(NptError::NotAHugePageLeaf)
        );
    }

    #[test]
    fn identity_npt_sets_user_on_every_level() {
        // A non-zero physical base (a heap buffer high in RAM); ncr3 and the
        // intermediate pointers must reflect it.
        let phys_base = 0x1_0000u64;
        let mut buf = alloc::vec![0u8; 0x1_4000];
        // 4 MiB → two 2 MiB leaves in one PD.
        let layout = build_identity_npt_2mib(&mut buf, phys_base, 4 * 1024 * 1024).unwrap();
        assert_eq!(layout.ncr3, phys_base);
        assert_eq!(layout.table_count, 3); // PML4 + PDPT + 1 PD
        assert_eq!(layout.bytes, 3 * 4096);

        // Tables live at buf offsets 0 (PML4), 0x1000 (PDPT), 0x2000 (PD); the
        // pointers encode phys_base + offset.
        let uwp = flags::PRESENT | flags::WRITABLE | flags::USER;
        assert_eq!(
            read_entry(&buf, 0, 0),
            ((phys_base + 0x1000) & ADDR_MASK) | uwp
        );
        assert_eq!(
            read_entry(&buf, 0x1000, 0),
            ((phys_base + 0x2000) & ADDR_MASK) | uwp
        );
        // Leaves identity-map GPA 0 and 2 MiB (independent of phys_base).
        assert_eq!(read_entry(&buf, 0x2000, 0), uwp | flags::HUGE_PAGE);
        assert_eq!(
            read_entry(&buf, 0x2000, 1),
            HUGE_2MIB | uwp | flags::HUGE_PAGE
        );
        // PD[2] untouched (not present).
        assert_eq!(read_entry(&buf, 0x2000, 2) & flags::PRESENT, 0);
    }

    #[test]
    fn one_gib_fills_a_single_pd() {
        let mut buf = alloc::vec![0u8; 0x4000];
        let layout = build_identity_npt_2mib(&mut buf, 0, 1024 * 1024 * 1024).unwrap();
        assert_eq!(layout.table_count, 3);
        let uwp = flags::PRESENT | flags::WRITABLE | flags::USER | flags::HUGE_PAGE;
        // PD at buf offset 0x2000; the last leaf maps 511 * 2 MiB.
        assert_eq!(read_entry(&buf, 0x2000, 511), (511 * HUGE_2MIB) | uwp);
    }

    #[test]
    fn non_identity_map_points_leaves_at_the_spa_window() {
        // Guest GPA [0, 4 MiB) → system-physical [0x40_0000, 0x60_0000): the
        // guest sees its RAM at GPA 0 but it lives at a disjoint SPA window.
        let phys_base = 0x1_0000u64;
        let spa_base = 0x40_0000u64; // 4 MiB, 2 MiB-aligned
        let mut buf = alloc::vec![0u8; 0x3000];
        let layout = build_npt_2mib(&mut buf, phys_base, spa_base, 4 * 1024 * 1024).unwrap();
        assert_eq!(layout.ncr3, phys_base);
        let uwp = flags::PRESENT | flags::WRITABLE | flags::USER | flags::HUGE_PAGE;
        // GPA 0 → SPA spa_base; GPA 2 MiB → SPA spa_base + 2 MiB.
        assert_eq!(read_entry(&buf, 0x2000, 0), (spa_base & ADDR_MASK) | uwp);
        assert_eq!(
            read_entry(&buf, 0x2000, 1),
            ((spa_base + HUGE_2MIB) & ADDR_MASK) | uwp
        );
        // Intermediate pointers still encode phys_base (where the tables live),
        // not spa_base (where the guest RAM lives).
        let uwp_ptr = flags::PRESENT | flags::WRITABLE | flags::USER;
        assert_eq!(
            read_entry(&buf, 0, 0),
            ((phys_base + 0x1000) & ADDR_MASK) | uwp_ptr
        );
    }

    #[test]
    fn identity_map_equals_non_identity_with_zero_spa_base() {
        // build_identity_npt_2mib must be byte-identical to build_npt_2mib with
        // spa_base == 0 (it delegates).
        let mut a = alloc::vec![0u8; 0x3000];
        let mut b = alloc::vec![0u8; 0x3000];
        build_identity_npt_2mib(&mut a, 0x1_0000, 4 * 1024 * 1024).unwrap();
        build_npt_2mib(&mut b, 0x1_0000, 0, 4 * 1024 * 1024).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn demand_map_fills_a_not_present_leaf() {
        // Build a 2 MiB map (only PD[0] present), then demand-map GPA 2 MiB.
        let phys_base = 0x1_0000u64;
        let mut buf = alloc::vec![0u8; 0x3000];
        build_npt_2mib(&mut buf, phys_base, 0x40_0000, 2 * 1024 * 1024).unwrap();
        // GPA 2 MiB → PD[1], currently not present.
        assert_eq!(read_entry(&buf, 0x2000, 1) & flags::PRESENT, 0);
        let uwp = flags::PRESENT | flags::WRITABLE | flags::USER | flags::HUGE_PAGE;
        map_npt_2mib_leaf(&mut buf, phys_base, 0x20_0000, 0x80_0000).unwrap();
        // PD[1] now maps GPA 2 MiB → SPA 8 MiB; PD[0] is untouched.
        assert_eq!(read_entry(&buf, 0x2000, 1), (0x80_0000 & ADDR_MASK) | uwp);
        assert_eq!(read_entry(&buf, 0x2000, 0), (0x40_0000 & ADDR_MASK) | uwp);
    }

    #[test]
    fn demand_map_rejects_unaligned_and_missing_intermediates() {
        let phys_base = 0x1_0000u64;
        let mut buf = alloc::vec![0u8; 0x3000];
        build_npt_2mib(&mut buf, phys_base, 0, 2 * 1024 * 1024).unwrap();
        // Unaligned GPA/SPA.
        assert_eq!(
            map_npt_2mib_leaf(&mut buf, phys_base, 0x1000, 0x40_0000),
            Err(NptError::UnalignedBase)
        );
        // GPA 1 GiB → PDPT[1], which was never built (not present).
        assert_eq!(
            map_npt_2mib_leaf(&mut buf, phys_base, 1024 * 1024 * 1024, 0x40_0000),
            Err(NptError::IntermediateNotPresent)
        );
    }

    #[test]
    fn npt_4kib_maps_pages_and_leaves_guards_not_present() {
        let phys_base = 0x1_0000u64;
        // 3 pages, page 1 a guard.
        let mut buf = alloc::vec![0u8; 0x4000];
        let layout = build_identity_npt_4kib(&mut buf, phys_base, 3, &[1]).unwrap();
        assert_eq!(layout.ncr3, phys_base);
        assert_eq!(layout.table_count, 4); // PML4 + PDPT + PD + 1 PT
        assert_eq!(layout.bytes, 4 * 4096);

        let uw = flags::PRESENT | flags::WRITABLE | flags::USER;
        // PML4[0] → PDPT, PDPT[0] → PD, PD[0] → PT — all at phys_base + offset.
        assert_eq!(
            read_entry(&buf, 0, 0),
            ((phys_base + 0x1000) & ADDR_MASK) | uw
        );
        assert_eq!(
            read_entry(&buf, 0x1000, 0),
            ((phys_base + 0x2000) & ADDR_MASK) | uw
        );
        assert_eq!(
            read_entry(&buf, 0x2000, 0),
            ((phys_base + 0x3000) & ADDR_MASK) | uw
        );
        // PT (buf offset 0x3000): page 0 and 2 present, page 1 (guard) absent.
        // 4 KiB leaves carry no HUGE_PAGE bit.
        assert_eq!(read_entry(&buf, 0x3000, 0), uw); // GPA 0 → 0
        assert_eq!(read_entry(&buf, 0x3000, 1) & flags::PRESENT, 0); // guard
        assert_eq!(read_entry(&buf, 0x3000, 2), (2 * PAGE_SIZE) | uw);
        assert_eq!(read_entry(&buf, 0x3000, 2) & flags::HUGE_PAGE, 0);
    }

    #[test]
    fn npt_4kib_spans_multiple_page_tables() {
        // 513 pages → two PTs (one full, one with a single page).
        let mut buf = alloc::vec![0u8; 0x6000];
        let layout = build_identity_npt_4kib(&mut buf, 0, 513, &[]).unwrap();
        assert_eq!(layout.table_count, 5); // PML4 + PDPT + PD + 2 PTs
        let uw = flags::PRESENT | flags::WRITABLE | flags::USER;
        // PD[0] → PT0, PD[1] → PT1 (buf offsets 0x3000, 0x4000).
        assert_eq!(read_entry(&buf, 0x2000, 0), (0x3000 & ADDR_MASK) | uw);
        assert_eq!(read_entry(&buf, 0x2000, 1), (0x4000 & ADDR_MASK) | uw);
        // Last page (512) is PT1[0] → 512 * 4 KiB.
        assert_eq!(read_entry(&buf, 0x4000, 0), (512 * PAGE_SIZE) | uw);
        // PT1[1] is beyond num_pages → not present.
        assert_eq!(read_entry(&buf, 0x4000, 1) & flags::PRESENT, 0);
    }

    #[test]
    fn demand_map_4kib_fills_a_guard_page() {
        // Build a 4-page map with page 2 a guard, then demand-map it in.
        let phys_base = 0x1_0000u64;
        let mut buf = alloc::vec![0u8; 0x4000];
        build_identity_npt_4kib(&mut buf, phys_base, 4, &[2]).unwrap();
        // PT (buf 0x3000): page 2 currently not present.
        assert_eq!(read_entry(&buf, 0x3000, 2) & flags::PRESENT, 0);
        // Demand-map GPA 2*4KiB → SPA 0x50_0000.
        let uw = flags::PRESENT | flags::WRITABLE | flags::USER;
        map_npt_4kib_leaf(&mut buf, phys_base, 2 * PAGE_SIZE, 0x50_0000).unwrap();
        assert_eq!(read_entry(&buf, 0x3000, 2), (0x50_0000 & ADDR_MASK) | uw);
        assert_eq!(read_entry(&buf, 0x3000, 2) & flags::HUGE_PAGE, 0);
        // Neighbours untouched.
        assert_eq!(read_entry(&buf, 0x3000, 1), (PAGE_SIZE) | uw);
    }

    #[test]
    fn demand_map_4kib_rejects_unaligned_missing_and_huge_path() {
        let phys_base = 0x1_0000u64;
        // 4 KiB map: PD points at a PT, so the path is page-table-granular.
        let mut buf = alloc::vec![0u8; 0x4000];
        build_identity_npt_4kib(&mut buf, phys_base, 4, &[]).unwrap();
        // Unaligned GPA.
        assert_eq!(
            map_npt_4kib_leaf(&mut buf, phys_base, 0x800, 0x1000),
            Err(NptError::UnalignedBase)
        );
        // GPA 1 GiB → PDPT[1], never built.
        assert_eq!(
            map_npt_4kib_leaf(&mut buf, phys_base, 1024 * 1024 * 1024, 0x1000),
            Err(NptError::IntermediateNotPresent)
        );

        // A 2 MiB map's PD entry is a huge-page leaf → a 4 KiB leaf can't descend.
        let mut huge = alloc::vec![0u8; 0x3000];
        build_npt_2mib(&mut huge, phys_base, 0, 2 * 1024 * 1024).unwrap();
        assert_eq!(
            map_npt_4kib_leaf(&mut huge, phys_base, 0x1000, 0x1000),
            Err(NptError::HugePageOnPath)
        );
    }

    #[test]
    fn npt_4kib_rejects_bad_inputs() {
        let mut buf = alloc::vec![0u8; 0x6000];
        assert_eq!(
            build_identity_npt_4kib(&mut buf, 0, 0, &[]),
            Err(NptError::EmptyRegion)
        );
        assert_eq!(
            build_identity_npt_4kib(&mut buf, 0x800, 1, &[]),
            Err(NptError::UnalignedBase)
        );
        // A guard index outside the mapped range is rejected.
        assert_eq!(
            build_identity_npt_4kib(&mut buf, 0, 4, &[4]),
            Err(NptError::GuardPageOutOfRange {
                index: 4,
                num_pages: 4
            })
        );
        // > 1 GiB (one PD's worth of PTs) is refused.
        assert!(matches!(
            build_identity_npt_4kib(&mut buf, 0, 512 * 512 + 1, &[]),
            Err(NptError::MapTooLarge { .. })
        ));
        // A 3-page map needs 4 tables (16 KiB); a 3-page buffer is too small.
        let mut tiny = alloc::vec![0u8; 0x3000];
        assert!(matches!(
            build_identity_npt_4kib(&mut tiny, 0, 3, &[]),
            Err(NptError::TablesExceedBuffer { .. })
        ));
    }

    #[test]
    fn rejects_bad_inputs() {
        let mut buf = alloc::vec![0u8; 0x4000];
        assert_eq!(
            build_identity_npt_2mib(&mut buf, 0, 0),
            Err(NptError::EmptyRegion)
        );
        assert_eq!(
            build_identity_npt_2mib(&mut buf, 0x800, HUGE_2MIB),
            Err(NptError::UnalignedBase)
        );
        // A non-2 MiB-aligned SPA base is rejected (a huge-page leaf needs it).
        assert_eq!(
            build_npt_2mib(&mut buf, 0, 0x1000, HUGE_2MIB),
            Err(NptError::UnalignedSpaBase)
        );
        // A 4 MiB map needs 3 tables (12 KiB); a 2-page buffer is too small.
        let mut tiny = alloc::vec![0u8; 0x2000];
        assert!(matches!(
            build_identity_npt_2mib(&mut tiny, 0, 4 * 1024 * 1024),
            Err(NptError::TablesExceedBuffer { .. })
        ));
        // > 512 GiB needs more than one PDPT.
        let mut big = alloc::vec![0u8; 0x1000];
        assert!(matches!(
            build_identity_npt_2mib(&mut big, 0, 513 * 1024 * 1024 * 1024),
            Err(NptError::MapTooLarge { .. })
        ));
    }
}
