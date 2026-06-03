//! PCI Express root complex and configuration space emulation
//!
//! Provides a virtual PCIe root complex for guest VMs. Windows expects
//! a PCI Express bus with ECAM (Enhanced Configuration Access Mechanism)
//! for device enumeration.

/// PCI configuration space size per function
pub const PCI_CONFIG_SPACE_SIZE: usize = 256;
/// PCIe extended configuration space size per function
pub const PCIE_CONFIG_SPACE_SIZE: usize = 4096;
/// ECAM size per bus (256 devices * 8 functions * 4096 bytes)
pub const ECAM_BUS_SIZE: usize = 256 * 8 * 4096;

/// PCI configuration space header offsets
pub mod cfg {
    pub const VENDOR_ID: u16 = 0x00;
    pub const DEVICE_ID: u16 = 0x02;
    pub const COMMAND: u16 = 0x04;
    pub const STATUS: u16 = 0x06;
    pub const REVISION_ID: u16 = 0x08;
    pub const PROG_IF: u16 = 0x09;
    pub const SUBCLASS: u16 = 0x0A;
    pub const CLASS_CODE: u16 = 0x0B;
    pub const CACHE_LINE_SIZE: u16 = 0x0C;
    pub const LATENCY_TIMER: u16 = 0x0D;
    pub const HEADER_TYPE: u16 = 0x0E;
    pub const BIST: u16 = 0x0F;
    pub const BAR0: u16 = 0x10;
    pub const BAR1: u16 = 0x14;
    pub const BAR2: u16 = 0x18;
    pub const BAR3: u16 = 0x1C;
    pub const BAR4: u16 = 0x20;
    pub const BAR5: u16 = 0x24;
    pub const SUBSYSTEM_VENDOR_ID: u16 = 0x2C;
    pub const SUBSYSTEM_ID: u16 = 0x2E;
    pub const EXPANSION_ROM: u16 = 0x30;
    pub const CAPABILITY_PTR: u16 = 0x34;
    pub const INTERRUPT_LINE: u16 = 0x3C;
    pub const INTERRUPT_PIN: u16 = 0x3D;

    // Type 1 header (PCI bridge) specific
    pub const PRIMARY_BUS: u16 = 0x18;
    pub const SECONDARY_BUS: u16 = 0x19;
    pub const SUBORDINATE_BUS: u16 = 0x1A;
}

/// Well-known PCI vendor IDs
pub mod vendors {
    pub const INTEL: u16 = 0x8086;
    pub const AMD: u16 = 0x1022;
    pub const NVIDIA: u16 = 0x10DE;
    pub const REALTEK: u16 = 0x10EC;
}

/// PCI device class codes
pub mod class {
    pub const BRIDGE_HOST: u8 = 0x06;
    pub const BRIDGE_PCI: u8 = 0x06;
    pub const MULTIMEDIA_AUDIO: u8 = 0x04;
    pub const SERIAL_BUS_USB: u8 = 0x0C;
    pub const NETWORK_ETHERNET: u8 = 0x02;
    pub const DISPLAY_VGA: u8 = 0x03;
    pub const STORAGE_NVME: u8 = 0x01;
}

/// A PCI Bus:Device.Function address
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PciBdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl PciBdf {
    #[must_use]
    pub const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self {
            bus,
            device,
            function,
        }
    }

    /// Convert BDF to ECAM offset
    #[must_use]
    pub const fn ecam_offset(&self) -> usize {
        ((self.bus as usize) << 20)
            | ((self.device as usize) << 15)
            | ((self.function as usize) << 12)
    }
}

impl core::fmt::Display for PciBdf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

/// A virtual PCI device's configuration space
#[derive(Debug, Clone)]
pub struct PciConfigSpace {
    /// Raw configuration space bytes
    data: Vec<u8>,
    /// BARs writable mask (for size detection)
    bar_masks: [u32; 6],
    /// BDF address
    pub bdf: PciBdf,
}

impl PciConfigSpace {
    /// Create a new config space initialized to all-ones (no device)
    #[must_use]
    pub fn empty(bdf: PciBdf) -> Self {
        let mut data = vec![0xFF; PCIE_CONFIG_SPACE_SIZE];
        // Vendor/Device ID = 0xFFFF means no device
        data[0] = 0xFF;
        data[1] = 0xFF;
        Self {
            data,
            bar_masks: [0; 6],
            bdf,
        }
    }

    /// Create a config space for a real device
    #[must_use]
    pub fn new(bdf: PciBdf, vendor_id: u16, device_id: u16) -> Self {
        let mut data = vec![0u8; PCIE_CONFIG_SPACE_SIZE];
        let mut cs = Self {
            data,
            bar_masks: [0; 6],
            bdf,
        };
        cs.write_u16(cfg::VENDOR_ID, vendor_id);
        cs.write_u16(cfg::DEVICE_ID, device_id);
        // Status: capabilities list present
        cs.write_u16(cfg::STATUS, 0x0010);
        cs
    }

    /// Read a byte from config space
    #[must_use]
    pub fn read_u8(&self, offset: u16) -> u8 {
        self.data.get(offset as usize).copied().unwrap_or(0xFF)
    }

    /// Read a word from config space
    #[must_use]
    pub fn read_u16(&self, offset: u16) -> u16 {
        let o = offset as usize;
        if o + 1 < self.data.len() {
            u16::from_le_bytes([self.data[o], self.data[o + 1]])
        } else {
            0xFFFF
        }
    }

    /// Read a dword from config space
    #[must_use]
    pub fn read_u32(&self, offset: u16) -> u32 {
        let o = offset as usize;
        if o + 3 < self.data.len() {
            u32::from_le_bytes([
                self.data[o],
                self.data[o + 1],
                self.data[o + 2],
                self.data[o + 3],
            ])
        } else {
            0xFFFF_FFFF
        }
    }

    /// Write a byte to config space
    pub fn write_u8(&mut self, offset: u16, value: u8) {
        if (offset as usize) < self.data.len() {
            self.data[offset as usize] = value;
        }
    }

    /// Write a word to config space
    pub fn write_u16(&mut self, offset: u16, value: u16) {
        let bytes = value.to_le_bytes();
        let o = offset as usize;
        if o + 1 < self.data.len() {
            self.data[o] = bytes[0];
            self.data[o + 1] = bytes[1];
        }
    }

    /// Write a dword to config space
    pub fn write_u32(&mut self, offset: u16, value: u32) {
        let bytes = value.to_le_bytes();
        let o = offset as usize;
        if o + 3 < self.data.len() {
            self.data[o..o + 4].copy_from_slice(&bytes);
        }
    }

    /// Set class code, subclass, prog IF, and revision
    pub fn set_class(&mut self, class: u8, subclass: u8, prog_if: u8, revision: u8) {
        self.write_u8(cfg::REVISION_ID, revision);
        self.write_u8(cfg::PROG_IF, prog_if);
        self.write_u8(cfg::SUBCLASS, subclass);
        self.write_u8(cfg::CLASS_CODE, class);
    }

    /// Set a BAR value and its writable mask (for size detection)
    pub fn set_bar(&mut self, bar_index: usize, value: u32, mask: u32) {
        if bar_index < 6 {
            let offset = cfg::BAR0 + (bar_index as u16) * 4;
            self.write_u32(offset, value);
            self.bar_masks[bar_index] = mask;
        }
    }

    /// Set header type
    pub fn set_header_type(&mut self, header_type: u8) {
        self.write_u8(cfg::HEADER_TYPE, header_type);
    }

    /// Set subsystem vendor/device ID
    pub fn set_subsystem(&mut self, vendor: u16, device: u16) {
        self.write_u16(cfg::SUBSYSTEM_VENDOR_ID, vendor);
        self.write_u16(cfg::SUBSYSTEM_ID, device);
    }

    /// Set interrupt line and pin
    pub fn set_interrupt(&mut self, line: u8, pin: u8) {
        self.write_u8(cfg::INTERRUPT_LINE, line);
        self.write_u8(cfg::INTERRUPT_PIN, pin);
    }

    /// Handle a guest config space write (respecting BAR masks)
    pub fn guest_write_u32(&mut self, offset: u16, value: u32) {
        // BAR writes need special handling for size detection
        if offset >= cfg::BAR0 && offset <= cfg::BAR5 {
            let bar_idx = ((offset - cfg::BAR0) / 4) as usize;
            if bar_idx < 6 {
                let mask = self.bar_masks[bar_idx];
                let current = self.read_u32(offset);
                // Guest writes all-ones to detect size, then writes the address
                let new_val = (value & mask) | (current & !mask);
                self.write_u32(offset, new_val);
                return;
            }
        }
        self.write_u32(offset, value);
    }

    /// Get vendor ID
    #[must_use]
    pub fn vendor_id(&self) -> u16 {
        self.read_u16(cfg::VENDOR_ID)
    }

    /// Get device ID
    #[must_use]
    pub fn device_id(&self) -> u16 {
        self.read_u16(cfg::DEVICE_ID)
    }

    /// Check if this is a valid (present) device
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.vendor_id() != 0xFFFF
    }
}

/// Virtual PCI Express Root Complex
#[derive(Debug)]
pub struct PcieRootComplex {
    /// Devices on the virtual bus
    devices: Vec<PciConfigSpace>,
    /// ECAM base address (guest physical)
    pub ecam_base: u64,
}

impl PcieRootComplex {
    /// Create a new root complex with the given ECAM base address
    #[must_use]
    pub fn new(ecam_base: u64) -> Self {
        Self {
            devices: Vec::new(),
            ecam_base,
        }
    }

    /// Add a device to the root complex
    pub fn add_device(&mut self, config: PciConfigSpace) {
        self.devices.push(config);
    }

    /// Find a device by BDF
    #[must_use]
    pub fn find_device(&self, bdf: &PciBdf) -> Option<&PciConfigSpace> {
        self.devices.iter().find(|d| d.bdf == *bdf)
    }

    /// Find a device by BDF (mutable)
    pub fn find_device_mut(&mut self, bdf: &PciBdf) -> Option<&mut PciConfigSpace> {
        self.devices.iter_mut().find(|d| d.bdf == *bdf)
    }

    /// Handle ECAM MMIO read
    #[must_use]
    pub fn ecam_read(&self, offset: u64, size: u8) -> u32 {
        let bdf = PciBdf {
            bus: ((offset >> 20) & 0xFF) as u8,
            device: ((offset >> 15) & 0x1F) as u8,
            function: ((offset >> 12) & 0x7) as u8,
        };
        let reg_offset = (offset & 0xFFF) as u16;

        if let Some(dev) = self.find_device(&bdf) {
            match size {
                1 => u32::from(dev.read_u8(reg_offset)),
                2 => u32::from(dev.read_u16(reg_offset)),
                4 => dev.read_u32(reg_offset),
                _ => 0xFFFF_FFFF,
            }
        } else {
            // No device — return all ones
            0xFFFF_FFFF
        }
    }

    /// Handle ECAM MMIO write
    pub fn ecam_write(&mut self, offset: u64, value: u32, size: u8) {
        let bdf = PciBdf {
            bus: ((offset >> 20) & 0xFF) as u8,
            device: ((offset >> 15) & 0x1F) as u8,
            function: ((offset >> 12) & 0x7) as u8,
        };
        let reg_offset = (offset & 0xFFF) as u16;

        if let Some(dev) = self.find_device_mut(&bdf) {
            match size {
                1 => dev.write_u8(reg_offset, value as u8),
                2 => dev.write_u16(reg_offset, value as u16),
                4 => dev.guest_write_u32(reg_offset, value),
                _ => {}
            }
        }
    }

    /// Create a standard host bridge device (bus 0, device 0, function 0)
    #[must_use]
    pub fn create_host_bridge(vendor_id: u16, device_id: u16) -> PciConfigSpace {
        let mut dev = PciConfigSpace::new(PciBdf::new(0, 0, 0), vendor_id, device_id);
        dev.set_class(0x06, 0x00, 0x00, 0x00); // Host bridge
        dev.set_header_type(0x00);
        dev
    }

    /// Create a standard ISA/LPC bridge device
    #[must_use]
    pub fn create_isa_bridge(bdf: PciBdf, vendor_id: u16, device_id: u16) -> PciConfigSpace {
        let mut dev = PciConfigSpace::new(bdf, vendor_id, device_id);
        dev.set_class(0x06, 0x01, 0x00, 0x00); // ISA bridge
        dev.set_header_type(0x00);
        dev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_space_read_write() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 0, 0), 0x8086, 0x1234);
        assert_eq!(cs.vendor_id(), 0x8086);
        assert_eq!(cs.device_id(), 0x1234);
        assert!(cs.is_present());
    }

    #[test]
    fn empty_config_space() {
        let cs = PciConfigSpace::empty(PciBdf::new(0, 1, 0));
        assert_eq!(cs.vendor_id(), 0xFFFF);
        assert!(!cs.is_present());
    }

    #[test]
    fn ecam_read_present_device() {
        let mut rc = PcieRootComplex::new(0xB000_0000);
        let dev = PciConfigSpace::new(PciBdf::new(0, 2, 0), 0x8086, 0x5678);
        rc.add_device(dev);

        // BDF 0:2.0 = offset (2 << 15) = 0x10000
        let vendor = rc.ecam_read(0x10000, 2);
        assert_eq!(vendor, 0x8086);
    }

    #[test]
    fn ecam_read_absent_device() {
        let rc = PcieRootComplex::new(0xB000_0000);
        let result = rc.ecam_read(0x08000, 4); // BDF 0:1.0
        assert_eq!(result, 0xFFFF_FFFF);
    }

    #[test]
    fn bar_size_detection() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 3, 0), 0x8086, 0x9999);
        // 4KB BAR at BAR0
        cs.set_bar(0, 0xFEE0_0000, 0xFFFF_F000);

        // Guest writes all-ones to detect size
        cs.guest_write_u32(cfg::BAR0, 0xFFFF_FFFF);
        let readback = cs.read_u32(cfg::BAR0);
        // Should have mask applied
        assert_eq!(readback & 0xFFFF_F000, 0xFFFF_F000);
    }

    #[test]
    fn host_bridge_creation() {
        let dev = PcieRootComplex::create_host_bridge(0x8086, 0x9A14);
        assert_eq!(dev.vendor_id(), 0x8086);
        assert_eq!(dev.read_u8(cfg::CLASS_CODE), 0x06);
    }

    #[test]
    fn bdf_display() {
        let bdf = PciBdf::new(0, 31, 3);
        assert_eq!(format!("{bdf}"), "00:1f.3");
    }

    #[test]
    fn ecam_offset_calculation() {
        let bdf = PciBdf::new(0, 2, 0);
        assert_eq!(bdf.ecam_offset(), 0x10000);
    }
}
