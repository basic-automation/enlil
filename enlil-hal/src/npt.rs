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
