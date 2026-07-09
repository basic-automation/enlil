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
        // 00:02.1 audio device — only reachable because 02.0 is multifunction.
        put_function(&mut mem, off(2, 1), 0x8086, 0x2668, 0x04, 0x03, 0x00, 0x00);
        // 00:04.0 xHCI USB 3 controller (class 0x0C / sub 0x03 / prog-IF 0x30).
        put_function(&mut mem, off(4, 0), 0x1912, 0x0015, 0x0C, 0x03, 0x30, 0x00);
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
