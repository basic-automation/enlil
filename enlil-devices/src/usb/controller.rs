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

use std::collections::BTreeMap;

use super::emulated::{UsbDeviceModel, UsbTransferResult};
use super::types::DeviceSpeed as UsbSpeed;
use super::xhci::context::{
    EndpointContext, EpState, InputControlContext, MAX_DCI, SlotContext, SlotState,
    device_context_entry_offset, device_context_pointer, input_context_entry_offset,
    read_context_entry,
};
use super::xhci::registers::XECP_OFFSET;
use super::xhci::ring::GuestRingCursor;
use super::xhci::transfer::{
    CONTROL_DCI, DmaMemory, SetupPacket, TransferTrb, dci_endpoint_number, dci_is_in,
    gather_transfer_td,
};
use super::xhci::{
    CapabilityRegisters, CommandRing, CommandTrb, DoorbellArray, DoorbellTarget, EventRing,
    EventTrb, InterrupterRegisterSet, OperationalRegisters, RuntimeRegisters, TransferRing, Trb,
    TrbCompletionCode,
};
use crate::truncate::{Widen, u32_of};

/// `USBSTS` bit 3: Event Interrupt (EINT) — set when an event is posted.
const USBSTS_EINT: u32 = 1 << 3;
/// `USBSTS` bit 4: Port Change Detect (PCD).
const USBSTS_PCD: u32 = 1 << 4;
/// Commands executed per doorbell-0 service at most, bounding a guest that
/// authors a self-linking command ring of forever-valid TRBs.
const COMMAND_BURST_LIMIT: usize = 256;
/// Transfer Descriptors processed per endpoint doorbell at most, bounding a
/// guest that authors a self-linking transfer ring (the transfer-ring analogue
/// of [`COMMAND_BURST_LIMIT`]).
const TRANSFER_BURST_LIMIT: usize = 256;

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
    /// Consumer cursor for a guest-memory command ring — established from
    /// `CRCR` on the first doorbell 0 after the driver programs it, and
    /// re-established whenever the driver rewrites `CRCR`.
    guest_command_ring: Option<GuestRingCursor>,
    /// Device-slot allocation state; index = slot ID - 1.
    slots: Vec<bool>,
    /// Per-(slot ID, DCI) transfer rings. Created on first
    /// [`submit_transfer`](Self::submit_transfer) — input-context-driven ring
    /// setup waits on guest-memory device contexts.
    transfer_rings: BTreeMap<(u8, u8), TransferRing>,
    /// The endpoint contexts Configure Endpoint (and, for EP0, Address
    /// Device) installed per (slot ID, DCI) — the EP types, max packet
    /// sizes, and guest-memory ring addresses the driver declared.
    endpoint_configs: BTreeMap<(u8, u8), EndpointContext>,
    /// The device model bound to each slot — where TDs terminate.
    device_models: BTreeMap<u8, Box<dyn UsbDeviceModel>>,
    /// EP0 control-transfer stage tracking per slot (Setup/Data/Status
    /// arrive as separate TDs, xHCI §4.11.2.2).
    control_state: BTreeMap<u8, ControlStage>,
    /// Device models parked at root-hub ports (index = 0-based port) until
    /// Address Device binds them to a slot.
    port_models: Vec<Option<Box<dyn UsbDeviceModel>>>,
    /// The port each slot's model was bound from (the model parks there
    /// again on Disable Slot, so the guest can re-enumerate it).
    slot_ports: BTreeMap<u8, usize>,
    /// Opt-in: when set, an endpoint with no enqueued internal TRBs has its
    /// transfer ring fetched from guest memory at the endpoint context's TR
    /// Dequeue Pointer (the real hardware path). Off by default so the legacy
    /// [`submit_transfer`](Self::submit_transfer) modelling path — and every
    /// test that uses it — is unaffected. The KVM run loop enables it.
    guest_resident_transfers: bool,
    /// Per-(slot ID, DCI) consumer cursor for a guest-memory transfer ring,
    /// established from the endpoint context's TR Dequeue Pointer and
    /// re-established whenever that pointer changes (Configure Endpoint, Set TR
    /// Dequeue Pointer) or the endpoint is torn down (Reset/Disable).
    guest_transfer_rings: BTreeMap<(u8, u8), GuestRingCursor>,
}

/// Where slot's EP0 is within the Setup → Data → Status sequence.
#[derive(Debug)]
enum ControlStage {
    /// A Setup Stage TD has been processed; its packet is pending.
    SetupDone {
        /// The SETUP packet awaiting its data/status stages.
        setup: SetupPacket,
    },
    /// The Data Stage TD has been processed.
    DataDone {
        /// The SETUP packet that opened the transfer.
        setup: SetupPacket,
        /// OUT-data gathered from the guest (empty for IN transfers).
        out_data: Vec<u8>,
        /// Whether the device model already executed the request (IN
        /// transfers respond at the data stage; OUT waits for status).
        responded: bool,
    },
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
            guest_command_ring: None,
            slots,
            caps,
            transfer_rings: BTreeMap::new(),
            endpoint_configs: BTreeMap::new(),
            device_models: BTreeMap::new(),
            control_state: BTreeMap::new(),
            port_models: (0..num_ports).map(|_| None).collect(),
            slot_ports: BTreeMap::new(),
            guest_resident_transfers: false,
            guest_transfer_rings: BTreeMap::new(),
        }
    }

    /// Enable (or disable) guest-resident transfer rings: when enabled, an
    /// endpoint with no enqueued internal TRBs is driven from its guest-memory
    /// ring at the endpoint context's TR Dequeue Pointer (the real hardware
    /// path). Off by default so the legacy [`submit_transfer`](Self::submit_transfer)
    /// path is unchanged; the KVM run loop turns it on.
    pub const fn set_guest_resident_transfers(&mut self, enabled: bool) {
        self.guest_resident_transfers = enabled;
    }

    /// Read a 32-bit register at `offset` within the controller's MMIO window.
    #[must_use]
    pub fn read_register(&self, offset: u32) -> u32 {
        let op_base = u32::from(self.caps.caplength);
        match offset {
            o if o < op_base => self.caps.read(o),
            o if o < self.caps.rtsoff => self.op.read(o - op_base),
            o if o < self.caps.dboff => self.read_runtime(o - self.caps.rtsoff),
            // The Extended Capabilities region (Supported Protocol caps) that
            // HCCPARAMS1's xECP points at.
            o if o >= XECP_OFFSET => self.caps.read_extended(o),
            // Doorbells are write-only; reads return zero (xHCI §5.6).
            _ => 0,
        }
    }

    /// Write a 32-bit register at `offset` within the controller's MMIO
    /// window. Doorbell writes latch pending until
    /// [`service_doorbells`](Self::service_doorbells).
    pub fn write_register(&mut self, offset: u32, value: u32) {
        let op_base = u32::from(self.caps.caplength);
        match offset {
            // Capability registers are read-only.
            o if o < op_base => {}
            o if o < self.caps.rtsoff => self.write_operational(o - op_base, value),
            o if o < self.caps.dboff => self.write_runtime(o - self.caps.rtsoff, value),
            o => {
                let index = u8::try_from((o - self.caps.dboff) / 4).unwrap_or(u8::MAX);
                // Every doorbell latches pending — including doorbell 0:
                // command processing happens in `service_doorbells`, where
                // guest memory (for Address Device's input context) is in
                // hand, matching the run loop's doorbell-write-exit →
                // service shape.
                let _ = self.doorbells.write(index, value);
            }
        }
    }

    fn write_operational(&mut self, offset: u32, value: u32) {
        match offset {
            0x00 => self.op.write_usbcmd(value),
            0x04 => self.op.write_usbsts(value),
            0x14 => self.op.dnctrl = value,
            // Rewriting CRCR re-establishes the guest ring's consumer
            // cursor at the new pointer/RCS on the next doorbell 0.
            0x18 => {
                self.op.crcr = (self.op.crcr & !0xFFFF_FFFF) | u64::from(value);
                self.guest_command_ring = None;
            }
            0x1C => {
                self.op.crcr = (self.op.crcr & 0xFFFF_FFFF) | (u64::from(value) << 32);
                self.guest_command_ring = None;
            }
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
    /// `DBOFF` window — and call
    /// [`service_doorbells`](Self::service_doorbells) to have it processed.
    pub fn submit_command(&mut self, command: &CommandTrb) -> bool {
        self.command_ring.submit(command.to_trb(true))
    }

    /// Drain the command ring (doorbell 0, xHCI §4.6): execute each command
    /// against guest memory and post its Command Completion event. A halted
    /// controller leaves the ring untouched, as real hardware does.
    /// A driver-programmed `CRCR` means the ring lives in guest memory and
    /// commands are fetched there (cycle-bit delimited, Link TRBs followed);
    /// the internal ring remains the modelling path while `CRCR` is zero.
    fn process_command_ring(&mut self, mem: &mut dyn DmaMemory) {
        if !self.op.is_running() {
            return;
        }
        let pointer = self.op.crcr & !0x3F;
        if pointer == 0 {
            while let Some(trb) = self.command_ring.fetch() {
                self.execute_command(&trb, trb.parameter, mem);
            }
            return;
        }
        let mut cursor = self
            .guest_command_ring
            .take()
            .unwrap_or_else(|| GuestRingCursor::new(pointer, self.op.crcr & 1 != 0));
        // Bounded per doorbell so a guest authoring a self-linking ring of
        // forever-valid TRBs cannot wedge the controller.
        for _ in 0..COMMAND_BURST_LIMIT {
            let Some((address, trb)) = cursor.fetch(&*mem) else {
                break;
            };
            self.execute_command(&trb, address, mem);
        }
        self.guest_command_ring = Some(cursor);
    }

    /// Decode and execute one command TRB, posting its Command Completion
    /// event reporting `trb_pointer` as the command-TRB address (the guest
    /// ring's real fetch address; the parameter field on the internal
    /// modelling path).
    fn execute_command(&mut self, trb: &Trb, trb_pointer: u64, mem: &mut dyn DmaMemory) {
        let (code, slot_id) = match CommandTrb::from_trb(trb) {
            Some(CommandTrb::NoOp) => (TrbCompletionCode::Success, 0),
            Some(CommandTrb::EnableSlot) => self.enable_slot(),
            Some(CommandTrb::DisableSlot { slot_id }) => (self.disable_slot(slot_id, mem), slot_id),
            // Address Device reads the input context out of guest memory
            // and binds the named port's parked device model to the slot.
            // BSR (Block Set Address Request, control bit 9, xHCI §4.6.5):
            // set up the slot context only, leaving the device in the Default
            // state at address 0 — Linux issues this before the real pass.
            Some(CommandTrb::AddressDevice {
                slot_id,
                input_context_ptr,
            }) => {
                let bsr = trb.control & (1 << 9) != 0;
                (
                    self.address_device(slot_id, input_context_ptr, bsr, mem),
                    slot_id,
                )
            }
            // Configure Endpoint parses the input context's endpoint
            // contexts and installs (or drops) the slot's transfer rings.
            Some(CommandTrb::ConfigureEndpoint {
                slot_id,
                input_context_ptr,
                deconfigure,
            }) => (
                self.configure_endpoint(slot_id, input_context_ptr, deconfigure, mem),
                slot_id,
            ),
            // Evaluate Context re-reads the input context's evaluable fields
            // (EP0 Max Packet Size) without changing slot/endpoint state.
            Some(CommandTrb::EvaluateContext {
                slot_id,
                input_context_ptr,
            }) => (
                self.evaluate_context(slot_id, input_context_ptr, mem),
                slot_id,
            ),
            // Stop Endpoint pauses the ring and transitions the endpoint to
            // the Stopped state the driver reads back before repointing it.
            Some(CommandTrb::StopEndpoint {
                slot_id,
                endpoint_id,
            }) => (self.stop_endpoint(slot_id, endpoint_id, mem), slot_id),
            // Reset Endpoint recovers a halted endpoint (xHCI §4.6.8):
            // clear the halt so the ring processes TDs again.
            Some(CommandTrb::ResetEndpoint {
                slot_id,
                endpoint_id,
            }) => {
                let code = self.slot_dependent_success(slot_id);
                if code == TrbCompletionCode::Success
                    && let Some(ring) = self.transfer_rings.get_mut(&(slot_id, endpoint_id))
                {
                    ring.clear_halt();
                    // The endpoint runs again — reflect Running in the output
                    // context so the driver sees the recovery.
                    self.publish_ep_state(slot_id, endpoint_id, EpState::Running, mem);
                }
                (code, slot_id)
            }
            // Set TR Dequeue Pointer repoints a stopped/halted endpoint's
            // transfer ring (xHCI §4.6.10) — the second half of STALL
            // recovery after Reset Endpoint.
            Some(CommandTrb::SetTrDequeuePointer {
                slot_id,
                endpoint_id,
                dequeue_ptr,
                dcs,
            }) => (
                self.set_tr_dequeue_pointer(slot_id, endpoint_id, dequeue_ptr, dcs),
                slot_id,
            ),
            // Reset Device returns the slot to the Default state for
            // re-enumeration (xHCI §4.6.11).
            Some(CommandTrb::ResetDevice { slot_id }) => (self.reset_device(slot_id, mem), slot_id),
            // An undecodable TRB on the command ring is a TRB error.
            None => (TrbCompletionCode::TrbError, 0),
        };
        self.post_event_trb(|ring| ring.post_command_completion(trb_pointer, code, slot_id));
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

    /// Free a slot; disabling a never-enabled slot is a TRB error. A freed
    /// slot's transfer rings and control state go with it; a model the slot
    /// bound from a port parks there again (the device is still physically
    /// connected, so the guest can re-enumerate it), while an explicitly
    /// bound model is dropped.
    fn disable_slot(&mut self, slot_id: u8, mem: &mut dyn DmaMemory) -> TrbCompletionCode {
        match self.slots.get_mut(usize::from(slot_id.wrapping_sub(1))) {
            Some(used) if *used => {
                *used = false;
                // EP0 plus any configured endpoints, to zero in the output
                // device context the driver reads back.
                let mut dcis = vec![CONTROL_DCI];
                dcis.extend(self.slot_endpoint_dcis(slot_id));
                self.transfer_rings.retain(|(slot, _), _| *slot != slot_id);
                self.endpoint_configs
                    .retain(|(slot, _), _| *slot != slot_id);
                self.guest_transfer_rings
                    .retain(|(slot, _), _| *slot != slot_id);
                let model = self.device_models.remove(&slot_id);
                if let Some(port) = self.slot_ports.remove(&slot_id)
                    && let Some(parked) = self.port_models.get_mut(port)
                {
                    *parked = model;
                }
                self.control_state.remove(&slot_id);
                self.publish_disabled_context(slot_id, &dcis, mem);
                TrbCompletionCode::Success
            }
            _ => TrbCompletionCode::TrbError,
        }
    }

    /// Publish the output device context after Disable Slot (xHCI §4.6.4):
    /// the slot context returns to the Disabled state with no valid endpoint
    /// contexts, and every endpoint the slot held is zeroed. A no-op without
    /// `DCBAAP`/a backed DCBAA entry.
    fn publish_disabled_context(&self, slot_id: u8, dcis: &[u8], mem: &mut dyn DmaMemory) {
        if self.op.dcbaap == 0 {
            return;
        }
        let Some(out_ctx) = device_context_pointer(mem, self.op.dcbaap, slot_id) else {
            return;
        };
        if let Some(slot_bytes) = read_context_entry(mem, out_ctx + device_context_entry_offset(0))
        {
            let mut slot = SlotContext::parse(&slot_bytes);
            slot.slot_state = SlotState::DisabledEnabled as u8;
            slot.context_entries = 0;
            slot.usb_device_address = 0;
            let _ = mem.write(out_ctx + device_context_entry_offset(0), &slot.to_bytes());
        }
        for &dci in dcis {
            let _ = mem.write(out_ctx + device_context_entry_offset(dci), &[0_u8; 32]);
        }
    }

    /// Address Device (xHCI §4.6.5): validate the input context in guest
    /// memory and bind the device model parked at the slot context's
    /// root-hub port to the slot. A port with no parked model (pure
    /// port-status modelling) still addresses successfully.
    fn address_device(
        &mut self,
        slot_id: u8,
        input_context_ptr: u64,
        bsr: bool,
        mem: &mut dyn DmaMemory,
    ) -> TrbCompletionCode {
        if self.slot_dependent_success(slot_id) != TrbCompletionCode::Success {
            return TrbCompletionCode::TrbError;
        }
        // Input control context (xHCI §6.2.5.1): dword 0 = drop flags,
        // dword 1 = add flags. Address Device must add exactly the slot
        // and EP0 contexts (A0 | A1).
        let mut control = [0_u8; 8];
        if !mem.read(input_context_ptr, &mut control) {
            return TrbCompletionCode::TrbError;
        }
        let add_flags = u32::from_le_bytes([control[4], control[5], control[6], control[7]]);
        if add_flags & 0x3 != 0x3 {
            return TrbCompletionCode::ParameterError;
        }
        // Slot context dword 1 (32-byte contexts: input context + 0x24),
        // bits 23:16 = root-hub port number, 1-based (xHCI §6.2.2).
        let mut dword1 = [0_u8; 4];
        if !mem.read(input_context_ptr + 0x24, &mut dword1) {
            return TrbCompletionCode::TrbError;
        }
        let Some(port) = usize::from(dword1[2]).checked_sub(1) else {
            return TrbCompletionCode::ParameterError;
        };
        if port >= self.port_models.len() {
            return TrbCompletionCode::ParameterError;
        }
        if let Some(model) = self.port_models[port].take() {
            self.device_models.insert(slot_id, model);
            self.slot_ports.insert(slot_id, port);
        }
        // Record EP0's declared context (A1 carries it) so its max packet
        // size and ring address are known; a zeroed EP0 context (pure
        // port-status modelling) is tolerated.
        if let Some(bytes) = read_context_entry(
            mem,
            input_context_ptr + input_context_entry_offset(CONTROL_DCI),
        ) && let Some(context) = EndpointContext::parse(&bytes)
        {
            self.endpoint_configs
                .insert((slot_id, CONTROL_DCI), context);
        }
        // Publish the Output Device Context (xHCI §4.6.5): the xHC copies the
        // input slot/EP0 contexts into the device context the DCBAA names for
        // this slot, with Slot State → Addressed and the assigned USB Device
        // Address — the values the driver reads back to confirm the device is
        // addressed. Best-effort: skipped (the pre-run-loop modelling path)
        // when the driver has not programmed DCBAAP or its array is unbacked.
        self.publish_addressed_context(slot_id, input_context_ptr, bsr, mem);
        TrbCompletionCode::Success
    }

    /// Write the output device context for a freshly-addressed slot. Without
    /// BSR the slot context reaches the Addressed state with USB Device
    /// Address = `slot_id` (a deterministic, valid per-slot address); with BSR
    /// (Block Set Address Request) it reaches the Default state at address 0,
    /// the slot-context-only setup Linux does before the real addressing pass.
    /// The EP0 output context (when the driver supplied a valid one) is
    /// published with EP State = Running. A no-op when `DCBAAP`/the slot's
    /// DCBAA entry is not backed.
    fn publish_addressed_context(
        &self,
        slot_id: u8,
        input_context_ptr: u64,
        bsr: bool,
        mem: &mut dyn DmaMemory,
    ) {
        if self.op.dcbaap == 0 {
            return;
        }
        let Some(out_ctx) = device_context_pointer(mem, self.op.dcbaap, slot_id) else {
            return;
        };
        let Some(slot_bytes) =
            read_context_entry(mem, input_context_ptr + input_context_entry_offset(0))
        else {
            return;
        };
        let mut slot = SlotContext::parse(&slot_bytes);
        if bsr {
            slot.slot_state = SlotState::Default as u8;
            slot.usb_device_address = 0;
        } else {
            slot.slot_state = SlotState::Addressed as u8;
            slot.usb_device_address = slot_id;
        }
        // EP0 is valid once addressed, so Context Entries is at least 1.
        slot.context_entries = slot.context_entries.max(1);
        let _ = mem.write(out_ctx + device_context_entry_offset(0), &slot.to_bytes());
        // EP0 output context: the driver's declared EP0, EP State = Running
        // (dword 0, bits 2:0 = 1).
        if let Some(ep0) = self.endpoint_configs.get(&(slot_id, CONTROL_DCI)) {
            let mut bytes = ep0.to_bytes();
            bytes[0] = (bytes[0] & !0x7) | 1;
            let _ = mem.write(out_ctx + device_context_entry_offset(CONTROL_DCI), &bytes);
        }
    }

    /// Configure Endpoint (xHCI §4.6.6): parse the input context and install
    /// a transfer ring — at the declared TR Dequeue Pointer — for every
    /// added endpoint context, dropping the endpoints the drop flags name.
    /// The driver-side contract is A0 = 1 with A1 = D0 = D1 = 0 (the slot
    /// context comes along; EP0 and the slot itself are not configurable
    /// here), and every added context must carry a type valid for its DCI.
    /// An invalid context leaves the slot's endpoints untouched.
    fn configure_endpoint(
        &mut self,
        slot_id: u8,
        input_context_ptr: u64,
        deconfigure: bool,
        mem: &mut dyn DmaMemory,
    ) -> TrbCompletionCode {
        if self.slot_dependent_success(slot_id) != TrbCompletionCode::Success {
            return TrbCompletionCode::TrbError;
        }
        // Deconfigure (DC): drop every endpoint but EP0; the input context
        // pointer is not referenced (xHCI §6.4.3.5).
        if deconfigure {
            let dropped = self.slot_endpoint_dcis(slot_id);
            self.transfer_rings
                .retain(|&(slot, dci), _| slot != slot_id || dci <= CONTROL_DCI);
            self.endpoint_configs
                .retain(|&(slot, dci), _| slot != slot_id || dci <= CONTROL_DCI);
            self.publish_deconfigured_context(slot_id, &dropped, mem);
            return TrbCompletionCode::Success;
        }
        let Some(control) = InputControlContext::read(mem, input_context_ptr) else {
            return TrbCompletionCode::TrbError;
        };
        if control.drops(0) || control.drops(CONTROL_DCI) || control.adds(CONTROL_DCI) {
            return TrbCompletionCode::ParameterError;
        }
        // Parse every added endpoint context before touching any state, so
        // a bad context fails the whole command without partial effects.
        let mut added = Vec::new();
        for dci in (CONTROL_DCI + 1)..=MAX_DCI {
            if !control.adds(dci) {
                continue;
            }
            let entry = input_context_ptr + input_context_entry_offset(dci);
            let Some(bytes) = read_context_entry(mem, entry) else {
                return TrbCompletionCode::TrbError;
            };
            let Some(context) = EndpointContext::parse(&bytes) else {
                return TrbCompletionCode::ParameterError;
            };
            if !context.endpoint_type.valid_for_dci(dci) {
                return TrbCompletionCode::ParameterError;
            }
            added.push((dci, context));
        }
        let mut dropped = Vec::new();
        for dci in (CONTROL_DCI + 1)..=MAX_DCI {
            if control.drops(dci) {
                self.transfer_rings.remove(&(slot_id, dci));
                self.endpoint_configs.remove(&(slot_id, dci));
                self.guest_transfer_rings.remove(&(slot_id, dci));
                dropped.push(dci);
            }
        }
        let added_dcis: Vec<u8> = added.iter().map(|(dci, _)| *dci).collect();
        for (dci, context) in added {
            let mut ring = TransferRing::with_default_size(slot_id, dci);
            ring.ring_mut().set_base_addr(context.tr_dequeue_pointer);
            self.transfer_rings.insert((slot_id, dci), ring);
            self.endpoint_configs.insert((slot_id, dci), context);
            // A re-added endpoint may declare a new TR Dequeue Pointer; drop any
            // stale guest cursor so it rebuilds from the new context.
            self.guest_transfer_rings.remove(&(slot_id, dci));
        }
        self.publish_configured_context(slot_id, input_context_ptr, &added_dcis, &dropped, mem);
        TrbCompletionCode::Success
    }

    /// The DCIs of a slot's configured endpoints beyond EP0 (DCI > 1).
    fn slot_endpoint_dcis(&self, slot_id: u8) -> Vec<u8> {
        self.endpoint_configs
            .keys()
            .filter(|&&(slot, dci)| slot == slot_id && dci > CONTROL_DCI)
            .map(|&(_, dci)| dci)
            .collect()
    }

    /// The highest DCI with a configured endpoint context for a slot (0 if
    /// none) — the Context Entries field the output slot context advertises.
    fn highest_configured_dci(&self, slot_id: u8) -> u8 {
        self.endpoint_configs
            .keys()
            .filter(|&&(slot, _)| slot == slot_id)
            .map(|&(_, dci)| dci)
            .max()
            .unwrap_or(0)
    }

    /// Publish the output device context after a successful Configure Endpoint
    /// (xHCI §4.6.6): the slot context with Slot State = Configured and
    /// Context Entries = the highest configured DCI, each added endpoint's
    /// output context with EP State = Running, and each dropped endpoint's
    /// output context zeroed (EP State = Disabled). A no-op when `DCBAAP`/the
    /// slot's DCBAA entry is not backed.
    fn publish_configured_context(
        &self,
        slot_id: u8,
        input_context_ptr: u64,
        added: &[u8],
        dropped: &[u8],
        mem: &mut dyn DmaMemory,
    ) {
        if self.op.dcbaap == 0 {
            return;
        }
        let Some(out_ctx) = device_context_pointer(mem, self.op.dcbaap, slot_id) else {
            return;
        };
        if let Some(slot_bytes) =
            read_context_entry(mem, input_context_ptr + input_context_entry_offset(0))
        {
            let mut slot = SlotContext::parse(&slot_bytes);
            slot.slot_state = SlotState::Configured as u8;
            slot.context_entries = self.highest_configured_dci(slot_id).max(1);
            let _ = mem.write(out_ctx + device_context_entry_offset(0), &slot.to_bytes());
        }
        for &dci in added {
            if let Some(ctx) = self.endpoint_configs.get(&(slot_id, dci)) {
                let mut bytes = ctx.to_bytes();
                bytes[0] = (bytes[0] & !0x7) | 1; // EP State = Running
                let _ = mem.write(out_ctx + device_context_entry_offset(dci), &bytes);
            }
        }
        for &dci in dropped {
            let _ = mem.write(out_ctx + device_context_entry_offset(dci), &[0_u8; 32]);
        }
    }

    /// Evaluate Context (xHCI §4.6.7): re-evaluate the input context without
    /// changing slot or endpoint *state*. The only field this model carries
    /// that a driver evaluates is EP0's Max Packet Size (updated once the
    /// driver reads the device descriptor and learns the real value); the
    /// slot context's evaluable fields (Max Exit Latency, Interrupter Target)
    /// are not modelled (single interrupter), so A0 is accepted as a no-op.
    /// Only A0/A1 are valid adds for Evaluate Context.
    fn evaluate_context(
        &mut self,
        slot_id: u8,
        input_context_ptr: u64,
        mem: &mut dyn DmaMemory,
    ) -> TrbCompletionCode {
        if self.slot_dependent_success(slot_id) != TrbCompletionCode::Success {
            return TrbCompletionCode::TrbError;
        }
        let Some(control) = InputControlContext::read(mem, input_context_ptr) else {
            return TrbCompletionCode::TrbError;
        };
        if !control.adds(CONTROL_DCI) {
            // Nothing this model evaluates was requested (only A0): succeed.
            return TrbCompletionCode::Success;
        }
        let entry = input_context_ptr + input_context_entry_offset(CONTROL_DCI);
        let Some(bytes) = read_context_entry(mem, entry) else {
            return TrbCompletionCode::TrbError;
        };
        let Some(new_ep0) = EndpointContext::parse(&bytes) else {
            return TrbCompletionCode::ParameterError;
        };
        // Update only EP0's Max Packet Size, keeping its ring and the rest of
        // its declared context intact.
        if let Some(ep0) = self.endpoint_configs.get_mut(&(slot_id, CONTROL_DCI)) {
            ep0.max_packet_size = new_ep0.max_packet_size;
        }
        // Reflect the updated EP0 into the output context, preserving its
        // Running state.
        if self.op.dcbaap != 0
            && let Some(out_ctx) = device_context_pointer(mem, self.op.dcbaap, slot_id)
            && let Some(ep0) = self.endpoint_configs.get(&(slot_id, CONTROL_DCI))
        {
            let mut out = ep0.to_bytes();
            out[0] = (out[0] & !0x7) | 1; // EP State = Running
            let _ = mem.write(out_ctx + device_context_entry_offset(CONTROL_DCI), &out);
        }
        TrbCompletionCode::Success
    }

    /// Stop Endpoint (xHCI §4.6.9): pause the endpoint's transfer ring and
    /// transition it to the Stopped state in the output device context — the
    /// state the driver requires before it issues Set TR Dequeue Pointer.
    fn stop_endpoint(
        &mut self,
        slot_id: u8,
        endpoint_id: u8,
        mem: &mut dyn DmaMemory,
    ) -> TrbCompletionCode {
        let code = self.slot_dependent_success(slot_id);
        if code != TrbCompletionCode::Success {
            return code;
        }
        if let Some(ring) = self.transfer_rings.get_mut(&(slot_id, endpoint_id)) {
            ring.ring_mut().stop();
        }
        self.publish_ep_state(slot_id, endpoint_id, EpState::Stopped, mem);
        code
    }

    /// Patch one endpoint's EP State (dword 0, bits 2:0) in the output device
    /// context, preserving the rest of the context. A no-op without `DCBAAP`
    /// or when the output endpoint context is not backed.
    fn publish_ep_state(&self, slot_id: u8, dci: u8, state: EpState, mem: &mut dyn DmaMemory) {
        if self.op.dcbaap == 0 {
            return;
        }
        let Some(out_ctx) = device_context_pointer(mem, self.op.dcbaap, slot_id) else {
            return;
        };
        let entry = out_ctx + device_context_entry_offset(dci);
        if let Some(mut bytes) = read_context_entry(mem, entry) {
            bytes[0] = (bytes[0] & !0x7) | (state as u8 & 0x7);
            let _ = mem.write(entry, &bytes);
        }
    }

    /// Set TR Dequeue Pointer (xHCI §4.6.10): repoint an endpoint's transfer
    /// ring at `dequeue_ptr` with the given Dequeue Cycle State, the second
    /// half of STALL recovery (Reset Endpoint clears the halt; this tells the
    /// controller where to resume). The endpoint must be configured;
    /// otherwise the command is a Context State Error.
    fn set_tr_dequeue_pointer(
        &mut self,
        slot_id: u8,
        endpoint_id: u8,
        dequeue_ptr: u64,
        dcs: bool,
    ) -> TrbCompletionCode {
        if self.slot_dependent_success(slot_id) != TrbCompletionCode::Success {
            return TrbCompletionCode::TrbError;
        }
        let Some(ring) = self.transfer_rings.get_mut(&(slot_id, endpoint_id)) else {
            return TrbCompletionCode::ContextStateError;
        };
        ring.reset();
        let inner = ring.ring_mut();
        inner.set_base_addr(dequeue_ptr);
        inner.set_cycle_state(dcs);
        inner.start();
        // Keep the endpoint context (the guest-resident path's source of truth)
        // in step with the repositioned ring, and drop any live guest cursor so
        // it rebuilds from the new dequeue pointer.
        if let Some(ctx) = self.endpoint_configs.get_mut(&(slot_id, endpoint_id)) {
            ctx.tr_dequeue_pointer = dequeue_ptr & !0xF;
            ctx.dequeue_cycle_state = dcs;
        }
        self.guest_transfer_rings.remove(&(slot_id, endpoint_id));
        TrbCompletionCode::Success
    }

    /// Reset Device (xHCI §4.6.11): a USB bus reset returns the slot to the
    /// Default state — USB address 0, every non-control endpoint disabled and
    /// its ring dropped — leaving EP0 for re-enumeration. The slot stays
    /// allocated and its device model bound.
    fn reset_device(&mut self, slot_id: u8, mem: &mut dyn DmaMemory) -> TrbCompletionCode {
        if self.slot_dependent_success(slot_id) != TrbCompletionCode::Success {
            return TrbCompletionCode::TrbError;
        }
        let dropped = self.slot_endpoint_dcis(slot_id);
        self.transfer_rings
            .retain(|&(slot, dci), _| slot != slot_id || dci <= CONTROL_DCI);
        self.endpoint_configs
            .retain(|&(slot, dci), _| slot != slot_id || dci <= CONTROL_DCI);
        self.guest_transfer_rings
            .retain(|&(slot, dci), _| slot != slot_id || dci <= CONTROL_DCI);
        self.control_state.remove(&slot_id);
        if self.op.dcbaap != 0
            && let Some(out_ctx) = device_context_pointer(mem, self.op.dcbaap, slot_id)
        {
            if let Some(slot_bytes) =
                read_context_entry(mem, out_ctx + device_context_entry_offset(0))
            {
                let mut slot = SlotContext::parse(&slot_bytes);
                slot.slot_state = SlotState::Default as u8;
                slot.usb_device_address = 0;
                slot.context_entries = 1;
                let _ = mem.write(out_ctx + device_context_entry_offset(0), &slot.to_bytes());
            }
            for dci in dropped {
                let _ = mem.write(out_ctx + device_context_entry_offset(dci), &[0_u8; 32]);
            }
        }
        TrbCompletionCode::Success
    }

    /// Publish the output device context after a Deconfigure (DC = 1): the
    /// slot returns to the Addressed state with only EP0 valid, and every
    /// previously-configured endpoint's output context is zeroed (EP State =
    /// Disabled). The current output slot context is read back and patched
    /// (there is no input context on the DC path). A no-op without `DCBAAP`.
    fn publish_deconfigured_context(&self, slot_id: u8, dropped: &[u8], mem: &mut dyn DmaMemory) {
        if self.op.dcbaap == 0 {
            return;
        }
        let Some(out_ctx) = device_context_pointer(mem, self.op.dcbaap, slot_id) else {
            return;
        };
        if let Some(slot_bytes) = read_context_entry(mem, out_ctx + device_context_entry_offset(0))
        {
            let mut slot = SlotContext::parse(&slot_bytes);
            slot.slot_state = SlotState::Addressed as u8;
            slot.context_entries = 1;
            let _ = mem.write(out_ctx + device_context_entry_offset(0), &slot.to_bytes());
        }
        for &dci in dropped {
            let _ = mem.write(out_ctx + device_context_entry_offset(dci), &[0_u8; 32]);
        }
    }

    /// The endpoint context the guest's driver declared for (slot, DCI) —
    /// installed by Configure Endpoint (EP0's by Address Device).
    #[must_use]
    pub fn endpoint_config(&self, slot_id: u8, dci: u8) -> Option<&EndpointContext> {
        self.endpoint_configs.get(&(slot_id, dci))
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

    // -----------------------------------------------------------------------
    // Transfer-ring (TD) processing
    // -----------------------------------------------------------------------

    /// Bind the device model that `slot_id`'s transfer rings terminate
    /// against (an emulated device today, a libusb forwarder for routed
    /// physical devices later). Until Address Device parses input contexts
    /// out of guest memory, the platform binds the model explicitly after
    /// Enable Slot. Fails on a disabled slot.
    pub fn bind_device_model(&mut self, slot_id: u8, model: Box<dyn UsbDeviceModel>) -> bool {
        if !self.slot_enabled(slot_id) {
            return false;
        }
        self.device_models.insert(slot_id, model);
        true
    }

    /// Model the guest enqueueing a transfer TRB on the (slot, DCI) ring
    /// (stands in for the guest's TRB write until rings live in guest
    /// memory). Ring the slot's doorbell and call
    /// [`service_doorbells`](Self::service_doorbells) to have it processed.
    /// Fails on a disabled slot, DCI 0, a full ring, or a halted endpoint.
    pub fn submit_transfer(&mut self, slot_id: u8, dci: u8, trb: &TransferTrb) -> bool {
        if !self.slot_enabled(slot_id) || dci == 0 {
            return false;
        }
        let ring = self
            .transfer_rings
            .entry((slot_id, dci))
            .or_insert_with(|| TransferRing::with_default_size(slot_id, dci));
        ring.enqueue(trb.to_trb(true))
    }

    /// Drain every pending doorbell, processing the rung transfer rings
    /// against guest memory. This is the run loop's entry point after a
    /// doorbell-write exit — device-slot doorbells latch in
    /// [`write_register`](Self::write_register) and are serviced here, where
    /// guest memory is in hand. A halted controller leaves them latched.
    pub fn service_doorbells(&mut self, mem: &mut dyn DmaMemory) {
        if !self.op.is_running() {
            return;
        }
        for target in self.doorbells.drain_pending() {
            match target {
                DoorbellTarget::HostCommand => self.process_command_ring(mem),
                DoorbellTarget::ControlEndpoint { slot_id } => {
                    self.process_transfer_ring(slot_id, CONTROL_DCI, mem);
                }
                DoorbellTarget::Endpoint {
                    slot_id,
                    endpoint_id,
                }
                | DoorbellTarget::Stream {
                    slot_id,
                    endpoint_id,
                    ..
                } => self.process_transfer_ring(slot_id, endpoint_id, mem),
            }
        }
        // Deliver what the servicing produced into the guest's event ring
        // (a no-op until the driver programs ERSTBA).
        let _ = self.interrupter.flush_to_guest(mem);
    }

    /// Deliver internally queued events into the guest-memory event ring
    /// (xHCI §4.9.4), returning how many were written. The run loop calls
    /// this after posting events outside doorbell servicing — hot-plug's
    /// Port Status Change above all; [`service_doorbells`](Self::service_doorbells)
    /// flushes on its own. A no-op until the driver programs `ERSTBA`.
    pub fn flush_events(&mut self, mem: &mut dyn DmaMemory) -> usize {
        self.interrupter.flush_to_guest(mem)
    }

    /// Whether an endpoint is halted (after a STALL, until Reset Endpoint).
    #[must_use]
    pub fn endpoint_halted(&self, slot_id: u8, dci: u8) -> bool {
        self.transfer_rings
            .get(&(slot_id, dci))
            .is_some_and(TransferRing::is_halted)
    }

    /// Drain one rung transfer ring TD by TD (xHCI §4.9.2): gather each TD
    /// by the chain bit, execute it against the slot's device model, and
    /// post its Transfer Event. Stops at a halt (STALL) or an empty ring.
    fn process_transfer_ring(&mut self, slot_id: u8, dci: u8, mem: &mut dyn DmaMemory) {
        if !self.slot_enabled(slot_id) {
            return;
        }
        // A halted endpoint processes nothing until Reset Endpoint clears it
        // (neither the internal nor the guest-resident path).
        if self
            .transfer_rings
            .get(&(slot_id, dci))
            .is_some_and(TransferRing::is_halted)
        {
            return;
        }
        // Internal (submit_transfer) modelling path: drain enqueued TRBs first.
        self.drain_internal_transfer_ring(slot_id, dci, mem);
        // Guest-resident path (opt-in): fetch TRBs from the endpoint's guest
        // ring at its TR Dequeue Pointer — the real hardware path.
        if self.guest_resident_transfers {
            self.process_guest_transfer_ring(slot_id, dci, mem);
        }
    }

    /// Drain the in-process [`TransferRing`] an endpoint accumulated via
    /// [`submit_transfer`](Self::submit_transfer) — the legacy modelling path,
    /// run before the guest-resident path. Stops at a halt, an empty ring, or
    /// a missing ring.
    fn drain_internal_transfer_ring(&mut self, slot_id: u8, dci: u8, mem: &mut dyn DmaMemory) {
        loop {
            let Some(ring) = self.transfer_rings.get_mut(&(slot_id, dci)) else {
                return;
            };
            if ring.is_halted() || ring.ring().is_empty() {
                return;
            }
            match Self::gather_td(ring) {
                // An undecodable TRB on the ring is a TRB error.
                Err(trb_pointer) => {
                    self.post_transfer_event(
                        trb_pointer,
                        0,
                        TrbCompletionCode::TrbError,
                        slot_id,
                        dci,
                    );
                }
                Ok(td) if td.is_empty() => return,
                Ok(td) => {
                    if dci == CONTROL_DCI {
                        self.execute_control_td(slot_id, &td, mem);
                    } else {
                        self.execute_normal_td(slot_id, dci, &td, mem);
                    }
                }
            }
        }
    }

    /// Drain an endpoint's **guest-memory** transfer ring (the
    /// `guest_resident_transfers` path): fetch TDs from the endpoint context's
    /// TR Dequeue Pointer via a persistent [`GuestRingCursor`] and execute
    /// them, exactly as the internal path executes `submit_transfer`'d TDs.
    /// Bounded per doorbell so a self-linking ring can't wedge the controller;
    /// stops at a halt (a STALL'd TD) or when the ring's cycle bit says it is
    /// exhausted.
    fn process_guest_transfer_ring(&mut self, slot_id: u8, dci: u8, mem: &mut dyn DmaMemory) {
        if self
            .transfer_rings
            .get(&(slot_id, dci))
            .is_some_and(TransferRing::is_halted)
        {
            return;
        }
        // The endpoint must have declared a guest-memory ring in its context.
        let (deq, dcs) = match self.endpoint_configs.get(&(slot_id, dci)) {
            Some(ctx) if ctx.tr_dequeue_pointer != 0 => {
                (ctx.tr_dequeue_pointer, ctx.dequeue_cycle_state)
            }
            _ => return,
        };
        let mut cursor = self
            .guest_transfer_rings
            .remove(&(slot_id, dci))
            .unwrap_or_else(|| GuestRingCursor::new(deq, dcs));
        for _ in 0..TRANSFER_BURST_LIMIT {
            match gather_transfer_td(&mut cursor, &*mem) {
                Ok(td) if td.is_empty() => break,
                Ok(td) => {
                    if dci == CONTROL_DCI {
                        self.execute_control_td(slot_id, &td, mem);
                    } else {
                        self.execute_normal_td(slot_id, dci, &td, mem);
                    }
                    // A TD that halted the endpoint (STALL) stops the burst.
                    if self
                        .transfer_rings
                        .get(&(slot_id, dci))
                        .is_some_and(TransferRing::is_halted)
                    {
                        break;
                    }
                }
                Err(trb_pointer) => {
                    self.post_transfer_event(
                        trb_pointer,
                        0,
                        TrbCompletionCode::TrbError,
                        slot_id,
                        dci,
                    );
                    break;
                }
            }
        }
        self.guest_transfer_rings.insert((slot_id, dci), cursor);
    }

    /// Gather one TD off the ring: TRBs chain while the chain bit is set,
    /// the first chain-clear TRB closes the TD (ACRN's `USB_DATA_PART` /
    /// `USB_DATA_FULL` assembly). Each TRB is paired with the ring address a
    /// Transfer Event reports for it (`base + index * 16`). An undecodable
    /// TRB aborts the TD with its address as the error.
    fn gather_td(ring: &mut TransferRing) -> Result<Vec<(u64, TransferTrb)>, u64> {
        let mut td = Vec::new();
        loop {
            let index = ring.ring().dequeue_index();
            let base = ring.ring().base_addr();
            let Some(raw) = ring.dequeue() else { break };
            let trb_pointer = base + index.to_u64() * 16;
            match TransferTrb::from_trb(&raw) {
                Some(trb) => {
                    let chains = trb.chains();
                    td.push((trb_pointer, trb));
                    if !chains {
                        break;
                    }
                }
                None => return Err(trb_pointer),
            }
        }
        Ok(td)
    }

    /// Execute one TD on the control endpoint. The Setup/Data/Status stages
    /// arrive as separate TDs (xHCI §4.11.2.2), so this advances the slot's
    /// [`ControlStage`] machine: Setup stashes the packet, an IN data stage
    /// runs the request and fills the guest buffer, an OUT data stage
    /// gathers the guest's bytes, and Status runs any not-yet-run request
    /// and completes the transfer.
    fn execute_control_td(
        &mut self,
        slot_id: u8,
        td: &[(u64, TransferTrb)],
        mem: &mut dyn DmaMemory,
    ) {
        let last_pointer = td[td.len() - 1].0;
        match td[0].1 {
            TransferTrb::Setup { packet, ioc, .. } => {
                self.control_state
                    .insert(slot_id, ControlStage::SetupDone { setup: packet });
                if ioc {
                    self.post_transfer_event(
                        td[0].0,
                        0,
                        TrbCompletionCode::Success,
                        slot_id,
                        CONTROL_DCI,
                    );
                }
            }
            TransferTrb::Data { dir_in, .. } => {
                self.execute_control_data(slot_id, td, dir_in, mem);
            }
            TransferTrb::Status { ioc, .. } => {
                self.execute_control_status(slot_id, last_pointer, ioc, mem);
            }
            TransferTrb::NoOp { ioc } => {
                if ioc {
                    self.post_transfer_event(
                        last_pointer,
                        0,
                        TrbCompletionCode::Success,
                        slot_id,
                        CONTROL_DCI,
                    );
                }
            }
            // A TD headed by a Normal TRB on EP0 is malformed.
            TransferTrb::Normal { .. } => {
                self.post_transfer_event(
                    last_pointer,
                    0,
                    TrbCompletionCode::TrbError,
                    slot_id,
                    CONTROL_DCI,
                );
            }
        }
    }

    /// The Data Stage TD of a control transfer (a Data Stage TRB optionally
    /// chained with Normal TRBs). IN runs the request now and scatters the
    /// response into the guest buffers; OUT gathers the guest's bytes and
    /// defers the request to the Status stage.
    fn execute_control_data(
        &mut self,
        slot_id: u8,
        td: &[(u64, TransferTrb)],
        dir_in: bool,
        mem: &mut dyn DmaMemory,
    ) {
        let last_pointer = td[td.len() - 1].0;
        let ioc = td.iter().any(|(_, t)| t.interrupt_on_completion());
        let Some(buffers) = Self::td_buffers(td) else {
            self.post_transfer_event(
                last_pointer,
                0,
                TrbCompletionCode::TrbError,
                slot_id,
                CONTROL_DCI,
            );
            return;
        };
        let requested: u32 = buffers.iter().map(|(_, len)| *len).sum();
        // The data stage must follow a setup stage of the same direction.
        let setup = match self.control_state.remove(&slot_id) {
            Some(ControlStage::SetupDone { setup }) if setup.is_device_to_host() == dir_in => setup,
            _ => {
                self.post_transfer_event(
                    last_pointer,
                    0,
                    TrbCompletionCode::TrbError,
                    slot_id,
                    CONTROL_DCI,
                );
                return;
            }
        };
        if dir_in {
            let result = self
                .device_models
                .get_mut(&slot_id)
                .map_or(UsbTransferResult::Error, |model| model.control(&setup, &[]));
            match result {
                UsbTransferResult::Data(bytes) => {
                    let Some(written) = Self::scatter(mem, &buffers, &bytes) else {
                        self.post_transfer_event(
                            last_pointer,
                            requested,
                            TrbCompletionCode::UsbTransactionError,
                            slot_id,
                            CONTROL_DCI,
                        );
                        return;
                    };
                    self.control_state.insert(
                        slot_id,
                        ControlStage::DataDone {
                            setup,
                            out_data: Vec::new(),
                            responded: true,
                        },
                    );
                    self.complete_td(last_pointer, requested, written, ioc, slot_id, CONTROL_DCI);
                }
                UsbTransferResult::Stall => {
                    self.stall_endpoint(slot_id, CONTROL_DCI, last_pointer, requested, mem);
                }
                UsbTransferResult::Ack(_) | UsbTransferResult::Error => {
                    self.post_transfer_event(
                        last_pointer,
                        requested,
                        TrbCompletionCode::UsbTransactionError,
                        slot_id,
                        CONTROL_DCI,
                    );
                }
            }
        } else {
            let Some(data) = Self::gather_buffers(mem, &buffers) else {
                self.post_transfer_event(
                    last_pointer,
                    requested,
                    TrbCompletionCode::UsbTransactionError,
                    slot_id,
                    CONTROL_DCI,
                );
                return;
            };
            self.control_state.insert(
                slot_id,
                ControlStage::DataDone {
                    setup,
                    out_data: data,
                    responded: false,
                },
            );
            if ioc {
                self.post_transfer_event(
                    last_pointer,
                    0,
                    TrbCompletionCode::Success,
                    slot_id,
                    CONTROL_DCI,
                );
            }
        }
    }

    /// The Status Stage TD: run the request if the data stage did not
    /// already (OUT and no-data transfers), then complete the control
    /// transfer. Errors always post an event; success posts on IOC.
    fn execute_control_status(
        &mut self,
        slot_id: u8,
        trb_pointer: u64,
        ioc: bool,
        mem: &mut dyn DmaMemory,
    ) {
        let code = match self.control_state.remove(&slot_id) {
            Some(ControlStage::DataDone {
                responded: true, ..
            }) => TrbCompletionCode::Success,
            Some(ControlStage::DataDone {
                setup,
                out_data,
                responded: false,
            }) => self.run_control_request(slot_id, setup, &out_data),
            Some(ControlStage::SetupDone { setup }) => {
                self.run_control_request(slot_id, setup, &[])
            }
            // A status stage with no transfer in flight is a TRB error.
            None => TrbCompletionCode::TrbError,
        };
        if code == TrbCompletionCode::StallError {
            self.stall_endpoint(slot_id, CONTROL_DCI, trb_pointer, 0, mem);
            return;
        }
        if ioc || code != TrbCompletionCode::Success {
            self.post_transfer_event(trb_pointer, 0, code, slot_id, CONTROL_DCI);
        }
    }

    /// Run a control request against the slot's device model, mapping the
    /// outcome to a completion code (no bound model = transaction error, as
    /// for a device that fell off the bus).
    fn run_control_request(
        &mut self,
        slot_id: u8,
        setup: SetupPacket,
        out_data: &[u8],
    ) -> TrbCompletionCode {
        let result = self
            .device_models
            .get_mut(&slot_id)
            .map_or(UsbTransferResult::Error, |model| {
                model.control(&setup, out_data)
            });
        match result {
            UsbTransferResult::Ack(_) | UsbTransferResult::Data(_) => TrbCompletionCode::Success,
            UsbTransferResult::Stall => TrbCompletionCode::StallError,
            UsbTransferResult::Error => TrbCompletionCode::UsbTransactionError,
        }
    }

    /// Execute one bulk/interrupt TD: OUT gathers the guest buffers and
    /// sends them to the device, IN asks the device for up to the TD's
    /// capacity and scatters the response back.
    fn execute_normal_td(
        &mut self,
        slot_id: u8,
        dci: u8,
        td: &[(u64, TransferTrb)],
        mem: &mut dyn DmaMemory,
    ) {
        let last_pointer = td[td.len() - 1].0;
        let ioc = td.iter().any(|(_, t)| t.interrupt_on_completion());
        if let TransferTrb::NoOp { ioc } = td[0].1 {
            if ioc {
                self.post_transfer_event(last_pointer, 0, TrbCompletionCode::Success, slot_id, dci);
            }
            return;
        }
        let Some(buffers) = Self::td_buffers(td) else {
            self.post_transfer_event(last_pointer, 0, TrbCompletionCode::TrbError, slot_id, dci);
            return;
        };
        let requested: u32 = buffers.iter().map(|(_, len)| *len).sum();
        let endpoint = dci_endpoint_number(dci);
        let result = if dci_is_in(dci) {
            self.device_models
                .get_mut(&slot_id)
                .map_or(UsbTransferResult::Error, |model| {
                    model.transfer_in(endpoint, crate::truncate::usize_of(requested))
                })
        } else {
            match Self::gather_buffers(mem, &buffers) {
                Some(data) => self
                    .device_models
                    .get_mut(&slot_id)
                    .map_or(UsbTransferResult::Error, |model| {
                        model.transfer_out(endpoint, &data)
                    }),
                None => UsbTransferResult::Error,
            }
        };
        match result {
            UsbTransferResult::Data(bytes) => {
                let Some(written) = Self::scatter(mem, &buffers, &bytes) else {
                    self.post_transfer_event(
                        last_pointer,
                        requested,
                        TrbCompletionCode::UsbTransactionError,
                        slot_id,
                        dci,
                    );
                    return;
                };
                self.complete_td(last_pointer, requested, written, ioc, slot_id, dci);
            }
            UsbTransferResult::Ack(accepted) => {
                self.complete_td(last_pointer, requested, accepted, ioc, slot_id, dci);
            }
            UsbTransferResult::Stall => {
                self.stall_endpoint(slot_id, dci, last_pointer, requested, mem);
            }
            UsbTransferResult::Error => {
                self.post_transfer_event(
                    last_pointer,
                    requested,
                    TrbCompletionCode::UsbTransactionError,
                    slot_id,
                    dci,
                );
            }
        }
    }

    /// Post a TD's completion: the Transfer Event carries the **residual**
    /// (requested-but-untransferred bytes, the spec's 24-bit `EVENT_TRB_LEN`
    /// — drivers compute `transferred = requested - residual`). Success
    /// posts on IOC; a short transfer always posts, as Short Packet.
    fn complete_td(
        &mut self,
        trb_pointer: u64,
        requested: u32,
        transferred: usize,
        ioc: bool,
        slot_id: u8,
        dci: u8,
    ) {
        let residual = requested.saturating_sub(u32_of(transferred));
        let code = if residual > 0 {
            TrbCompletionCode::ShortPacket
        } else {
            TrbCompletionCode::Success
        };
        if ioc || residual > 0 {
            self.post_transfer_event(trb_pointer, residual, code, slot_id, dci);
        }
    }

    /// STALL: halt the endpoint (TDs stop processing until Reset Endpoint)
    /// and post the Stall Error event.
    fn stall_endpoint(
        &mut self,
        slot_id: u8,
        dci: u8,
        trb_pointer: u64,
        residual: u32,
        mem: &mut dyn DmaMemory,
    ) {
        if let Some(ring) = self.transfer_rings.get_mut(&(slot_id, dci)) {
            ring.halt();
        }
        if dci == CONTROL_DCI {
            self.control_state.remove(&slot_id);
        }
        // Reflect the Halted state into the output context (the driver reads
        // it to confirm the STALL before recovering with Reset Endpoint).
        self.publish_ep_state(slot_id, dci, EpState::Halted, mem);
        self.post_transfer_event(
            trb_pointer,
            residual,
            TrbCompletionCode::StallError,
            slot_id,
            dci,
        );
    }

    /// The (buffer, length) list of a data-carrying TD (a Data/Normal head
    /// chained with Normal TRBs). `None` if a non-data TRB is mixed in.
    fn td_buffers(td: &[(u64, TransferTrb)]) -> Option<Vec<(u64, u32)>> {
        td.iter()
            .map(|(_, trb)| match trb {
                TransferTrb::Data { buffer, length, .. }
                | TransferTrb::Normal { buffer, length, .. } => Some((*buffer, *length)),
                _ => None,
            })
            .collect()
    }

    /// Read and concatenate a TD's guest buffers (`None` on an unbacked
    /// address — a transaction error on the USB side).
    fn gather_buffers(mem: &dyn DmaMemory, buffers: &[(u64, u32)]) -> Option<Vec<u8>> {
        let mut data = Vec::new();
        for (address, length) in buffers {
            let mut chunk = vec![0_u8; crate::truncate::usize_of(*length)];
            if !mem.read(*address, &mut chunk) {
                return None;
            }
            data.append(&mut chunk);
        }
        Some(data)
    }

    /// Scatter `bytes` across a TD's guest buffers in order, returning how
    /// many were written (`None` on an unbacked address).
    fn scatter(mem: &mut dyn DmaMemory, buffers: &[(u64, u32)], bytes: &[u8]) -> Option<usize> {
        let mut offset = 0;
        for (address, length) in buffers {
            if offset >= bytes.len() {
                break;
            }
            let take = crate::truncate::usize_of(*length).min(bytes.len() - offset);
            if !mem.write(*address, &bytes[offset..offset + take]) {
                return None;
            }
            offset += take;
        }
        Some(offset)
    }

    /// Post a Transfer Event through the interrupt surfaces.
    fn post_transfer_event(
        &mut self,
        trb_pointer: u64,
        residual: u32,
        code: TrbCompletionCode,
        slot_id: u8,
        dci: u8,
    ) {
        self.post_event_trb(|ring| {
            ring.post_transfer_event(trb_pointer, residual, code, slot_id, dci)
        });
    }

    /// Attach a device of the given [`UsbSpeed`] to the **lowest free port of
    /// the matching protocol**, returning the 0-based port index it landed on
    /// (or `None` if every compatible port is occupied). This is the routing
    /// engine's entry point: it routes a device to this guest's controller
    /// without choosing a port itself.
    ///
    /// The root-hub ports are split by the Supported Protocol capabilities
    /// into a USB 2.0 group (the lower-numbered half — LS/FS/HS devices) and
    /// a USB 3.0 group (SS/SSP devices), exactly as the capabilities advertise
    /// them; a `SuperSpeed` drive will not land on a USB 2.0 port, matching real
    /// hardware.
    pub fn attach_device(&mut self, speed: UsbSpeed) -> Option<usize> {
        let usb2 = usize::from(self.caps.usb2_port_count());
        let range = if speed.is_superspeed() {
            usb2..self.op.ports.len()
        } else {
            0..usb2
        };
        let port = range
            .into_iter()
            .find(|&i| !self.op.ports[i].is_connected())?;
        if self.connect_device(port, speed.xhci_speed_id()) {
            Some(port)
        } else {
            None
        }
    }

    /// [`attach_device`](Self::attach_device), additionally parking `model`
    /// at the chosen port: when the guest's driver issues Address Device
    /// naming that port, the model binds to the slot and its transfer rings
    /// terminate against it. This is the routing path for devices with a
    /// backing model (emulated today, libusb-forwarded later).
    pub fn attach_device_with_model(
        &mut self,
        speed: UsbSpeed,
        model: Box<dyn UsbDeviceModel>,
    ) -> Option<usize> {
        let port = self.attach_device(speed)?;
        if let Some(parked) = self.port_models.get_mut(port) {
            *parked = Some(model);
        }
        Some(port)
    }

    /// Take the device model parked at `port` (0-based), if any — the
    /// registry uses this to carry a model along a live reassignment.
    pub fn take_port_model(&mut self, port: usize) -> Option<Box<dyn UsbDeviceModel>> {
        self.port_models.get_mut(port).and_then(Option::take)
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
    /// signalling as a connect. Any model parked at the port — or bound to a
    /// slot from it — goes with the device.
    pub fn disconnect_device(&mut self, port: usize) -> bool {
        let Some(p) = self.op.ports.get_mut(port) else {
            return false;
        };
        p.disconnect_device();
        if let Some(parked) = self.port_models.get_mut(port) {
            *parked = None;
        }
        if let Some((&slot_id, _)) = self.slot_ports.iter().find(|&(_, &p)| p == port) {
            self.slot_ports.remove(&slot_id);
            self.device_models.remove(&slot_id);
        }
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

    /// Ring doorbell 0 through the register window and service it (command
    /// processing happens at service time, when guest memory is in hand).
    fn kick_commands(c: &mut VirtualXhciController) {
        let mut mem = super::super::xhci::VecDmaMemory::new(0, 0);
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
    }

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
        kick_commands(&mut c);
        assert!(c.pop_event().is_none());
        // Running: the same doorbell drains the ring.
        c.write_register(u32::from(c.caps.caplength), 1);
        kick_commands(&mut c);
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
        kick_commands(&mut c);
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
        kick_commands(&mut c);
        let _ = c.pop_event();
        assert!(!c.slot_enabled(1));

        // Disabling it again is a TRB error.
        c.submit_command(&CommandTrb::DisableSlot { slot_id: slot });
        kick_commands(&mut c);
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
        kick_commands(&mut c);
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
    fn attach_fills_the_matching_protocol_port_and_maps_speed() {
        let mut c = running_controller(); // 4 ports: 0-1 USB2, 2-3 USB3
        c.write_register(c.caps.rtsoff + 0x20, 2); // IE

        // A keyboard (low speed) takes the lowest USB 2.0 port (0), speed id 2.
        assert_eq!(c.attach_device(UsbSpeed::Low), Some(0));
        let portsc0 = c.read_register(0x20 + 0x400);
        assert_eq!(
            (portsc0 >> 10) & 0xF,
            u32::from(UsbSpeed::Low.xhci_speed_id())
        );
        let _ = c.pop_event();

        // A SuperSpeed drive skips the USB 2.0 ports and lands on the lowest
        // USB 3.0 port (2), speed id 4 — it cannot land on a USB 2.0 port.
        assert_eq!(c.attach_device(UsbSpeed::Super), Some(2));
        let portsc2 = c.read_register(0x20 + 0x400 + 32);
        assert_eq!(
            (portsc2 >> 10) & 0xF,
            u32::from(UsbSpeed::Super.xhci_speed_id())
        );

        // Fill each protocol group; a third device of that protocol has no port.
        assert_eq!(c.attach_device(UsbSpeed::High), Some(1)); // last USB2
        assert_eq!(c.attach_device(UsbSpeed::Full), None); // USB2 full
        assert_eq!(c.attach_device(UsbSpeed::SuperPlus), Some(3)); // last USB3
        assert_eq!(c.attach_device(UsbSpeed::Super), None); // USB3 full

        // Detaching a USB2 port frees it for the next LS/FS/HS device.
        assert!(c.disconnect_device(0));
        assert_eq!(c.attach_device(UsbSpeed::Full), Some(0));
    }

    #[test]
    fn hccparams1_advertises_parseable_supported_protocol_caps() {
        use super::super::xhci::registers::XECP_OFFSET;
        let c = VirtualXhciController::new(4);

        // HCCPARAMS1 xECP (bits 31:16, in dwords) points at the cap list.
        let hccparams1 = c.read_register(0x10);
        let xecp_dwords = hccparams1 >> 16;
        assert_ne!(xecp_dwords, 0, "a real xHCI never has xECP == 0");
        assert_eq!(xecp_dwords * 4, XECP_OFFSET);

        // Walk the list: USB 2.0 cap, then (via the next pointer) USB 3.0.
        let cap0 = c.read_register(XECP_OFFSET);
        assert_eq!(cap0 & 0xFF, 0x02, "Supported Protocol cap ID");
        assert_eq!(cap0 >> 24, 2, "major revision 2 (USB 2.0)");
        assert_eq!(
            c.read_register(XECP_OFFSET + 4),
            u32::from_le_bytes(*b"USB ")
        );
        let ports0 = c.read_register(XECP_OFFSET + 8);
        assert_eq!(ports0 & 0xFF, 1, "USB2 compatible port offset");
        assert_eq!((ports0 >> 8) & 0xFF, 2, "USB2 covers ports 1-2");

        let next = (cap0 >> 8) & 0xFF; // next-cap pointer in dwords
        assert_eq!(next, 4);
        let cap1 = c.read_register(XECP_OFFSET + next * 4);
        assert_eq!(cap1 & 0xFF, 0x02);
        assert_eq!(cap1 >> 24, 3, "major revision 3 (USB 3.0)");
        assert_eq!((cap1 >> 8) & 0xFF, 0, "USB 3.0 cap ends the list");
        let ports1 = c.read_register(XECP_OFFSET + next * 4 + 8);
        assert_eq!(ports1 & 0xFF, 3, "USB3 ports start after the USB2 group");
        assert_eq!((ports1 >> 8) & 0xFF, 2, "USB3 covers ports 3-4");
    }

    /// A running controller with slot 1 enabled and a loopback device
    /// (VID:PID 1234:5678) bound to it, plus 4 KiB of "guest RAM" at 0x1000.
    fn controller_with_loopback() -> (VirtualXhciController, super::super::xhci::VecDmaMemory) {
        use super::super::emulated::LoopbackDevice;
        let mut c = running_controller();
        c.submit_command(&CommandTrb::EnableSlot);
        kick_commands(&mut c);
        let _ = c.pop_event();
        assert!(c.bind_device_model(1, Box::new(LoopbackDevice::new(0x1234, 0x5678))));
        (c, super::super::xhci::VecDmaMemory::new(0x1000, 4096))
    }

    /// Ring the doorbell for slot 1 / `dci` through the register window and
    /// service it against `mem` (the run loop's doorbell-exit shape).
    fn ring_and_service(
        c: &mut VirtualXhciController,
        dci: u8,
        mem: &mut super::super::xhci::VecDmaMemory,
    ) {
        c.write_register(c.caps.dboff + 4, u32::from(dci));
        c.service_doorbells(mem);
    }

    #[test]
    fn control_get_descriptor_fills_the_guest_buffer() {
        use super::super::xhci::transfer::{SetupPacket, TransferTrb, TransferType};
        let (mut c, mut mem) = controller_with_loopback();

        // The driver's GET_DESCRIPTOR(Device) sequence: Setup, Data IN at
        // guest 0x1100, Status OUT with IOC.
        let setup = SetupPacket {
            request_type: 0x80,
            request: 6,
            value: 0x0100,
            index: 0,
            length: 18,
        };
        assert!(c.submit_transfer(
            1,
            CONTROL_DCI,
            &TransferTrb::Setup {
                packet: setup,
                transfer_type: TransferType::InData,
                ioc: false,
            },
        ));
        assert!(c.submit_transfer(
            1,
            CONTROL_DCI,
            &TransferTrb::Data {
                buffer: 0x1100,
                length: 18,
                dir_in: true,
                chain: false,
                ioc: false,
            },
        ));
        assert!(c.submit_transfer(
            1,
            CONTROL_DCI,
            &TransferTrb::Status {
                dir_in: false,
                ioc: true,
            },
        ));
        ring_and_service(&mut c, CONTROL_DCI, &mut mem);

        // The 18-byte device descriptor landed at guest 0x1100 (offset 0x100).
        let bytes = mem.bytes();
        assert_eq!(bytes[0x100], 18, "bLength");
        assert_eq!(bytes[0x101], 1, "bDescriptorType DEVICE");
        assert_eq!(&bytes[0x108..0x10C], &[0x34, 0x12, 0x78, 0x56], "VID/PID");

        // One transfer event: the IOC'd status stage, Success, on EP0's DCI.
        match c.pop_event() {
            Some(EventTrb::TransferEvent {
                completion_code,
                slot_id,
                endpoint_id,
                ..
            }) => {
                assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8);
                assert_eq!(slot_id, 1);
                assert_eq!(endpoint_id, CONTROL_DCI);
            }
            other => panic!("expected a transfer event, got {other:?}"),
        }
        assert!(c.pop_event().is_none(), "no event without IOC");
    }

    #[test]
    fn bulk_loopback_round_trips_through_chained_trbs() {
        use super::super::xhci::transfer::TransferTrb;
        let (mut c, mut mem) = controller_with_loopback();

        // Guest data at 0x1000: two chained OUT TRBs form one 8-byte TD on
        // EP1 OUT (DCI 2).
        assert!(mem.write(0x1000, b"abcdEFGH"));
        for (buffer, chain) in [(0x1000_u64, true), (0x1004, false)] {
            assert!(c.submit_transfer(
                1,
                2,
                &TransferTrb::Normal {
                    buffer,
                    length: 4,
                    chain,
                    ioc: !chain,
                    isp: false,
                },
            ));
        }
        ring_and_service(&mut c, 2, &mut mem);
        match c.pop_event() {
            Some(EventTrb::TransferEvent {
                completion_code,
                transfer_length,
                endpoint_id,
                ..
            }) => {
                assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8);
                assert_eq!(transfer_length, 0, "residual is 0 — all 8 went out");
                assert_eq!(endpoint_id, 2);
            }
            other => panic!("expected a transfer event, got {other:?}"),
        }

        // EP1 IN (DCI 3) asks for 16 into 0x1200 — only 8 are queued, so the
        // event is a Short Packet with residual 8 and the bytes match.
        assert!(c.submit_transfer(
            1,
            3,
            &TransferTrb::Normal {
                buffer: 0x1200,
                length: 16,
                chain: false,
                ioc: true,
                isp: true,
            },
        ));
        ring_and_service(&mut c, 3, &mut mem);
        match c.pop_event() {
            Some(EventTrb::TransferEvent {
                completion_code,
                transfer_length,
                ..
            }) => {
                assert_eq!(
                    completion_code as u8,
                    TrbCompletionCode::ShortPacket as u8,
                    "16 asked, 8 delivered"
                );
                assert_eq!(transfer_length, 8, "residual = requested - transferred");
            }
            other => panic!("expected a transfer event, got {other:?}"),
        }
        assert_eq!(&mem.bytes()[0x200..0x208], b"abcdEFGH");
    }

    #[test]
    fn stall_halts_the_endpoint_until_reset_endpoint() {
        use super::super::xhci::transfer::{SetupPacket, TransferTrb, TransferType};
        let (mut c, mut mem) = controller_with_loopback();

        // An unsupported vendor request STALLs at the status stage.
        let weird = SetupPacket {
            request_type: 0x40,
            request: 0x42,
            value: 0,
            index: 0,
            length: 0,
        };
        assert!(c.submit_transfer(
            1,
            CONTROL_DCI,
            &TransferTrb::Setup {
                packet: weird,
                transfer_type: TransferType::NoData,
                ioc: false,
            },
        ));
        assert!(c.submit_transfer(
            1,
            CONTROL_DCI,
            &TransferTrb::Status {
                dir_in: true,
                ioc: true,
            },
        ));
        ring_and_service(&mut c, CONTROL_DCI, &mut mem);
        match c.pop_event() {
            Some(EventTrb::TransferEvent {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::StallError as u8),
            other => panic!("expected a stall event, got {other:?}"),
        }
        assert!(c.endpoint_halted(1, CONTROL_DCI));

        // Halted: the ring accepts nothing and processes nothing.
        assert!(!c.submit_transfer(
            1,
            CONTROL_DCI,
            &TransferTrb::Status {
                dir_in: true,
                ioc: true,
            },
        ));

        // Reset Endpoint recovers it, exactly as the driver would.
        c.submit_command(&CommandTrb::ResetEndpoint {
            slot_id: 1,
            endpoint_id: CONTROL_DCI,
        });
        kick_commands(&mut c);
        let _ = c.pop_event();
        assert!(!c.endpoint_halted(1, CONTROL_DCI));
        assert!(c.submit_transfer(1, CONTROL_DCI, &TransferTrb::NoOp { ioc: true }));
    }

    #[test]
    fn stall_and_reset_reflect_ep_state_in_the_output_context() {
        use super::super::emulated::LoopbackDevice;
        use super::super::xhci::EpState;
        use super::super::xhci::transfer::{DmaMemory, SetupPacket, TransferTrb, TransferType};
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x4000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap));
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes()));

        // Park a stalling loopback at port 1 and address it with a valid EP0.
        let port = c
            .attach_device_with_model(UsbSpeed::High, Box::new(LoopbackDevice::new(1, 2)))
            .unwrap();
        let _ = c.pop_event();
        let addr_in = 0x1000_u64;
        assert!(mem.write(addr_in + 4, &0x3_u32.to_le_bytes())); // A0|A1
        assert!(mem.write(
            addr_in + 0x24,
            &(u32::try_from(port + 1).unwrap() << 16).to_le_bytes()
        ));
        let ep0 = EndpointContext {
            endpoint_type: super::super::xhci::EndpointType::Control,
            max_packet_size: 64,
            max_burst_size: 0,
            error_count: 3,
            interval: 0,
            tr_dequeue_pointer: 0x4000,
            dequeue_cycle_state: true,
            average_trb_length: 8,
        };
        assert!(mem.write(
            addr_in + input_context_entry_offset(CONTROL_DCI),
            &ep0.to_bytes()
        ));
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event();
        let _ = c.pop_event();

        // EP0 is Running in the output context after Address Device.
        let mut ep0_out = [0_u8; 32];
        assert!(mem.read(
            out_ctx + device_context_entry_offset(CONTROL_DCI),
            &mut ep0_out
        ));
        assert_eq!(ep0_out[0] & 0x7, EpState::Running as u8);

        // An unsupported vendor control request STALLs at the status stage.
        let weird = SetupPacket {
            request_type: 0x40,
            request: 0x42,
            value: 0,
            index: 0,
            length: 0,
        };
        assert!(c.submit_transfer(
            1,
            CONTROL_DCI,
            &TransferTrb::Setup {
                packet: weird,
                transfer_type: TransferType::NoData,
                ioc: false,
            },
        ));
        assert!(c.submit_transfer(
            1,
            CONTROL_DCI,
            &TransferTrb::Status {
                dir_in: true,
                ioc: true,
            },
        ));
        ring_and_service(&mut c, CONTROL_DCI, &mut mem);
        let _ = c.pop_event(); // the StallError transfer event

        // The output EP0 context now reads Halted.
        assert!(mem.read(
            out_ctx + device_context_entry_offset(CONTROL_DCI),
            &mut ep0_out
        ));
        assert_eq!(ep0_out[0] & 0x7, EpState::Halted as u8);

        // Reset Endpoint recovers it back to Running.
        c.submit_command(&CommandTrb::ResetEndpoint {
            slot_id: 1,
            endpoint_id: CONTROL_DCI,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event();
        assert!(mem.read(
            out_ctx + device_context_entry_offset(CONTROL_DCI),
            &mut ep0_out
        ));
        assert_eq!(ep0_out[0] & 0x7, EpState::Running as u8);
    }

    // With guest-resident transfers enabled, an endpoint with nothing enqueued
    // via submit_transfer is driven straight from its guest-memory ring at the
    // endpoint context's TR Dequeue Pointer — the real hardware path. A No-Op
    // transfer TRB the "guest" wrote into EP0's ring is fetched and completed.
    #[test]
    fn guest_resident_transfer_ring_is_driven_from_guest_memory() {
        use super::super::emulated::LoopbackDevice;
        use super::super::xhci::VecDmaMemory;
        use super::super::xhci::transfer::TransferTrb;

        // EP0's guest-memory transfer ring.
        const RING: u64 = 0x4000;

        let mut c = running_controller();
        c.set_guest_resident_transfers(true);
        let mut mem = VecDmaMemory::new(0x1000, 0x6000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap));
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes()));

        // Park a loopback at port 1 and address it; EP0's ring lives at 0x4000.
        let port = c
            .attach_device_with_model(UsbSpeed::High, Box::new(LoopbackDevice::new(0x1234, 0x5678)))
            .unwrap();
        let _ = c.pop_event();
        let addr_in = 0x1000_u64;
        assert!(mem.write(addr_in + 4, &0x3_u32.to_le_bytes())); // A0|A1
        assert!(mem.write(
            addr_in + 0x24,
            &(u32::try_from(port + 1).unwrap() << 16).to_le_bytes()
        ));
        let ep0 = EndpointContext {
            endpoint_type: super::super::xhci::EndpointType::Control,
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
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event();
        let _ = c.pop_event();

        // The "guest" writes a No-Op transfer TRB into EP0's ring (cycle = 1, the
        // initial Consumer Cycle State) — no submit_transfer is used.
        assert!(mem.write(RING, &TransferTrb::NoOp { ioc: true }.to_trb(true).to_bytes()));

        // Ring the EP0 doorbell: the ring is fetched from guest memory and the
        // No-Op completes with Success.
        ring_and_service(&mut c, CONTROL_DCI, &mut mem);
        match c.pop_event() {
            Some(EventTrb::TransferEvent {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8),
            other => panic!("expected a transfer completion from the guest ring, got {other:?}"),
        }

        // With the flag OFF the same doorbell processes nothing (no enqueued
        // internal TRBs) — proving the guest path is what drove the completion.
        c.set_guest_resident_transfers(false);
        assert!(mem.write(RING + 16, &TransferTrb::NoOp { ioc: true }.to_trb(true).to_bytes()));
        ring_and_service(&mut c, CONTROL_DCI, &mut mem);
        assert!(c.pop_event().is_none());
    }

    #[test]
    fn set_tr_dequeue_pointer_repoints_the_ring() {
        use super::super::xhci::transfer::TransferTrb;
        let (mut c, mut mem) = controller_with_loopback();

        // A transfer on EP1 IN (DCI 3) auto-creates its ring.
        assert!(c.submit_transfer(1, 3, &TransferTrb::NoOp { ioc: true }));
        ring_and_service(&mut c, 3, &mut mem);
        let _ = c.pop_event();

        // Repoint the ring to a fresh guest address (STALL recovery's second
        // half, after Reset Endpoint).
        c.submit_command(&CommandTrb::SetTrDequeuePointer {
            slot_id: 1,
            endpoint_id: 3,
            dequeue_ptr: 0x5000,
            dcs: true,
        });
        kick_commands(&mut c);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);

        // A subsequent transfer reports its TRB pointer from the new base.
        assert!(c.submit_transfer(1, 3, &TransferTrb::NoOp { ioc: true }));
        ring_and_service(&mut c, 3, &mut mem);
        match c.pop_event() {
            Some(EventTrb::TransferEvent { trb_pointer, .. }) => assert_eq!(trb_pointer, 0x5000),
            other => panic!("expected a transfer event, got {other:?}"),
        }

        // Repointing an endpoint with no transfer ring is a Context State Error.
        c.submit_command(&CommandTrb::SetTrDequeuePointer {
            slot_id: 1,
            endpoint_id: 6,
            dequeue_ptr: 0x6000,
            dcs: true,
        });
        kick_commands(&mut c);
        assert_eq!(
            completion_code(&mut c),
            TrbCompletionCode::ContextStateError as u8
        );
    }

    #[test]
    fn device_slot_doorbells_defer_until_serviced_with_memory() {
        use super::super::xhci::transfer::TransferTrb;
        let (mut c, mut mem) = controller_with_loopback();
        assert!(c.submit_transfer(1, 3, &TransferTrb::NoOp { ioc: true }));

        // The doorbell write alone latches but does not process (no guest
        // memory is in hand at register-write time).
        c.write_register(c.caps.dboff + 4, 3);
        assert!(c.pop_event().is_none());
        assert!(c.doorbells.is_pending(1));

        c.service_doorbells(&mut mem);
        assert!(matches!(
            c.pop_event(),
            Some(EventTrb::TransferEvent { .. })
        ));
        assert!(!c.doorbells.is_pending(1));
    }

    #[test]
    fn transfer_to_unbacked_memory_is_a_transaction_error() {
        use super::super::xhci::transfer::TransferTrb;
        let (mut c, mut mem) = controller_with_loopback();

        // OUT TD pointing far outside the backed range.
        assert!(c.submit_transfer(
            1,
            2,
            &TransferTrb::Normal {
                buffer: 0xDEAD_0000,
                length: 4,
                chain: false,
                ioc: true,
                isp: false,
            },
        ));
        ring_and_service(&mut c, 2, &mut mem);
        match c.pop_event() {
            Some(EventTrb::TransferEvent {
                completion_code, ..
            }) => assert_eq!(
                completion_code as u8,
                TrbCompletionCode::UsbTransactionError as u8
            ),
            other => panic!("expected a transaction error, got {other:?}"),
        }
    }

    #[test]
    fn disable_slot_drops_rings_models_and_control_state() {
        use super::super::xhci::transfer::TransferTrb;
        let (mut c, _mem) = controller_with_loopback();
        assert!(c.submit_transfer(1, 3, &TransferTrb::NoOp { ioc: true }));

        c.submit_command(&CommandTrb::DisableSlot { slot_id: 1 });
        kick_commands(&mut c);
        let _ = c.pop_event();

        // The slot's transfer machinery went with it.
        assert!(!c.endpoint_halted(1, 3));
        assert!(!c.submit_transfer(1, 3, &TransferTrb::NoOp { ioc: true }));
        assert!(!c.bind_device_model(
            1,
            Box::new(super::super::emulated::LoopbackDevice::new(0, 0))
        ));
    }

    /// Write a minimal Address Device input context at `ptr`: input control
    /// context adding A0|A1, slot context naming `port_number` (1-based).
    fn write_input_context(mem: &mut super::super::xhci::VecDmaMemory, ptr: u64, port_number: u8) {
        use super::super::xhci::transfer::DmaMemory;
        assert!(mem.write(ptr + 4, &0x3_u32.to_le_bytes())); // add flags A0|A1
        let dword1 = u32::from(port_number) << 16; // root-hub port number
        assert!(mem.write(ptr + 0x24, &dword1.to_le_bytes()));
    }

    /// Pop the next event and return its command-completion code.
    fn completion_code(c: &mut VirtualXhciController) -> u8 {
        match c.pop_event() {
            Some(EventTrb::CommandCompletion {
                completion_code, ..
            }) => completion_code as u8,
            other => panic!("expected a command completion, got {other:?}"),
        }
    }

    #[test]
    fn address_device_validates_slot_and_input_context() {
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x100);
        write_input_context(&mut mem, 0x1000, 1);

        // On a never-enabled slot: TRB error, even with a valid context.
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 5,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::TrbError as u8);

        // After Enable Slot, a valid input context addresses successfully.
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event(); // the Enable Slot completion
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);

        // Add flags missing A0|A1: parameter error. Out-of-range port too.
        let mut bad_flags = super::super::xhci::VecDmaMemory::new(0x1000, 0x100);
        write_input_context(&mut bad_flags, 0x1000, 1);
        {
            use super::super::xhci::transfer::DmaMemory;
            assert!(bad_flags.write(0x1004, &1_u32.to_le_bytes())); // only A0
        }
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut bad_flags);
        assert_eq!(
            completion_code(&mut c),
            TrbCompletionCode::ParameterError as u8
        );

        let mut bad_port = super::super::xhci::VecDmaMemory::new(0x1000, 0x100);
        write_input_context(&mut bad_port, 0x1000, 99);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut bad_port);
        assert_eq!(
            completion_code(&mut c),
            TrbCompletionCode::ParameterError as u8
        );

        // An unreadable context pointer is a TRB error.
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0xDEAD_0000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::TrbError as u8);
    }

    #[test]
    fn address_device_binds_the_parked_port_model_and_disable_reparks_it() {
        use super::super::emulated::LoopbackDevice;
        use super::super::xhci::transfer::{DmaMemory, TransferTrb};
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x100);

        // Routing parks a model at the port; the guest's driver enables a
        // slot and addresses the device at that port.
        let port = c
            .attach_device_with_model(UsbSpeed::High, Box::new(LoopbackDevice::new(1, 2)))
            .unwrap();
        let _ = c.pop_event(); // port status change
        write_input_context(&mut mem, 0x1000, u8::try_from(port + 1).unwrap());
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event();
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);

        // The slot's transfer rings now terminate against the model: a bulk
        // OUT TD on EP1 OUT (DCI 2) is acked by the loopback.
        assert!(mem.write(0x1080, b"ping"));
        assert!(c.submit_transfer(
            1,
            2,
            &TransferTrb::Normal {
                buffer: 0x1080,
                length: 4,
                chain: false,
                ioc: true,
                isp: false,
            },
        ));
        c.write_register(c.caps.dboff + 4, 2);
        c.service_doorbells(&mut mem);
        match c.pop_event() {
            Some(EventTrb::TransferEvent {
                completion_code, ..
            }) => assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8),
            other => panic!("expected a transfer event, got {other:?}"),
        }

        // Disable Slot parks the model back at the port: re-enabling and
        // re-addressing finds it again (the guest re-enumerates).
        c.submit_command(&CommandTrb::DisableSlot { slot_id: 1 });
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event(); // disable completion
        let _ = c.pop_event(); // enable completion
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);
        assert!(c.submit_transfer(1, 2, &TransferTrb::NoOp { ioc: true }));
        c.write_register(c.caps.dboff + 4, 2);
        c.service_doorbells(&mut mem);
        assert!(matches!(
            c.pop_event(),
            Some(EventTrb::TransferEvent { .. })
        ));

        // Disconnecting the port drops the parked model for good.
        c.submit_command(&CommandTrb::DisableSlot { slot_id: 1 });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event();
        assert!(c.disconnect_device(port));
        assert!(c.take_port_model(port).is_none());
    }

    #[test]
    fn address_device_publishes_the_output_device_context() {
        use super::super::xhci::transfer::DmaMemory;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x4000);

        // DCBAAP names a Device Context Base Array; slot 1's entry points at
        // where the controller must publish the output device context.
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap)); // DCBAAP lo
        c.write_register(op + 0x34, 0); // DCBAAP hi
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes())); // DCBAA[1]

        // A full Address Device input context: A0|A1, a slot context naming
        // port 1 at high speed, and a valid EP0 control context.
        let input = 0x1000_u64;
        assert!(mem.write(input + 4, &0x3_u32.to_le_bytes())); // add A0|A1
        let slot_in = SlotContext {
            route_string: 0,
            speed: 3,
            context_entries: 1,
            root_hub_port_number: 1,
            usb_device_address: 0,
            slot_state: SlotState::DisabledEnabled as u8,
        };
        assert!(mem.write(input + input_context_entry_offset(0), &slot_in.to_bytes()));
        let ep0 = EndpointContext {
            endpoint_type: super::super::xhci::EndpointType::Control,
            max_packet_size: 64,
            max_burst_size: 0,
            error_count: 3,
            interval: 0,
            tr_dequeue_pointer: 0x4000,
            dequeue_cycle_state: true,
            average_trb_length: 8,
        };
        assert!(mem.write(
            input + input_context_entry_offset(CONTROL_DCI),
            &ep0.to_bytes()
        ));

        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: input,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event(); // Enable Slot completion
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);

        // The driver reads back the output slot context: Addressed, with the
        // assigned address, and speed/port preserved from the input.
        let mut slot_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(0), &mut slot_bytes));
        let slot_out = SlotContext::parse(&slot_bytes);
        assert_eq!(slot_out.slot_state, SlotState::Addressed as u8);
        assert_eq!(slot_out.usb_device_address, 1);
        assert_eq!(slot_out.speed, 3);
        assert_eq!(slot_out.root_hub_port_number, 1);
        assert!(slot_out.context_entries >= 1);

        // The output EP0 context carries EP State = Running (bits 2:0 = 1)
        // and the driver's declared max packet size.
        let mut ep0_bytes = [0_u8; 32];
        assert!(mem.read(
            out_ctx + device_context_entry_offset(CONTROL_DCI),
            &mut ep0_bytes
        ));
        assert_eq!(ep0_bytes[0] & 0x7, 1);
        assert_eq!(
            EndpointContext::parse(&ep0_bytes).unwrap().max_packet_size,
            64
        );
    }

    #[test]
    fn address_device_with_bsr_leaves_the_slot_in_default() {
        use super::super::xhci::transfer::DmaMemory;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x4000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap));
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes()));

        let addr_in = 0x1000_u64;
        write_input_context(&mut mem, addr_in, 1);
        c.submit_command(&CommandTrb::EnableSlot);
        // Address Device with the BSR control bit (9) set: crafted on the
        // command ring directly, since BSR is a raw-command modifier.
        let mut trb = CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        }
        .to_trb(true);
        trb.control |= 1 << 9; // BSR
        assert!(c.command_ring.submit(trb));
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event(); // enable
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);

        // The slot reaches the Default state at address 0 (not Addressed).
        let mut slot_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(0), &mut slot_bytes));
        let slot_out = SlotContext::parse(&slot_bytes);
        assert_eq!(slot_out.slot_state, SlotState::Default as u8);
        assert_eq!(slot_out.usb_device_address, 0);

        // A second Address Device without BSR completes the addressing.
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);
        assert!(mem.read(out_ctx + device_context_entry_offset(0), &mut slot_bytes));
        let slot_out = SlotContext::parse(&slot_bytes);
        assert_eq!(slot_out.slot_state, SlotState::Addressed as u8);
        assert_eq!(slot_out.usb_device_address, 1);
    }

    #[test]
    fn address_device_without_dcbaap_skips_the_output_context() {
        // The pre-run-loop modelling path: no DCBAAP programmed, so there is
        // nowhere to publish — Address Device still succeeds and binds.
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x100);
        write_input_context(&mut mem, 0x1000, 1);
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event();
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);
    }

    #[test]
    fn configure_endpoint_publishes_the_output_device_context() {
        use super::super::xhci::transfer::DmaMemory;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x4000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap));
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes())); // DCBAA[1]

        // Enable + Address the slot so it reaches the Addressed state.
        let addr_in = 0x1000_u64;
        write_input_context(&mut mem, addr_in, 1);
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event(); // enable
        let _ = c.pop_event(); // address

        // Configure Endpoint adding a bulk OUT endpoint at EP1 OUT (DCI 2).
        let cfg_in = 0x1100_u64;
        write_configure_context(&mut mem, cfg_in, 0, &[(2, bulk_out_context(0x4000))]);
        c.submit_command(&CommandTrb::ConfigureEndpoint {
            slot_id: 1,
            input_context_ptr: cfg_in,
            deconfigure: false,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);

        // The output slot context is now Configured with Context Entries = 2.
        let mut slot_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(0), &mut slot_bytes));
        let slot_out = SlotContext::parse(&slot_bytes);
        assert_eq!(slot_out.slot_state, SlotState::Configured as u8);
        assert_eq!(slot_out.context_entries, 2);

        // The added EP2 output context is Running with its declared ring.
        let mut ep_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(2), &mut ep_bytes));
        assert_eq!(ep_bytes[0] & 0x7, 1);
        assert_eq!(
            EndpointContext::parse(&ep_bytes)
                .unwrap()
                .tr_dequeue_pointer,
            0x4000
        );

        // Deconfigure (DC = 1) returns the slot to Addressed and zeroes EP2.
        c.submit_command(&CommandTrb::ConfigureEndpoint {
            slot_id: 1,
            input_context_ptr: 0,
            deconfigure: true,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);
        assert!(mem.read(out_ctx + device_context_entry_offset(0), &mut slot_bytes));
        assert_eq!(
            SlotContext::parse(&slot_bytes).slot_state,
            SlotState::Addressed as u8
        );
        assert!(mem.read(out_ctx + device_context_entry_offset(2), &mut ep_bytes));
        assert_eq!(ep_bytes, [0_u8; 32]);
    }

    #[test]
    fn evaluate_context_updates_ep0_max_packet_size() {
        use super::super::xhci::transfer::DmaMemory;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x4000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap));
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes()));

        // Address the slot with an EP0 declaring the spec's initial 8-byte
        // max packet size (a full-speed device before its descriptor is read).
        let addr_in = 0x1000_u64;
        assert!(mem.write(addr_in + 4, &0x3_u32.to_le_bytes())); // A0|A1
        assert!(mem.write(addr_in + 0x24, &(1_u32 << 16).to_le_bytes())); // port 1
        let ep0_small = EndpointContext {
            endpoint_type: super::super::xhci::EndpointType::Control,
            max_packet_size: 8,
            max_burst_size: 0,
            error_count: 3,
            interval: 0,
            tr_dequeue_pointer: 0x4000,
            dequeue_cycle_state: true,
            average_trb_length: 8,
        };
        assert!(mem.write(
            addr_in + input_context_entry_offset(CONTROL_DCI),
            &ep0_small.to_bytes()
        ));
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event();
        let _ = c.pop_event();
        assert_eq!(
            c.endpoint_config(1, CONTROL_DCI).unwrap().max_packet_size,
            8
        );

        // Evaluate Context with EP0 now declaring the real 64-byte max packet.
        let eval_in = 0x1100_u64;
        assert!(mem.write(eval_in + 4, &0x2_u32.to_le_bytes())); // A1 (EP0)
        let mut ep0_full = ep0_small;
        ep0_full.max_packet_size = 64;
        assert!(mem.write(
            eval_in + input_context_entry_offset(CONTROL_DCI),
            &ep0_full.to_bytes()
        ));
        c.submit_command(&CommandTrb::EvaluateContext {
            slot_id: 1,
            input_context_ptr: eval_in,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);

        // The stored EP0 config and the output context both carry 64 now, and
        // EP0 is still Running (state unchanged).
        assert_eq!(
            c.endpoint_config(1, CONTROL_DCI).unwrap().max_packet_size,
            64
        );
        let mut ep0_bytes = [0_u8; 32];
        assert!(mem.read(
            out_ctx + device_context_entry_offset(CONTROL_DCI),
            &mut ep0_bytes
        ));
        assert_eq!(ep0_bytes[0] & 0x7, 1); // Running
        assert_eq!(
            EndpointContext::parse(&ep0_bytes).unwrap().max_packet_size,
            64
        );
    }

    #[test]
    fn stop_endpoint_transitions_the_output_context_to_stopped() {
        use super::super::xhci::EpState;
        use super::super::xhci::transfer::DmaMemory;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x4000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap));
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes()));

        let addr_in = 0x1000_u64;
        write_input_context(&mut mem, addr_in, 1);
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        });
        let cfg_in = 0x1100_u64;
        write_configure_context(&mut mem, cfg_in, 0, &[(2, bulk_out_context(0x4000))]);
        c.submit_command(&CommandTrb::ConfigureEndpoint {
            slot_id: 1,
            input_context_ptr: cfg_in,
            deconfigure: false,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        for _ in 0..3 {
            let _ = c.pop_event();
        }
        // EP2 is Running after Configure Endpoint.
        let mut ep_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(2), &mut ep_bytes));
        assert_eq!(ep_bytes[0] & 0x7, EpState::Running as u8);

        // Stop Endpoint transitions it to Stopped.
        c.submit_command(&CommandTrb::StopEndpoint {
            slot_id: 1,
            endpoint_id: 2,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);
        assert!(mem.read(out_ctx + device_context_entry_offset(2), &mut ep_bytes));
        assert_eq!(ep_bytes[0] & 0x7, EpState::Stopped as u8);
        // The rest of the endpoint context is preserved (its declared ring).
        assert_eq!(
            EndpointContext::parse(&ep_bytes)
                .unwrap()
                .tr_dequeue_pointer,
            0x4000
        );
    }

    #[test]
    fn reset_device_returns_the_slot_to_default() {
        use super::super::xhci::transfer::DmaMemory;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x4000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap));
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes()));

        // Address then configure an endpoint.
        let addr_in = 0x1000_u64;
        write_input_context(&mut mem, addr_in, 1);
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        });
        let cfg_in = 0x1100_u64;
        write_configure_context(&mut mem, cfg_in, 0, &[(2, bulk_out_context(0x4000))]);
        c.submit_command(&CommandTrb::ConfigureEndpoint {
            slot_id: 1,
            input_context_ptr: cfg_in,
            deconfigure: false,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        for _ in 0..3 {
            let _ = c.pop_event();
        }
        assert!(c.endpoint_config(1, 2).is_some());

        // Reset Device: slot returns to Default with address 0, EP2 dropped,
        // EP0 retained; the slot stays enabled.
        c.submit_command(&CommandTrb::ResetDevice { slot_id: 1 });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);
        assert!(c.slot_enabled(1));
        assert!(c.endpoint_config(1, 2).is_none());

        let mut slot_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(0), &mut slot_bytes));
        let slot_out = SlotContext::parse(&slot_bytes);
        assert_eq!(slot_out.slot_state, SlotState::Default as u8);
        assert_eq!(slot_out.usb_device_address, 0);
        assert_eq!(slot_out.context_entries, 1);

        let mut ep_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(2), &mut ep_bytes));
        assert_eq!(ep_bytes, [0_u8; 32]);
    }

    #[test]
    fn disable_slot_publishes_the_disabled_output_context() {
        use super::super::xhci::transfer::DmaMemory;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x4000);
        let dcbaap = 0x2000_u64;
        let out_ctx = 0x3000_u64;
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x30, u32_of(dcbaap));
        assert!(mem.write(dcbaap + 8, &out_ctx.to_le_bytes()));

        // Address then configure an endpoint so the output context is populated.
        let addr_in = 0x1000_u64;
        write_input_context(&mut mem, addr_in, 1);
        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: addr_in,
        });
        let cfg_in = 0x1100_u64;
        write_configure_context(&mut mem, cfg_in, 0, &[(2, bulk_out_context(0x4000))]);
        c.submit_command(&CommandTrb::ConfigureEndpoint {
            slot_id: 1,
            input_context_ptr: cfg_in,
            deconfigure: false,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        for _ in 0..3 {
            let _ = c.pop_event();
        }

        // Disable Slot returns the output slot context to the Disabled state
        // with no address and zeroes the held endpoint contexts.
        c.submit_command(&CommandTrb::DisableSlot { slot_id: 1 });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);

        let mut slot_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(0), &mut slot_bytes));
        let slot_out = SlotContext::parse(&slot_bytes);
        assert_eq!(slot_out.slot_state, SlotState::DisabledEnabled as u8);
        assert_eq!(slot_out.usb_device_address, 0);
        assert_eq!(slot_out.context_entries, 0);

        let mut ep_bytes = [0_u8; 32];
        assert!(mem.read(out_ctx + device_context_entry_offset(2), &mut ep_bytes));
        assert_eq!(ep_bytes, [0_u8; 32]);
    }

    /// A bulk OUT endpoint context (EP1 OUT = DCI 2) with its ring at `ring`.
    fn bulk_out_context(ring: u64) -> EndpointContext {
        EndpointContext {
            endpoint_type: super::super::xhci::EndpointType::BulkOut,
            max_packet_size: 512,
            max_burst_size: 0,
            error_count: 3,
            interval: 0,
            tr_dequeue_pointer: ring,
            dequeue_cycle_state: true,
            average_trb_length: 512,
        }
    }

    /// Write a Configure Endpoint input context at `ptr`: A0 plus an add
    /// flag and context entry per `entries` element, and `drop_flags`.
    fn write_configure_context(
        mem: &mut super::super::xhci::VecDmaMemory,
        ptr: u64,
        drop_flags: u32,
        entries: &[(u8, EndpointContext)],
    ) {
        use super::super::xhci::transfer::DmaMemory;
        let mut add_flags = 1_u32; // A0: the slot context comes along
        for (dci, _) in entries {
            add_flags |= 1 << dci;
        }
        assert!(mem.write(ptr, &drop_flags.to_le_bytes()));
        assert!(mem.write(ptr + 4, &add_flags.to_le_bytes()));
        for (dci, context) in entries {
            assert!(mem.write(ptr + input_context_entry_offset(*dci), &context.to_bytes()));
        }
    }

    /// A running controller with slot 1 enabled and 0x200 bytes of guest
    /// RAM at 0x1000 for input contexts.
    fn controller_with_slot() -> (VirtualXhciController, super::super::xhci::VecDmaMemory) {
        let mut c = running_controller();
        c.submit_command(&CommandTrb::EnableSlot);
        kick_commands(&mut c);
        let _ = c.pop_event();
        (c, super::super::xhci::VecDmaMemory::new(0x1000, 0x200))
    }

    /// Submit a Configure Endpoint and return its completion code.
    fn configure(
        c: &mut VirtualXhciController,
        mem: &mut super::super::xhci::VecDmaMemory,
        input_context_ptr: u64,
        deconfigure: bool,
    ) -> u8 {
        c.submit_command(&CommandTrb::ConfigureEndpoint {
            slot_id: 1,
            input_context_ptr,
            deconfigure,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(mem);
        completion_code(c)
    }

    #[test]
    fn configure_endpoint_installs_and_drops_rings_from_the_input_context() {
        use super::super::xhci::EndpointType;
        let (mut c, mut mem) = controller_with_slot();

        // The driver declares EP1 OUT (DCI 2, bulk) and EP1 IN (DCI 3,
        // interrupt — keyboard-shaped) with their rings in guest memory.
        let interrupt_in = EndpointContext {
            endpoint_type: EndpointType::InterruptIn,
            max_packet_size: 8,
            max_burst_size: 0,
            error_count: 3,
            interval: 7,
            tr_dequeue_pointer: 0x7650,
            dequeue_cycle_state: true,
            average_trb_length: 8,
        };
        write_configure_context(
            &mut mem,
            0x1000,
            0,
            &[(2, bulk_out_context(0x4560)), (3, interrupt_in)],
        );
        assert_eq!(
            configure(&mut c, &mut mem, 0x1000, false),
            TrbCompletionCode::Success as u8
        );

        // Both endpoints carry the declared characteristics, and their
        // rings sit at the declared TR Dequeue Pointers.
        assert_eq!(c.endpoint_config(1, 2), Some(&bulk_out_context(0x4560)));
        assert_eq!(c.endpoint_config(1, 3), Some(&interrupt_in));
        assert_eq!(c.transfer_rings[&(1, 2)].ring().base_addr(), 0x4560);
        assert_eq!(c.transfer_rings[&(1, 3)].ring().base_addr(), 0x7650);

        // A follow-up configure dropping DCI 3 removes only that endpoint.
        write_configure_context(&mut mem, 0x1000, 1 << 3, &[]);
        assert_eq!(
            configure(&mut c, &mut mem, 0x1000, false),
            TrbCompletionCode::Success as u8
        );
        assert!(c.endpoint_config(1, 3).is_none());
        assert!(!c.transfer_rings.contains_key(&(1, 3)));
        assert!(c.endpoint_config(1, 2).is_some());

        // Disable Slot clears the survivors with the slot.
        c.submit_command(&CommandTrb::DisableSlot { slot_id: 1 });
        kick_commands(&mut c);
        let _ = c.pop_event();
        assert!(c.endpoint_config(1, 2).is_none());
    }

    #[test]
    fn configure_endpoint_validates_flags_and_contexts() {
        use super::super::xhci::transfer::DmaMemory;
        let (mut c, mut mem) = controller_with_slot();

        // A disabled slot is a TRB error regardless of the context.
        c.submit_command(&CommandTrb::ConfigureEndpoint {
            slot_id: 5,
            input_context_ptr: 0x1000,
            deconfigure: false,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion_code(&mut c), TrbCompletionCode::TrbError as u8);

        // An unreadable input context pointer is a TRB error.
        assert_eq!(
            configure(&mut c, &mut mem, 0xDEAD_0000, false),
            TrbCompletionCode::TrbError as u8
        );

        // A1 (EP0) cannot be re-configured here: parameter error.
        write_configure_context(&mut mem, 0x1000, 0, &[]);
        assert!(mem.write(0x1004, &0x3_u32.to_le_bytes())); // A0|A1
        assert_eq!(
            configure(&mut c, &mut mem, 0x1000, false),
            TrbCompletionCode::ParameterError as u8
        );

        // D1 (EP0) is reserved: parameter error.
        write_configure_context(&mut mem, 0x1000, 1 << 1, &[]);
        assert_eq!(
            configure(&mut c, &mut mem, 0x1000, false),
            TrbCompletionCode::ParameterError as u8
        );

        // An added context whose EP Type is 0 (Not Valid) is a parameter
        // error — the zeroed entry at DCI 4 was never written.
        write_configure_context(&mut mem, 0x1000, 0, &[]);
        assert!(mem.write(0x1004, &((1_u32 << 4) | 1).to_le_bytes()));
        assert_eq!(
            configure(&mut c, &mut mem, 0x1000, false),
            TrbCompletionCode::ParameterError as u8
        );

        // A direction/DCI mismatch (bulk OUT context at an IN DCI) is a
        // parameter error, and the valid entry alongside it must NOT have
        // been installed (the command fails without partial effects).
        write_configure_context(
            &mut mem,
            0x1000,
            0,
            &[(2, bulk_out_context(0x4560)), (5, bulk_out_context(0x8880))],
        );
        assert_eq!(
            configure(&mut c, &mut mem, 0x1000, false),
            TrbCompletionCode::ParameterError as u8
        );
        assert!(c.endpoint_config(1, 2).is_none());
        assert!(!c.transfer_rings.contains_key(&(1, 2)));
    }

    #[test]
    fn deconfigure_drops_every_endpoint_but_control() {
        use super::super::xhci::transfer::TransferTrb;
        let (mut c, mut mem) = controller_with_slot();
        write_configure_context(&mut mem, 0x1000, 0, &[(2, bulk_out_context(0x4560))]);
        assert_eq!(
            configure(&mut c, &mut mem, 0x1000, false),
            TrbCompletionCode::Success as u8
        );
        // EP0 has live ring state too (a pending NoOp creates its ring).
        assert!(c.submit_transfer(1, CONTROL_DCI, &TransferTrb::NoOp { ioc: false }));

        // DC set: the input context pointer is not referenced — a garbage
        // pointer must not fail the command.
        assert_eq!(
            configure(&mut c, &mut mem, 0xDEAD_0000, true),
            TrbCompletionCode::Success as u8
        );
        assert!(c.endpoint_config(1, 2).is_none());
        assert!(!c.transfer_rings.contains_key(&(1, 2)));
        assert!(
            c.transfer_rings.contains_key(&(1, CONTROL_DCI)),
            "EP0 survives"
        );
    }

    #[test]
    fn events_land_in_the_guest_event_ring_once_erstba_is_programmed() {
        use super::super::xhci::transfer::DmaMemory;
        use super::super::xhci::trb::TrbType;
        let (mut c, mut mem) = controller_with_slot();
        let _drained = c.pop_event(); // keep the internal queue empty? (none pending)

        // The driver programs a one-segment event ring: ERST at 0x1000
        // describing 16 TRBs at 0x1100, ERDP parked at the first slot.
        assert!(mem.write(0x1000, &0x1100_u64.to_le_bytes()));
        assert!(mem.write(0x1008, &16_u16.to_le_bytes()));
        c.write_register(c.caps.rtsoff + 0x28, 1); // ERSTSZ
        c.write_register(c.caps.rtsoff + 0x30, 0x1000); // ERSTBA lo
        c.write_register(c.caps.rtsoff + 0x34, 0); // ERSTBA hi
        c.write_register(c.caps.rtsoff + 0x38, 0x1100); // ERDP lo
        c.write_register(c.caps.rtsoff + 0x3C, 0); // ERDP hi

        // A serviced command's completion event is written to guest memory.
        c.submit_command(&CommandTrb::NoOp);
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let mut bytes = [0_u8; 16];
        assert!(mem.read(0x1100, &mut bytes));
        let trb = Trb::from_bytes(&bytes);
        assert_eq!(trb.decoded_type(), TrbType::CommandCompletionEvent);
        assert_eq!(trb.control & 1, 1, "PCS 1 on the first lap");
        assert_eq!((trb.status >> 24) & 0xFF, TrbCompletionCode::Success as u32);
        assert!(
            c.pop_event().is_none(),
            "delivered events leave the internal queue"
        );

        // Hot-plug events post outside doorbell servicing; the run loop's
        // explicit flush writes them to the next slot.
        assert!(c.connect_device(0, 3));
        assert_eq!(c.flush_events(&mut mem), 1);
        assert!(mem.read(0x1110, &mut bytes));
        assert_eq!(
            Trb::from_bytes(&bytes).decoded_type(),
            TrbType::PortStatusChangeEvent
        );
    }

    /// Write a raw TRB into guest memory at `addr`.
    fn write_trb(mem: &mut super::super::xhci::VecDmaMemory, addr: u64, trb: &Trb) {
        use super::super::xhci::transfer::DmaMemory;
        assert!(mem.write(addr, &trb.to_bytes()));
    }

    /// Pop the next event, asserting it is a command completion, and
    /// return (command TRB pointer, completion code, slot ID).
    fn completion(c: &mut VirtualXhciController) -> (u64, u8, u8) {
        match c.pop_event() {
            Some(EventTrb::CommandCompletion {
                command_trb_pointer,
                completion_code,
                slot_id,
            }) => (command_trb_pointer, completion_code as u8, slot_id),
            other => panic!("expected a command completion, got {other:?}"),
        }
    }

    #[test]
    fn commands_fetch_from_the_guest_ring_once_crcr_is_programmed() {
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x400);
        let op = u32::from(c.caps.caplength);

        // The driver writes Enable Slot + No Op at 0x1200 (cycle 1; the
        // zeroed TRBs beyond have cycle 0 = end of ring) and points CRCR
        // there with RCS = 1.
        write_trb(&mut mem, 0x1200, &CommandTrb::EnableSlot.to_trb(true));
        write_trb(&mut mem, 0x1210, &CommandTrb::NoOp.to_trb(true));
        c.write_register(op + 0x18, 0x1201); // CRCR lo: pointer | RCS
        c.write_register(op + 0x1C, 0);
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);

        // Completions report the real guest addresses the TRBs were
        // fetched from, in ring order.
        let (pointer, code, slot_id) = completion(&mut c);
        assert_eq!(
            (pointer, code, slot_id),
            (0x1200, TrbCompletionCode::Success as u8, 1)
        );
        assert!(c.slot_enabled(1));
        assert_eq!(completion(&mut c).0, 0x1210);
        assert!(c.pop_event().is_none(), "cycle bit delimits the ring");

        // The cursor persists across doorbells: the driver enqueues one
        // more and rings again.
        write_trb(&mut mem, 0x1220, &CommandTrb::NoOp.to_trb(true));
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion(&mut c).0, 0x1220);
    }

    #[test]
    fn guest_command_ring_follows_link_trbs_and_toggle_cycle() {
        use super::super::xhci::TrbType;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x400);
        let op = u32::from(c.caps.caplength);

        // A two-TRB segment: No Op, then a Link back to the segment base
        // with Toggle Cycle set (the standard single-segment ring shape).
        write_trb(&mut mem, 0x1200, &CommandTrb::NoOp.to_trb(true));
        let mut link = Trb::zeroed();
        link.set_trb_type(TrbType::Link);
        link.parameter = 0x1200;
        link.control |= 0x2; // Toggle Cycle
        link.set_cycle_bit(true);
        write_trb(&mut mem, 0x1210, &link);
        c.write_register(op + 0x18, 0x1201);
        c.write_register(op + 0x1C, 0);
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);

        // One command this lap; the link flipped CCS to 0, so the lap-1
        // No Op at the base (cycle 1) is not consumed again.
        assert_eq!(completion(&mut c).0, 0x1200);
        assert!(c.pop_event().is_none());

        // Lap 2: the driver writes with cycle 0 now.
        write_trb(&mut mem, 0x1200, &CommandTrb::NoOp.to_trb(false));
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        assert_eq!(completion(&mut c).0, 0x1200);
    }

    #[test]
    fn address_device_records_ep0s_declared_context() {
        use super::super::xhci::EndpointType;
        use super::super::xhci::transfer::DmaMemory;
        let mut c = running_controller();
        let mut mem = super::super::xhci::VecDmaMemory::new(0x1000, 0x100);
        write_input_context(&mut mem, 0x1000, 1);
        let ep0 = EndpointContext {
            endpoint_type: EndpointType::Control,
            max_packet_size: 64,
            max_burst_size: 0,
            error_count: 3,
            interval: 0,
            tr_dequeue_pointer: 0x2340,
            dequeue_cycle_state: true,
            average_trb_length: 8,
        };
        assert!(mem.write(
            0x1000 + input_context_entry_offset(CONTROL_DCI),
            &ep0.to_bytes()
        ));

        c.submit_command(&CommandTrb::EnableSlot);
        c.submit_command(&CommandTrb::AddressDevice {
            slot_id: 1,
            input_context_ptr: 0x1000,
        });
        c.write_register(c.caps.dboff, 0);
        c.service_doorbells(&mut mem);
        let _ = c.pop_event();
        assert_eq!(completion_code(&mut c), TrbCompletionCode::Success as u8);
        assert_eq!(c.endpoint_config(1, CONTROL_DCI), Some(&ep0));
    }
}
