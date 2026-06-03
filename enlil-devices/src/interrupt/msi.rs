//! MSI/MSI-X interrupt message emulation.

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

/// MSI capability structure for a PCI device.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct MsiCapability {
    /// Whether MSI is enabled.
    pub enabled: bool,
    /// Number of vectors allocated (power of 2, 1-32).
    pub num_vectors: u8,
    /// Whether 64-bit addressing is supported.
    pub is_64bit: bool,
    /// Whether per-vector masking is supported.
    pub per_vector_masking: bool,
    /// Base message.
    pub message: MsiMessage,
    /// Mask bits (one per vector).
    pub mask_bits: u32,
    /// Pending bits (one per vector, read-only).
    pub pending_bits: u32,
}

#[allow(dead_code)]
impl MsiCapability {
    /// Create a new MSI capability with default values.
    ///
    /// # Arguments
    ///
    /// * `is_64bit` - Whether 64-bit addressing is supported
    /// * `per_vector_masking` - Whether per-vector masking is supported
    #[must_use]
    pub const fn new(is_64bit: bool, per_vector_masking: bool) -> Self {
        Self {
            enabled: false,
            num_vectors: 1,
            is_64bit,
            per_vector_masking,
            message: MsiMessage::new(0xFEE0_0000, 0),
            mask_bits: 0,
            pending_bits: 0,
        }
    }

    /// Get the pending bits for this capability.
    #[must_use]
    pub const fn pending_bits(&self) -> u32 {
        self.pending_bits
    }

    /// Check if a specific vector is masked.
    ///
    /// # Arguments
    ///
    /// * `vector_idx` - Vector index to check
    #[must_use]
    pub const fn is_vector_masked(&self, vector_idx: u8) -> bool {
        if !self.per_vector_masking {
            return false;
        }
        (self.mask_bits & (1 << vector_idx)) != 0
    }

    /// Get the MSI message for a specific vector index.
    ///
    /// # Arguments
    ///
    /// * `vector_idx` - Vector index
    #[must_use]
    #[allow(clippy::cast_lossless)]
    pub const fn message_for_vector(&self, vector_idx: u8) -> MsiMessage {
        let mut msg = self.message;
        msg.data = (msg.data & !0xFF) | ((msg.data & 0xFF).wrapping_add(vector_idx as u32) & 0xFF);
        msg
    }
}

/// MSI-X capability structure.
#[derive(Debug, Clone)]
pub struct MsixCapability {
    /// Whether MSI-X is enabled.
    pub enabled: bool,
    /// Function mask — masks all vectors when set.
    pub function_mask: bool,
    /// Table size (number of entries, 1-2048).
    pub table_size: u16,
    /// Table entries.
    pub table: Vec<MsixTableEntry>,
    /// Pending bit array.
    pub pba: Vec<u64>,
}

impl MsixCapability {
    /// Create a new MSI-X capability.
    ///
    /// # Arguments
    ///
    /// * `table_size` - Number of table entries to allocate
    #[must_use]
    pub fn new(table_size: u16) -> Self {
        let mut table = Vec::with_capacity(table_size as usize);
        for _ in 0..table_size {
            table.push(MsixTableEntry::default());
        }
        let pba_size = (table_size as usize).div_ceil(64);
        Self {
            enabled: false,
            function_mask: false,
            table_size,
            table,
            pba: vec![0u64; pba_size],
        }
    }

    /// Get the message for a table entry, if not masked.
    ///
    /// # Arguments
    ///
    /// * `index` - Table entry index
    #[must_use]
    pub fn get_message(&self, index: u16) -> Option<MsiMessage> {
        if !self.enabled || self.function_mask {
            return None;
        }
        let entry = self.table.get(index as usize)?;
        if entry.masked {
            return None;
        }
        Some(MsiMessage::new(entry.address, entry.data))
    }

    /// Set a pending bit.
    ///
    /// # Arguments
    ///
    /// * `index` - Bit index to set
    pub fn set_pending(&mut self, index: u16) {
        let word = index as usize / 64;
        let bit = index as usize % 64;
        if word < self.pba.len() {
            self.pba[word] |= 1u64 << bit;
        }
    }

    /// Clear a pending bit.
    ///
    /// # Arguments
    ///
    /// * `index` - Bit index to clear
    pub fn clear_pending(&mut self, index: u16) {
        let word = index as usize / 64;
        let bit = index as usize % 64;
        if word < self.pba.len() {
            self.pba[word] &= !(1u64 << bit);
        }
    }
}

/// MSI-X table entry.
#[derive(Debug, Clone, Copy)]
pub struct MsixTableEntry {
    /// Message address (low 32 bits + high 32 bits).
    pub address: u64,
    /// Message data.
    pub data: u32,
    /// Vector control — bit 0 is mask bit.
    pub masked: bool,
}

impl Default for MsixTableEntry {
    fn default() -> Self {
        Self {
            address: 0,
            data: 0,
            masked: true, // All entries start masked per spec
        }
    }
}

impl MsixTableEntry {
    /// Read the vector control register.
    #[must_use]
    pub const fn vector_control(&self) -> u32 {
        self.masked as u32
    }

    /// Write the vector control register.
    ///
    /// # Arguments
    ///
    /// * `val` - Value to write (bit 0 is the mask bit)
    pub const fn set_vector_control(&mut self, val: u32) {
        self.masked = (val & 1) != 0;
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

    #[test]
    fn test_msi_capability_creation() {
        let cap = MsiCapability::new(true, true);
        assert!(!cap.enabled);
        assert_eq!(cap.num_vectors, 1);
        assert!(cap.is_64bit);
        assert!(cap.per_vector_masking);
    }

    #[test]
    fn test_msi_capability_vector_masking() {
        let mut cap = MsiCapability::new(false, true);

        // With per_vector_masking enabled, check initial state
        assert!(!cap.is_vector_masked(0));
        assert!(!cap.is_vector_masked(3));

        // Set mask for vectors 0, 2, 3
        cap.mask_bits = 0x0D; // bits 0, 2, 3
        assert!(cap.is_vector_masked(0));
        assert!(!cap.is_vector_masked(1));
        assert!(cap.is_vector_masked(2));
        assert!(cap.is_vector_masked(3));

        // With per_vector_masking disabled, always returns false
        cap.per_vector_masking = false;
        assert!(!cap.is_vector_masked(0));
        assert!(!cap.is_vector_masked(3));
    }

    #[test]
    fn test_msi_capability_message_for_vector() {
        let cap = MsiCapability::new(true, true);

        let base_vector = cap.message.vector();

        // Test getting messages for different vectors
        for i in 0..8 {
            let msg = cap.message_for_vector(i);
            assert_eq!(msg.vector(), base_vector.wrapping_add(i));
            assert_eq!(msg.address, cap.message.address);
        }

        // Test vector wrapping (8-bit)
        let msg = cap.message_for_vector(255);
        assert_eq!(msg.vector(), 255);

        // Vector 0 is a valid edge case.
        let msg = cap.message_for_vector(0);
        assert_eq!(msg.vector(), base_vector.wrapping_add(0));
    }

    #[test]
    fn test_msix_capability() {
        let mut cap = MsixCapability::new(4);
        assert_eq!(cap.table.len(), 4);
        assert!(cap.table[0].masked);

        // Configure entry 0
        cap.enabled = true;
        cap.table[0].address = 0xFEE0_1000;
        cap.table[0].data = 0x41;
        cap.table[0].masked = false;

        let msg = cap.get_message(0).unwrap();
        assert_eq!(msg.vector(), 0x41);
        assert_eq!(msg.destination_id(), 1);

        // Masked entry returns None
        assert!(cap.get_message(1).is_none());
    }

    #[test]
    fn test_msix_pending_bits() {
        let mut cap = MsixCapability::new(128);
        cap.set_pending(65);
        assert_eq!(cap.pba[1], 1u64 << 1);
        cap.clear_pending(65);
        assert_eq!(cap.pba[1], 0);
    }

    #[test]
    fn test_msix_table_entry_vector_control() {
        let mut entry = MsixTableEntry::default();
        assert_eq!(entry.vector_control(), 1); // masked by default

        entry.masked = false;
        assert_eq!(entry.vector_control(), 0);

        entry.set_vector_control(1);
        assert!(entry.masked);
    }
}
