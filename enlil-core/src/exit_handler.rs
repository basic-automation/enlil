//! VM-exit dispatch into the device bus.
//!
//! Bridges the hypervisor-agnostic [`VmExitHandler`] surfaced by the KVM
//! backend to the address-decoding device bus in `enlil-devices`. We implement
//! the trait *for* [`enlil_devices::bus::Bus`] rather than building a parallel
//! decode path, so there is a single bus and a single address decode — the
//! same structure rust-vmm's `vm-device` `IoManager` uses.
//!
//! Unmapped reads return all-ones (the value a real x86 bus floats to when no
//! device drives the lines); unmapped writes are dropped and logged at trace
//! level.

use crate::kvm_backend::VmExitHandler;
use enlil_devices::bus::Bus;

/// Value returned for reads of unmapped I/O / MMIO addresses.
const FLOATING_BUS: u8 = 0xff;

impl VmExitHandler for Bus {
    fn io_in(&mut self, port: u16, data: &mut [u8]) {
        if !self.pio.read(port, data) {
            data.fill(FLOATING_BUS);
        }
    }

    fn io_out(&mut self, port: u16, data: &[u8]) {
        if !self.pio.write(port, data) {
            log::trace!(
                "unmapped PIO write to port {port:#06x} ({} bytes) dropped",
                data.len()
            );
        }
    }

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        if !self.mmio.read(addr, data) {
            data.fill(FLOATING_BUS);
        }
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        if !self.mmio.write(addr, data) {
            log::trace!(
                "unmapped MMIO write to {addr:#x} ({} bytes) dropped",
                data.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enlil_devices::bus::PioDevice;
    use std::sync::{Arc, Mutex};

    /// A PIO device that appends every written byte to a shared buffer and
    /// returns a fixed marker on read, so a test can observe what the bus
    /// delivered after the device has been moved into the bus.
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl PioDevice for Sink {
        fn pio_read(&mut self, _offset: u16, data: &mut [u8]) {
            data.fill(0x42);
        }
        fn pio_write(&mut self, _offset: u16, data: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(data);
        }
    }

    #[test]
    fn io_out_reaches_the_registered_device() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut bus = Bus::new();
        bus.pio
            .register(0x3f8, 8, Box::new(Sink(Arc::clone(&captured))))
            .unwrap();

        let handler: &mut dyn VmExitHandler = &mut bus;
        handler.io_out(0x3f8, b"OK");

        assert_eq!(&*captured.lock().unwrap(), b"OK");
    }

    #[test]
    fn io_in_from_mapped_device_fills_from_the_device() {
        let mut bus = Bus::new();
        bus.pio
            .register(0x3f8, 8, Box::new(Sink(Arc::new(Mutex::new(Vec::new())))))
            .unwrap();

        let mut buf = [0u8; 1];
        let handler: &mut dyn VmExitHandler = &mut bus;
        handler.io_in(0x3f8, &mut buf);

        assert_eq!(buf[0], 0x42);
    }

    #[test]
    fn io_in_from_unmapped_port_floats_high() {
        let mut bus = Bus::new();
        let mut buf = [0u8; 2];
        let handler: &mut dyn VmExitHandler = &mut bus;
        handler.io_in(0x80, &mut buf);
        assert_eq!(buf, [0xff, 0xff]);
    }

    #[test]
    fn mmio_read_from_unmapped_address_floats_high() {
        let mut bus = Bus::new();
        let mut buf = [0u8; 4];
        let handler: &mut dyn VmExitHandler = &mut bus;
        handler.mmio_read(0xdead_0000, &mut buf);
        assert_eq!(buf, [0xff; 4]);
    }
}
