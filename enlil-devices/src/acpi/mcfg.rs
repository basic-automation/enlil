//! MCFG (PCI Express Memory-Mapped Configuration) table builder
//!
//! Required for PCI Express configuration space access. Windows uses this
//! to discover the ECAM (Enhanced Configuration Access Mechanism) base address.

use super::tables::{AcpiSdtHeader, OemInfo};
use crate::truncate::u32_of;

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
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
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
    pub fn build(&self) -> Vec<u8> {
        // MCFG: 36-byte header + 8-byte reserved + 16 bytes per allocation
        let total_length = 36 + 8 + self.allocations.len() * 16;
        let mut buf = Vec::with_capacity(total_length);

        let header = AcpiSdtHeader::new(*b"MCFG", u32_of(total_length), 1, &self.oem);
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

/// Parse the ECAM allocations out of an MCFG table.
///
/// Each [`McfgAllocation`] gives a `PCIe` Enhanced Configuration Access Mechanism
/// window — a base address and the bus range it covers — which the config-space
/// walk (Phase 6.3, "PCI devices via MCFG ECAM") uses to reach device config
/// space. The table is a 36-byte SDT header + 8 reserved bytes + one 16-byte
/// allocation entry each; a trailing partial entry is ignored.
#[must_use]
pub fn parse_mcfg_allocations(mcfg: &[u8]) -> Vec<McfgAllocation> {
    /// First allocation entry: past the 36-byte header and 8 reserved bytes.
    const ALLOCATIONS: usize = 44;
    let mut out = Vec::new();
    let mut off = ALLOCATIONS;
    while off + 16 <= mcfg.len() {
        let base_address = u64::from_le_bytes([
            mcfg[off],
            mcfg[off + 1],
            mcfg[off + 2],
            mcfg[off + 3],
            mcfg[off + 4],
            mcfg[off + 5],
            mcfg[off + 6],
            mcfg[off + 7],
        ]);
        out.push(McfgAllocation {
            base_address,
            segment_group: u16::from_le_bytes([mcfg[off + 8], mcfg[off + 9]]),
            start_bus: mcfg[off + 10],
            end_bus: mcfg[off + 11],
        });
        off += 16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mcfg_allocations_round_trips_the_builder() {
        let mcfg = McfgBuilder::new()
            .add_allocation(McfgAllocation {
                base_address: 0xE000_0000,
                segment_group: 0,
                start_bus: 0,
                end_bus: 255,
            })
            .add_allocation(McfgAllocation {
                base_address: 0xF000_0000,
                segment_group: 1,
                start_bus: 0,
                end_bus: 63,
            })
            .build();
        let allocs = parse_mcfg_allocations(&mcfg);
        assert_eq!(allocs.len(), 2);
        assert_eq!(allocs[0].base_address, 0xE000_0000);
        assert_eq!(allocs[0].end_bus, 255);
        assert_eq!(allocs[1].base_address, 0xF000_0000);
        assert_eq!(allocs[1].segment_group, 1);
        assert_eq!(allocs[1].end_bus, 63);
        // A header with no allocations parses to an empty list.
        assert!(parse_mcfg_allocations(&McfgBuilder::new().build()).is_empty());
    }

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

    /// The ECAM base is encoded by two independent surfaces a guest can
    /// cross-reference: the MCFG table (here) and the Q35 MCH's PCIEXBAR
    /// register (config 0x60). Built from the same base, they must agree —
    /// otherwise the firmware-described config aperture and the hardware
    /// decode disagree, an impossible machine.
    #[test]
    fn mcfg_base_matches_the_mch_pciexbar() {
        use crate::pcie::{PCIEXBAR_ENABLE, PCIEXBAR_OFFSET, PcieRootComplex};

        let ecam_base = 0xB000_0000u64;
        let mcfg = McfgBuilder::standard(ecam_base).build();
        let mcfg_base = u64::from_le_bytes(mcfg[44..52].try_into().unwrap());

        let mch = PcieRootComplex::create_q35_host_bridge(ecam_base);
        let pciexbar = u64::from(mch.read_u32(PCIEXBAR_OFFSET))
            | (u64::from(mch.read_u32(PCIEXBAR_OFFSET + 4)) << 32);
        assert_eq!(
            pciexbar & PCIEXBAR_ENABLE,
            PCIEXBAR_ENABLE,
            "PCIEXBAR enabled"
        );
        assert_eq!(pciexbar & !0xFu64, mcfg_base, "PCIEXBAR base == MCFG base");
    }
}
