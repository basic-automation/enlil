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
use crate::stealth_msr::StealthMsrRouter;
use enlil_devices::bus::{MmioBus, MmioDevice, PioBus, PioDevice};
use enlil_devices::chipset::{Gpe0Block, SharedAcpiPm1Block, SharedSystemControlPortA};
use enlil_devices::dma::{Dma8237, DmaPageRegisters};
use enlil_devices::interrupt::{
    IoApicMmio, PirqRouter, SharedInterruptController, SharedPic, PIRQ_DEFAULT_IRQS,
};
use enlil_devices::pcie::{
    vendors, EcamSpace, PciBdf, PciConfigIo, PciResetControl, PcieRootComplex, SharedRootComplex,
    BOARD_SUBSYSTEM_DEVICE_ID, BOARD_SUBSYSTEM_VENDOR_ID, ICH9_SMBUS_BDF, PIRQ_ROUTE_CONFIG_BASE,
    SMBUS_INTERRUPT_PIN,
};
use enlil_devices::ps2::{SharedI8042, PS2_KBD_IRQ, PS2_MOUSE_IRQ};
use enlil_devices::smbus::{SmbusHost, SMBUS_IO_BASE};
use enlil_devices::timer::{
    AcpiPmTimer, Pit, RtcTime, SharedAcpiPmTimer, SharedHpet, SharedPit, SharedRtc,
    SystemControlPortB, HPET_TICK_NS, RTC_IRQ,
};
use enlil_devices::usb::xhci::transfer::DmaMemory;
use enlil_devices::usb::{SharedXhci, UsbSpeed, VirtualXhciController, XhciMmio};
use std::cell::RefCell;
use std::rc::Rc;

/// The system device bus: a PIO bus and an MMIO bus behind one exit handler.
#[derive(Default)]
pub struct DeviceBus {
    /// Port-I/O devices (e.g. the 16550 UART at `0x3F8`, PS/2, PIT).
    pub pio: PioBus,
    /// Memory-mapped devices (e.g. LAPIC, IOAPIC, HPET, PCIe ECAM).
    pub mmio: MmioBus,
    /// A clone of the PCI config front-end's `0xCF9` reset latch, captured when
    /// [`add_pcie`](Self::add_pcie) mounts it, so the platform layer can poll the
    /// guest's reboot request. `None` until `add_pcie` runs.
    pci_reset: Option<PciResetControl>,
    /// Per-vCPU stealth MSR shadows (APERF/MPERF, PMC, LBR). When present, the
    /// handler serves forwarded guest `RDMSR`/`WRMSR` for the modelled MSRs
    /// from the **active** vCPU's router instead of `#GP`-ing them; `None`
    /// leaves all MSR exits unhandled.
    ///
    /// The bus is a single [`VmExitHandler`] shared by every vCPU, but each
    /// vCPU must read its *own* counters — a real SMP guest sees independent
    /// per-logical-CPU APERF/MPERF/PMC/LBR, and two vCPUs reading the *same*
    /// shadow would be a detectable tell. The run loop selects which vCPU's
    /// router serves the next MSR exit with [`set_active_vcpu`] before each
    /// `KVM_RUN`. A single-vCPU install ([`set_stealth_msr_router`]) is just a
    /// bank of one. `None` until a router is installed.
    ///
    /// [`set_stealth_msr_router`]: Self::set_stealth_msr_router
    /// [`set_active_vcpu`]: Self::set_active_vcpu
    /// [`VmExitHandler`]: crate::kvm_backend::VmExitHandler
    stealth: Option<StealthBank>,
}

/// A bank of per-vCPU [`StealthMsrRouter`]s with an `active` selector.
///
/// One router per vCPU; `active` names the vCPU whose router currently serves
/// MSR exits on the shared bus. The invariant `active < routers.len()` is
/// upheld by the constructors and [`set_active`](Self::set_active) (which
/// rejects out-of-range selections), and `routers` is never empty, so indexing
/// `routers[active]` is always valid.
struct StealthBank {
    routers: Vec<StealthMsrRouter>,
    active: usize,
}

impl StealthBank {
    /// A bank of exactly the given routers (must be non-empty), active vCPU 0.
    fn new(routers: Vec<StealthMsrRouter>) -> Self {
        debug_assert!(!routers.is_empty(), "a stealth bank needs ≥1 router");
        Self { routers, active: 0 }
    }

    /// The active vCPU's router.
    fn active(&mut self) -> &mut StealthMsrRouter {
        &mut self.routers[self.active]
    }

    /// Select the active vCPU. Returns `false` (leaving `active` unchanged) if
    /// `index` names no router in the bank.
    fn set_active(&mut self, index: usize) -> bool {
        if index < self.routers.len() {
            self.active = index;
            true
        } else {
            false
        }
    }
}

impl DeviceBus {
    /// An empty bus with no devices registered.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pio: PioBus::new(),
            mmio: MmioBus::new(),
            pci_reset: None,
            stealth: None,
        }
    }

    /// Install a single stealth MSR router (a one-vCPU bank) so forwarded guest
    /// `RDMSR`/`WRMSR` of the modelled MSRs (APERF/MPERF, the PMC counters, and
    /// the LBR registers) are served from its shadows. Requires the backend to
    /// have userspace MSR forwarding on
    /// ([`KvmBackend::enable_userspace_msr_exits`](crate::kvm_backend::KvmBackend::enable_userspace_msr_exits)).
    ///
    /// For an SMP guest use [`set_stealth_msr_routers`](Self::set_stealth_msr_routers)
    /// to install one router per vCPU.
    pub fn set_stealth_msr_router(&mut self, router: StealthMsrRouter) {
        self.stealth = Some(StealthBank::new(vec![router]));
    }

    /// Install one stealth MSR router per vCPU (a bank), so each vCPU's
    /// forwarded MSR exits are served from its *own* APERF/MPERF/PMC/LBR
    /// shadows. The run loop selects which one answers with
    /// [`set_active_vcpu`](Self::set_active_vcpu) before each guest entry.
    /// `routers[i]` serves vCPU `i`; the active vCPU starts at 0.
    ///
    /// # Panics
    /// Panics if `routers` is empty — a bank must back at least one vCPU.
    pub fn set_stealth_msr_routers(&mut self, routers: Vec<StealthMsrRouter>) {
        assert!(!routers.is_empty(), "a stealth bank needs ≥1 router");
        self.stealth = Some(StealthBank::new(routers));
    }

    /// Select which vCPU's router serves subsequent MSR exits. The run loop
    /// calls this with the vCPU index before entering it, so a forwarded
    /// `RDMSR`/`WRMSR` on vCPU `index` hits `index`'s shadow state.
    ///
    /// Returns `false` (leaving the selection unchanged) if no router is
    /// installed or `index` names no vCPU in the bank.
    pub fn set_active_vcpu(&mut self, index: usize) -> bool {
        self.stealth.as_mut().is_some_and(|b| b.set_active(index))
    }

    /// The currently-active vCPU index (the one whose router serves MSR exits),
    /// or `None` if no router is installed.
    #[must_use]
    pub fn active_vcpu(&self) -> Option<usize> {
        self.stealth.as_ref().map(|b| b.active)
    }

    /// The number of per-vCPU routers installed, or 0 if none.
    #[must_use]
    pub fn stealth_vcpu_count(&self) -> usize {
        self.stealth.as_ref().map_or(0, |b| b.routers.len())
    }

    /// Mutable access to the **active** vCPU's stealth MSR router, if any — so
    /// the run loop can advance its shadow counters (`PmcState::advance_counters`,
    /// `VcpuTimingState::advance`) between guest entries. To reach a specific
    /// vCPU's router regardless of the active selection use
    /// [`stealth_msr_for_mut`](Self::stealth_msr_for_mut).
    #[must_use]
    pub fn stealth_msr_mut(&mut self) -> Option<&mut StealthMsrRouter> {
        self.stealth.as_mut().map(StealthBank::active)
    }

    /// Mutable access to vCPU `index`'s stealth MSR router, if one is installed
    /// for it — independent of which vCPU is active. Used to seed or inspect a
    /// particular vCPU's shadows.
    #[must_use]
    pub fn stealth_msr_for_mut(&mut self, index: usize) -> Option<&mut StealthMsrRouter> {
        self.stealth.as_mut().and_then(|b| b.routers.get_mut(index))
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

    /// Mount a [`SharedPit`] on the PIO bus over `0x40..=0x43`, like
    /// [`add_pit`](Self::add_pit), but keeping a caller-owned handle to the same
    /// PIT. A boxed `Pit` is reachable only through its `PioDevice` methods, so
    /// nothing could advance it once mounted; with a `SharedPit` the run loop /
    /// timer thread holds a clone and calls `tick` to drive channel-0 IRQ0 while
    /// the guest programs the counters through the ports.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the PIT's range overlaps an
    /// already-registered device.
    pub fn add_pit_shared(&mut self, pit: &SharedPit) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(pit.port()))
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
    /// If `root` does not already contain a device at BDF 0:0.0, the **Q35 MCH
    /// host bridge** (`8086:29C0`) is seeded there so a guest enumerating the bus
    /// at boot finds at least the root device (matching real hardware, where the
    /// host bridge always answers). Its `PCIEXBAR` register is seeded from
    /// `root.ecam_base`, so the base the MCH advertises through config space and
    /// the ECAM window the platform decodes (and the MCFG table describes) are
    /// the same address by construction.
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
        self.pci_reset = Some(cam.reset_handle());
        {
            let mut rc = shared.borrow_mut();
            if rc.find_device(&PciBdf::new(0, 0, 0)).is_none() {
                let ecam_base = rc.ecam_base;
                rc.add_device(PcieRootComplex::create_q35_host_bridge(ecam_base));
            }
        }
        self.add_pci_config_io(cam)?;
        self.add_mmio(Box::new(EcamSpace::new(shared.clone())))?;
        Ok(shared)
    }

    /// A clone of the PCI config front-end's `0xCF9` Reset Control latch, set once
    /// [`add_pcie`](Self::add_pcie) has mounted the front-end. Lets the platform
    /// layer poll a guest's `0xCF9`/`reboot=pci` reboot request.
    #[must_use]
    pub fn pci_reset_handle(&self) -> Option<PciResetControl> {
        self.pci_reset.clone()
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

    /// Like [`standard_pc`](Self::standard_pc), but additionally wires the legacy
    /// devices' interrupt lines into `pic` so they actually reach a vCPU. Each
    /// line is wired through [`SharedInterruptController::isa_line`], which
    /// applies the standard-PC interrupt-source overrides: the 8254 PIT's
    /// channel-0 line ([`IRQ_PIT`]) lands on **GSI 2** — the pin the MADT
    /// advertises for the timer — and the COM1 16550's line ([`IRQ_COM1`])
    /// identity-maps to GSI 4. Each device pulses its line through `pic`, which
    /// routes it through the I/O APIC RTE to the destination LAPIC's IRR.
    ///
    /// The caller owns `pic` (it clones a handle into each line) so it can mount
    /// the LAPIC/IOAPIC MMIO, program the redirection table, and read pending
    /// vectors. The redirection entries start masked, so until the guest OS
    /// programs the I/O APIC these lines deliver nothing — exactly as on real
    /// hardware.
    ///
    /// Returns the assembled bus and the [`SharedRootComplex`] handle, as
    /// [`standard_pc`](Self::standard_pc) does.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if any two devices' ranges
    /// overlap (they do not at these canonical addresses).
    pub fn standard_pc_with_interrupts(
        serial_output: SerialOutput,
        pic: &SharedInterruptController,
    ) -> Result<(Self, SharedRootComplex), enlil_devices::bus::BusError> {
        let mut bus = Self::new();

        // I/O APIC lines go through `isa_line`, which applies the standard-PC
        // interrupt-source overrides: the PIT (ISA IRQ0) lands on GSI 2 — the
        // pin the MADT advertises and the guest programs — while COM1 (IRQ4)
        // identity-maps.
        let mut com1 = SerialPort::com1(serial_output);
        com1.attach_irq_line(Box::new(pic.isa_line(IRQ_COM1)));
        bus.add_serial(com1)?;

        let mut pit = Pit::new();
        pit.attach_irq0(Box::new(pic.isa_line(IRQ_PIT)));
        bus.add_pit(pit)?;

        bus.add_ioapic(pic)?;

        let pcie = bus.add_pcie(PcieRootComplex::new(DEFAULT_ECAM_BASE))?;
        Ok((bus, pcie))
    }

    /// Mount the I/O APIC MMIO aperture ([`IoApicMmio`]) at `0xFEC0_0000` over
    /// `pic`, so a guest OS can program the redirection table — routing device
    /// IRQ lines (attached via [`SharedInterruptController::line`]) to vCPU
    /// vectors. Without it the RTEs stay masked and no device IRQ is delivered.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the aperture overlaps an
    /// already-registered MMIO device.
    pub fn add_ioapic(
        &mut self,
        pic: &SharedInterruptController,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.add_mmio(Box::new(IoApicMmio::new(pic.clone())))
    }

    /// Mount the legacy dual-8259 [`SharedPic`] front-end on the PIO bus: the
    /// master's two ports (`0x20`/`0x21`), the slave's (`0xA0`/`0xA1`), and the
    /// chipset ELCR ports (`0x4D0`/`0x4D1`). This is the interrupt controller
    /// early boot programs *before* the OS switches to the I/O APIC; without it
    /// those ports read back as open-bus and the guest cannot mask/EOI or read
    /// the PIC, stalling early-boot interrupt setup. The ELCR lets the guest
    /// select per-line edge vs level triggering (PCI INTx lines are level).
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if any port range overlaps
    /// an already-registered device.
    pub fn add_pic(&mut self, pic: &SharedPic) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(pic.master_port()))?;
        self.add_pio(Box::new(pic.slave_port()))?;
        self.add_pio(Box::new(pic.elcr_port()))?;
        Ok(())
    }

    /// Mount the MC146818 RTC / CMOS front-end on the PIO bus over its index and
    /// data ports (`0x70`/`0x71`). A guest reads the time-of-day and CMOS
    /// equipment config here at boot, and writes the NMI-disable bit through the
    /// index port; without it those ports read back open-bus `0xFF`.
    ///
    /// The caller owns `rtc` (a [`SharedRtc`]) so it can advance the wall clock
    /// (`tick_second`), inject the guest's view of time, and attach the IRQ8
    /// sink — e.g. `rtc.with(|r| r.attach_irq(Box::new(pic.line(8))))` for the
    /// 8259, or `ioapic.isa_line(8)` for the I/O APIC.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the port pair overlaps an
    /// already-registered device.
    pub fn add_rtc(&mut self, rtc: &SharedRtc) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(rtc.port()))
    }

    /// Mount the i8042 PS/2 controller front-end on the PIO bus: the data port
    /// (`0x60`) and the status/command port (`0x64`). It deliberately leaves
    /// `0x61`-`0x63` unclaimed — those are the PC-speaker / NMI status ports of
    /// other chipset functions. A guest probes the keyboard/mouse here early in
    /// boot (Windows before USB HID, Linux's `i8042` driver); without it the
    /// ports read open-bus `0xFF`.
    ///
    /// The caller owns `ps2` (a [`SharedI8042`]) so it can inject host input and
    /// attach the IRQ1 (keyboard) / IRQ12 (mouse) sinks — e.g.
    /// `ps2.attach_kbd_irq(Box::new(pic.line(1)))`.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if either port overlaps an
    /// already-registered device.
    pub fn add_ps2(&mut self, ps2: &SharedI8042) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(ps2.data_port()))?;
        self.add_pio(Box::new(ps2.cmd_port()))?;
        Ok(())
    }

    /// Mount **System Control Port B** ([`SystemControlPortB`]) at `0x61` — the
    /// chipset NMI status/control register that also gates the PIT's channel-2
    /// tone (the PC speaker) and reports its OUT pin and the DRAM-refresh clock.
    /// It lives between the i8042's two ports, which is why [`add_ps2`] leaves
    /// `0x61` unclaimed; this method, coupled to the same [`SharedPit`] the run
    /// loop ticks, fills it in. Without it a guest beeping through the speaker or
    /// polling the refresh bit for timing reads back open-bus `0xFF`.
    ///
    /// [`add_ps2`]: Self::add_ps2
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if `0x61` overlaps an
    /// already-registered device.
    pub fn add_system_control_b(
        &mut self,
        pit: &SharedPit,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(SystemControlPortB::new(pit.clone())))
    }

    /// Mount **System Control Port A** ([`SystemControlPortA`]) at `0x92` — the
    /// chipset fast-A20 / fast-reset register every x86 boot path touches. The
    /// A20 gate (bit 1) reads back enabled (Enlil runs no legacy BIOS A20 dance
    /// and KVM keeps A20 open) and a guest write to it sticks; bit 0 latches a
    /// CPU-reset request edge. Without it `0x92` reads open-bus `0xFF`, so a
    /// guest confirming A20 there sees the wrong state.
    ///
    /// The caller owns `sysctl_a` (a [`SharedSystemControlPortA`]) so the run loop
    /// can poll its one-shot fast-reset latch (`take_reset`) and re-init the vCPU.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if `0x92` overlaps an
    /// already-registered device.
    pub fn add_system_control_a(
        &mut self,
        sysctl_a: &SharedSystemControlPortA,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(sysctl_a.port()))
    }

    /// Mount the **HPET** register block ([`SharedHpet`]) on the MMIO bus at
    /// [`HPET_MMIO_BASE`](enlil_devices::timer::HPET_MMIO_BASE) — the 1 KiB
    /// aperture the ACPI HPET table points the guest at. Windows requires the
    /// HPET for high-resolution timing and Linux uses it as a clocksource; both
    /// read the capability/period and the 64-bit main counter here. The caller
    /// owns `hpet` so the run loop can advance the counter (`tick`) and deliver
    /// the timers' interrupts.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the HPET aperture overlaps
    /// an already-registered MMIO device.
    pub fn add_hpet(&mut self, hpet: &SharedHpet) -> Result<(), enlil_devices::bus::BusError> {
        self.add_mmio(Box::new(hpet.mmio()))
    }

    /// Mount the **ACPI PM Timer** ([`SharedAcpiPmTimer`]) on the PIO bus at the
    /// `PM_TMR_BLK` port (`0x608`) the emitted FADT advertises. An OS reads this
    /// free-running 3.579545 MHz counter to calibrate and cross-check its other
    /// clocks; without it the port reads open-bus `0xFFFF_FFFF` and the guest's
    /// time calibration diverges. The caller owns `pmt` so the run loop can
    /// advance the counter from elapsed wall-clock time.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the `PM_TMR` window overlaps
    /// an already-registered device.
    pub fn add_acpi_pm_timer(
        &mut self,
        pmt: &SharedAcpiPmTimer,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(pmt.port()))
    }

    /// Mount the **ACPI PM1a event/control block** ([`SharedAcpiPm1Block`]) on the
    /// PIO bus over `0x600`..`0x605` — the `PM1a_EVT_BLK`/`PM1a_CNT_BLK` ports the
    /// emitted FADT advertises. This is how a guest OS enters a sleep state; most
    /// importantly, a write of `SLP_TYP | SLP_EN` to the control register is the
    /// **shutdown** path. The caller owns `pm1` so the run loop can poll
    /// `take_sleep` and tear the guest down (and press a virtual power button).
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the block overlaps an
    /// already-registered device.
    pub fn add_acpi_pm1(
        &mut self,
        pm1: &SharedAcpiPm1Block,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(pm1.port()))
    }

    /// Mount the **SMI command port** (`0xB2`, the FADT's `SMI_CMD`) on the PIO
    /// bus, wired to `pm1`'s `SCI_EN`. An ACPI OS enters ACPI mode by writing
    /// `ACPI_ENABLE` here and polling `SCI_EN`; without this port the write hits
    /// open bus, `SCI_EN` never sets, and the OS aborts ACPI init. The same `pm1`
    /// must back the `PM1a` block so the OS sees its poll succeed.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if `0xB2` overlaps an
    /// already-registered device.
    pub fn add_smi_command(
        &mut self,
        pm1: &SharedAcpiPm1Block,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(pm1.smi_command_port()))
    }

    /// Mount the **ACPI GPE0 block** ([`Gpe0Block`]) on the PIO bus over
    /// `0x620`..`0x62F` — the `GPE0_BLK` the emitted FADT advertises. A guest's
    /// ACPICA reads and clears these General-Purpose Event registers during ACPI
    /// init; mounting the block makes its status read back clear (no phantom
    /// events) instead of open-bus `0xFF`.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the block overlaps an
    /// already-registered device.
    pub fn add_gpe0(&mut self) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(Gpe0Block::new()))
    }

    /// Mount the **8237A DMA controllers** and the **DMA page registers** on the
    /// PIO bus: DMA-1 over `0x00`..`0x0F`, DMA-2 over `0xC0`..`0xDF`, and the page
    /// registers over `0x80`..`0x8F`. The model is passive (no transfer engine —
    /// nothing in-tree owns a channel yet), but mounting it means a guest that
    /// `request_region`s and probes ISA DMA at boot (Linux always does) reads back
    /// coherent register state instead of open-bus `0xFF` — an open DMA window is
    /// otherwise a cheap VM tell.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if any of the three windows
    /// overlaps an already-registered device.
    pub fn add_dma_controllers(&mut self) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(Dma8237::primary()))?;
        self.add_pio(Box::new(Dma8237::secondary()))?;
        self.add_pio(Box::new(DmaPageRegisters::new()))
    }

    /// Mount the **QEMU `fw_cfg`** device ([`FwCfgDevice`]) on the PIO bus over
    /// its two registers `0x510` (16-bit item selector) and `0x511` (byte-stream
    /// data). This is the firmware-configuration channel an OVMF/SeaBIOS guest
    /// reads the synthesized ACPI and SMBIOS table set from at boot (via the
    /// `etc/acpi/*` and `etc/smbios/*` files), and the `etc/table-loader` link
    /// script that places and checksums them. The caller populates `fw_cfg`
    /// (e.g. [`FwCfgDevice::add_acpi_tables`] / [`FwCfgDevice::add_smbios`])
    /// before mounting; the device is read-only to the guest after that, so the
    /// bus owns it outright.
    ///
    /// [`FwCfgDevice`]: enlil_devices::fw_cfg::FwCfgDevice
    /// [`FwCfgDevice::add_acpi_tables`]: enlil_devices::fw_cfg::FwCfgDevice::add_acpi_tables
    /// [`FwCfgDevice::add_smbios`]: enlil_devices::fw_cfg::FwCfgDevice::add_smbios
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if `0x510`..`0x512` overlaps
    /// an already-registered device.
    pub fn add_fw_cfg(
        &mut self,
        fw_cfg: enlil_devices::fw_cfg::FwCfgDevice,
    ) -> Result<(), enlil_devices::bus::BusError> {
        self.add_pio(Box::new(fw_cfg))
    }

    /// Like [`standard_pc`](Self::standard_pc), but wires the legacy devices'
    /// interrupt lines into the dual-8259 `pic` (the early-boot interrupt
    /// controller) and mounts its four ports: the 8254 PIT's channel-0 line
    /// drives [`IRQ_PIT`] and the COM1 16550's line drives [`IRQ_COM1`], each
    /// latching an edge on the master 8259 that — once the guest has run the ICW
    /// sequence and unmasked the line — asserts `INTR` with the programmed
    /// vector.
    ///
    /// This is the PIC counterpart to
    /// [`standard_pc_with_interrupts`](Self::standard_pc_with_interrupts) (which
    /// wires the same lines into the I/O APIC). The caller owns `pic` so it can
    /// read the pending vector / acknowledge the INTA from the vCPU run loop.
    ///
    /// Returns the assembled bus and the [`SharedRootComplex`] handle, as
    /// [`standard_pc`](Self::standard_pc) does.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if any two devices' ranges
    /// overlap (they do not at these canonical addresses).
    pub fn standard_pc_with_pic(
        serial_output: SerialOutput,
        pic: &SharedPic,
    ) -> Result<(Self, SharedRootComplex), enlil_devices::bus::BusError> {
        let mut bus = Self::new();

        let mut com1 = SerialPort::com1(serial_output);
        com1.attach_irq_line(Box::new(pic.line(IRQ_COM1)));
        bus.add_serial(com1)?;

        let mut pit = Pit::new();
        pit.attach_irq0(Box::new(pic.line(IRQ_PIT)));
        bus.add_pit(pit)?;

        bus.add_pic(pic)?;

        let pcie = bus.add_pcie(PcieRootComplex::new(DEFAULT_ECAM_BASE))?;
        Ok((bus, pcie))
    }

    /// The transparent, hardware-accurate interrupt config: each legacy device
    /// line drives **both** the dual-8259 `pic` and the I/O APIC `ioapic`, and
    /// both front-ends are mounted (PIC ports `0x20`/`0x21`/`0xA0`/`0xA1` and the
    /// I/O APIC aperture at `0xFEC0_0000`).
    ///
    /// On real hardware an ISA IRQ line is wired to *both* the 8259 input and the
    /// I/O APIC pin; firmware/the OS leaves one path masked (PIC mode at boot,
    /// then it masks the PIC and switches to the I/O APIC). Modelling both as
    /// live — with the guest masking whichever it isn't using — is what keeps the
    /// switchover transparent: the device asserts one line and whichever
    /// controller the guest has unmasked delivers it. PIT channel-0 drives
    /// [`IRQ_PIT`] and COM1 drives [`IRQ_COM1`] on both controllers.
    ///
    /// The two controllers see different "pins" for the same line: the 8259
    /// takes the bare ISA IRQ (timer on IRQ0), while the I/O APIC side goes
    /// through [`SharedInterruptController::isa_line`], which applies the MADT
    /// interrupt-source overrides — so the PIT lands on GSI 2 (the pin the guest
    /// programs from the MADT) on the APIC path and on IRQ0 on the PIC path,
    /// exactly as a real PC/AT wires it.
    ///
    /// Returns the assembled bus and the [`SharedRootComplex`] handle.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if any two devices' ranges
    /// overlap (they do not at these canonical addresses).
    pub fn standard_pc_with_dual_irq(
        serial_output: SerialOutput,
        pic: &SharedPic,
        ioapic: &SharedInterruptController,
    ) -> Result<(Self, SharedRootComplex), enlil_devices::bus::BusError> {
        let mut bus = Self::new();

        // Tee each device line to both controllers (see [`dual_irq_line`]).
        let mut com1 = SerialPort::com1(serial_output);
        com1.attach_irq_line(Box::new(dual_irq_line(pic, ioapic, IRQ_COM1)));
        bus.add_serial(com1)?;

        let mut pit = Pit::new();
        pit.attach_irq0(Box::new(dual_irq_line(pic, ioapic, IRQ_PIT)));
        bus.add_pit(pit)?;

        bus.add_pic(pic)?;
        bus.add_ioapic(ioapic)?;

        let pcie = bus.add_pcie(PcieRootComplex::new(DEFAULT_ECAM_BASE))?;
        Ok((bus, pcie))
    }

    /// Assemble a **complete** transparent standard PC in one call: every legacy
    /// device a guest touches at boot, mounted at its canonical address and with
    /// its interrupt line teed into **both** the dual-8259 PIC and the I/O APIC
    /// (the [`standard_pc_with_dual_irq`](Self::standard_pc_with_dual_irq) model),
    /// so the boot-time PIC→APIC switchover is seamless on every line — not just
    /// the PIT and COM1.
    ///
    /// On top of [`standard_pc_with_dual_irq`](Self::standard_pc_with_dual_irq)'s
    /// COM1 (`IRQ_COM1`) and PIT (`IRQ_PIT`), this also mounts and IRQ-wires the
    /// devices the earlier dual-IRQ factory left out:
    /// - the **MC146818 RTC/CMOS** (`0x70`/`0x71`, `RTC_IRQ` = 8), its wall clock
    ///   seeded from `rtc_unix_secs` (seconds since the Unix epoch);
    /// - the **i8042 PS/2 controller** (`0x60`/`0x64`), keyboard on
    ///   `PS2_KBD_IRQ` = 1 and mouse on `PS2_MOUSE_IRQ` = 12.
    ///
    /// The PIT is mounted as a [`SharedPit`] (not a boxed `Pit`) so the run loop /
    /// timer thread can `tick` it after assembly; the RTC and PS/2 are likewise
    /// returned as their shared handles so the caller can advance the clock
    /// (`tick_second`) and inject host input. Both interrupt controllers are
    /// created here (`vcpu_count` LAPICs) and returned, along with the
    /// [`SharedRootComplex`], in a [`StandardPc`] bundle — the single entry point
    /// the KVM run loop binds to.
    ///
    /// The APIC side of every line goes through
    /// [`SharedInterruptController::isa_line`], which applies the MADT
    /// interrupt-source overrides (the PIT lands on GSI 2); the PIC side takes the
    /// bare ISA IRQ, exactly as a real PC/AT wires each line to both controllers.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if any two devices' ranges
    /// overlap (they do not at these canonical addresses, so this is effectively
    /// infallible for the default layout).
    pub fn standard_pc_complete(
        serial_output: SerialOutput,
        rtc_unix_secs: u64,
        vcpu_count: u8,
    ) -> Result<StandardPc, enlil_devices::bus::BusError> {
        let pic = SharedPic::new();
        let ioapic = SharedInterruptController::new(vcpu_count);
        let rtc = SharedRtc::new(RtcTime::from_unix(rtc_unix_secs));
        let ps2 = SharedI8042::new();
        let pit = SharedPit::new();

        let mut bus = Self::new();

        // COM1 16550 (IRQ4).
        let mut com1 = SerialPort::com1(serial_output);
        com1.attach_irq_line(Box::new(dual_irq_line(&pic, &ioapic, IRQ_COM1)));
        bus.add_serial(com1)?;

        // 8254 PIT channel-0 (IRQ0 → GSI 2 on the APIC path). Shared so the
        // timer thread can tick it once mounted.
        pit.with(|p| p.attach_irq0(Box::new(dual_irq_line(&pic, &ioapic, IRQ_PIT))));
        bus.add_pit_shared(&pit)?;

        // System Control Port B (0x61): PIT channel-2 gate + PC speaker, sharing
        // the same PIT handle.
        bus.add_system_control_b(&pit)?;

        // System Control Port A (0x92): fast A20 (enabled) + fast reset. Shared so
        // the run loop can poll the reset latch.
        let sysctl_a = SharedSystemControlPortA::new();
        bus.add_system_control_a(&sysctl_a)?;

        // MC146818 RTC/CMOS (IRQ8).
        rtc.with(|r| r.attach_irq(Box::new(dual_irq_line(&pic, &ioapic, RTC_IRQ))));
        bus.add_rtc(&rtc)?;

        // i8042 PS/2: keyboard (IRQ1) + mouse (IRQ12).
        ps2.attach_kbd_irq(Box::new(dual_irq_line(&pic, &ioapic, PS2_KBD_IRQ)));
        ps2.attach_mouse_irq(Box::new(dual_irq_line(&pic, &ioapic, PS2_MOUSE_IRQ)));
        bus.add_ps2(&ps2)?;

        // Both interrupt-controller front-ends.
        bus.add_pic(&pic)?;
        bus.add_ioapic(&ioapic)?;

        // HPET register block (0xFED0_0000) — Windows requires it, Linux uses it
        // as a clocksource. Shared so the run loop can advance the counter.
        let hpet = SharedHpet::new();
        bus.add_hpet(&hpet)?;

        // ACPI PM timer (0x608) — the FADT-advertised free-running clock the OS
        // calibrates against. Shared so the run loop can advance it.
        let pm_timer = SharedAcpiPmTimer::new();
        bus.add_acpi_pm_timer(&pm_timer)?;

        // ACPI PM1a event/control block (0x600/0x604) — the OS's sleep/shutdown
        // path. Shared so the run loop can poll the S5 sleep request.
        let pm1 = SharedAcpiPm1Block::new();
        bus.add_acpi_pm1(&pm1)?;

        // SMI command port (0xB2) — wired to the same PM1 block so the OS's
        // ACPI-enable handshake (write ACPI_ENABLE, poll SCI_EN) completes.
        bus.add_smi_command(&pm1)?;

        // ACPI GPE0 block (0x620) — keep its status quiet during ACPI init.
        bus.add_gpe0()?;

        // 8237A DMA controllers (0x00-0x0F, 0xC0-0xDF) + page registers
        // (0x80-0x8F) — passive register model so an ISA-DMA probe reads coherent
        // state, not open bus. Claimed in the DSDT's SYSR _CRS.
        bus.add_dma_controllers()?;

        // PCIe config space (legacy CAM + ECAM), seeded with a host bridge.
        let pcie = bus.add_pcie(PcieRootComplex::new(DEFAULT_ECAM_BASE))?;
        // The 0xCF9 reset latch the CAM front-end just mounted (reboot=pci path).
        let pci_reset = bus
            .pci_reset_handle()
            .expect("add_pcie mounts the 0xCF9 reset latch");

        // Seed the ICH9 LPC bridge / PCI interrupt router at 00:1F.0 (its config
        // space holds the PIRQ routing registers a guest programs), and its
        // SMBus sibling at 00:1F.3.
        {
            let mut rc = pcie.borrow_mut();
            if rc.find_device(&ICH9_LPC_BDF).is_none() {
                let mut bridge = PcieRootComplex::create_isa_bridge(
                    ICH9_LPC_BDF,
                    vendors::INTEL,
                    ICH9_LPC_DEVICE_ID,
                );
                // D31 is multifunction on every ICH9 (the SMBus controller below
                // is function 3), so function 0's header must say so or the
                // guest never probes past it.
                bridge.set_header_type(0x80);
                // Board firmware stamps the board vendor's subsystem IDs on
                // every onboard function (the generic ISA-bridge factory can't
                // assume a board, so it's done at the platform seeding site).
                bridge.set_subsystem(BOARD_SUBSYSTEM_VENDOR_ID, BOARD_SUBSYSTEM_DEVICE_ID);
                // PMBASE/ACPI_CNTL: on an ICH the ACPI PM I/O block's location
                // physically comes from these LPC registers — firmware programs
                // them and then writes the same ports into the FADT. The PM
                // models already sit at the ICH9 fixed offsets from 0x600
                // (PM1 +0/+4, PM_TMR +8, GPE0 +0x20), so encode that base,
                // enabled, with the SCI on the FADT's IRQ.
                bridge.write_u32(
                    enlil_devices::pcie::LPC_PMBASE_OFFSET,
                    u32::from(enlil_devices::chipset::PM1_EVT_PORT) | 1,
                );
                bridge.write_u8(
                    enlil_devices::pcie::LPC_ACPI_CNTL_OFFSET,
                    enlil_devices::pcie::ACPI_CNTL_ACPI_EN
                        | enlil_devices::pcie::acpi_cntl_sci_select(
                            enlil_devices::chipset::SCI_IRQ,
                        ),
                );
                // Firmware programs the PIRQ routing registers out of their 0x80
                // reset (disabled) state to the defaults the DSDT advertises — the
                // same PIRQ_DEFAULT_IRQS the link devices' _CRS reports — so a guest
                // reading PIRQ[A-D]_ROUT, the PirqRouter that syncs from it, and the
                // ACPI namespace all agree on PCI-mode routing.
                for (line, &irq) in PIRQ_DEFAULT_IRQS.iter().enumerate() {
                    bridge.write_u8(PIRQ_ROUTE_CONFIG_BASE + line as u16, irq);
                }
                rc.add_device(bridge);
            }
            if rc.find_device(&ICH9_SMBUS_BDF).is_none() {
                rc.add_device(PcieRootComplex::create_ich9_smbus(SMBUS_IO_BASE));
            }
        }
        // The live SMBus host register file behind the BAR the config function
        // advertises (an empty bus: probes complete with DEV_ERR, not open bus).
        // Its completion interrupt (INTB# of device 31) is delivered through the
        // live PIRQ routing into both controllers, so a guest driving the
        // controller with INTREN set gets the interrupt config space promised.
        let mut smbus = SmbusHost::new();
        {
            let pcie = pcie.clone();
            let pic = pic.clone();
            let ioapic = ioapic.clone();
            smbus.set_interrupt_line(move |level| {
                route_pci_intx(
                    &pcie,
                    &pic,
                    &ioapic,
                    ICH9_SMBUS_BDF.device,
                    SMBUS_INTERRUPT_PIN,
                    level,
                );
            });
        }
        bus.add_pio(Box::new(smbus))?;

        // The discrete xHCI USB 3.0 controller (Phase 4.4): config function at
        // XHCI_BDF, register file behind its 64 KiB BAR0, INTA# through the
        // live PIRQ routing. The routing engine attaches each guest's routed
        // USB devices to this controller's ports.
        {
            let mut rc = pcie.borrow_mut();
            if rc.find_device(&XHCI_BDF).is_none() {
                rc.add_device(PcieRootComplex::create_xhci_controller(
                    XHCI_BDF,
                    XHCI_MMIO_BASE,
                ));
            }
        }
        let xhci: SharedXhci = {
            // The run loop has guest memory in hand at doorbell time, so drive
            // transfer rings from the guest's TR Dequeue Pointers (the real
            // hardware path) rather than the internal submit_transfer queue.
            let mut controller = VirtualXhciController::new(XHCI_PORTS);
            controller.set_guest_resident_transfers(true);
            Rc::new(RefCell::new(controller))
        };
        let mut xhci_mmio =
            XhciMmio::new(Rc::clone(&xhci), u64::from(XHCI_MMIO_BASE), XHCI_MMIO_SIZE);
        {
            let pcie = pcie.clone();
            let pic = pic.clone();
            let ioapic = ioapic.clone();
            xhci_mmio.set_interrupt_line(move |level| {
                route_pci_intx(&pcie, &pic, &ioapic, XHCI_BDF.device, 1, level);
            });
        }
        bus.add_mmio(Box::new(xhci_mmio))?;

        Ok(StandardPc {
            bus,
            pcie,
            pic,
            ioapic,
            rtc,
            ps2,
            pit,
            hpet,
            pm_timer,
            pm1,
            sysctl_a,
            pci_reset,
            xhci,
        })
    }
}

/// A platform-level event a guest raised that the vCPU run loop must act on,
/// surfaced by [`StandardPc::poll_platform_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformEvent {
    /// The guest committed an ACPI sleep transition (the `SLP_TYP` value). The
    /// DSDT's `_S5` value means **power off**; other values are lighter sleep
    /// states the run loop maps as it implements them.
    Sleep(u8),
    /// The guest pulsed a fast/INIT CPU reset via System Control Port A (`0x92`
    /// bit 0). The run loop should reinitialise the vCPU to its reset vector.
    Reset,
}

/// Build one device-IRQ sink that drives **both** interrupt controllers from a
/// single line, as a real PC/AT wires each ISA IRQ to both the 8259 and the I/O
/// APIC. The PIC takes the bare ISA `irq`; the APIC side goes through
/// [`SharedInterruptController::isa_line`], which applies the MADT
/// interrupt-source overrides (e.g. IRQ0 → GSI 2). The returned closure is
/// `Fn(bool) + Send`, so it satisfies both crate-local `IrqLine` traits (the one
/// in [`crate::serial`] and `enlil_devices`') via their blanket impls and can be
/// boxed for either device's `attach_*` method.
fn dual_irq_line(
    pic: &SharedPic,
    ioapic: &SharedInterruptController,
    irq: u8,
) -> impl Fn(bool) + Send + use<> {
    let pic_sink = pic.line(irq);
    let apic_sink = ioapic.isa_line(irq);
    move |level: bool| {
        pic_sink(level);
        apic_sink(level);
    }
}

/// A fully-assembled transparent standard PC: the [`DeviceBus`] with every legacy
/// device mounted and IRQ-wired, plus the caller-owned shared handles for the
/// pieces that must be driven or inspected from outside the bus. Produced by
/// [`DeviceBus::standard_pc_complete`].
pub struct StandardPc {
    /// The assembled device bus (the [`VmExitHandler`] the backend binds to).
    pub bus: DeviceBus,
    /// The `PCIe` root complex, for attaching further PCI devices post-assembly.
    pub pcie: SharedRootComplex,
    /// The dual-8259 PIC — read the pending vector / acknowledge INTA from here.
    pub pic: SharedPic,
    /// The I/O APIC + LAPIC complex — read per-vCPU pending vectors from here.
    pub ioapic: SharedInterruptController,
    /// The MC146818 RTC/CMOS — advance the wall clock (`tick_second`) from here.
    pub rtc: SharedRtc,
    /// The i8042 PS/2 controller — inject host keyboard/mouse input from here.
    pub ps2: SharedI8042,
    /// The 8254 PIT — `tick` channel-0 from the run loop / timer thread.
    pub pit: SharedPit,
    /// The HPET — `tick` the main counter from the run loop / timer thread.
    pub hpet: SharedHpet,
    /// The ACPI PM timer — `advance` the counter from the run loop / timer thread.
    pub pm_timer: SharedAcpiPmTimer,
    /// The ACPI PM1a block — poll `take_sleep` from the run loop to handle
    /// guest-initiated shutdown / sleep.
    pub pm1: SharedAcpiPm1Block,
    /// System Control Port A (`0x92`) — poll `take_reset` from the run loop to
    /// handle a guest-initiated fast/INIT reset, and read the live A20 state.
    pub sysctl_a: SharedSystemControlPortA,
    /// The chipset Reset Control Register (`0xCF9`) — poll `take_reset` from the
    /// run loop to handle a guest-initiated `reboot=pci` reset (the modern path,
    /// alongside `0x92`).
    pub pci_reset: PciResetControl,
    /// The virtual xHCI controller — hot-plug routed USB devices through
    /// [`connect_usb_device`](Self::connect_usb_device), not this handle, so
    /// the port-status interrupt is delivered too.
    pub xhci: SharedXhci,
}

impl StandardPc {
    /// Poll the platform-control latches a guest can raise that the vCPU run loop
    /// must act on outside the normal exit path — currently an ACPI sleep/shutdown
    /// ([`PlatformEvent::Sleep`], via the PM1a block) and a CPU reset
    /// ([`PlatformEvent::Reset`], via either System Control Port A `0x92` or the
    /// chipset Reset Control Register `0xCF9`). Returns the highest-priority
    /// pending event (sleep before reset) and consumes its latch; the run loop
    /// calls this each iteration and acts on what it returns.
    #[must_use]
    pub fn poll_platform_events(&self) -> Option<PlatformEvent> {
        if let Some(slp_typ) = self.pm1.take_sleep() {
            return Some(PlatformEvent::Sleep(slp_typ));
        }
        // Both reset latches must be drained, so a pending request on the
        // not-first source isn't stranded behind an early return.
        let reset = self.sysctl_a.take_reset() | self.pci_reset.take_reset();
        if reset {
            return Some(PlatformEvent::Reset);
        }
        None
    }

    /// Synthesize the ACPI table set for `config` and mount a populated QEMU
    /// `fw_cfg` device that delivers it through the **full firmware path** —
    /// `etc/acpi/rsdp`, `etc/acpi/tables`, and the `etc/table-loader` command
    /// stream that relocates and re-checksums the tables at the guest-chosen
    /// load address (see [`FwCfgDevice::add_acpi_with_loader`]). The device is
    /// mounted read-only on the PIO bus at `0x510`/`0x511`.
    ///
    /// This is the assembled-platform counterpart to the standalone
    /// [`FwCfgDevice::add_acpi_with_loader`]: a guest's OVMF/SeaBIOS reads the
    /// tables off this `fw_cfg` and installs them itself. `fw_cfg` is *not*
    /// mounted by [`standard_pc_complete`](Self::standard_pc_complete), so call
    /// this once on the assembled PC when ACPI delivery is wanted.
    ///
    /// [`FwCfgDevice`]: enlil_devices::fw_cfg::FwCfgDevice
    /// [`FwCfgDevice::add_acpi_with_loader`]: enlil_devices::fw_cfg::FwCfgDevice::add_acpi_with_loader
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the `fw_cfg` ports overlap
    /// an already-mounted device (e.g. if called twice).
    pub fn install_acpi_fw_cfg(
        &mut self,
        config: &enlil_devices::acpi::AcpiTableSetConfig,
    ) -> Result<(), enlil_devices::bus::BusError> {
        let mut fw_cfg = enlil_devices::fw_cfg::FwCfgDevice::new();
        fw_cfg.add_acpi_with_loader(config);
        self.bus.add_fw_cfg(fw_cfg)
    }

    /// Mount a single `fw_cfg` device delivering the **complete firmware table
    /// set** a guest's OVMF/SeaBIOS reads at boot: the ACPI files plus the
    /// `etc/table-loader` (via [`FwCfgDevice::add_acpi_with_loader`]) **and** the
    /// SMBIOS file set `etc/smbios/smbios-{anchor,tables}` (via
    /// [`FwCfgDevice::add_smbios_from_config`]).
    ///
    /// SMBIOS is delivered as plain files, not through `etc/table-loader`:
    /// OVMF's `SmbiosPlatformDxe` reads the anchor and structure table and
    /// re-installs the structures via the EFI SMBIOS protocol itself, computing
    /// the table address (it does not rely on a bios-linker-loader `ADD_POINTER`
    /// for SMBIOS, and neither does QEMU). Call this *instead of*
    /// [`install_acpi_fw_cfg`](Self::install_acpi_fw_cfg) — both mount the
    /// `0x510`/`0x511` registers, so calling both errors on the port overlap.
    ///
    /// # Errors
    /// Propagates [`enlil_devices::bus::BusError`] if the `fw_cfg` ports overlap
    /// an already-mounted device.
    pub fn install_firmware_tables(
        &mut self,
        acpi: &enlil_devices::acpi::AcpiTableSetConfig,
        smbios: &enlil_devices::smbios::SmbiosConfig,
    ) -> Result<(), enlil_devices::bus::BusError> {
        let mut fw_cfg = enlil_devices::fw_cfg::FwCfgDevice::new();
        fw_cfg.add_acpi_with_loader(acpi);
        fw_cfg.add_smbios_from_config(smbios);
        self.bus.add_fw_cfg(fw_cfg)
    }

    /// Reference-cycle delta used to seed the timing shadows at install so the
    /// guest's first APERF/MPERF read already shows the model's core/ref ratio
    /// rather than the all-zero 1.0 identity (a VM tell in its own right — see
    /// roadmap 5.4 and [`VcpuTimingState::advance`]). One million reference
    /// cycles is sub-millisecond at multi-GHz and well below any counter wrap.
    ///
    /// [`VcpuTimingState::advance`]: crate::timing_stealth::VcpuTimingState::advance
    pub const STEALTH_SEED_REF_CYCLES: u64 = 1_000_000;

    /// Install a per-vCPU stealth MSR router on the device bus and return the
    /// shared [`VcpuTimingState`](crate::timing_stealth::VcpuTimingState) handle
    /// the run loop drives.
    ///
    /// Builds a [`StealthMsrRouter`] (a fresh `PmcState` and an `LbrState` for
    /// `platform`) over a new timing state, seeds the APERF/MPERF shadows with
    /// one [`STEALTH_SEED_REF_CYCLES`](Self::STEALTH_SEED_REF_CYCLES) advance at
    /// the default [`PmcRateModel`] rate so a guest reading APERF/MPERF
    /// immediately sees the non-unity model ratio, installs the router on the
    /// bus, and hands back the `Arc` so the run loop can call
    /// `on_vmexit`/`on_vmresume` around `KVM_RUN` and `advance` the shadows.
    ///
    /// The backend-side stealth setup (`enable_userspace_msr_exits` to forward
    /// the MSR exits, `clear_cpuid_hypervisor_bit`) is configured separately on
    /// the [`KvmBackend`](crate::kvm_backend::KvmBackend).
    pub fn install_stealth_msr_router(
        &mut self,
        platform: enlil_devices::stealth::lbr::LbrPlatform,
    ) -> std::sync::Arc<crate::timing_stealth::VcpuTimingState> {
        let (router, timing) = Self::seeded_stealth_router(platform);
        self.bus.set_stealth_msr_router(router);
        timing
    }

    /// Install `vcpu_count` independent stealth MSR routers on the device bus —
    /// one per vCPU — and return the per-vCPU shared
    /// [`VcpuTimingState`](crate::timing_stealth::VcpuTimingState) handles
    /// (`handles[i]` drives vCPU `i`).
    ///
    /// Each router is an independent [`StealthMsrRouter`] (its own `PmcState`,
    /// `LbrState`, and timing shadow) seeded exactly as
    /// [`install_stealth_msr_router`](Self::install_stealth_msr_router) seeds the
    /// single-vCPU case, so every vCPU's first APERF/MPERF read shows the model
    /// ratio rather than the 1.0 identity. An SMP guest reading a per-CPU
    /// counter on vCPU `i` then sees `i`'s own monotonic shadow, not a value
    /// shared with (and racing) the other vCPUs. The run loop selects which one
    /// answers with [`DeviceBus::set_active_vcpu`] before each entry.
    ///
    /// # Panics
    /// Panics if `vcpu_count` is 0 — a bank must back at least one vCPU.
    pub fn install_stealth_msr_routers(
        &mut self,
        platform: enlil_devices::stealth::lbr::LbrPlatform,
        vcpu_count: usize,
    ) -> Vec<std::sync::Arc<crate::timing_stealth::VcpuTimingState>> {
        assert!(vcpu_count > 0, "a stealth bank needs ≥1 vCPU");
        let (routers, handles): (Vec<_>, Vec<_>) = (0..vcpu_count)
            .map(|_| Self::seeded_stealth_router(platform))
            .unzip();
        self.bus.set_stealth_msr_routers(routers);
        handles
    }

    /// Build one stealth MSR router for `platform` over a fresh timing state,
    /// seeded with a single [`STEALTH_SEED_REF_CYCLES`](Self::STEALTH_SEED_REF_CYCLES)
    /// advance at the default [`PmcRateModel`] rate, and return it together with
    /// a clone of its shared timing handle. Shared by the single- and
    /// multi-vCPU install paths so they seed identically.
    fn seeded_stealth_router(
        platform: enlil_devices::stealth::lbr::LbrPlatform,
    ) -> (
        StealthMsrRouter,
        std::sync::Arc<crate::timing_stealth::VcpuTimingState>,
    ) {
        use enlil_devices::stealth::{lbr::LbrState, pmc::PmcRateModel};
        let timing = crate::timing_stealth::VcpuTimingState::new();
        timing.advance(Self::STEALTH_SEED_REF_CYCLES, &PmcRateModel::DEFAULT);
        let router = StealthMsrRouter::new(std::sync::Arc::clone(&timing), LbrState::new(platform));
        (router, timing)
    }

    /// Drive a PCI device's level-triggered `INTx` line into the interrupt fabric,
    /// the way a real south-bridge does. `slot` is the device's PCI device number,
    /// `pin` its interrupt pin (`1`=INTA..`4`=INTD, from config `0x3D`), and
    /// `level` the asserted state.
    ///
    /// The routing is read **live** from the ICH9 LPC bridge's config space (the
    /// `PIRQ[A-D]_ROUT` registers a guest programmed), so it always reflects what the
    /// guest configured. The line is driven into *both* controllers, matching the
    /// hardware: the **8259** sees the routed ISA IRQ as a *level* line (the guest
    /// must have set it level in the ELCR — what PCI interrupts require), and the
    /// **I/O APIC** sees the PIRQ line's fixed GSI (16-19). Whichever path the
    /// guest has unmasked delivers it; deasserting (`level = false`) withdraws it.
    /// Hot-plug a USB device onto the virtual xHCI's root-hub `port`
    /// (0-based) at the xHCI speed code (1=FS, 2=LS, 3=HS, 4=SS) — or detach
    /// with `connected = false`. Flips the port, posts the Port Status Change
    /// event, and drives the controller's `INTA#` through the live PIRQ
    /// routing so a guest with the interrupter enabled is notified exactly as
    /// on hardware.
    pub fn connect_usb_device(&self, port: usize, speed: u8, connected: bool) -> bool {
        let (changed, level) = {
            let mut xhci = self.xhci.borrow_mut();
            let changed = if connected {
                xhci.connect_device(port, speed)
            } else {
                xhci.disconnect_device(port)
            };
            (changed, xhci.intx_level())
        };
        if changed {
            self.assert_pci_intx(XHCI_BDF.device, 1, level);
        }
        changed
    }

    /// Attach a routed USB device of the given [`UsbSpeed`] to the lowest free
    /// root-hub port, delivering the port-status interrupt through the live
    /// PIRQ routing. Returns the 0-based port it landed on (the handle the
    /// routing engine records for a later detach), or `None` if the
    /// controller's ports are all occupied. This is the seam the
    /// [routing engine](enlil_devices::usb::RoutingState) drives: it decides
    /// *which guest* a device goes to; this attaches it to that guest's
    /// controller without picking a port itself.
    pub fn attach_usb_device(&self, speed: UsbSpeed) -> Option<usize> {
        let (port, level) = {
            let mut xhci = self.xhci.borrow_mut();
            (xhci.attach_device(speed), xhci.intx_level())
        };
        if port.is_some() {
            self.assert_pci_intx(XHCI_BDF.device, 1, level);
        }
        port
    }

    /// Drain any xHCI doorbells the guest rang against guest memory, then
    /// reconcile the controller's `INTA#` with what the servicing produced.
    ///
    /// This is the run loop's USB DMA entry point. A guest rings a doorbell by
    /// writing the doorbell register; that MMIO exit reaches
    /// [`XhciMmio`](enlil_devices::usb::XhciMmio) and *latches* the doorbell
    /// (the ring can't be processed there — guest memory isn't in hand on an
    /// MMIO write). Call this after [`run_vcpu`](KvmBackend::run_vcpu) returns
    /// with the VM's [`GuestMemory`](KvmBackend::guest_memory) so the command
    /// and transfer rings are processed and the produced events delivered into
    /// the guest's event ring. The interrupt line is then driven to match the
    /// controller's resulting state, exactly as the MMIO write path does.
    pub fn service_usb_dma(&self, mem: &mut dyn DmaMemory) {
        let level = {
            let mut xhci = self.xhci.borrow_mut();
            xhci.service_doorbells(mem);
            xhci.intx_level()
        };
        self.assert_pci_intx(XHCI_BDF.device, 1, level);
    }

    /// Flush xHCI events the controller queued *outside* doorbell servicing —
    /// hot-plug Port Status Change events above all — into the guest's event
    /// ring, returning how many were delivered.
    ///
    /// Hot-plug ([`connect_usb_device`](Self::connect_usb_device) /
    /// [`attach_usb_device`](Self::attach_usb_device)) posts the event and
    /// asserts `INTA#` immediately, but the event TRB itself can only be
    /// written once guest memory is in hand; the run loop calls this after such
    /// a notification with the VM's [`GuestMemory`](KvmBackend::guest_memory).
    /// Returns zero until the driver has programmed `ERSTBA`/`ERSTSZ`.
    /// [`service_usb_dma`](Self::service_usb_dma) flushes on its own, so this is
    /// only needed for the non-doorbell event sources.
    pub fn flush_usb_events(&self, mem: &mut dyn DmaMemory) -> usize {
        self.xhci.borrow_mut().flush_events(mem)
    }

    pub fn assert_pci_intx(&self, slot: u8, pin: u8, level: bool) {
        route_pci_intx(&self.pcie, &self.pic, &self.ioapic, slot, pin, level);
    }

    /// Advance every free-running platform clock by one elapsed-time delta of `ns`
    /// nanoseconds, keeping the three timekeeping sources a guest cross-checks
    /// coherent from a single time base. This is the timekeeping core a vCPU run
    /// loop / timer thread calls each iteration:
    ///
    /// - the **8254 PIT** (channel 0) advances and, when it crosses terminal
    ///   count, pulses its IRQ0 line — which the factory already teed into both
    ///   interrupt controllers, so the timer interrupt is delivered automatically;
    /// - the **HPET** main counter advances at its 10 MHz rate (only while the
    ///   guest has enabled it); each HPET timer that fires while routed through
    ///   the **I/O APIC** (the normal mode) is delivered to its configured GSI as
    ///   an edge, and the fired `(timer, gsi)` pairs are also **returned** for
    ///   observability;
    /// - the **ACPI PM timer** advances at its fixed 3.579545 MHz rate.
    ///
    /// In HPET **legacy-replacement** mode (the guest set the HPET's legacy bit),
    /// timer 0 stands in for the **PIT** (IRQ0) and timer 1 for the **RTC** (IRQ8);
    /// those fired timers are delivered through the ISA override path into *both*
    /// controllers (like the legacy devices they replace). A guest that enables
    /// legacy replacement drives its periodic interrupt from HPET timer 0 and does
    /// not also program the PIT, so the PIT (still ticked above) stays idle and
    /// does not double-fire. The MC146818 RTC is driven separately
    /// (`rtc.tick_second()` once per wall second).
    #[must_use]
    pub fn advance_clocks(&self, ns: u64) -> Vec<(usize, u8)> {
        // PIT: tick() takes nanoseconds directly and pulses the wired IRQ0 sink.
        let _ = self.pit.tick(ns);
        // ACPI PM timer: convert the elapsed time to its 3.579545 MHz ticks.
        self.pm_timer.advance(AcpiPmTimer::ns_to_ticks(ns));
        // HPET: convert to its 10 MHz counter ticks; collect any fired timers.
        let fired = self.hpet.tick(ns / HPET_TICK_NS);

        // Deliver each fired timer as an edge (assert then deassert).
        let legacy = self.hpet.with(|h| h.legacy_routing());
        for &(timer, gsi) in &fired {
            if legacy && timer <= 1 {
                // Legacy replacement: timer 0 -> IRQ0, timer 1 -> IRQ8, into both
                // the 8259 and the I/O APIC (via the ISA-override path), exactly
                // like the PIT/RTC lines this mode replaces.
                let isa = if timer == 0 { IRQ_PIT } else { RTC_IRQ };
                let pic_line = self.pic.line(isa);
                let apic_line = self.ioapic.isa_line(isa);
                pic_line(true);
                pic_line(false);
                apic_line(true);
                apic_line(false);
            } else {
                // Normal mode: route to the timer's configured I/O APIC GSI.
                let line = self.ioapic.line(gsi);
                line(true);
                line(false);
            }
        }
        fired
    }
}

/// Drive a PCI device's level-triggered `INTx` line into the interrupt fabric
/// through the **live** PIRQ routing: the `PIRQ[A-D]_ROUT` bytes are read from
/// the ICH9 LPC bridge's config space at assertion time, so the route always
/// reflects what the guest last programmed. Shared by
/// [`StandardPc::assert_pci_intx`] (the run loop's path) and the interrupt
/// sinks of bus-internal PCI functions (e.g. the SMBus controller), which
/// capture clones of these shared handles.
fn route_pci_intx(
    pcie: &SharedRootComplex,
    pic: &SharedPic,
    ioapic: &SharedInterruptController,
    slot: u8,
    pin: u8,
    level: bool,
) {
    // Read the four PIRQ routing bytes the guest programmed in the bridge config.
    let regs = {
        let rc = pcie.borrow();
        match rc.find_device(&ICH9_LPC_BDF) {
            Some(bridge) => [
                bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE),
                bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE + 1),
                bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE + 2),
                bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE + 3),
            ],
            None => return,
        }
    };
    let mut router = PirqRouter::new();
    router.sync_from_config(regs);

    // 8259 path: the routed ISA IRQ as a level line (PCI INTx is level).
    if let Some(irq) = router.device_isa_irq(slot, pin) {
        pic.with(|p| p.set_irq_level(irq, level));
    }
    // I/O APIC path: the PIRQ line's fixed GSI 16-19.
    if let Some(gsi) = PirqRouter::device_gsi(slot, pin) {
        let line = ioapic.line(gsi);
        line(level);
    }
}

/// Legacy ISA IRQ line for the 8254 PIT channel-0 (system timer).
pub const IRQ_PIT: u8 = 0;
/// Legacy ISA IRQ line for the COM1 16550 UART.
pub const IRQ_COM1: u8 = 4;

/// BDF of the ICH9 LPC bridge / PCI interrupt router (`00:1F.0`) seeded by
/// [`DeviceBus::standard_pc_complete`]; its config space holds the
/// `PIRQ[A-D]_ROUT` routing registers. Sourced from the shared
/// [`enlil_devices::pcie::ICH9_LPC_BRIDGE_BDF`] so the live bridge and the DSDT's
/// `ISA_` `_ADR` cannot drift to different PCI locations.
const ICH9_LPC_BDF: PciBdf = enlil_devices::pcie::ICH9_LPC_BRIDGE_BDF;
/// PCI device ID of the ICH9 LPC interface bridge (Intel 82801IB, `D31:F0`).
const ICH9_LPC_DEVICE_ID: u16 = enlil_devices::pcie::ICH9_LPC_DEVICE_ID;

/// BDF of the discrete Renesas xHCI USB 3.0 controller seeded by
/// [`DeviceBus::standard_pc_complete`]: an expansion slot on bus 0.
const XHCI_BDF: PciBdf = PciBdf::new(0, 4, 0);
/// Firmware-assigned BAR0 of the xHCI register window, inside the 32-bit PCI
/// MMIO hole the DSDT's `PCI0._CRS` produces (`0xC000_0000..0xFEC0_0000`).
const XHCI_MMIO_BASE: u32 = 0xFE90_0000;
/// Size of the xHCI BAR0 window (64 KiB).
const XHCI_MMIO_SIZE: u64 = 0x1_0000;
/// Root-hub port count on the virtual controller.
const XHCI_PORTS: u8 = 4;

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
    fn rdmsr(&mut self, msr: u32) -> Option<u64> {
        self.stealth.as_mut().and_then(|b| b.active().read_msr(msr))
    }
    fn wrmsr(&mut self, msr: u32, value: u64) -> bool {
        self.stealth
            .as_mut()
            .is_some_and(|b| b.active().write_msr(msr, value))
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

    /// MMIO device claimed at a low, real-mode-reachable address so a 16-bit
    /// guest blob can drive it. Records the last write and returns a fixed
    /// sentinel on read.
    struct LowMmio {
        last_write: Rc<RefCell<Option<(u64, u64)>>>,
    }

    impl MmioDevice for LowMmio {
        fn mmio_read(&mut self, _offset: u64, _size: u8) -> u64 {
            0x3C
        }
        fn mmio_write(&mut self, offset: u64, _size: u8, data: u64) {
            *self.last_write.borrow_mut() = Some((offset, data));
        }
        fn mmio_range(&self) -> (u64, u64) {
            (0x8000, 0x9000)
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
    fn fw_cfg_mounts_and_serves_the_file_set_over_pio() {
        use enlil_devices::fw_cfg::{selector, FwCfgDevice, FW_CFG_PORT_DATA, FW_CFG_PORT_SEL};

        // A populated fw_cfg: the SMBIOS file set plus a known file we can read
        // back by its returned selector.
        let mut fw = FwCfgDevice::new();
        fw.add_smbios(vec![0xAA, 0xBB], vec![0x01, 0x02, 0x03]);
        let probe_sel = fw.add_file("etc/enlil/probe", vec![0xDE, 0xAD, 0xBE, 0xEF]);

        let mut bus = DeviceBus::new();
        bus.add_fw_cfg(fw).expect("mount fw_cfg");
        // The device claims exactly its two registers.
        assert!(bus.pio.is_mapped(FW_CFG_PORT_SEL));
        assert!(bus.pio.is_mapped(FW_CFG_PORT_DATA));
        assert!(!bus.pio.is_mapped(FW_CFG_PORT_DATA + 1));

        // Helper: read `n` bytes from the data port one at a time, as firmware
        // does over the byte-stream register.
        fn read_stream(bus: &mut DeviceBus, n: usize) -> Vec<u8> {
            (0..n)
                .map(|_| {
                    let mut one = [0u8; 1];
                    VmExitHandler::io_in(bus, FW_CFG_PORT_DATA, &mut one);
                    one[0]
                })
                .collect()
        }

        // Select the signature item (16-bit selector write) and read "QEMU" —
        // the probe an OVMF/SeaBIOS guest does to detect fw_cfg.
        VmExitHandler::io_out(
            &mut bus,
            FW_CFG_PORT_SEL,
            &selector::SIGNATURE.to_le_bytes(),
        );
        assert_eq!(read_stream(&mut bus, 4), b"QEMU");

        // Select our registered file and read its bytes back through the bus —
        // proving a guest can pull a delivered table/file off the mounted device.
        VmExitHandler::io_out(&mut bus, FW_CFG_PORT_SEL, &probe_sel.to_le_bytes());
        assert_eq!(read_stream(&mut bus, 4), vec![0xDE, 0xAD, 0xBE, 0xEF]);

        // Re-selecting rewinds the stream (the firmware re-reads from offset 0).
        VmExitHandler::io_out(&mut bus, FW_CFG_PORT_SEL, &probe_sel.to_le_bytes());
        assert_eq!(read_stream(&mut bus, 1), vec![0xDE]);
    }

    #[test]
    fn install_acpi_fw_cfg_delivers_the_full_loader_path_over_pio() {
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::acpi::AcpiTableSetConfig;
        use enlil_devices::fw_cfg::{FW_CFG_PORT_DATA, FW_CFG_PORT_SEL};

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            0,
            4,
        )
        .expect("assemble standard PC");
        pc.install_acpi_fw_cfg(&AcpiTableSetConfig::default())
            .expect("mount populated fw_cfg");

        // The two fw_cfg registers are now on the assembled PC's PIO bus.
        assert!(pc.bus.pio.is_mapped(FW_CFG_PORT_SEL));
        assert!(pc.bus.pio.is_mapped(FW_CFG_PORT_DATA));

        let read_file = |bus: &mut DeviceBus, sel: u16, n: usize| -> Vec<u8> {
            VmExitHandler::io_out(bus, FW_CFG_PORT_SEL, &sel.to_le_bytes());
            (0..n)
                .map(|_| {
                    let mut one = [0u8; 1];
                    VmExitHandler::io_in(bus, FW_CFG_PORT_DATA, &mut one);
                    one[0]
                })
                .collect()
        };

        // Files registered in order: etc/acpi/rsdp (0x20), etc/acpi/tables
        // (0x21), etc/table-loader (0x22). The RSDP is delivered at base 0 (its
        // XsdtAddress @24 is the pure offset 0), so the loader's ADD_POINTERs
        // are valid; the table-loader file begins with an ALLOCATE command.
        let rsdp = read_file(&mut pc.bus, 0x20, 32);
        assert_eq!(u64::from_le_bytes(rsdp[24..32].try_into().unwrap()), 0);
        let loader_head = read_file(&mut pc.bus, 0x22, 4);
        assert_eq!(u32::from_le_bytes(loader_head.try_into().unwrap()), 0x1);
    }

    #[test]
    fn install_firmware_tables_delivers_acpi_loader_and_smbios() {
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::acpi::AcpiTableSetConfig;
        use enlil_devices::fw_cfg::{selector, FW_CFG_PORT_DATA, FW_CFG_PORT_SEL};
        use enlil_devices::smbios::SmbiosConfig;

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            0,
            4,
        )
        .expect("assemble standard PC");
        pc.install_firmware_tables(&AcpiTableSetConfig::default(), &SmbiosConfig::default())
            .expect("mount full firmware table set");

        // Read the fw_cfg file directory and collect the delivered file names.
        VmExitHandler::io_out(
            &mut pc.bus,
            FW_CFG_PORT_SEL,
            &selector::FILE_DIR.to_le_bytes(),
        );
        let read = |bus: &mut DeviceBus, n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    let mut one = [0u8; 1];
                    VmExitHandler::io_in(bus, FW_CFG_PORT_DATA, &mut one);
                    one[0]
                })
                .collect()
        };
        let count = u32::from_be_bytes(read(&mut pc.bus, 4).try_into().unwrap()) as usize;
        assert_eq!(
            count, 5,
            "rsdp + tables + table-loader + smbios anchor + tables"
        );
        let mut names = Vec::new();
        for _ in 0..count {
            let entry = read(&mut pc.bus, 64); // size(4) selector(2) reserved(2) name(56)
            let name_end = entry[8..].iter().position(|&c| c == 0).unwrap_or(56);
            names.push(String::from_utf8_lossy(&entry[8..8 + name_end]).into_owned());
        }
        for expected in [
            "etc/acpi/rsdp",
            "etc/acpi/tables",
            "etc/table-loader",
            "etc/smbios/smbios-anchor",
            "etc/smbios/smbios-tables",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
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
        // Intel Q35 MCH host bridge: device_id:vendor_id = 0x29C0_8086.
        assert_eq!(u32::from_le_bytes(data), 0x29C0_8086);

        // ...and the same device through the ECAM MMIO window at base + 0.
        VmExitHandler::mmio_read(&mut bus, 0xB000_0000, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x29C0_8086);

        // A device added through the shared handle *after* both front-ends are
        // mounted is visible through ECAM (proving the shared device set).
        shared
            .borrow_mut()
            .add_device(PciConfigSpace::new(PciBdf::new(0, 2, 0), 0x10EC, 0x8168));
        let dev_addr = 0xB000_0000 + (u64::from(2u32) << 15); // BDF 0:2.0 ecam offset
        VmExitHandler::mmio_read(&mut bus, dev_addr, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x8168_10EC);
    }

    /// The MCH's `PCIEXBAR` (config `0x60`) is where a real guest can learn the
    /// ECAM base from the hardware itself; it must advertise exactly the window
    /// the bus decodes (and the MCFG table describes) — same fact, two surfaces.
    #[test]
    fn q35_pciexbar_advertises_the_live_ecam_window() {
        use super::DEFAULT_ECAM_BASE;
        use enlil_devices::pcie::{PcieRootComplex, PCIEXBAR_ENABLE, PCIEXBAR_OFFSET};

        let mut bus = DeviceBus::new();
        bus.add_pcie(PcieRootComplex::new(DEFAULT_ECAM_BASE))
            .unwrap();

        // Read PCIEXBAR through the legacy CAM: enable | 0:0.0 | reg 0x60.
        let addr: u32 = 0x8000_0000 | u32::from(PCIEXBAR_OFFSET);
        VmExitHandler::io_out(&mut bus, 0xCF8, &addr.to_le_bytes());
        let mut data = [0u8; 4];
        VmExitHandler::io_in(&mut bus, 0xCFC, &mut data);
        let pciexbar = u64::from(u32::from_le_bytes(data));

        // Enabled, and the base is the window the MMIO bus actually decodes.
        assert_eq!(pciexbar & PCIEXBAR_ENABLE, PCIEXBAR_ENABLE);
        let base = pciexbar & !0xFu64;
        assert_eq!(base, DEFAULT_ECAM_BASE);
        assert!(bus.mmio.is_mapped(base));
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
        assert_eq!(u32::from_le_bytes(data), 0x29C0_8086);
        VmExitHandler::mmio_read(&mut bus, DEFAULT_ECAM_BASE, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x29C0_8086);

        // Guest serial output reaches the shared sink (byte-at-a-time, as a
        // guest drives a byte-wide register).
        for &b in b"hi" {
            VmExitHandler::io_out(&mut bus, COM1, &[b]);
        }
        assert_eq!(&*sink.lock().unwrap(), b"hi");
    }

    #[test]
    fn standard_pc_with_interrupts_routes_uart_irq_to_a_vcpu() {
        use crate::serial::{SerialOutput, SerialOutputMode, COM1, IER_REG};
        use enlil_devices::interrupt::SharedInterruptController;
        use std::sync::{Arc, Mutex};

        // One vCPU; enable LAPIC 0 (SVR bit 8) as a guest OS would when it brings
        // up the APIC. (LAPIC_SVR offset 0x0F0 is not re-exported from
        // enlil-devices, so the literal is used here.) The IRQ4 redirection
        // entry is programmed below through the I/O APIC's MMIO aperture, the way
        // a real guest does it.
        let pic = SharedInterruptController::new(1);
        pic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));

        let sink = Arc::new(Mutex::new(Vec::new()));
        let (mut bus, _pcie) = DeviceBus::standard_pc_with_interrupts(
            SerialOutput::new("guest", SerialOutputMode::Shared(Arc::clone(&sink))),
            &pic,
        )
        .unwrap();

        // Same canonical layout as standard_pc, plus the I/O APIC aperture.
        assert!(bus.pio.is_mapped(COM1));
        assert!(bus.pio.is_mapped(0x40));
        assert!(bus.mmio.is_mapped(0xFEC0_0000));

        // Program IRQ4's RTE through the I/O APIC MMIO aperture (vector 0x24 to
        // LAPIC 0, unmasked) the way a guest OS does: IOREGSEL=0x18 (REDTBL base
        // 0x10 + 2*4) then IOWIN, then high dword at 0x19.
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x18u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0x24u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x19u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // No interrupt has fired yet.
        assert!(!pic.with(|c| c.has_pending(0)));

        // Enabling the THR-empty interrupt in the IER asserts IRQ4 immediately
        // (the THR is always empty in our model). This is a pure `io_out` exit,
        // exactly as the guest would issue it.
        VmExitHandler::io_out(&mut bus, COM1 + IER_REG, &[0x02]);

        // The line drove IRQ4 through the I/O APIC RTE into LAPIC 0's IRR.
        assert!(pic.with(|c| c.has_pending(0)));
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x24));
    }

    #[test]
    fn standard_pc_with_pic_routes_uart_irq_through_the_legacy_8259() {
        use crate::serial::{SerialOutput, SerialOutputMode, COM1, IER_REG};
        use enlil_devices::interrupt::{SharedPic, MASTER_CMD, MASTER_DATA, SLAVE_CMD, SLAVE_DATA};
        use std::sync::{Arc, Mutex};

        let pic = SharedPic::new();
        let sink = Arc::new(Mutex::new(Vec::new()));
        let (mut bus, _pcie) = DeviceBus::standard_pc_with_pic(
            SerialOutput::new("guest", SerialOutputMode::Shared(Arc::clone(&sink))),
            &pic,
        )
        .unwrap();

        // The four legacy PIC ports are on the PIO bus, alongside COM1/PIT/CAM.
        assert!(bus.pio.is_mapped(MASTER_CMD));
        assert!(bus.pio.is_mapped(MASTER_DATA));
        assert!(bus.pio.is_mapped(SLAVE_CMD));
        assert!(bus.pio.is_mapped(SLAVE_DATA));
        assert!(bus.pio.is_mapped(COM1));
        assert!(bus.pio.is_mapped(0x40));

        // A guest BIOS programs the PIC for the PC/AT layout through the bus,
        // byte-at-a-time, exactly as KVM delivers the OUT exits: master to
        // vectors 0x20-0x27, slave to 0x28-0x2F, slave cascaded on master IR2.
        VmExitHandler::io_out(&mut bus, MASTER_CMD, &[0x11]); // ICW1
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x20]); // ICW2: base 0x20
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x04]); // ICW3: slave IR2
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x01]); // ICW4: 8086
        VmExitHandler::io_out(&mut bus, SLAVE_CMD, &[0x11]);
        VmExitHandler::io_out(&mut bus, SLAVE_DATA, &[0x28]);
        VmExitHandler::io_out(&mut bus, SLAVE_DATA, &[0x02]);
        VmExitHandler::io_out(&mut bus, SLAVE_DATA, &[0x01]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x00]); // unmask master
        VmExitHandler::io_out(&mut bus, SLAVE_DATA, &[0x00]); // unmask slave

        // Nothing pending yet.
        assert!(!pic.with(|p| p.has_interrupt()));

        // Enabling the UART's THR-empty interrupt asserts IRQ4 (a pure io_out
        // exit). It latches on the master 8259 and INTR carries vector 0x24.
        VmExitHandler::io_out(&mut bus, COM1 + IER_REG, &[0x02]);
        assert_eq!(pic.with(|p| p.pending_vector()), Some(0x24));

        // The CPU's INTA consumes it and puts IRQ4 in service.
        assert_eq!(pic.with(|p| p.acknowledge()), Some(0x24));
    }

    #[test]
    fn standard_pc_with_pic_masks_keep_an_irq_from_the_cpu() {
        use crate::serial::{SerialOutput, SerialOutputMode, COM1, IER_REG};
        use enlil_devices::interrupt::{SharedPic, MASTER_CMD, MASTER_DATA};

        let pic = SharedPic::new();
        let (mut bus, _pcie) = DeviceBus::standard_pc_with_pic(
            SerialOutput::new("guest", SerialOutputMode::Null),
            &pic,
        )
        .unwrap();

        // Initialize the master, then leave IRQ4 masked (OCW1 bit 4 set).
        VmExitHandler::io_out(&mut bus, MASTER_CMD, &[0x11]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x20]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x04]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x01]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[1 << 4]); // mask IRQ4

        VmExitHandler::io_out(&mut bus, COM1 + IER_REG, &[0x02]);
        // Latched but masked: no INTR to the CPU.
        assert!(!pic.with(|p| p.has_interrupt()));
        // Unmasking through the bus lets the latched request through.
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x00]);
        assert_eq!(pic.with(|p| p.pending_vector()), Some(0x24));
    }

    #[test]
    fn standard_pc_with_dual_irq_drives_both_controllers_from_one_line() {
        use crate::serial::{SerialOutput, SerialOutputMode, COM1, IER_REG};
        use enlil_devices::interrupt::{
            SharedInterruptController, SharedPic, MASTER_CMD, MASTER_DATA, SLAVE_CMD, SLAVE_DATA,
        };

        let pic = SharedPic::new();
        let ioapic = SharedInterruptController::new(1);
        // Enable LAPIC 0 (SVR) as the guest would when bringing up the APIC.
        ioapic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));

        let (mut bus, _pcie) = DeviceBus::standard_pc_with_dual_irq(
            SerialOutput::new("guest", SerialOutputMode::Null),
            &pic,
            &ioapic,
        )
        .unwrap();

        // Both front-ends are present: the four PIC ports and the I/O APIC page.
        assert!(bus.pio.is_mapped(MASTER_CMD));
        assert!(bus.pio.is_mapped(SLAVE_CMD));
        assert!(bus.mmio.is_mapped(0xFEC0_0000));

        // Program the 8259 master for the PC/AT layout (IRQ4 -> vector 0x24).
        VmExitHandler::io_out(&mut bus, MASTER_CMD, &[0x11]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x20]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x04]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x01]);
        VmExitHandler::io_out(&mut bus, SLAVE_CMD, &[0x11]);
        VmExitHandler::io_out(&mut bus, SLAVE_DATA, &[0x28]);
        VmExitHandler::io_out(&mut bus, SLAVE_DATA, &[0x02]);
        VmExitHandler::io_out(&mut bus, SLAVE_DATA, &[0x01]);
        VmExitHandler::io_out(&mut bus, MASTER_DATA, &[0x00]);
        VmExitHandler::io_out(&mut bus, SLAVE_DATA, &[0x00]);

        // Program IRQ4's I/O APIC RTE through the aperture (vector 0x34 -> LAPIC 0).
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x18u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0x34u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x19u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // One device event (enable THRE interrupt) asserts the single IRQ4 line,
        // which the tee drives into *both* controllers simultaneously.
        VmExitHandler::io_out(&mut bus, COM1 + IER_REG, &[0x02]);

        // The 8259 has it pending with its vector...
        assert_eq!(pic.with(|p| p.pending_vector()), Some(0x24));
        // ...and the I/O APIC routed it to LAPIC 0 with its (different) vector.
        assert_eq!(ioapic.with(|c| c.pending_vector(0)), Some(0x34));
    }

    #[test]
    fn shared_pit_ticked_from_a_handle_routes_irq0_to_gsi_2() {
        use enlil_devices::interrupt::SharedInterruptController;
        use enlil_devices::timer::SharedPit;

        // The end-to-end PIT timer path the override-aware wiring enables: a
        // guest reads GSI 2 for the timer from the MADT and programs that RTE;
        // the PIT, wired via isa_line(0), must assert GSI 2 — and a SharedPit
        // lets us actually tick it after it is mounted on the bus.
        let pic = SharedInterruptController::new(1);
        pic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));
        let pit = SharedPit::new();
        pit.with(|p| p.attach_irq0(Box::new(pic.isa_line(0)))); // IRQ0 -> GSI 2

        let mut bus = DeviceBus::new();
        bus.add_pit_shared(&pit).unwrap();
        bus.add_ioapic(&pic).unwrap();
        assert!(bus.pio.is_mapped(0x40));

        // Program GSI 2's RTE (REDTBL 0x10 + 2*2 = 0x14) -> vector 0x40, LAPIC 0.
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x14u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0x40u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x15u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // Guest programs channel 0: mode 2 (rate generator), a short reload.
        VmExitHandler::io_out(&mut bus, 0x43, &[0x34]); // ch0, lo/hi, mode 2
        VmExitHandler::io_out(&mut bus, 0x40, &[2]); // reload low
        VmExitHandler::io_out(&mut bus, 0x40, &[0]); // reload high

        // No interrupt until the timer thread advances the PIT past its count.
        assert!(!pic.with(|c| c.has_pending(0)));
        for _ in 0..4 {
            let _ = pit.tick(1000); // ~1.2 ticks each at 838 ns/tick
        }
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x40));
    }

    #[test]
    fn ps2_keyboard_irq1_routes_through_the_ioapic() {
        use enlil_devices::interrupt::SharedInterruptController;
        use enlil_devices::ps2::SharedI8042;

        let pic = SharedInterruptController::new(1);
        pic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));
        let ps2 = SharedI8042::new();
        ps2.attach_kbd_irq(Box::new(pic.isa_line(1))); // IRQ1, identity GSI 1

        let mut bus = DeviceBus::new();
        bus.add_ps2(&ps2).unwrap();
        bus.add_ioapic(&pic).unwrap();
        assert!(bus.pio.is_mapped(0x60));
        assert!(bus.pio.is_mapped(0x64));
        assert!(
            !bus.pio.is_mapped(0x61),
            "speaker/NMI port must stay open-bus"
        );

        // Program GSI 1's RTE (REDTBL 0x10 + 1*2 = 0x12) -> vector 0x31, LAPIC 0.
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x12u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0x31u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x13u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // A host keypress raises IRQ1, which routes through the RTE to LAPIC 0.
        ps2.inject_key(0x1E);
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x31));

        // The guest reads the scancode from the data port; IRQ1 deasserts.
        let mut sc = [0u8; 1];
        VmExitHandler::io_in(&mut bus, 0x60, &mut sc);
        assert_eq!(sc[0], 0x1E);
    }

    #[test]
    fn rtc_reads_the_time_through_the_bus_ports() {
        use enlil_devices::timer::{RtcTime, SharedRtc, RTC_INDEX};

        // 2026-06-07T13:14:15Z.
        let rtc = SharedRtc::new(RtcTime::from_unix(1_780_838_055));
        let mut bus = DeviceBus::new();
        bus.add_rtc(&rtc).unwrap();
        assert!(bus.pio.is_mapped(RTC_INDEX));

        // Select the hours register (index port), then read the data port — the
        // RTC defaults to 24-hour binary mode, so 13 reads back as 13.
        VmExitHandler::io_out(&mut bus, RTC_INDEX, &[0x04]);
        let mut data = [0u8; 1];
        VmExitHandler::io_in(&mut bus, 0x71, &mut data);
        assert_eq!(data[0], 13);

        // The index port is write-only: it reads back open-bus 0xFF.
        let mut idx = [0u8; 1];
        VmExitHandler::io_in(&mut bus, RTC_INDEX, &mut idx);
        assert_eq!(idx[0], 0xFF);
    }

    #[test]
    fn rtc_irq8_routes_through_the_ioapic_to_a_vcpu() {
        use enlil_devices::interrupt::SharedInterruptController;
        use enlil_devices::timer::{RtcTime, SharedRtc};

        let pic = SharedInterruptController::new(1);
        pic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));
        let rtc = SharedRtc::new(RtcTime::from_unix(1_780_838_055));
        // Wire RTC IRQ8 into the I/O APIC (GSI 8, identity) and enable the
        // update-ended interrupt (Register B: DM | 24H | UIE).
        rtc.with(|r| {
            r.attach_irq(Box::new(pic.isa_line(8)));
            r.write_index(0x0B);
            r.write_data(0x04 | 0x02 | 0x10);
        });

        let mut bus = DeviceBus::new();
        bus.add_rtc(&rtc).unwrap();
        bus.add_ioapic(&pic).unwrap();

        // Program GSI 8's RTE through the aperture (vector 0x38 -> LAPIC 0):
        // REDTBL base 0x10 + 8*2 = 0x20 (low), 0x21 (high).
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x20u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0x38u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x21u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // A one-second tick raises the update-ended interrupt, which routes
        // through GSI 8's RTE into LAPIC 0.
        assert!(rtc.with(enlil_devices::timer::Rtc146818::tick_second));
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x38));
    }

    #[test]
    fn standard_pc_complete_mounts_every_legacy_device_and_dual_wires_irqs() {
        use super::StandardPc;
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::interrupt::{MASTER_CMD, MASTER_DATA, SLAVE_CMD, SLAVE_DATA};

        // 2026-06-07T13:14:15Z, one vCPU.
        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();
        let StandardPc {
            mut bus,
            pic,
            ioapic,
            ps2,
            ..
        } = pc;

        // Every legacy device a guest touches at boot is mounted at its canonical
        // address: COM1, PIT, System Control Port B (0x61), RTC, PS/2 data+cmd,
        // System Control Port A (0x92), the ACPI PM1 block (0x600/0x604), PM timer
        // (0x608) and GPE0 block (0x620), both 8259s, the ELCR, and the PCIe CAM
        // ports — plus the I/O APIC page.
        for port in [
            0x3F8u16, 0x40, 0x61, 0x70, 0x60, 0x64, 0x92, 0x600, 0x604, 0x608, 0x620, 0x20, 0xA0,
            0x4D0, 0xCF8,
        ] {
            assert!(
                bus.pio.is_mapped(port),
                "PIO port {port:#x} should be mapped"
            );
        }
        assert!(
            bus.mmio.is_mapped(0xFEC0_0000),
            "I/O APIC page should be mapped"
        );
        assert!(
            bus.mmio.is_mapped(0xFED0_0000),
            "HPET register block should be mapped"
        );

        // Bring up LAPIC 0 and program the PC/AT 8259 layout (master base 0x20),
        // then unmask every line on both chips.
        ioapic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));
        for (port, val) in [
            (MASTER_CMD, 0x11),
            (MASTER_DATA, 0x20),
            (MASTER_DATA, 0x04),
            (MASTER_DATA, 0x01),
            (SLAVE_CMD, 0x11),
            (SLAVE_DATA, 0x28),
            (SLAVE_DATA, 0x02),
            (SLAVE_DATA, 0x01),
            (MASTER_DATA, 0x00),
            (SLAVE_DATA, 0x00),
        ] {
            VmExitHandler::io_out(&mut bus, port, &[val]);
        }

        // Program GSI 1's RTE (keyboard) through the aperture: vector 0x31 ->
        // LAPIC 0. REDTBL base 0x10 + 1*2 = 0x12 (low), 0x13 (high).
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x12u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0x31u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0000, &0x13u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // A host keypress asserts the single IRQ1 line, which the factory tees
        // into *both* controllers (the per-line helper every legacy device uses).
        ps2.inject_key(0x1E);
        // The 8259 master has it pending at vector base + 1 = 0x21...
        assert_eq!(pic.with(|p| p.pending_vector()), Some(0x21));
        // ...and the I/O APIC routed it to LAPIC 0 at the RTE's vector 0x31.
        assert_eq!(ioapic.with(|c| c.pending_vector(0)), Some(0x31));
    }

    #[test]
    fn standard_pc_complete_programs_the_default_pirq_routing() {
        use super::{
            PirqRouter, StandardPc, ICH9_LPC_BDF, PIRQ_DEFAULT_IRQS, PIRQ_ROUTE_CONFIG_BASE,
        };
        use crate::serial::{SerialOutput, SerialOutputMode};

        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();
        let StandardPc { pcie, .. } = pc;

        // The firmware programmed the LPC bridge's PIRQ[A-D]_ROUT out of their 0x80
        // reset state to the advertised defaults, so the live router agrees with the
        // DSDT link devices.
        let rc = pcie.borrow();
        let bridge = rc
            .find_device(&ICH9_LPC_BDF)
            .expect("ICH9 LPC bridge is mounted");
        let mut regs = [0u8; 4];
        for (line, slot) in regs.iter_mut().enumerate() {
            *slot = bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE + line as u16);
        }
        assert_eq!(
            regs, PIRQ_DEFAULT_IRQS,
            "PIRQ[A-D]_ROUT must hold the firmware-default routing"
        );

        // A router synced from those bytes resolves a slot-0 INTA device (PIRQA) to
        // PIRQ_DEFAULT_IRQS[0] — the same IRQ the LNKA _CRS reports.
        let mut router = PirqRouter::new();
        router.sync_from_config(regs);
        assert_eq!(router.device_isa_irq(0, 1), Some(PIRQ_DEFAULT_IRQS[0]));
    }

    /// `D31` is multifunction on every ICH9: the LPC's header must say so, the
    /// SMBus controller must answer at `1F.3` with the BAR the firmware
    /// assigned, and the live register file must decode behind that BAR — one
    /// consistent story across config space, the PIRQ defaults, and the bus.
    #[test]
    fn standard_pc_complete_mounts_the_ich9_smbus_function() {
        use super::{PirqRouter, StandardPc, ICH9_LPC_BDF, PIRQ_DEFAULT_IRQS};
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::pcie::{
            cfg, PciBdf, BOARD_SUBSYSTEM_DEVICE_ID, BOARD_SUBSYSTEM_VENDOR_ID, ICH9_SMBUS_BDF,
        };
        use enlil_devices::smbus::{HST_STS, SMBUS_IO_BASE, STS_INUSE};

        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();
        let StandardPc { mut bus, pcie, .. } = pc;

        {
            let rc = pcie.borrow();
            // Function 0 declares the device multifunction, or the guest never
            // probes function 3.
            let lpc = rc.find_device(&ICH9_LPC_BDF).unwrap();
            assert_eq!(lpc.read_u8(cfg::HEADER_TYPE) & 0x80, 0x80);

            // The SMBus function: SMBus class, SMB_BASE in BAR4, and an
            // interrupt line that matches what the default PIRQ routing
            // resolves INTB# of device 31 to (the same answer the DSDT's
            // link devices give).
            let smb = rc.find_device(&ICH9_SMBUS_BDF).unwrap();
            assert_eq!(smb.read_u8(cfg::CLASS_CODE), 0x0C);
            assert_eq!(smb.read_u8(cfg::SUBCLASS), 0x05);

            // Every onboard function carries the board vendor's subsystem IDs.
            for dev in [lpc, smb, rc.find_device(&PciBdf::new(0, 0, 0)).unwrap()] {
                assert_eq!(
                    dev.read_u16(cfg::SUBSYSTEM_VENDOR_ID),
                    BOARD_SUBSYSTEM_VENDOR_ID
                );
                assert_eq!(dev.read_u16(cfg::SUBSYSTEM_ID), BOARD_SUBSYSTEM_DEVICE_ID);
            }

            // PMBASE/ACPI_CNTL: the LPC registers the ACPI PM I/O block
            // physically hangs off must encode the ports the FADT advertises
            // and the bus decodes (PM1 +0/+4, PM_TMR +8, GPE0 +0x20), with
            // the decode enabled and the SCI on the FADT's IRQ.
            use enlil_devices::chipset::{GPE0_PORT, PM1_CNT_PORT, PM1_EVT_PORT, SCI_IRQ};
            use enlil_devices::pcie::{
                acpi_cntl_sci_irq, ACPI_CNTL_ACPI_EN, LPC_ACPI_CNTL_OFFSET, LPC_PMBASE_OFFSET,
            };
            use enlil_devices::timer::PM_TIMER_PORT;
            let pmbase = lpc.read_u32(LPC_PMBASE_OFFSET);
            assert_eq!(pmbase & 1, 1, "PMBASE bit 0 is hardwired (I/O space)");
            let base = u16::try_from(pmbase & 0xFF80).unwrap();
            assert_eq!(base, PM1_EVT_PORT);
            assert_eq!(base + 4, PM1_CNT_PORT);
            assert_eq!(base + 8, PM_TIMER_PORT);
            assert_eq!(base + 0x20, GPE0_PORT);
            let cntl = lpc.read_u8(LPC_ACPI_CNTL_OFFSET);
            assert_eq!(cntl & ACPI_CNTL_ACPI_EN, ACPI_CNTL_ACPI_EN);
            assert_eq!(acpi_cntl_sci_irq(cntl), SCI_IRQ);
            assert_eq!(smb.read_u32(cfg::BAR4), u32::from(SMBUS_IO_BASE) | 1);
            assert_eq!(smb.read_u8(cfg::INTERRUPT_PIN), 2);
            let expected = PirqRouter::default_device_isa_irq(31, 2).unwrap();
            assert_eq!(smb.read_u8(cfg::INTERRUPT_LINE), expected);
            assert_eq!(expected, PIRQ_DEFAULT_IRQS[0]); // INTB#@31 -> PIRQA
        }

        // The register file decodes behind the advertised BAR: an idle
        // controller (first status read 0, second shows the INUSE semaphore
        // the first read took) — not open bus.
        let mut data = [0u8; 1];
        VmExitHandler::io_in(&mut bus, SMBUS_IO_BASE + HST_STS, &mut data);
        assert_eq!(data[0], 0);
        VmExitHandler::io_in(&mut bus, SMBUS_IO_BASE + HST_STS, &mut data);
        assert_eq!(data[0], STS_INUSE);
    }

    /// The SMBus completion interrupt travels the whole path a real one does:
    /// the guest starts a transaction with `INTREN` set, the (empty-bus)
    /// `DEV_ERR` completion asserts `INTB#`, the live PIRQ routing resolves it
    /// (device 31 INTB# -> PIRQA -> GSI 16), and the I/O APIC delivers the
    /// programmed vector — until the driver clears the status and the
    /// level-triggered line drops.
    #[test]
    fn smbus_completion_interrupt_routes_through_the_live_pirq_routing() {
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::smbus::{
            CNT_INTREN, CNT_START, HST_CNT, HST_STS, SMBUS_IO_BASE, STS_DEV_ERR, XMIT_SLVA,
        };

        use enlil_devices::interrupt::ELCR_SLAVE;

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();
        pc.ioapic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));

        // Program the 8259 (PC/AT layout) and mark IRQ11 — PIRQA's default
        // route — level in the ELCR (slave line 3), as a PCI interrupt must be.
        pc.pic.with(|p| {
            for (port, val) in [
                (0x20u16, 0x11u8),
                (0x21, 0x20),
                (0x21, 0x04),
                (0x21, 0x01),
                (0xA0, 0x11),
                (0xA1, 0x28),
                (0xA1, 0x02),
                (0xA1, 0x01),
                (0x21, 0x00),
                (0xA1, 0x00),
            ] {
                p.write_port(port, val);
            }
            p.write_elcr(ELCR_SLAVE, 1 << 3);
        });

        // Program PIRQA's I/O APIC GSI (16): vector 0x60 -> LAPIC 0.
        // REDTBL base 0x10 + 16*2 = 0x30 (low), 0x31 (high).
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x30u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0x60u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x31u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // The guest probes slave 0x50 with the interrupt enabled.
        VmExitHandler::io_out(&mut pc.bus, SMBUS_IO_BASE + XMIT_SLVA, &[(0x50 << 1) | 1]);
        VmExitHandler::io_out(
            &mut pc.bus,
            SMBUS_IO_BASE + HST_CNT,
            &[CNT_START | CNT_INTREN],
        );

        // The DEV_ERR completion asserted INTB#: the 8259 presents IRQ11's
        // vector (slave base 0x28 + 3 = 0x2B), the I/O APIC delivered GSI 16's.
        assert_eq!(pc.pic.with(|p| p.pending_vector()), Some(0x2B));
        assert_eq!(pc.ioapic.with(|c| c.pending_vector(0)), Some(0x60));

        // The driver clears the status; the level line deasserts and the
        // request is withdrawn from the 8259.
        VmExitHandler::io_out(&mut pc.bus, SMBUS_IO_BASE + HST_STS, &[STS_DEV_ERR]);
        assert_eq!(pc.pic.with(|p| p.pending_vector()), None);
    }

    /// The xHCI function is enumerable, its register file decodes behind the
    /// BAR config space advertises, and a hot-plug delivers the port-status
    /// interrupt through the live PIRQ routing — the full path a guest's USB
    /// stack exercises.
    #[test]
    fn xhci_is_enumerable_and_hotplug_interrupts_through_the_pirq_routing() {
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::pcie::{cfg, PciBdf};
        use enlil_devices::usb::{EventTrb, UsbSpeed};

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();
        pc.ioapic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));

        // Enumerable at 00:04.0: Renesas xHCI, USB class with xHCI prog-if,
        // BAR0 = the mounted MMIO window, INTA#.
        {
            let rc = pc.pcie.borrow();
            let dev = rc.find_device(&PciBdf::new(0, 4, 0)).unwrap();
            assert_eq!(dev.read_u16(cfg::VENDOR_ID), 0x1912);
            assert_eq!(dev.read_u8(cfg::CLASS_CODE), 0x0C);
            assert_eq!(dev.read_u8(cfg::SUBCLASS), 0x03);
            assert_eq!(dev.read_u8(cfg::PROG_IF), 0x30);
            assert_eq!(dev.read_u32(cfg::BAR0), 0xFE90_0000);
            assert_eq!(dev.read_u8(cfg::INTERRUPT_PIN), 1);
        }

        // The register window decodes behind the BAR: CAPLENGTH/HCIVERSION.
        let mut data = [0u8; 4];
        VmExitHandler::mmio_read(&mut pc.bus, 0xFE90_0000, &mut data);
        let r0 = u32::from_le_bytes(data);
        assert_eq!(r0 & 0xFF, 0x20);
        assert_eq!(r0 >> 16, 0x0110);

        // Program PIRQA's GSI (16; slot-4 INTA swizzles to PIRQA): vector
        // 0x70, then enable interrupter 0 (IMAN.IE at RTSOFF + 0x20) through
        // the BAR like a driver does.
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x30u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0x70u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x31u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFE90_0000 + 0x1020, &2u32.to_le_bytes());

        // Hot-plug a SuperSpeed device on port 0: the I/O APIC delivers the
        // PIRQA vector, and the event ring carries the port-status change.
        assert!(pc.connect_usb_device(0, 4, true));
        assert_eq!(pc.ioapic.with(|c| c.pending_vector(0)), Some(0x70));
        match pc.xhci.borrow_mut().pop_event() {
            Some(EventTrb::PortStatusChange { port_id }) => assert_eq!(port_id, 1),
            other => panic!("expected a port status change, got {other:?}"),
        }

        // The guest acknowledges: clearing IMAN.IP through the BAR withdraws
        // the level line (the MMIO adapter re-syncs the routed INTx).
        VmExitHandler::mmio_write(&mut pc.bus, 0xFE90_0000 + 0x1020, &3u32.to_le_bytes());
        assert!(!pc.xhci.borrow().intx_level());

        // The routing-engine seam: attach_usb_device picks the lowest free
        // port by speed (port 0 already taken above, so this lands on 1) and
        // delivers the interrupt the same way.
        assert_eq!(pc.attach_usb_device(UsbSpeed::Low), Some(1));
        assert_eq!(pc.ioapic.with(|c| c.pending_vector(0)), Some(0x70));
    }

    /// The run-loop USB DMA seam: a guest rings the host-command doorbell
    /// through the BAR (an MMIO exit that only *latches* it), and the doorbell
    /// is then drained by `service_usb_dma` — with guest memory in hand — which
    /// processes the command ring and produces its Command Completion event.
    /// This is the integration the MMIO write path can't do on its own.
    #[test]
    fn xhci_doorbell_is_serviced_through_the_run_loop_dma_seam() {
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::usb::xhci::{CommandTrb, EventTrb, TrbCompletionCode, VecDmaMemory};

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();

        // BAR0 the firmware assigned the xHCI function (see the test above).
        const XHCI_BAR0: u64 = 0xFE90_0000;
        let (op, dboff) = {
            let xhci = pc.xhci.borrow();
            (u64::from(xhci.caps.caplength), u64::from(xhci.caps.dboff))
        };

        // Bring the controller up through the BAR like a driver: CONFIG's
        // MaxSlotsEn, then USBCMD Run/Stop.
        VmExitHandler::mmio_write(&mut pc.bus, XHCI_BAR0 + op + 0x38, &8u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, XHCI_BAR0 + op, &1u32.to_le_bytes());

        // Queue a No-Op command on the command ring.
        assert!(pc.xhci.borrow_mut().submit_command(&CommandTrb::NoOp));

        // Ring doorbell 0 (host command) through the BAR. The MMIO write only
        // latches it — no event yet, since guest memory wasn't in hand here.
        VmExitHandler::mmio_write(&mut pc.bus, XHCI_BAR0 + dboff, &0u32.to_le_bytes());
        assert!(
            pc.xhci.borrow_mut().pop_event().is_none(),
            "doorbell MMIO write must only latch, not process the ring"
        );

        // The run loop drains the latched doorbell with guest memory: the
        // No-Op is processed and its Command Completion event produced.
        let mut mem = VecDmaMemory::new(0, 0);
        pc.service_usb_dma(&mut mem);
        let event = pc.xhci.borrow_mut().pop_event();
        match event {
            Some(EventTrb::CommandCompletion {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8),
            other => panic!("expected a command completion, got {other:?}"),
        }
    }

    /// The run-loop flush seam for events posted *outside* doorbell servicing:
    /// a hot-plug posts a Port Status Change to the controller's internal queue
    /// (and asserts INTA#) at notify time, but the event TRB can only be
    /// written when guest memory is in hand. `flush_usb_events` delivers it
    /// into the guest's event ring.
    #[test]
    fn xhci_hotplug_event_flushed_to_guest_ring_through_the_run_loop_seam() {
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::usb::xhci::{DmaMemory, VecDmaMemory};

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();

        // BAR0 / RTSOFF (runtime registers) for the xHCI function.
        const XHCI_BAR0: u64 = 0xFE90_0000;
        const RTSOFF: u64 = 0x1000;

        // Guest RAM with an ERST (one segment of 16 TRBs) and the event-ring
        // segment it points at.
        const ERSTBA: u64 = 0x2000;
        const SEG_BASE: u64 = 0x2100;
        let mut mem = VecDmaMemory::new(0, 0x4000);
        assert!(mem.write(ERSTBA, &SEG_BASE.to_le_bytes())); // segment base
        assert!(mem.write(ERSTBA + 8, &16u16.to_le_bytes())); // segment size (TRBs)

        // Program interrupter 0 through the BAR like a driver: ERSTSZ, ERSTBA,
        // ERDP (parked at the segment base), then enable the interrupter.
        for (off, v) in [
            (0x28_u64, 1_u32),       // ERSTSZ = 1 segment
            (0x30, ERSTBA as u32),   // ERSTBA lo
            (0x34, 0),               // ERSTBA hi
            (0x38, SEG_BASE as u32), // ERDP lo
            (0x3C, 0),               // ERDP hi
            (0x20, 2),               // IMAN.IE
        ] {
            VmExitHandler::mmio_write(&mut pc.bus, XHCI_BAR0 + RTSOFF + off, &v.to_le_bytes());
        }

        // Nothing queued yet.
        assert_eq!(pc.flush_usb_events(&mut mem), 0);

        // Hot-plug a SuperSpeed device on port 0: posts a Port Status Change.
        assert!(pc.connect_usb_device(0, 4, true));

        // The run loop flushes it into the guest event ring: exactly one event
        // delivered, and the TRB at ERDP is a Port Status Change Event (type
        // 34); a second flush has nothing left.
        assert_eq!(pc.flush_usb_events(&mut mem), 1);
        let mut trb = [0u8; 16];
        assert!(mem.read(SEG_BASE, &mut trb));
        let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
        assert_eq!(
            (control >> 10) & 0x3F,
            34,
            "expected a Port Status Change Event"
        );
        assert_eq!(pc.flush_usb_events(&mut mem), 0);
    }

    /// End-to-end: the StandardPc run loop drives a guest-resident transfer
    /// ring through `service_usb_dma`. The controller is constructed with
    /// guest-resident transfers enabled, so a No-Op TRB the guest wrote into
    /// EP0's ring (and announced by ringing the endpoint doorbell over the BAR)
    /// is fetched from guest memory and completed when the run loop services
    /// the doorbell — no `submit_transfer` involved.
    #[test]
    fn standardpc_drives_a_guest_transfer_ring_through_service_usb_dma() {
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::usb::emulated::LoopbackDevice;
        use enlil_devices::usb::xhci::context::{
            input_context_entry_offset, EndpointContext, EndpointType,
        };
        use enlil_devices::usb::xhci::transfer::DmaMemory;
        use enlil_devices::usb::xhci::{
            CommandTrb, EventTrb, TransferTrb, TrbCompletionCode, VecDmaMemory,
        };
        use enlil_devices::usb::UsbSpeed;

        const CONTROL_DCI: u8 = 1;
        const XHCI_BAR0: u64 = 0xFE90_0000;
        const RING: u64 = 0x4000;

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();

        let mut mem = VecDmaMemory::new(0x1000, 0x6000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes()));

        // Bring the controller up and address a loopback device with EP0's
        // transfer ring at RING — driven through the controller handle (setup).
        let dboff = {
            let mut x = pc.xhci.borrow_mut();
            let op = u32::from(x.caps.caplength);
            x.write_register(op + 0x38, 8); // CONFIG MaxSlotsEn
            x.write_register(op, 1); // USBCMD R/S
            x.write_register(op + 0x30, dcbaap as u32); // DCBAAP lo
            let port = x
                .attach_device_with_model(
                    UsbSpeed::High,
                    Box::new(LoopbackDevice::new(0x1234, 0x5678)),
                )
                .unwrap();
            let _ = x.pop_event();
            let addr_in = 0x1000_u64;
            assert!(mem.write(addr_in + 4, &0x3_u32.to_le_bytes())); // A0|A1
            assert!(mem.write(
                addr_in + 0x24,
                &(u32::try_from(port + 1).unwrap() << 16).to_le_bytes()
            ));
            let ep0 = EndpointContext {
                endpoint_type: EndpointType::Control,
                max_packet_size: 64,
                max_burst_size: 0,
                error_count: 3,
                interval: 0,
                tr_dequeue_pointer: RING,
                dequeue_cycle_state: true,
                average_trb_length: 8,
            };
            assert!(mem.write(
                addr_in + input_context_entry_offset(CONTROL_DCI),
                &ep0.to_bytes()
            ));
            x.submit_command(&CommandTrb::EnableSlot);
            x.submit_command(&CommandTrb::AddressDevice {
                slot_id: 1,
                input_context_ptr: addr_in,
            });
            let db = x.caps.dboff;
            x.write_register(db, 0);
            x.service_doorbells(&mut mem);
            let _ = x.pop_event();
            let _ = x.pop_event();
            u64::from(db)
        };

        // The guest writes a No-Op transfer TRB into EP0's ring (cycle = 1).
        assert!(mem.write(
            RING,
            &TransferTrb::NoOp { ioc: true }.to_trb(true).to_bytes()
        ));

        // The guest rings the EP0 doorbell over the BAR (slot 1 = dboff + 4,
        // value = the control endpoint's DCI), and the run loop services it.
        VmExitHandler::mmio_write(
            &mut pc.bus,
            XHCI_BAR0 + dboff + 4,
            &u32::from(CONTROL_DCI).to_le_bytes(),
        );
        pc.service_usb_dma(&mut mem);

        // The No-Op fetched from EP0's guest ring completed with Success.
        let event = pc.xhci.borrow_mut().pop_event();
        match event {
            Some(EventTrb::TransferEvent {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8),
            other => panic!("expected a transfer completion from the guest ring, got {other:?}"),
        }
    }

    #[test]
    fn advance_clocks_keeps_the_platform_timers_coherent_from_one_time_base() {
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::timer::PM_TIMER_FREQ_HZ;

        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();

        // The HPET counter only advances once the guest enables it (config bit 0).
        pc.hpet.with(|h| h.write(0x010, 1));

        // Advance the whole platform by one wall-clock second.
        let _ = pc.advance_clocks(1_000_000_000);

        // The ACPI PM timer advanced by exactly its 3.579545 MHz frequency.
        assert_eq!(pc.pm_timer.read(), PM_TIMER_FREQ_HZ);
        // The HPET (10 MHz / 100 ns per tick) advanced by 10,000,000 ticks.
        assert_eq!(pc.hpet.with(|h| h.counter()), 10_000_000);
    }

    #[test]
    fn advance_clocks_delivers_a_fired_hpet_timer_to_its_ioapic_gsi() {
        use crate::serial::{SerialOutput, SerialOutputMode};

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();
        pc.ioapic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));

        // Program HPET timer 0: interrupt-enable (bit 2) + route to GSI 16
        // (bits 9-13 = 16 -> 16 << 9 = 0x2000), one-shot comparator 100, then
        // enable the main counter.
        pc.hpet.with(|h| {
            h.write(0x100, 0x04 | 0x2000);
            h.write(0x108, 100);
            h.write(0x010, 1);
        });

        // Program GSI 16's RTE through the I/O APIC aperture: vector 0x40 ->
        // LAPIC 0. REDTBL base 0x10 + 16*2 = 0x30 (low), 0x31 (high).
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x30u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0x40u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x31u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // Nothing fired yet.
        assert!(!pc.ioapic.with(|c| c.has_pending(0)));

        // Advance past the comparator (>=100 HPET ticks = >=10,000 ns); the timer
        // fires and advance_clocks delivers it to GSI 16 -> LAPIC 0.
        let fired = pc.advance_clocks(1_000_000);
        assert_eq!(fired, vec![(0, 16)], "HPET timer 0 fired, routed to GSI 16");
        assert_eq!(pc.ioapic.with(|c| c.pending_vector(0)), Some(0x40));
    }

    #[test]
    fn advance_clocks_legacy_hpet_timer0_replaces_the_pit_on_irq0() {
        use crate::serial::{SerialOutput, SerialOutputMode};

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();
        pc.ioapic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));

        // Program the I/O APIC GSI 2 RTE — where IRQ0 lands under the MADT
        // override — to vector 0x60. REDTBL base 0x10 + 2*2 = 0x14 (low)/0x15.
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x14u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0x60u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x15u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // Enable HPET legacy replacement (config bit 1) + counter (bit 0), and
        // arm timer 0 (interrupt-enable, one-shot, comparator 100).
        pc.hpet.with(|h| {
            h.write(0x100, 0x04);
            h.write(0x108, 100);
            h.write(0x010, 0x03);
        });

        // Advance past the comparator: timer 0 fires and, in legacy mode, is
        // delivered as IRQ0 -> GSI 2 -> LAPIC 0 (the PIT's path), not its own GSI.
        let fired = pc.advance_clocks(1_000_000);
        // Timer 0 fired (its configured route is irrelevant in legacy mode).
        assert_eq!(fired, vec![(0, 0)]);
        assert_eq!(pc.ioapic.with(|c| c.pending_vector(0)), Some(0x60));
    }

    #[test]
    fn smi_command_enables_acpi_mode_through_the_bus() {
        use crate::serial::{SerialOutput, SerialOutputMode};

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();

        // Fresh machine: legacy mode, SCI_EN clear in PM1a_CNT (0x604).
        let mut cnt = [0u8; 2];
        VmExitHandler::io_in(&mut pc.bus, 0x604, &mut cnt);
        assert_eq!(u16::from_le_bytes(cnt) & 1, 0, "SCI_EN clear before enable");

        // The OS writes ACPI_ENABLE (0xA0) to the SMI command port (0xB2), then
        // polls PM1a_CNT until SCI_EN reads back set — the ACPICA handshake.
        VmExitHandler::io_out(&mut pc.bus, 0xB2, &[0xA0]);
        VmExitHandler::io_in(&mut pc.bus, 0x604, &mut cnt);
        assert_eq!(
            u16::from_le_bytes(cnt) & 1,
            1,
            "SCI_EN set after ACPI_ENABLE"
        );
        assert!(pc.pm1.with(|b| b.sci_enabled()));

        // ACPI_DISABLE (0xA1) returns to legacy mode.
        VmExitHandler::io_out(&mut pc.bus, 0xB2, &[0xA1]);
        VmExitHandler::io_in(&mut pc.bus, 0x604, &mut cnt);
        assert_eq!(
            u16::from_le_bytes(cnt) & 1,
            0,
            "SCI_EN clear after ACPI_DISABLE"
        );
    }

    #[test]
    fn poll_platform_events_surfaces_guest_shutdown_and_reset() {
        use super::PlatformEvent;
        use crate::serial::{SerialOutput, SerialOutputMode};

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();

        // Nothing pending on a fresh machine.
        assert_eq!(pc.poll_platform_events(), None);

        // The guest writes the S5 ACPI transition to PM1a_CNT (0x604):
        // SLP_TYP=5 | SLP_EN(1<<13).
        let s5 = (5u16 << 10) | (1 << 13);
        VmExitHandler::io_out(&mut pc.bus, 0x604, &s5.to_le_bytes());
        assert_eq!(pc.poll_platform_events(), Some(PlatformEvent::Sleep(5)));
        assert_eq!(pc.poll_platform_events(), None, "latch consumed");

        // The guest pulses a fast reset through System Control Port A (0x92 bit 0).
        VmExitHandler::io_out(&mut pc.bus, 0x92, &[0x01]);
        assert_eq!(pc.poll_platform_events(), Some(PlatformEvent::Reset));
        assert_eq!(pc.poll_platform_events(), None);

        // The guest reboots via the chipset Reset Control Register (0xCF9): a
        // byte write with RST_CPU set (SYS_RST|RST_CPU = 0x06) — the reboot=pci
        // path — also surfaces as a Reset event.
        VmExitHandler::io_out(&mut pc.bus, 0xCF9, &[0x06]);
        assert_eq!(pc.poll_platform_events(), Some(PlatformEvent::Reset));
        assert_eq!(pc.poll_platform_events(), None, "0xCF9 latch consumed");
    }

    #[test]
    fn pci_intx_routes_through_the_lpc_bridge_to_both_controllers() {
        use super::ICH9_LPC_BDF;
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::interrupt::ELCR_SLAVE;

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            1_780_838_055,
            1,
        )
        .unwrap();
        pc.ioapic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));

        // The guest enumerates the ICH9 LPC bridge (00:1F.0) and routes PIRQB ->
        // IRQ10 by writing its config register 0x61.
        {
            let mut rc = pc.pcie.borrow_mut();
            let bridge = rc.find_device_mut(&ICH9_LPC_BDF).unwrap();
            bridge.write_u8(0x61, 10);
        }

        // Program the 8259 (PC/AT layout) and mark IRQ10 (slave line 2) level in
        // the ELCR — PCI interrupts are level-triggered.
        pc.pic.with(|p| {
            for (port, val) in [
                (0x20u16, 0x11u8),
                (0x21, 0x20),
                (0x21, 0x04),
                (0x21, 0x01),
                (0xA0, 0x11),
                (0xA1, 0x28),
                (0xA1, 0x02),
                (0xA1, 0x01),
                (0x21, 0x00),
                (0xA1, 0x00),
            ] {
                p.write_port(port, val);
            }
            p.write_elcr(ELCR_SLAVE, 1 << 2);
        });

        // Program PIRQB's I/O APIC GSI (16 + 1 = 17): vector 0x50 -> LAPIC 0.
        // REDTBL base 0x10 + 17*2 = 0x32 (low), 0x33 (high).
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x32u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0x50u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0000, &0x33u32.to_le_bytes());
        VmExitHandler::mmio_write(&mut pc.bus, 0xFEC0_0010, &0u32.to_le_bytes());

        // A device in slot 1 asserts INTA: swizzles to PIRQB, which the guest
        // routed to IRQ10 (PIC) and which wires to GSI 17 (I/O APIC).
        pc.assert_pci_intx(1, 1, true);
        // The 8259 presents IRQ10's vector (slave base 0x28 + line 2 = 0x2A)...
        assert_eq!(pc.pic.with(|p| p.pending_vector()), Some(0x2A));
        // ...and the I/O APIC delivered GSI 17's vector to LAPIC 0.
        assert_eq!(pc.ioapic.with(|c| c.pending_vector(0)), Some(0x50));

        // Deasserting the level line withdraws it from the 8259.
        pc.assert_pci_intx(1, 1, false);
        assert_eq!(pc.pic.with(|p| p.pending_vector()), None);
    }

    #[test]
    fn standard_pc_with_interrupts_delivers_nothing_through_a_masked_ioapic() {
        use crate::serial::{SerialOutput, SerialOutputMode, COM1, IER_REG};
        use enlil_devices::interrupt::SharedInterruptController;

        // LAPIC enabled but the RTEs are left at their reset (masked) state: a
        // device asserting before the OS programs the I/O APIC reaches no vCPU.
        let pic = SharedInterruptController::new(1);
        pic.with(|c| c.lapics[0].write_register(0x0F0, 0x1FF));

        let (mut bus, _pcie) = DeviceBus::standard_pc_with_interrupts(
            SerialOutput::new("guest", SerialOutputMode::Null),
            &pic,
        )
        .unwrap();

        VmExitHandler::io_out(&mut bus, COM1 + IER_REG, &[0x02]);
        assert!(!pic.with(|c| c.has_pending(0)));
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
        use crate::kvm_backend::{is_kvm_available, GuestExit, GuestRam, KvmBackend};
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

        // Back the guest with one page-aligned page; place the code at
        // guest-physical 0x1000. A plain `Vec<u8>` is only byte-aligned and
        // KVM would reject it with EINVAL, so use page-aligned `GuestRam`.
        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        // No in-kernel IRQ chip: with an in-kernel local APIC, KVM handles
        // `HLT` itself (the vCPU parks waiting for an interrupt) and never
        // exits with `KVM_EXIT_HLT`, so this "run until it halts" probe would
        // block forever. Without the IRQ chip, `HLT` exits to userspace.
        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
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

    // Proves the *input* (`in`) path end-to-end on real KVM: the guest reads
    // the COM1 line-status register, and the byte the SerialPort returns has to
    // flow device → KVM `KVM_EXIT_IO`(in) → guest `AL` → KVM `KVM_EXIT_IO`(out)
    // → SerialPort sink. The smoke test above only covers the output path.
    //
    // Self-skips when `/dev/kvm` is unavailable rather than faking a pass.
    #[cfg(target_os = "linux")]
    #[test]
    fn serial_input_path_smoke() {
        use crate::kvm_backend::{is_kvm_available, GuestExit, GuestRam, KvmBackend};
        use crate::serial::{SerialOutput, SerialOutputMode, SerialPort};
        use std::sync::{Arc, Mutex};

        if !is_kvm_available() {
            eprintln!("skipping serial_input_path_smoke: /dev/kvm not available");
            return;
        }

        // 16-bit real-mode blob: read COM1 LSR into AL, echo AL to COM1, halt.
        //   BA FD 03   mov dx, 0x3FD   ; line-status register
        //   EC         in  al, dx      ; al = LSR (0x60 = THRE|TEMT, idle UART)
        //   BA F8 03   mov dx, 0x3F8   ; transmit holding register
        //   EE         out dx, al      ; echo the status byte back out
        //   F4         hlt
        #[rustfmt::skip]
        let code: [u8; 9] = [
            0xBA, 0xFD, 0x03,
            0xEC,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        // No in-kernel IRQ chip, so `HLT` exits to userspace (see the smoke
        // test above for why).
        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let captured = Arc::new(Mutex::new(Vec::new()));
        let serial = SerialPort::com1(SerialOutput::new(
            "in-smoke",
            SerialOutputMode::Shared(Arc::clone(&captured)),
        ));
        let mut bus = DeviceBus::new();
        bus.add_serial(serial).unwrap();

        let mut halted = false;
        for _ in 0..100 {
            let exit = backend.run_vcpu(0, &mut bus).expect("run vcpu");
            if exit == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");

        // The single byte echoed back is exactly the LSR value the SerialPort
        // computed on the `in` exit — an idle UART reports THRE|TEMT (0x60) —
        // proving the device's input data reached the guest register and
        // round-tripped back out through the bus on real KVM.
        assert_eq!(&*captured.lock().unwrap(), &[0x60]);
    }

    // Proves the MMIO exit path (read and write) end-to-end on real KVM. The
    // PIO smoke tests above cover port I/O; xHCI and the other modern devices
    // are MMIO-driven, so the doorbell/ring work depends on this path. A
    // real-mode blob writes a byte to a device mapped at a low (16-bit
    // reachable) address, then reads it back and echoes the read to COM1.
    //
    // Self-skips when `/dev/kvm` is unavailable rather than faking a pass.
    #[cfg(target_os = "linux")]
    #[test]
    fn mmio_path_smoke() {
        use crate::kvm_backend::{is_kvm_available, GuestExit, GuestRam, KvmBackend};
        use crate::serial::{SerialOutput, SerialOutputMode, SerialPort};
        use std::sync::{Arc, Mutex};

        if !is_kvm_available() {
            eprintln!("skipping mmio_path_smoke: /dev/kvm not available");
            return;
        }

        // 16-bit real-mode blob (DS base 0, so [0x8000] is guest-physical
        // 0x8000 — unmapped RAM, so KVM traps it as MMIO):
        //   B0 55         mov al, 0x55
        //   A2 00 80      mov [0x8000], al   ; MMIO write 0x55 -> device off 0
        //   A0 00 80      mov al, [0x8000]   ; MMIO read -> al = 0x3C sentinel
        //   BA F8 03      mov dx, 0x3F8      ; COM1 transmit holding register
        //   EE            out dx, al         ; echo the read byte out
        //   F4            hlt
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0xB0, 0x55,
            0xA2, 0x00, 0x80,
            0xA0, 0x00, 0x80,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let captured = Arc::new(Mutex::new(Vec::new()));
        let serial = SerialPort::com1(SerialOutput::new(
            "mmio-smoke",
            SerialOutputMode::Shared(Arc::clone(&captured)),
        ));
        let last_write = Rc::new(RefCell::new(None));
        let mut bus = DeviceBus::new();
        bus.add_serial(serial).unwrap();
        bus.add_mmio(Box::new(LowMmio {
            last_write: Rc::clone(&last_write),
        }))
        .unwrap();

        let mut halted = false;
        for _ in 0..100 {
            let exit = backend.run_vcpu(0, &mut bus).expect("run vcpu");
            if exit == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");

        // The MMIO write reached the device at offset 0 with the byte the guest
        // stored, and the value the device returned on the MMIO read flowed
        // back into the guest register (then out to the serial sink).
        assert_eq!(*last_write.borrow(), Some((0, 0x55)));
        assert_eq!(&*captured.lock().unwrap(), &[0x3C]);
    }

    // The other guest-boot tests use new_without_irqchip() so HLT exits. This
    // one runs a guest on the *production* backend (new(), in-kernel IRQ chip)
    // to prove device PIO exits still reach the DeviceBus there. Port I/O exits
    // to userspace even with the in-kernel APIC; only HLT is absorbed by it, so
    // we stop once the expected output arrives rather than waiting for a halt
    // that never surfaces. Self-skips without /dev/kvm.
    #[cfg(target_os = "linux")]
    #[test]
    fn serial_output_under_production_irqchip() {
        use crate::kvm_backend::{is_kvm_available, GuestRam, KvmBackend};
        use crate::serial::{SerialOutput, SerialOutputMode, SerialPort};
        use std::sync::{Arc, Mutex};

        if !is_kvm_available() {
            eprintln!("skipping serial_output_under_production_irqchip: no /dev/kvm");
            return;
        }

        // Same OK-then-HLT blob as the smoke test (out 'O', out 'K', hlt).
        #[rustfmt::skip]
        let code: [u8; 10] = [0xBA,0xF8,0x03, 0xB0,0x4F, 0xEE, 0xB0,0x4B, 0xEE, 0xF4];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        // Production constructor: WITH the in-kernel IRQ chip.
        let mut backend = KvmBackend::new().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let captured = Arc::new(Mutex::new(Vec::new()));
        let serial = SerialPort::com1(SerialOutput::new(
            "prod-irqchip",
            SerialOutputMode::Shared(Arc::clone(&captured)),
        ));
        let mut bus = DeviceBus::new();
        bus.add_serial(serial).unwrap();

        // Run until the two output bytes have been collected. Crucially, do not
        // call run_vcpu again afterwards: the guest's next instruction is HLT,
        // which the in-kernel APIC parks on (no KVM_EXIT_HLT), so another
        // KVM_RUN would block.
        let mut got = false;
        for _ in 0..100 {
            backend.run_vcpu(0, &mut bus).expect("run vcpu");
            if captured.lock().unwrap().len() >= 2 {
                got = true;
                break;
            }
        }
        assert!(
            got,
            "guest produced no serial output under the production IRQ chip"
        );
        assert_eq!(&*captured.lock().unwrap(), b"OK");
    }

    // End-to-end on real KVM: the whole stealth MSR stack assembled through the
    // platform — install_stealth_msr_router on the bus, enable_userspace_msr_exits
    // + forward_msrs_to_userspace(router.filter_ranges()) on the backend — lets a
    // guest rdmsr APERF (KVM-known, normally in-kernel) and read back the value
    // the run loop seeded into the timing shadow, routed through the DeviceBus
    // handler. Self-skips without /dev/kvm or the userspace-MSR cap.
    #[cfg(target_os = "linux")]
    #[test]
    fn full_stealth_stack_serves_seeded_aperf_to_a_guest() {
        use crate::kvm_backend::{is_kvm_available, GuestExit, GuestRam, KvmBackend};
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::stealth::lbr::LbrPlatform;
        use std::sync::{Arc, Mutex};

        if !is_kvm_available() {
            eprintln!("skipping full_stealth_stack_...: no /dev/kvm");
            return;
        }

        // rdmsr(IA32_APERF=0xE8); out 0x3F8, al; hlt.
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0x66, 0xB9, 0xE8, 0x00, 0x00, 0x00,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];
        const ENTRY: u64 = 0x1000;
        const APERF: u32 = 0xE8;

        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Shared(Arc::clone(&sink))),
            0,
            1,
        )
        .expect("build standard pc");

        // Install the router and seed APERF with a known value (low byte 0xBE).
        let timing = pc.install_stealth_msr_router(LbrPlatform::AmdSvm);
        timing.write_aperf(0x0000_0000_0000_00BE);
        let ranges = pc
            .bus
            .stealth_msr_mut()
            .expect("router installed")
            .filter_ranges();

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        if let Err(e) = backend.enable_userspace_msr_exits() {
            eprintln!("skipping full_stealth_stack_...: {e}");
            return;
        }
        backend
            .forward_msrs_to_userspace(&ranges)
            .expect("install msr filter");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut saw_aperf = false;
        let mut halted = false;
        for _ in 0..100 {
            match backend.run_vcpu(0, &mut pc.bus).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                GuestExit::MsrRead { msr } if msr == APERF => saw_aperf = true,
                _ => {}
            }
        }
        assert!(halted, "guest never reached HLT");
        assert!(saw_aperf, "APERF was not forwarded to the bus");
        // The seeded shadow's low byte reached the guest and echoed out COM1.
        assert_eq!(&*sink.lock().unwrap(), &[0xBE]);
    }

    // The bus delegates forwarded MSR exits to an installed stealth router, and
    // refuses every MSR (→ #GP) when none is installed — the wiring that lets a
    // guest read spoofed APERF/MPERF/PMC/LBR values through the run-loop handler.
    #[test]
    fn stealth_msr_router_serves_msr_exits_through_the_bus() {
        use crate::kvm_backend::VmExitHandler;
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{intel_msr, LbrPlatform, LbrState};
        use enlil_devices::stealth::timing::msr as timing_msr;

        let mut bus = DeviceBus::new();
        // No router yet: an unmodelled MSR is refused, not silently spoofed.
        assert_eq!(bus.rdmsr(timing_msr::IA32_APERF), None);
        assert!(!bus.wrmsr(intel_msr::IA32_DEBUGCTL, 1));

        let timing = VcpuTimingState::new();
        timing.write_aperf(0xABCD);
        bus.set_stealth_msr_router(StealthMsrRouter::new(
            timing,
            LbrState::new(LbrPlatform::AmdSvm),
        ));

        // Reads now come from the shadow state.
        assert_eq!(bus.rdmsr(timing_msr::IA32_APERF), Some(0xABCD));
        // Writes reach the shadows: enabling LBR via DEBUGCTL is observable.
        assert!(bus.wrmsr(intel_msr::IA32_DEBUGCTL, 1));
        assert_eq!(bus.rdmsr(intel_msr::IA32_DEBUGCTL), Some(1));
        assert!(bus.stealth_msr_mut().unwrap().lbr.lbr_enabled);
        // An MSR outside the modelled set still falls through to #GP.
        assert_eq!(bus.rdmsr(0x10), None);
    }

    // The platform convenience installs a router seeded to the model ratio and
    // hands back a shared timing handle the run loop drives — so APERF/MPERF
    // read non-zero with the model's core/ref ratio (never the 1.0 tell), and
    // driving the returned handle is visible through the bus.
    #[test]
    fn install_stealth_msr_router_seeds_the_model_ratio_and_shares_the_handle() {
        use crate::kvm_backend::VmExitHandler;
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::stealth::lbr::LbrPlatform;
        use enlil_devices::stealth::pmc::PmcRateModel;
        use enlil_devices::stealth::timing::msr as timing_msr;

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            0,
            1,
        )
        .expect("build standard pc");

        let timing = pc.install_stealth_msr_router(LbrPlatform::AmdSvm);

        let aperf = pc.bus.rdmsr(timing_msr::IA32_APERF).expect("APERF served");
        let mperf = pc.bus.rdmsr(timing_msr::IA32_MPERF).expect("MPERF served");
        assert!(aperf > 0 && mperf > 0, "shadows seeded non-zero");
        assert_ne!(aperf, mperf, "seeded ratio must not be the 1.0 tell");
        assert_eq!(
            aperf * 1000 / mperf,
            PmcRateModel::DEFAULT.core_per_kilo_ref,
            "APERF/MPERF must encode the model core/ref ratio"
        );

        // The returned handle aliases the router's shadow: a run-loop write is
        // visible through the bus's MSR read.
        timing.write_aperf(0x1_2345);
        assert_eq!(pc.bus.rdmsr(timing_msr::IA32_APERF), Some(0x1_2345));
    }

    // A per-vCPU bank serves the *active* vCPU's shadow: with two routers
    // installed, the MSR a guest reads through the bus depends on which vCPU is
    // active — independent state, never one shared counter. This is the core
    // of per-vCPU stealth: an SMP guest sees each logical CPU's own APERF.
    #[test]
    fn stealth_bank_serves_the_active_vcpu_shadow() {
        use crate::kvm_backend::VmExitHandler;
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{LbrPlatform, LbrState};
        use enlil_devices::stealth::timing::msr as timing_msr;

        // Two vCPUs, each APERF distinct so we can tell whose shadow answered.
        let t0 = VcpuTimingState::new();
        t0.write_aperf(0xAAAA);
        let t1 = VcpuTimingState::new();
        t1.write_aperf(0xBBBB);
        let mut bus = DeviceBus::new();
        bus.set_stealth_msr_routers(vec![
            StealthMsrRouter::new(t0, LbrState::new(LbrPlatform::AmdSvm)),
            StealthMsrRouter::new(t1, LbrState::new(LbrPlatform::AmdSvm)),
        ]);
        assert_eq!(bus.stealth_vcpu_count(), 2);
        assert_eq!(bus.active_vcpu(), Some(0), "active starts at vCPU 0");

        // vCPU 0 active → reads vCPU 0's shadow.
        assert_eq!(bus.rdmsr(timing_msr::IA32_APERF), Some(0xAAAA));
        // Select vCPU 1 → reads vCPU 1's shadow, not vCPU 0's.
        assert!(bus.set_active_vcpu(1));
        assert_eq!(bus.active_vcpu(), Some(1));
        assert_eq!(bus.rdmsr(timing_msr::IA32_APERF), Some(0xBBBB));
        // Back to vCPU 0.
        assert!(bus.set_active_vcpu(0));
        assert_eq!(bus.rdmsr(timing_msr::IA32_APERF), Some(0xAAAA));
    }

    // A WRMSR forwarded while vCPU 1 is active lands in vCPU 1's shadow only —
    // vCPU 0's identical MSR is untouched. Proves writes are per-vCPU too, so
    // one vCPU programming DEBUGCTL/LBR cannot leak into another's view.
    #[test]
    fn stealth_bank_writes_isolate_per_vcpu() {
        use crate::kvm_backend::VmExitHandler;
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{intel_msr, LbrPlatform, LbrState};

        let mut bus = DeviceBus::new();
        bus.set_stealth_msr_routers(vec![
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::IntelVmx)),
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::IntelVmx)),
        ]);

        // Enable LBR (DEBUGCTL bit 0) on vCPU 1 only.
        assert!(bus.set_active_vcpu(1));
        assert!(bus.wrmsr(intel_msr::IA32_DEBUGCTL, 1));
        assert!(bus.stealth_msr_for_mut(1).unwrap().lbr.lbr_enabled);
        // vCPU 0 never saw the write.
        assert!(!bus.stealth_msr_for_mut(0).unwrap().lbr.lbr_enabled);
        assert!(bus.set_active_vcpu(0));
        assert_eq!(bus.rdmsr(intel_msr::IA32_DEBUGCTL), Some(0));
    }

    // Out-of-range selections are rejected and leave the active vCPU unchanged;
    // selecting on an empty bus is a no-op. The bank invariant (active always
    // names a real router) is what makes the handler's `routers[active]`
    // indexing infallible.
    #[test]
    fn stealth_bank_rejects_out_of_range_active() {
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{LbrPlatform, LbrState};

        let mut bus = DeviceBus::new();
        // No bank installed: selection fails, count is 0, active is None.
        assert!(!bus.set_active_vcpu(0));
        assert_eq!(bus.active_vcpu(), None);
        assert_eq!(bus.stealth_vcpu_count(), 0);

        bus.set_stealth_msr_routers(vec![
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::AmdSvm)),
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::AmdSvm)),
        ]);
        assert!(bus.set_active_vcpu(1));
        // Index == len and beyond are rejected; the prior selection stands.
        assert!(!bus.set_active_vcpu(2));
        assert!(!bus.set_active_vcpu(99));
        assert_eq!(
            bus.active_vcpu(),
            Some(1),
            "rejected selection left active as-is"
        );
        // A specific vCPU's router is reachable regardless of the active one.
        assert!(bus.stealth_msr_for_mut(0).is_some());
        assert!(bus.stealth_msr_for_mut(1).is_some());
        assert!(bus.stealth_msr_for_mut(2).is_none());
    }

    // The multi-vCPU install helper seeds every vCPU's shadows to the model
    // ratio independently and hands back one timing handle per vCPU; driving
    // handle[i] is visible only through vCPU i's MSR view.
    #[test]
    fn install_stealth_msr_routers_seeds_each_vcpu_independently() {
        use crate::kvm_backend::VmExitHandler;
        use crate::serial::{SerialOutput, SerialOutputMode};
        use enlil_devices::stealth::lbr::LbrPlatform;
        use enlil_devices::stealth::pmc::PmcRateModel;
        use enlil_devices::stealth::timing::msr as timing_msr;

        let mut pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            0,
            1,
        )
        .expect("build standard pc");

        let handles = pc.install_stealth_msr_routers(LbrPlatform::IntelVmx, 3);
        assert_eq!(handles.len(), 3);
        assert_eq!(pc.bus.stealth_vcpu_count(), 3);

        // Every vCPU is seeded to the model ratio (not the 1.0 tell).
        for i in 0..3 {
            assert!(pc.bus.set_active_vcpu(i));
            let aperf = pc.bus.rdmsr(timing_msr::IA32_APERF).expect("APERF served");
            let mperf = pc.bus.rdmsr(timing_msr::IA32_MPERF).expect("MPERF served");
            assert!(aperf > 0 && mperf > 0, "vCPU {i} shadows seeded non-zero");
            assert_eq!(
                aperf * 1000 / mperf,
                PmcRateModel::DEFAULT.core_per_kilo_ref,
                "vCPU {i} APERF/MPERF encodes the model ratio"
            );
        }

        // The handles are independent: writing handle[2] is visible only on
        // vCPU 2, not vCPU 0.
        handles[2].write_aperf(0xDEAD_BEEF);
        pc.bus.set_active_vcpu(2);
        assert_eq!(pc.bus.rdmsr(timing_msr::IA32_APERF), Some(0xDEAD_BEEF));
        pc.bus.set_active_vcpu(0);
        assert_ne!(pc.bus.rdmsr(timing_msr::IA32_APERF), Some(0xDEAD_BEEF));
    }
}
