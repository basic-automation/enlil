//! Device bus abstractions.
//!
//! Dispatches PIO (port I/O) and MMIO (memory-mapped I/O) accesses from guest
//! VM exits to the appropriate virtual device handler.
//!
//! The [`Bus`] owns its devices (boxed [`PioDevice`] / [`MmioDevice`] trait
//! objects), keyed by the address range each device claims, and routes every
//! access to the device whose range contains the target port/address. Accesses
//! that fall outside every registered range read back all-ones (the value a
//! real x86 bus floats to when nothing drives it) and drop writes, exactly as
//! unmapped hardware would.
//!
//! [`Bus`] also implements [`enlil_core::kvm_backend::VmExitHandler`], so the
//! KVM run loop can hand guest I/O and MMIO exits straight to the device model
//! with no separate decode path. The handler interface is byte-oriented
//! (little-endian, as KVM delivers it); this module converts between those
//! byte buffers and the width-and-value form the device traits use.

use std::collections::BTreeMap;

use enlil_core::kvm_backend::VmExitHandler;

use crate::truncate::{Widen, u8_of};

/// Trait for devices that handle port I/O.
///
/// `port` is the absolute I/O port; `size` is the access width in bytes
/// (1, 2, or 4). Reads return the value in the low `size` bytes of the `u32`.
pub trait PioDevice {
    /// Handle a guest `in` from `port` of `size` bytes.
    fn pio_read(&mut self, port: u16, size: u8) -> u32;
    /// Handle a guest `out` to `port` of `size` bytes carrying `data`.
    fn pio_write(&mut self, port: u16, size: u8, data: u32);
    /// The half-open port range `[base, end)` this device claims.
    fn port_range(&self) -> (u16, u16);
}

/// Trait for devices that handle memory-mapped I/O.
///
/// `offset` is relative to the device's MMIO base; `size` is the access width
/// in bytes (1–8). Reads return the value in the low `size` bytes of the `u64`.
pub trait MmioDevice {
    /// Handle a guest read of `size` bytes at `offset` within this device.
    fn mmio_read(&mut self, offset: u64, size: u8) -> u64;
    /// Handle a guest write of `size` bytes carrying `data` at `offset`.
    fn mmio_write(&mut self, offset: u64, size: u8, data: u64);
    /// The half-open address range `[base, end)` this device claims.
    fn mmio_range(&self) -> (u64, u64);
}

/// Error returned when a device cannot be registered on a [`Bus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// The device's declared range overlaps an already-registered device.
    Overlap {
        /// Start of the offending range.
        base: u64,
        /// End (exclusive) of the offending range.
        end: u64,
    },
    /// The device declared an empty or inverted range (`end <= base`).
    EmptyRange {
        /// Start of the offending range.
        base: u64,
        /// End (exclusive) of the offending range.
        end: u64,
    },
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overlap { base, end } => {
                write!(
                    f,
                    "device range [{base:#x}, {end:#x}) overlaps an existing device"
                )
            }
            Self::EmptyRange { base, end } => {
                write!(f, "device range [{base:#x}, {end:#x}) is empty or inverted")
            }
        }
    }
}

impl std::error::Error for RegisterError {}

/// A registered PIO device and the end of its claimed range.
struct PioEntry {
    /// End (exclusive) of the device's port range.
    end: u16,
    /// The device itself.
    device: Box<dyn PioDevice + Send>,
}

/// A registered MMIO device and the end of its claimed range.
struct MmioEntry {
    /// End (exclusive) of the device's address range.
    end: u64,
    /// The device itself.
    device: Box<dyn MmioDevice + Send>,
}

/// Routes guest PIO and MMIO accesses to the devices that own them.
///
/// Devices are keyed by the base of their claimed range in a [`BTreeMap`], so
/// a lookup is a single `O(log n)` "greatest base ≤ target" query followed by
/// an upper-bound check against that device's range end.
#[derive(Default)]
pub struct Bus {
    /// PIO devices keyed by range base.
    pio: BTreeMap<u16, PioEntry>,
    /// MMIO devices keyed by range base.
    mmio: BTreeMap<u64, MmioEntry>,
}

impl Bus {
    /// Create an empty bus with no devices registered.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pio: BTreeMap::new(),
            mmio: BTreeMap::new(),
        }
    }

    /// Number of PIO devices registered.
    #[must_use]
    pub fn pio_count(&self) -> usize {
        self.pio.len()
    }

    /// Number of MMIO devices registered.
    #[must_use]
    pub fn mmio_count(&self) -> usize {
        self.mmio.len()
    }

    // -- Registration --

    /// Register a PIO device, claiming the port range it reports via
    /// [`PioDevice::port_range`].
    ///
    /// # Errors
    /// Returns [`RegisterError::EmptyRange`] if the device reports `end <= base`,
    /// or [`RegisterError::Overlap`] if its range intersects an already-
    /// registered device.
    pub fn register_pio(&mut self, device: Box<dyn PioDevice + Send>) -> Result<(), RegisterError> {
        let (base, end) = device.port_range();
        if end <= base {
            return Err(RegisterError::EmptyRange {
                base: base.to_u64(),
                end: end.to_u64(),
            });
        }
        let overlaps = self
            .pio
            .iter()
            .any(|(&existing_base, entry)| base < entry.end && existing_base < end);
        if overlaps {
            return Err(RegisterError::Overlap {
                base: base.to_u64(),
                end: end.to_u64(),
            });
        }
        self.pio.insert(base, PioEntry { end, device });
        Ok(())
    }

    /// Register an MMIO device, claiming the address range it reports via
    /// [`MmioDevice::mmio_range`].
    ///
    /// # Errors
    /// Returns [`RegisterError::EmptyRange`] if the device reports `end <= base`,
    /// or [`RegisterError::Overlap`] if its range intersects an already-
    /// registered device.
    pub fn register_mmio(
        &mut self,
        device: Box<dyn MmioDevice + Send>,
    ) -> Result<(), RegisterError> {
        let (base, end) = device.mmio_range();
        if end <= base {
            return Err(RegisterError::EmptyRange { base, end });
        }
        let overlaps = self
            .mmio
            .iter()
            .any(|(&existing_base, entry)| base < entry.end && existing_base < end);
        if overlaps {
            return Err(RegisterError::Overlap { base, end });
        }
        self.mmio.insert(base, MmioEntry { end, device });
        Ok(())
    }

    // -- Device lookup --

    /// The PIO device whose claimed range contains `port`, if any.
    fn pio_device(&mut self, port: u16) -> Option<&mut (dyn PioDevice + Send + 'static)> {
        self.pio
            .range_mut(..=port)
            .next_back()
            .filter(|(_, entry)| port < entry.end)
            .map(|(_, entry)| entry.device.as_mut())
    }

    /// The MMIO device whose claimed range contains `addr`, plus its base
    /// address (so the caller can form the device-relative offset).
    fn mmio_device(&mut self, addr: u64) -> Option<(u64, &mut (dyn MmioDevice + Send + 'static))> {
        self.mmio
            .range_mut(..=addr)
            .next_back()
            .filter(|(_, entry)| addr < entry.end)
            .map(|(&base, entry)| (base, entry.device.as_mut()))
    }

    // -- Byte-oriented dispatch (matches `VmExitHandler`'s interface) --

    /// Service a guest port read, filling `data` (1–4 bytes, little-endian).
    /// Buffers longer than four bytes are treated as repeated byte-wide string
    /// input (`rep insb`) from the same port. Unmapped ports read all-ones.
    pub fn read_pio(&mut self, port: u16, data: &mut [u8]) {
        if data.len() <= 4 {
            self.read_pio_one(port, data);
        } else {
            for byte in data.iter_mut() {
                self.read_pio_one(port, std::slice::from_mut(byte));
            }
        }
    }

    /// Service a guest port write of `data` (1–4 bytes, little-endian).
    /// Buffers longer than four bytes are treated as repeated byte-wide string
    /// output (`rep outsb`) to the same port. Unmapped ports drop the write.
    pub fn write_pio(&mut self, port: u16, data: &[u8]) {
        if data.len() <= 4 {
            self.write_pio_one(port, data);
        } else {
            for byte in data {
                self.write_pio_one(port, std::slice::from_ref(byte));
            }
        }
    }

    /// Service a guest MMIO read, filling `data` (1–8 bytes, little-endian).
    /// Buffers longer than eight bytes are split into byte-wide accesses with
    /// ascending addresses. Unmapped addresses read all-ones.
    pub fn read_mmio(&mut self, addr: u64, data: &mut [u8]) {
        if data.len() <= 8 {
            self.read_mmio_one(addr, data);
        } else {
            for (offset, byte) in data.iter_mut().enumerate() {
                self.read_mmio_one(
                    addr.wrapping_add(offset.to_u64()),
                    std::slice::from_mut(byte),
                );
            }
        }
    }

    /// Service a guest MMIO write of `data` (1–8 bytes, little-endian).
    /// Buffers longer than eight bytes are split into byte-wide accesses with
    /// ascending addresses. Unmapped addresses drop the write.
    pub fn write_mmio(&mut self, addr: u64, data: &[u8]) {
        if data.len() <= 8 {
            self.write_mmio_one(addr, data);
        } else {
            for (offset, byte) in data.iter().enumerate() {
                self.write_mmio_one(
                    addr.wrapping_add(offset.to_u64()),
                    std::slice::from_ref(byte),
                );
            }
        }
    }

    // -- Single-access helpers (width <= the register size) --

    fn read_pio_one(&mut self, port: u16, data: &mut [u8]) {
        let size = u8_of(data.len());
        let value = self
            .pio_device(port)
            .map_or(u32::MAX, |device| device.pio_read(port, size));
        let bytes = value.to_le_bytes();
        for (dst, src) in data.iter_mut().zip(bytes) {
            *dst = src;
        }
    }

    fn write_pio_one(&mut self, port: u16, data: &[u8]) {
        let size = u8_of(data.len());
        let mut bytes = [0u8; 4];
        for (dst, src) in bytes.iter_mut().zip(data) {
            *dst = *src;
        }
        let value = u32::from_le_bytes(bytes);
        if let Some(device) = self.pio_device(port) {
            device.pio_write(port, size, value);
        }
    }

    fn read_mmio_one(&mut self, addr: u64, data: &mut [u8]) {
        let size = u8_of(data.len());
        let value = self.mmio_device(addr).map_or(u64::MAX, |(base, device)| {
            device.mmio_read(addr - base, size)
        });
        let bytes = value.to_le_bytes();
        for (dst, src) in data.iter_mut().zip(bytes) {
            *dst = src;
        }
    }

    fn write_mmio_one(&mut self, addr: u64, data: &[u8]) {
        let size = u8_of(data.len());
        let mut bytes = [0u8; 8];
        for (dst, src) in bytes.iter_mut().zip(data) {
            *dst = *src;
        }
        let value = u64::from_le_bytes(bytes);
        if let Some((base, device)) = self.mmio_device(addr) {
            device.mmio_write(addr - base, size, value);
        }
    }
}

/// Bridges KVM vCPU exits to the device model: every guest I/O / MMIO access
/// surfaced by the run loop is routed to the owning device (or floats to
/// all-ones / is dropped when unmapped).
impl VmExitHandler for Bus {
    fn io_in(&mut self, port: u16, data: &mut [u8]) {
        self.read_pio(port, data);
    }
    fn io_out(&mut self, port: u16, data: &[u8]) {
        self.write_pio(port, data);
    }
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.read_mmio(addr, data);
    }
    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        self.write_mmio(addr, data);
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::{Bus, MmioDevice, PioDevice, RegisterError, VmExitHandler};
    use std::sync::{Arc, Mutex};

    /// Shared log of `(port_or_offset, size, data)` writes, so a test can
    /// inspect what a boxed device recorded after dispatch.
    type WriteLog<T> = Arc<Mutex<Vec<(T, u8, u64)>>>;

    /// A minimal PIO device over a fixed range. Reads return `read_value`;
    /// writes are appended to the shared `writes` log.
    struct FakePio {
        base: u16,
        len: u16,
        read_value: u32,
        writes: WriteLog<u16>,
    }

    impl FakePio {
        fn new(base: u16, len: u16, read_value: u32) -> (Self, WriteLog<u16>) {
            let writes: WriteLog<u16> = Arc::new(Mutex::new(Vec::new()));
            let dev = Self {
                base,
                len,
                read_value,
                writes: Arc::clone(&writes),
            };
            (dev, writes)
        }
    }

    impl PioDevice for FakePio {
        fn pio_read(&mut self, _port: u16, _size: u8) -> u32 {
            self.read_value
        }
        fn pio_write(&mut self, port: u16, size: u8, data: u32) {
            self.writes
                .lock()
                .unwrap()
                .push((port, size, u64::from(data)));
        }
        fn port_range(&self) -> (u16, u16) {
            (self.base, self.base + self.len)
        }
    }

    /// A minimal MMIO device that stores a single 64-bit cell and logs writes.
    struct FakeMmio {
        base: u64,
        len: u64,
        cell: u64,
        writes: WriteLog<u64>,
    }

    impl FakeMmio {
        fn new(base: u64, len: u64, cell: u64) -> (Self, WriteLog<u64>) {
            let writes: WriteLog<u64> = Arc::new(Mutex::new(Vec::new()));
            let dev = Self {
                base,
                len,
                cell,
                writes: Arc::clone(&writes),
            };
            (dev, writes)
        }
    }

    impl MmioDevice for FakeMmio {
        fn mmio_read(&mut self, _offset: u64, _size: u8) -> u64 {
            self.cell
        }
        fn mmio_write(&mut self, offset: u64, size: u8, data: u64) {
            self.writes.lock().unwrap().push((offset, size, data));
            self.cell = data;
        }
        fn mmio_range(&self) -> (u64, u64) {
            (self.base, self.base + self.len)
        }
    }

    /// Register a PIO fake and discard its write log (for read-only tests).
    fn pio(bus: &mut Bus, base: u16, len: u16, read_value: u32) {
        let (dev, _log) = FakePio::new(base, len, read_value);
        bus.register_pio(Box::new(dev)).expect("register pio");
    }

    #[test]
    fn register_rejects_empty_and_overlapping_ranges() {
        let mut bus = Bus::new();
        // Empty range.
        let (empty_dev, _) = FakePio::new(0x100, 0, 0);
        let empty = bus.register_pio(Box::new(empty_dev));
        assert_eq!(
            empty,
            Err(RegisterError::EmptyRange {
                base: 0x100,
                end: 0x100
            })
        );

        // First real device: 0x3f8..0x400.
        pio(&mut bus, 0x3f8, 8, 0);
        assert_eq!(bus.pio_count(), 1);

        // Overlapping device: 0x3ff..0x401 intersects the above.
        let (overlap_dev, _) = FakePio::new(0x3ff, 2, 0);
        let overlap = bus.register_pio(Box::new(overlap_dev));
        assert_eq!(
            overlap,
            Err(RegisterError::Overlap {
                base: 0x3ff,
                end: 0x401
            })
        );
        assert_eq!(bus.pio_count(), 1);

        // Adjacent, non-overlapping device registers fine.
        pio(&mut bus, 0x400, 8, 0);
        assert_eq!(bus.pio_count(), 2);
    }

    #[test]
    fn pio_read_returns_device_value_little_endian() {
        let mut bus = Bus::new();
        pio(&mut bus, 0x3f8, 8, 0x1234_5678);

        let mut one = [0u8; 1];
        bus.read_pio(0x3f8, &mut one);
        assert_eq!(one, [0x78]);

        let mut two = [0u8; 2];
        bus.read_pio(0x3f9, &mut two);
        assert_eq!(two, [0x78, 0x56]);

        let mut four = [0u8; 4];
        bus.read_pio(0x3fa, &mut four);
        assert_eq!(four, [0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn pio_write_reassembles_little_endian_value_and_size() {
        let mut bus = Bus::new();
        let (dev, log) = FakePio::new(0x3f8, 8, 0);
        bus.register_pio(Box::new(dev)).unwrap();

        bus.write_pio(0x3f8, &[0xef, 0xbe, 0xad, 0xde]);

        let writes = log.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0], (0x3f8, 4, 0xdead_beef));
    }

    #[test]
    fn unmapped_pio_reads_float_high_and_drops_writes() {
        let mut bus = Bus::new();
        let mut buf = [0u8; 4];
        bus.read_pio(0x80, &mut buf);
        assert_eq!(buf, [0xff, 0xff, 0xff, 0xff]);
        // Writing to nothing must not panic.
        bus.write_pio(0x80, &[1, 2, 3, 4]);
    }

    #[test]
    fn mmio_read_write_roundtrips_through_offset() {
        let mut bus = Bus::new();
        let (dev, log) = FakeMmio::new(0xfed4_0000, 0x1000, 0);
        bus.register_mmio(Box::new(dev)).unwrap();

        // Write a 4-byte value at base+0x10.
        bus.write_mmio(0xfed4_0010, &[0x11, 0x22, 0x33, 0x44]);
        // The device sees the device-relative offset (0x10), the access width,
        // and the little-endian value.
        let writes = log.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0], (0x10, 4, 0x4433_2211));

        // The fake echoes its stored cell back on any read.
        let mut out = [0u8; 4];
        bus.read_mmio(0xfed4_0010, &mut out);
        assert_eq!(out, [0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn unmapped_mmio_reads_float_high() {
        let mut bus = Bus::new();
        let mut buf = [0u8; 8];
        bus.read_mmio(0xdead_0000, &mut buf);
        assert_eq!(buf, [0xff; 8]);
        bus.write_mmio(0xdead_0000, &[1]); // must not panic
    }

    #[test]
    fn vm_exit_handler_routes_to_the_right_device() {
        // Two PIO devices on disjoint ranges; verify the handler path (the KVM
        // entry point) lands each access on the correct device.
        let mut bus = Bus::new();
        pio(&mut bus, 0x60, 1, 0xaa); // PS/2-ish
        let (com1, com1_log) = FakePio::new(0x3f8, 8, 0x55); // COM1-ish
        bus.register_pio(Box::new(com1)).unwrap();

        let mut a = [0u8; 1];
        VmExitHandler::io_in(&mut bus, 0x60, &mut a);
        assert_eq!(a, [0xaa]);

        let mut b = [0u8; 1];
        VmExitHandler::io_in(&mut bus, 0x3f8, &mut b);
        assert_eq!(b, [0x55]);

        // A write through the handler reaches the mapped device; an unmapped
        // write is dropped cleanly (no panic, nothing recorded).
        VmExitHandler::io_out(&mut bus, 0x3f8, &[0x41]);
        VmExitHandler::io_out(&mut bus, 0x999, &[0x41]);
        let writes = com1_log.lock().unwrap().clone();
        assert_eq!(writes.as_slice(), &[(0x3f8, 1, 0x41)]);
    }

    #[test]
    fn rep_string_pio_writes_each_byte_to_same_port() {
        // A >4-byte PIO buffer is byte-wide string output (`rep outsb`) to one
        // port: every element must reach the device as a separate 1-byte write.
        let mut bus = Bus::new();
        let (dev, log) = FakePio::new(0x3f8, 8, 0);
        bus.register_pio(Box::new(dev)).unwrap();

        bus.write_pio(0x3f8, b"Hi!"); // <= 4 bytes is a single access...
        bus.write_pio(0x3f8, b"hello world!"); // ...but 12 bytes is 12 writes

        let writes = log.lock().unwrap().clone();
        // First call: one combined access of 3 bytes; then 12 single-byte ones.
        assert_eq!(writes.len(), 1 + 12);
        assert_eq!(writes[0].1, 3); // size of the combined access
        assert!(
            writes[1..]
                .iter()
                .all(|&(port, size, _)| port == 0x3f8 && size == 1),
            "string output must be byte-wide writes to the same port"
        );
        // The string bytes arrive in order.
        let bytes: Vec<u8> = writes[1..]
            .iter()
            .map(|&(_, _, d)| u8::try_from(d).unwrap())
            .collect();
        assert_eq!(bytes, b"hello world!");
    }
}
