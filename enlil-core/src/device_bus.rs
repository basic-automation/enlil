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
use crate::serial::{SerialOutput, SerialPort};
use enlil_devices::bus::{MmioBus, MmioDevice, PioBus, PioDevice};
use enlil_devices::pcie::{
    vendors, EcamSpace, PciBdf, PciConfigIo, PcieRootComplex, SharedRootComplex,
};
use enlil_devices::timer::Pit;

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

    /// Mount the 8254 [`Pit`] on the PIO bus over the four ports `0x40..=0x43`
    /// (channel data `0x40`-`0x42` and the command/read-back port `0x43`).
    ///
    /// The PIT is one of the first devices a guest touches at boot; without it
    /// the legacy timer ports read back as open-bus `0xFF`, which stalls or trips
    /// up BIOS/early-kernel timer calibration. Channel-0 IRQ0 delivery is a
    /// separate concern wired through the interrupt path.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the PIT's range overlaps an
    /// already-registered device.
    pub fn add_pit(&mut self, pit: Pit) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(pit))
    }

    /// Mount the legacy PCI **Configuration Mechanism #1** front-end
    /// ([`PciConfigIo`]) on the PIO bus over the eight ports `0xCF8..=0xCFF`
    /// (the `CONFIG_ADDRESS`/`CONFIG_DATA` pair).
    ///
    /// A guest BIOS / early kernel uses these ports to enumerate the PCI bus
    /// before it has brought up ECAM MMIO; without them the config ports read
    /// back as open-bus `0xFF`, so the guest finds no host bridge and no devices.
    /// ECAM (the MMIO front-end over the same [`PcieRootComplex`]) is mounted
    /// separately on the MMIO bus.
    ///
    /// [`PcieRootComplex`]: enlil_devices::pcie::PcieRootComplex
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the range overlaps an
    /// already-registered device.
    pub fn add_pci_config_io(
        &mut self,
        pci: PciConfigIo,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(pci))
    }

    /// Mount a complete `PCIe` config-space front-end over `root`: the legacy
    /// PIO [`PciConfigIo`] (`0xCF8`/`0xCFC`) on the PIO bus **and** the
    /// [`EcamSpace`] MMIO window (at `root.ecam_base`) on the MMIO bus, both
    /// sharing one [`PcieRootComplex`] so a register programmed through either
    /// mechanism is visible through the other.
    ///
    /// If `root` does not already contain a device at BDF 0:0.0, a default Intel
    /// 440FX-style **host bridge** is seeded there so a guest enumerating the bus
    /// at boot finds at least the root device (matching real hardware, where the
    /// host bridge always answers).
    ///
    /// Returns the [`SharedRootComplex`] handle so the caller can add further
    /// devices after both front-ends are mounted (the change is seen by both).
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if either the CAM port range
    /// (`0xCF8..=0xCFF`) or the ECAM MMIO window overlaps an already-registered
    /// device.
    pub fn add_pcie(
        &mut self,
        root: PcieRootComplex,
    ) -> Result<SharedRootComplex, enlil_devices::bus::BusError> {
        let cam = PciConfigIo::new(root);
        let shared = cam.shared();
        {
            let mut rc = shared.borrow_mut();
            if rc.find_device(&PciBdf::new(0, 0, 0)).is_none() {
                rc.add_device(PcieRootComplex::create_host_bridge(vendors::INTEL, 0x1237));
            }
        }
        self.add_pci_config_io(cam)?;
        self.add_mmio(Box::new(EcamSpace::new(shared.clone())))?;
        Ok(shared)
    }

    /// Assemble a bus with the legacy devices a PC guest expects to find at the
    /// canonical fixed addresses, in one call: the **COM1** 16550 UART (`0x3F8`),
    /// the **8254 PIT** (`0x40`-`0x43`), and the **`PCIe`** config-space pair —
    /// legacy CAM (`0xCF8`/`0xCFC`) plus the ECAM MMIO window at
    /// [`DEFAULT_ECAM_BASE`] — over one shared root complex seeded with a default
    /// host bridge at 0:0.0.
    ///
    /// [`DEFAULT_ECAM_BASE`] is the base a standard single-segment MCFG ACPI
    /// table advertises (`enlil_devices::acpi`), so the window the guest is told
    /// about matches the one we decode.
    ///
    /// Returns the assembled bus and the [`SharedRootComplex`] handle so the
    /// caller can attach further PCI devices (visible through both front-ends).
    /// Serial TX is routed to `serial_output`; pass a
    /// [`SerialOutputMode::Shared`](crate::serial::SerialOutputMode::Shared) sink
    /// to keep the guest's console output observable.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if any two devices' ranges
    /// overlap (they do not at these canonical addresses, so this is effectively
    /// infallible for the default layout).
    pub fn standard_pc(
        serial_output: SerialOutput,
    ) -> Result<(Self, SharedRootComplex), enlil_devices::bus::BusError> {
        let mut bus = Self::new();
        bus.add_serial(SerialPort::com1(serial_output))?;
        bus.add_pit(Pit::new())?;
        let pcie = bus.add_pcie(PcieRootComplex::new(DEFAULT_ECAM_BASE))?;
        Ok((bus, pcie))
    }
}

/// Guest-physical base of the `PCIe` ECAM window for the default single-segment
/// layout. Matches the MCFG table emitted by `enlil_devices::acpi`, so a guest
/// that discovers ECAM from ACPI finds it where [`DeviceBus::standard_pc`]
/// mounts it.
pub const DEFAULT_ECAM_BASE: u64 = 0xB000_0000;

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
    fn pit_on_the_bus_programs_and_reads_back_a_channel_count() {
        use enlil_devices::timer::Pit;

        let mut bus = DeviceBus::new();
        bus.add_pit(Pit::new()).unwrap();

        // The PIT owns exactly its four ports 0x40..=0x43.
        assert!(bus.pio.is_mapped(0x40));
        assert!(bus.pio.is_mapped(0x43));
        assert!(!bus.pio.is_mapped(0x44));

        // Drive it the way the KVM backend would: byte-at-a-time `io_out`.
        // Command 0x34 = channel 0, lo/hi access, mode 2 (rate generator).
        VmExitHandler::io_out(&mut bus, 0x43, &[0x34]);
        VmExitHandler::io_out(&mut bus, 0x40, &[0x34]); // reload low byte
        VmExitHandler::io_out(&mut bus, 0x40, &[0x12]); // reload high byte

        // Latch channel 0, then read its count back lo/hi through the bus.
        VmExitHandler::io_out(&mut bus, 0x43, &[0x00]);
        let mut lo = [0u8; 1];
        let mut hi = [0u8; 1];
        VmExitHandler::io_in(&mut bus, 0x40, &mut lo);
        VmExitHandler::io_in(&mut bus, 0x40, &mut hi);
        assert_eq!(u16::from_le_bytes([lo[0], hi[0]]), 0x1234);
    }

    #[test]
    fn pit_and_serial_coexist_on_the_pio_bus() {
        use crate::serial::{SerialOutput, SerialOutputMode, SerialPort, COM1};
        use enlil_devices::timer::Pit;
        use std::sync::{Arc, Mutex};

        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut bus = DeviceBus::new();
        bus.add_serial(SerialPort::com1(SerialOutput::new(
            "guest",
            SerialOutputMode::Shared(Arc::clone(&sink)),
        )))
        .unwrap();
        // Non-overlapping ranges (0x40..0x44 vs 0x3F8..0x400) register cleanly.
        bus.add_pit(Pit::new()).unwrap();

        // Each device still answers on its own ports.
        VmExitHandler::io_out(&mut bus, COM1, b"X");
        assert_eq!(&*sink.lock().unwrap(), b"X");
        VmExitHandler::io_out(&mut bus, 0x43, &[0x34]);
        VmExitHandler::io_out(&mut bus, 0x40, &[0x01]);
        VmExitHandler::io_out(&mut bus, 0x40, &[0x00]);
        assert!(bus.pio.is_mapped(0x40));
        assert!(bus.pio.is_mapped(COM1));
    }

    #[test]
    fn guest_enumerates_pci_through_cf8_cfc_on_the_bus() {
        use enlil_devices::pcie::{PciBdf, PciConfigIo, PciConfigSpace, PcieRootComplex};

        // A root complex with a single device at BDF 0:2.0.
        let mut rc = PcieRootComplex::new(0xB000_0000);
        rc.add_device(PciConfigSpace::new(PciBdf::new(0, 2, 0), 0x8086, 0x5678));

        let mut bus = DeviceBus::new();
        bus.add_pci_config_io(PciConfigIo::new(rc)).unwrap();

        // The front-end owns exactly the eight legacy CAM ports 0xCF8..=0xCFF.
        assert!(bus.pio.is_mapped(0xCF8));
        assert!(bus.pio.is_mapped(0xCFF));
        assert!(!bus.pio.is_mapped(0xD00));

        // Drive it the way a guest BIOS would: a dword OUT to CONFIG_ADDRESS
        // (enable | bus 0 | device 2 | func 0 | reg 0), then a dword IN from
        // CONFIG_DATA, byte-at-a-time as KVM delivers the exit.
        let addr: u32 = 0x8000_0000 | (2 << 11);
        VmExitHandler::io_out(&mut bus, 0xCF8, &addr.to_le_bytes());
        let mut data = [0u8; 4];
        VmExitHandler::io_in(&mut bus, 0xCFC, &mut data);
        // device_id:vendor_id = 0x5678_8086.
        assert_eq!(u32::from_le_bytes(data), 0x5678_8086);

        // An absent function (0:1.0) enumerates as all-ones (no device).
        let absent: u32 = 0x8000_0000 | (1 << 11);
        VmExitHandler::io_out(&mut bus, 0xCF8, &absent.to_le_bytes());
        VmExitHandler::io_in(&mut bus, 0xCFC, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0xFFFF_FFFF);
    }

    #[test]
    fn add_pcie_mounts_cam_and_ecam_sharing_one_device_set() {
        use enlil_devices::pcie::{PciBdf, PciConfigSpace, PcieRootComplex};

        let mut bus = DeviceBus::new();
        // Mount both front-ends over a fresh root complex at the standard ECAM
        // base. add_pcie seeds a default host bridge at 0:0.0.
        let shared = bus.add_pcie(PcieRootComplex::new(0xB000_0000)).unwrap();

        // CAM owns the eight legacy ports; ECAM owns the 256 MiB window.
        assert!(bus.pio.is_mapped(0xCF8));
        assert!(bus.pio.is_mapped(0xCFF));
        assert!(bus.mmio.is_mapped(0xB000_0000));
        assert!(bus.mmio.is_mapped(0xBFFF_FFFF));
        assert!(!bus.mmio.is_mapped(0xC000_0000));

        // The seeded host bridge (0:0.0) answers through the legacy CAM ports:
        // a dword OUT to CONFIG_ADDRESS (enable | 0:0.0 | reg 0) then IN.
        let addr: u32 = 0x8000_0000;
        VmExitHandler::io_out(&mut bus, 0xCF8, &addr.to_le_bytes());
        let mut data = [0u8; 4];
        VmExitHandler::io_in(&mut bus, 0xCFC, &mut data);
        // Intel 440FX host bridge: device_id:vendor_id = 0x1237_8086.
        assert_eq!(u32::from_le_bytes(data), 0x1237_8086);

        // ...and the same device through the ECAM MMIO window at base + 0.
        VmExitHandler::mmio_read(&mut bus, 0xB000_0000, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x1237_8086);

        // A device added through the shared handle *after* both front-ends are
        // mounted is visible through ECAM (proving the shared device set).
        shared
            .borrow_mut()
            .add_device(PciConfigSpace::new(PciBdf::new(0, 2, 0), 0x10EC, 0x8168));
        let dev_addr = 0xB000_0000 + (u64::from(2u32) << 15); // BDF 0:2.0 ecam offset
        VmExitHandler::mmio_read(&mut bus, dev_addr, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x8168_10EC);
    }

    #[test]
    fn standard_pc_mounts_all_legacy_devices_at_canonical_addresses() {
        use crate::device_bus::DEFAULT_ECAM_BASE;
        use crate::serial::{SerialOutput, SerialOutputMode, COM1};
        use std::sync::{Arc, Mutex};

        let sink = Arc::new(Mutex::new(Vec::new()));
        let (mut bus, _pcie) = DeviceBus::standard_pc(SerialOutput::new(
            "guest",
            SerialOutputMode::Shared(Arc::clone(&sink)),
        ))
        .unwrap();

        // COM1 UART, 8254 PIT, and the legacy PCI CAM ports are all on the PIO bus.
        assert!(bus.pio.is_mapped(COM1));
        assert!(bus.pio.is_mapped(0x40));
        assert!(bus.pio.is_mapped(0x43));
        assert!(bus.pio.is_mapped(0xCF8));
        assert!(bus.pio.is_mapped(0xCFF));
        // The ECAM MMIO window sits at the MCFG-advertised base.
        assert!(bus.mmio.is_mapped(DEFAULT_ECAM_BASE));

        // The default host bridge answers through both config mechanisms.
        let addr: u32 = 0x8000_0000; // enable | 0:0.0 | reg 0
        VmExitHandler::io_out(&mut bus, 0xCF8, &addr.to_le_bytes());
        let mut data = [0u8; 4];
        VmExitHandler::io_in(&mut bus, 0xCFC, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x1237_8086);
        VmExitHandler::mmio_read(&mut bus, DEFAULT_ECAM_BASE, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x1237_8086);

        // Guest serial output reaches the shared sink (byte-at-a-time, as a
        // guest drives a byte-wide register).
        for &b in b"hi" {
            VmExitHandler::io_out(&mut bus, COM1, &[b]);
        }
        assert_eq!(&*sink.lock().unwrap(), b"hi");
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
