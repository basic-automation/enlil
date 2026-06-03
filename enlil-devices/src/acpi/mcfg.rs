//! MCFG (PCI Express Memory-Mapped Configuration) table builder
//!
//! Required for PCI Express configuration space access. Windows uses this
//! to discover the ECAM (Enhanced Configuration Access Mechanism) base address.

use super::tables::{AcpiSdtHeader, OemInfo};

/// A single MCFG allocation entry
#[derive(Debug, Clone)]
pub struct McfgAllocation {
    pub base_address: u64,
    pub segment_group: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

impl McfgAllocation {
    /// Standard single-segment PCI Express config space
    #[must_use]
    pub const fn standard(base: u64) -> Self {
        Self {
            base_address: base,
            segment_group: 0,
            start_bus: 0,
            end_bus: 255,
        }
    }

    fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&self.base_address.to_le_bytes());
        buf[8..10].copy_from_slice(&self.segment_group.to_le_bytes());
        buf[10] = self.start_bus;
        buf[11] = self.end_bus;
        // bytes 12-15: reserved
        buf
    }
}

/// MCFG table builder
pub struct McfgBuilder {
    oem: OemInfo,
    allocations: Vec<McfgAllocation>,
}

impl McfgBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            allocations: Vec::new(),
        }
    }

    #[must_use]
    pub fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub fn add_allocation(mut self, alloc: McfgAllocation) -> Self {
        self.allocations.push(alloc);
        self
    }

    /// Standard MCFG with a single PCI segment at the given ECAM base
    #[must_use]
    pub fn standard(ecam_base: u64) -> Self {
        Self::new().add_allocation(McfgAllocation::standard(ecam_base))
    }

    /// Build the MCFG table as bytes
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn build(&self) -> Vec<u8> {
        // MCFG: 36-byte header + 8-byte reserved + 16 bytes per allocation
        let total_length = 36 + 8 + self.allocations.len() * 16;
        let mut buf = Vec::with_capacity(total_length);

        let header = AcpiSdtHeader::new(*b"MCFG", total_length as u32, 1, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // 8 bytes reserved
        buf.extend_from_slice(&[0u8; 8]);

        for alloc in &self.allocations {
            buf.extend_from_slice(&alloc.to_bytes());
        }

        // Checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for McfgBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcfg_standard() {
        let mcfg = McfgBuilder::standard(0xB000_0000).build();
        assert_eq!(&mcfg[0..4], b"MCFG");
        assert_eq!(mcfg.len(), 36 + 8 + 16);
        let sum: u8 = mcfg.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn mcfg_ecam_base() {
        let mcfg = McfgBuilder::standard(0xB000_0000).build();
        // ECAM base at offset 36 + 8 = 44
        let base = u64::from_le_bytes(mcfg[44..52].try_into().unwrap());
        assert_eq!(base, 0xB000_0000);
    }

    #[test]
    fn mcfg_bus_range() {
        let mcfg = McfgBuilder::standard(0xB000_0000).build();
        assert_eq!(mcfg[54], 0); // start bus
        assert_eq!(mcfg[55], 255); // end bus
    }
}
