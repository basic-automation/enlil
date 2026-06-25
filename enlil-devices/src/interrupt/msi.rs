//! MSI/MSI-X interrupt message emulation.
//!
//! The message itself ([`MsiMessage`]) — the address+data pair a PCI function
//! hands to the interrupt path — lives here. The *capability* state (the config
//! registers and the BAR-resident table/PBA) is modelled where it belongs, on
//! the device: [`PciConfigSpace::add_msi_capability`](crate::pcie::PciConfigSpace::add_msi_capability)
//! / [`add_msix_capability`](crate::pcie::PciConfigSpace::add_msix_capability)
//! for the config-space side and [`MsixTable`](crate::pcie::MsixTable) for the
//! MMIO-faithful table + Pending Bit Array.

use super::DeliveryMode;

/// MSI message — address + data pair that encodes an interrupt.
#[derive(Debug, Clone, Copy)]
pub struct MsiMessage {
    /// MSI address register value.
    pub address: u64,
    /// MSI data register value.
    pub data: u32,
    /// Delivery mode extracted from data.
    pub delivery_mode: DeliveryMode,
}

impl MsiMessage {
    /// Create a new MSI message.
    ///
    /// # Arguments
    ///
    /// * `address` - MSI address register value
    /// * `data` - MSI data register value
    #[must_use]
    pub const fn new(address: u64, data: u32) -> Self {
        let dm = DeliveryMode::from_bits(((data >> 8) & 0x7) as u8);
        Self {
            address,
            data,
            delivery_mode: dm,
        }
    }

    /// Get the interrupt vector from the data register.
    #[must_use]
    pub const fn vector(&self) -> u8 {
        (self.data & 0xFF) as u8
    }

    /// Get the destination APIC ID from the address register.
    #[must_use]
    pub const fn destination_id(&self) -> u8 {
        ((self.address >> 12) & 0xFF) as u8
    }

    /// Whether destination mode is logical (vs physical).
    #[must_use]
    pub const fn destination_mode_logical(&self) -> bool {
        (self.address & (1 << 2)) != 0
    }

    /// Whether this is a level-triggered MSI.
    #[must_use]
    pub const fn is_level(&self) -> bool {
        (self.data & (1 << 15)) != 0
    }

    /// Whether this is an assert (vs deassert) for level-triggered.
    #[must_use]
    pub const fn is_assert(&self) -> bool {
        (self.data & (1 << 14)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_msi_message_fields() {
        // Address: dest APIC ID 2, physical mode
        // Data: vector 0x30, fixed delivery
        let msg = MsiMessage::new(0xFEE0_2000, 0x30);
        assert_eq!(msg.vector(), 0x30);
        assert_eq!(msg.destination_id(), 2);
        assert!(!msg.destination_mode_logical());
        assert_eq!(msg.delivery_mode, DeliveryMode::Fixed);
    }

    #[test]
    fn test_msi_message_logical_dest() {
        let msg = MsiMessage::new(0xFEE0_2004, 0x40); // bit 2 set = logical
        assert!(msg.destination_mode_logical());
    }
}
