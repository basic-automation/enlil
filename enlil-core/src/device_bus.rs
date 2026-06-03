//! Bridges guest vCPU exits to the `enlil-devices` device bus.
//!
//! [`DeviceBus`] bundles the two `enlil-devices` buses — one for port I/O
//! (`PioBus`), one for memory-mapped I/O (`MmioBus`) — and implements the
//! backend-facing [`VmExitHandler`](crate::kvm_backend::VmExitHandler). When the
//! KVM backend ([`crate::kvm_backend`]) translates a vCPU exit into an I/O or
//! MMIO access it calls these methods, which forward straight to the bus's own
//! address decode (`enlil-devices::bus`). There is deliberately **one** bus and
//! **one** address-decode path: the backend never decodes addresses itself
//! (matches the rust-vmm `vm-device` `IoManager` model).
//!
//! Unmapped accesses inherit the bus's x86 *open-bus* semantics: reads return
//! all-ones and writes are dropped, so a guest probing an absent device sees the
//! same thing it would on real hardware rather than a hypervisor stall.

use crate::kvm_backend::VmExitHandler;
use enlil_devices::bus::{MmioBus, MmioDevice, PioBus, PioDevice};

/// The system device bus: a PIO bus and an MMIO bus behind one exit handler.
#[derive(Default)]
pub struct DeviceBus {
    /// Port-I/O devices (e.g. the 16550 UART at `0x3F8`, PS/2, PIT).
    pub pio: PioBus,
    /// Memory-mapped devices (e.g. LAPIC, IOAPIC, HPET, PCIe ECAM).
    pub mmio: MmioBus,
}

impl DeviceBus {
    /// An empty bus with no devices registered.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pio: PioBus::new(),
            mmio: MmioBus::new(),
        }
    }

    /// Register a port-I/O device over the range it declares.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the device's range is
    /// empty or overlaps an already-registered device.
    pub fn add_pio(
        &mut self,
        device: Box<dyn PioDevice>,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.pio.register(device)
    }

    /// Register a memory-mapped device over the range it declares.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the device's range is
    /// empty or overlaps an already-registered device.
    pub fn add_mmio(
        &mut self,
        device: Box<dyn MmioDevice>,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.mmio.register(device)
    }
}

impl VmExitHandler for DeviceBus {
    fn io_in(&mut self, port: u16, data: &mut [u8]) {
        self.pio.read(port, data);
    }
    fn io_out(&mut self, port: u16, data: &[u8]) {
        self.pio.write(port, data);
    }
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.mmio.read(addr, data);
    }
    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        self.mmio.write(addr, data);
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceBus;
    use crate::kvm_backend::VmExitHandler;
    use enlil_devices::bus::{MmioDevice, PioDevice};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Minimal one-byte "serial-like" device that captures every byte written
    /// and returns a fixed status byte on read.
    struct FakeSerial {
        out: Rc<RefCell<Vec<u8>>>,
        status: u8,
    }

    impl PioDevice for FakeSerial {
        fn pio_read(&mut self, _port: u16, _size: u8) -> u32 {
            u32::from(self.status)
        }
        fn pio_write(&mut self, _port: u16, _size: u8, data: u32) {
            self.out.borrow_mut().push(data.to_le_bytes()[0]);
        }
        fn port_range(&self) -> (u16, u16) {
            (0x3F8, 0x400) // COM1, 8 ports
        }
    }

    struct FakeMmio {
        last_write: Rc<RefCell<Option<(u64, u64)>>>,
    }

    impl MmioDevice for FakeMmio {
        fn mmio_read(&mut self, _offset: u64, _size: u8) -> u64 {
            0xABCD
        }
        fn mmio_write(&mut self, offset: u64, _size: u8, data: u64) {
            *self.last_write.borrow_mut() = Some((offset, data));
        }
        fn mmio_range(&self) -> (u64, u64) {
            (0xFEB0_0000, 0xFEB0_1000)
        }
    }

    #[test]
    fn io_out_forwards_guest_bytes_to_the_serial_device() {
        let out = Rc::new(RefCell::new(Vec::new()));
        let mut bus = DeviceBus::new();
        bus.add_pio(Box::new(FakeSerial {
            out: Rc::clone(&out),
            status: 0x20,
        }))
        .unwrap();

        // A guest writing "Hi" to COM1 one byte at a time, as KVM would deliver it.
        VmExitHandler::io_out(&mut bus, 0x3F8, b"H");
        VmExitHandler::io_out(&mut bus, 0x3F8, b"i");
        assert_eq!(&*out.borrow(), b"Hi");

        // Reading the line-status register returns the device's status byte.
        let mut lsr = [0u8; 1];
        VmExitHandler::io_in(&mut bus, 0x3FD, &mut lsr);
        assert_eq!(lsr, [0x20]);
    }

    #[test]
    fn mmio_exits_reach_the_right_device_at_the_right_offset() {
        let last = Rc::new(RefCell::new(None));
        let mut bus = DeviceBus::new();
        bus.add_mmio(Box::new(FakeMmio {
            last_write: Rc::clone(&last),
        }))
        .unwrap();

        VmExitHandler::mmio_write(&mut bus, 0xFEB0_0040, &0x55u32.to_le_bytes());
        assert_eq!(*last.borrow(), Some((0x40, 0x55)));

        let mut buf = [0u8; 2];
        VmExitHandler::mmio_read(&mut bus, 0xFEB0_0000, &mut buf);
        assert_eq!(buf, 0xABCDu16.to_le_bytes());
    }

    #[test]
    fn unmapped_exit_is_open_bus_not_a_panic() {
        let mut bus = DeviceBus::new();
        let mut buf = [0u8; 4];
        VmExitHandler::io_in(&mut bus, 0xCF8, &mut buf);
        assert_eq!(buf, [0xFF, 0xFF, 0xFF, 0xFF]);
        // Writes to nothing are silently dropped.
        VmExitHandler::io_out(&mut bus, 0xCF8, &[0xDE, 0xAD]);
        VmExitHandler::mmio_write(&mut bus, 0x1234_0000, &[1, 2, 3, 4]);
    }
}
