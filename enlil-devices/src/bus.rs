//! Device bus abstractions.
//!
//! Dispatches PIO (port I/O) and MMIO (memory-mapped I/O) accesses from guest
//! VM exits to the virtual device that owns the target address range.
//!
//! A guest access is a *byte access* of 1, 2 or 4 bytes (PIO) or up to 8 bytes
//! (MMIO). The bus front door ([`PioBus::read`]/[`PioBus::write`],
//! [`MmioBus::read`]/[`MmioBus::write`]) is therefore byte-slice oriented,
//! matching the shape KVM hands us on an exit (and
//! `enlil-core::kvm_backend::VmExitHandler`), so wiring a vCPU run loop to the
//! bus is a direct forward with no second address-decode path. Internally the
//! bus converts to/from the width-oriented [`PioDevice`]/[`MmioDevice`] trait
//! methods (little-endian, matching x86).
//!
//! Devices register over an explicit address *range* `[base, end)`. Lookup finds
//! the device whose base is the greatest `<= addr` and confirms `addr < end`, so
//! an access past a device's range is *not* misrouted to it (the previous routing
//! table stored only the base and had no upper bound). Overlapping registrations
//! are rejected.
//!
//! Unmapped accesses follow the x86 *open-bus* convention: reads return all-ones
//! (`0xFF` per byte) and writes are silently dropped.

use crate::truncate::u8_of;
use std::collections::BTreeMap;

/// Direction of an I/O operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoDirection {
    Read,
    Write,
}

/// A port I/O (PIO) access from a guest.
#[derive(Debug, Clone)]
pub struct PioAccess {
    pub port: u16,
    pub direction: IoDirection,
    pub size: u8,
    pub data: u32,
}

/// A memory-mapped I/O (MMIO) access from a guest.
#[derive(Debug, Clone)]
pub struct MmioAccess {
    pub address: u64,
    pub direction: IoDirection,
    pub size: u8,
    pub data: u64,
}

/// Error returned when registering a device on a bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusError {
    /// The device's declared range was empty (`end <= base`).
    EmptyRange,
    /// The device's range overlaps an already-registered device. Carries the
    /// `[base, end)` of the conflicting resident.
    Overlap { base: u64, end: u64 },
}

impl core::fmt::Display for BusError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::EmptyRange => f.write_str("device declared an empty address range"),
            Self::Overlap { base, end } => {
                write!(f, "address range overlaps device at [{base:#x}, {end:#x})")
            }
        }
    }
}

impl std::error::Error for BusError {}

/// Trait for devices that handle port I/O.
///
/// `port` is the absolute guest port; `size` is the access width in bytes
/// (1/2/4). Reads return the value in the low `size` bytes of the `u32`.
pub trait PioDevice {
    fn pio_read(&mut self, port: u16, size: u8) -> u32;
    fn pio_write(&mut self, port: u16, size: u8, data: u32);
    /// The half-open port range `[base, end)` this device claims.
    fn port_range(&self) -> (u16, u16);
}

/// Trait for devices that handle memory-mapped I/O.
///
/// `offset` is relative to the device's base address; `size` is the access
/// width in bytes (1/2/4/8). Reads return the value in the low `size` bytes of
/// the `u64`.
pub trait MmioDevice {
    fn mmio_read(&mut self, offset: u64, size: u8) -> u64;
    fn mmio_write(&mut self, offset: u64, size: u8, data: u64);
    /// The half-open address range `[base, end)` this device claims.
    fn mmio_range(&self) -> (u64, u64);
}

/// Build a `u32` from the low (up to 4) little-endian bytes of `data`.
fn le_to_u32(data: &[u8]) -> u32 {
    let mut bytes = [0u8; 4];
    let n = data.len().min(4);
    bytes[..n].copy_from_slice(&data[..n]);
    u32::from_le_bytes(bytes)
}

/// Build a `u64` from the low (up to 8) little-endian bytes of `data`.
fn le_to_u64(data: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    let n = data.len().min(8);
    bytes[..n].copy_from_slice(&data[..n]);
    u64::from_le_bytes(bytes)
}

/// Copy the little-endian bytes of a read result into the guest's buffer,
/// stopping at whichever of the two is shorter (the access width).
fn fill_le(data: &mut [u8], le_bytes: &[u8]) {
    for (dst, &src) in data.iter_mut().zip(le_bytes.iter()) {
        *dst = src;
    }
}

/// A registered PIO device plus the exclusive upper bound of its range.
struct PioEntry {
    /// Exclusive end of the claimed range.
    end: u16,
    device: Box<dyn PioDevice>,
}

/// PIO bus — owns devices and routes port I/O to the one covering each port.
#[derive(Default)]
pub struct PioBus {
    /// Maps each device's base port to its entry.
    devices: BTreeMap<u16, PioEntry>,
}

impl PioBus {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
        }
    }

    /// Register `device` over the half-open range it declares via
    /// [`PioDevice::port_range`].
    ///
    /// # Errors
    /// Returns [`BusError::EmptyRange`] if the declared range is empty, or
    /// [`BusError::Overlap`] if it intersects an already-registered device.
    pub fn register(&mut self, device: Box<dyn PioDevice>) -> Result<(), BusError> {
        let (base, end) = device.port_range();
        if end <= base {
            return Err(BusError::EmptyRange);
        }
        // The device immediately at or below `base` must end at or before it.
        if let Some((_, prev)) = self.devices.range(..=base).next_back()
            && prev.end > base
        {
            return Err(BusError::Overlap {
                base: u64::from(base),
                end: u64::from(prev.end),
            });
        }
        // The next device above `base` must start at or after our `end`.
        if let Some((&next_base, next)) = self.devices.range(base..).next()
            && next_base < end
        {
            return Err(BusError::Overlap {
                base: u64::from(next_base),
                end: u64::from(next.end),
            });
        }
        self.devices.insert(base, PioEntry { end, device });
        Ok(())
    }

    /// Whether some registered device covers `port`.
    #[must_use]
    pub fn is_mapped(&self, port: u16) -> bool {
        self.devices
            .range(..=port)
            .next_back()
            .is_some_and(|(_, e)| port < e.end)
    }

    fn lookup_mut(&mut self, port: u16) -> Option<&mut PioEntry> {
        let (_, entry) = self.devices.range_mut(..=port).next_back()?;
        (port < entry.end).then_some(entry)
    }

    /// Service a guest port read of `data.len()` bytes, filling `data` in place.
    /// Unmapped ports read as all-ones (open bus).
    pub fn read(&mut self, port: u16, data: &mut [u8]) {
        if let Some(entry) = self.lookup_mut(port) {
            let value = entry.device.pio_read(port, u8_of(data.len()));
            fill_le(data, &value.to_le_bytes());
        } else {
            data.fill(0xFF);
        }
    }

    /// Service a guest port write. Writes to unmapped ports are dropped.
    pub fn write(&mut self, port: u16, data: &[u8]) {
        if let Some(entry) = self.lookup_mut(port) {
            entry
                .device
                .pio_write(port, u8_of(data.len()), le_to_u32(data));
        }
    }
}

/// A registered MMIO device plus the exclusive upper bound of its range.
struct MmioEntry {
    /// Exclusive end of the claimed range.
    end: u64,
    device: Box<dyn MmioDevice>,
}

/// MMIO bus — owns devices and routes memory-mapped I/O to the one covering
/// each address.
#[derive(Default)]
pub struct MmioBus {
    /// Maps each device's base address to its entry.
    devices: BTreeMap<u64, MmioEntry>,
}

impl MmioBus {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
        }
    }

    /// Register `device` over the half-open range it declares via
    /// [`MmioDevice::mmio_range`].
    ///
    /// # Errors
    /// Returns [`BusError::EmptyRange`] if the declared range is empty, or
    /// [`BusError::Overlap`] if it intersects an already-registered device.
    pub fn register(&mut self, device: Box<dyn MmioDevice>) -> Result<(), BusError> {
        let (base, end) = device.mmio_range();
        if end <= base {
            return Err(BusError::EmptyRange);
        }
        if let Some((_, prev)) = self.devices.range(..=base).next_back()
            && prev.end > base
        {
            return Err(BusError::Overlap {
                base,
                end: prev.end,
            });
        }
        if let Some((&next_base, next)) = self.devices.range(base..).next()
            && next_base < end
        {
            return Err(BusError::Overlap {
                base: next_base,
                end: next.end,
            });
        }
        self.devices.insert(base, MmioEntry { end, device });
        Ok(())
    }

    /// Whether some registered device covers `address`.
    #[must_use]
    pub fn is_mapped(&self, address: u64) -> bool {
        self.devices
            .range(..=address)
            .next_back()
            .is_some_and(|(_, e)| address < e.end)
    }

    fn lookup_mut(&mut self, address: u64) -> Option<(u64, &mut MmioEntry)> {
        let (&base, entry) = self.devices.range_mut(..=address).next_back()?;
        (address < entry.end).then_some((base, entry))
    }

    /// Service a guest MMIO read of `data.len()` bytes, filling `data` in place.
    /// Unmapped addresses read as all-ones (open bus).
    pub fn read(&mut self, address: u64, data: &mut [u8]) {
        if let Some((base, entry)) = self.lookup_mut(address) {
            let value = entry.device.mmio_read(address - base, u8_of(data.len()));
            fill_le(data, &value.to_le_bytes());
        } else {
            data.fill(0xFF);
        }
    }

    /// Service a guest MMIO write. Writes to unmapped addresses are dropped.
    pub fn write(&mut self, address: u64, data: &[u8]) {
        if let Some((base, entry)) = self.lookup_mut(address) {
            entry
                .device
                .mmio_write(address - base, u8_of(data.len()), le_to_u64(data));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BusError, MmioBus, MmioDevice, PioBus, PioDevice};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// One recorded access: `(addr_or_offset, size, data)` (`data` is 0 for reads).
    type Access = (u64, u8, u64);

    /// A test device that logs every access into a shared `Vec` (so the log is
    /// still inspectable after the device is moved into a bus) and returns a
    /// fixed pattern on read.
    struct Recorder {
        base: u64,
        len: u64,
        read_value: u64,
        reads: Rc<RefCell<Vec<Access>>>,
        writes: Rc<RefCell<Vec<Access>>>,
    }

    impl Recorder {
        fn new(base: u64, len: u64, read_value: u64) -> Self {
            Self {
                base,
                len,
                read_value,
                reads: Rc::new(RefCell::new(Vec::new())),
                writes: Rc::new(RefCell::new(Vec::new())),
            }
        }

        /// A clone of the write log that stays live after the device is boxed
        /// and handed to a bus.
        fn write_log(&self) -> Rc<RefCell<Vec<Access>>> {
            Rc::clone(&self.writes)
        }
    }

    impl PioDevice for Recorder {
        fn pio_read(&mut self, port: u16, size: u8) -> u32 {
            self.reads.borrow_mut().push((u64::from(port), size, 0));
            u32::from_le_bytes(self.read_value.to_le_bytes()[..4].try_into().unwrap())
        }
        fn pio_write(&mut self, port: u16, size: u8, data: u32) {
            self.writes
                .borrow_mut()
                .push((u64::from(port), size, u64::from(data)));
        }
        fn port_range(&self) -> (u16, u16) {
            (
                u16::try_from(self.base).unwrap(),
                u16::try_from(self.base + self.len).unwrap(),
            )
        }
    }

    impl MmioDevice for Recorder {
        fn mmio_read(&mut self, offset: u64, size: u8) -> u64 {
            self.reads.borrow_mut().push((offset, size, 0));
            self.read_value
        }
        fn mmio_write(&mut self, offset: u64, size: u8, data: u64) {
            self.writes.borrow_mut().push((offset, size, data));
        }
        fn mmio_range(&self) -> (u64, u64) {
            (self.base, self.base + self.len)
        }
    }

    #[test]
    fn pio_write_reaches_owning_device_with_le_data() {
        let mut bus = PioBus::new();
        let dev = Recorder::new(0x3F8, 8, 0);
        let writes = dev.write_log();
        bus.register(Box::new(dev)).unwrap();

        bus.write(0x3F8, b"OK"); // 0x4B4F little-endian
        assert_eq!(&*writes.borrow(), &[(0x3F8, 2, 0x4B4F)]);
    }

    #[test]
    fn pio_read_returns_device_value_little_endian() {
        let mut bus = PioBus::new();
        bus.register(Box::new(Recorder::new(0x60, 4, 0x1234_5678)))
            .unwrap();

        let mut buf = [0u8; 4];
        bus.read(0x60, &mut buf);
        assert_eq!(buf, [0x78, 0x56, 0x34, 0x12]);

        // A 1-byte read takes only the low byte.
        let mut one = [0u8; 1];
        bus.read(0x60, &mut one);
        assert_eq!(one, [0x78]);
    }

    #[test]
    fn unmapped_pio_is_open_bus() {
        let mut bus = PioBus::new();
        bus.register(Box::new(Recorder::new(0x3F8, 8, 0))).unwrap();

        // Below, above, and just past the device's range all read as 0xFF.
        let mut buf = [0u8; 2];
        bus.read(0x70, &mut buf);
        assert_eq!(buf, [0xFF, 0xFF]);
        bus.read(0x400, &mut buf);
        assert_eq!(buf, [0xFF, 0xFF]);
        // 0x3F8 + 8 == 0x400 is the exclusive end -> not owned.
        assert!(!bus.is_mapped(0x400));
        assert!(bus.is_mapped(0x3FF));
        // Writes to unmapped ports must not panic.
        bus.write(0x70, &[1]);
    }

    #[test]
    fn pio_overlap_is_rejected() {
        let mut bus = PioBus::new();
        bus.register(Box::new(Recorder::new(0x3F8, 8, 0))).unwrap();
        // Starts inside the first device's range.
        let err = bus
            .register(Box::new(Recorder::new(0x3FF, 8, 0)))
            .unwrap_err();
        assert!(matches!(err, BusError::Overlap { .. }));
        // Straddles from below into the first device.
        let err = bus
            .register(Box::new(Recorder::new(0x3F0, 16, 0)))
            .unwrap_err();
        assert!(matches!(err, BusError::Overlap { .. }));
        // Adjacent (ends exactly at 0x3F8) is fine.
        bus.register(Box::new(Recorder::new(0x3F0, 8, 0))).unwrap();
    }

    #[test]
    fn mmio_dispatch_uses_offset_and_width() {
        let mut bus = MmioBus::new();
        let dev = Recorder::new(0xFEE0_0000, 0x1000, 0xDEAD_BEEF_CAFE_BABE);
        let writes = dev.write_log();
        bus.register(Box::new(dev)).unwrap();

        // 8-byte read returns the full value, little-endian.
        let mut buf = [0u8; 8];
        bus.read(0xFEE0_0000, &mut buf);
        assert_eq!(buf, 0xDEAD_BEEF_CAFE_BABE_u64.to_le_bytes());

        // A write at base + 0x20 must reach the device as offset 0x20, width 4.
        bus.write(0xFEE0_0020, &0x1122_3344_u32.to_le_bytes());
        assert_eq!(&*writes.borrow(), &[(0x20, 4, 0x1122_3344)]);

        // The last byte of the range is owned; the exclusive end is not.
        assert!(bus.is_mapped(0xFEE0_0FFF));
        assert!(!bus.is_mapped(0xFEE0_1000));
    }

    #[test]
    fn mmio_empty_range_is_rejected() {
        let mut bus = MmioBus::new();
        let err = bus
            .register(Box::new(Recorder::new(0x1000, 0, 0)))
            .unwrap_err();
        assert_eq!(err, BusError::EmptyRange);
    }
}
