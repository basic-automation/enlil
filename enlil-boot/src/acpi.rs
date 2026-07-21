//! Minimal bare-metal ACPI discovery for the enlil kernel (Phase 6.3).
//!
//! The full ACPI/device-tree readers live in `enlil-devices`, but that is a
//! `std` crate (tokio, anyhow) the `no_std` boot kernel cannot link yet. So the
//! kernel carries this minimal walker — the same shape as its own heap/IDT — to
//! discover physical hardware from the firmware tables the UEFI stage handed
//! off: it validates the RSDP, follows the XSDT, and counts the enabled CPUs in
//! the MADT. The parsing is pure and host-tested against synthetic tables; only
//! the raw-pointer reads of the identity-mapped firmware tables are gated to
//! the firmware target.

/// Whether a byte span's 8-bit checksum is zero (every ACPI table sums to 0).
#[must_use]
pub fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) == 0
}

/// The 8-byte RSDP signature.
pub const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";

/// The XSDT signature.
pub const XSDT_SIGNATURE: &[u8; 4] = b"XSDT";

/// The MADT (APIC) table signature.
pub const MADT_SIGNATURE: &[u8; 4] = b"APIC";

/// The `MCFG` (PCI Express `ECAM`) table signature.
pub const MCFG_SIGNATURE: &[u8; 4] = b"MCFG";

/// The `DMAR` table signature — Intel VT-d DMA remapping.
pub const DMAR_SIGNATURE: &[u8; 4] = b"DMAR";

/// The `IVRS` table signature — AMD-Vi (I/O virtualization) remapping.
pub const IVRS_SIGNATURE: &[u8; 4] = b"IVRS";

/// Which IOMMU the firmware advertises (the DMA-remapping engine Phase 6.4
/// programs for per-guest device isolation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IommuKind {
    /// No IOMMU table found.
    #[default]
    None,
    /// Intel VT-d (a `DMAR` table).
    IntelVtd,
    /// AMD-Vi (an `IVRS` table).
    AmdVi,
}

impl IommuKind {
    /// A short human name for the serial report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::IntelVtd => "Intel VT-d",
            Self::AmdVi => "AMD-Vi",
        }
    }
}

/// Classify an SDT signature as an IOMMU table (or [`IommuKind::None`]).
#[must_use]
pub fn iommu_kind_from_signature(sig: [u8; 4]) -> IommuKind {
    if &sig == DMAR_SIGNATURE {
        IommuKind::IntelVtd
    } else if &sig == IVRS_SIGNATURE {
        IommuKind::AmdVi
    } else {
        IommuKind::None
    }
}

/// Length of an ACPI System Description Table header.
pub const SDT_HEADER_LEN: usize = 36;

/// Read a little-endian `u32` at `off`, or `None` past the end.
fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let raw = bytes.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(raw.try_into().ok()?))
}

/// Read a little-endian `u64` at `off`, or `None` past the end.
fn read_u64(bytes: &[u8], off: usize) -> Option<u64> {
    let raw = bytes.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes(raw.try_into().ok()?))
}

/// Validate an ACPI 2.0+ RSDP and return its XSDT physical address.
///
/// Checks the 8-byte signature, the revision (≥ 2, which guarantees an XSDT),
/// and the first-20-byte ACPI-1.0 checksum, then returns the 64-bit XSDT
/// address at offset 24. Returns `None` if the signature, revision, checksum,
/// or length are wrong.
#[must_use]
pub fn rsdp_xsdt_address(rsdp: &[u8]) -> Option<u64> {
    if rsdp.len() < 33 || &rsdp[..8] != RSDP_SIGNATURE {
        return None;
    }
    // ACPI 2.0+ (revision ≥ 2) is required for the XSDT at offset 24.
    if *rsdp.get(15)? < 2 {
        return None;
    }
    // The ACPI-1.0 checksum covers the first 20 bytes.
    if !checksum_ok(&rsdp[..20]) {
        return None;
    }
    read_u64(rsdp, 24)
}

/// The 4-byte signature of an SDT (its first four bytes), or `None`.
#[must_use]
pub fn sdt_signature(sdt: &[u8]) -> Option<[u8; 4]> {
    sdt.get(..4)?.try_into().ok()
}

/// The declared length of an SDT (the `u32` at offset 4), or `None`.
#[must_use]
pub fn sdt_length(sdt: &[u8]) -> Option<u32> {
    read_u32(sdt, 4)
}

/// The number of 64-bit table pointers an XSDT of `length` bytes holds:
/// `(length - header) / 8`.
#[must_use]
pub const fn xsdt_entry_count(length: u32) -> usize {
    (length as usize).saturating_sub(SDT_HEADER_LEN) / 8
}

/// The `i`th table pointer in an XSDT body, or `None` past the end.
#[must_use]
pub fn xsdt_entry(xsdt: &[u8], i: usize) -> Option<u64> {
    let off = SDT_HEADER_LEN.checked_add(i.checked_mul(8)?)?;
    read_u64(xsdt, off)
}

/// MADT interrupt-controller structure type: Processor Local APIC.
pub const MADT_LOCAL_APIC: u8 = 0;

/// MADT interrupt-controller structure type: Processor Local x2APIC.
pub const MADT_LOCAL_X2APIC: u8 = 9;

/// The "processor enabled" bit in a MADT local-APIC/x2APIC flags word.
pub const MADT_CPU_ENABLED: u32 = 1 << 0;

/// Count the enabled processors in a MADT.
///
/// Walks the interrupt-controller structures after the 44-byte MADT header
/// (36-byte SDT header + 4-byte local-APIC address + 4-byte flags), counting
/// Local APIC (type 0) and Local x2APIC (type 9) entries whose flags mark the
/// processor enabled. Each structure's `length` byte bounds the walk; a
/// zero-length structure stops it (a malformed table cannot spin).
#[must_use]
pub fn madt_enabled_cpu_count(madt: &[u8]) -> u32 {
    const MADT_STRUCTS_OFFSET: usize = 44;
    let mut count = 0;
    let mut off = MADT_STRUCTS_OFFSET;
    while off + 2 <= madt.len() {
        let kind = madt[off];
        let len = madt[off + 1] as usize;
        if len == 0 || off + len > madt.len() {
            break;
        }
        match kind {
            MADT_LOCAL_APIC => {
                // flags at struct offset 4 (u32).
                if let Some(flags) = read_u32(madt, off + 4)
                    && flags & MADT_CPU_ENABLED != 0
                {
                    count += 1;
                }
            }
            MADT_LOCAL_X2APIC => {
                // flags at struct offset 8 (u32).
                if let Some(flags) = read_u32(madt, off + 8)
                    && flags & MADT_CPU_ENABLED != 0
                {
                    count += 1;
                }
            }
            _ => {}
        }
        off += len;
    }
    count
}

/// Collect the enabled processors' APIC / x2APIC IDs from a MADT into `out`,
/// returning the total enabled count (which may exceed `out.len()`).
///
/// The AP inventory the SMP bring-up (INIT-SIPI-SIPI, ROADMAP 6.2) targets: the
/// BSP is one of these IDs, the rest are the application processors to wake.
/// Walks the same interrupt-controller structures as [`madt_enabled_cpu_count`],
/// reading the Local APIC ID (type 0, `u8` at struct offset 3) or the Local
/// x2APIC ID (type 9, `u32` at struct offset 4) of each enabled processor, in
/// table order. Writes up to `out.len()` ids but always returns the true count,
/// so a caller can tell its buffer was too small.
#[must_use]
pub fn madt_enabled_apic_ids(madt: &[u8], out: &mut [u32]) -> usize {
    const MADT_STRUCTS_OFFSET: usize = 44;
    let mut count = 0usize;
    let mut off = MADT_STRUCTS_OFFSET;
    while off + 2 <= madt.len() {
        let kind = madt[off];
        let len = madt[off + 1] as usize;
        if len == 0 || off + len > madt.len() {
            break;
        }
        let id_and_flags = match kind {
            MADT_LOCAL_APIC => madt
                .get(off + 3)
                .map(|&id| u32::from(id))
                .zip(read_u32(madt, off + 4)),
            MADT_LOCAL_X2APIC => read_u32(madt, off + 4).zip(read_u32(madt, off + 8)),
            _ => None,
        };
        if let Some((id, flags)) = id_and_flags
            && flags & MADT_CPU_ENABLED != 0
        {
            if let Some(slot) = out.get_mut(count) {
                *slot = id;
            }
            count += 1;
        }
        off += len;
    }
    count
}

/// The first `ECAM` allocation in an `MCFG`: `(base, segment, start_bus, end_bus)`.
///
/// The `MCFG` body is an 8-byte reserved field then 16-byte allocation entries:
/// `ECAM` base (`u64`) @0, PCI segment group (`u16`) @8, start bus @10, end bus
/// @11. Returns the first entry, or `None` if the table has none.
#[must_use]
pub fn mcfg_first_allocation(mcfg: &[u8]) -> Option<(u64, u16, u8, u8)> {
    const MCFG_ALLOCS_OFFSET: usize = SDT_HEADER_LEN + 8;
    let base = read_u64(mcfg, MCFG_ALLOCS_OFFSET)?;
    let segment = u16::from_le_bytes(
        mcfg.get(MCFG_ALLOCS_OFFSET + 8..MCFG_ALLOCS_OFFSET + 10)?
            .try_into()
            .ok()?,
    );
    let start_bus = *mcfg.get(MCFG_ALLOCS_OFFSET + 10)?;
    let end_bus = *mcfg.get(MCFG_ALLOCS_OFFSET + 11)?;
    Some((base, segment, start_bus, end_bus))
}

/// How many enabled-processor APIC IDs [`AcpiSummary`] records inline.
///
/// The BSP plus a handful of APs — enough to report the SMP inventory on serial
/// without a heap allocation. The true enabled count ([`AcpiSummary::enabled_cpus`])
/// may exceed this; only the first `MAX_REPORTED_APIC_IDS` ids are kept.
pub const MAX_REPORTED_APIC_IDS: usize = 8;

/// What the kernel discovered from the firmware ACPI tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AcpiSummary {
    /// Number of tables the XSDT references.
    pub tables: usize,
    /// Enabled processors counted in the MADT (0 if no MADT was found).
    pub enabled_cpus: u32,
    /// The first [`MAX_REPORTED_APIC_IDS`] enabled processors' APIC IDs — the
    /// AP inventory SMP bring-up targets (ROADMAP 6.2).
    pub apic_ids: [u32; MAX_REPORTED_APIC_IDS],
    /// How many entries of [`apic_ids`](Self::apic_ids) are valid
    /// (`min(enabled_cpus, MAX_REPORTED_APIC_IDS)`).
    pub apic_id_count: usize,
    /// PCI Express `ECAM` base physical address from the `MCFG` (0 if absent).
    pub ecam_base: u64,
    /// Highest PCI bus number the `ECAM` window covers (from the `MCFG`).
    pub ecam_end_bus: u8,
    /// The IOMMU the firmware advertises (`DMAR`/`IVRS`), or `None`.
    pub iommu: IommuKind,
}

#[cfg(target_os = "uefi")]
pub use hw::discover;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        AcpiSummary, IommuKind, MADT_SIGNATURE, MAX_REPORTED_APIC_IDS, MCFG_SIGNATURE,
        SDT_HEADER_LEN, iommu_kind_from_signature, madt_enabled_apic_ids, madt_enabled_cpu_count,
        mcfg_first_allocation, rsdp_xsdt_address, sdt_length, sdt_signature, xsdt_entry,
        xsdt_entry_count,
    };

    /// View `len` bytes of identity-mapped physical memory at `phys`.
    ///
    /// # Safety
    ///
    /// `phys` must point at `len` readable bytes of the live, identity-mapped
    /// firmware ACPI region (the kernel runs on the firmware's identity map).
    const unsafe fn phys_slice<'a>(phys: u64, len: usize) -> &'a [u8] {
        // SAFETY: the caller guarantees a live, readable identity-mapped span.
        unsafe { core::slice::from_raw_parts(phys as *const u8, len) }
    }

    /// Walk the firmware ACPI tables from the handed-off RSDP and summarize
    /// them, or `None` if the RSDP is invalid.
    ///
    /// Validates the RSDP, reads the XSDT (its header first, for the length,
    /// then its full body), counts the referenced tables, and — on finding the
    /// MADT — counts the enabled CPUs. All reads are of the identity-mapped
    /// firmware tables (the kernel has not yet replaced the firmware page
    /// tables), bounded by each table's declared length.
    #[must_use]
    pub fn discover(rsdp_phys: u64) -> Option<AcpiSummary> {
        if rsdp_phys == 0 {
            return None;
        }
        // The RSDP is 36 bytes for ACPI 2.0+.
        // SAFETY: the firmware handed off this RSDP pointer into mapped memory.
        let rsdp = unsafe { phys_slice(rsdp_phys, 36) };
        let xsdt_phys = rsdp_xsdt_address(rsdp)?;

        // Read the XSDT header for its length, then the full table.
        // SAFETY: xsdt_phys came from a validated RSDP; the header is mapped.
        let xsdt_hdr = unsafe { phys_slice(xsdt_phys, SDT_HEADER_LEN) };
        let xsdt_len = sdt_length(xsdt_hdr)?;
        // SAFETY: the XSDT spans xsdt_len mapped bytes from xsdt_phys.
        let xsdt = unsafe { phys_slice(xsdt_phys, xsdt_len as usize) };

        let mut summary = AcpiSummary {
            tables: xsdt_entry_count(xsdt_len),
            enabled_cpus: 0,
            apic_ids: [0; MAX_REPORTED_APIC_IDS],
            apic_id_count: 0,
            ecam_base: 0,
            ecam_end_bus: 0,
            iommu: IommuKind::None,
        };

        // Scan the referenced tables for the MADT (enabled CPUs) and the MCFG
        // (PCIe ECAM window).
        let mut i = 0;
        while let Some(table_phys) = xsdt_entry(xsdt, i) {
            // SAFETY: each XSDT entry points at a mapped SDT; read its header.
            let hdr = unsafe { phys_slice(table_phys, SDT_HEADER_LEN) };
            let Some(sig) = sdt_signature(hdr) else {
                i += 1;
                continue;
            };
            match iommu_kind_from_signature(sig) {
                IommuKind::None => {}
                kind => summary.iommu = kind,
            }
            if let Some(len) = sdt_length(hdr) {
                // SAFETY: the table spans `len` mapped bytes from table_phys.
                let table = unsafe { phys_slice(table_phys, len as usize) };
                if sig == *MADT_SIGNATURE {
                    summary.enabled_cpus = madt_enabled_cpu_count(table);
                    let total = madt_enabled_apic_ids(table, &mut summary.apic_ids);
                    summary.apic_id_count = total.min(MAX_REPORTED_APIC_IDS);
                } else if sig == *MCFG_SIGNATURE
                    && let Some((base, _seg, _start, end)) = mcfg_first_allocation(table)
                {
                    summary.ecam_base = base;
                    summary.ecam_end_bus = end;
                }
            }
            i += 1;
        }
        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal SDT header with `sig` (a 4-byte signature) and `length`.
    fn sdt_header(sig: &[u8], length: u32) -> [u8; SDT_HEADER_LEN] {
        let mut h = [0u8; SDT_HEADER_LEN];
        h[..4].copy_from_slice(sig);
        h[4..8].copy_from_slice(&length.to_le_bytes());
        h
    }

    #[test]
    fn checksum_ok_sums_to_zero() {
        assert!(checksum_ok(&[0x10, 0x20, 0xD0])); // 0x100 = 0 mod 256
        assert!(!checksum_ok(&[0x10, 0x20, 0x00]));
    }

    #[test]
    fn rsdp_returns_the_xsdt_address_when_valid() {
        let mut rsdp = [0u8; 36];
        rsdp[..8].copy_from_slice(RSDP_SIGNATURE);
        rsdp[15] = 2; // revision 2 (ACPI 2.0+)
        rsdp[24..32].copy_from_slice(&0x7B7E_0000u64.to_le_bytes()); // XSDT addr
        // Fix the first-20-byte checksum to 0.
        let sum = rsdp[..20].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        rsdp[9] = rsdp[9].wrapping_sub(sum);
        assert_eq!(rsdp_xsdt_address(&rsdp), Some(0x7B7E_0000));
    }

    #[test]
    fn rsdp_rejects_bad_signature_revision_and_checksum() {
        let mut rsdp = [0u8; 36];
        rsdp[..8].copy_from_slice(RSDP_SIGNATURE);
        rsdp[15] = 2;
        // Checksum is nonzero (all zero XSDT addr, no fixup) → rejected.
        assert_eq!(rsdp_xsdt_address(&rsdp), None);
        // Wrong signature.
        let mut bad = rsdp;
        bad[0] = b'X';
        assert_eq!(rsdp_xsdt_address(&bad), None);
        // ACPI 1.0 revision (0) → no XSDT.
        let mut old = rsdp;
        old[15] = 0;
        assert_eq!(rsdp_xsdt_address(&old), None);
    }

    #[test]
    fn xsdt_entry_count_and_entries() {
        // Header + two 8-byte pointers.
        let length = u32::try_from(SDT_HEADER_LEN + 16).unwrap();
        let mut xsdt = sdt_header(XSDT_SIGNATURE, length).to_vec();
        xsdt.extend_from_slice(&0x1111u64.to_le_bytes());
        xsdt.extend_from_slice(&0x2222u64.to_le_bytes());
        assert_eq!(xsdt_entry_count(length), 2);
        assert_eq!(xsdt_entry(&xsdt, 0), Some(0x1111));
        assert_eq!(xsdt_entry(&xsdt, 1), Some(0x2222));
        assert_eq!(xsdt_entry(&xsdt, 2), None);
    }

    #[test]
    fn madt_counts_only_enabled_apic_and_x2apic() {
        let mut madt = sdt_header(MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]); // local-APIC addr + flags → offset 44
        // Local APIC, enabled (type 0, len 8, flags bit0 set at struct off 4).
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 0, 0, 0x01, 0, 0, 0]);
        // Local APIC, disabled.
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 1, 1, 0x00, 0, 0, 0]);
        // Local x2APIC, enabled (type 9, len 16, flags at struct off 8).
        madt.extend_from_slice(&[
            MADT_LOCAL_X2APIC,
            16,
            0,
            0,
            0,
            0,
            0,
            0,
            0x01,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        // An unrelated structure (I/O APIC, type 1) is ignored.
        madt.extend_from_slice(&[1, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(madt_enabled_cpu_count(&madt), 2);
    }

    #[test]
    fn madt_collects_enabled_apic_ids_in_order() {
        let mut madt = sdt_header(MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]); // local-APIC addr + flags → offset 44
        // Local APIC id 5, enabled (type 0, len 8; id @off+3, flags @off+4).
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 0, 5, 0x01, 0, 0, 0]);
        // Local APIC id 3, disabled → skipped.
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 0, 3, 0x00, 0, 0, 0]);
        // Local x2APIC id 0x1000_0001, enabled (type 9, len 16; id @off+4).
        madt.extend_from_slice(&[
            MADT_LOCAL_X2APIC,
            16,
            0,
            0,
            0x01,
            0x00,
            0x00,
            0x10,
            0x01,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        let mut ids = [0u32; 4];
        let count = madt_enabled_apic_ids(&madt, &mut ids);
        assert_eq!(count, 2);
        assert_eq!(&ids[..count], &[5, 0x1000_0001]);
    }

    #[test]
    fn madt_apic_id_count_exceeds_a_small_buffer() {
        let mut madt = sdt_header(MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]);
        for id in 0..4u8 {
            madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 0, id, 0x01, 0, 0, 0]);
        }
        // Buffer holds only 2 — the true count (4) is still returned.
        let mut ids = [0u32; 2];
        let count = madt_enabled_apic_ids(&madt, &mut ids);
        assert_eq!(count, 4);
        assert_eq!(ids, [0, 1]);
    }

    #[test]
    fn mcfg_reads_the_first_ecam_allocation() {
        let mut mcfg = sdt_header(MCFG_SIGNATURE, 0).to_vec();
        mcfg.extend_from_slice(&[0u8; 8]); // reserved
        // Allocation: base 0xB000_0000, segment 0, start bus 0, end bus 0xFF.
        mcfg.extend_from_slice(&0xB000_0000u64.to_le_bytes());
        mcfg.extend_from_slice(&0u16.to_le_bytes()); // segment
        mcfg.push(0x00); // start bus
        mcfg.push(0xFF); // end bus
        mcfg.extend_from_slice(&[0u8; 4]); // reserved
        assert_eq!(
            mcfg_first_allocation(&mcfg),
            Some((0xB000_0000, 0, 0x00, 0xFF))
        );
    }

    #[test]
    fn iommu_kind_classifies_dmar_and_ivrs() {
        assert_eq!(
            iommu_kind_from_signature(*DMAR_SIGNATURE),
            IommuKind::IntelVtd
        );
        assert_eq!(iommu_kind_from_signature(*IVRS_SIGNATURE), IommuKind::AmdVi);
        assert_eq!(iommu_kind_from_signature(*MADT_SIGNATURE), IommuKind::None);
        assert_eq!(IommuKind::default(), IommuKind::None);
    }

    #[test]
    fn mcfg_rejects_a_truncated_table() {
        let mcfg = sdt_header(MCFG_SIGNATURE, 0).to_vec();
        assert_eq!(mcfg_first_allocation(&mcfg), None);
    }

    #[test]
    fn madt_stops_on_a_zero_length_structure() {
        let mut madt = sdt_header(MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]);
        // A malformed zero-length structure must not spin.
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 0, 0, 0]);
        assert_eq!(madt_enabled_cpu_count(&madt), 0);
    }
}
