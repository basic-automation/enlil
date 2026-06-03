//! Device bus abstractions.
//!
//! Dispatches PIO (port I/O) and MMIO (memory-mapped I/O) accesses from guest
//! VM exits to the appropriate virtual device handler. Modelled on rust-vmm's
//! `vm-device` `IoManager`: separate PIO and MMIO buses, devices registered
//! over an address *range*, and every access decoded against the full
//! `[base, base + len)` span (not just a lower-bound check) before dispatch.
//!
//! The byte-slice read/write shape mirrors `enlil-core`'s `VmExitHandler`, so
//! the KVM run loop can forward an exit straight to the bus without a parallel
//! address-decode path.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};

/// Direction of an I/O operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoDirection {
    /// Guest is reading from the device.
    Read,
    /// Guest is writing to the device.
    Write,
}

/// Error returned when registering a device on a bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BusError {
    /// The requested range intersects an already-registered device.
    #[error("device range [{base:#x}, {end:#x}) overlaps an existing device")]
    Overlap {
        /// Inclusive start of the rejected range.
        base: u64,
        /// Exclusive end of the rejected range.
        end: u64,
    },
    /// A device was registered with a zero-length range.
    #[error("device range at {base:#x} has zero length")]
    ZeroLength {
        /// Start of the rejected (empty) range.
        base: u64,
    },
}

/// A device that responds to port-I/O (PIO) accesses.
///
/// `offset` is the access address relative to the device's registered base, so
/// the device never needs to know where it was mapped. `data`'s length is the
/// access width (1/2/4 bytes for legacy x86 ports): on a read the device fills
/// it, on a write the device consumes it.
pub trait PioDevice {
    /// Guest read `data.len()` bytes at `offset`; fill `data`.
    fn pio_read(&mut self, offset: u16, data: &mut [u8]);
    /// Guest wrote `data` at `offset`.
    fn pio_write(&mut self, offset: u16, data: &[u8]);
}

/// A device that responds to memory-mapped-I/O (MMIO) accesses.
pub trait MmioDevice {
    /// Guest read `data.len()` bytes at `offset`; fill `data`.
    fn mmio_read(&mut self, offset: u64, data: &mut [u8]);
    /// Guest wrote `data` at `offset`.
    fn mmio_write(&mut self, offset: u64, data: &[u8]);
}

/// A registered PIO device and the length of its port window.
struct PioRegion {
    len: u16,
    device: Box<dyn PioDevice>,
}

/// PIO bus — routes port I/O to devices registered over `[base, base + len)`.
#[derive(Default)]
pub struct PioBus {
    /// Maps each device's base port to its window and handler.
    regions: BTreeMap<u16, PioRegion>,
}

impl PioBus {
    /// Create an empty PIO bus.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            regions: BTreeMap::new(),
        }
    }

    /// Register `device` over the port window `[base, base + len)`.
    ///
    /// # Errors
    /// Returns [`BusError::ZeroLength`] if `len == 0`, or [`BusError::Overlap`]
    /// if the window intersects an already-registered device.
    pub fn register(
        &mut self,
        base: u16,
        len: u16,
        device: Box<dyn PioDevice>,
    ) -> Result<(), BusError> {
        if len == 0 {
            return Err(BusError::ZeroLength {
                base: u64::from(base),
            });
        }
        // Port space is 16-bit; widen so `base + len` cannot overflow.
        let end = u32::from(base) + u32::from(len);
        let overlap = || BusError::Overlap {
            base: u64::from(base),
            end: u64::from(end),
        };
        // A region starting at or before `base` must end at or before `base`.
        if let Some((&start, region)) = self.regions.range(..=base).next_back()
            && u32::from(start) + u32::from(region.len) > u32::from(base)
        {
            return Err(overlap());
        }
        // The next region after `base` must start at or after `end`.
        if let Some((&start, _)) = self.regions.range((Excluded(base), Unbounded)).next()
            && u32::from(start) < end
        {
            return Err(overlap());
        }
        self.regions.insert(base, PioRegion { len, device });
        Ok(())
    }

    /// Route a read at `port` to its device, filling `data`.
    ///
    /// Returns `true` if a device handled the access; on `false` no device
    /// owns `port` and `data` is left untouched (the caller decides the
    /// unmapped-read value, conventionally all-ones).
    #[must_use]
    pub fn read(&mut self, port: u16, data: &mut [u8]) -> bool {
        if let Some((&base, region)) = self.regions.range_mut(..=port).next_back()
            && u32::from(port) < u32::from(base) + u32::from(region.len)
        {
            region.device.pio_read(port - base, data);
            return true;
        }
        false
    }

    /// Route a write of `data` at `port` to its device.
    ///
    /// Returns `true` if a device handled the access, `false` if `port` is
    /// unmapped (the write is dropped).
    #[must_use]
    pub fn write(&mut self, port: u16, data: &[u8]) -> bool {
        if let Some((&base, region)) = self.regions.range_mut(..=port).next_back()
            && u32::from(port) < u32::from(base) + u32::from(region.len)
        {
            region.device.pio_write(port - base, data);
            return true;
        }
        false
    }
}

/// A registered MMIO device and the length of its address window.
struct MmioRegion {
    len: u64,
    device: Box<dyn MmioDevice>,
}

/// MMIO bus — routes memory-mapped I/O to devices over `[base, base + len)`.
#[derive(Default)]
pub struct MmioBus {
    /// Maps each device's base address to its window and handler.
    regions: BTreeMap<u64, MmioRegion>,
}

impl MmioBus {
    /// Create an empty MMIO bus.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            regions: BTreeMap::new(),
        }
    }

    /// Register `device` over the address window `[base, base + len)`.
    ///
    /// # Errors
    /// Returns [`BusError::ZeroLength`] if `len == 0`, or [`BusError::Overlap`]
    /// if the window intersects an already-registered device.
    pub fn register(
        &mut self,
        base: u64,
        len: u64,
        device: Box<dyn MmioDevice>,
    ) -> Result<(), BusError> {
        if len == 0 {
            return Err(BusError::ZeroLength { base });
        }
        // Compute the exclusive end in u128 so the top of the 64-bit address
        // space cannot wrap.
        let end = u128::from(base) + u128::from(len);
        let overlap = || BusError::Overlap {
            base,
            // Saturate for the error message only; the real check uses `end`.
            end: u64::try_from(end).unwrap_or(u64::MAX),
        };
        if let Some((&start, region)) = self.regions.range(..=base).next_back()
            && u128::from(start) + u128::from(region.len) > u128::from(base)
        {
            return Err(overlap());
        }
        if let Some((&start, _)) = self.regions.range((Excluded(base), Unbounded)).next()
            && u128::from(start) < end
        {
            return Err(overlap());
        }
        self.regions.insert(base, MmioRegion { len, device });
        Ok(())
    }

    /// Route a read at `addr` to its device, filling `data`.
    ///
    /// Returns `true` if a device handled the access; on `false` no device
    /// owns `addr` and `data` is left untouched.
    #[must_use]
    pub fn read(&mut self, addr: u64, data: &mut [u8]) -> bool {
        if let Some((&base, region)) = self.regions.range_mut(..=addr).next_back()
            && u128::from(addr) < u128::from(base) + u128::from(region.len)
        {
            region.device.mmio_read(addr - base, data);
            return true;
        }
        false
    }

    /// Route a write of `data` at `addr` to its device.
    ///
    /// Returns `true` if a device handled the access, `false` if `addr` is
    /// unmapped (the write is dropped).
    #[must_use]
    pub fn write(&mut self, addr: u64, data: &[u8]) -> bool {
        if let Some((&base, region)) = self.regions.range_mut(..=addr).next_back()
            && u128::from(addr) < u128::from(base) + u128::from(region.len)
        {
            region.device.mmio_write(addr - base, data);
            return true;
        }
        false
    }
}

/// A complete device bus: a PIO bus and an MMIO bus.
///
/// This is the type the VMM run loop drives — `enlil-core` implements its
/// `VmExitHandler` for `Bus`, forwarding `io_in`/`io_out` to [`Bus::pio`] and
/// `mmio_read`/`mmio_write` to [`Bus::mmio`].
#[derive(Default)]
pub struct Bus {
    /// Port-I/O bus (x86 16-bit port space).
    pub pio: PioBus,
    /// Memory-mapped-I/O bus (64-bit physical address space).
    pub mmio: MmioBus,
}

impl Bus {
    /// Create an empty bus with no devices registered.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pio: PioBus::new(),
            mmio: MmioBus::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bus, BusError, MmioDevice, PioDevice};

    /// A tiny device that records writes and returns a fixed pattern on read,
    /// used to prove the bus routes to the right device at the right offset.
    #[derive(Default)]
    struct ScratchDevice {
        writes: Vec<(u64, Vec<u8>)>,
    }

    impl PioDevice for ScratchDevice {
        fn pio_read(&mut self, offset: u16, data: &mut [u8]) {
            data.fill(u8::try_from(offset & 0xff).unwrap_or(0));
        }
        fn pio_write(&mut self, offset: u16, data: &[u8]) {
            self.writes.push((u64::from(offset), data.to_vec()));
        }
    }

    impl MmioDevice for ScratchDevice {
        fn mmio_read(&mut self, offset: u64, data: &mut [u8]) {
            data.fill(u8::try_from(offset & 0xff).unwrap_or(0));
        }
        fn mmio_write(&mut self, offset: u64, data: &[u8]) {
            self.writes.push((offset, data.to_vec()));
        }
    }

    #[test]
    fn pio_routes_within_range_and_translates_offset() {
        let mut bus = Bus::new();
        // COM1-style 8-port window.
        bus.pio
            .register(0x3f8, 8, Box::new(ScratchDevice::default()))
            .unwrap();

        // Write to the THR (offset 0) and to LSR (offset 5).
        assert!(bus.pio.write(0x3f8, b"A"));
        assert!(bus.pio.write(0x3fd, b"Z"));

        // A read inside the window returns the offset pattern.
        let mut buf = [0u8; 1];
        assert!(bus.pio.read(0x3fd, &mut buf));
        assert_eq!(buf[0], 0x05);

        // Just past the window is unmapped: not handled, buffer untouched.
        let mut miss = [0xaau8; 1];
        assert!(!bus.pio.read(0x400, &mut miss));
        assert_eq!(miss[0], 0xaa);
        // And below the window is unmapped too.
        assert!(!bus.pio.write(0x3f7, b"!"));
    }

    #[test]
    fn pio_rejects_overlap_and_zero_length() {
        let mut bus = Bus::new();
        bus.pio
            .register(0x60, 4, Box::new(ScratchDevice::default()))
            .unwrap();

        // Overlaps the existing [0x60, 0x64) window from below and above.
        assert_eq!(
            bus.pio
                .register(0x62, 4, Box::new(ScratchDevice::default()))
                .unwrap_err(),
            BusError::Overlap {
                base: 0x62,
                end: 0x66
            }
        );
        assert_eq!(
            bus.pio
                .register(0x5e, 4, Box::new(ScratchDevice::default()))
                .unwrap_err(),
            BusError::Overlap {
                base: 0x5e,
                end: 0x62
            }
        );
        // Zero length is rejected.
        assert_eq!(
            bus.pio
                .register(0x70, 0, Box::new(ScratchDevice::default()))
                .unwrap_err(),
            BusError::ZeroLength { base: 0x70 }
        );
        // Abutting (no overlap) is allowed.
        assert!(
            bus.pio
                .register(0x64, 4, Box::new(ScratchDevice::default()))
                .is_ok()
        );
    }

    #[test]
    fn mmio_routes_within_range_and_rejects_overlap() {
        let mut bus = Bus::new();
        // A 4 KiB MMIO window (e.g. an LAPIC-style page).
        bus.mmio
            .register(0xfee0_0000, 0x1000, Box::new(ScratchDevice::default()))
            .unwrap();

        assert!(bus.mmio.write(0xfee0_0030, &[1, 2, 3, 4]));
        let mut buf = [0u8; 4];
        assert!(bus.mmio.read(0xfee0_00f0, &mut buf));
        assert_eq!(buf, [0xf0, 0xf0, 0xf0, 0xf0]);

        // One byte past the page is unmapped.
        let mut miss = [7u8; 4];
        assert!(!bus.mmio.read(0xfee0_1000, &mut miss));
        assert_eq!(miss, [7, 7, 7, 7]);

        assert_eq!(
            bus.mmio
                .register(0xfee0_0800, 0x1000, Box::new(ScratchDevice::default()))
                .unwrap_err(),
            BusError::Overlap {
                base: 0xfee0_0800,
                end: 0xfee0_1800
            }
        );
    }

    #[test]
    fn mmio_handles_top_of_address_space_without_overflow() {
        let mut bus = Bus::new();
        // A window butting against u64::MAX must not panic on the end calc.
        bus.mmio
            .register(u64::MAX - 0xff, 0x100, Box::new(ScratchDevice::default()))
            .unwrap();
        let mut buf = [0u8; 1];
        assert!(bus.mmio.read(u64::MAX, &mut buf));
    }
}
