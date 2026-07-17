//! Minimal bare-metal PCI discovery for the enlil kernel (Phase 6.3).
//!
//! Complements the ACPI walk ([`crate::acpi`]) with device discovery: the
//! kernel enumerates the PCI bus using the legacy configuration mechanism #1
//! (the `0xCF8`/`0xCFC` I/O-port pair), which needs no MMIO mapping — unlike
//! ECAM it works straight after `ExitBootServices()` on the firmware's page
//! tables. It counts the present functions on bus 0 and reads the host bridge's
//! identity as a proof the reads reach real config space. The address
//! arithmetic and identity decode are pure and host-tested; only the port I/O
//! is gated to the firmware target.

/// PCI configuration-space address port (mechanism #1).
pub const PCI_CONFIG_ADDRESS: u16 = 0xCF8;

/// PCI configuration-space data port (mechanism #1).
pub const PCI_CONFIG_DATA: u16 = 0xCFC;

/// The vendor id a bus/device/function returns when nothing is present.
pub const PCI_VENDOR_ABSENT: u16 = 0xFFFF;

/// The 32-bit `CONFIG_ADDRESS` value selecting `offset` in the config space of
/// `bus:device.function` (mechanism #1, SDM/PCI spec).
///
/// Bit 31 is the enable bit; the register offset is dword-aligned (low two bits
/// forced to 0). `device` is 5 bits, `function` 3 bits — wider inputs are
/// masked to their fields.
#[must_use]
pub const fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | (((device as u32) & 0x1F) << 11)
        | (((function as u32) & 0x07) << 8)
        | ((offset as u32) & 0xFC)
}

/// The vendor id (low 16 bits) from a config dword-0 read.
#[must_use]
pub const fn vendor_of(config_dword0: u32) -> u16 {
    (config_dword0 & 0xFFFF) as u16
}

/// The device id (high 16 bits) from a config dword-0 read.
#[must_use]
pub const fn device_of(config_dword0: u32) -> u16 {
    (config_dword0 >> 16) as u16
}

/// Whether a vendor id denotes a present function (not the all-ones "absent").
#[must_use]
pub const fn vendor_present(vendor: u16) -> bool {
    vendor != PCI_VENDOR_ABSENT
}

/// The base-class code (high byte of the class dword at config offset 0x08).
#[must_use]
pub const fn class_of(config_dword_at_08: u32) -> u8 {
    (config_dword_at_08 >> 24) as u8
}

/// The MMIO address of `offset` in `bus:device.function`'s config space within
/// an ECAM window based at `ecam_base`.
///
/// The PCI Express firmware-spec formula:
/// `ecam_base + (bus << 20) + (device << 15) + (function << 12) + offset`.
/// Unlike the legacy `0xCF8`/`0xCFC` mechanism this reaches the full 4 KiB
/// extended config space and all 256 buses. `device`/`function` are masked to
/// their 5-/3-bit fields.
#[must_use]
pub const fn ecam_config_address(
    ecam_base: u64,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
) -> u64 {
    ecam_base
        + ((bus as u64) << 20)
        + (((device as u64) & 0x1F) << 15)
        + (((function as u64) & 0x07) << 12)
        + (offset as u64)
}

/// What an ECAM full-bus scan discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EcamScan {
    /// Present functions found across every scanned bus.
    pub functions: u32,
    /// Distinct buses that carried at least one present function.
    pub buses_in_use: u32,
}

/// PCI base class: mass-storage controller (SATA/NVMe/SCSI/IDE).
pub const CLASS_STORAGE: u8 = 0x01;
/// PCI base class: network controller.
pub const CLASS_NETWORK: u8 = 0x02;
/// PCI base class: display controller (VGA/GPU).
pub const CLASS_DISPLAY: u8 = 0x03;

/// Capability id: MSI-X (message-signaled interrupts, table form).
pub const CAP_ID_MSIX: u8 = 0x11;

/// The PCI `STATUS` register's "capabilities list present" bit (bit 4).
pub const STATUS_CAP_LIST: u16 = 1 << 4;

/// Whether a `STATUS` word advertises a capabilities list.
#[must_use]
pub const fn status_has_caps(status: u16) -> bool {
    status & STATUS_CAP_LIST != 0
}

/// A capability entry's id (its low byte).
#[must_use]
pub const fn cap_id_of(cap_dword: u32) -> u8 {
    (cap_dword & 0xFF) as u8
}

/// A capability entry's "next pointer" (config offset of the next cap, or 0 to
/// end the chain) — the second byte, masked to a dword-aligned offset.
#[must_use]
pub const fn cap_next_of(cap_dword: u32) -> u8 {
    ((cap_dword >> 8) & 0xFC) as u8
}

/// What the kernel discovered from a PCI bus-0 scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PciScan {
    /// Present functions found on bus 0.
    pub functions: u32,
    /// The host bridge (00:00.0) vendor id.
    pub host_vendor: u16,
    /// The host bridge (00:00.0) device id.
    pub host_device: u16,
    /// Mass-storage controllers (class 0x01) on bus 0.
    pub storage: u32,
    /// Network controllers (class 0x02) on bus 0.
    pub network: u32,
    /// Display controllers (class 0x03) on bus 0.
    pub display: u32,
    /// Functions advertising an MSI-X capability (message-signaled interrupts).
    pub msix_capable: u32,
}

impl PciScan {
    /// Fold a present function's class code into the per-class counters.
    #[cfg(any(target_os = "uefi", test))]
    const fn count_class(&mut self, class: u8) {
        match class {
            CLASS_STORAGE => self.storage += 1,
            CLASS_NETWORK => self.network += 1,
            CLASS_DISPLAY => self.display += 1,
            _ => {}
        }
    }
}

#[cfg(target_os = "uefi")]
pub use hw::{scan_bus0, scan_ecam};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        CAP_ID_MSIX, EcamScan, PCI_CONFIG_ADDRESS, PCI_CONFIG_DATA, PciScan, cap_id_of,
        cap_next_of, class_of, config_address, device_of, ecam_config_address, status_has_caps,
        vendor_of, vendor_present,
    };

    /// Write a 32-bit `value` to `port`.
    ///
    /// # Safety
    ///
    /// `port` must be a valid I/O port for a dword write at ring 0.
    unsafe fn outl(port: u16, value: u32) {
        unsafe {
            core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// Read a 32-bit value from `port`.
    ///
    /// # Safety
    ///
    /// `port` must be a valid I/O port for a dword read at ring 0.
    unsafe fn inl(port: u16) -> u32 {
        let value: u32;
        unsafe {
            core::arch::asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Read config dword `offset` of `bus:device.function` via mechanism #1.
    fn config_read(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        // SAFETY: 0xCF8/0xCFC are the architectural PCI config ports; a select
        // then a data read is the mechanism-#1 protocol.
        unsafe {
            outl(
                PCI_CONFIG_ADDRESS,
                config_address(bus, device, function, offset),
            );
            inl(PCI_CONFIG_DATA)
        }
    }

    /// Whether `bus:device.function` advertises an MSI-X capability.
    ///
    /// Walks the capability chain from config `0x34` when the `STATUS` register
    /// (config `0x06`) marks a cap list present, cycle-guarded (a bounded number
    /// of steps) against a malformed chain. Read-only — no config writes.
    fn has_msix(bus: u8, device: u8, function: u8) -> bool {
        /// Config offset of the Command/Status dword (STATUS is the high half).
        const STATUS_DWORD: u8 = 0x04;
        /// Config offset of the capabilities pointer.
        const CAP_PTR_OFFSET: u8 = 0x34;
        /// Bound on the walk (48 * 4 B covers the 256 B legacy config space).
        const MAX_CAPS: u8 = 48;

        let status = (config_read(bus, device, function, STATUS_DWORD) >> 16) as u16;
        if !status_has_caps(status) {
            return false;
        }
        let mut ptr = (config_read(bus, device, function, CAP_PTR_OFFSET) & 0xFC) as u8;
        let mut steps = 0;
        while ptr != 0 && steps < MAX_CAPS {
            let cap = config_read(bus, device, function, ptr);
            if cap_id_of(cap) == CAP_ID_MSIX {
                return true;
            }
            ptr = cap_next_of(cap);
            steps += 1;
        }
        false
    }

    /// Enumerate the present functions on PCI bus 0 and read the host bridge's
    /// identity.
    ///
    /// Probes every device/function on bus 0 by reading config dword 0 (vendor/
    /// device); an all-ones vendor means "absent". Function 0 of each device is
    /// probed first, and functions 1–7 only when device 0's header advertises
    /// multifunction (header-type bit 7) — the standard enumeration that avoids
    /// phantom functions. The host bridge is 00:00.0.
    #[must_use]
    pub fn scan_bus0() -> PciScan {
        /// Config offset of the header-type byte (bits: 7 = multifunction).
        const HEADER_TYPE_OFFSET: u8 = 0x0C;
        const HEADER_MULTIFUNCTION: u32 = 0x0080_0000; // bit 23 (byte 0x0E)
        /// Config offset of the class-code dword (revision/prog-if/subclass/class).
        const CLASS_OFFSET: u8 = 0x08;

        let mut scan = PciScan::default();
        let host = config_read(0, 0, 0, 0);
        scan.host_vendor = vendor_of(host);
        scan.host_device = device_of(host);

        for device in 0u8..32 {
            let dword0 = config_read(0, device, 0, 0);
            if !vendor_present(vendor_of(dword0)) {
                continue;
            }
            scan.functions += 1;
            scan.count_class(class_of(config_read(0, device, 0, CLASS_OFFSET)));
            if has_msix(0, device, 0) {
                scan.msix_capable += 1;
            }
            // Probe the other functions only if device 0 is multifunction.
            let multifunction =
                config_read(0, device, 0, HEADER_TYPE_OFFSET) & HEADER_MULTIFUNCTION != 0;
            if multifunction {
                for function in 1u8..8 {
                    if vendor_present(vendor_of(config_read(0, device, function, 0))) {
                        scan.functions += 1;
                        scan.count_class(class_of(config_read(0, device, function, CLASS_OFFSET)));
                        if has_msix(0, device, function) {
                            scan.msix_capable += 1;
                        }
                    }
                }
            }
        }
        scan
    }

    /// Read config dword `offset` of `bus:device.function` through the ECAM
    /// window at `ecam_base` (MMIO).
    ///
    /// # Safety
    ///
    /// `ecam_base` must be the firmware ECAM base and the addressed dword must
    /// lie in a mapped ECAM page (covered by the kernel's identity map).
    unsafe fn ecam_read(ecam_base: u64, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        let addr = ecam_config_address(ecam_base, bus, device, function, offset);
        // SAFETY: the caller guarantees a mapped ECAM dword; config reads have no
        // side effects and absent functions read back all-ones.
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    /// Enumerate every present function across buses `0..=end_bus` through the
    /// `ECAM` window — the full topology the legacy bus-0 scan cannot reach.
    ///
    /// Probes function 0 of each device, then functions 1–7 only when device 0
    /// is multifunction. Counts present functions and the buses that carry any.
    ///
    /// # Safety
    ///
    /// `ecam_base` must be the firmware ECAM base, its window identity-mapped by
    /// the kernel's page tables (`< 4 GiB` on this layout).
    #[must_use]
    pub unsafe fn scan_ecam(ecam_base: u64, end_bus: u8) -> EcamScan {
        const HEADER_TYPE_OFFSET: u16 = 0x0C;
        const HEADER_MULTIFUNCTION: u32 = 0x0080_0000;

        let mut scan = EcamScan::default();
        for bus in 0..=end_bus {
            let mut bus_used = false;
            for device in 0u8..32 {
                // SAFETY: caller guarantees the ECAM window is mapped.
                if !vendor_present(vendor_of(unsafe {
                    ecam_read(ecam_base, bus, device, 0, 0)
                })) {
                    continue;
                }
                scan.functions += 1;
                bus_used = true;
                // SAFETY: same mapped ECAM window.
                let multifunction = unsafe {
                    ecam_read(ecam_base, bus, device, 0, HEADER_TYPE_OFFSET) & HEADER_MULTIFUNCTION
                        != 0
                };
                if multifunction {
                    for function in 1u8..8 {
                        // SAFETY: same mapped ECAM window.
                        if vendor_present(vendor_of(unsafe {
                            ecam_read(ecam_base, bus, device, function, 0)
                        })) {
                            scan.functions += 1;
                        }
                    }
                }
            }
            if bus_used {
                scan.buses_in_use += 1;
            }
        }
        scan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_address_matches_mechanism_1_layout() {
        // Enable bit + bus 0, device 0, function 0, offset 0.
        assert_eq!(config_address(0, 0, 0, 0), 0x8000_0000);
        // bus 1, device 2, function 3, offset 0x10.
        let a = config_address(1, 2, 3, 0x10);
        assert_eq!(a & 0x8000_0000, 0x8000_0000); // enable
        assert_eq!((a >> 16) & 0xFF, 1); // bus
        assert_eq!((a >> 11) & 0x1F, 2); // device
        assert_eq!((a >> 8) & 0x07, 3); // function
        assert_eq!(a & 0xFC, 0x10); // offset (dword-aligned)
    }

    #[test]
    fn offset_is_dword_aligned() {
        // The low two bits of the offset are forced to 0.
        assert_eq!(config_address(0, 0, 0, 0x13) & 0xFF, 0x10);
    }

    #[test]
    fn device_and_function_fields_are_masked() {
        // Over-wide device/function inputs stay within their bit-fields.
        let a = config_address(0, 0xFF, 0xFF, 0);
        assert_eq!((a >> 11) & 0x1F, 0x1F); // device: 5 bits
        assert_eq!((a >> 8) & 0x07, 0x07); // function: 3 bits
    }

    #[test]
    fn vendor_and_device_decode() {
        // dword0 = device:vendor = 0x29C0_8086 (q35 host bridge).
        let dword0 = 0x29C0_8086;
        assert_eq!(vendor_of(dword0), 0x8086);
        assert_eq!(device_of(dword0), 0x29C0);
        assert!(vendor_present(vendor_of(dword0)));
    }

    #[test]
    fn absent_vendor_is_not_present() {
        assert!(!vendor_present(PCI_VENDOR_ABSENT));
        assert_eq!(vendor_of(0xFFFF_FFFF), PCI_VENDOR_ABSENT);
    }

    #[test]
    fn class_of_reads_the_high_byte() {
        // class dword = class:subclass:progif:revision.
        assert_eq!(class_of(0x0106_0001), CLASS_STORAGE); // SATA (01:06)
        assert_eq!(class_of(0x0200_0000), CLASS_NETWORK);
        assert_eq!(class_of(0x0300_0000), CLASS_DISPLAY);
        assert_eq!(class_of(0x0600_0000), 0x06); // host bridge
    }

    #[test]
    fn ecam_address_matches_the_pcie_layout() {
        // base + bus<<20 + dev<<15 + fn<<12 + offset.
        assert_eq!(ecam_config_address(0xE000_0000, 0, 0, 0, 0), 0xE000_0000);
        // bus 1, device 2, function 3, extended offset 0x100.
        assert_eq!(
            ecam_config_address(0xE000_0000, 1, 2, 3, 0x100),
            0xE000_0000 + (1 << 20) + (2 << 15) + (3 << 12) + 0x100
        );
        // Extended config space (offset > 0xFF) is reachable, unlike legacy.
        assert_eq!(
            ecam_config_address(0xE000_0000, 0, 0, 0, 0xFFF) & 0xFFF,
            0xFFF
        );
    }

    #[test]
    fn capability_chain_decode() {
        // STATUS with the cap-list bit set is detected.
        assert!(status_has_caps(STATUS_CAP_LIST | 0x0010));
        assert!(!status_has_caps(0));
        // A cap dword: id in the low byte, next pointer in the second byte.
        let cap = 0x0000_5011; // next 0x50, id 0x11 (MSI-X)
        assert_eq!(cap_id_of(cap), CAP_ID_MSIX);
        assert_eq!(cap_next_of(cap), 0x50);
        // The next pointer is dword-aligned (low two bits dropped).
        assert_eq!(cap_next_of(0x0000_5311), 0x50);
        // A zero next pointer ends the chain.
        assert_eq!(cap_next_of(0x0000_0005), 0);
    }

    #[test]
    fn count_class_tallies_by_base_class() {
        let mut scan = PciScan::default();
        scan.count_class(CLASS_STORAGE);
        scan.count_class(CLASS_STORAGE);
        scan.count_class(CLASS_NETWORK);
        scan.count_class(0x06); // bridge — not tallied
        assert_eq!(scan.storage, 2);
        assert_eq!(scan.network, 1);
        assert_eq!(scan.display, 0);
    }
}
