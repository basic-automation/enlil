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
    /// `0x00` general device, `0x01` PCI-to-PCI bridge, `0x02` CardBus bridge.
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
        header_type: read_u8(mem, base + u64::from(cfg::HEADER_TYPE))?,
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
    /// Capability ID (`0x01` PM, `0x05` MSI, `0x10` PCIe, `0x11` MSI-X, …).
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

/// Discover every xHCI (USB 3.x) host controller across all ECAM windows,
/// pairing each with its BAR0 MMIO base — the "PCI enum to xHCI BARs" step
/// the USB routing engine and the bare-metal xHCI driver (Phase 6.5) consume.
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

    /// Write a function's identity into a config-space image at `base`.
    fn put_function(
        mem: &mut [u8],
        base: usize,
        vendor: u16,
        device: u16,
        class: u8,
        subclass: u8,
        prog_if: u8,
        header_type: u8,
    ) {
        mem[base..base + 2].copy_from_slice(&vendor.to_le_bytes());
        mem[base + 2..base + 4].copy_from_slice(&device.to_le_bytes());
        mem[base + usize::from(cfg::REVISION_ID)] = 0x02;
        mem[base + usize::from(cfg::PROG_IF)] = prog_if;
        mem[base + usize::from(cfg::SUBCLASS)] = subclass;
        mem[base + usize::from(cfg::CLASS_CODE)] = class;
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
        put_function(&mut mem, off(0, 0), 0x8086, 0x29C0, 0x06, 0x00, 0x00, 0x00);
        // 00:02.0 VGA display controller, multifunction (header bit 7).
        put_function(&mut mem, off(2, 0), 0x8086, 0x2918, 0x03, 0x00, 0x00, 0x80);
        // BAR0: 32-bit non-prefetchable memory at 0xF600_0000; BAR1: I/O at 0xE000.
        put_bar(&mut mem, off(2, 0), 0, 0xF600_0000);
        put_bar(&mut mem, off(2, 0), 1, 0xE000 | 0x1);
        // 00:02.1 audio device — only reachable because 02.0 is multifunction.
        put_function(&mut mem, off(2, 1), 0x8086, 0x2668, 0x04, 0x03, 0x00, 0x00);
        // 00:04.0 xHCI USB 3 controller (class 0x0C / sub 0x03 / prog-IF 0x30).
        put_function(&mut mem, off(4, 0), 0x1912, 0x0015, 0x0C, 0x03, 0x30, 0x00);
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
    fn finds_xhci_controllers_with_their_mmio_base() {
        let (mem, alloc) = synthetic_ecam();
        let controllers = find_xhci_controllers(&mem, &[alloc]);
        assert_eq!(controllers.len(), 1);
        assert_eq!(controllers[0].function.bdf(), (0, 4, 0));
        assert_eq!(controllers[0].mmio_base, Some(0x0000_0004_F700_0000));
    }

    /// A bare general PciFunction at 00:00.0 for capability-walk tests.
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
