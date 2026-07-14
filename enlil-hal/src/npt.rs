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

/// Why building a nested identity map failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NptError {
    /// `bytes_to_map` was zero — nothing to map.
    EmptyRegion,
    /// `tables_base` was not 4 KiB-aligned, so an entry would decode to the
    /// wrong address.
    UnalignedBase,
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
}

/// Build a 2 MiB-huge-page identity map of `[0, bytes_to_map)` for use as an
/// SVM guest's nested page table.
///
/// Writes the tables into `buf` at `tables_base` (a physical address; on the
/// identity-mapped bare-metal kernel the buffer's virtual address *is* its
/// physical address).
///
/// Returns an [`NptLayout`] whose `ncr3` is the value to program into the
/// VMCB's `NESTED_CR3` field ([`svm::control::NESTED_CR3`](crate::svm::control)).
/// The mapped region must cover every guest-physical address the guest touches
/// (its code, stack, and any MMIO), which for a flat bring-up guest is the low
/// range holding its code and these very tables.
///
/// # Errors
///
/// - [`NptError::EmptyRegion`] if `bytes_to_map == 0`;
/// - [`NptError::UnalignedBase`] if `tables_base` is not 4 KiB-aligned;
/// - [`NptError::MapTooLarge`] if the map exceeds 512 GiB;
/// - [`NptError::TablesExceedBuffer`] if the tables do not fit in `buf`.
pub fn build_identity_npt_2mib(
    buf: &mut [u8],
    tables_base: u64,
    bytes_to_map: u64,
) -> Result<NptLayout, NptError> {
    if bytes_to_map == 0 {
        return Err(NptError::EmptyRegion);
    }
    if tables_base & (PAGE_SIZE - 1) != 0 {
        return Err(NptError::UnalignedBase);
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
    let base = usize::try_from(tables_base).unwrap_or(usize::MAX);
    let region_bytes = table_count
        .checked_mul(4096)
        .ok_or(NptError::TablesExceedBuffer {
            needed: usize::MAX,
            have: buf.len(),
        })?;
    let end = base
        .checked_add(region_bytes)
        .ok_or(NptError::TablesExceedBuffer {
            needed: usize::MAX,
            have: buf.len(),
        })?;
    if end > buf.len() {
        return Err(NptError::TablesExceedBuffer {
            needed: end,
            have: buf.len(),
        });
    }
    // Unmapped entries must read back not-present.
    for byte in &mut buf[base..end] {
        *byte = 0;
    }

    // NPT sets U/S on every level (see module docs).
    let table_flags = flags::PRESENT | flags::WRITABLE | flags::USER;
    let pdpt_pa = tables_base + PAGE_SIZE;
    let first_pd_pa = tables_base + 2 * PAGE_SIZE;
    let pdpt_off = base + 4096;
    let first_pd_off = base + 2 * 4096;

    // PML4[0] → PDPT (a ≤512 GiB map lives entirely under PML4[0]).
    write_entry(buf, base, (pdpt_pa & ADDR_MASK) | table_flags);

    // PDPT[k] → PD[k], one PD per gibibyte.
    for k in 0..num_pd {
        let pd_pa = first_pd_pa + (k as u64) * PAGE_SIZE;
        write_entry(buf, pdpt_off + k * 8, (pd_pa & ADDR_MASK) | table_flags);
    }

    // PD[k][j] → the (k*512 + j)-th 2 MiB frame.
    let leaf_flags = flags::PRESENT | flags::WRITABLE | flags::USER | flags::HUGE_PAGE;
    for page in 0..num_2mib {
        let k = page / 512;
        let j = page % 512;
        let pd_off = first_pd_off + k * 4096;
        let phys = (page as u64) * HUGE_2MIB;
        write_entry(buf, pd_off + j * 8, (phys & ADDR_MASK) | leaf_flags);
    }

    Ok(NptLayout {
        ncr3: tables_base,
        table_count,
        bytes: region_bytes,
    })
}

/// Write a little-endian `u64` page-table entry at byte `offset` in `buf`.
fn write_entry(buf: &mut [u8], offset: usize, entry: u64) {
    buf[offset..offset + 8].copy_from_slice(&entry.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_entry(buf: &[u8], table_pa: u64, index: usize) -> u64 {
        let off = usize::try_from(table_pa).expect("test offset fits") + index * 8;
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[off..off + 8]);
        u64::from_le_bytes(b)
    }

    #[test]
    fn identity_npt_sets_user_on_every_level() {
        let base = 0x1_0000u64;
        let mut buf = alloc::vec![0u8; 0x1_4000];
        // 4 MiB → two 2 MiB leaves in one PD.
        let layout = build_identity_npt_2mib(&mut buf, base, 4 * 1024 * 1024).unwrap();
        assert_eq!(layout.ncr3, base);
        assert_eq!(layout.table_count, 3); // PML4 + PDPT + 1 PD
        assert_eq!(layout.bytes, 3 * 4096);

        let pml4 = base;
        let pdpt = base + 0x1000;
        let pd = base + 0x2000;
        let uwp = flags::PRESENT | flags::WRITABLE | flags::USER;
        assert_eq!(read_entry(&buf, pml4, 0), (pdpt & ADDR_MASK) | uwp);
        assert_eq!(read_entry(&buf, pdpt, 0), (pd & ADDR_MASK) | uwp);
        // Leaves: huge page at 0 and at 2 MiB, USER + WRITABLE set.
        assert_eq!(read_entry(&buf, pd, 0), uwp | flags::HUGE_PAGE);
        assert_eq!(read_entry(&buf, pd, 1), HUGE_2MIB | uwp | flags::HUGE_PAGE);
        // PD[2] untouched (not present).
        assert_eq!(read_entry(&buf, pd, 2) & flags::PRESENT, 0);
    }

    #[test]
    fn one_gib_fills_a_single_pd() {
        let mut buf = alloc::vec![0u8; 0x4000];
        let layout = build_identity_npt_2mib(&mut buf, 0, 1024 * 1024 * 1024).unwrap();
        assert_eq!(layout.table_count, 3);
        let pd = 0x2000u64;
        let uwp = flags::PRESENT | flags::WRITABLE | flags::USER | flags::HUGE_PAGE;
        assert_eq!(read_entry(&buf, pd, 511), (511 * HUGE_2MIB) | uwp);
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
