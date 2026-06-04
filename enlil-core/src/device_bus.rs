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
use crate::serial::SerialPort;
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

    /// Mount a [`SerialPort`] (16550 UART) on the PIO bus over the eight ports
    /// `[base, base + 8)` it claims.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the serial port's range
    /// overlaps an already-registered device.
    pub fn add_serial(&mut self, serial: SerialPort) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(serial))
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

    #[test]
    fn real_serial_uart_on_the_bus_routes_guest_writes_to_the_sink() {
        use crate::serial::{SerialOutput, SerialOutputMode, SerialPort, COM1, LSR_REG};
        use std::sync::{Arc, Mutex};

        // A shared sink so the guest's TX stays observable after the device is
        // moved into the bus.
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut bus = DeviceBus::new();
        bus.add_serial(SerialPort::com1(SerialOutput::new(
            "guest",
            SerialOutputMode::Shared(Arc::clone(&sink)),
        )))
        .unwrap();

        // Drive it the way the KVM backend would: byte-at-a-time `io_out` to the
        // data register.
        for &b in b"OK" {
            VmExitHandler::io_out(&mut bus, COM1, &[b]);
        }
        assert_eq!(&*sink.lock().unwrap(), b"OK");

        // The Line Status Register reports the transmitter is ready (THR empty).
        let mut lsr = [0u8; 1];
        VmExitHandler::io_in(&mut bus, COM1 + LSR_REG, &mut lsr);
        assert_ne!(lsr[0] & 0x20, 0, "THR-empty bit should be set");

        // The device owns exactly its eight ports.
        assert!(bus.pio.is_mapped(COM1));
        assert!(bus.pio.is_mapped(COM1 + 7));
        assert!(!bus.pio.is_mapped(COM1 + 8));
    }

    // End-to-end smoke test: a real guest writes to COM1 and the bytes arrive in
    // the serial sink through the real KVM exit → DeviceBus → SerialPort path.
    // Self-skips when `/dev/kvm` is unavailable (no nested virt) rather than
    // faking a pass.
    #[cfg(target_os = "linux")]
    #[test]
    fn serial_console_smoke() {
        use crate::kvm_backend::{is_kvm_available, GuestExit, KvmBackend};
        use crate::serial::{SerialOutput, SerialOutputMode, SerialPort};
        use std::sync::{Arc, Mutex};

        if !is_kvm_available() {
            eprintln!("skipping serial_console_smoke: /dev/kvm not available (no nested virt)");
            return;
        }

        // A tiny 16-bit real-mode blob that emits "OK" to COM1 then halts:
        //   BA F8 03   mov dx, 0x3F8
        //   B0 4F      mov al, 'O'
        //   EE         out dx, al
        //   B0 4B      mov al, 'K'
        //   EE         out dx, al
        //   F4         hlt
        #[rustfmt::skip]
        let code: [u8; 10] = [
            0xBA, 0xF8, 0x03,
            0xB0, 0x4F,
            0xEE,
            0xB0, 0x4B,
            0xEE,
            0xF4,
        ];

        // Back the guest with one page; place the code at guest-physical 0x1000.
        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut mem = vec![0u8; SIZE];
        mem[..code.len()].copy_from_slice(&code);
        let host_addr = mem.as_mut_ptr() as u64;

        let mut backend = KvmBackend::new().expect("create KVM VM");
        // SAFETY: `mem` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, SIZE as u64) }.expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        // The COM1 UART, output captured in a shared buffer so we can read it
        // back after the device is moved into the bus.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let serial = SerialPort::com1(SerialOutput::new(
            "smoke",
            SerialOutputMode::Shared(Arc::clone(&captured)),
        ));
        let mut bus = DeviceBus::new();
        bus.add_serial(serial).unwrap();

        // Drive the vCPU until it halts (bounded so a misbehaving guest can't
        // spin forever).
        let mut halted = false;
        for _ in 0..100 {
            let exit = backend.run_vcpu(0, &mut bus).expect("run vcpu");
            if exit == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");

        // The bytes the guest `out`-ed to COM1 traversed the real KVM exit →
        // DeviceBus → SerialPort → UART path and landed in the shared sink.
        assert_eq!(&*captured.lock().unwrap(), b"OK");
    }
}
