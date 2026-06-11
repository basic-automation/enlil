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
    pub const RENESAS: u16 = 0x1912;
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

    /// The ACPI `_ADR` value for this function: `(device << 16) | function`
    /// (ACPI §6.1.1). A device object under a PCI bus uses this to bind to the
    /// PCI function at this address. The bus number is *not* part of `_ADR` — the
    /// parent bus device fixes it.
    #[must_use]
    pub const fn acpi_adr(&self) -> u32 {
        ((self.device as u32) << 16) | (self.function as u32)
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

    /// Handle a guest config-space write of `width` (1/2/4) bytes at `offset`,
    /// respecting both BAR size-detection masks and the **read-only header
    /// registers** a guest must not be able to change (Vendor/Device ID, Class
    /// Code, Header Type, Subsystem IDs — the device identity). Real hardware
    /// ignores writes to those registers; a guest that tries (some drivers
    /// probe-write to size a register) used to scribble over the identity the
    /// platform programmed. Firmware/platform seeding uses the unmasked
    /// `write_u*` methods, so this only constrains *guest* writes.
    pub fn guest_write(&mut self, offset: u16, width: u8, value: u32) {
        // BARs: the writable bits are the size-detection mask; the rest hold.
        if (cfg::BAR0..=cfg::BAR5).contains(&offset) && width == 4 {
            let bar_idx = ((offset - cfg::BAR0) / 4) as usize;
            if bar_idx < 6 {
                let mask = self.bar_masks[bar_idx];
                let current = self.read_u32(offset);
                self.write_u32(offset, (value & mask) | (current & !mask));
                return;
            }
        }
        // Everything else: write byte by byte, skipping read-only bytes.
        let bytes = value.to_le_bytes();
        for (i, &b) in bytes.iter().enumerate().take(usize::from(width)) {
            let off = offset + u16::try_from(i).unwrap_or(0);
            if !Self::byte_is_read_only(off) {
                self.write_u8(off, b);
            }
        }
    }

    /// Handle a guest 32-bit config write (BAR-mask + read-only aware).
    pub fn guest_write_u32(&mut self, offset: u16, value: u32) {
        self.guest_write(offset, 4, value);
    }

    /// Whether the config byte at `offset` is a read-only Type 0 header
    /// register a guest write must not change: the device identity (Vendor ID
    /// `0x00-01`, Device ID `0x02-03`, Revision ID `0x08`, Class/Subclass/
    /// `ProgIF` `0x09-0x0B`, Header Type `0x0E`) and the Subsystem IDs
    /// (`0x2C-0x2F`). Command/Status, the cache-line/latency bytes, BARs, and
    /// the interrupt line stay guest-writable.
    #[must_use]
    const fn byte_is_read_only(offset: u16) -> bool {
        matches!(offset,
            cfg::VENDOR_ID..=0x03                  // Vendor + Device ID
            | cfg::REVISION_ID..=cfg::CLASS_CODE   // Revision, ProgIF, Subclass, Class
            | cfg::HEADER_TYPE
            | cfg::SUBSYSTEM_VENDOR_ID..=0x2F) // Subsystem Vendor + Device ID
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
            // Every width is a guest write: BAR size-detection masks and the
            // read-only header registers (device identity) are honored, so a
            // guest can't reprogram Vendor/Device/Subsystem/Class IDs.
            if matches!(size, 1 | 2 | 4) {
                dev.guest_write(reg_offset, size, value);
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

    /// Create the **Q35 MCH** host bridge (`00:00.0`, `8086:29C0`) with its
    /// `PCIEXBAR` register seeded to advertise the live ECAM window.
    ///
    /// On a real Q35 the MCH's `PCIEXBAR` (config `0x60`, 64-bit) is where the
    /// ECAM base physically comes from — firmware programs it and *then* writes
    /// the same address into the MCFG ACPI table. A guest (or a detector) can
    /// read it back through config space and cross-check it against MCFG, so the
    /// register must encode the same base the platform actually decodes:
    /// bit 0 = enable, bits 2:1 = window size (`00` = 256 MiB, the full
    /// single-segment window), bits 38:28 = the base address.
    #[must_use]
    pub fn create_q35_host_bridge(ecam_base: u64) -> PciConfigSpace {
        let mut dev = Self::create_host_bridge(vendors::INTEL, Q35_HOST_BRIDGE_DEVICE_ID);
        // A2-stepping silicon: the revision real 82Q35 parts report.
        dev.write_u8(cfg::REVISION_ID, 0x02);
        // The board vendor's subsystem IDs, as board firmware programs them.
        dev.set_subsystem(BOARD_SUBSYSTEM_VENDOR_ID, BOARD_SUBSYSTEM_DEVICE_ID);
        let pciexbar = (ecam_base & PCIEXBAR_ADDR_MASK) | PCIEXBAR_ENABLE;
        dev.write_u32(PCIEXBAR_OFFSET, u32_of(pciexbar & 0xFFFF_FFFF));
        dev.write_u32(PCIEXBAR_OFFSET + 4, u32_of(pciexbar >> 32));
        dev
    }

    /// Create the **ICH9 `SMBus` host controller** (`00:1F.3`, `8086:2930`).
    ///
    /// Every ICH-generation `D31` carries this function — a southbridge whose
    /// `1F.3` is absent is a SKU that never shipped, so the chipset identity
    /// needs it present. `io_base` is the firmware-assigned `SMB_BASE` (BAR4,
    /// a 32-byte I/O block; the live register file is
    /// [`smbus::SmbusHost`](crate::smbus::SmbusHost)). The interrupt pin is
    /// `INTB#` (as the datasheet hardwires), and the interrupt line is
    /// pre-programmed to the IRQ the default PIRQ routing resolves that pin
    /// to — so config space, the PIRQ registers, and the DSDT link devices
    /// tell the guest one consistent story.
    ///
    /// # Panics
    /// Never in practice: `INTB#` is a valid interrupt pin, so the default
    /// PIRQ routing always resolves it.
    #[must_use]
    pub fn create_ich9_smbus(io_base: u16) -> PciConfigSpace {
        let mut dev = PciConfigSpace::new(ICH9_SMBUS_BDF, vendors::INTEL, ICH9_SMBUS_DEVICE_ID);
        dev.set_class(0x0C, 0x05, 0x00, 0x02); // Serial bus: SMBus, A2 stepping
        dev.set_header_type(0x00);
        dev.set_subsystem(BOARD_SUBSYSTEM_VENDOR_ID, BOARD_SUBSYSTEM_DEVICE_ID);
        // BAR4 = SMB_BASE: a 32-byte I/O BAR (bit 0 = I/O space indicator).
        dev.set_bar(4, u32::from(io_base) | 1, 0xFFFF_FFE0);
        let line = crate::interrupt::PirqRouter::default_device_isa_irq(
            ICH9_SMBUS_BDF.device,
            SMBUS_INTERRUPT_PIN,
        )
        .expect("INTB# always swizzles to a PIRQ line");
        dev.set_interrupt(line, SMBUS_INTERRUPT_PIN);
        dev
    }

    /// Create the discrete **xHCI USB 3.0 host controller** function: a
    /// Renesas uPD720202 (`1912:0015`), the ubiquitous add-in xHCI chip of
    /// the era Enlil's chipset models (the Q35 generation predates
    /// chipset-integrated xHCI, so a discrete controller is the identity a
    /// real board of that generation would carry). `mmio_base` is the
    /// firmware-assigned BAR0 (a 64 KiB MMIO window holding the register
    /// file in [`usb::XhciMmio`](crate::usb::XhciMmio)). The interrupt pin
    /// is `INTA#`, pre-routed to the default PIRQ resolution for its slot.
    ///
    /// # Panics
    /// Never in practice: `INTA#` is a valid interrupt pin, so the default
    /// PIRQ routing always resolves it.
    #[must_use]
    pub fn create_xhci_controller(bdf: PciBdf, mmio_base: u32) -> PciConfigSpace {
        let mut dev = PciConfigSpace::new(bdf, vendors::RENESAS, XHCI_DEVICE_ID);
        dev.set_class(0x0C, 0x03, 0x30, 0x02); // Serial bus: USB, xHCI
        dev.set_header_type(0x00);
        // BAR0: 64 KiB non-prefetchable 32-bit memory.
        dev.set_bar(0, mmio_base, 0xFFFF_0000);
        let line = crate::interrupt::PirqRouter::default_device_isa_irq(bdf.device, 1)
            .expect("INTA# always swizzles to a PIRQ line");
        dev.set_interrupt(line, 1);
        dev
    }

    /// Create a standard ISA/LPC bridge device.
    ///
    /// For an ICH9-style (or PIIX3-style) bridge this is also the **PCI
    /// interrupt router**: the four `PIRQ[A-D]_ROUT` routing registers live in
    /// this device's config space (the four bytes from
    /// [`PIRQ_ROUTE_CONFIG_BASE`]), and reset to `0x80` (routing disabled),
    /// which is what a guest reads before it programs them. The
    /// [`PirqRouter`](crate::interrupt::PirqRouter) is synced from those bytes
    /// via [`sync_from_config`](crate::interrupt::PirqRouter::sync_from_config).
    /// The ICH9's second bank, `PIRQ[E-H]_ROUT` at `0x68`..`0x6B`, is also
    /// seeded to its reset state; nothing in-tree routes through E-H yet.
    #[must_use]
    pub fn create_isa_bridge(bdf: PciBdf, vendor_id: u16, device_id: u16) -> PciConfigSpace {
        let mut dev = PciConfigSpace::new(bdf, vendor_id, device_id);
        dev.set_class(0x06, 0x01, 0x00, 0x00); // ISA bridge
        dev.set_header_type(0x00);
        // PIRQ routing registers reset to "disabled" (bit 7 set): A-D and the
        // ICH9-only E-H bank.
        for i in 0..4 {
            dev.write_u8(PIRQ_ROUTE_CONFIG_BASE + i, 0x80);
            dev.write_u8(PIRQ_EH_ROUTE_CONFIG_BASE + i, 0x80);
        }
        dev
    }
}

/// PCI device ID of the Q35 MCH host bridge (Intel 82Q35 Express DRAM
/// Controller, `D0:F0`).
///
/// The chipset generation Enlil models: unlike the i440FX it actually *has*
/// ECAM (`PCIEXBAR`), so the MCFG table the platform emits describes a register
/// the host bridge really carries.
pub const Q35_HOST_BRIDGE_DEVICE_ID: u16 = 0x29C0;

/// MCH config-space offset of `PCIEXBAR` (the PCI Express register-range base
/// address; 64 bits at `0x60`-`0x67`). Intel 3 Series chipset datasheet §5.1.9.
pub const PCIEXBAR_OFFSET: u16 = 0x60;
/// `PCIEXBAR` bit 0: the ECAM window decode enable.
pub const PCIEXBAR_ENABLE: u64 = 1;
/// `PCIEXBAR` base-address field: bits 38:28 (a 256 MiB-aligned base; with the
/// length field left at `00` the window is the full 256 MiB segment).
pub const PCIEXBAR_ADDR_MASK: u64 = 0x7F_F000_0000;

/// The PCI location of the ICH9 LPC interface bridge: `00:1F.0` (`D31:F0`).
///
/// This is the single source of truth for the bridge's address — the device bus
/// mounts the live bridge here and the DSDT's `ISA_` device object derives its
/// `_ADR` from it ([`PciBdf::acpi_adr`]), so the ACPI namespace binds to the
/// real bridge instead of an empty slot.
pub const ICH9_LPC_BRIDGE_BDF: PciBdf = PciBdf::new(0, 31, 0);

/// PCI device ID of the ICH9 LPC interface bridge (Intel 82801IB, `D31:F0`).
pub const ICH9_LPC_DEVICE_ID: u16 = 0x2918;

/// PCI subsystem **vendor** ID stamped on the chipset's onboard functions:
/// `ASUSTeK` Computer Inc.'s PCI-SIG vendor ID.
///
/// A real board's firmware programs the board vendor's ID into the subsystem
/// vendor register of every onboard function; a guest (or detector) can
/// cross-check it against the baseboard manufacturer SMBIOS advertises
/// (`ASUSTeK COMPUTER INC.` in the default profile — a test pins the pair).
/// All-zero subsystem IDs are what an unconfigured/synthetic platform shows.
pub const BOARD_SUBSYSTEM_VENDOR_ID: u16 = 0x1043;

/// PCI subsystem **device** ID stamped on the chipset's onboard functions.
///
/// A board-specific value the vendor assigns; boards reuse one value across
/// their onboard chipset functions, which is exactly what we do.
pub const BOARD_SUBSYSTEM_DEVICE_ID: u16 = 0x8694;

/// PCI device ID of the Renesas uPD720202 xHCI USB 3.0 host controller.
pub const XHCI_DEVICE_ID: u16 = 0x0015;

/// The PCI location of the ICH9 `SMBus` host controller: `00:1F.3` (`D31:F3`).
pub const ICH9_SMBUS_BDF: PciBdf = PciBdf::new(0, 31, 3);

/// PCI device ID of the ICH9 `SMBus` host controller (Intel 82801IB, `D31:F3`).
pub const ICH9_SMBUS_DEVICE_ID: u16 = 0x2930;

/// The `SMBus` controller's interrupt pin: `INTB#` (config `0x3D` = 2), per the
/// ICH9 datasheet's `D31:F3` interrupt-pin register.
pub const SMBUS_INTERRUPT_PIN: u8 = 2;

/// LPC config offset of `PMBASE` (ICH9 `D31:F0`, 32-bit).
///
/// Bits 15:7 are the ACPI PM I/O block's base, bit 0 is hardwired 1 (I/O
/// space). The fixed PM register offsets hang off it — `PM1_STS/EN` at +0,
/// `PM1_CNT` at +4, `PM1_TMR` at +8, `GPE0` at +0x20 — so the base this
/// register encodes, the blocks the FADT advertises, and the ports the bus
/// decodes must all agree.
pub const LPC_PMBASE_OFFSET: u16 = 0x40;

/// LPC config offset of `ACPI_CNTL` (ICH9 D31:F0): bit 7 (`ACPI_EN`) enables
/// the PMBASE decode, bits 2:0 select the SCI's ISA IRQ.
pub const LPC_ACPI_CNTL_OFFSET: u16 = 0x44;

/// `ACPI_CNTL` bit 7: the ACPI I/O decode enable.
pub const ACPI_CNTL_ACPI_EN: u8 = 0x80;

/// Encode an SCI IRQ into `ACPI_CNTL` bits 2:0 (ICH9 datasheet: 0-2 select
/// IRQ 9-11, 4-7 select IRQ 20-23; other IRQs are not selectable).
#[must_use]
pub const fn acpi_cntl_sci_select(irq: u8) -> u8 {
    match irq {
        10 => 1,
        11 => 2,
        20 => 4,
        21 => 5,
        22 => 6,
        23 => 7,
        _ => 0, // IRQ9, the power-on default
    }
}

/// Decode `ACPI_CNTL` bits 2:0 back to the SCI's ISA IRQ.
#[must_use]
pub const fn acpi_cntl_sci_irq(cntl: u8) -> u8 {
    match cntl & 0x7 {
        1 => 10,
        2 => 11,
        4 => 20,
        5 => 21,
        6 => 22,
        7 => 23,
        _ => 9,
    }
}

/// Config-space offset of the first PCI interrupt-routing register.
///
/// `PIRQA_ROUT`; the four `PIRQ[A-D]_ROUT` registers are contiguous at
/// `0x60`..`0x63` — the same offsets and byte semantics on the ICH9 LPC bridge
/// as on the PIIX3 it replaced. A guest programs interrupt routing by writing
/// these; the [`PirqRouter`](crate::interrupt::PirqRouter) reads them back.
pub const PIRQ_ROUTE_CONFIG_BASE: u16 = 0x60;

/// Config-space offset of the ICH9's second routing bank, `PIRQ[E-H]_ROUT`
/// (`0x68`..`0x6B`). Modeled only as reset-state config bytes for now — no
/// in-tree device routes through PIRQ E-H.
pub const PIRQ_EH_ROUTE_CONFIG_BASE: u16 = 0x68;

/// Legacy PCI Configuration Mechanism #1: the `CONFIG_ADDRESS` port (32-bit
/// register at `0xCF8`).
pub const CONFIG_ADDRESS_PORT: u16 = 0xCF8;
/// Legacy PCI Configuration Mechanism #1: the `CONFIG_DATA` window (32-bit, at
/// `0xCFC`-`0xCFF`).
pub const CONFIG_DATA_PORT: u16 = 0xCFC;
/// Bit 31 of `CONFIG_ADDRESS` enables a configuration cycle.
const CONFIG_ENABLE: u32 = 0x8000_0000;

/// The chipset **Reset Control Register** (`RST_CNT`) port.
///
/// A single byte the PIIX/ICH south-bridge decodes at `0xCF9` — physically inside
/// the `CONFIG_ADDRESS` dword window, but a *byte* access to `0xCF9` hits this
/// register, not config address byte 1. Writing it is the standard way modern
/// firmware and OSes reboot (Linux `BOOT_CF9`/`reboot=pci`).
pub const RESET_CONTROL_PORT: u16 = 0xCF9;
/// `RST_CNT` bit 1 (`SYS_RST`): selects a hard (1) vs soft (0) reset.
const RST_CNT_SYS_RST: u8 = 1 << 1;
/// `RST_CNT` bit 2 (`RST_CPU`): a 0→1 write triggers the reset.
const RST_CNT_RST_CPU: u8 = 1 << 2;
/// `RST_CNT` bit 3 (`FULL_RST`): with `SYS_RST`, requests a full power-cycle.
const RST_CNT_FULL_RST: u8 = 1 << 3;
/// The `RST_CNT` bits that latch and read back (`RST_CPU` is write-only / self-
/// clearing, so it is not stored).
const RST_CNT_STORED: u8 = RST_CNT_SYS_RST | RST_CNT_FULL_RST;
/// The `RST_CNT` value that triggers a (hard) reboot: `SYS_RST | RST_CPU`.
///
/// This is what the FADT's `RESET_VALUE` advertises for the `0xCF9` reset
/// register, so an OS resetting through the ACPI-advertised register hits the same
/// byte this model acts on.
pub const RST_CNT_REBOOT_VALUE: u8 = RST_CNT_SYS_RST | RST_CNT_RST_CPU;

/// The mutable state behind a [`PciResetControl`]: the read-back `RST_CNT` value
/// and the one-shot reboot latch.
#[derive(Default)]
struct ResetState {
    /// Stored `SYS_RST`/`FULL_RST` bits (what a guest reads back from `0xCF9`).
    rcr: u8,
    /// Set on a `RST_CPU` write; consumed by [`PciResetControl::take_reset`].
    reset_requested: bool,
}

/// A shareable handle to a [`PciConfigIo`]'s `0xCF9` Reset Control Register.
///
/// The register lives inside the boxed `PciConfigIo` on the bus, so the run loop
/// can't reach it directly; this clone of the same latch lets the platform layer
/// poll the reboot request (`take_reset`) — mirroring
/// [`SharedSystemControlPortA`](crate::chipset::SharedSystemControlPortA) for the
/// `0x92` fast-reset path.
#[derive(Clone, Default)]
pub struct PciResetControl(Rc<RefCell<ResetState>>);

impl PciResetControl {
    /// Apply a guest write to `RST_CNT`: a `RST_CPU` (bit 2) write latches a
    /// reboot request; the `SYS_RST`/`FULL_RST` bits are stored for read-back.
    fn write(&self, val: u8) {
        let mut state = self.0.borrow_mut();
        if val & RST_CNT_RST_CPU != 0 {
            state.reset_requested = true;
        }
        state.rcr = val & RST_CNT_STORED;
    }

    /// The current `RST_CNT` read-back value.
    #[must_use]
    pub fn value(&self) -> u8 {
        self.0.borrow().rcr
    }

    /// Consume the one-shot reboot latch: `true` exactly once per `RST_CPU` write.
    #[must_use]
    pub fn take_reset(&self) -> bool {
        let mut state = self.0.borrow_mut();
        let requested = state.reset_requested;
        state.reset_requested = false;
        requested
    }
}

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
    /// The chipset Reset Control Register (`0xCF9`), shared so the run loop can
    /// poll the reboot latch from outside the boxed device. See [`PciResetControl`].
    reset: PciResetControl,
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
    pub fn with_shared(root: SharedRootComplex) -> Self {
        Self {
            root,
            config_address: 0,
            reset: PciResetControl::default(),
        }
    }

    /// A clone of the shared `0xCF9` Reset Control latch, so the platform layer
    /// can poll the guest's reboot request after the device is boxed onto the bus.
    #[must_use]
    pub fn reset_handle(&self) -> PciResetControl {
        self.reset.clone()
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

    /// Whether a *byte* access to `port` targets the Reset Control Register
    /// (`0xCF9`) rather than a byte of `CONFIG_ADDRESS`. The chipset decodes the
    /// `0xCF9` byte specially; dword `CONFIG_ADDRESS` writes (port `0xCF8`) and
    /// accesses to `0xCFA`/`0xCFB` are untouched.
    const fn is_reset_control(port: u16, size: u8) -> bool {
        port == RESET_CONTROL_PORT && size == 1
    }

    /// Consume the one-shot `0xCF9` reboot latch: returns `true` exactly once per
    /// `RST_CPU` write, so the run loop re-inits the vCPU to its reset vector.
    #[must_use]
    pub fn take_reset(&self) -> bool {
        self.reset.take_reset()
    }

    /// The current Reset Control Register read-back value (`0xCF9`).
    #[must_use]
    pub fn reset_control(&self) -> u8 {
        self.reset.value()
    }
}

impl PioDevice for PciConfigIo {
    fn pio_read(&mut self, port: u16, size: u8) -> u32 {
        if Self::is_reset_control(port, size) {
            // 0xCF9 byte: the Reset Control Register, not CONFIG_ADDRESS byte 1.
            u32::from(self.reset_control())
        } else if port < CONFIG_DATA_PORT {
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
        if Self::is_reset_control(port, size) {
            // 0xCF9 byte: the Reset Control Register (reboot path), not a byte of
            // CONFIG_ADDRESS.
            self.reset.write(u8_of(data));
        } else if port < CONFIG_DATA_PORT {
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

    /// The Q35 MCH's identity and its `PCIEXBAR` must encode the same chipset
    /// generation and the same ECAM base the platform actually decodes — a guest
    /// can cross-check the host bridge ID against the existence of ECAM, and
    /// `PCIEXBAR` against the MCFG table, so the surfaces must agree.
    #[test]
    fn q35_host_bridge_identity_and_pciexbar() {
        let bridge = PcieRootComplex::create_q35_host_bridge(0xB000_0000);
        assert_eq!(bridge.vendor_id(), vendors::INTEL);
        assert_eq!(bridge.device_id(), Q35_HOST_BRIDGE_DEVICE_ID);
        // Host bridge class, A2-stepping revision.
        assert_eq!(bridge.read_u8(cfg::CLASS_CODE), 0x06);
        assert_eq!(bridge.read_u8(cfg::SUBCLASS), 0x00);
        assert_eq!(bridge.read_u8(cfg::REVISION_ID), 0x02);
        // PCIEXBAR: enabled, 256 MiB window (length bits 2:1 = 00), base intact.
        assert_eq!(bridge.read_u32(PCIEXBAR_OFFSET), 0xB000_0001);
        assert_eq!(bridge.read_u32(PCIEXBAR_OFFSET + 4), 0);
        // The board vendor's subsystem IDs are stamped, not left zero.
        assert_eq!(
            bridge.read_u16(cfg::SUBSYSTEM_VENDOR_ID),
            BOARD_SUBSYSTEM_VENDOR_ID
        );
        assert_eq!(
            bridge.read_u16(cfg::SUBSYSTEM_ID),
            BOARD_SUBSYSTEM_DEVICE_ID
        );
    }

    /// The PCI subsystem vendor and the SMBIOS baseboard manufacturer encode
    /// the same fact (who made the board); the default profiles must agree.
    #[test]
    fn board_subsystem_vendor_matches_the_smbios_baseboard_vendor() {
        let board = crate::smbios::SmbiosConfig::default().baseboard_manufacturer;
        assert!(
            board.starts_with("ASUSTeK"),
            "SMBIOS default baseboard is ASUSTeK; if this changes, change \
             BOARD_SUBSYSTEM_VENDOR_ID to the new vendor's PCI-SIG ID too"
        );
        assert_eq!(
            BOARD_SUBSYSTEM_VENDOR_ID, 0x1043,
            "0x1043 is ASUSTeK's PCI-SIG vendor ID"
        );
    }

    /// The ICH9 LPC bridge resets both PIRQ routing banks — `PIRQ[A-D]_ROUT` at
    /// `0x60` and the ICH9-only `PIRQ[E-H]_ROUT` at `0x68` — to `0x80` (routing
    /// disabled), so a guest probing either bank before firmware programs it
    /// reads the documented reset state, not open bus.
    #[test]
    fn ich9_lpc_bridge_resets_both_pirq_banks() {
        let bridge = PcieRootComplex::create_isa_bridge(
            ICH9_LPC_BRIDGE_BDF,
            vendors::INTEL,
            ICH9_LPC_DEVICE_ID,
        );
        assert_eq!(ICH9_LPC_BRIDGE_BDF.acpi_adr(), 0x001F_0000);
        assert_eq!(bridge.read_u8(cfg::SUBCLASS), 0x01); // ISA bridge
        for i in 0..4 {
            assert_eq!(bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE + i), 0x80);
            assert_eq!(bridge.read_u8(PIRQ_EH_ROUTE_CONFIG_BASE + i), 0x80);
        }
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

    /// The device-identity registers are read-only to a guest: a guest write
    /// (any width) leaves Vendor/Device/Subsystem/Class IDs and the header
    /// type unchanged, while writable registers (Command, BARs, interrupt
    /// line) still take. This protects the chipset identity the platform
    /// programs from a guest's probe-writes.
    #[test]
    fn guest_writes_cannot_change_the_device_identity() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 4, 0), vendors::RENESAS, XHCI_DEVICE_ID);
        cs.set_class(0x0C, 0x03, 0x30, 0x02);
        cs.set_subsystem(BOARD_SUBSYSTEM_VENDOR_ID, BOARD_SUBSYSTEM_DEVICE_ID);
        cs.set_bar(0, 0xFE90_0000, 0xFFFF_0000);

        // A guest tries to overwrite the whole identity block — ignored.
        cs.guest_write(cfg::VENDOR_ID, 4, 0xDEAD_BEEF); // vendor+device
        cs.guest_write(cfg::CLASS_CODE, 1, 0xFF); // class byte
        cs.guest_write(cfg::SUBSYSTEM_VENDOR_ID, 4, 0x1234_5678);
        cs.guest_write(cfg::REVISION_ID, 1, 0xAB);
        cs.guest_write(cfg::HEADER_TYPE, 1, 0xFF);
        assert_eq!(cs.vendor_id(), vendors::RENESAS);
        assert_eq!(cs.device_id(), XHCI_DEVICE_ID);
        assert_eq!(cs.read_u8(cfg::CLASS_CODE), 0x0C);
        assert_eq!(cs.read_u8(cfg::REVISION_ID), 0x02);
        assert_eq!(
            cs.read_u16(cfg::SUBSYSTEM_VENDOR_ID),
            BOARD_SUBSYSTEM_VENDOR_ID
        );
        assert_eq!(cs.read_u16(cfg::SUBSYSTEM_ID), BOARD_SUBSYSTEM_DEVICE_ID);

        // Writable registers still take: Command (enable bus master/MMIO) and
        // the interrupt line.
        cs.guest_write(cfg::COMMAND, 2, 0x0006);
        assert_eq!(cs.read_u16(cfg::COMMAND), 0x0006);
        cs.guest_write(cfg::INTERRUPT_LINE, 1, 0x0B);
        assert_eq!(cs.read_u8(cfg::INTERRUPT_LINE), 0x0B);

        // BAR sizing still works (the writable bits are the size mask).
        cs.guest_write(cfg::BAR0, 4, 0xFFFF_FFFF);
        assert_eq!(cs.read_u32(cfg::BAR0) & 0xFFFF_0000, 0xFFFF_0000);
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
        // The full dword still holds byte 1 (0x12) — a byte read of 0xCF9 itself
        // is the Reset Control Register, not CONFIG_ADDRESS byte 1
        // (see reset_control_register_is_decoded_at_cf9).
        assert_eq!((io.pio_read(CONFIG_ADDRESS_PORT, 4) >> 8) & 0xFF, 0x12);
    }

    #[test]
    fn reset_control_register_is_decoded_at_cf9() {
        let mut io = PciConfigIo::new(PcieRootComplex::new(0));
        // A 32-bit CONFIG_ADDRESS write must not disturb the RCR or trigger reset.
        io.pio_write(CONFIG_ADDRESS_PORT, 4, 0x8000_1234);
        assert_eq!(io.pio_read(RESET_CONTROL_PORT, 1) & 0xFF, 0x00);
        assert!(!io.take_reset());

        // Arm a hard reset (SYS_RST), then pulse RST_CPU — the canonical reboot.
        io.pio_write(RESET_CONTROL_PORT, 1, 0x02); // SYS_RST, no reboot yet
        assert!(!io.take_reset());
        assert_eq!(io.pio_read(RESET_CONTROL_PORT, 1) & 0xFF, 0x02);
        io.pio_write(RESET_CONTROL_PORT, 1, 0x06); // SYS_RST | RST_CPU -> reboot
        assert!(io.take_reset(), "RST_CPU write latches a reboot request");
        // The latch is one-shot.
        assert!(!io.take_reset());
        // RST_CPU is not stored; only SYS_RST reads back.
        assert_eq!(io.pio_read(RESET_CONTROL_PORT, 1) & 0xFF, 0x02);
        // The CONFIG_ADDRESS dword is untouched by the 0xCF9 byte traffic.
        assert_eq!(io.pio_read(CONFIG_ADDRESS_PORT, 4), 0x8000_1234);
    }

    #[test]
    fn full_reset_bit_reads_back() {
        let mut io = PciConfigIo::new(PcieRootComplex::new(0));
        io.pio_write(RESET_CONTROL_PORT, 1, 0x0E); // FULL_RST | SYS_RST | RST_CPU
        assert!(io.take_reset());
        assert_eq!(io.pio_read(RESET_CONTROL_PORT, 1) & 0xFF, 0x0A);
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
