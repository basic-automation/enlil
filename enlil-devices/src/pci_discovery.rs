//! PCI Express config-space discovery over an ECAM window.
//!
//! The [`mcfg`](crate::acpi::mcfg) parser reads the firmware's MCFG table into
//! [`McfgAllocation`]s, each naming an Enhanced Configuration Access Mechanism
//! (ECAM) window: a physical base address and the bus range it decodes. This
//! module is the next step of Phase 6.3 "PCI devices via MCFG ECAM" — the walk
//! that turns those windows into the actual list of present PCI functions.
//!
//! ECAM maps every function's 4 KiB config space at a fixed physical offset
//! from the window base:
//!
//! ```text
//! addr = base + (bus - start_bus) * 2^20 + device * 2^15 + function * 2^12 + reg
//! ```
//!
//! so enumeration is a flat scan of the whole bus range — no PCI-to-PCI bridge
//! recursion is needed, because ECAM exposes buses behind bridges directly.
//! A function is present when its Vendor ID is neither `0xFFFF` (the value the
//! root complex returns for an unpopulated function) nor `0x0000` (reserved by
//! PCI-SIG, and what a zeroed/unmapped aperture reads as). Reads that fall
//! outside the supplied memory image are treated as absent rather than
//! panicking, so a window whose backing bytes were not captured simply yields
//! no devices for the missing region.

use crate::acpi::mcfg::McfgAllocation;
use crate::pcie::cfg;

/// One PCI function discovered by an ECAM config-space walk.
///
/// Holds the segment/bus/device/function coordinates and the identity fields
/// from the config-space header (Vendor/Device ID, revision, the class triple,
/// and the raw Header Type byte). The class triple plus a handful of
/// convenience predicates are enough for the device-tree build and for the
/// downstream USB-controller discovery item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciFunction {
    /// PCI segment group (from the ECAM allocation).
    pub segment: u16,
    /// Bus number.
    pub bus: u8,
    /// Device number (0-31).
    pub device: u8,
    /// Function number (0-7).
    pub function: u8,
    /// Vendor ID (config offset 0x00).
    pub vendor_id: u16,
    /// Device ID (config offset 0x02).
    pub device_id: u16,
    /// Revision ID (config offset 0x08).
    pub revision: u8,
    /// Programming interface / prog-IF (config offset 0x09).
    pub prog_if: u8,
    /// Subclass code (config offset 0x0A).
    pub subclass: u8,
    /// Base class code (config offset 0x0B).
    pub class_code: u8,
    /// Raw Header Type byte (config offset 0x0E), including the
    /// multifunction bit (bit 7).
    pub header_type: u8,
    /// Subsystem Vendor ID (config offset 0x2C) — identifies the card's OEM;
    /// `0` for non-general (header-layout ≠ 0x00) functions, which carry no
    /// subsystem IDs at that offset.
    pub subsystem_vendor_id: u16,
    /// Subsystem ID (config offset 0x2E); `0` for non-general functions.
    pub subsystem_id: u16,
}

impl PciFunction {
    /// The `(bus, device, function)` coordinate triple.
    #[must_use]
    pub const fn bdf(&self) -> (u8, u8, u8) {
        (self.bus, self.device, self.function)
    }

    /// Whether function 0 of this device advertises multiple functions
    /// (Header Type bit 7).
    #[must_use]
    pub const fn is_multifunction(&self) -> bool {
        self.header_type & 0x80 != 0
    }

    /// The header layout (Header Type with the multifunction bit masked off):
    /// `0x00` general device, `0x01` PCI-to-PCI bridge, `0x02` `CardBus` bridge.
    #[must_use]
    pub const fn header_layout(&self) -> u8 {
        self.header_type & 0x7F
    }

    /// Whether this function is a PCI-to-PCI bridge (class 0x06, subclass
    /// 0x04) — a header-layout-0x01 device whose secondary side hosts more
    /// buses (already covered by the flat ECAM scan).
    #[must_use]
    pub const fn is_pci_bridge(&self) -> bool {
        self.class_code == 0x06 && self.subclass == 0x04
    }

    /// Whether this function is a display controller (base class 0x03) — the
    /// GPUs the display/passthrough phases route.
    #[must_use]
    pub const fn is_display_controller(&self) -> bool {
        self.class_code == 0x03
    }

    /// Whether this function is a USB host controller (base class 0x0C,
    /// subclass 0x03). The `prog_if` distinguishes the generation: 0x30 =
    /// xHCI (USB 3), 0x20 = EHCI, 0x10 = OHCI, 0x00 = UHCI.
    #[must_use]
    pub const fn is_usb_controller(&self) -> bool {
        self.class_code == 0x0C && self.subclass == 0x03
    }

    /// Whether this function is specifically an xHCI (USB 3.x) host
    /// controller — the controllers Phase 6.5's bare-metal USB driver and the
    /// USB-routing engine care about.
    #[must_use]
    pub const fn is_xhci(&self) -> bool {
        self.is_usb_controller() && self.prog_if == 0x30
    }
}

/// A decoded PCI Base Address Register (BAR).
///
/// Only the base address and kind are recovered here — a BAR's *size* is
/// discovered by writing all-ones and reading back the writable bits, which
/// mutates config space and so belongs to the live bare-metal path, not this
/// read-only walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    /// A memory-mapped BAR at `base`. `is_64bit` BARs consume two consecutive
    /// slots (this one holds the low 32 bits, the next the high 32).
    Memory {
        /// Physical base address (low bits masked off).
        base: u64,
        /// Whether this is a 64-bit BAR (type bits `10`).
        is_64bit: bool,
        /// Whether the region is prefetchable (bit 3).
        prefetchable: bool,
    },
    /// An I/O-space BAR at port `base`.
    Io {
        /// I/O port base (low two bits masked off).
        base: u32,
    },
    /// A BAR programmed to zero — unimplemented or not yet assigned.
    Unimplemented,
}

/// Decode the BARs of one function from its config space.
///
/// Returns one [`Bar`] per implemented slot in ascending order. A general
/// device (header layout 0x00) has up to six BAR slots; a PCI-to-PCI bridge
/// (0x01) has two; other header types have none. A 64-bit memory BAR occupies
/// two slots and is reported once, with the following slot consumed. Slots
/// whose bytes fall outside the memory image stop the decode.
#[must_use]
pub fn read_bars(mem: &[u8], alloc: &McfgAllocation, func: &PciFunction) -> Vec<Bar> {
    let slots = match func.header_layout() {
        0x00 => 6,
        0x01 => 2,
        _ => 0,
    };
    let Some(base) = function_base(alloc, func.bus, func.device, func.function) else {
        return Vec::new();
    };
    let mut bars = Vec::new();
    let mut i = 0u64;
    while i < slots {
        let Some(raw) = read_u32(mem, base + u64::from(cfg::BAR0) + i * 4) else {
            break;
        };
        if raw == 0 {
            bars.push(Bar::Unimplemented);
            i += 1;
        } else if raw & 0x1 != 0 {
            bars.push(Bar::Io { base: raw & !0x3 });
            i += 1;
        } else {
            let prefetchable = raw & 0x8 != 0;
            let is_64bit = (raw >> 1) & 0x3 == 0x2;
            let low = u64::from(raw & !0xF);
            if is_64bit {
                // Consume the high dword; a missing high slot means the image
                // is truncated — stop rather than guess.
                let Some(high) = read_u32(mem, base + u64::from(cfg::BAR0) + (i + 1) * 4) else {
                    break;
                };
                bars.push(Bar::Memory {
                    base: (u64::from(high) << 32) | low,
                    is_64bit,
                    prefetchable,
                });
                i += 2;
            } else {
                bars.push(Bar::Memory {
                    base: low,
                    is_64bit,
                    prefetchable,
                });
                i += 1;
            }
        }
    }
    bars
}

/// Decode a 32-bit memory BAR's size from the value read back after writing
/// all-ones to it (the standard PCI BAR-sizing probe).
///
/// The card leaves its decoded-address bits writable and hard-wires the rest to
/// zero, so after an all-ones write the readback's low bits read zero up to the
/// region size. Masking off the low 4 info bits, inverting the remaining
/// writable size bits, and adding one recovers the size in bytes (a power of
/// two). A readback with no writable size bits (`0` after masking) means the BAR
/// is unimplemented — size `0`.
///
/// This is the pure decode the live bare-metal sizing path calls after its
/// write-probe; [`read_bars`] itself stays read-only and does not probe.
#[must_use]
pub const fn memory_bar_size_32(readback: u32) -> u32 {
    let masked = readback & 0xFFFF_FFF0;
    if masked == 0 {
        0
    } else {
        (!masked).wrapping_add(1)
    }
}

/// Decode a 64-bit memory BAR's size from the low and high dwords read back
/// after writing all-ones to both halves.
///
/// Like [`memory_bar_size_32`] but the writable size bits span both registers:
/// the low dword's info bits (bits 3:0) are masked off, the two dwords are
/// combined, inverted, and incremented. Size `0` if no writable bits remain.
#[must_use]
pub fn memory_bar_size_64(low_readback: u32, high_readback: u32) -> u64 {
    let masked = (u64::from(high_readback) << 32) | u64::from(low_readback & 0xFFFF_FFF0);
    if masked == 0 {
        0
    } else {
        (!masked).wrapping_add(1)
    }
}

/// Decode an I/O BAR's size from the all-ones write-probe readback.
///
/// As [`memory_bar_size_32`] but only bits 1:0 are info bits (the low bit marks
/// I/O space). I/O BARs decode at most a 32-bit port range; size `0` if no
/// writable size bits remain.
#[must_use]
pub const fn io_bar_size(readback: u32) -> u32 {
    let masked = readback & 0xFFFF_FFFC;
    if masked == 0 {
        0
    } else {
        (!masked).wrapping_add(1)
    }
}

/// The MMIO base address of an xHCI controller's register set — BAR0, the
/// controller's memory-mapped register window per the xHCI spec.
///
/// Returns `None` if `func` is not an xHCI controller or its first BAR is not
/// a memory BAR (an I/O or unimplemented BAR0 is not a valid xHCI mapping).
#[must_use]
pub fn xhci_mmio_base(mem: &[u8], alloc: &McfgAllocation, func: &PciFunction) -> Option<u64> {
    if !func.is_xhci() {
        return None;
    }
    match read_bars(mem, alloc, func).first() {
        Some(&Bar::Memory { base, .. }) => Some(base),
        _ => None,
    }
}

/// The physical address of a function's config space within an ECAM window,
/// or `None` if the bus falls outside the window's decoded range.
fn function_base(alloc: &McfgAllocation, bus: u8, device: u8, function: u8) -> Option<u64> {
    if bus < alloc.start_bus || bus > alloc.end_bus {
        return None;
    }
    let bus_off = u64::from(bus - alloc.start_bus) << 20;
    let dev_off = u64::from(device) << 15;
    let fn_off = u64::from(function) << 12;
    Some(alloc.base_address + bus_off + dev_off + fn_off)
}

/// Read one byte of the flat memory image at a physical address, or `None`
/// if it lies outside the image.
fn read_u8(mem: &[u8], addr: u64) -> Option<u8> {
    usize::try_from(addr).ok().and_then(|i| mem.get(i).copied())
}

/// Read a little-endian `u16` at a physical address, or `None` if either
/// byte lies outside the image.
fn read_u16(mem: &[u8], addr: u64) -> Option<u16> {
    let lo = read_u8(mem, addr)?;
    let hi = read_u8(mem, addr + 1)?;
    Some(u16::from_le_bytes([lo, hi]))
}

/// Read a little-endian `u32` at a physical address, or `None` if any byte
/// lies outside the image.
fn read_u32(mem: &[u8], addr: u64) -> Option<u32> {
    let b0 = read_u8(mem, addr)?;
    let b1 = read_u8(mem, addr + 1)?;
    let b2 = read_u8(mem, addr + 2)?;
    let b3 = read_u8(mem, addr + 3)?;
    Some(u32::from_le_bytes([b0, b1, b2, b3]))
}

/// Read the config-space header of one function, returning `None` if the
/// function is absent (Vendor ID `0xFFFF`/`0x0000`) or its bytes fall outside
/// the memory image.
fn read_function(
    mem: &[u8],
    alloc: &McfgAllocation,
    bus: u8,
    device: u8,
    function: u8,
) -> Option<PciFunction> {
    let base = function_base(alloc, bus, device, function)?;
    let vendor_id = read_u16(mem, base + u64::from(cfg::VENDOR_ID))?;
    if vendor_id == 0xFFFF || vendor_id == 0x0000 {
        return None;
    }
    let header_type = read_u8(mem, base + u64::from(cfg::HEADER_TYPE))?;
    // Subsystem IDs live at 0x2C/0x2E only in the general (layout-0x00) header;
    // a bridge (0x01) uses that region for other fields, so report 0 there.
    let header_layout = header_type & 0x7F;
    let (subsystem_vendor_id, subsystem_id) = if header_layout == 0x00 {
        (
            read_u16(mem, base + u64::from(cfg::SUBSYSTEM_VENDOR_ID))?,
            read_u16(mem, base + u64::from(cfg::SUBSYSTEM_ID))?,
        )
    } else {
        (0, 0)
    };
    Some(PciFunction {
        segment: alloc.segment_group,
        bus,
        device,
        function,
        vendor_id,
        device_id: read_u16(mem, base + u64::from(cfg::DEVICE_ID))?,
        revision: read_u8(mem, base + u64::from(cfg::REVISION_ID))?,
        prog_if: read_u8(mem, base + u64::from(cfg::PROG_IF))?,
        subclass: read_u8(mem, base + u64::from(cfg::SUBCLASS))?,
        class_code: read_u8(mem, base + u64::from(cfg::CLASS_CODE))?,
        header_type,
        subsystem_vendor_id,
        subsystem_id,
    })
}

/// Walk one ECAM window over the flat memory image `mem`, returning every
/// present PCI function in bus/device/function order.
///
/// For each device the walk reads function 0 first; a device whose function 0
/// is absent is skipped entirely (its other functions cannot exist), and a
/// device is probed for functions 1-7 only when function 0 sets the
/// multifunction header bit — the standard PCI enumeration rule.
#[must_use]
pub fn walk_ecam(mem: &[u8], alloc: &McfgAllocation) -> Vec<PciFunction> {
    let mut out = Vec::new();
    for bus in alloc.start_bus..=alloc.end_bus {
        for device in 0u8..32 {
            let Some(zero) = read_function(mem, alloc, bus, device, 0) else {
                continue;
            };
            let multifunction = zero.is_multifunction();
            out.push(zero);
            if multifunction {
                for function in 1u8..8 {
                    if let Some(func) = read_function(mem, alloc, bus, device, function) {
                        out.push(func);
                    }
                }
            }
        }
    }
    out
}

/// Walk every ECAM window in `allocs` and concatenate the discovered
/// functions. This is the caller-facing form applied to the
/// [`McfgAllocation`]s the MCFG parser produced.
#[must_use]
pub fn walk_ecam_allocations(mem: &[u8], allocs: &[McfgAllocation]) -> Vec<PciFunction> {
    allocs
        .iter()
        .flat_map(|alloc| walk_ecam(mem, alloc))
        .collect()
}

/// A discovered xHCI USB host controller: the PCI function and its BAR0 MMIO
/// register base, if that BAR decodes to a memory window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XhciController {
    /// The PCI function.
    pub function: PciFunction,
    /// The controller's MMIO register base (BAR0), or `None` if BAR0 is not a
    /// memory BAR (e.g. firmware has not assigned it).
    pub mmio_base: Option<u64>,
}

/// A legacy PCI capability found in standard config space (the `0x40..=0xFF`
/// device-specific region), reached from the capability pointer at `0x34`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    /// Capability ID (`0x01` PM, `0x05` MSI, `0x10` `PCIe`, `0x11` MSI-X, …).
    pub id: u8,
    /// Config-space offset of the capability structure.
    pub offset: u16,
}

impl Capability {
    /// Power Management capability ID.
    pub const POWER_MANAGEMENT: u8 = 0x01;
    /// MSI capability ID.
    pub const MSI: u8 = 0x05;
    /// PCI Express capability ID.
    pub const PCI_EXPRESS: u8 = 0x10;
    /// MSI-X capability ID.
    pub const MSI_X: u8 = 0x11;
}

/// Walk a function's legacy capability list, following the next-pointer chain
/// from the capability pointer at config `0x34`.
///
/// Returns each `(id, offset)` in list order. Empty if the STATUS
/// "capabilities list" bit (bit 4) is clear. The walk is cycle-guarded (a
/// malformed next-pointer that revisits an offset stops it) and masks the
/// reserved low two bits of each pointer.
#[must_use]
pub fn capabilities(mem: &[u8], alloc: &McfgAllocation, func: &PciFunction) -> Vec<Capability> {
    let Some(base) = function_base(alloc, func.bus, func.device, func.function) else {
        return Vec::new();
    };
    // Capabilities present only if STATUS bit 4 is set.
    match read_u16(mem, base + u64::from(cfg::STATUS)) {
        Some(status) if status & 0x10 != 0 => {}
        _ => return Vec::new(),
    }
    let Some(head) = read_u8(mem, base + u64::from(cfg::CAPABILITY_PTR)) else {
        return Vec::new();
    };
    let mut ptr = head & 0xFC;
    let mut out = Vec::new();
    let mut visited = Vec::new();
    while ptr >= 0x40 && !visited.contains(&ptr) {
        visited.push(ptr);
        let (Some(id), Some(next)) = (
            read_u8(mem, base + u64::from(ptr)),
            read_u8(mem, base + u64::from(ptr) + 1),
        ) else {
            break;
        };
        out.push(Capability {
            id,
            offset: u16::from(ptr),
        });
        ptr = next & 0xFC;
    }
    out
}

/// The MSI-X table size (number of interrupt vectors) a function advertises, or
/// `None` if it has no MSI-X capability.
///
/// Read from the MSI-X Message Control register at offset +2 of the capability:
/// bits 10:0 hold `table_size - 1`, so the returned count is
/// `(control & 0x7FF) + 1` (1..=2048). This is what interrupt setup needs to
/// size a guest's MSI-X table.
#[must_use]
pub fn msix_table_size(mem: &[u8], alloc: &McfgAllocation, func: &PciFunction) -> Option<u16> {
    let cap = capabilities(mem, alloc, func)
        .into_iter()
        .find(|c| c.id == Capability::MSI_X)?;
    let base = function_base(alloc, func.bus, func.device, func.function)?;
    let control = read_u16(mem, base + u64::from(cap.offset) + 2)?;
    Some((control & 0x7FF) + 1)
}

/// A PCI Express extended capability in extended config space (`>= 0x100`),
/// reached from the fixed head at offset `0x100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtendedCapability {
    /// Extended capability ID (`0x0010` SR-IOV, `0x000B` Vendor-specific, …).
    pub id: u16,
    /// Capability version (low nibble of the header's second byte).
    pub version: u8,
    /// Extended-config-space offset of the capability structure.
    pub offset: u16,
}

impl ExtendedCapability {
    /// Single Root I/O Virtualization (SR-IOV) extended capability ID.
    pub const SR_IOV: u16 = 0x0010;
    /// Access Control Services (ACS) extended capability ID.
    pub const ACS: u16 = 0x000D;
}

/// Walk a function's PCI Express extended capability list from the fixed head
/// at offset `0x100`, following the next-offset chain in each 32-bit header.
///
/// Returns each `(id, version, offset)` in list order. Empty if extended config
/// space is not backed by `mem` or the head reads as all-ones/zero (no extended
/// capabilities). Cycle-guarded like [`capabilities`].
#[must_use]
pub fn extended_capabilities(
    mem: &[u8],
    alloc: &McfgAllocation,
    func: &PciFunction,
) -> Vec<ExtendedCapability> {
    let Some(base) = function_base(alloc, func.bus, func.device, func.function) else {
        return Vec::new();
    };
    let mut off: u16 = 0x100;
    let mut out = Vec::new();
    let mut visited = Vec::new();
    while off >= 0x100 && !visited.contains(&off) {
        visited.push(off);
        let Some(header) = read_u32(mem, base + u64::from(off)) else {
            break;
        };
        let id = (header & 0xFFFF) as u16;
        // All-ones (unimplemented) or a zero header ends the list.
        if id == 0xFFFF || header == 0 {
            break;
        }
        out.push(ExtendedCapability {
            id,
            version: ((header >> 16) & 0xF) as u8,
            offset: off,
        });
        let next = ((header >> 20) & 0xFFF) as u16;
        if next == 0 {
            break;
        }
        off = next & 0xFFC;
    }
    out
}

/// Whether a function advertises the SR-IOV extended capability — the check the
/// GPU/NIC SR-IOV passthrough tiers (Phases 3.2, 7.3) gate on.
#[must_use]
pub fn is_sr_iov_capable(mem: &[u8], alloc: &McfgAllocation, func: &PciFunction) -> bool {
    extended_capabilities(mem, alloc, func)
        .iter()
        .any(|cap| cap.id == ExtendedCapability::SR_IOV)
}

/// The decoded fields of a Physical Function's SR-IOV extended capability — how
/// many Virtual Functions it supports and where they land in config space.
///
/// This is the register view the VF-enable step (Phases 3.2 NIC, 7.3 GPU) needs
/// before writing `NumVFs`/`VF Enable`: `total_vfs` caps how many VFs may be
/// enabled, and `first_vf_offset` + `vf_stride` place each VF's routing ID
/// relative to the PF (see [`SrIovCapability::vf_routing_id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrIovCapability {
    /// Whether VF Enable (Control bit 0) is currently set.
    pub vf_enabled: bool,
    /// `InitialVFs` — the number of VFs initially associated with the PF.
    pub initial_vfs: u16,
    /// `TotalVFs` — the maximum number of VFs the PF can support.
    pub total_vfs: u16,
    /// `NumVFs` — the number of VFs currently configured to be visible.
    pub num_vfs: u16,
    /// First VF Offset — the routing-ID delta from the PF to VF 0.
    pub first_vf_offset: u16,
    /// VF Stride — the routing-ID delta between consecutive VFs.
    pub vf_stride: u16,
    /// VF Device ID — the Device ID VFs report (the PF's own Device ID applies
    /// to the PF only).
    pub vf_device_id: u16,
    /// Supported Page Sizes bitmask (each bit n → page size 2^(n+12)).
    pub supported_page_sizes: u32,
    /// System Page Size bitmask currently programmed (one bit set).
    pub system_page_size: u32,
}

impl SrIovCapability {
    /// The 16-bit routing ID (`bus<<8 | dev<<3 | func`) of the `vf_index`-th VF
    /// (0-based) of a PF at `pf`, per the `PCIe` SR-IOV addressing formula
    /// `RID(VF) = RID(PF) + FirstVFOffset + vf_index * VFStride`.
    ///
    /// Returns `None` if `vf_index >= total_vfs` (out of the PF's VF range) or
    /// the computed routing ID would overflow 16 bits.
    #[must_use]
    pub fn vf_routing_id(&self, pf: &PciFunction, vf_index: u16) -> Option<u16> {
        if vf_index >= self.total_vfs {
            return None;
        }
        let pf_rid =
            (u16::from(pf.bus) << 8) | (u16::from(pf.device) << 3) | u16::from(pf.function);
        let stride = self.vf_stride.checked_mul(vf_index)?;
        pf_rid
            .checked_add(self.first_vf_offset)?
            .checked_add(stride)
    }
}

/// Decode a function's SR-IOV extended capability, if it has one.
///
/// Reads the SR-IOV register block (Control/InitialVFs/TotalVFs/NumVFs/First VF
/// Offset/VF Stride/VF Device ID/page sizes) from extended config space — the
/// "read the SR-IOV cap's TotalVFs/NumVFs" step (Phase 7.3d) that precedes
/// writing `NumVFs` to enable VFs. `None` if the function is not SR-IOV capable
/// or the register block is not backed by `mem`.
#[must_use]
pub fn sr_iov_capability(
    mem: &[u8],
    alloc: &McfgAllocation,
    func: &PciFunction,
) -> Option<SrIovCapability> {
    let cap = extended_capabilities(mem, alloc, func)
        .into_iter()
        .find(|c| c.id == ExtendedCapability::SR_IOV)?;
    let base = function_base(alloc, func.bus, func.device, func.function)?;
    let field = base + u64::from(cap.offset);
    Some(SrIovCapability {
        vf_enabled: read_u16(mem, field + 0x08)? & 0x1 != 0,
        initial_vfs: read_u16(mem, field + 0x0C)?,
        total_vfs: read_u16(mem, field + 0x0E)?,
        num_vfs: read_u16(mem, field + 0x10)?,
        first_vf_offset: read_u16(mem, field + 0x14)?,
        vf_stride: read_u16(mem, field + 0x16)?,
        vf_device_id: read_u16(mem, field + 0x1A)?,
        supported_page_sizes: read_u32(mem, field + 0x1C)?,
        system_page_size: read_u32(mem, field + 0x20)?,
    })
}

/// SR-IOV Control-register (offset `0x08`) bit for VF Enable — turns the VFs on.
pub const SR_IOV_CTRL_VF_ENABLE: u16 = 1 << 0;
/// SR-IOV Control-register bit for VF Memory Space Enable — lets the VFs' memory
/// BARs decode; without it the enabled VFs answer no MMIO.
pub const SR_IOV_CTRL_VF_MSE: u16 = 1 << 3;
/// Config-space offset of the SR-IOV `NumVFs` register relative to the cap base.
pub const SR_IOV_OFF_NUM_VFS: u16 = 0x10;
/// Config-space offset of the SR-IOV Control register relative to the cap base.
pub const SR_IOV_OFF_CONTROL: u16 = 0x08;

/// Why building an SR-IOV VF-enable plan failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrIovPlanError {
    /// `requested_vfs` was zero — enabling zero VFs is a no-op, not a plan.
    ZeroVfs,
    /// `requested_vfs` exceeds the PF's `TotalVFs`.
    ExceedsTotalVfs {
        /// The requested VF count.
        requested: u16,
        /// The PF's maximum (`TotalVFs`).
        total: u16,
    },
    /// A VF's routing ID overflowed 16 bits (a malformed First VF Offset /
    /// VF Stride against this PF).
    RoutingOverflow {
        /// The 0-based VF index whose routing ID overflowed.
        vf_index: u16,
    },
}

/// The config-space writes and resulting VF routing IDs needed to enable
/// `requested_vfs` Virtual Functions on a PF — the "enable VFs" step (Phase
/// 7.3d) that follows reading the [`SrIovCapability`].
///
/// This is the pure decision layer: it validates the request against `TotalVFs`,
/// enumerates the VFs' routing IDs, and states the two config writes (set
/// `NumVFs`, then set VF Enable + VF MSE in Control). The live bare-metal path
/// performs the writes in order — `NumVFs` first, then the Control bits — and
/// waits the spec-mandated settle time before touching the VFs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfEnablePlan {
    /// Value to write to the `NumVFs` register (config offset
    /// [`SR_IOV_OFF_NUM_VFS`] from the cap base).
    pub num_vfs: u16,
    /// Bits to OR into the SR-IOV Control register (config offset
    /// [`SR_IOV_OFF_CONTROL`]) — VF Enable and VF MSE.
    pub control_bits_to_set: u16,
    /// Routing IDs of the enabled VFs, in VF-index order.
    pub vf_routing_ids: Vec<u16>,
}

/// Build the plan to enable `requested_vfs` VFs on the PF at `pf` given its
/// decoded `cap`.
///
/// # Errors
/// - [`SrIovPlanError::ZeroVfs`] if `requested_vfs == 0`;
/// - [`SrIovPlanError::ExceedsTotalVfs`] if `requested_vfs > cap.total_vfs`;
/// - [`SrIovPlanError::RoutingOverflow`] if a VF's routing ID overflows 16 bits.
pub fn plan_enable_vfs(
    cap: &SrIovCapability,
    pf: &PciFunction,
    requested_vfs: u16,
) -> Result<VfEnablePlan, SrIovPlanError> {
    if requested_vfs == 0 {
        return Err(SrIovPlanError::ZeroVfs);
    }
    if requested_vfs > cap.total_vfs {
        return Err(SrIovPlanError::ExceedsTotalVfs {
            requested: requested_vfs,
            total: cap.total_vfs,
        });
    }
    let mut vf_routing_ids = Vec::with_capacity(requested_vfs as usize);
    for vf_index in 0..requested_vfs {
        let rid = cap
            .vf_routing_id(pf, vf_index)
            .ok_or(SrIovPlanError::RoutingOverflow { vf_index })?;
        vf_routing_ids.push(rid);
    }
    Ok(VfEnablePlan {
        num_vfs: requested_vfs,
        control_bits_to_set: SR_IOV_CTRL_VF_ENABLE | SR_IOV_CTRL_VF_MSE,
        vf_routing_ids,
    })
}

/// Discover every xHCI (USB 3.x) host controller across all ECAM windows.
///
/// Pairs each with its BAR0 MMIO base — the "PCI enum to xHCI BARs" step the USB
/// routing engine and the bare-metal xHCI driver (Phase 6.5) consume.
#[must_use]
pub fn find_xhci_controllers(mem: &[u8], allocs: &[McfgAllocation]) -> Vec<XhciController> {
    walk_ecam_allocations(mem, allocs)
        .into_iter()
        .filter(PciFunction::is_xhci)
        .map(|function| {
            // BAR0 is decoded from the window whose bus range contains the
            // function; windows that do not cover the bus return `None`.
            let mmio_base = allocs
                .iter()
                .find_map(|alloc| xhci_mmio_base(mem, alloc, &function));
            XhciController {
                function,
                mmio_base,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a function's identity into a config-space image at `base`. `class`
    /// is the `(class_code, subclass, prog_if)` triple.
    fn put_function(
        mem: &mut [u8],
        base: usize,
        vendor: u16,
        device: u16,
        class: (u8, u8, u8),
        header_type: u8,
    ) {
        let (class_code, subclass, prog_if) = class;
        mem[base..base + 2].copy_from_slice(&vendor.to_le_bytes());
        mem[base + 2..base + 4].copy_from_slice(&device.to_le_bytes());
        mem[base + usize::from(cfg::REVISION_ID)] = 0x02;
        mem[base + usize::from(cfg::PROG_IF)] = prog_if;
        mem[base + usize::from(cfg::SUBCLASS)] = subclass;
        mem[base + usize::from(cfg::CLASS_CODE)] = class_code;
        mem[base + usize::from(cfg::HEADER_TYPE)] = header_type;
    }

    /// Write a raw 32-bit BAR value into a function's config space.
    fn put_bar(mem: &mut [u8], fn_base: usize, index: usize, value: u32) {
        let off = fn_base + usize::from(cfg::BAR0) + index * 4;
        mem[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// The ECAM function offset for base address 0.
    fn off(device: u8, function: u8) -> usize {
        (usize::from(device) << 15) | (usize::from(function) << 12)
    }

    /// A synthetic single-segment ECAM window at base 0 with a host bridge, a
    /// multifunction VGA+audio device, an xHCI controller, and one slot left
    /// zeroed (absent).
    fn synthetic_ecam() -> (Vec<u8>, McfgAllocation) {
        // Cover through device 5's config space.
        let mut mem = vec![0u8; off(5, 0) + 0x1000];
        // 00:00.0 Intel host bridge (class 0x06 bridge / 0x00 host).
        put_function(
            &mut mem,
            off(0, 0),
            0x8086,
            0x29C0,
            (0x06, 0x00, 0x00),
            0x00,
        );
        // 00:02.0 VGA display controller, multifunction (header bit 7).
        put_function(
            &mut mem,
            off(2, 0),
            0x8086,
            0x2918,
            (0x03, 0x00, 0x00),
            0x80,
        );
        // BAR0: 32-bit non-prefetchable memory at 0xF600_0000; BAR1: I/O at 0xE000.
        put_bar(&mut mem, off(2, 0), 0, 0xF600_0000);
        put_bar(&mut mem, off(2, 0), 1, 0xE000 | 0x1);
        // 00:02.1 audio device — only reachable because 02.0 is multifunction.
        put_function(
            &mut mem,
            off(2, 1),
            0x8086,
            0x2668,
            (0x04, 0x03, 0x00),
            0x00,
        );
        // 00:04.0 xHCI USB 3 controller (class 0x0C / sub 0x03 / prog-IF 0x30).
        put_function(
            &mut mem,
            off(4, 0),
            0x1912,
            0x0015,
            (0x0C, 0x03, 0x30),
            0x00,
        );
        // Its subsystem IDs (0x2C/0x2E) identify the card's OEM.
        mem[off(4, 0) + 0x2C..off(4, 0) + 0x2E].copy_from_slice(&0x1043u16.to_le_bytes());
        mem[off(4, 0) + 0x2E..off(4, 0) + 0x30].copy_from_slice(&0x8694u16.to_le_bytes());
        // BAR0: 64-bit non-prefetchable memory at 0x0000_0004_F700_0000
        // (low dword carries base bits 31:4 | type 0b10; high dword bits 63:32).
        put_bar(&mut mem, off(4, 0), 0, 0xF700_0000 | 0x4);
        put_bar(&mut mem, off(4, 0), 1, 0x0000_0004);
        // 00:05.0 left zeroed -> Vendor 0x0000 -> treated as absent.
        (mem, McfgAllocation::standard(0))
    }

    #[test]
    fn walks_present_functions_and_honors_multifunction() {
        let (mem, alloc) = synthetic_ecam();
        let funcs = walk_ecam(&mem, &alloc);
        let bdfs: Vec<_> = funcs.iter().map(PciFunction::bdf).collect();
        assert_eq!(bdfs, vec![(0, 0, 0), (0, 2, 0), (0, 2, 1), (0, 4, 0)]);
    }

    #[test]
    fn a_single_function_device_is_not_probed_for_more_functions() {
        // The host bridge (00:00.0) is single-function (header 0x00), so the
        // walk never probes functions 1-7 of device 0 — exactly one 00:00.x
        // function is reported.
        let (mem, alloc) = synthetic_ecam();
        let funcs = walk_ecam(&mem, &alloc);
        let dev0 = funcs.iter().filter(|f| f.bdf().1 == 0).count();
        assert_eq!(dev0, 1);
    }

    #[test]
    fn classifies_display_and_usb_controllers() {
        let (mem, alloc) = synthetic_ecam();
        let funcs = walk_ecam(&mem, &alloc);

        let vga = funcs.iter().find(|f| f.bdf() == (0, 2, 0)).unwrap();
        assert!(vga.is_display_controller());
        assert!(vga.is_multifunction());
        assert_eq!(vga.header_layout(), 0x00);

        // 00:00.0 is a host bridge (class 0x06 / subclass 0x00) — a bridge
        // class, but not a PCI-to-PCI bridge (subclass 0x04).
        let host = funcs.iter().find(|f| f.bdf() == (0, 0, 0)).unwrap();
        assert_eq!(host.class_code, 0x06);
        assert!(!host.is_pci_bridge());
        assert!(!host.is_display_controller());

        let xhci = funcs.iter().find(|f| f.bdf() == (0, 4, 0)).unwrap();
        assert!(xhci.is_usb_controller());
        assert!(xhci.is_xhci());
        assert_eq!(xhci.vendor_id, 0x1912);
        assert_eq!(xhci.device_id, 0x0015);
        // Subsystem IDs (0x2C/0x2E) are read for the general-header xHCI.
        assert_eq!(xhci.subsystem_vendor_id, 0x1043);
        assert_eq!(xhci.subsystem_id, 0x8694);
        // The host bridge is a general header too but has no subsystem IDs set.
        assert_eq!(host.subsystem_vendor_id, 0);
    }

    #[test]
    fn decodes_a_64bit_memory_bar_and_finds_the_xhci_mmio_base() {
        let (mem, alloc) = synthetic_ecam();
        let xhci = walk_ecam(&mem, &alloc)
            .into_iter()
            .find(|f| f.bdf() == (0, 4, 0))
            .unwrap();

        let bars = read_bars(&mem, &alloc, &xhci);
        // BAR0 is a 64-bit memory BAR occupying two slots, reported once; the
        // remaining two slots (0x18/0x24 region: indices 2-5, minus the one
        // the 64-bit BAR consumed) are unprogrammed.
        assert_eq!(
            bars[0],
            Bar::Memory {
                base: 0x0000_0004_F700_0000,
                is_64bit: true,
                prefetchable: false,
            }
        );
        assert!(bars[1..].iter().all(|b| *b == Bar::Unimplemented));
        assert_eq!(bars.len(), 5); // 1 (64-bit pair) + 4 unprogrammed
        assert_eq!(
            xhci_mmio_base(&mem, &alloc, &xhci),
            Some(0x0000_0004_F700_0000)
        );
    }

    #[test]
    fn decodes_memory_and_io_bars() {
        let (mem, alloc) = synthetic_ecam();
        let vga = walk_ecam(&mem, &alloc)
            .into_iter()
            .find(|f| f.bdf() == (0, 2, 0))
            .unwrap();
        let bars = read_bars(&mem, &alloc, &vga);
        assert_eq!(
            bars[0],
            Bar::Memory {
                base: 0xF600_0000,
                is_64bit: false,
                prefetchable: false,
            }
        );
        assert_eq!(bars[1], Bar::Io { base: 0xE000 });
        // The remaining four slots are unprogrammed.
        assert_eq!(bars[2..], [Bar::Unimplemented; 4]);
    }

    #[test]
    fn decodes_bar_sizes_from_the_write_probe_readback() {
        // 32-bit memory BAR: a 16 MiB region leaves bits 23:4 writable → after
        // an all-ones write it reads back 0xFF00_0000 (low 24 bits zero, info
        // bits included). Size = 0x0100_0000 = 16 MiB.
        assert_eq!(memory_bar_size_32(0xFF00_0000), 0x0100_0000);
        // A 16-byte region (the minimum): readback 0xFFFF_FFF0 → 0x10.
        assert_eq!(memory_bar_size_32(0xFFFF_FFF0), 0x10);
        // Prefetchable/64-bit info bits (low 4) are ignored: 0xFF00_000C decodes
        // the same 16 MiB.
        assert_eq!(memory_bar_size_32(0xFF00_000C), 0x0100_0000);
        // Unimplemented BAR (no writable bits) → size 0.
        assert_eq!(memory_bar_size_32(0), 0);

        // 64-bit memory BAR spanning both dwords: a 4 GiB region reads back
        // low=0x0000_0000, high=0xFFFF_FFFF → size 0x1_0000_0000.
        assert_eq!(memory_bar_size_64(0x0000_0000, 0xFFFF_FFFF), 0x1_0000_0000);
        // A 256 MiB 64-bit BAR: low=0xF000_0000, high=0xFFFF_FFFF.
        assert_eq!(memory_bar_size_64(0xF000_0000, 0xFFFF_FFFF), 0x1000_0000);
        assert_eq!(memory_bar_size_64(0, 0), 0);

        // I/O BAR: only bits 1:0 are info bits. A 256-byte port range reads back
        // 0xFFFF_FF01 (bit 0 set = I/O) → mask → 0xFFFF_FF00 → size 0x100.
        assert_eq!(io_bar_size(0xFFFF_FF01), 0x100);
        assert_eq!(io_bar_size(0), 0);
    }

    #[test]
    fn finds_xhci_controllers_with_their_mmio_base() {
        let (mem, alloc) = synthetic_ecam();
        let controllers = find_xhci_controllers(&mem, &[alloc]);
        assert_eq!(controllers.len(), 1);
        assert_eq!(controllers[0].function.bdf(), (0, 4, 0));
        assert_eq!(controllers[0].mmio_base, Some(0x0000_0004_F700_0000));
    }

    /// A bare general `PciFunction` at 00:00.0 for capability-walk tests.
    fn plain_function() -> PciFunction {
        PciFunction {
            segment: 0,
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0x8086,
            device_id: 0x10FB,
            revision: 1,
            prog_if: 0,
            subclass: 0,
            class_code: 0x02,
            header_type: 0,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
        }
    }

    #[test]
    fn walks_the_legacy_capability_chain() {
        let mut mem = vec![0u8; 0x110];
        mem[0x06..0x08].copy_from_slice(&0x0010u16.to_le_bytes()); // STATUS: caps present
        mem[0x34] = 0x40; // capability pointer -> first cap
        mem[0x40] = Capability::POWER_MANAGEMENT;
        mem[0x41] = 0x50; // -> next
        mem[0x50] = Capability::MSI;
        mem[0x51] = 0x60; // -> next
        mem[0x60] = Capability::MSI_X;
        mem[0x61] = 0x00; // end of list
        // MSI-X Message Control at +2: table size 8 → encoded as 7.
        mem[0x62..0x64].copy_from_slice(&7u16.to_le_bytes());

        let alloc = McfgAllocation::standard(0);
        let func = plain_function();
        assert_eq!(msix_table_size(&mem, &alloc, &func), Some(8));
        // A function with no MSI-X capability reports None.
        let mut bare = vec![0u8; 0x100];
        bare[0x06..0x08].copy_from_slice(&0x0010u16.to_le_bytes()); // caps present
        bare[0x34] = 0x40;
        bare[0x40] = Capability::MSI; // MSI only, no MSI-X
        bare[0x41] = 0x00;
        assert_eq!(msix_table_size(&bare, &alloc, &func), None);

        let caps = capabilities(&mem, &McfgAllocation::standard(0), &plain_function());
        assert_eq!(
            caps,
            vec![
                Capability {
                    id: 0x01,
                    offset: 0x40,
                },
                Capability {
                    id: 0x05,
                    offset: 0x50,
                },
                Capability {
                    id: 0x11,
                    offset: 0x60,
                },
            ]
        );
    }

    #[test]
    fn no_capabilities_when_the_status_bit_is_clear() {
        let mut mem = vec![0u8; 0x110];
        // STATUS caps bit clear, but a stale pointer/cap present: must be ignored.
        mem[0x34] = 0x40;
        mem[0x40] = Capability::MSI;
        assert!(
            capabilities(&mem, &McfgAllocation::standard(0), &plain_function()).is_empty(),
            "no walk without the STATUS capabilities-list bit"
        );
    }

    #[test]
    fn walks_the_extended_capability_list_and_detects_sr_iov() {
        let mut mem = vec![0u8; 0x110];
        // Extended cap head at 0x100: id 0x0010 (SR-IOV), version 1, next 0 (end).
        let header = u32::from(ExtendedCapability::SR_IOV) | (1 << 16);
        mem[0x100..0x104].copy_from_slice(&header.to_le_bytes());

        let alloc = McfgAllocation::standard(0);
        let func = plain_function();
        assert_eq!(
            extended_capabilities(&mem, &alloc, &func),
            vec![ExtendedCapability {
                id: 0x0010,
                version: 1,
                offset: 0x100,
            }]
        );
        assert!(is_sr_iov_capable(&mem, &alloc, &func));
    }

    #[test]
    fn decodes_the_sr_iov_register_block_and_vf_routing_ids() {
        // Extended config space large enough to hold the SR-IOV block at 0x100.
        let mut mem = vec![0u8; 0x160];
        let header = u32::from(ExtendedCapability::SR_IOV) | (1 << 16); // ver 1, next 0
        mem[0x100..0x104].copy_from_slice(&header.to_le_bytes());
        // Control (VF Enable set), InitialVFs, TotalVFs, NumVFs.
        mem[0x108..0x10A].copy_from_slice(&0x0001u16.to_le_bytes()); // Control
        mem[0x10C..0x10E].copy_from_slice(&8u16.to_le_bytes()); // InitialVFs
        mem[0x10E..0x110].copy_from_slice(&16u16.to_le_bytes()); // TotalVFs
        mem[0x110..0x112].copy_from_slice(&4u16.to_le_bytes()); // NumVFs
        mem[0x114..0x116].copy_from_slice(&0x80u16.to_le_bytes()); // First VF Offset
        mem[0x116..0x118].copy_from_slice(&2u16.to_le_bytes()); // VF Stride
        mem[0x11A..0x11C].copy_from_slice(&0x10EDu16.to_le_bytes()); // VF Device ID
        mem[0x11C..0x120].copy_from_slice(&0x0000_0553u32.to_le_bytes()); // Supported page sizes
        mem[0x120..0x124].copy_from_slice(&0x0000_0001u32.to_le_bytes()); // System page size

        let alloc = McfgAllocation::standard(0);
        let func = plain_function();
        let cap = sr_iov_capability(&mem, &alloc, &func).expect("SR-IOV cap present");
        assert!(cap.vf_enabled);
        assert_eq!(cap.initial_vfs, 8);
        assert_eq!(cap.total_vfs, 16);
        assert_eq!(cap.num_vfs, 4);
        assert_eq!(cap.first_vf_offset, 0x80);
        assert_eq!(cap.vf_stride, 2);
        assert_eq!(cap.vf_device_id, 0x10ED);
        assert_eq!(cap.supported_page_sizes, 0x0000_0553);
        assert_eq!(cap.system_page_size, 1);

        // plain_function() is at BDF 00:00.0 → PF routing ID 0. VF k lands at
        // 0x80 + k*2.
        assert_eq!(func.bdf(), (0, 0, 0));
        assert_eq!(cap.vf_routing_id(&func, 0), Some(0x80));
        assert_eq!(cap.vf_routing_id(&func, 1), Some(0x82));
        assert_eq!(cap.vf_routing_id(&func, 15), Some(0x80 + 15 * 2));
        // vf_index == total_vfs is out of range.
        assert_eq!(cap.vf_routing_id(&func, 16), None);
    }

    #[test]
    fn plans_a_vf_enable_from_the_capability() {
        // A PF with 16 TotalVFs, First VF Offset 0x80, stride 2 at BDF 00:00.0.
        let cap = SrIovCapability {
            vf_enabled: false,
            initial_vfs: 8,
            total_vfs: 16,
            num_vfs: 0,
            first_vf_offset: 0x80,
            vf_stride: 2,
            vf_device_id: 0x10ED,
            supported_page_sizes: 0x553,
            system_page_size: 1,
        };
        let pf = plain_function();

        let plan = plan_enable_vfs(&cap, &pf, 4).expect("plan 4 VFs");
        assert_eq!(plan.num_vfs, 4);
        assert_eq!(
            plan.control_bits_to_set,
            SR_IOV_CTRL_VF_ENABLE | SR_IOV_CTRL_VF_MSE
        );
        // VF k routing ID = 0x80 + k*2.
        assert_eq!(plan.vf_routing_ids, vec![0x80, 0x82, 0x84, 0x86]);

        // Requesting all TotalVFs is allowed; requesting one more is rejected.
        assert_eq!(
            plan_enable_vfs(&cap, &pf, 16).unwrap().vf_routing_ids.len(),
            16
        );
        assert_eq!(
            plan_enable_vfs(&cap, &pf, 17),
            Err(SrIovPlanError::ExceedsTotalVfs {
                requested: 17,
                total: 16,
            })
        );
        assert_eq!(plan_enable_vfs(&cap, &pf, 0), Err(SrIovPlanError::ZeroVfs));
    }

    #[test]
    fn sr_iov_capability_is_none_without_the_cap() {
        // Only the base 256 bytes → no extended region, no SR-IOV block.
        let mem = vec![0u8; 0x100];
        let alloc = McfgAllocation::standard(0);
        assert_eq!(sr_iov_capability(&mem, &alloc, &plain_function()), None);
    }

    #[test]
    fn extended_capabilities_absent_without_a_backing_image() {
        // Only 256 bytes of config space (no extended region) -> no ext caps,
        // and not SR-IOV capable, without panicking.
        let mem = vec![0u8; 0x100];
        let alloc = McfgAllocation::standard(0);
        let func = plain_function();
        assert!(extended_capabilities(&mem, &alloc, &func).is_empty());
        assert!(!is_sr_iov_capable(&mem, &alloc, &func));
    }

    #[test]
    fn xhci_mmio_base_is_none_for_a_non_xhci_function() {
        let (mem, alloc) = synthetic_ecam();
        let vga = walk_ecam(&mem, &alloc)
            .into_iter()
            .find(|f| f.bdf() == (0, 2, 0))
            .unwrap();
        assert_eq!(xhci_mmio_base(&mem, &alloc, &vga), None);
    }

    #[test]
    fn pci_to_pci_bridge_is_recognized() {
        let bridge = PciFunction {
            segment: 0,
            bus: 0,
            device: 0x1C,
            function: 0,
            vendor_id: 0x8086,
            device_id: 0x244E,
            revision: 0,
            prog_if: 0,
            subclass: 0x04,
            class_code: 0x06,
            header_type: 0x01,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
        };
        assert!(bridge.is_pci_bridge());
        assert_eq!(bridge.header_layout(), 0x01);
    }

    #[test]
    fn reads_outside_the_image_are_absent_not_panics() {
        // A window whose base points past the end of the image yields nothing
        // rather than panicking on the out-of-range indices.
        let mem = vec![0u8; 0x1000];
        let alloc = McfgAllocation::standard(0x8000_0000);
        assert!(walk_ecam(&mem, &alloc).is_empty());
    }

    #[test]
    fn walk_allocations_concatenates_every_window() {
        let (mem, alloc) = synthetic_ecam();
        // The same window listed twice yields its functions twice — the walk
        // does not deduplicate across allocations.
        let doubled = walk_ecam_allocations(&mem, &[alloc.clone(), alloc]);
        assert_eq!(doubled.len(), 8);
    }

    #[test]
    fn segment_group_is_carried_from_the_allocation() {
        let (mem, _) = synthetic_ecam();
        let alloc = McfgAllocation {
            base_address: 0,
            segment_group: 3,
            start_bus: 0,
            end_bus: 0,
        };
        let funcs = walk_ecam(&mem, &alloc);
        assert!(funcs.iter().all(|f| f.segment == 3));
        assert!(!funcs.is_empty());
    }
}
