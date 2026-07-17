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

/// What the kernel discovered from the firmware ACPI tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AcpiSummary {
    /// Number of tables the XSDT references.
    pub tables: usize,
    /// Enabled processors counted in the MADT (0 if no MADT was found).
    pub enabled_cpus: u32,
}

#[cfg(target_os = "uefi")]
pub use hw::discover;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        AcpiSummary, MADT_SIGNATURE, SDT_HEADER_LEN, madt_enabled_cpu_count, rsdp_xsdt_address,
        sdt_length, sdt_signature, xsdt_entry, xsdt_entry_count,
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
        };

        // Scan the referenced tables for the MADT and count its enabled CPUs.
        let mut i = 0;
        while let Some(table_phys) = xsdt_entry(xsdt, i) {
            // SAFETY: each XSDT entry points at a mapped SDT; read its header.
            let hdr = unsafe { phys_slice(table_phys, SDT_HEADER_LEN) };
            if sdt_signature(hdr) == Some(*MADT_SIGNATURE)
                && let Some(len) = sdt_length(hdr)
            {
                // SAFETY: the MADT spans `len` mapped bytes from table_phys.
                let madt = unsafe { phys_slice(table_phys, len as usize) };
                summary.enabled_cpus = madt_enabled_cpu_count(madt);
            }
            i += 1;
        }
        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal SDT header with `sig` and `length`.
    fn sdt_header(sig: &[u8; 4], length: u32) -> [u8; SDT_HEADER_LEN] {
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
        let length = (SDT_HEADER_LEN + 16) as u32;
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
    fn madt_stops_on_a_zero_length_structure() {
        let mut madt = sdt_header(MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]);
        // A malformed zero-length structure must not spin.
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 0, 0, 0]);
        assert_eq!(madt_enabled_cpu_count(&madt), 0);
    }
}
