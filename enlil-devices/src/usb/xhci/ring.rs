//! TRB ring buffer implementation for xHCI command, transfer, and event rings.
//!
//! xHCI uses circular ring buffers of TRBs for communication between software
//! and the host controller. Three ring types exist:
//! - **Command Ring:** Software → controller commands (addressed via CRCR)
//! - **Transfer Ring:** Per-endpoint data transfer descriptors
//! - **Event Ring:** Controller → software completion notifications
//!
//! All rings use a producer/consumer model with a Cycle State bit to track
//! ownership. When the producer toggles the cycle bit in a TRB, the consumer
//! knows a new entry is available.

use super::transfer::DmaMemory;
use super::trb::{Trb, TrbType};

// ---------------------------------------------------------------------------
// Ring configuration
// ---------------------------------------------------------------------------

/// Default number of TRB entries per ring segment.
const DEFAULT_RING_SIZE: usize = 256;

// ---------------------------------------------------------------------------
// Guest-memory ring cursor (consumer side)
// ---------------------------------------------------------------------------

/// How many Link TRBs one fetch will follow before giving up — a guard
/// against a cyclic link chain wedging the controller.
const LINK_HOP_LIMIT: usize = 8;

/// The consumer side of a TRB ring resident in guest memory (xHCI §4.9):
/// a dequeue pointer plus the Consumer Cycle State.
///
/// A TRB belongs to the consumer while its cycle bit matches CCS; the first
/// mismatch is the end of the ring's valid TRBs. Link TRBs (type 6) chain
/// segments: the cursor follows the link's Ring Segment Pointer and toggles
/// CCS when the link's Toggle Cycle bit is set. The command ring consumes
/// through this cursor today; transfer rings move onto it when their guest
/// addresses come from the endpoint contexts.
#[derive(Debug, Clone, Copy)]
pub struct GuestRingCursor {
    /// Guest physical address of the next TRB to consume.
    dequeue: u64,
    /// Consumer Cycle State.
    cycle: bool,
}

impl GuestRingCursor {
    /// A cursor at `pointer` with the given initial Consumer Cycle State
    /// (for the command ring: `CRCR` bits 63:6 and RCS, xHCI §5.4.5).
    #[must_use]
    pub const fn new(pointer: u64, cycle: bool) -> Self {
        Self {
            dequeue: pointer,
            cycle,
        }
    }

    /// Guest physical address of the next TRB to consume.
    #[must_use]
    pub const fn dequeue_pointer(&self) -> u64 {
        self.dequeue
    }

    /// Fetch the TRB at the dequeue pointer, following Link TRBs. Returns
    /// the TRB and the guest address it was fetched from (what its
    /// completion event reports), or `None` when the next TRB's cycle bit
    /// says the ring is exhausted (or its memory is unbacked).
    pub fn fetch(&mut self, mem: &dyn DmaMemory) -> Option<(u64, Trb)> {
        for _ in 0..LINK_HOP_LIMIT {
            let mut bytes = [0_u8; 16];
            if !mem.read(self.dequeue, &mut bytes) {
                return None;
            }
            let trb = Trb::from_bytes(&bytes);
            if trb.cycle_bit() != self.cycle {
                return None;
            }
            if trb.decoded_type() == TrbType::Link {
                // Toggle Cycle (control bit 1) flips CCS for the next
                // segment lap (xHCI §6.4.4.1).
                if trb.control & 0x2 != 0 {
                    self.cycle = !self.cycle;
                }
                self.dequeue = trb.parameter & !0xF;
                continue;
            }
            let address = self.dequeue;
            self.dequeue += 16;
            return Some((address, trb));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// TRB Ring (generic base)
// ---------------------------------------------------------------------------

/// A generic TRB ring buffer used as the foundation for command and transfer rings.
///
/// Implements the producer side of the ring: software enqueues TRBs and advances
/// the enqueue pointer. The cycle state bit is toggled when wrapping around.
#[derive(Debug)]
pub struct TrbRing {
    /// Ring entries.
    entries: Vec<Trb>,
    /// Current enqueue index (where the next TRB will be written).
    enqueue_idx: usize,
    /// Current dequeue index (where the consumer reads next).
    dequeue_idx: usize,
    /// Producer cycle state (toggled on wrap).
    cycle_state: bool,
    /// Whether this ring is currently running (started by software).
    running: bool,
    /// Guest physical address of this ring in guest memory.
    guest_base_addr: u64,
}

impl TrbRing {
    /// Create a new TRB ring with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: vec![Trb::default(); capacity],
            enqueue_idx: 0,
            dequeue_idx: 0,
            cycle_state: true,
            running: false,
            guest_base_addr: 0,
        }
    }

    /// Create a ring with the default size.
    #[must_use]
    pub fn with_default_size() -> Self {
        Self::new(DEFAULT_RING_SIZE)
    }

    /// Set the guest physical base address of this ring.
    pub const fn set_base_addr(&mut self, addr: u64) {
        self.guest_base_addr = addr;
    }

    /// Set the consumer/producer cycle state (Set TR Dequeue Pointer's DCS).
    pub const fn set_cycle_state(&mut self, cycle: bool) {
        self.cycle_state = cycle;
    }

    /// Get the guest physical base address.
    #[must_use]
    pub const fn base_addr(&self) -> u64 {
        self.guest_base_addr
    }

    /// Start the ring (enable processing).
    pub const fn start(&mut self) {
        self.running = true;
    }

    /// Stop the ring (disable processing).
    pub const fn stop(&mut self) {
        self.running = false;
    }

    /// Check if the ring is running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// Enqueue a TRB at the current enqueue position.
    ///
    /// Returns `true` if successful, `false` if the ring is full.
    pub fn enqueue(&mut self, mut trb: Trb) -> bool {
        if self.is_full() {
            return false;
        }
        // Set the cycle bit to match producer state.
        trb.set_cycle(self.cycle_state);
        self.entries[self.enqueue_idx] = trb;
        self.advance_enqueue();
        true
    }

    /// Dequeue a TRB from the current dequeue position.
    ///
    /// Returns `None` if the ring is empty.
    pub fn dequeue(&mut self) -> Option<Trb> {
        if self.is_empty() {
            return None;
        }
        let trb = self.entries[self.dequeue_idx];
        self.advance_dequeue();
        Some(trb)
    }

    /// Peek at the next TRB to be dequeued without removing it.
    #[must_use]
    pub fn peek(&self) -> Option<&Trb> {
        if self.is_empty() {
            return None;
        }
        Some(&self.entries[self.dequeue_idx])
    }

    /// Check if the ring is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.enqueue_idx == self.dequeue_idx
    }

    /// Check if the ring is full.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        (self.enqueue_idx + 1) % self.entries.len() == self.dequeue_idx
    }

    /// Number of TRBs currently enqueued.
    #[must_use]
    pub const fn len(&self) -> usize {
        if self.enqueue_idx >= self.dequeue_idx {
            self.enqueue_idx - self.dequeue_idx
        } else {
            self.entries.len() - self.dequeue_idx + self.enqueue_idx
        }
    }

    /// Ring capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// Current cycle state.
    #[must_use]
    pub const fn cycle_state(&self) -> bool {
        self.cycle_state
    }

    /// Current enqueue index.
    #[must_use]
    pub const fn enqueue_index(&self) -> usize {
        self.enqueue_idx
    }

    /// Current dequeue index.
    #[must_use]
    pub const fn dequeue_index(&self) -> usize {
        self.dequeue_idx
    }

    /// Reset the ring to its initial state.
    pub fn reset(&mut self) {
        self.enqueue_idx = 0;
        self.dequeue_idx = 0;
        self.cycle_state = true;
        self.running = false;
        for trb in &mut self.entries {
            *trb = Trb::default();
        }
    }

    /// Advance the enqueue pointer, toggling cycle state on wrap.
    const fn advance_enqueue(&mut self) {
        self.enqueue_idx += 1;
        if self.enqueue_idx >= self.entries.len() {
            self.enqueue_idx = 0;
            self.cycle_state = !self.cycle_state;
        }
    }

    /// Advance the dequeue pointer.
    const fn advance_dequeue(&mut self) {
        self.dequeue_idx += 1;
        if self.dequeue_idx >= self.entries.len() {
            self.dequeue_idx = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Command Ring
// ---------------------------------------------------------------------------

/// The xHCI Command Ring — used by software to issue commands to the controller.
///
/// Commands include: Enable Slot, Address Device, Configure Endpoint,
/// Evaluate Context, Reset Endpoint, Stop Endpoint, Set TR Dequeue Pointer,
/// Reset Device, No Op.
///
/// The controller reads the Command Ring base address from the CRCR register.
#[derive(Debug)]
pub struct CommandRing {
    /// Underlying TRB ring.
    ring: TrbRing,
    /// Abort flag — set when software writes to CRCR to abort a command.
    abort_pending: bool,
}

impl CommandRing {
    /// Create a new command ring.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            ring: TrbRing::new(capacity),
            abort_pending: false,
        }
    }

    /// Create with default capacity.
    #[must_use]
    pub fn with_default_size() -> Self {
        Self::new(DEFAULT_RING_SIZE)
    }

    /// Submit a command TRB.
    pub fn submit(&mut self, trb: Trb) -> bool {
        self.ring.enqueue(trb)
    }

    /// Fetch the next command for processing.
    pub fn fetch(&mut self) -> Option<Trb> {
        self.ring.dequeue()
    }

    /// Set the abort flag (triggered by CRCR write with abort bit).
    pub const fn request_abort(&mut self) {
        self.abort_pending = true;
    }

    /// Check and clear the abort flag.
    pub const fn take_abort(&mut self) -> bool {
        let was = self.abort_pending;
        self.abort_pending = false;
        was
    }

    /// Access the underlying ring.
    #[must_use]
    pub const fn ring(&self) -> &TrbRing {
        &self.ring
    }

    /// Mutable access to the underlying ring.
    pub const fn ring_mut(&mut self) -> &mut TrbRing {
        &mut self.ring
    }

    /// Reset the command ring.
    pub fn reset(&mut self) {
        self.ring.reset();
        self.abort_pending = false;
    }
}

// ---------------------------------------------------------------------------
// Transfer Ring
// ---------------------------------------------------------------------------

/// A per-endpoint Transfer Ring — used to schedule data transfers.
///
/// Each device slot + endpoint pair has its own transfer ring. Software
/// enqueues Normal TRBs (and other transfer types like Setup/Data/Status
/// for control endpoints), then rings the doorbell to notify the controller.
#[derive(Debug)]
pub struct TransferRing {
    /// Underlying TRB ring.
    ring: TrbRing,
    /// Slot ID this transfer ring belongs to.
    slot_id: u8,
    /// Endpoint index (1-based, as per xHCI spec: 1 = EP0 OUT, 2 = EP0 IN, ...).
    endpoint_id: u8,
    /// Whether this endpoint is currently halted.
    halted: bool,
}

impl TransferRing {
    /// Create a new transfer ring for the given slot and endpoint.
    #[must_use]
    pub fn new(slot_id: u8, endpoint_id: u8, capacity: usize) -> Self {
        Self {
            ring: TrbRing::new(capacity),
            slot_id,
            endpoint_id,
            halted: false,
        }
    }

    /// Create with default capacity.
    #[must_use]
    pub fn with_default_size(slot_id: u8, endpoint_id: u8) -> Self {
        Self::new(slot_id, endpoint_id, DEFAULT_RING_SIZE)
    }

    /// Slot ID.
    #[must_use]
    pub const fn slot_id(&self) -> u8 {
        self.slot_id
    }

    /// Endpoint ID.
    #[must_use]
    pub const fn endpoint_id(&self) -> u8 {
        self.endpoint_id
    }

    /// Enqueue a transfer TRB.
    pub fn enqueue(&mut self, trb: Trb) -> bool {
        if self.halted {
            return false;
        }
        self.ring.enqueue(trb)
    }

    /// Dequeue the next transfer TRB for processing.
    pub fn dequeue(&mut self) -> Option<Trb> {
        self.ring.dequeue()
    }

    /// Halt this endpoint (e.g., on a STALL or error).
    pub const fn halt(&mut self) {
        self.halted = true;
        self.ring.stop();
    }

    /// Clear the halt condition (after Reset Endpoint command).
    pub const fn clear_halt(&mut self) {
        self.halted = false;
        self.ring.start();
    }

    /// Check if this endpoint is halted.
    #[must_use]
    pub const fn is_halted(&self) -> bool {
        self.halted
    }

    /// Access the underlying ring.
    #[must_use]
    pub const fn ring(&self) -> &TrbRing {
        &self.ring
    }

    /// Mutable access.
    pub const fn ring_mut(&mut self) -> &mut TrbRing {
        &mut self.ring
    }

    /// Reset the transfer ring.
    pub fn reset(&mut self) {
        self.ring.reset();
        self.halted = false;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::xhci::trb::TrbType;

    #[test]
    fn trb_ring_enqueue_dequeue() {
        let mut ring = TrbRing::new(4);
        assert!(ring.is_empty());

        let trb = Trb::new(TrbType::Normal);
        assert!(ring.enqueue(trb));
        assert_eq!(ring.len(), 1);

        let out = ring.dequeue().unwrap();
        assert_eq!(out.decoded_type(), TrbType::Normal);
        assert!(ring.is_empty());
    }

    #[test]
    fn trb_ring_full() {
        let mut ring = TrbRing::new(3);
        assert!(ring.enqueue(Trb::new(TrbType::Normal)));
        assert!(ring.enqueue(Trb::new(TrbType::Normal)));
        // Ring of size 3 holds 2 entries (one slot is sentinel).
        assert!(ring.is_full());
        assert!(!ring.enqueue(Trb::new(TrbType::Normal)));
    }

    #[test]
    fn trb_ring_wrap_around() {
        let mut ring = TrbRing::new(4);
        assert!(ring.cycle_state());

        // Fill ring (3 of 4 slots usable due to sentinel), drain, then
        // enqueue one more to push enqueue_idx past capacity → wrap + toggle.
        for _ in 0..3 {
            ring.enqueue(Trb::new(TrbType::Normal));
        }
        for _ in 0..3 {
            ring.dequeue();
        }
        // enqueue_idx = 3, dequeue_idx = 3 — ring is empty.
        // One more enqueue pushes enqueue_idx to 4 → wraps to 0.
        ring.enqueue(Trb::new(TrbType::Normal));

        // Cycle should have toggled on wrap.
        assert!(!ring.cycle_state());
    }

    #[test]
    fn trb_ring_reset() {
        let mut ring = TrbRing::new(4);
        ring.enqueue(Trb::new(TrbType::Normal));
        ring.start();
        ring.reset();
        assert!(ring.is_empty());
        assert!(!ring.is_running());
        assert!(ring.cycle_state());
    }

    #[test]
    fn command_ring_submit_fetch() {
        let mut cmd = CommandRing::with_default_size();
        let trb = Trb::new(TrbType::EnableSlotCommand);
        assert!(cmd.submit(trb));

        let out = cmd.fetch().unwrap();
        assert_eq!(out.decoded_type(), TrbType::EnableSlotCommand);
    }

    #[test]
    fn command_ring_abort() {
        let mut cmd = CommandRing::with_default_size();
        assert!(!cmd.take_abort());
        cmd.request_abort();
        assert!(cmd.take_abort());
        assert!(!cmd.take_abort()); // Cleared after take.
    }

    #[test]
    fn transfer_ring_halt() {
        let mut tr = TransferRing::with_default_size(1, 1);
        assert!(tr.enqueue(Trb::new(TrbType::Normal)));

        tr.halt();
        assert!(tr.is_halted());
        assert!(!tr.enqueue(Trb::new(TrbType::Normal)));

        tr.clear_halt();
        assert!(!tr.is_halted());
        assert!(tr.enqueue(Trb::new(TrbType::Normal)));
    }

    #[test]
    fn transfer_ring_identity() {
        let tr = TransferRing::with_default_size(3, 5);
        assert_eq!(tr.slot_id(), 3);
        assert_eq!(tr.endpoint_id(), 5);
    }

    #[test]
    fn trb_ring_peek() {
        let mut ring = TrbRing::new(4);
        assert!(ring.peek().is_none());

        ring.enqueue(Trb::new(TrbType::Normal));
        let peeked = ring.peek().unwrap();
        assert_eq!(peeked.decoded_type(), TrbType::Normal);
        // Peek doesn't consume.
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn trb_ring_base_addr() {
        let mut ring = TrbRing::new(4);
        ring.set_base_addr(0xDEAD_0000);
        assert_eq!(ring.base_addr(), 0xDEAD_0000);
    }
}
