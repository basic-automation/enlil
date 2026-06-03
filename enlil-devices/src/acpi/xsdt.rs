//! XSDT (Extended System Description Table) builder
//!
//! The XSDT contains 64-bit pointers to all other ACPI tables.
//! RSDP → XSDT → [FADT, MADT, MCFG, HPET, ...]

use super::tables::{AcpiSdtHeader, OemInfo};
use crate::truncate::u32_of;

/// XSDT builder — collects table addresses and generates the binary table
pub struct XsdtBuilder {
    oem: OemInfo,
    table_addresses: Vec<u64>,
}

impl XsdtBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            table_addresses: Vec::new(),
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub fn add_table(mut self, address: u64) -> Self {
        self.table_addresses.push(address);
        self
    }

    #[must_use]
    pub fn add_tables(mut self, addresses: &[u64]) -> Self {
        self.table_addresses.extend_from_slice(addresses);
        self
    }

    /// Build the XSDT as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let total_length = 36 + self.table_addresses.len() * 8;
        let mut buf = Vec::with_capacity(total_length);

        let header = AcpiSdtHeader::new(*b"XSDT", u32_of(total_length), 1, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        for &addr in &self.table_addresses {
            buf.extend_from_slice(&addr.to_le_bytes());
        }

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for XsdtBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xsdt_empty() {
        let xsdt = XsdtBuilder::new().build();
        assert_eq!(xsdt.len(), 36);
        assert_eq!(&xsdt[0..4], b"XSDT");
        let sum: u8 = xsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn xsdt_with_tables() {
        let xsdt = XsdtBuilder::new()
            .add_table(0x1000)
            .add_table(0x2000)
            .add_table(0x3000)
            .build();
        assert_eq!(xsdt.len(), 36 + 24);
        // Check first address
        let addr = u64::from_le_bytes(xsdt[36..44].try_into().unwrap());
        assert_eq!(addr, 0x1000);
        let sum: u8 = xsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn xsdt_table_count() {
        let xsdt = XsdtBuilder::new()
            .add_tables(&[0x1000, 0x2000, 0x3000, 0x4000])
            .build();
        let length = u32::from_le_bytes(xsdt[4..8].try_into().unwrap());
        assert_eq!(length, 36 + 32); // header + 4 * 8
    }
}
