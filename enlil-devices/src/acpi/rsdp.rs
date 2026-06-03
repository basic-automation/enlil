//! ACPI 2.0+ RSDP (Root System Description Pointer) builder.
//!
//! The RSDP is the entry point to the ACPI table hierarchy.
//! Located at a well-known physical address, it points to the XSDT.

use crate::truncate::u32_of;
const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
const RSDP_REVISION_2: u8 = 2;
const RSDP_V1_LEN: usize = 20;
const RSDP_V2_LEN: usize = 36;

/// Default OEM ID that looks like a real motherboard vendor.
const DEFAULT_OEM_ID: [u8; 6] = *b"ALASKA";

pub struct RsdpBuilder {
    oem_id: [u8; 6],
    xsdt_address: u64,
}

impl Default for RsdpBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RsdpBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            oem_id: DEFAULT_OEM_ID,
            xsdt_address: 0,
        }
    }

    #[must_use]
    pub const fn oem_id(mut self, id: [u8; 6]) -> Self {
        self.oem_id = id;
        self
    }

    #[must_use]
    pub const fn xsdt_address(mut self, addr: u64) -> Self {
        self.xsdt_address = addr;
        self
    }

    /// Build the raw RSDP bytes (36 bytes for ACPI 2.0+).
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let mut buf = vec![0u8; RSDP_V2_LEN];

        // Signature (offset 0, 8 bytes)
        buf[..8].copy_from_slice(RSDP_SIGNATURE);

        // Checksum placeholder (offset 8, 1 byte) — filled below
        // OEM ID (offset 9, 6 bytes)
        buf[9..15].copy_from_slice(&self.oem_id);

        // Revision (offset 15, 1 byte)
        buf[15] = RSDP_REVISION_2;

        // RsdtAddress (offset 16, 4 bytes) — set to 0 for ACPI 2.0+ (use XSDT)
        buf[16..20].copy_from_slice(&0u32.to_le_bytes());

        // Length (offset 20, 4 bytes)
        buf[20..24].copy_from_slice(&(u32_of(RSDP_V2_LEN)).to_le_bytes());

        // XsdtAddress (offset 24, 8 bytes)
        buf[24..32].copy_from_slice(&self.xsdt_address.to_le_bytes());

        // Extended checksum placeholder (offset 32, 1 byte) — filled below
        // Reserved (offset 33, 3 bytes) — already zero

        // Calculate ACPI 1.0 checksum (first 20 bytes)
        let v1_sum: u8 = buf[..RSDP_V1_LEN]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[8] = 0u8.wrapping_sub(v1_sum);

        // Calculate extended checksum (all 36 bytes)
        let v2_sum: u8 = buf[..RSDP_V2_LEN]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[32] = 0u8.wrapping_sub(v2_sum);

        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rsdp_size() {
        let rsdp = RsdpBuilder::new().build();
        assert_eq!(rsdp.len(), 36);
    }

    #[test]
    fn rsdp_signature() {
        let rsdp = RsdpBuilder::new().build();
        assert_eq!(&rsdp[..8], b"RSD PTR ");
    }

    #[test]
    fn rsdp_v1_checksum() {
        let rsdp = RsdpBuilder::new().xsdt_address(0xDEAD_0000).build();
        let sum: u8 = rsdp[..20].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(sum, 0, "ACPI 1.0 checksum must be zero");
    }

    #[test]
    fn rsdp_v2_checksum() {
        let rsdp = RsdpBuilder::new().xsdt_address(0xDEAD_0000).build();
        let sum: u8 = rsdp.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(sum, 0, "ACPI 2.0 extended checksum must be zero");
    }

    #[test]
    fn rsdp_revision() {
        let rsdp = RsdpBuilder::new().build();
        assert_eq!(rsdp[15], 2);
    }

    #[test]
    fn rsdp_xsdt_address() {
        let addr: u64 = 0x1234_5678_9ABC_DEF0;
        let rsdp = RsdpBuilder::new().xsdt_address(addr).build();
        let stored = u64::from_le_bytes(rsdp[24..32].try_into().unwrap());
        assert_eq!(stored, addr);
    }

    #[test]
    fn rsdp_custom_oem() {
        let rsdp = RsdpBuilder::new().oem_id(*b"LENOVO").build();
        assert_eq!(&rsdp[9..15], b"LENOVO");
        // Checksums must still be valid
        let sum: u8 = rsdp.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(sum, 0);
    }
}
