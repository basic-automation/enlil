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

/// Config-space offset where [`PciConfigSpace::add_power_management_capability`]
/// places the PCI Power Management Capability (the device-specific region,
/// past the standard header).
pub const PM_CAP_OFFSET: u16 = 0x50;
/// [`PM_CAP_OFFSET`] as the byte the capability pointer stores.
const PM_CAP_OFFSET_BYTE: u8 = 0x50;

/// Config-space offset where [`PciConfigSpace::add_msi_capability`] places the
/// MSI capability structure (in the device-specific region, clear of the PM
/// capability at [`PM_CAP_OFFSET`]).
pub const MSI_CAP_OFFSET: u16 = 0x60;

/// Config-space offset of the MSI-X capability structure (12 bytes).
///
/// Placed clear of the MSI capability, which spans at most 14 bytes from
/// [`MSI_CAP_OFFSET`] (`0x60..=0x6D`). See
/// [`PciConfigSpace::add_msix_capability`].
pub const MSIX_CAP_OFFSET: u16 = 0x70;

/// Config-space offset of the PCI Express Capability structure (60 bytes, v2).
///
/// Placed clear of the MSI-X capability at [`MSIX_CAP_OFFSET`] (`0x70..=0x7B`);
/// the v2 structure spans `0x90..=0xCB`, inside the 256-byte config window. See
/// [`PciConfigSpace::add_pci_express_capability`].
pub const PCIE_CAP_OFFSET: u16 = 0x90;

/// PCI Express Capability **Device/Port Type** values (`PCIe` Base spec, the
/// PCI Express Capabilities Register bits 7:4).
pub mod pcie_type {
    /// `PCIe` Endpoint.
    pub const ENDPOINT: u8 = 0x0;
    /// Legacy `PCIe` Endpoint.
    pub const LEGACY_ENDPOINT: u8 = 0x1;
    /// Root Port of a `PCIe` Root Complex.
    pub const ROOT_PORT: u8 = 0x4;
    /// Upstream Port of a `PCIe` Switch.
    pub const UPSTREAM_PORT: u8 = 0x5;
    /// Downstream Port of a `PCIe` Switch.
    pub const DOWNSTREAM_PORT: u8 = 0x6;
    /// `PCIe`-to-PCI/PCI-X Bridge.
    pub const PCIE_TO_PCI_BRIDGE: u8 = 0x7;
    /// Root Complex Integrated Endpoint.
    pub const RC_INTEGRATED_ENDPOINT: u8 = 0x9;
}

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
        // No capabilities list yet: the STATUS "capabilities list" bit stays
        // clear until one is added (add_power_management_capability), so the
        // bit and the null capability pointer can't disagree.
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

    /// Install a **PCI Power Management Capability** (cap ID 0x01) — the
    /// simplest standard capability, present on essentially every real PCI/
    /// `PCIe` function — and point the capabilities list at it. This makes the
    /// STATUS "capabilities list present" bit truthful: a guest that sees the
    /// bit set and walks from the capability pointer finds a real, terminating
    /// list instead of a null one (an asserted-but-empty list is a tell).
    ///
    /// The capability sits at [`PM_CAP_OFFSET`] in the device-specific config
    /// region; its PMCSR power-state field stays guest-writable (D0..D3 is a
    /// legitimate guest operation), the PMC fields are read-only descriptors.
    pub fn add_power_management_capability(&mut self) {
        // Capability ID 0x01 (PM), next-capability pointer 0 (end of list).
        self.write_u8(PM_CAP_OFFSET, 0x01);
        self.write_u8(PM_CAP_OFFSET + 1, 0x00);
        // PMC (Power Management Capabilities): version 3 (PCI PM 1.2), no PME.
        self.write_u16(PM_CAP_OFFSET + 2, 0x0003);
        // PMCSR: power state D0 (0).
        self.write_u16(PM_CAP_OFFSET + 4, 0x0000);
        // Point the capabilities list here and assert the STATUS caps bit.
        self.write_u8(cfg::CAPABILITY_PTR, PM_CAP_OFFSET_BYTE);
        let status = self.read_u16(cfg::STATUS) | 0x0010;
        self.write_u16(cfg::STATUS, status);
    }

    /// Add a 64-bit-capable MSI capability (Capability ID 0x05) and make it the
    /// head of the capabilities list, chaining to whatever was previously the
    /// head. Modern guests prefer MSI over legacy `INTx`, walking the list from
    /// the capability pointer to find it. The capability starts disabled (Message
    /// Control bit 0 clear) with one vector and a zeroed address/data the guest
    /// programs; this pairs with the MSI delivery path.
    ///
    /// The structure (PCI Local Bus spec §6.8.1, 64-bit form) is laid out at
    /// [`MSI_CAP_OFFSET`]: cap id, next ptr, Message Control, Message Address
    /// (lo+hi), Message Data.
    pub fn add_msi_capability(&mut self) {
        // Chain: this capability points at the current list head, then becomes
        // the new head — so it composes with add_power_management_capability in
        // either order without orphaning an entry.
        let prev_head = self.read_u8(cfg::CAPABILITY_PTR);
        self.write_u8(MSI_CAP_OFFSET, 0x05); // Capability ID: MSI
        self.write_u8(MSI_CAP_OFFSET + 1, prev_head); // next-capability pointer
        // Message Control: 64-bit address capable (bit 7); MSI disabled (bit 0
        // clear); Multiple Message Capable = 0 (one vector).
        self.write_u16(MSI_CAP_OFFSET + 2, 0x0080);
        self.write_u32(MSI_CAP_OFFSET + 4, 0x0000_0000); // Message Address (lo)
        self.write_u32(MSI_CAP_OFFSET + 8, 0x0000_0000); // Message Address (hi)
        self.write_u16(MSI_CAP_OFFSET + 12, 0x0000); // Message Data

        // MSI is the new list head; assert the STATUS caps bit.
        let head = u8::try_from(MSI_CAP_OFFSET).unwrap_or(0);
        self.write_u8(cfg::CAPABILITY_PTR, head);
        let status = self.read_u16(cfg::STATUS) | 0x0010;
        self.write_u16(cfg::STATUS, status);
    }

    /// Whether the guest has enabled MSI (Message Control bit 0). Only meaningful
    /// after [`add_msi_capability`](Self::add_msi_capability).
    #[must_use]
    pub fn msi_enabled(&self) -> bool {
        self.read_u16(MSI_CAP_OFFSET + 2) & 0x0001 != 0
    }

    /// The MSI message the guest has programmed into the MSI capability, or
    /// `None` if the capability is absent or MSI is disabled.
    ///
    /// This is the read side of MSI delivery: a device that wants to raise its
    /// interrupt while the guest has enabled MSI builds this `(address, data)`
    /// message and hands it to the interrupt path (the emulated
    /// [`InterruptController::deliver_msi`](crate::interrupt::InterruptController::deliver_msi)
    /// or, on the KVM backend, `KVM_SIGNAL_MSI`) instead of asserting `INTx`.
    /// Both the 32-bit and 64-bit capability layouts are handled — the Message
    /// Data lives at +8 or +12 depending on the 64-bit-capable bit.
    #[must_use]
    pub fn msi_message(&self) -> Option<crate::interrupt::MsiMessage> {
        if self.read_u8(MSI_CAP_OFFSET) != 0x05 || !self.msi_enabled() {
            return None;
        }
        let is_64bit = self.read_u16(MSI_CAP_OFFSET + 2) & 0x0080 != 0;
        let addr_lo = u64::from(self.read_u32(MSI_CAP_OFFSET + 4));
        let (address, data) = if is_64bit {
            let addr_hi = u64::from(self.read_u32(MSI_CAP_OFFSET + 8));
            (
                addr_lo | (addr_hi << 32),
                u32::from(self.read_u16(MSI_CAP_OFFSET + 12)),
            )
        } else {
            (addr_lo, u32::from(self.read_u16(MSI_CAP_OFFSET + 8)))
        };
        Some(crate::interrupt::MsiMessage::new(address, data))
    }

    /// Add an **MSI-X capability** (Capability ID 0x11) at the head of the
    /// capabilities list, chaining to whatever was previously the head.
    ///
    /// MSI-X is what high-vector-count devices (`NVMe`, modern NICs,
    /// `virtio-pci` with many queues) advertise: unlike MSI's single in-config
    /// message, MSI-X keeps a *table* of up to 2048 independent vectors in
    /// device MMIO (a BAR), each separately maskable, plus a Pending Bit Array.
    /// The capability in config space only points at those structures and
    /// carries the global Enable / Function Mask bits.
    ///
    /// `table_size` is the number of vectors (1..=2048); it is stored as
    /// `N - 1` in Message Control bits 10:0 (PCI Local Bus spec §6.8.2). The
    /// table and PBA each live at `*_offset` (8-byte aligned) within BAR
    /// `*_bir`. The capability starts disabled (Enable bit 15 clear) and
    /// function-unmasked (bit 14 clear); the guest sets Enable once it has
    /// programmed the table. Pairs with [`MsixTable`] for the BAR-backed table.
    pub fn add_msix_capability(
        &mut self,
        table_size: u16,
        table_bir: u8,
        table_offset: u32,
        pba_bir: u8,
        pba_offset: u32,
    ) {
        // Chain onto the current list head, then become the new head — composes
        // with the PM/MSI caps in any order without orphaning an entry.
        let prev_head = self.read_u8(cfg::CAPABILITY_PTR);
        self.write_u8(MSIX_CAP_OFFSET, 0x11); // Capability ID: MSI-X
        self.write_u8(MSIX_CAP_OFFSET + 1, prev_head); // next-capability pointer
        // Message Control: Table Size = N-1 in bits 10:0; Enable (bit 15) and
        // Function Mask (bit 14) clear out of the box.
        let encoded_size = table_size.saturating_sub(1) & 0x07FF;
        self.write_u16(MSIX_CAP_OFFSET + 2, encoded_size);
        // Table Offset / Table BIR: BIR in bits 2:0, 8-byte-aligned offset above.
        self.write_u32(
            MSIX_CAP_OFFSET + 4,
            (table_offset & !0x7) | u32::from(table_bir & 0x7),
        );
        // PBA Offset / PBA BIR: same encoding.
        self.write_u32(
            MSIX_CAP_OFFSET + 8,
            (pba_offset & !0x7) | u32::from(pba_bir & 0x7),
        );

        // MSI-X is the new list head; assert the STATUS caps bit.
        let head = u8::try_from(MSIX_CAP_OFFSET).unwrap_or(0);
        self.write_u8(cfg::CAPABILITY_PTR, head);
        let status = self.read_u16(cfg::STATUS) | 0x0010;
        self.write_u16(cfg::STATUS, status);
    }

    /// Whether the guest has enabled MSI-X (Message Control bit 15). Only
    /// meaningful after [`add_msix_capability`](Self::add_msix_capability).
    #[must_use]
    pub fn msix_enabled(&self) -> bool {
        self.read_u16(MSIX_CAP_OFFSET + 2) & 0x8000 != 0
    }

    /// Whether the guest has set the MSI-X **Function Mask** (Message Control
    /// bit 14) — a global mask over every vector regardless of per-entry masks.
    #[must_use]
    pub fn msix_function_masked(&self) -> bool {
        self.read_u16(MSIX_CAP_OFFSET + 2) & 0x4000 != 0
    }

    /// The configured MSI-X table size (number of vectors) — Message Control
    /// bits 10:0 decoded from the stored `N - 1`. Returns 0 if MSI-X is absent.
    #[must_use]
    pub fn msix_table_size(&self) -> u16 {
        if self.read_u8(MSIX_CAP_OFFSET) != 0x11 {
            return 0;
        }
        (self.read_u16(MSIX_CAP_OFFSET + 2) & 0x07FF) + 1
    }

    /// Add a version-2 **PCI Express Capability** (Capability ID 0x10) at the
    /// head of the capabilities list, chaining to whatever was previously the
    /// head.
    ///
    /// This is the structure that makes a function a *`PCIe`* function rather
    /// than a plain PCI one: every native `PCIe` device exposes it, and a guest
    /// that finds an ECAM-reachable device with no PCI Express Capability has
    /// caught a tell. `device_port_type` is one of [`pcie_type`]. The capability
    /// advertises a modest x1 / 2.5 GT/s link and 256-byte max payload, with the
    /// guest-writable Device/Link Control registers left at their reset values.
    pub fn add_pci_express_capability(&mut self, device_port_type: u8) {
        let prev_head = self.read_u8(cfg::CAPABILITY_PTR);
        self.write_u8(PCIE_CAP_OFFSET, 0x10); // Capability ID: PCI Express
        self.write_u8(PCIE_CAP_OFFSET + 1, prev_head); // next-capability pointer
        // PCI Express Capabilities Register: Capability Version = 2 (bits 3:0),
        // Device/Port Type (bits 7:4); Slot Implemented and Interrupt Message
        // Number stay 0 (an endpoint has no slot).
        let caps_reg = 0x0002 | (u16::from(device_port_type & 0xF) << 4);
        self.write_u16(PCIE_CAP_OFFSET + 2, caps_reg);
        // Device Capabilities: Max_Payload_Size Supported = 256 bytes (001b).
        self.write_u32(PCIE_CAP_OFFSET + 4, 0x0000_0001);
        // Device Control / Status reset to 0.
        self.write_u16(PCIE_CAP_OFFSET + 8, 0x0000);
        self.write_u16(PCIE_CAP_OFFSET + 10, 0x0000);
        // Link Capabilities: Max Link Speed = 2.5 GT/s (1), Max Link Width = x1
        // (1 << 4) -> 0x11.
        self.write_u32(PCIE_CAP_OFFSET + 12, 0x0000_0011);
        // Link Control reset 0; Link Status: Current Link Speed 1, Width x1.
        self.write_u16(PCIE_CAP_OFFSET + 16, 0x0000);
        self.write_u16(PCIE_CAP_OFFSET + 18, 0x0011);
        // v2 registers (Device/Link Capabilities/Control/Status 2) reset to 0;
        // config space was zero-initialised, so they are already correct. The
        // structure occupies PCIE_CAP_OFFSET..=+0x3B.

        // PCI Express is the new list head; assert the STATUS caps bit.
        let head = u8::try_from(PCIE_CAP_OFFSET).unwrap_or(0);
        self.write_u8(cfg::CAPABILITY_PTR, head);
        let status = self.read_u16(cfg::STATUS) | 0x0010;
        self.write_u16(cfg::STATUS, status);
    }

    /// The PCI Express Capability **version** (PCI Express Capabilities Register
    /// bits 3:0), or 0 if the capability is absent.
    #[must_use]
    pub fn pci_express_version(&self) -> u8 {
        if self.read_u8(PCIE_CAP_OFFSET) != 0x10 {
            return 0;
        }
        u8_of(usize::from(self.read_u16(PCIE_CAP_OFFSET + 2) & 0x000F))
    }

    /// The PCI Express **Device/Port Type** (Capabilities Register bits 7:4) —
    /// one of [`pcie_type`]. Returns 0 (Endpoint) if the capability is absent;
    /// pair with [`pci_express_version`](Self::pci_express_version) to tell
    /// "absent" from "endpoint".
    #[must_use]
    pub fn pci_express_device_type(&self) -> u8 {
        u8_of(usize::from(
            (self.read_u16(PCIE_CAP_OFFSET + 2) >> 4) & 0x000F,
        ))
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
        // If this write overlaps an installed MSI-X capability, snapshot its
        // read-only fields — Table Size (Message Control bits 10:0) and the
        // Table/PBA Offset+BIR dwords describe the fixed hardware layout. Only
        // the Enable (bit 15) and Function Mask (bit 14) control bits are
        // guest-writable; a driver that could resize the table or relocate it
        // into the wrong BAR would be both a correctness and a stealth bug.
        let msix_ro = (offset < MSIX_CAP_OFFSET + 12)
            && (offset + u16::from(width) > MSIX_CAP_OFFSET)
            && self.read_u8(MSIX_CAP_OFFSET) == 0x11;
        let saved = msix_ro.then(|| {
            (
                self.read_u16(MSIX_CAP_OFFSET + 2) & 0x07FF, // Table Size (RO)
                self.read_u32(MSIX_CAP_OFFSET + 4),          // Table Offset/BIR (RO)
                self.read_u32(MSIX_CAP_OFFSET + 8),          // PBA Offset/BIR (RO)
            )
        });

        // Likewise the MSI Message Control register (MSI_CAP_OFFSET + 2, 16-bit)
        // carries read-only capability descriptors a guest must not change:
        // Multiple Message Capable (bits 3:1), 64-bit Address Capable (bit 7),
        // Per-Vector Masking Capable (bit 8), and the reserved bits (15:9). Only
        // MSI Enable (bit 0) and Multiple Message Enable (bits 6:4) are guest-
        // writable (PCI Local Bus spec §6.8.1). This matters for stealth *and*
        // correctness: msi_message() reads the 64-bit-capable bit to locate the
        // Message Data word, so a guest clearing it would misdirect the decode,
        // and inflating Multiple Message Capable spoofs a vector count the device
        // doesn't have.
        let msi_ctrl_ro = (offset < MSI_CAP_OFFSET + 4)
            && (offset + u16::from(width) > MSI_CAP_OFFSET + 2)
            && self.read_u8(MSI_CAP_OFFSET) == 0x05;
        let saved_msi_ctrl = msi_ctrl_ro.then(|| self.read_u16(MSI_CAP_OFFSET + 2));

        // The PM Capabilities register (PMC, PM_CAP_OFFSET + 2) is a wholly
        // read-only 16-bit descriptor — PM spec version, PME support, the D1/D2
        // and AUX-current fields are fixed hardware properties (PCI PM spec
        // §3.2.3). Only the PMCSR power-state register (+4) is guest-writable
        // (D0..D3 transitions), so it is left alone. Snapshot the whole PMC.
        let pmc_ro = (offset < PM_CAP_OFFSET + 4)
            && (offset + u16::from(width) > PM_CAP_OFFSET + 2)
            && self.read_u8(PM_CAP_OFFSET) == 0x01;
        let saved_pmc = pmc_ro.then(|| self.read_u16(PM_CAP_OFFSET + 2));

        // The PCI Status register (0x06) has no plain read-write bits: it is
        // read-only except for the RW1C error bits — Master Data Parity Error
        // [8], Signaled Target Abort [11], Received Target Abort [12], Received
        // Master Abort [13], Signaled System Error [14], Detected Parity Error
        // [15] (PCI Local Bus spec §6.2.3). The read-only bits include the
        // Capabilities List bit [4], which the capability-header protection
        // below itself reads — a guest that set/cleared it could corrupt that
        // logic and its own cap-list walk, besides being a transparency tell.
        // Snapshot the register and which bytes the write covers so the generic
        // byte loop can be undone: RO bits restored, W1C bits cleared only where
        // the guest wrote a 1.
        let status_touched = offset < cfg::STATUS + 2 && offset + u16::from(width) > cfg::STATUS;
        let saved_status = status_touched.then(|| {
            let mut covered = 0u16;
            if offset <= cfg::STATUS {
                covered |= 0x00FF;
            }
            if offset + u16::from(width) > cfg::STATUS + 1 {
                covered |= 0xFF00;
            }
            (self.read_u16(cfg::STATUS), covered)
        });

        // The PCI Command register (0x04) is only partly writable. On a PCIe
        // function the legacy bits Special Cycles [3], Memory Write & Invalidate
        // [4], VGA Palette Snoop [5], the reserved bit [7] and Fast Back-to-Back
        // Enable [9] are hardwired to 0, and [15:11] are reserved — only I/O [0],
        // Memory [1], Bus Master [2], Parity Error Response [6], SERR# [8] and
        // Interrupt Disable [10] are guest-writable (PCI Local Bus spec §6.2.2,
        // PCIe Base §7.5.1.1). Storing the raw value let a guest set hardwired-0
        // bits and read them back — a transparency tell. Snapshot so the generic
        // loop can be re-masked to the writable bits in the covered bytes.
        let command_touched =
            offset < cfg::COMMAND + 2 && offset + u16::from(width) > cfg::COMMAND;
        let saved_command = command_touched.then(|| {
            let mut covered = 0u16;
            if offset <= cfg::COMMAND {
                covered |= 0x00FF;
            }
            if offset + u16::from(width) > cfg::COMMAND + 1 {
                covered |= 0xFF00;
            }
            (self.read_u16(cfg::COMMAND), covered)
        });

        // The PCI Express Capabilities register (PCIE_CAP_OFFSET + 2) is also a
        // wholly read-only descriptor: Capability Version (bits 3:0), Device/Port
        // Type (7:4), Slot Implemented (8), and Interrupt Message Number (13:9)
        // are all fixed (PCIe Base spec §7.5.3.2). pci_express_version() and
        // pci_express_device_type() read it, so a guest must not be able to
        // masquerade as a different version or port type.
        let pcie_cap_ro = (offset < PCIE_CAP_OFFSET + 4)
            && (offset + u16::from(width) > PCIE_CAP_OFFSET + 2)
            && self.read_u8(PCIE_CAP_OFFSET) == 0x10;
        let saved_pcie_cap = pcie_cap_ro.then(|| self.read_u16(PCIE_CAP_OFFSET + 2));

        // Everything else: write byte by byte, skipping read-only bytes — both
        // the fixed header registers and each capability's read-only structural
        // header (ID + next-pointer).
        let bytes = value.to_le_bytes();
        for (i, &b) in bytes.iter().enumerate().take(usize::from(width)) {
            let off = offset + u16::try_from(i).unwrap_or(0);
            if !Self::byte_is_read_only(off) && !self.capability_header_byte_is_read_only(off) {
                self.write_u8(off, b);
            }
        }

        // Restore the MSI-X read-only fields the generic write may have touched.
        if let Some((table_size, table_off, pba_off)) = saved {
            let ctrl = (self.read_u16(MSIX_CAP_OFFSET + 2) & !0x07FF) | table_size;
            self.write_u16(MSIX_CAP_OFFSET + 2, ctrl);
            self.write_u32(MSIX_CAP_OFFSET + 4, table_off);
            self.write_u32(MSIX_CAP_OFFSET + 8, pba_off);
        }

        // Restore the MSI Message Control read-only bits, keeping only the
        // guest-writable Enable (bit 0) and Multiple Message Enable (bits 6:4).
        if let Some(saved_ctrl) = saved_msi_ctrl {
            const MSI_CTRL_RW: u16 = 0x0071; // bit 0 (Enable) | bits 6:4 (MME)
            let written = self.read_u16(MSI_CAP_OFFSET + 2);
            self.write_u16(
                MSI_CAP_OFFSET + 2,
                (written & MSI_CTRL_RW) | (saved_ctrl & !MSI_CTRL_RW),
            );
        }

        // Restore the wholly-read-only PM PMC and PCIe Capabilities registers.
        if let Some(pmc) = saved_pmc {
            self.write_u16(PM_CAP_OFFSET + 2, pmc);
        }
        if let Some(pcie_cap) = saved_pcie_cap {
            self.write_u16(PCIE_CAP_OFFSET + 2, pcie_cap);
        }

        // Restore the Status register: keep every read-only bit at its prior
        // value and only clear the RW1C error bits the guest wrote a 1 to within
        // the bytes the write actually covered.
        if let Some((old_status, covered)) = saved_status {
            // RW1C error bits: Master Data Parity Error [8], Signaled Target
            // Abort [11], Received Target Abort [12], Received Master Abort [13],
            // Signaled System Error [14], Detected Parity Error [15].
            const STATUS_W1C_MASK: u16 = 0xF900;
            let attempted = self.read_u16(cfg::STATUS);
            let w1c_clear = attempted & STATUS_W1C_MASK & covered;
            self.write_u16(cfg::STATUS, old_status & !w1c_clear);
        }

        // Restore the Command register: keep the guest's value only in the
        // writable bits of the covered bytes; force the hardwired-0 / reserved
        // bits back to their prior value (0).
        if let Some((old_command, covered)) = saved_command {
            const COMMAND_WRITABLE_MASK: u16 = 0x0547; // bits 0,1,2,6,8,10
            let writable = COMMAND_WRITABLE_MASK & covered;
            let attempted = self.read_u16(cfg::COMMAND);
            self.write_u16(cfg::COMMAND, (old_command & !writable) | (attempted & writable));
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

    /// Whether `offset` lands on a capability's read-only structural header —
    /// its Capability ID byte (`+0`) or next-capability pointer (`+1`). Those
    /// two bytes of every standard capability are fixed: a guest that rewrote
    /// them would corrupt the very capabilities list its own driver walks (and a
    /// broken list is itself anomalous). The control/data registers *inside* a
    /// capability stay writable (and have their own read-only-bit handling).
    ///
    /// Walks the list from the capabilities pointer, bounded against a malformed
    /// or looping list; a no-op when the STATUS capabilities bit is clear.
    #[must_use]
    fn capability_header_byte_is_read_only(&self, offset: u16) -> bool {
        if self.read_u16(cfg::STATUS) & 0x0010 == 0 {
            return false; // no capabilities list advertised
        }
        let mut ptr = self.read_u8(cfg::CAPABILITY_PTR);
        // At most ~48 capabilities fit in the 0x40..0x100 device region; the
        // bound also guarantees termination on a looping next-pointer chain.
        for _ in 0..48 {
            if ptr < 0x40 {
                break; // 0 terminates the list; anything below 0x40 is invalid
            }
            let cap = u16::from(ptr);
            if offset == cap || offset == cap + 1 {
                return true;
            }
            ptr = self.read_u8(cap + 1); // follow the next-capability pointer
        }
        false
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

/// Size in bytes of one MSI-X table entry (PCI Local Bus spec §6.8.2.1).
pub const MSIX_ENTRY_SIZE: u32 = 16;

/// One 16-byte MSI-X table entry as the guest sees it in the table BAR: a full
/// 64-bit Message Address, a 32-bit Message Data, and a Vector Control dword
/// whose bit 0 is the per-vector Mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsixEntry {
    /// Message Address, lower 32 bits (table offset +0).
    pub addr_lo: u32,
    /// Message Address, upper 32 bits (table offset +4).
    pub addr_hi: u32,
    /// Message Data (table offset +8).
    pub data: u32,
    /// Vector Control (table offset +12); bit 0 = Mask, the rest reserved.
    pub vector_control: u32,
}

impl MsixEntry {
    /// A reset entry: address/data zero, **masked** (Vector Control bit 0 set).
    /// Real MSI-X tables come up masked so a half-programmed vector can't fire.
    const RESET: Self = Self {
        addr_lo: 0,
        addr_hi: 0,
        data: 0,
        vector_control: 0x1,
    };

    /// Whether this vector is masked by its per-entry Mask bit.
    #[must_use]
    pub const fn masked(&self) -> bool {
        self.vector_control & 0x1 != 0
    }

    /// The full 64-bit message address.
    #[must_use]
    pub const fn address(&self) -> u64 {
        ((self.addr_hi as u64) << 32) | self.addr_lo as u64
    }
}

/// A BAR-backed **MSI-X table + Pending Bit Array** — the structures the MSI-X
/// capability (see [`PciConfigSpace::add_msix_capability`]) points at.
///
/// The table holds one [`MsixEntry`] per vector; the guest programs each entry's
/// address/data and toggles its Mask bit through MMIO into the table BAR. When a
/// device wants to raise an interrupt it calls [`signal`](Self::signal): if the
/// vector is deliverable the (address, data) message is returned for injection;
/// if it is masked (per-entry Mask, the capability Function Mask, or MSI-X
/// disabled) the request is recorded in the PBA instead, and a later unmask
/// replays it via [`take_pending`](Self::take_pending). This mirrors how a real
/// MSI-X function defers a masked interrupt rather than dropping it.
#[derive(Debug, Clone)]
pub struct MsixTable {
    entries: Vec<MsixEntry>,
    /// One pending bit per vector (the PBA, exposed to the guest as read-only
    /// MMIO qwords in the PBA BAR region).
    pending: Vec<bool>,
}

impl MsixTable {
    /// Create a table of `num_vectors` (clamped to 1..=2048) reset/masked entries.
    #[must_use]
    pub fn new(num_vectors: u16) -> Self {
        let n = (num_vectors.clamp(1, 2048)) as usize;
        Self {
            entries: vec![MsixEntry::RESET; n],
            pending: vec![false; n],
        }
    }

    /// Number of vectors in the table (always >= 1).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Always false — a table has at least one vector; present for lint parity.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Borrow a vector's entry, if `vec` is in range.
    #[must_use]
    pub fn entry(&self, vec: usize) -> Option<&MsixEntry> {
        self.entries.get(vec)
    }

    /// Whether vector `vec` is masked by its per-entry Mask bit (out-of-range
    /// vectors read as masked).
    #[must_use]
    pub fn is_masked(&self, vec: usize) -> bool {
        self.entries.get(vec).is_none_or(MsixEntry::masked)
    }

    /// Whether vector `vec` has a pending (deferred-because-masked) interrupt.
    #[must_use]
    pub fn is_pending(&self, vec: usize) -> bool {
        self.pending.get(vec).copied().unwrap_or(false)
    }

    /// Read a 4-byte-aligned dword from the **table** MMIO region at `offset`
    /// (bytes from the start of the table BAR window). Out-of-range or
    /// misaligned reads return all-ones, as a real controller's reserved space
    /// would.
    #[must_use]
    pub fn read_table_u32(&self, offset: u32) -> u32 {
        if !offset.is_multiple_of(4) {
            return 0xFFFF_FFFF;
        }
        let vec = (offset / MSIX_ENTRY_SIZE) as usize;
        let Some(e) = self.entries.get(vec) else {
            return 0xFFFF_FFFF;
        };
        match offset % MSIX_ENTRY_SIZE {
            0 => e.addr_lo,
            4 => e.addr_hi,
            8 => e.data,
            _ => e.vector_control & 0x1, // reserved bits read 0
        }
    }

    /// Write a 4-byte-aligned dword into the **table** MMIO region at `offset`.
    /// Address/Data fields take as written; Vector Control keeps only the Mask
    /// bit (reserved bits are RAZ). Misaligned/out-of-range writes are dropped.
    pub fn write_table_u32(&mut self, offset: u32, value: u32) {
        if !offset.is_multiple_of(4) {
            return;
        }
        let vec = (offset / MSIX_ENTRY_SIZE) as usize;
        let Some(e) = self.entries.get_mut(vec) else {
            return;
        };
        match offset % MSIX_ENTRY_SIZE {
            0 => e.addr_lo = value,
            4 => e.addr_hi = value,
            8 => e.data = value,
            _ => e.vector_control = value & 0x1,
        }
    }

    /// Read a PBA qword: bit `k` of qword `q` is the pending bit for vector
    /// `q * 64 + k`. The PBA is read-only MMIO to the guest.
    #[must_use]
    pub fn read_pba_u64(&self, qword_index: usize) -> u64 {
        let base = qword_index * 64;
        let mut bits = 0u64;
        for k in 0..64 {
            if self.pending.get(base + k).copied().unwrap_or(false) {
                bits |= 1u64 << k;
            }
        }
        bits
    }

    /// Attempt to raise vector `vec`. `globally_masked` folds in MSI-X-disabled
    /// and the capability Function Mask. Returns the `(address, data)` message
    /// to inject when delivery is allowed; otherwise sets the vector's PBA bit
    /// and returns `None` so the interrupt is replayed on a later unmask.
    pub fn signal(&mut self, vec: usize, globally_masked: bool) -> Option<(u64, u32)> {
        let entry = *self.entries.get(vec)?;
        if globally_masked || entry.masked() {
            if let Some(p) = self.pending.get_mut(vec) {
                *p = true;
            }
            None
        } else {
            if let Some(p) = self.pending.get_mut(vec) {
                *p = false;
            }
            Some((entry.address(), entry.data))
        }
    }

    /// Drain every vector whose pending bit is set and is now deliverable
    /// (per-entry unmasked and not globally masked), clearing each PBA bit and
    /// returning its message. Call after a table unmask, a Function-Mask clear,
    /// or an MSI-X enable to flush deferred interrupts in vector order.
    pub fn take_pending(&mut self, globally_masked: bool) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        if globally_masked {
            return out;
        }
        for vec in 0..self.entries.len() {
            if self.pending[vec] && !self.entries[vec].masked() {
                self.pending[vec] = false;
                out.push((self.entries[vec].address(), self.entries[vec].data));
            }
        }
        out
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
        dev.add_power_management_capability();
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
        dev.add_power_management_capability();
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
        // A real Renesas uPD720201 xHCI enumerates Power Management, MSI, MSI-X,
        // and a PCI Express (Endpoint) capability; advertise the same list so a
        // guest sees a faithful discrete USB 3.0 controller rather than a bare
        // PCI function with only legacy INTx (which modern xHCI drivers flag).
        // The MSI-X table and PBA live in BAR0 (BIR 0) at the fixed offsets the
        // controller's register window decodes, so a guest that programs the
        // table through MMIO reaches the real [`MsixTable`].
        dev.add_power_management_capability();
        dev.add_msi_capability();
        dev.add_msix_capability(
            crate::usb::XHCI_MSIX_VECTORS,
            0,
            crate::usb::MSIX_TABLE_BAR_OFFSET,
            0,
            crate::usb::MSIX_PBA_BAR_OFFSET,
        );
        dev.add_pci_express_capability(pcie_type::ENDPOINT);
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
        dev.add_power_management_capability();
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

    /// A function with a Power Management capability presents a consistent,
    /// walkable capability list: STATUS asserts "capabilities present", the
    /// pointer leads to a PM cap (ID 0x01), and the cap terminates the list.
    /// A bare device asserts no list and has a null pointer (no asserted-but-
    /// empty list).
    #[test]
    fn power_management_capability_makes_a_walkable_list() {
        // Bare device: no caps bit, null pointer.
        let bare = PciConfigSpace::new(PciBdf::new(0, 5, 0), 0x1234, 0x5678);
        assert_eq!(bare.read_u16(cfg::STATUS) & 0x0010, 0, "no caps advertised");
        assert_eq!(bare.read_u8(cfg::CAPABILITY_PTR), 0);

        // The Q35 MCH (and the other session functions) carry the PM cap.
        let mch = PcieRootComplex::create_q35_host_bridge(0xB000_0000);
        assert_ne!(mch.read_u16(cfg::STATUS) & 0x0010, 0, "caps present");
        let ptr = mch.read_u8(cfg::CAPABILITY_PTR);
        assert_eq!(u16::from(ptr), PM_CAP_OFFSET);
        // Walk it: cap ID 0x01 (PM), next pointer 0 (end).
        assert_eq!(mch.read_u8(PM_CAP_OFFSET), 0x01);
        assert_eq!(mch.read_u8(PM_CAP_OFFSET + 1), 0x00);
        // PMCSR power state defaults to D0.
        assert_eq!(mch.read_u16(PM_CAP_OFFSET + 4) & 0x3, 0);
    }

    #[test]
    fn msi_capability_is_walkable_and_chains_with_pm() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 6, 0), 0x8086, 0x1234);
        cs.add_power_management_capability();
        cs.add_msi_capability();

        // Caps bit set; the list head is now MSI, chaining to PM, then ending.
        assert_ne!(cs.read_u16(cfg::STATUS) & 0x0010, 0);
        let head = cs.read_u8(cfg::CAPABILITY_PTR);
        assert_eq!(u16::from(head), MSI_CAP_OFFSET);
        assert_eq!(cs.read_u8(MSI_CAP_OFFSET), 0x05, "MSI cap id");
        // MSI is 64-bit capable and disabled out of the box.
        assert_ne!(
            cs.read_u16(MSI_CAP_OFFSET + 2) & 0x0080,
            0,
            "64-bit capable"
        );
        assert!(!cs.msi_enabled());
        // Next pointer -> PM -> null.
        let next = cs.read_u8(MSI_CAP_OFFSET + 1);
        assert_eq!(u16::from(next), PM_CAP_OFFSET);
        assert_eq!(cs.read_u8(PM_CAP_OFFSET), 0x01);
        assert_eq!(cs.read_u8(PM_CAP_OFFSET + 1), 0x00, "list terminates");
    }

    #[test]
    fn guest_writes_cannot_change_msi_message_control_readonly_bits() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 6, 0), 0x8086, 0x1234);
        cs.add_msi_capability(); // Message Control = 0x0080 (64-bit cap, MMC=0, disabled)

        // A driver probe-writes all-ones to Message Control. Only Enable (bit 0)
        // and Multiple Message Enable (bits 6:4) may take; the read-only
        // descriptors must hold: Multiple Message Capable (bits 3:1) stays 0,
        // the 64-bit-capable bit (7) stays 1, Per-Vector-Masking (8) stays 0,
        // and the reserved bits (15:9) stay 0.
        cs.guest_write(MSI_CAP_OFFSET + 2, 2, 0xFFFF);
        let ctrl = cs.read_u16(MSI_CAP_OFFSET + 2);
        assert_eq!(ctrl & 0x0001, 0x0001, "Enable (RW) took");
        assert_eq!(ctrl & 0x0070, 0x0070, "Multiple Message Enable (RW) took");
        assert_eq!(ctrl & 0x000E, 0, "Multiple Message Capable (RO) held at 0");
        assert_eq!(ctrl & 0x0080, 0x0080, "64-bit Address Capable (RO) held");
        assert_eq!(
            ctrl & 0xFF00,
            0,
            "PVM-capable + reserved bits (RO) held at 0"
        );

        // The 64-bit decode the message path relies on is therefore unchanged:
        // a guest cannot clear the 64-bit-capable bit to misdirect msi_message().
        cs.guest_write(MSI_CAP_OFFSET + 2, 2, 0x0001); // try enable-only, clearing bit 7
        assert_ne!(
            cs.read_u16(MSI_CAP_OFFSET + 2) & 0x0080,
            0,
            "64-bit-capable bit cannot be cleared by the guest"
        );
    }

    #[test]
    fn guest_writes_cannot_corrupt_the_capability_list_structure() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 6, 0), 0x8086, 0x1234);
        cs.add_power_management_capability();
        cs.add_msi_capability(); // head -> MSI(0x60) -> PM(0x50) -> null

        let head = cs.read_u8(cfg::CAPABILITY_PTR);
        let msi_next = cs.read_u8(MSI_CAP_OFFSET + 1);

        // A guest tries to scribble over both capability headers (ID + next ptr).
        cs.guest_write(MSI_CAP_OFFSET, 2, 0xFFFF); // MSI cap ID + next ptr
        cs.guest_write(PM_CAP_OFFSET, 2, 0xFFFF); // PM cap ID + next ptr

        assert_eq!(cs.read_u8(MSI_CAP_OFFSET), 0x05, "MSI cap ID held");
        assert_eq!(
            cs.read_u8(MSI_CAP_OFFSET + 1),
            msi_next,
            "MSI next ptr held"
        );
        assert_eq!(cs.read_u8(PM_CAP_OFFSET), 0x01, "PM cap ID held");
        assert_eq!(
            cs.read_u8(PM_CAP_OFFSET + 1),
            0x00,
            "PM list still terminates"
        );
        // The list is still walkable end to end.
        assert_eq!(u16::from(head), MSI_CAP_OFFSET);
        assert_eq!(u16::from(cs.read_u8(MSI_CAP_OFFSET + 1)), PM_CAP_OFFSET);
    }

    #[test]
    fn guest_writes_to_the_status_register_preserve_ro_bits_and_w1c_errors() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 6, 0), 0x8086, 0x1234);
        cs.add_msi_capability(); // sets the Capabilities List bit [4] in STATUS
        // Seed two RW1C error bits as if hardware had latched them: Received
        // Master Abort [13] and Signaled System Error [14].
        let seeded = cs.read_u16(cfg::STATUS) | 0x6000;
        cs.write_u16(cfg::STATUS, seeded);
        assert_ne!(cs.read_u16(cfg::STATUS) & 0x0010, 0, "caps list bit set");

        // A guest writes all-ones to the whole Status register.
        cs.guest_write(cfg::STATUS, 2, 0xFFFF);

        let after = cs.read_u16(cfg::STATUS);
        // The read-only Capabilities List bit [4] is unchanged (still set) — a
        // guest cannot clear it (which would also break the cap-header guard).
        assert_ne!(after & 0x0010, 0, "Capabilities List bit is read-only");
        // The two seeded RW1C error bits were cleared by the write-1.
        assert_eq!(after & 0x6000, 0, "RW1C error bits cleared by write-1");
        // A guest cannot SET a read-only bit it had no business setting (e.g.
        // 66 MHz Capable [5], not advertised by this model): it stays 0.
        assert_eq!(after & 0x0020, 0, "guest cannot set the read-only 66MHz bit");
    }

    #[test]
    fn guest_command_register_writable_bits_take_but_hardwired_bits_stay_zero() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 6, 0), 0x8086, 0x1234);
        // A guest writes all-ones to the Command register.
        cs.guest_write(cfg::COMMAND, 2, 0xFFFF);
        let cmd = cs.read_u16(cfg::COMMAND);
        // The writable bits (I/O [0], Mem [1], Bus Master [2], PERR [6],
        // SERR# [8], Interrupt Disable [10]) took.
        assert_eq!(cmd & 0x0547, 0x0547, "writable Command bits took");
        // The hardwired-0 (PCIe) / reserved bits read back 0.
        assert_eq!(cmd & !0x0547, 0, "hardwired-0 and reserved Command bits stay 0");

        // A byte write to the high Command byte must not disturb the low byte's
        // already-set writable bits.
        cs.guest_write(cfg::COMMAND + 1, 1, 0xFF);
        assert_eq!(
            cs.read_u16(cfg::COMMAND) & 0x07,
            0x07,
            "low-byte writable bits survive a high-byte write"
        );
    }

    #[test]
    fn guest_writes_cannot_change_pm_or_pcie_capability_descriptors() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 6, 0), 0x8086, 0x1234);
        cs.add_power_management_capability(); // PMC = 0x0003 (PM v1.2, no PME)
        cs.add_pci_express_capability(pcie_type::ROOT_PORT);

        let pmc0 = cs.read_u16(PM_CAP_OFFSET + 2);
        let pcie0 = cs.read_u16(PCIE_CAP_OFFSET + 2);
        assert_eq!(pmc0, 0x0003, "PMC default");
        assert_eq!(cs.pci_express_version(), 2);
        assert_eq!(cs.pci_express_device_type(), pcie_type::ROOT_PORT);

        // A guest probe-writes all-ones to both read-only descriptor registers.
        cs.guest_write(PM_CAP_OFFSET + 2, 2, 0xFFFF);
        cs.guest_write(PCIE_CAP_OFFSET + 2, 2, 0xFFFF);

        assert_eq!(cs.read_u16(PM_CAP_OFFSET + 2), pmc0, "PMC held read-only");
        assert_eq!(
            cs.read_u16(PCIE_CAP_OFFSET + 2),
            pcie0,
            "PCIe Capabilities register held read-only"
        );
        // The version and port type the topology walk reads are unchanged: a
        // guest cannot masquerade a root port as an endpoint.
        assert_eq!(cs.pci_express_version(), 2);
        assert_eq!(cs.pci_express_device_type(), pcie_type::ROOT_PORT);

        // The PMCSR power-state register stays guest-writable (D3hot).
        cs.guest_write(PM_CAP_OFFSET + 4, 2, 0x0003);
        assert_eq!(
            cs.read_u16(PM_CAP_OFFSET + 4) & 0x3,
            0x3,
            "D-state writable"
        );
    }

    #[test]
    fn guest_programs_msi_address_data_and_enable() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 7, 0), 0x8086, 0x1234);
        cs.add_msi_capability();
        assert_eq!(u16::from(cs.read_u8(cfg::CAPABILITY_PTR)), MSI_CAP_OFFSET);
        assert_eq!(
            cs.read_u8(MSI_CAP_OFFSET + 1),
            0,
            "lone cap terminates the list"
        );

        // Guest programs the MSI address/data and sets the enable bit.
        cs.guest_write_u32(MSI_CAP_OFFSET + 4, 0xFEE0_0000);
        cs.guest_write_u32(MSI_CAP_OFFSET + 8, 0x0000_0000);
        cs.guest_write(MSI_CAP_OFFSET + 12, 2, 0x0041);
        cs.guest_write(MSI_CAP_OFFSET + 2, 2, 0x0081); // 64-bit + enable
        assert!(cs.msi_enabled());
        assert_eq!(cs.read_u32(MSI_CAP_OFFSET + 4), 0xFEE0_0000);
        assert_eq!(cs.read_u16(MSI_CAP_OFFSET + 12), 0x0041);
    }

    /// An MSI-X capability is a walkable cap-ID-0x11 entry that chains ahead of
    /// the PM cap, decodes its Table Size and Table/PBA Offset+BIR exactly as
    /// programmed, and starts disabled + function-unmasked.
    #[test]
    fn msix_capability_is_walkable_and_decodes_table_geometry() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 8, 0), 0x1AF4, 0x1041);
        cs.add_power_management_capability();
        // 8 vectors; table in BAR1 @ 0x2000, PBA in BAR1 @ 0x3000.
        cs.add_msix_capability(8, 1, 0x2000, 1, 0x3000);

        assert_ne!(cs.read_u16(cfg::STATUS) & 0x0010, 0, "caps bit");
        let head = cs.read_u8(cfg::CAPABILITY_PTR);
        assert_eq!(u16::from(head), MSIX_CAP_OFFSET);
        assert_eq!(cs.read_u8(MSIX_CAP_OFFSET), 0x11, "MSI-X cap id");
        // Chains to PM, which terminates.
        assert_eq!(u16::from(cs.read_u8(MSIX_CAP_OFFSET + 1)), PM_CAP_OFFSET);
        assert_eq!(cs.read_u8(PM_CAP_OFFSET + 1), 0x00, "list terminates");

        // Geometry decodes exactly; Table Size stored as N-1.
        assert_eq!(cs.read_u16(MSIX_CAP_OFFSET + 2) & 0x07FF, 7);
        assert_eq!(cs.msix_table_size(), 8);
        assert_eq!(
            cs.read_u32(MSIX_CAP_OFFSET + 4),
            0x2000 | 1,
            "table off|bir"
        );
        assert_eq!(cs.read_u32(MSIX_CAP_OFFSET + 8), 0x3000 | 1, "pba off|bir");
        // Disabled + unmasked out of the box.
        assert!(!cs.msix_enabled());
        assert!(!cs.msix_function_masked());
        assert_eq!(cs.msix_table_size(), 8);
    }

    /// The guest may toggle MSI-X Enable / Function Mask, but Table Size and the
    /// Table/PBA Offset+BIR dwords are read-only — a probe-write cannot resize
    /// or relocate the table.
    #[test]
    fn guest_controls_msix_enable_but_not_table_geometry() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 9, 0), 0x1AF4, 0x1041);
        cs.add_msix_capability(16, 2, 0x4000, 2, 0x5000);
        assert_eq!(cs.msix_table_size(), 16);

        // Guest enables MSI-X and sets the function mask (bits 15 + 14).
        cs.guest_write(MSIX_CAP_OFFSET + 2, 2, 0xC000);
        assert!(cs.msix_enabled());
        assert!(cs.msix_function_masked());
        // ...without disturbing the Table Size field.
        assert_eq!(cs.msix_table_size(), 16);

        // A driver that probe-writes the whole control word + offsets cannot
        // shrink the table or move it into another BAR.
        cs.guest_write(MSIX_CAP_OFFSET + 2, 2, 0xFFFF); // try to set Table Size
        assert_eq!(cs.msix_table_size(), 16, "table size is read-only");
        cs.guest_write_u32(MSIX_CAP_OFFSET + 4, 0xDEAD_BEEF); // try to move table
        cs.guest_write_u32(MSIX_CAP_OFFSET + 8, 0xDEAD_BEEF); // try to move PBA
        assert_eq!(cs.read_u32(MSIX_CAP_OFFSET + 4), 0x4000 | 2);
        assert_eq!(cs.read_u32(MSIX_CAP_OFFSET + 8), 0x5000 | 2);
        // Enable/mask still controllable after the probe.
        assert!(cs.msix_enabled());

        // Guest clears Enable.
        cs.guest_write(MSIX_CAP_OFFSET + 2, 2, 0x0000);
        assert!(!cs.msix_enabled());
        assert!(!cs.msix_function_masked());
        assert_eq!(cs.msix_table_size(), 16);
    }

    /// A fresh MSI-X table comes up with every vector masked (Vector Control
    /// bit 0 set), no pending bits, and table MMIO reflecting the reset state.
    #[test]
    fn msix_table_resets_masked_with_empty_pba() {
        let t = MsixTable::new(4);
        assert_eq!(t.len(), 4);
        assert!(!t.is_empty());
        for v in 0..4usize {
            assert!(t.is_masked(v), "vector {v} masked at reset");
            assert!(!t.is_pending(v));
            // Address/data read as zero; Vector Control reads back the Mask bit.
            let base = u32::try_from(v).unwrap() * MSIX_ENTRY_SIZE;
            assert_eq!(t.read_table_u32(base), 0);
            assert_eq!(t.read_table_u32(base + 12), 1);
        }
        assert_eq!(t.read_pba_u64(0), 0);
        // Size clamps to >= 1 and <= 2048.
        assert_eq!(MsixTable::new(0).len(), 1);
        assert_eq!(MsixTable::new(5000).len(), 2048);
        // Misaligned reads report all-ones.
        assert_eq!(t.read_table_u32(2), 0xFFFF_FFFF);
    }

    /// Programming a vector's address/data through table MMIO and clearing its
    /// Mask bit makes `signal` return that exact message; reserved Vector
    /// Control bits are dropped on write.
    #[test]
    fn msix_program_then_signal_delivers_message() {
        let mut t = MsixTable::new(2);
        // Program vector 1: addr 0xFEE0_1000, data 0x0031.
        t.write_table_u32(MSIX_ENTRY_SIZE, 0xFEE0_1000); // addr_lo
        t.write_table_u32(MSIX_ENTRY_SIZE + 4, 0x0000_0000); // addr_hi
        t.write_table_u32(MSIX_ENTRY_SIZE + 8, 0x0000_0031); // data
        t.write_table_u32(MSIX_ENTRY_SIZE + 12, 0xFFFF_FFFE); // unmask; reserved RAZ
        assert_eq!(
            t.read_table_u32(MSIX_ENTRY_SIZE + 12),
            0,
            "reserved bits dropped"
        );
        assert!(!t.is_masked(1));

        let msg = t.signal(1, false).expect("deliverable");
        assert_eq!(msg, (0xFEE0_1000u64, 0x0031));
        assert!(!t.is_pending(1));
    }

    /// A masked vector defers the interrupt into the PBA; clearing the Mask bit
    /// and replaying via `take_pending` delivers it exactly once.
    #[test]
    fn msix_masked_signal_is_deferred_then_replayed_on_unmask() {
        let mut t = MsixTable::new(3);
        t.write_table_u32(MSIX_ENTRY_SIZE * 2, 0xFEE0_2000); // vec 2 addr_lo
        t.write_table_u32(MSIX_ENTRY_SIZE * 2 + 8, 0x00AA); // vec 2 data
        // Vector 2 still masked (reset): signal is deferred, not delivered.
        assert!(t.signal(2, false).is_none());
        assert!(t.is_pending(2));
        assert_eq!(t.read_pba_u64(0) & (1 << 2), 1 << 2, "PBA bit 2 set");

        // Unmask vector 2 and replay.
        t.write_table_u32(MSIX_ENTRY_SIZE * 2 + 12, 0);
        let replayed = t.take_pending(false);
        assert_eq!(replayed, vec![(0xFEE0_2000u64, 0x00AA)]);
        assert!(!t.is_pending(2), "PBA cleared after replay");
        assert!(t.take_pending(false).is_empty(), "replayed only once");
    }

    /// The global mask (MSI-X disabled or Function Mask set) defers every
    /// vector regardless of its per-entry Mask, and blocks replay until lifted.
    #[test]
    fn msix_global_mask_defers_all_and_blocks_replay() {
        let mut t = MsixTable::new(2);
        // Unmask vector 0 per-entry, but raise it while globally masked.
        t.write_table_u32(8, 0x0001); // vec 0 data
        t.write_table_u32(12, 0); // vec 0 unmasked
        assert!(t.signal(0, true).is_none(), "global mask defers");
        assert!(t.is_pending(0));
        // Replay while globally masked yields nothing...
        assert!(t.take_pending(true).is_empty());
        assert!(t.is_pending(0), "stays pending under global mask");
        // ...and flushes once the global mask is lifted.
        assert_eq!(t.take_pending(false), vec![(0u64, 0x0001)]);
    }

    /// A PCI Express Capability is a walkable cap-ID-0x10 v2 entry advertising
    /// the requested Device/Port Type, chaining ahead of the other caps, and
    /// describing a modest x1 link — what a guest needs to accept the function
    /// as a native `PCIe` device.
    #[test]
    fn pci_express_capability_is_walkable_and_describes_a_pcie_endpoint() {
        let mut cs = PciConfigSpace::new(PciBdf::new(0, 10, 0), 0x1AF4, 0x1041);
        cs.add_power_management_capability();
        cs.add_msix_capability(4, 1, 0x1000, 1, 0x2000);
        cs.add_pci_express_capability(pcie_type::ENDPOINT);

        // PCIe cap is the new list head, chaining MSI-X -> PM -> null.
        assert_ne!(cs.read_u16(cfg::STATUS) & 0x0010, 0, "caps bit");
        assert_eq!(u16::from(cs.read_u8(cfg::CAPABILITY_PTR)), PCIE_CAP_OFFSET);
        assert_eq!(cs.read_u8(PCIE_CAP_OFFSET), 0x10, "PCIe cap id");
        assert_eq!(u16::from(cs.read_u8(PCIE_CAP_OFFSET + 1)), MSIX_CAP_OFFSET);
        assert_eq!(u16::from(cs.read_u8(MSIX_CAP_OFFSET + 1)), PM_CAP_OFFSET);
        assert_eq!(cs.read_u8(PM_CAP_OFFSET + 1), 0x00, "list terminates");

        // Version 2, Endpoint type.
        assert_eq!(cs.pci_express_version(), 2);
        assert_eq!(cs.pci_express_device_type(), pcie_type::ENDPOINT);
        // Link Capabilities + Status both report 2.5 GT/s x1.
        assert_eq!(cs.read_u32(PCIE_CAP_OFFSET + 12) & 0x3FF, 0x011);
        assert_eq!(cs.read_u16(PCIE_CAP_OFFSET + 18) & 0x3FF, 0x011);
        // Max payload supported = 256 bytes (001b).
        assert_eq!(cs.read_u32(PCIE_CAP_OFFSET + 4) & 0x7, 0x1);
    }

    /// The Device/Port Type round-trips for a non-endpoint, and the version
    /// accessor reports 0 when the capability is absent (vs. a real endpoint's
    /// type 0).
    #[test]
    fn pci_express_device_type_round_trips_and_absent_reads_zero_version() {
        let mut rc = PciConfigSpace::new(PciBdf::new(0, 0, 0), 0x8086, 0x29C0);
        rc.add_pci_express_capability(pcie_type::ROOT_PORT);
        assert_eq!(rc.pci_express_version(), 2);
        assert_eq!(rc.pci_express_device_type(), pcie_type::ROOT_PORT);

        // A function without the capability reports version 0.
        let plain = PciConfigSpace::new(PciBdf::new(0, 1, 0), 0x8086, 0x1234);
        assert_eq!(plain.pci_express_version(), 0);
    }

    /// The discrete xHCI controller enumerates a real uPD720201-style capability
    /// list — PCI Express (Endpoint) -> MSI-X -> MSI -> Power Management -> end —
    /// so a guest USB 3.0 driver sees a faithful `PCIe` endpoint with both MSI
    /// and MSI-X, not a legacy-INTx-only PCI function. The MSI-X table and PBA
    /// are advertised in BAR0 at the offsets the controller's MMIO window
    /// decodes ([`crate::usb::MSIX_TABLE_BAR_OFFSET`] / `MSIX_PBA_BAR_OFFSET`).
    #[test]
    fn xhci_controller_advertises_pcie_endpoint_msi_and_pm_caps() {
        let cs = PcieRootComplex::create_xhci_controller(PciBdf::new(0, 0x14, 0), 0xFE90_0000);
        assert_ne!(cs.read_u16(cfg::STATUS) & 0x0010, 0, "caps bit set");

        // Walk the capability list from the pointer, collecting (offset, id).
        let mut off = cs.read_u8(cfg::CAPABILITY_PTR);
        let mut walk = Vec::new();
        // Bound the walk so a malformed loop can't hang the test.
        for _ in 0..16 {
            if off == 0 {
                break;
            }
            let id = cs.read_u8(u16::from(off));
            walk.push((off, id));
            off = cs.read_u8(u16::from(off) + 1);
        }
        assert_eq!(
            walk,
            vec![
                (0x90u8, 0x10u8), // PCI Express
                (0x70, 0x11),     // MSI-X
                (0x60, 0x05),     // MSI
                (0x50, 0x01),     // Power Management
            ],
            "xHCI cap list: PCIe -> MSI-X -> MSI -> PM -> end"
        );
        assert_eq!(cs.pci_express_device_type(), pcie_type::ENDPOINT);
        assert_eq!(cs.pci_express_version(), 2);
        assert!(
            !cs.msi_enabled(),
            "MSI present but disabled until programmed"
        );

        // MSI-X geometry: sized from XHCI_MSIX_VECTORS, table + PBA in BAR0 at
        // the fixed MMIO offsets, disabled + function-unmasked out of reset.
        assert_eq!(cs.msix_table_size(), crate::usb::XHCI_MSIX_VECTORS);
        assert_eq!(
            cs.read_u32(MSIX_CAP_OFFSET + 4),
            crate::usb::MSIX_TABLE_BAR_OFFSET, // BIR 0, offset in low bits = 0
            "MSI-X table in BAR0 at the MMIO offset"
        );
        assert_eq!(
            cs.read_u32(MSIX_CAP_OFFSET + 8),
            crate::usb::MSIX_PBA_BAR_OFFSET,
            "MSI-X PBA in BAR0 at the MMIO offset"
        );
        assert!(!cs.msix_enabled(), "MSI-X present but disabled until set");
        assert!(!cs.msix_function_masked());
    }

    /// `msi_message` is the read side of MSI delivery: nothing until the guest
    /// enables MSI, then the exact programmed 64-bit address + vector — the
    /// message a device hands to the interrupt path instead of asserting `INTx`.
    #[test]
    fn msi_message_reflects_programmed_address_and_vector() {
        let mut cs = PcieRootComplex::create_xhci_controller(PciBdf::new(0, 0x14, 0), 0xFE90_0000);
        assert!(cs.msi_message().is_none(), "disabled MSI yields no message");

        // Guest programs a 64-bit MSI to LAPIC 0, vector 0x42, then enables it.
        cs.guest_write_u32(MSI_CAP_OFFSET + 4, 0xFEE0_0000); // address low
        cs.guest_write_u32(MSI_CAP_OFFSET + 8, 0x0000_0000); // address high
        cs.guest_write(MSI_CAP_OFFSET + 12, 2, 0x0042); // data: vector 0x42
        cs.guest_write(MSI_CAP_OFFSET + 2, 2, 0x0081); // 64-bit + enable

        let msg = cs.msi_message().expect("enabled MSI yields a message");
        assert_eq!(msg.address, 0xFEE0_0000);
        assert_eq!(msg.data, 0x42);
        assert_eq!(msg.vector(), 0x42);
        assert_eq!(msg.destination_id(), 0);

        // Clearing the enable bit silences it again.
        cs.guest_write(MSI_CAP_OFFSET + 2, 2, 0x0080);
        assert!(cs.msi_message().is_none());
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
