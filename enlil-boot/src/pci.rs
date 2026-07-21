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

/// Config offset of Base Address Register 0. BARs 0–5 are the six dwords at
/// `0x10, 0x14, … 0x24`.
pub const BAR0_OFFSET: u8 = 0x10;

/// BAR bit 0 — address space: 1 = I/O space, 0 = memory space.
pub const BAR_SPACE_IO: u32 = 1 << 0;

/// BAR memory-type field (bits 2:1) mask.
pub const BAR_MEM_TYPE_MASK: u32 = 0b11 << 1;

/// BAR memory type 10b — a 64-bit BAR (its high half is the next dword).
pub const BAR_MEM_TYPE_64: u32 = 0b10 << 1;

/// Whether a BAR maps I/O space (bit 0 set) rather than memory.
#[must_use]
pub const fn bar_is_io(bar: u32) -> bool {
    bar & BAR_SPACE_IO != 0
}

/// Whether a BAR is a 64-bit memory BAR (memory space, type field 10b) — its
/// upper 32 bits live in the following BAR dword.
#[must_use]
pub const fn bar_is_mem_64(bar: u32) -> bool {
    bar & BAR_SPACE_IO == 0 && bar & BAR_MEM_TYPE_MASK == BAR_MEM_TYPE_64
}

/// The 32-bit memory base a memory BAR encodes (low 4 flag bits cleared).
#[must_use]
pub const fn bar_mem_base(bar: u32) -> u32 {
    bar & 0xFFFF_FFF0
}

/// Combine a 64-bit BAR's low and high dwords into the full base address.
#[must_use]
pub const fn bar_mem_base_64(low: u32, high: u32) -> u64 {
    ((high as u64) << 32) | (bar_mem_base(low) as u64)
}

/// The I/O port base an I/O BAR encodes (low 2 flag bits cleared).
///
/// x86 I/O ports are 16-bit, so only the low word is meaningful; the mask keeps
/// the cast lossless.
#[must_use]
pub const fn bar_io_base(bar: u32) -> u16 {
    (bar & 0xFFFC) as u16
}

/// The size of a 32-bit memory BAR from its all-ones write-probe read-back.
///
/// After writing `0xFFFF_FFFF` to a BAR, the address bits read back as
/// `~(size − 1)`; the region size is `~(readback & mask) + 1` (PCI spec §6.2.5.1).
/// The low 4 flag bits are masked off first. Returns 0 for an unimplemented BAR
/// (reads back 0 after masking).
#[must_use]
pub const fn bar_mem_size(probe_readback: u32) -> u64 {
    let masked = probe_readback & 0xFFFF_FFF0;
    if masked == 0 { 0 } else { (!masked as u64) + 1 }
}

/// The size of a 64-bit memory BAR from the all-ones write-probe read-back of
/// its low and high dwords (the 64-bit analogue of [`bar_mem_size`]).
#[must_use]
pub const fn bar_mem_size_64(probe_low: u32, probe_high: u32) -> u64 {
    let masked = ((probe_high as u64) << 32) | ((probe_low & 0xFFFF_FFF0) as u64);
    if masked == 0 {
        0
    } else {
        (!masked).wrapping_add(1)
    }
}

/// A decoded memory BAR located on a bus scan: which function carries it, its
/// base address, size, and whether it is a 64-bit BAR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBar {
    /// PCI bus number.
    pub bus: u8,
    /// PCI device number.
    pub device: u8,
    /// PCI function number.
    pub function: u8,
    /// Which BAR index (0–5) it is.
    pub index: u8,
    /// The decoded base physical address.
    pub base: u64,
    /// The region size in bytes (from the write-probe), 0 if unsized.
    pub size: u64,
    /// Whether it is a 64-bit BAR.
    pub is_64: bool,
}

/// A summary of the memory BARs across PCI bus 0 — the total MMIO footprint.
///
/// The hypervisor must account for it when it routes config space and plans
/// device passthrough (each guest's assigned BARs must land in its own address
/// space).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryBarSummary {
    /// Number of implemented memory BARs found (a 64-bit BAR counts once).
    pub count: u32,
    /// Sum of their region sizes in bytes (from the write-probe; unsized BARs
    /// contribute 0).
    pub total_bytes: u64,
}

impl MemoryBarSummary {
    /// Fold one implemented memory BAR of `size` bytes into the summary.
    #[cfg(any(target_os = "uefi", test))]
    const fn record(&mut self, size: u64) {
        self.count += 1;
        self.total_bytes = self.total_bytes.saturating_add(size);
    }
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
pub use hw::{first_memory_bar, scan_bus0, scan_ecam, scan_memory_bars};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        BAR0_OFFSET, CAP_ID_MSIX, EcamScan, MemoryBar, PCI_CONFIG_ADDRESS, PCI_CONFIG_DATA,
        PciScan, bar_is_io, bar_is_mem_64, bar_mem_base, bar_mem_base_64, bar_mem_size,
        bar_mem_size_64, cap_id_of, cap_next_of, class_of, config_address, device_of,
        ecam_config_address, status_has_caps, vendor_of, vendor_present,
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

    /// Write config dword `offset` of `bus:device.function` via mechanism #1.
    ///
    /// Used only for the BAR write-probe sizing below, which always restores the
    /// original value — config space is never left modified.
    fn config_write(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
        // SAFETY: 0xCF8/0xCFC are the architectural PCI config ports; select
        // then data write is the mechanism-#1 protocol.
        unsafe {
            outl(
                PCI_CONFIG_ADDRESS,
                config_address(bus, device, function, offset),
            );
            outl(PCI_CONFIG_DATA, value);
        }
    }

    /// Size a memory BAR at config `off` by the standard write-probe: save the
    /// BAR, write all-ones, read back the size mask, and **restore the original**.
    ///
    /// For a 64-bit BAR (`is_64`) both dwords are probed and combined. Config
    /// space is left exactly as found.
    fn size_memory_bar(device: u8, function: u8, off: u8, is_64: bool) -> u64 {
        let orig_lo = config_read(0, device, function, off);
        config_write(0, device, function, off, 0xFFFF_FFFF);
        let probe_lo = config_read(0, device, function, off);
        if is_64 {
            let orig_hi = config_read(0, device, function, off + 4);
            config_write(0, device, function, off + 4, 0xFFFF_FFFF);
            let probe_hi = config_read(0, device, function, off + 4);
            // Restore both dwords.
            config_write(0, device, function, off, orig_lo);
            config_write(0, device, function, off + 4, orig_hi);
            bar_mem_size_64(probe_lo, probe_hi)
        } else {
            config_write(0, device, function, off, orig_lo);
            bar_mem_size(probe_lo)
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

    /// Find the first memory BAR on bus 0 (in device/function/BAR-index order)
    /// and decode it — the MMIO window a driver or passthrough would claim.
    ///
    /// Read-only: reads each present function's BARs 0–5 (skipping the second
    /// dword of a 64-bit BAR), returning the first memory BAR with a non-zero
    /// base. A 64-bit BAR's high half is read from the following dword. Returns
    /// `None` if no function on bus 0 exposes a memory BAR (e.g. a bare q35 with
    /// only bridges). No BAR sizing (that needs a write-probe) — just the base.
    #[must_use]
    pub fn first_memory_bar() -> MemoryBar {
        find_first_memory_bar().unwrap_or(MemoryBar {
            bus: 0,
            device: 0,
            function: 0,
            index: 0,
            base: 0,
            size: 0,
            is_64: false,
        })
    }

    /// The inner search — `None` when bus 0 has no memory BAR.
    fn find_first_memory_bar() -> Option<MemoryBar> {
        const HEADER_TYPE_OFFSET: u8 = 0x0C;
        const HEADER_MULTIFUNCTION: u32 = 0x0080_0000;
        /// Only header-type-0 (endpoint) functions have BARs 0–5; a PCI-to-PCI
        /// bridge (header type 1) has just BARs 0–1, but the layout byte below
        /// bounds that. We simply read up to 6 dwords and skip 64-bit high halves.
        const BAR_COUNT: u8 = 6;

        for device in 0u8..32 {
            let dword0 = config_read(0, device, 0, 0);
            if !vendor_present(vendor_of(dword0)) {
                continue;
            }
            let multifunction =
                config_read(0, device, 0, HEADER_TYPE_OFFSET) & HEADER_MULTIFUNCTION != 0;
            let last_fn = if multifunction { 7 } else { 0 };
            for function in 0u8..=last_fn {
                if function != 0 && !vendor_present(vendor_of(config_read(0, device, function, 0)))
                {
                    continue;
                }
                let mut i = 0u8;
                while i < BAR_COUNT {
                    let off = BAR0_OFFSET + i * 4;
                    let bar = config_read(0, device, function, off);
                    if bar_is_io(bar) {
                        i += 1;
                        continue;
                    }
                    if bar_is_mem_64(bar) {
                        let high = config_read(0, device, function, off + 4);
                        let base = bar_mem_base_64(bar, high);
                        if base != 0 {
                            return Some(MemoryBar {
                                bus: 0,
                                device,
                                function,
                                index: i,
                                base,
                                size: size_memory_bar(device, function, off, true),
                                is_64: true,
                            });
                        }
                        i += 2; // a 64-bit BAR consumes this dword and the next
                    } else {
                        let base = bar_mem_base(bar);
                        if base != 0 {
                            return Some(MemoryBar {
                                bus: 0,
                                device,
                                function,
                                index: i,
                                base: u64::from(base),
                                size: size_memory_bar(device, function, off, false),
                                is_64: false,
                            });
                        }
                        i += 1;
                    }
                }
            }
        }
        None
    }

    /// Summarize every implemented memory BAR across PCI bus 0 — their count and
    /// total region size (the MMIO footprint the hypervisor must account for).
    ///
    /// Walks each present function's BARs 0–5 (following the multifunction bit),
    /// skips I/O BARs and unimplemented (base 0) BARs, sizes each memory BAR via
    /// the write-probe, and pairs a 64-bit BAR with its high dword (counted
    /// once). The read-only companion to [`first_memory_bar`] that reports the
    /// whole bus's MMIO rather than the first window.
    #[must_use]
    pub fn scan_memory_bars() -> super::MemoryBarSummary {
        const HEADER_TYPE_OFFSET: u8 = 0x0C;
        const HEADER_MULTIFUNCTION: u32 = 0x0080_0000;
        const BAR_COUNT: u8 = 6;

        let mut summary = super::MemoryBarSummary::default();
        for device in 0u8..32 {
            let dword0 = config_read(0, device, 0, 0);
            if !vendor_present(vendor_of(dword0)) {
                continue;
            }
            let multifunction =
                config_read(0, device, 0, HEADER_TYPE_OFFSET) & HEADER_MULTIFUNCTION != 0;
            let last_fn = if multifunction { 7 } else { 0 };
            for function in 0u8..=last_fn {
                if function != 0 && !vendor_present(vendor_of(config_read(0, device, function, 0)))
                {
                    continue;
                }
                let mut i = 0u8;
                while i < BAR_COUNT {
                    let off = BAR0_OFFSET + i * 4;
                    let bar = config_read(0, device, function, off);
                    if bar_is_io(bar) {
                        i += 1;
                        continue;
                    }
                    if bar_is_mem_64(bar) {
                        let high = config_read(0, device, function, off + 4);
                        if bar_mem_base_64(bar, high) != 0 {
                            summary.record(size_memory_bar(device, function, off, true));
                        }
                        i += 2; // a 64-bit BAR consumes this dword and the next
                    } else {
                        if bar_mem_base(bar) != 0 {
                            summary.record(size_memory_bar(device, function, off, false));
                        }
                        i += 1;
                    }
                }
            }
        }
        summary
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
    fn bar_decodes_io_memory_and_width() {
        // A 32-bit prefetchable memory BAR at 0xFE00_0000 (flags in low bits).
        let mem32 = 0xFE00_0008; // bit 3 prefetchable, type 00 (32-bit), mem
        assert!(!bar_is_io(mem32));
        assert!(!bar_is_mem_64(mem32));
        assert_eq!(bar_mem_base(mem32), 0xFE00_0000);
        // A 64-bit memory BAR (type 10b) with a high dword.
        let mem64_lo = 0xF000_000C; // type 10b (bits 2:1 = 10), mem
        assert!(bar_is_mem_64(mem64_lo));
        assert_eq!(bar_mem_base_64(mem64_lo, 0x0000_0001), 0x1_F000_0000);
        // An I/O BAR (bit 0 set).
        let io = 0x0000_C001;
        assert!(bar_is_io(io));
        assert_eq!(bar_io_base(io), 0xC000);
    }

    #[test]
    fn bar_size_from_write_probe() {
        // A 16 MiB BAR reads back ~(16 MiB - 1) = 0xFF00_0000 in the addr bits.
        assert_eq!(bar_mem_size(0xFF00_0000), 16 * 1024 * 1024);
        // A 256-byte BAR: ~(0xFF) = 0xFFFF_FF00 → 0x100.
        assert_eq!(bar_mem_size(0xFFFF_FF00), 0x100);
        // An unimplemented BAR (reads back 0) has size 0.
        assert_eq!(bar_mem_size(0), 0);
        // Low flag bits are ignored (prefetchable/type bits set → same size).
        assert_eq!(bar_mem_size(0xFF00_000C), 16 * 1024 * 1024);
        // 64-bit: high dword extends the mask. 8 GiB region.
        // ~(8 GiB - 1) = 0xFFFF_FFFE_0000_0000 → low 0x0000_0000, high 0xFFFF_FFFE.
        assert_eq!(
            bar_mem_size_64(0x0000_0000, 0xFFFF_FFFE),
            8 * 1024 * 1024 * 1024
        );
        assert_eq!(bar_mem_size_64(0, 0), 0);
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

    #[test]
    fn memory_bar_summary_accumulates_count_and_total() {
        let mut s = MemoryBarSummary::default();
        assert_eq!((s.count, s.total_bytes), (0, 0));
        s.record(16 * 1024 * 1024);
        s.record(0x100);
        s.record(0); // an unsized BAR contributes 0 bytes but is still counted
        assert_eq!(s.count, 3);
        assert_eq!(s.total_bytes, 16 * 1024 * 1024 + 0x100);
        // The total saturates rather than overflowing on a pathological size.
        s.record(u64::MAX);
        assert_eq!(s.total_bytes, u64::MAX);
        assert_eq!(s.count, 4);
    }
}
