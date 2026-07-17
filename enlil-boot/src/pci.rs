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

/// What the kernel discovered from a PCI bus-0 scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PciScan {
    /// Present functions found on bus 0.
    pub functions: u32,
    /// The host bridge (00:00.0) vendor id.
    pub host_vendor: u16,
    /// The host bridge (00:00.0) device id.
    pub host_device: u16,
}

#[cfg(target_os = "uefi")]
pub use hw::scan_bus0;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        PCI_CONFIG_ADDRESS, PCI_CONFIG_DATA, PciScan, config_address, device_of, vendor_of,
        vendor_present,
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
            // Probe the other functions only if device 0 is multifunction.
            let multifunction =
                config_read(0, device, 0, HEADER_TYPE_OFFSET) & HEADER_MULTIFUNCTION != 0;
            if multifunction {
                for function in 1u8..8 {
                    if vendor_present(vendor_of(config_read(0, device, function, 0))) {
                        scan.functions += 1;
                    }
                }
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
}
