//! WSMT (Windows SMM Security Mitigations Table) builder
//!
//! Microsoft-defined table (WSMT spec v1.0) that a UEFI firmware emits to
//! declare its SMM security mitigations.
//!
//! Windows reads it to decide whether it can safely enable features such as
//! Virtualization Based Security (VBS) without additional runtime SMM checks.
//!
//! Real consumer UEFI firmware (AMI, the vendor this crate impersonates) emits a
//! WSMT on virtually every modern Windows machine with all three mitigations
//! asserted. Its **absence** is therefore a fidelity gap a guest can notice: an
//! anti-detection probe that enumerates the ACPI table set sees the FADT/APIC/
//! MCFG a real board has but no WSMT — a shape no shipping AMI board produces.
//!
//! The table is tiny and fixed-format: the 36-byte ACPI SDT header followed by a
//! single 4-byte Protection Flags DWORD (total 40 bytes), mirroring the WAET
//! table's shape.

use super::tables::{AcpiSdtHeader, OemInfo};

/// Fixed Communication Buffers (WSMT §2.2, bit 0).
///
/// The firmware guarantees SMI handlers only ever dereference communication
/// buffers that lie entirely outside SMRAM, so a malicious OS cannot trick an
/// SMI into reading/writing SMRAM.
pub const WSMT_FIXED_COMM_BUFFERS: u32 = 1 << 0;
/// Communication Buffer Nested Pointer Protection (WSMT §2.2, bit 1).
///
/// The firmware validates every pointer *inside* a communication buffer the same
/// way, closing the nested-pointer confused-deputy hole.
pub const WSMT_COMM_BUFFER_NESTED_PTR_PROTECTION: u32 = 1 << 1;
/// System Resource Protection (WSMT §2.2, bit 2).
///
/// The firmware protects system resources (e.g. IO/MMIO/MSR/PCI config) from
/// being reprogrammed by a compromised OS via SMI.
pub const WSMT_SYSTEM_RESOURCE_PROTECTION: u32 = 1 << 2;

/// The full protection-flag set a hardened modern firmware reports — all three
/// mitigations asserted, which is what a shipping AMI board on the impersonated
/// hardware presents.
pub const WSMT_ALL_MITIGATIONS: u32 = WSMT_FIXED_COMM_BUFFERS
    | WSMT_COMM_BUFFER_NESTED_PTR_PROTECTION
    | WSMT_SYSTEM_RESOURCE_PROTECTION;

/// WSMT table builder.
pub struct WsmtBuilder {
    oem: OemInfo,
    protection_flags: u32,
}

impl WsmtBuilder {
    /// Create a WSMT builder reporting all three SMM security mitigations, as a
    /// hardened modern UEFI firmware does.
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            protection_flags: WSMT_ALL_MITIGATIONS,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    /// Override the Protection Flags DWORD (mainly for tests / non-default
    /// firmware profiles).
    #[must_use]
    pub const fn protection_flags(mut self, flags: u32) -> Self {
        self.protection_flags = flags;
        self
    }

    /// Build the WSMT table as a byte vector.
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let total_length: u32 = 40; // 36-byte header + 4-byte Protection Flags
        let mut buf = Vec::with_capacity(total_length as usize);

        let header = AcpiSdtHeader::new(*b"WSMT", total_length, 1, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Offset 36: Protection Flags (4 bytes, little-endian).
        buf.extend_from_slice(&self.protection_flags.to_le_bytes());

        // Fix up checksum (offset 9): all bytes must sum to 0 mod 256.
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for WsmtBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsmt_builds_with_signature_and_length() {
        let wsmt = WsmtBuilder::new().build();
        assert_eq!(wsmt.len(), 40);
        assert_eq!(&wsmt[0..4], b"WSMT");
        // Header length field (offset 4) must equal the real byte length.
        assert_eq!(u32::from_le_bytes(wsmt[4..8].try_into().unwrap()), 40);
        // Revision (offset 8) is 1 per the WSMT spec.
        assert_eq!(wsmt[8], 1);
    }

    #[test]
    fn wsmt_checksum_is_zero() {
        let wsmt = WsmtBuilder::new().build();
        let sum: u8 = wsmt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0, "ACPI checksum must sum to 0 mod 256");
    }

    #[test]
    fn wsmt_defaults_to_all_three_mitigations() {
        let wsmt = WsmtBuilder::new().build();
        let flags = u32::from_le_bytes(wsmt[36..40].try_into().unwrap());
        assert_eq!(flags, 0x7);
        assert_eq!(
            flags,
            WSMT_FIXED_COMM_BUFFERS
                | WSMT_COMM_BUFFER_NESTED_PTR_PROTECTION
                | WSMT_SYSTEM_RESOURCE_PROTECTION
        );
    }

    #[test]
    fn wsmt_custom_flags_round_trip() {
        let wsmt = WsmtBuilder::new()
            .protection_flags(WSMT_FIXED_COMM_BUFFERS)
            .build();
        let flags = u32::from_le_bytes(wsmt[36..40].try_into().unwrap());
        assert_eq!(flags, WSMT_FIXED_COMM_BUFFERS);
    }

    #[test]
    fn wsmt_carries_the_ami_oem_id() {
        let wsmt = WsmtBuilder::new().build();
        // Offset 10..16 is the 6-byte OEM ID; the default profile is AMI/ALASKA.
        assert_eq!(&wsmt[10..16], b"ALASKA");
    }
}
