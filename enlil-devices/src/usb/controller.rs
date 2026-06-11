//! The assembled per-guest virtual xHCI controller (Phase 4.4).
//!
//! Wires the xHCI building blocks — capability/operational/runtime register
//! files, port register sets, the doorbell array, the command ring, and the
//! interrupter's event ring — into one controller with a single MMIO-style
//! register window, laid out exactly as the capability registers advertise:
//!
//! ```text
//! 0x0000  Capability registers   (CAPLENGTH = 0x20)
//! 0x0020  Operational registers  (ports at op-relative 0x400)
//! 0x1000  Runtime registers      (RTSOFF; interrupter 0 at +0x20)
//! 0x2000  Doorbell array         (DBOFF; DB0 = command ring)
//! ```
//!
//! Command processing follows xHCI §4.6: a doorbell-0 ring while the
//! controller is running drains the command ring, executing each command
//! (slot management is real — Enable/Disable Slot allocate and free from the
//! slot pool) and posting a Command Completion event per command on the
//! interrupter's event ring. Device attach is modelled by
//! [`connect_device`](VirtualXhciController::connect_device), which flips the
//! port's `PORTSC` and posts the Port Status Change event a guest's xHCI
//! driver waits for.
//!
//! Guest-memory-resident rings (fetching TRBs at the guest's `CRCR`/`ERSTBA`
//! addresses) wait on the KVM run loop; until then commands are submitted
//! through [`submit_command`](VirtualXhciController::submit_command), which
//! models the guest's enqueue, and events are drained with
//! [`pop_event`](VirtualXhciController::pop_event).

use super::types::DeviceSpeed as UsbSpeed;
use super::xhci::{
    CapabilityRegisters, CommandRing, CommandTrb, DoorbellArray, DoorbellTarget, EventRing,
    EventTrb, InterrupterRegisterSet, OperationalRegisters, RuntimeRegisters, Trb,
    TrbCompletionCode,
};
use crate::truncate::u32_of;

/// `USBSTS` bit 3: Event Interrupt (EINT) — set when an event is posted.
const USBSTS_EINT: u32 = 1 << 3;
/// `USBSTS` bit 4: Port Change Detect (PCD).
const USBSTS_PCD: u32 = 1 << 4;

/// The per-guest virtual xHCI controller.
///
/// One instance per guest; the routing engine attaches the physical devices
/// it routed here by connecting them to this controller's ports.
#[derive(Debug)]
pub struct VirtualXhciController {
    /// Capability register block (fixes the window layout below).
    pub caps: CapabilityRegisters,
    /// Operational registers, including the port register sets.
    pub op: OperationalRegisters,
    /// Runtime registers (`MFINDEX`).
    pub runtime: RuntimeRegisters,
    /// Interrupter 0: interrupt management + the event ring.
    pub interrupter: InterrupterRegisterSet,
    /// Doorbell array (index 0 = command ring, 1.. = device slots).
    pub doorbells: DoorbellArray,
    /// The command ring doorbell 0 drains.
    pub command_ring: CommandRing,
    /// Device-slot allocation state; index = slot ID - 1.
    slots: Vec<bool>,
}

impl VirtualXhciController {
    /// A halted controller with `num_ports` empty ports and no slots enabled.
    #[must_use]
    pub fn new(num_ports: u8) -> Self {
        let caps = CapabilityRegisters::new_virtual(num_ports);
        let slots = vec![false; caps.max_slots() as usize];
        Self {
            op: OperationalRegisters::new(num_ports as usize),
            runtime: RuntimeRegisters::new(),
            interrupter: InterrupterRegisterSet::new(),
            doorbells: DoorbellArray::new(caps.max_slots()),
            command_ring: CommandRing::with_default_size(),
            slots,
            caps,
        }
    }

    /// Read a 32-bit register at `offset` within the controller's MMIO window.
    #[must_use]
    pub fn read_register(&self, offset: u32) -> u32 {
        let op_base = u32::from(self.caps.caplength);
        match offset {
            o if o < op_base => self.caps.read(o),
            o if o < self.caps.rtsoff => self.op.read(o - op_base),
            o if o < self.caps.dboff => self.read_runtime(o - self.caps.rtsoff),
            // Doorbells are write-only; reads return zero (xHCI §5.6).
            _ => 0,
        }
    }

    /// Write a 32-bit register at `offset` within the controller's MMIO
    /// window. A doorbell-0 write kicks command-ring processing.
    pub fn write_register(&mut self, offset: u32, value: u32) {
        let op_base = u32::from(self.caps.caplength);
        match offset {
            // Capability registers are read-only.
            o if o < op_base => {}
            o if o < self.caps.rtsoff => self.write_operational(o - op_base, value),
            o if o < self.caps.dboff => self.write_runtime(o - self.caps.rtsoff, value),
            o => {
                let index = u8::try_from((o - self.caps.dboff) / 4).unwrap_or(u8::MAX);
                if self.doorbells.write(index, value) == Some(DoorbellTarget::HostCommand) {
                    self.process_command_ring();
                    self.doorbells.clear_pending(index);
                }
            }
        }
    }

    fn write_operational(&mut self, offset: u32, value: u32) {
        match offset {
            0x00 => self.op.write_usbcmd(value),
            0x04 => self.op.write_usbsts(value),
            0x14 => self.op.dnctrl = value,
            0x18 => self.op.crcr = (self.op.crcr & !0xFFFF_FFFF) | u64::from(value),
            0x1C => self.op.crcr = (self.op.crcr & 0xFFFF_FFFF) | (u64::from(value) << 32),
            0x30 => self.op.dcbaap = (self.op.dcbaap & !0xFFFF_FFFF) | u64::from(value),
            0x34 => self.op.dcbaap = (self.op.dcbaap & 0xFFFF_FFFF) | (u64::from(value) << 32),
            0x38 => self.op.config = value,
            o if o >= 0x400 => {
                // PORTSC is the only writable port register modelled;
                // PORTPMSC/PORTLI/PORTHLPMC writes are accepted and ignored.
                let port = ((o - 0x400) / 16) as usize;
                if (o - 0x400) % 16 == 0
                    && let Some(p) = self.op.ports.get_mut(port)
                {
                    p.write_portsc(value);
                }
            }
            _ => {}
        }
    }

    fn read_runtime(&self, offset: u32) -> u32 {
        let split = |v: u64, hi: bool| if hi { u32_of(v >> 32) } else { u32_of(v) };
        match offset {
            0x00 => self.runtime.mfindex,
            // Interrupter 0 register set at RTSOFF + 0x20 (xHCI §5.5.2).
            0x20 => self.interrupter.iman,
            0x24 => self.interrupter.imod,
            0x28 => self.interrupter.erstsz,
            0x30 => split(self.interrupter.erstba, false),
            0x34 => split(self.interrupter.erstba, true),
            0x38 => split(self.interrupter.erdp, false),
            0x3C => split(self.interrupter.erdp, true),
            _ => 0,
        }
    }

    fn write_runtime(&mut self, offset: u32, value: u32) {
        let lo = |v: u64| (v & !0xFFFF_FFFF) | u64::from(value);
        let hi = |v: u64| (v & 0xFFFF_FFFF) | (u64::from(value) << 32);
        match offset {
            0x20 => self.interrupter.write_iman(value),
            0x24 => self.interrupter.imod = value,
            0x28 => self.interrupter.erstsz = value,
            0x30 => self.interrupter.erstba = lo(self.interrupter.erstba),
            0x34 => self.interrupter.erstba = hi(self.interrupter.erstba),
            0x38 => self.interrupter.write_erdp(lo(self.interrupter.erdp)),
            0x3C => self.interrupter.write_erdp(hi(self.interrupter.erdp)),
            _ => {}
        }
    }

    /// Model the guest enqueueing a command on the command ring (until rings
    /// live in guest memory, this stands in for the guest's TRB write). Ring
    /// doorbell 0 — via [`write_register`](Self::write_register) at the
    /// `DBOFF` window — to have it processed.
    pub fn submit_command(&mut self, command: &CommandTrb) -> bool {
        self.command_ring.submit(command.to_trb(true))
    }

    /// Drain the command ring (doorbell 0, xHCI §4.6): execute each command
    /// and post its Command Completion event. A halted controller leaves the
    /// ring untouched, as real hardware does.
    fn process_command_ring(&mut self) {
        if !self.op.is_running() {
            return;
        }
        while let Some(trb) = self.command_ring.fetch() {
            let (code, slot_id) = match CommandTrb::from_trb(&trb) {
                Some(CommandTrb::NoOp) => (TrbCompletionCode::Success, 0),
                Some(CommandTrb::EnableSlot) => self.enable_slot(),
                Some(CommandTrb::DisableSlot { slot_id }) => (self.disable_slot(slot_id), slot_id),
                // Address/configure/reset/stop need the device-context memory
                // the KVM run loop will provide; succeed on an enabled slot so
                // a driver's bring-up sequence can proceed, error otherwise.
                Some(
                    CommandTrb::AddressDevice { slot_id, .. }
                    | CommandTrb::ConfigureEndpoint { slot_id, .. }
                    | CommandTrb::ResetEndpoint { slot_id, .. }
                    | CommandTrb::StopEndpoint { slot_id, .. },
                ) => (self.slot_dependent_success(slot_id), slot_id),
                // An undecodable TRB on the command ring is a TRB error.
                None => (TrbCompletionCode::TrbError, 0),
            };
            self.post_event_trb(|ring| ring.post_command_completion(trb.parameter, code, slot_id));
        }
    }

    /// Allocate the lowest free device slot.
    fn enable_slot(&mut self) -> (TrbCompletionCode, u8) {
        let limit = usize::from(self.op.max_slots_enabled().min(self.caps.max_slots()));
        for (i, used) in self.slots.iter_mut().take(limit).enumerate() {
            if !*used {
                *used = true;
                return (
                    TrbCompletionCode::Success,
                    u8::try_from(i + 1).unwrap_or(u8::MAX),
                );
            }
        }
        (TrbCompletionCode::NoSlotsAvailableError, 0)
    }

    /// Free a slot; disabling a never-enabled slot is a TRB error.
    fn disable_slot(&mut self, slot_id: u8) -> TrbCompletionCode {
        match self.slots.get_mut(usize::from(slot_id.wrapping_sub(1))) {
            Some(used) if *used => {
                *used = false;
                TrbCompletionCode::Success
            }
            _ => TrbCompletionCode::TrbError,
        }
    }

    fn slot_dependent_success(&self, slot_id: u8) -> TrbCompletionCode {
        match self.slots.get(usize::from(slot_id.wrapping_sub(1))) {
            Some(true) => TrbCompletionCode::Success,
            _ => TrbCompletionCode::TrbError,
        }
    }

    /// Whether a device slot is currently enabled.
    #[must_use]
    pub fn slot_enabled(&self, slot_id: u8) -> bool {
        self.slots
            .get(usize::from(slot_id.wrapping_sub(1)))
            .copied()
            .unwrap_or(false)
    }

    /// Attach a device of the given [`UsbSpeed`] to the **lowest free**
    /// root-hub port, returning the 0-based port index it landed on (or
    /// `None` if every port is occupied). This is the routing engine's entry
    /// point: it routes a device to this guest's controller without choosing
    /// a port itself.
    pub fn attach_device(&mut self, speed: UsbSpeed) -> Option<usize> {
        let port = self.op.ports.iter().position(|p| !p.is_connected())?;
        if self.connect_device(port, speed.xhci_speed_id()) {
            Some(port)
        } else {
            None
        }
    }

    /// Connect a device to `port` (0-based) at the xHCI speed code
    /// (1=FS, 2=LS, 3=HS, 4=SS): flips the port's `PORTSC` (CCS + CSC), sets
    /// `USBSTS.PCD`, and posts the Port Status Change event (port IDs are
    /// 1-based on the event) the guest's driver waits for.
    pub fn connect_device(&mut self, port: usize, speed: u8) -> bool {
        let Some(p) = self.op.ports.get_mut(port) else {
            return false;
        };
        p.connect_device(speed);
        self.signal_port_change(port)
    }

    /// Disconnect the device on `port` (0-based), with the same status-change
    /// signalling as a connect.
    pub fn disconnect_device(&mut self, port: usize) -> bool {
        let Some(p) = self.op.ports.get_mut(port) else {
            return false;
        };
        p.disconnect_device();
        self.signal_port_change(port)
    }

    fn signal_port_change(&mut self, port: usize) -> bool {
        self.op.usbsts |= USBSTS_PCD;
        let port_id = u8::try_from(port + 1).unwrap_or(u8::MAX);
        self.post_event_trb(|ring| {
            ring.post_port_status_change(port_id, TrbCompletionCode::Success)
        })
    }

    /// Post an event through `post` and raise the interrupt surfaces:
    /// `USBSTS.EINT` and the interrupter's IP bit (IP latches regardless of
    /// IE — IE only gates whether the pin asserts, xHCI §5.5.2.1).
    fn post_event_trb(&mut self, post: impl FnOnce(&mut EventRing) -> bool) -> bool {
        let posted = post(&mut self.interrupter.event_ring);
        if posted {
            self.op.usbsts |= USBSTS_EINT;
            self.interrupter.set_pending(true);
        }
        posted
    }

    /// The level of the controller's legacy `INTx` pin: asserted while the
    /// interrupter has a pending event (IP) with interrupts enabled (IE).
    #[must_use]
    pub const fn intx_level(&self) -> bool {
        self.interrupter.interrupt_pending() && self.interrupter.interrupt_enabled()
    }

    /// Pop the next pending event from the interrupter's event ring (what the
    /// guest's event-ring dequeue models until rings live in guest memory).
    pub fn pop_event(&mut self) -> Option<EventTrb> {
        let trb = self.next_event_raw()?;
        EventTrb::from_trb(&trb)
    }

    fn next_event_raw(&mut self) -> Option<Trb> {
        let ring = &mut self.interrupter.event_ring;
        if ring.is_empty() {
            return None;
        }
        let idx = ring.dequeue_index();
        let trb = ring.read_trb(idx).copied()?;
        ring.set_dequeue_index((idx + 1) % ring.capacity());
        Some(trb)
    }
}

/// A shared handle to one guest's virtual xHCI controller.
///
/// The MMIO adapter below holds one clone for the guest's register accesses;
/// the platform holds another for hot-plug (`connect_device`) and the run
/// loop's interrupt sampling. `Rc<RefCell>` matches the single-threaded bus
/// (like [`SharedRootComplex`](crate::pcie::SharedRootComplex)); a hot-plug
/// monitor on another thread posts events through a channel the run loop
/// drains, it does not touch this handle directly.
pub type SharedXhci = std::rc::Rc<std::cell::RefCell<VirtualXhciController>>;

/// Bus adapter exposing a [`VirtualXhciController`]'s register window as MMIO
/// at the base address its PCI BAR0 advertises.
///
/// xHCI registers are 32-bit (64-bit registers are two dword halves, which
/// the controller's window already models), so accesses are dispatched as
/// dwords; a 64-bit access is split into two. After every access — and after
/// any event the access may have caused — the adapter pushes the controller's
/// [`intx_level`](VirtualXhciController::intx_level) into the wired interrupt
/// sink, giving level-triggered `INTx` semantics: asserted while IP&IE,
/// withdrawn when the guest clears IP through `IMAN`.
pub struct XhciMmio {
    controller: SharedXhci,
    base: u64,
    size: u64,
    interrupt_line: Option<Box<dyn Fn(bool)>>,
}

impl XhciMmio {
    /// Wrap `controller`, claiming `size` bytes of MMIO at `base` (the BAR0
    /// window; 64 KiB on the PCI function Enlil mounts).
    #[must_use]
    pub const fn new(controller: SharedXhci, base: u64, size: u64) -> Self {
        Self {
            controller,
            base,
            size,
            interrupt_line: None,
        }
    }

    /// Wire the function's `INTx` sink (the platform routes it through the
    /// live PIRQ routing, like every PCI interrupt).
    pub fn set_interrupt_line(&mut self, line: impl Fn(bool) + 'static) {
        self.interrupt_line = Some(Box::new(line));
    }

    fn sync_interrupt_line(&self) {
        if let Some(line) = &self.interrupt_line {
            line(self.controller.borrow().intx_level());
        }
    }
}

impl crate::bus::MmioDevice for XhciMmio {
    fn mmio_read(&mut self, offset: u64, size: u8) -> u64 {
        let reg = u32::try_from(offset & !0x3).unwrap_or(u32::MAX);
        let ctrl = self.controller.borrow();
        if size >= 8 {
            u64::from(ctrl.read_register(reg)) | (u64::from(ctrl.read_register(reg + 4)) << 32)
        } else {
            u64::from(ctrl.read_register(reg))
        }
    }

    fn mmio_write(&mut self, offset: u64, size: u8, data: u64) {
        let reg = u32::try_from(offset & !0x3).unwrap_or(u32::MAX);
        {
            let mut ctrl = self.controller.borrow_mut();
            ctrl.write_register(reg, u32_of(data));
            if size >= 8 {
                ctrl.write_register(reg + 4, u32_of(data >> 32));
            }
        }
        // The write may have posted events (doorbell) or cleared IP (IMAN).
        self.sync_interrupt_line();
    }

    fn mmio_range(&self) -> (u64, u64) {
        (self.base, self.base + self.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A controller brought to the running state with 8 slots enabled, the
    /// way a driver does it (CONFIG, then USBCMD.R/S through the register
    /// window).
    fn running_controller() -> VirtualXhciController {
        let mut c = VirtualXhciController::new(4);
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x38, 8); // CONFIG: MaxSlotsEn = 8
        c.write_register(op, 1); // USBCMD: R/S
        assert!(!c.op.is_halted());
        c
    }

    #[test]
    fn register_window_dispatches_to_the_advertised_blocks() {
        let c = VirtualXhciController::new(4);
        // CAPLENGTH | HCIVERSION at offset 0.
        let r0 = c.read_register(0);
        assert_eq!(r0 & 0xFF, 0x20);
        assert_eq!(r0 >> 16, 0x0110);
        // USBSTS at op + 0x04 reads HCHalted at reset.
        assert_eq!(c.read_register(0x20 + 0x04) & 1, 1);
        // MFINDEX at RTSOFF; doorbells are write-only zeros.
        assert_eq!(c.read_register(c.caps.rtsoff), 0);
        assert_eq!(c.read_register(c.caps.dboff), 0);
    }

    #[test]
    fn command_ring_is_only_processed_while_running() {
        let mut c = VirtualXhciController::new(4);
        c.write_register(u32::from(c.caps.caplength) + 0x38, 8);
        assert!(c.submit_command(&CommandTrb::NoOp));
        // Halted: the doorbell does nothing.
        c.write_register(c.caps.dboff, 0);
        assert!(c.pop_event().is_none());
        // Running: the same doorbell drains the ring.
        c.write_register(u32::from(c.caps.caplength), 1);
        c.write_register(c.caps.dboff, 0);
        match c.pop_event() {
            Some(EventTrb::CommandCompletion {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8),
            other => panic!("expected a command completion, got {other:?}"),
        }
    }

    #[test]
    fn enable_slot_allocates_and_disable_frees() {
        let mut c = running_controller();
        c.submit_command(&CommandTrb::EnableSlot);
        c.write_register(c.caps.dboff, 0);
        let slot = match c.pop_event() {
            Some(EventTrb::CommandCompletion {
                completion_code,
                slot_id,
                ..
            }) => {
                assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8);
                slot_id
            }
            other => panic!("expected a command completion, got {other:?}"),
        };
        assert_eq!(slot, 1);
        assert!(c.slot_enabled(1));

        c.submit_command(&CommandTrb::DisableSlot { slot_id: slot });
        c.write_register(c.caps.dboff, 0);
        let _ = c.pop_event();
        assert!(!c.slot_enabled(1));

        // Disabling it again is a TRB error.
        c.submit_command(&CommandTrb::DisableSlot { slot_id: slot });
        c.write_register(c.caps.dboff, 0);
        match c.pop_event() {
            Some(EventTrb::CommandCompletion {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::TrbError as u8),
            other => panic!("expected a command completion, got {other:?}"),
        }
    }

    #[test]
    fn slot_pool_exhaustion_reports_no_slots_available() {
        let mut c = running_controller(); // MaxSlotsEn = 8
        for _ in 0..8 {
            c.submit_command(&CommandTrb::EnableSlot);
        }
        c.submit_command(&CommandTrb::EnableSlot); // the ninth
        c.write_register(c.caps.dboff, 0);
        let mut codes = Vec::new();
        while let Some(EventTrb::CommandCompletion {
            completion_code, ..
        }) = c.pop_event()
        {
            codes.push(completion_code as u8);
        }
        assert_eq!(codes.len(), 9);
        assert!(
            codes[..8]
                .iter()
                .all(|&code| code == TrbCompletionCode::Success as u8)
        );
        assert_eq!(codes[8], TrbCompletionCode::NoSlotsAvailableError as u8);
    }

    #[test]
    fn connect_posts_a_port_status_change_and_raises_the_interrupt_surfaces() {
        let mut c = running_controller();
        // Driver enables interrupter 0 (IE) through the runtime window.
        c.write_register(c.caps.rtsoff + 0x20, 2);

        assert!(c.connect_device(2, 4)); // SuperSpeed on port index 2
        // PORTSC: connected, CSC latched (op + 0x400 + 2*16).
        let portsc = c.read_register(0x20 + 0x400 + 32);
        assert_eq!(portsc & 1, 1, "CCS");
        assert_ne!(portsc & (1 << 17), 0, "CSC");
        // USBSTS: PCD + EINT; interrupter: IP.
        let usbsts = c.read_register(0x20 + 0x04);
        assert_ne!(usbsts & USBSTS_PCD, 0);
        assert_ne!(usbsts & USBSTS_EINT, 0);
        assert!(c.interrupter.interrupt_pending());
        // The event carries the 1-based port ID.
        match c.pop_event() {
            Some(EventTrb::PortStatusChange { port_id }) => assert_eq!(port_id, 3),
            other => panic!("expected a port status change, got {other:?}"),
        }
    }

    #[test]
    fn attach_fills_the_lowest_free_port_and_maps_speed() {
        let mut c = running_controller(); // 4 ports
        c.write_register(c.caps.rtsoff + 0x20, 2); // IE

        // A keyboard (low speed) takes port 0 with PORTSC speed id 2.
        assert_eq!(c.attach_device(UsbSpeed::Low), Some(0));
        let portsc0 = c.read_register(0x20 + 0x400);
        assert_eq!(
            (portsc0 >> 10) & 0xF,
            u32::from(UsbSpeed::Low.xhci_speed_id())
        );
        let _ = c.pop_event();

        // A SuperSpeed drive takes the next free port (1) at speed id 4.
        assert_eq!(c.attach_device(UsbSpeed::Super), Some(1));
        let portsc1 = c.read_register(0x20 + 0x400 + 16);
        assert_eq!(
            (portsc1 >> 10) & 0xF,
            u32::from(UsbSpeed::Super.xhci_speed_id())
        );

        // Fill the remaining two, then the fifth attach finds no free port.
        assert_eq!(c.attach_device(UsbSpeed::High), Some(2));
        assert_eq!(c.attach_device(UsbSpeed::Full), Some(3));
        assert_eq!(c.attach_device(UsbSpeed::High), None);

        // Detaching frees the port for reuse (lowest-free again).
        assert!(c.disconnect_device(1));
        assert_eq!(c.attach_device(UsbSpeed::High), Some(1));
    }

    #[test]
    fn address_device_requires_an_enabled_slot() {
        let mut c = running_controller();
        // On a never-enabled slot: TRB error.
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 5,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        match c.pop_event() {
            Some(EventTrb::CommandCompletion {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::TrbError as u8),
            other => panic!("expected a command completion, got {other:?}"),
        }
        // After Enable Slot: success (context handling waits on guest memory).
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        let _ = c.pop_event(); // the Enable Slot completion
        match c.pop_event() {
            Some(EventTrb::CommandCompletion {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8),
            other => panic!("expected a command completion, got {other:?}"),
        }
    }
}
