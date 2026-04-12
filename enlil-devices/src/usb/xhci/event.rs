//! xHCI Event Ring and Interrupter Register Set.
//!
//! The Event Ring is the controller-to-software notification mechanism.
//! Unlike command/transfer rings (which are producer-driven by software),
//! the event ring is producer-driven by the controller. Software is the
//! consumer: it reads event TRBs and advances the dequeue pointer.
//!
//! Each interrupter has its own Event Ring Segment Table (ERST) that
//! maps the ring segments in guest physical memory.

use super::trb::{Trb, TrbCompletionCode, TrbType};
use std::fmt;

// ---------------------------------------------------------------------------
// Event Ring Segment Table Entry
// ---------------------------------------------------------------------------

/// A single entry in the Event Ring Segment Table (ERST).
///
/// Each entry describes one contiguous segment of TRBs in memory.
#[derive(Debug, Clone, Copy)]
pub struct EventRingSegment {
    /// Base address of the segment (64-byte aligned).
    pub base_address: u64,
    /// Number of TRBs in this segment (16–4096).
    pub size: u16,
}

impl EventRingSegment {
    /// Create a new segment entry.
    #[must_use]
    pub const fn new(base_address: u64, size: u16) -> Self {
        Self { base_address, size }
    }
}

// ---------------------------------------------------------------------------
// Event Ring
// ---------------------------------------------------------------------------

/// An Event Ring managed by the virtual xHCI controller.
///
/// The controller (our emulation code) enqueues event TRBs here.
/// The guest driver dequeues them and updates the Event Ring Dequeue Pointer
/// in the interrupter register set.
#[derive(Debug)]
pub struct EventRing {
    /// The segment table entries describing this ring's memory layout.
    segments: Vec<EventRingSegment>,
    /// Current event TRB storage (flattened across all segments).
    entries: Vec<Trb>,
    /// Current enqueue index (controller writes here).
    enqueue_idx: usize,
    /// Current dequeue index (guest reads from here).
    dequeue_idx: usize,
    /// Producer cycle state — toggled on wrap.
    cycle_state: bool,
    /// Total capacity across all segments.
    capacity: usize,
}

impl EventRing {
    /// Create a new event ring with a single segment of the given size.
    #[must_use]
    pub fn new(segment_size: usize) -> Self {
        let segment = EventRingSegment::new(0, segment_size as u16);
        Self {
            segments: vec![segment],
            entries: vec![Trb::zeroed(); segment_size],
            enqueue_idx: 0,
            dequeue_idx: 0,
            cycle_state: true,
            capacity: segment_size,
        }
    }

    /// Create an event ring from a segment table.
    #[must_use]
    pub fn from_segments(segments: Vec<EventRingSegment>) -> Self {
        let capacity: usize = segments.iter().map(|s| usize::from(s.size)).sum();
        Self {
            segments,
            entries: vec![Trb::zeroed(); capacity],
            enqueue_idx: 0,
            dequeue_idx: 0,
            cycle_state: true,
            capacity,
        }
    }

    /// Post an event TRB to the ring (controller side).
    ///
    /// Returns `true` if the event was posted, `false` if the ring is full.
    pub fn post_event(&mut self, mut trb: Trb) -> bool {
        if self.is_full() {
            return false;
        }
        trb.set_cycle_bit(self.cycle_state);
        self.entries[self.enqueue_idx] = trb;
        self.advance_enqueue();
        true
    }

    /// Post a transfer completion event.
    pub fn post_transfer_event(
        &mut self,
        trb_pointer: u64,
        transfer_length: u32,
        completion_code: TrbCompletionCode,
        slot_id: u8,
        endpoint_id: u8,
    ) -> bool {
        let mut trb = Trb::zeroed();
        trb.parameter = trb_pointer;
        trb.status = (transfer_length & 0xFF_FFFF)
            | ((completion_code as u32) << 24);
        trb.control = (u32::from(slot_id) << 24)
            | (u32::from(endpoint_id) << 16)
            | ((TrbType::TransferEvent as u32) << 10);
        self.post_event(trb)
    }

    /// Post a command completion event.
    pub fn post_command_completion(
        &mut self,
        trb_pointer: u64,
        completion_code: TrbCompletionCode,
        slot_id: u8,
    ) -> bool {
        let mut trb = Trb::zeroed();
        trb.parameter = trb_pointer;
        trb.status = (completion_code as u32) << 24;
        trb.control = (u32::from(slot_id) << 24)
            | ((TrbType::CommandCompletionEvent as u32) << 10);
        self.post_event(trb)
    }

    /// Post a port status change event.
    pub fn post_port_status_change(&mut self, port_id: u8, completion_code: TrbCompletionCode) -> bool {
        let mut trb = Trb::zeroed();
        trb.parameter = u64::from(port_id) << 24;
        trb.status = (completion_code as u32) << 24;
        trb.control = (TrbType::PortStatusChangeEvent as u32) << 10;
        self.post_event(trb)
    }

    /// Advance the enqueue pointer, wrapping and toggling cycle state as needed.
    fn advance_enqueue(&mut self) {
        self.enqueue_idx += 1;
        if self.enqueue_idx >= self.capacity {
            self.enqueue_idx = 0;
            self.cycle_state = !self.cycle_state;
        }
    }

    /// Check if the ring is full (enqueue would overwrite unread events).
    #[must_use]
    pub fn is_full(&self) -> bool {
        let next = if self.enqueue_idx + 1 >= self.capacity {
            0
        } else {
            self.enqueue_idx + 1
        };
        next == self.dequeue_idx
    }

    /// Check if the ring is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.enqueue_idx == self.dequeue_idx
    }

    /// Number of pending (unread) events.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        if self.enqueue_idx >= self.dequeue_idx {
            self.enqueue_idx - self.dequeue_idx
        } else {
            self.capacity - self.dequeue_idx + self.enqueue_idx
        }
    }

    /// Update the dequeue pointer (called when guest advances ERDP).
    pub fn set_dequeue_index(&mut self, idx: usize) {
        if idx < self.capacity {
            self.dequeue_idx = idx;
        }
    }

    /// Get the current dequeue index.
    #[must_use]
    pub const fn dequeue_index(&self) -> usize {
        self.dequeue_idx
    }

    /// Get the current enqueue index.
    #[must_use]
    pub const fn enqueue_index(&self) -> usize {
        self.enqueue_idx
    }

    /// Get the cycle state.
    #[must_use]
    pub const fn cycle_state(&self) -> bool {
        self.cycle_state
    }

    /// Read the TRB at a given index.
    #[must_use]
    pub fn read_trb(&self, idx: usize) -> Option<&Trb> {
        self.entries.get(idx)
    }

    /// Get the segment table.
    #[must_use]
    pub fn segments(&self) -> &[EventRingSegment] {
        &self.segments
    }

    /// Total ring capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Reset the ring to initial state.
    pub fn reset(&mut self) {
        self.enqueue_idx = 0;
        self.dequeue_idx = 0;
        self.cycle_state = true;
        for trb in &mut self.entries {
            *trb = Trb::zeroed();
        }
    }
}

// ---------------------------------------------------------------------------
// Interrupter Register Set (xHCI 5.5.2)
// ---------------------------------------------------------------------------

/// Virtual Interrupter Register Set.
///
/// Each interrupter manages one Event Ring and controls interrupt delivery
/// to the guest. xHCI supports up to 1024 interrupters; we typically use 1.
#[derive(Debug)]
pub struct InterrupterRegisterSet {
    /// Interrupt Management Register — pending/enable bits.
    pub iman: u32,
    /// Interrupt Moderation Register — interval and counter.
    pub imod: u32,
    /// Event Ring Segment Table Size (number of entries).
    pub erstsz: u32,
    /// Event Ring Segment Table Base Address (64-bit).
    pub erstba: u64,
    /// Event Ring Dequeue Pointer (64-bit, 4-byte aligned).
    pub erdp: u64,
    /// The event ring managed by this interrupter.
    pub event_ring: EventRing,
}

impl InterrupterRegisterSet {
    /// Create a new interrupter with a default event ring size.
    #[must_use]
    pub fn new() -> Self {
        Self {
            iman: 0,
            imod: 0x0000_0FA0, // Default: 4000 * 250ns = 1ms moderation interval
            erstsz: 1,
            erstba: 0,
            erdp: 0,
            event_ring: EventRing::new(256),
        }
    }

    /// Check if interrupts are pending.
    #[must_use]
    pub const fn interrupt_pending(&self) -> bool {
        (self.iman & 1) != 0
    }

    /// Check if interrupts are enabled.
    #[must_use]
    pub const fn interrupt_enabled(&self) -> bool {
        (self.iman & 2) != 0
    }

    /// Set the interrupt pending bit.
    pub fn set_pending(&mut self, pending: bool) {
        if pending {
            self.iman |= 1;
        } else {
            self.iman &= !1;
        }
    }

    /// Write to the IMAN register (write-1-to-clear for IP bit).
    pub fn write_iman(&mut self, value: u32) {
        // Bit 0 (IP): write-1-to-clear
        if value & 1 != 0 {
            self.iman &= !1;
        }
        // Bit 1 (IE): writable
        self.iman = (self.iman & 1) | (value & 2);
    }

    /// Write to the ERDP register.
    ///
    /// Bits [3:0] contain flags (EHB in bit 3), bits [63:4] are the address.
    pub fn write_erdp(&mut self, value: u64) {
        // Clear Event Handler Busy (EHB) if bit 3 is set (write-1-to-clear).
        let ehb_clear = (value & 0x8) != 0;
        self.erdp = value & !0xF; // Store address portion only

        if ehb_clear {
            // Update the event ring dequeue pointer based on the address.
            // In a real implementation, we'd compute the index from the address.
            // For our virtual controller, we track indices directly.
        }
    }

    /// Reset the interrupter to power-on defaults.
    pub fn reset(&mut self) {
        self.iman = 0;
        self.imod = 0x0000_0FA0;
        self.erstsz = 1;
        self.erstba = 0;
        self.erdp = 0;
        self.event_ring.reset();
    }
}

impl Default for InterrupterRegisterSet {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for InterrupterRegisterSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Interrupter[IMAN={:#x}, IMOD={:#x}, ERSTSZ={}, pending={}]",
            self.iman,
            self.imod,
            self.erstsz,
            self.event_ring.pending_count()
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ring_basic() {
        let mut ring = EventRing::new(4);
        assert!(ring.is_empty());
        assert_eq!(ring.pending_count(), 0);

        let trb = Trb::zeroed();
        assert!(ring.post_event(trb));
        assert_eq!(ring.pending_count(), 1);
        assert!(!ring.is_empty());
    }

    #[test]
    fn event_ring_fills() {
        let mut ring = EventRing::new(4);
        // Can post 3 events (capacity - 1 for full detection)
        assert!(ring.post_event(Trb::zeroed()));
        assert!(ring.post_event(Trb::zeroed()));
        assert!(ring.post_event(Trb::zeroed()));
        assert!(ring.is_full());
        assert!(!ring.post_event(Trb::zeroed()));
    }

    #[test]
    fn event_ring_wrap_toggles_cycle() {
        let mut ring = EventRing::new(4);
        assert!(ring.cycle_state());
        ring.post_event(Trb::zeroed());
        ring.post_event(Trb::zeroed());
        ring.post_event(Trb::zeroed());
        // Dequeue to free space
        ring.set_dequeue_index(3);
        ring.post_event(Trb::zeroed()); // wraps
        assert!(!ring.cycle_state());
    }

    #[test]
    fn post_transfer_event() {
        let mut ring = EventRing::new(16);
        assert!(ring.post_transfer_event(
            0x1000,
            512,
            TrbCompletionCode::Success,
            1,
            2,
        ));
        assert_eq!(ring.pending_count(), 1);
    }

    #[test]
    fn post_command_completion() {
        let mut ring = EventRing::new(16);
        assert!(ring.post_command_completion(0x2000, TrbCompletionCode::Success, 3));
        assert_eq!(ring.pending_count(), 1);
    }

    #[test]
    fn post_port_status_change() {
        let mut ring = EventRing::new(16);
        assert!(ring.post_port_status_change(1, TrbCompletionCode::Success));
        assert_eq!(ring.pending_count(), 1);
    }

    #[test]
    fn interrupter_defaults() {
        let ir = InterrupterRegisterSet::new();
        assert!(!ir.interrupt_pending());
        assert!(!ir.interrupt_enabled());
        assert_eq!(ir.imod, 0x0000_0FA0);
    }

    #[test]
    fn interrupter_iman_write_1_to_clear() {
        let mut ir = InterrupterRegisterSet::new();
        ir.set_pending(true);
        assert!(ir.interrupt_pending());
        // Write 1 to bit 0 clears IP, set bit 1 for IE
        ir.write_iman(0x3);
        assert!(!ir.interrupt_pending());
        assert!(ir.interrupt_enabled());
    }

    #[test]
    fn event_ring_reset() {
        let mut ring = EventRing::new(8);
        ring.post_event(Trb::zeroed());
        ring.post_event(Trb::zeroed());
        ring.reset();
        assert!(ring.is_empty());
        assert!(ring.cycle_state());
    }

    #[test]
    fn interrupter_display() {
        let ir = InterrupterRegisterSet::new();
        let s = ir.to_string();
        assert!(s.contains("Interrupter"));
        assert!(s.contains("pending=0"));
    }
}
