//! HPET (High Precision Event Timer) ACPI table builder
//!
//! Windows requires HPET for high-resolution timing. The HPET table tells
//! the OS where the HPET registers are memory-mapped.

use super::tables::{AcpiSdtHeader, OemInfo};

/// Standard HPET MMIO base address
pub const HPET_BASE_ADDRESS: u64 = 0xFED0_0000;

/// HPET table builder
pub struct HpetBuilder {
    oem: OemInfo,
    hardware_rev_id: u8,
    comparator_count: u8,
    counter_size: bool,
    legacy_replacement: bool,
    pci_vendor_id: u16,
    base_address: u64,
    hpet_number: u8,
    min_tick: u16,
    page_protection: u8,
}

impl HpetBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            hardware_rev_id: 1,
            comparator_count: 2, // 3 comparators (0-2)
            counter_size: true,  // 64-bit counter
            legacy_replacement: true,
            pci_vendor_id: 0x8086, // Intel
            base_address: HPET_BASE_ADDRESS,
            hpet_number: 0,
            min_tick: 0x0080,
            page_protection: 0,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub const fn base_address(mut self, addr: u64) -> Self {
        self.base_address = addr;
        self
    }

    #[must_use]
    pub const fn pci_vendor_id(mut self, vid: u16) -> Self {
        self.pci_vendor_id = vid;
        self
    }

    /// Build the HPET table
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let total_length: u32 = 56; // Fixed size
        let mut buf = Vec::with_capacity(total_length as usize);

        let header = AcpiSdtHeader::new(*b"HPET", total_length, 1, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Offset 36: Event Timer Block ID (4 bytes)
        let mut event_timer_block_id: u32 = u32::from(self.hardware_rev_id);
        event_timer_block_id |= u32::from(self.comparator_count) << 8;
        if self.counter_size {
            event_timer_block_id |= 1 << 13;
        }
        if self.legacy_replacement {
            event_timer_block_id |= 1 << 15;
        }
        event_timer_block_id |= u32::from(self.pci_vendor_id) << 16;
        buf.extend_from_slice(&event_timer_block_id.to_le_bytes());

        // Offset 40: Base Address (12-byte GAS)
        buf.push(0); // address space: memory
        buf.push(64); // bit width
        buf.push(0); // bit offset
        buf.push(0); // access size (undefined)
        buf.extend_from_slice(&self.base_address.to_le_bytes());

        // Offset 52: HPET Number
        buf.push(self.hpet_number);

        // Offset 53: Main Counter Minimum Clock Tick
        buf.extend_from_slice(&self.min_tick.to_le_bytes());

        // Offset 55: Page Protection
        buf.push(self.page_protection);

        // Checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for HpetBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hpet_builds() {
        let hpet = HpetBuilder::new().build();
        assert_eq!(hpet.len(), 56);
        assert_eq!(&hpet[0..4], b"HPET");
    }

    #[test]
    fn hpet_checksum() {
        let hpet = HpetBuilder::new().build();
        let sum: u8 = hpet.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn hpet_base_address() {
        let hpet = HpetBuilder::new().base_address(0xFED0_0000).build();
        let addr = u64::from_le_bytes(hpet[44..52].try_into().unwrap());
        assert_eq!(addr, 0xFED0_0000);
    }

    #[test]
    fn hpet_custom_vendor() {
        let hpet = HpetBuilder::new().pci_vendor_id(0x1022).build(); // AMD
        let event_id = u32::from_le_bytes(hpet[36..40].try_into().unwrap());
        assert_eq!((event_id >> 16) & 0xFFFF, 0x1022);
    }
}
