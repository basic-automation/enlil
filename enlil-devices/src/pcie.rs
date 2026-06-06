//! PCI Express root complex and configuration space emulation
//!
//! Provides a virtual `PCIe` root complex for guest VMs. Windows expects
//! a PCI Express bus with ECAM (Enhanced Configuration Access Mechanism)
//! for device enumeration.

use crate::bus::{MmioDevice, PioDevice};
use crate::truncate::{u8_of, u16_of, u32_of};
use std::cell::RefCell;
use std::rc::Rc;

/// A reference-counted, interior-mutable handle to a [`PcieRootComplex`].
///
/// Both config-space front-ends — the legacy PIO [`PciConfigIo`]
/// (`0xCF8`/`0xCFC`) and the MMIO [`EcamSpace`] — hold a clone of one of these,
/// so a register (e.g. a BAR) programmed through either path is immediately
/// visible through the other: there is a *single* backing device set, not two
/// divergent copies. The vCPU run loop is single-threaded today, so `Rc<RefCell>`
/// is sufficient; revisit to `Arc<Mutex>` only when the bus must cross vCPU
/// threads.
pub type SharedRootComplex = Rc<RefCell<PcieRootComplex>>;
/// PCI configuration space size per function
pub const PCI_CONFIG_SPACE_SIZE: usize = 256;
/// `PCIe` extended configuration space size per function
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
        let data = vec![0u8; PCIE_CONFIG_SPACE_SIZE];
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
            let offset = cfg::BAR0 + (u16_of(bar_index)) * 4;
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
        if (cfg::BAR0..=cfg::BAR5).contains(&offset) {
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
    pub const fn new(ecam_base: u64) -> Self {
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

        self.find_device(&bdf)
            .map_or(0xFFFF_FFFF, |dev| match size {
                1 => u32::from(dev.read_u8(reg_offset)),
                2 => u32::from(dev.read_u16(reg_offset)),
                4 => dev.read_u32(reg_offset),
                _ => 0xFFFF_FFFF,
            })
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
                1 => dev.write_u8(reg_offset, u8_of(value)),
                2 => dev.write_u16(reg_offset, u16_of(value)),
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

/// Legacy PCI Configuration Mechanism #1: the `CONFIG_ADDRESS` port (32-bit
/// register at `0xCF8`).
pub const CONFIG_ADDRESS_PORT: u16 = 0xCF8;
/// Legacy PCI Configuration Mechanism #1: the `CONFIG_DATA` window (32-bit, at
/// `0xCFC`-`0xCFF`).
pub const CONFIG_DATA_PORT: u16 = 0xCFC;
/// Bit 31 of `CONFIG_ADDRESS` enables a configuration cycle.
const CONFIG_ENABLE: u32 = 0x8000_0000;

/// Legacy PCI **Configuration Mechanism #1** front-end (the `0xCF8`/`0xCFC` port
/// pair) over a [`PcieRootComplex`].
///
/// This is the access mechanism a guest BIOS / early kernel uses to enumerate
/// the PCI bus *before* it has set up ECAM MMIO. Mechanism #1 latches a target
/// address (bus/device/function/register) into the 32-bit `CONFIG_ADDRESS`
/// register at port `0xCF8`, then reads or writes the selected configuration
/// register through the `CONFIG_DATA` window at `0xCFC`-`0xCFF`.
///
/// `CONFIG_ADDRESS` layout (per the PCI Local Bus spec):
/// ```text
///  31     30..24    23..16   15..11   10..8    7..2    1..0
/// ┌────┬──────────┬────────┬────────┬───────┬───────┬──────┐
/// │ EN │ reserved │  bus   │ device │ func  │  reg  │  00  │
/// └────┴──────────┴────────┴────────┴───────┴───────┴──────┘
/// ```
/// The low two bits of the register select are always zero (dword-aligned), so
/// a byte/word access to `CONFIG_DATA` is steered to the right sub-register by
/// the *port* offset within the `0xCFC`-`0xCFF` window — `reg | (port - 0xCFC)`.
/// All decode reuses [`PcieRootComplex::ecam_read`]/[`PcieRootComplex::ecam_write`]
/// so there is a single config-space decode path shared with the ECAM front-end:
/// the latched B/D/F is folded back into an ECAM-style offset, whose low 8 bits
/// cover the 256-byte legacy config space Mechanism #1 can reach.
pub struct PciConfigIo {
    /// The (shared) root complex whose devices are enumerated through these
    /// ports. Shared with the MMIO [`EcamSpace`] so both front-ends mutate one
    /// device set.
    root: SharedRootComplex,
    /// The latched `CONFIG_ADDRESS` value (port `0xCF8`).
    config_address: u32,
}

/// Low-`size`-byte mask (1/2/4 bytes → `0xFF`/`0xFFFF`/`0xFFFF_FFFF`).
const fn size_mask(size: u8) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

impl PciConfigIo {
    /// Wrap a root complex behind the legacy `CONFIG_ADDRESS`/`CONFIG_DATA`
    /// ports, taking sole ownership of it (it is moved into a fresh shared
    /// handle). Use [`PciConfigIo::with_shared`] to share a root complex with an
    /// [`EcamSpace`].
    #[must_use]
    pub fn new(root: PcieRootComplex) -> Self {
        Self::with_shared(Rc::new(RefCell::new(root)))
    }

    /// Wrap an already-shared root complex behind the legacy ports, so an
    /// [`EcamSpace`] mounted over the same handle sees the same device set.
    #[must_use]
    pub const fn with_shared(root: SharedRootComplex) -> Self {
        Self {
            root,
            config_address: 0,
        }
    }

    /// A clone of the shared root-complex handle (e.g. to add devices or to
    /// mount a matching [`EcamSpace`] over the same device set).
    #[must_use]
    pub fn shared(&self) -> SharedRootComplex {
        Rc::clone(&self.root)
    }

    /// Whether the enable bit (`CONFIG_ADDRESS` bit 31) is set — a config cycle
    /// is only generated when it is.
    const fn enabled(&self) -> bool {
        self.config_address & CONFIG_ENABLE != 0
    }

    /// ECAM-style offset for a `CONFIG_DATA` access whose byte offset within the
    /// `0xCFC`-`0xCFF` window is `data_offset` (0..=3). Folds the latched B/D/F
    /// and the dword-aligned register select back into the same offset encoding
    /// [`PcieRootComplex::ecam_read`] decodes, so both front-ends share one path.
    fn target_offset(&self, data_offset: u16) -> u64 {
        let addr = self.config_address;
        let bus = ((addr >> 16) & 0xFF) as u8;
        let device = ((addr >> 11) & 0x1F) as u8;
        let function = ((addr >> 8) & 0x07) as u8;
        // Register select (bits 7:2) with the data-window byte offset supplying
        // the low two bits the latched address forces to zero.
        let reg = (addr & 0xFC) | u32::from(data_offset & 0x3);
        PciBdf::new(bus, device, function).ecam_offset() as u64 | u64::from(reg)
    }

    /// Merge a (possibly sub-dword) guest write into the latched
    /// `CONFIG_ADDRESS`, leaving the bytes outside the access untouched.
    fn write_address(&mut self, byte_off: u16, size: u8, data: u32) {
        let shift = u32::from(byte_off) * 8;
        if shift >= 32 {
            return;
        }
        let mask = size_mask(size) << shift;
        let value = (data & size_mask(size)) << shift;
        self.config_address = (self.config_address & !mask) | value;
    }
}

impl PioDevice for PciConfigIo {
    fn pio_read(&mut self, port: u16, size: u8) -> u32 {
        if port < CONFIG_DATA_PORT {
            // CONFIG_ADDRESS window (0xCF8-0xCFB): return the latched value with
            // the addressed byte shifted down into the low bits.
            let shift = u32::from(port - CONFIG_ADDRESS_PORT) * 8;
            (self.config_address >> shift) & size_mask(size)
        } else {
            // CONFIG_DATA window (0xCFC-0xCFF): a config cycle only happens with
            // the enable bit set; otherwise the read is open-bus.
            if !self.enabled() {
                return size_mask(size);
            }
            let offset = self.target_offset(port - CONFIG_DATA_PORT);
            self.root.borrow().ecam_read(offset, size)
        }
    }

    fn pio_write(&mut self, port: u16, size: u8, data: u32) {
        if port < CONFIG_DATA_PORT {
            self.write_address(port - CONFIG_ADDRESS_PORT, size, data);
        } else if self.enabled() {
            let offset = self.target_offset(port - CONFIG_DATA_PORT);
            self.root.borrow_mut().ecam_write(offset, data, size);
        }
    }

    fn port_range(&self) -> (u16, u16) {
        // Eight ports: CONFIG_ADDRESS (0xCF8-0xCFB) + CONFIG_DATA (0xCFC-0xCFF).
        (CONFIG_ADDRESS_PORT, CONFIG_DATA_PORT + 4)
    }
}

/// Size of a single PCI segment's ECAM window: 256 buses × 1 MiB/bus
/// (`bus << 20`), i.e. 256 MiB.
///
/// This matches the buses `0..=255` a standard single-segment MCFG advertises
/// (see `acpi::mcfg`), so the MMIO window the guest is told about and the one we
/// decode are the same span.
pub const ECAM_SEGMENT_SIZE: u64 = 256 << 20;

/// **ECAM** (Enhanced Configuration Access Mechanism) MMIO front-end over a
/// [`PcieRootComplex`].
///
/// This is the memory-mapped config-space window a `PCIe`-aware guest (notably
/// Windows, and any modern Linux that honours the MCFG table) uses *after* it
/// has discovered the ECAM base from ACPI MCFG. The window lives at the root
/// complex's `ecam_base` and spans [`ECAM_SEGMENT_SIZE`] (one PCI segment,
/// buses 0..=255). An access at offset `o` within the window maps directly to
/// config-space offset `o` via the ECAM addressing formula
/// `(bus << 20) | (device << 15) | (function << 12) | reg` — which is exactly
/// the encoding [`PcieRootComplex::ecam_read`]/[`PcieRootComplex::ecam_write`]
/// already decode, so this device is a thin forward with **no** second B/D/F
/// decode path. Unlike the legacy PIO [`PciConfigIo`] (which reaches only the
/// first 256 bytes), ECAM exposes the full 4 KiB extended config space.
///
/// The root complex is shared (via [`SharedRootComplex`]) with the matching
/// `PciConfigIo`, so a guest that programs a register through one mechanism sees
/// it through the other.
pub struct EcamSpace {
    /// The shared root complex this window decodes into.
    root: SharedRootComplex,
    /// Guest-physical base of the ECAM window (the root complex's `ecam_base`).
    base: u64,
}

impl EcamSpace {
    /// Mount an ECAM window over `root` at its configured `ecam_base`.
    #[must_use]
    pub fn new(root: SharedRootComplex) -> Self {
        let base = root.borrow().ecam_base;
        Self { root, base }
    }
}

impl MmioDevice for EcamSpace {
    fn mmio_read(&mut self, offset: u64, size: u8) -> u64 {
        match size {
            1 | 2 | 4 => u64::from(self.root.borrow().ecam_read(offset, size)),
            8 => {
                // A naturally-aligned 8-byte config read is two adjacent dwords;
                // ecam_read is dword-granular, so combine low + high.
                let root = self.root.borrow();
                let lo = u64::from(root.ecam_read(offset, 4));
                let hi = u64::from(root.ecam_read(offset + 4, 4));
                lo | (hi << 32)
            }
            // Any other width reads open-bus, matching an absent decode.
            _ => u64::MAX,
        }
    }

    fn mmio_write(&mut self, offset: u64, size: u8, data: u64) {
        match size {
            1 | 2 | 4 => self
                .root
                .borrow_mut()
                .ecam_write(offset, u32_of(data), size),
            8 => {
                let mut root = self.root.borrow_mut();
                root.ecam_write(offset, u32_of(data), 4);
                root.ecam_write(offset + 4, u32_of(data >> 32), 4);
            }
            _ => {}
        }
    }

    fn mmio_range(&self) -> (u64, u64) {
        (self.base, self.base + ECAM_SEGMENT_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_space_read_write() {
        let cs = PciConfigSpace::new(PciBdf::new(0, 0, 0), 0x8086, 0x1234);
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

    // --- Legacy PCI Configuration Mechanism #1 (0xCF8/0xCFC) ---

    /// Build a `CONFIG_ADDRESS` value for an enabled config cycle.
    fn config_address(bus: u8, device: u8, function: u8, reg: u8) -> u32 {
        CONFIG_ENABLE
            | (u32::from(bus) << 16)
            | (u32::from(device) << 11)
            | (u32::from(function) << 8)
            | u32::from(reg & 0xFC)
    }

    /// A root complex with one device (Intel `0x8086:0x5678`) at BDF 0:2.0.
    fn io_with_device() -> PciConfigIo {
        let mut rc = PcieRootComplex::new(0xB000_0000);
        rc.add_device(PciConfigSpace::new(PciBdf::new(0, 2, 0), 0x8086, 0x5678));
        PciConfigIo::new(rc)
    }

    #[test]
    fn config_address_latches_and_reads_back() {
        let mut io = PciConfigIo::new(PcieRootComplex::new(0));
        io.pio_write(CONFIG_ADDRESS_PORT, 4, 0x8000_1234);
        assert_eq!(io.pio_read(CONFIG_ADDRESS_PORT, 4), 0x8000_1234);
        // A byte read of 0xCF9 returns byte 1 of the latched value (0x12).
        assert_eq!(io.pio_read(CONFIG_ADDRESS_PORT + 1, 1), 0x12);
    }

    #[test]
    fn byte_writes_to_config_address_merge_in_place() {
        let mut io = PciConfigIo::new(PcieRootComplex::new(0));
        io.pio_write(CONFIG_ADDRESS_PORT, 4, 0x8000_0000);
        // Byte-poke bus=0x05 into bits 23:16 (0xCFA) without disturbing the rest.
        io.pio_write(CONFIG_ADDRESS_PORT + 2, 1, 0x05);
        assert_eq!(io.pio_read(CONFIG_ADDRESS_PORT, 4), 0x8005_0000);
    }

    #[test]
    fn mechanism1_reads_vendor_and_device_id() {
        let mut io = io_with_device();
        io.pio_write(CONFIG_ADDRESS_PORT, 4, config_address(0, 2, 0, 0x00));
        // Dword read yields device_id:vendor_id (0x5678_8086).
        assert_eq!(io.pio_read(CONFIG_DATA_PORT, 4), 0x5678_8086);
    }

    #[test]
    fn mechanism1_subword_access_steers_by_data_port_offset() {
        let mut io = io_with_device();
        io.pio_write(CONFIG_ADDRESS_PORT, 4, config_address(0, 2, 0, 0x00));
        // Word at 0xCFC = vendor, word at 0xCFE = device id, byte at 0xCFD = vendor hi.
        assert_eq!(io.pio_read(CONFIG_DATA_PORT, 2), 0x8086);
        assert_eq!(io.pio_read(CONFIG_DATA_PORT + 2, 2), 0x5678);
        assert_eq!(io.pio_read(CONFIG_DATA_PORT + 1, 1), 0x80);
    }

    #[test]
    fn disabled_config_cycle_reads_open_bus() {
        let mut io = io_with_device();
        // Same B/D/F but enable bit (31) clear -> no config cycle.
        io.pio_write(
            CONFIG_ADDRESS_PORT,
            4,
            config_address(0, 2, 0, 0) & !CONFIG_ENABLE,
        );
        assert_eq!(io.pio_read(CONFIG_DATA_PORT, 4), 0xFFFF_FFFF);
        assert_eq!(io.pio_read(CONFIG_DATA_PORT, 2), 0xFFFF);
    }

    #[test]
    fn mechanism1_absent_device_reads_all_ones() {
        let mut io = io_with_device();
        // BDF 0:3.0 has no device.
        io.pio_write(CONFIG_ADDRESS_PORT, 4, config_address(0, 3, 0, 0x00));
        assert_eq!(io.pio_read(CONFIG_DATA_PORT, 4), 0xFFFF_FFFF);
    }

    #[test]
    fn mechanism1_write_reaches_config_space() {
        let mut io = io_with_device();
        // Program the interrupt-line register (0x3C) through CONFIG_DATA.
        // 0x3C == cfg::INTERRUPT_LINE.
        io.pio_write(CONFIG_ADDRESS_PORT, 4, config_address(0, 2, 0, 0x3C));
        io.pio_write(CONFIG_DATA_PORT, 1, 0x0B);
        // Read it straight back through the same window.
        assert_eq!(io.pio_read(CONFIG_DATA_PORT, 1), 0x0B);
        // ...and it landed in the backing config space at offset 0x3C.
        let root = io.shared();
        let root = root.borrow();
        let dev = root.find_device(&PciBdf::new(0, 2, 0)).unwrap();
        assert_eq!(dev.read_u8(cfg::INTERRUPT_LINE), 0x0B);
    }

    #[test]
    fn write_disabled_config_cycle_is_dropped() {
        let mut io = io_with_device();
        io.pio_write(
            CONFIG_ADDRESS_PORT,
            4,
            config_address(0, 2, 0, 0x3C) & !CONFIG_ENABLE,
        );
        io.pio_write(CONFIG_DATA_PORT, 1, 0xEE);
        let root = io.shared();
        let root = root.borrow();
        let dev = root.find_device(&PciBdf::new(0, 2, 0)).unwrap();
        assert_eq!(
            dev.read_u8(cfg::INTERRUPT_LINE),
            0x00,
            "disabled write must not reach config space"
        );
    }

    #[test]
    fn port_range_claims_the_eight_legacy_cam_ports() {
        let io = PciConfigIo::new(PcieRootComplex::new(0));
        assert_eq!(io.port_range(), (0xCF8, 0xD00));
    }

    /// A shared root complex (base `0xB000_0000`) with one device at BDF 0:2.0.
    fn shared_with_device() -> SharedRootComplex {
        let mut rc = PcieRootComplex::new(0xB000_0000);
        rc.add_device(PciConfigSpace::new(PciBdf::new(0, 2, 0), 0x8086, 0x5678));
        Rc::new(RefCell::new(rc))
    }

    #[test]
    fn ecam_window_spans_one_segment_at_the_base() {
        let ecam = EcamSpace::new(Rc::new(RefCell::new(PcieRootComplex::new(0xB000_0000))));
        // 256 buses × 1 MiB = 256 MiB → [0xB000_0000, 0xC000_0000).
        assert_eq!(ecam.mmio_range(), (0xB000_0000, 0xC000_0000));
    }

    #[test]
    fn ecam_reads_vendor_and_device_id() {
        let mut ecam = EcamSpace::new(shared_with_device());
        // Offset of BDF 0:2.0's config space within the window, register 0.
        let offset = PciBdf::new(0, 2, 0).ecam_offset() as u64;
        assert_eq!(ecam.mmio_read(offset, 4), 0x5678_8086);
        // Word/byte sub-accesses steer within the dword like the PIO path.
        assert_eq!(ecam.mmio_read(offset, 2), 0x8086);
        assert_eq!(ecam.mmio_read(offset + 2, 2), 0x5678);
    }

    #[test]
    fn ecam_absent_device_reads_all_ones() {
        let mut ecam = EcamSpace::new(shared_with_device());
        // BDF 0:3.0 has no device.
        let offset = PciBdf::new(0, 3, 0).ecam_offset() as u64;
        assert_eq!(ecam.mmio_read(offset, 4), 0xFFFF_FFFF);
    }

    #[test]
    fn ecam_eight_byte_read_combines_two_adjacent_dwords() {
        let shared = shared_with_device();
        // Program a known dword at register 0x10 (BAR0) so the high half of an
        // 8-byte read starting at register 0x0C is non-trivial.
        shared
            .borrow_mut()
            .find_device_mut(&PciBdf::new(0, 2, 0))
            .unwrap()
            .write_u32(0x10, 0xCAFE_F00D);
        let mut ecam = EcamSpace::new(shared);
        let base = PciBdf::new(0, 2, 0).ecam_offset() as u64;
        // dword at 0x0C is the BIST/header/latency/cache word (header type 0 →
        // 0x0000_0000 by default); dword at 0x10 is the BAR we just set.
        let lo = u64::from(0x0000_0000u32);
        let hi = u64::from(0xCAFE_F00Du32);
        assert_eq!(ecam.mmio_read(base + 0x0C, 8), lo | (hi << 32));
    }

    #[test]
    fn cam_and_ecam_share_one_device_set() {
        // One shared root complex behind both front-ends.
        let shared = shared_with_device();
        let mut cam = PciConfigIo::with_shared(Rc::clone(&shared));
        let mut ecam = EcamSpace::new(Rc::clone(&shared));

        // Program the interrupt-line register (0x3C) of 0:2.0 through the legacy
        // CAM ports...
        cam.pio_write(CONFIG_ADDRESS_PORT, 4, config_address(0, 2, 0, 0x3C));
        cam.pio_write(CONFIG_DATA_PORT, 1, 0x0B);

        // ...and read it straight back through the ECAM MMIO window: the write
        // is visible because both front-ends decode into the same device set.
        let offset = PciBdf::new(0, 2, 0).ecam_offset() as u64 + 0x3C;
        assert_eq!(ecam.mmio_read(offset, 1), 0x0B);

        // The reverse direction too: an ECAM write is seen through CAM.
        ecam.mmio_write(offset, 1, 0x2A);
        cam.pio_write(CONFIG_ADDRESS_PORT, 4, config_address(0, 2, 0, 0x3C));
        assert_eq!(cam.pio_read(CONFIG_DATA_PORT, 1), 0x2A);
    }
}
