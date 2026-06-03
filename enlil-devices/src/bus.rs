//! Device bus abstractions.
//!
//! Dispatches PIO (port I/O) and MMIO (memory-mapped I/O) accesses
//! from guest VM exits to the appropriate virtual device handler.

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

/// Trait for devices that handle port I/O.
pub trait PioDevice {
    fn pio_read(&mut self, port: u16, size: u8) -> u32;
    fn pio_write(&mut self, port: u16, size: u8, data: u32);
    fn port_range(&self) -> (u16, u16); // (base, base + len)
}

/// Trait for devices that handle memory-mapped I/O.
pub trait MmioDevice {
    fn mmio_read(&mut self, offset: u64, size: u8) -> u64;
    fn mmio_write(&mut self, offset: u64, size: u8, data: u64);
    fn mmio_range(&self) -> (u64, u64); // (base, base + len)
}

/// PIO bus — routes port I/O to registered devices.
pub struct PioBus {
    /// Maps port base address to device index.
    devices: BTreeMap<u16, usize>,
}

impl PioBus {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
        }
    }

    /// Register a device's port range. Returns the device index.
    pub fn register(&mut self, base: u16, device_index: usize) {
        self.devices.insert(base, device_index);
    }

    /// Find which device owns a given port.
    #[must_use]
    pub fn lookup(&self, port: u16) -> Option<usize> {
        // Find the device whose base is <= port
        self.devices.range(..=port).next_back().map(|(_, &idx)| idx)
    }
}

impl Default for PioBus {
    fn default() -> Self {
        Self::new()
    }
}

/// MMIO bus — routes memory-mapped I/O to registered devices.
pub struct MmioBus {
    /// Maps MMIO base address to device index.
    devices: BTreeMap<u64, usize>,
}

impl MmioBus {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
        }
    }

    /// Register a device's MMIO base address.
    pub fn register(&mut self, base: u64, device_index: usize) {
        self.devices.insert(base, device_index);
    }

    /// Look up which device owns a given MMIO address.
    #[must_use]
    pub fn lookup(&self, address: u64) -> Option<usize> {
        self.devices
            .range(..=address)
            .next_back()
            .map(|(_, &idx)| idx)
    }
}

impl Default for MmioBus {
    fn default() -> Self {
        Self::new()
    }
}
