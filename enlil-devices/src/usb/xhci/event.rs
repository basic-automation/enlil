//! xHCI Event Ring and Interrupter Register Set.
//!
//! The Event Ring is the controller-to-software notification mechanism.
//! Unlike command/transfer rings (which are producer-driven by software),
//! the event ring is producer-driven by the controller. Software is the
//! consumer: it reads event TRBs and advances the dequeue pointer.
//!
//! Each interrupter has its own Event Ring Segment Table (ERST) that
//! maps the ring segments in guest physical memory.

use super::transfer::DmaMemory;
use super::trb::{Trb, TrbCompletionCode, TrbType};
use crate::truncate::u16_of;
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
        let segment = EventRingSegment::new(0, u16_of(segment_size));
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
        trb.status = (transfer_length & 0xFF_FFFF) | ((completion_code as u32) << 24);
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
        trb.control = (u32::from(slot_id) << 24) | ((TrbType::CommandCompletionEvent as u32) << 10);
        self.post_event(trb)
    }

    /// Post a port status change event.
    pub fn post_port_status_change(
        &mut self,
        port_id: u8,
        completion_code: TrbCompletionCode,
    ) -> bool {
        let mut trb = Trb::zeroed();
        trb.parameter = u64::from(port_id) << 24;
        trb.status = (completion_code as u32) << 24;
        trb.control = (TrbType::PortStatusChangeEvent as u32) << 10;
        self.post_event(trb)
    }

    /// Advance the enqueue pointer, wrapping and toggling cycle state as needed.
    const fn advance_enqueue(&mut self) {
        self.enqueue_idx += 1;
        if self.enqueue_idx >= self.capacity {
            self.enqueue_idx = 0;
            self.cycle_state = !self.cycle_state;
        }
    }

    /// Check if the ring is full (enqueue would overwrite unread events).
    #[must_use]
    pub const fn is_full(&self) -> bool {
        let next = if self.enqueue_idx + 1 >= self.capacity {
            0
        } else {
            self.enqueue_idx + 1
        };
        next == self.dequeue_idx
    }

    /// Check if the ring is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.enqueue_idx == self.dequeue_idx
    }

    /// Number of pending (unread) events.
    #[must_use]
    pub const fn pending_count(&self) -> usize {
        if self.enqueue_idx >= self.dequeue_idx {
            self.enqueue_idx - self.dequeue_idx
        } else {
            self.capacity - self.dequeue_idx + self.enqueue_idx
        }
    }

    /// Update the dequeue pointer (called when guest advances ERDP).
    pub const fn set_dequeue_index(&mut self, idx: usize) {
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
// Guest-memory event ring writer
// ---------------------------------------------------------------------------

/// One ERST entry is 16 bytes: segment base (64-byte aligned), segment size
/// in TRBs, reserved (xHCI §6.5).
const ERST_ENTRY_SIZE: u64 = 16;

/// The controller-side writer for an event ring resident in guest memory.
///
/// Built from the Event Ring Segment Table the guest's driver programmed
/// through `ERSTBA`/`ERSTSZ` (xHCI §6.5): events are written segment by
/// segment at the enqueue position, the Producer Cycle State starts at 1
/// and toggles each time the write position wraps off the last segment
/// back to the first (§4.9.4), and the ring is full when advancing would
/// reach the TRB `ERDP` says the driver has not consumed yet.
#[derive(Debug)]
pub struct GuestEventRing {
    /// The `ERSTBA` this table was read from (for change detection).
    erstba: u64,
    /// The `ERSTSZ` in force when the table was read.
    erstsz: u32,
    /// The decoded segment table.
    segments: Vec<EventRingSegment>,
    /// Segment the enqueue position is in.
    segment_index: usize,
    /// TRB index within that segment.
    trb_index: usize,
    /// Producer Cycle State.
    cycle: bool,
}

impl GuestEventRing {
    /// Decode the ERST at `erstba` with `erstsz` entries out of guest
    /// memory. `None` for an empty/unreadable table or a segment size
    /// outside the spec's 16–4096 TRBs — the driver misprogrammed the
    /// ring, so events stay queued internally.
    #[must_use]
    pub fn load(mem: &dyn DmaMemory, erstba: u64, erstsz: u32) -> Option<Self> {
        if erstsz == 0 {
            return None;
        }
        let mut segments = Vec::new();
        for i in 0..u64::from(erstsz) {
            let mut entry = [0_u8; 16];
            if !mem.read(erstba + i * ERST_ENTRY_SIZE, &mut entry) {
                return None;
            }
            let base = u64::from_le_bytes(entry[0..8].try_into().ok()?) & !0x3F;
            let size = u16::from_le_bytes([entry[8], entry[9]]);
            if !(16..=4096).contains(&size) {
                return None;
            }
            segments.push(EventRingSegment::new(base, size));
        }
        Some(Self {
            erstba,
            erstsz,
            segments,
            segment_index: 0,
            trb_index: 0,
            cycle: true,
        })
    }

    /// Whether this table was built from the given `ERSTBA`/`ERSTSZ` (if
    /// not, the driver reprogrammed the ring and the table must be
    /// reloaded).
    #[must_use]
    pub const fn matches(&self, erstba: u64, erstsz: u32) -> bool {
        self.erstba == erstba && self.erstsz == erstsz
    }

    /// Guest physical address of the slot at (`segment`, `index`).
    fn address_of(&self, segment: usize, index: usize) -> u64 {
        use crate::truncate::Widen;
        self.segments[segment].base_address + index.to_u64() * 16
    }

    /// The position after (`segment`, `index`), wrapping off the last
    /// segment to the first.
    fn position_after(&self, segment: usize, index: usize) -> (usize, usize) {
        if index + 1 < usize::from(self.segments[segment].size) {
            (segment, index + 1)
        } else if segment + 1 < self.segments.len() {
            (segment + 1, 0)
        } else {
            (0, 0)
        }
    }

    /// Write one event TRB at the enqueue position with the Producer Cycle
    /// State, then advance. `false` — and no write — if the ring is full
    /// (the next position is the TRB `erdp` points at) or the segment is
    /// unbacked; the event stays queued for a later flush.
    pub fn write_event(&mut self, mem: &mut dyn DmaMemory, mut trb: Trb, erdp: u64) -> bool {
        let (next_segment, next_index) = self.position_after(self.segment_index, self.trb_index);
        if self.address_of(next_segment, next_index) == erdp & !0xF {
            return false;
        }
        trb.set_cycle_bit(self.cycle);
        if !mem.write(
            self.address_of(self.segment_index, self.trb_index),
            &trb.to_bytes(),
        ) {
            return false;
        }
        if (next_segment, next_index) == (0, 0) {
            self.cycle = !self.cycle;
        }
        self.segment_index = next_segment;
        self.trb_index = next_index;
        true
    }

    /// Current Producer Cycle State.
    #[must_use]
    pub const fn cycle_state(&self) -> bool {
        self.cycle
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
    /// The guest-memory writer built from the driver's ERST (`None` until
    /// the first flush after `ERSTBA`/`ERSTSZ` are programmed).
    guest_ring: Option<GuestEventRing>,
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
            guest_ring: None,
        }
    }

    /// Drain internally queued events into the guest-memory event ring the
    /// driver described through `ERSTBA`/`ERSTSZ`, returning how many were
    /// delivered. A no-op until the segment table is programmed (events
    /// then stay on the internal queue, where tests pop them directly);
    /// delivery stops at a full guest ring — the rest stay queued for the
    /// next flush.
    pub fn flush_to_guest(&mut self, mem: &mut dyn DmaMemory) -> usize {
        if self.erstba == 0 {
            return 0;
        }
        if !self
            .guest_ring
            .as_ref()
            .is_some_and(|ring| ring.matches(self.erstba, self.erstsz))
        {
            self.guest_ring = GuestEventRing::load(mem, self.erstba, self.erstsz);
        }
        let Some(ring) = &mut self.guest_ring else {
            return 0;
        };
        let mut delivered = 0;
        while !self.event_ring.is_empty() {
            let index = self.event_ring.dequeue_index();
            let Some(trb) = self.event_ring.read_trb(index).copied() else {
                break;
            };
            if !ring.write_event(mem, trb, self.erdp) {
                break;
            }
            self.event_ring
                .set_dequeue_index((index + 1) % self.event_ring.capacity());
            delivered += 1;
        }
        delivered
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
    pub const fn set_pending(&mut self, pending: bool) {
        if pending {
            self.iman |= 1;
        } else {
            self.iman &= !1;
        }
    }

    /// Write to the IMAN register (write-1-to-clear for IP bit).
    pub const fn write_iman(&mut self, value: u32) {
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
    pub const fn write_erdp(&mut self, value: u64) {
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
        self.guest_ring = None;
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
        assert!(ring.post_transfer_event(0x1000, 512, TrbCompletionCode::Success, 1, 2,));
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

    use super::super::transfer::VecDmaMemory;

    /// Write an ERST at `erstba` describing the given (base, size) segments.
    fn write_erst(mem: &mut VecDmaMemory, erstba: u64, segments: &[(u64, u16)]) {
        use crate::truncate::Widen;
        for (i, (base, size)) in segments.iter().enumerate() {
            let offset = erstba + i.to_u64() * ERST_ENTRY_SIZE;
            assert!(mem.write(offset, &base.to_le_bytes()));
            assert!(mem.write(offset + 8, &size.to_le_bytes()));
        }
    }

    /// The raw TRB written at guest `addr`.
    fn trb_at(mem: &VecDmaMemory, addr: u64) -> Trb {
        let mut bytes = [0_u8; 16];
        assert!(mem.read(addr, &mut bytes));
        Trb::from_bytes(&bytes)
    }

    #[test]
    fn guest_ring_load_validates_the_segment_table() {
        let mut mem = VecDmaMemory::new(0x1000, 0x1000);
        write_erst(&mut mem, 0x1000, &[(0x1100, 16), (0x1300, 16)]);
        let ring = GuestEventRing::load(&mem, 0x1000, 2).unwrap();
        assert!(ring.matches(0x1000, 2));
        assert!(!ring.matches(0x1000, 1));

        // Zero entries, an unbacked table, or an out-of-spec segment size
        // all refuse to load.
        assert!(GuestEventRing::load(&mem, 0x1000, 0).is_none());
        assert!(GuestEventRing::load(&mem, 0xDEAD_0000, 1).is_none());
        write_erst(&mut mem, 0x1800, &[(0x1100, 8)]); // < 16 TRBs
        assert!(GuestEventRing::load(&mem, 0x1800, 1).is_none());
    }

    #[test]
    fn guest_ring_writes_wrap_segments_and_toggle_cycle() {
        let mut mem = VecDmaMemory::new(0x1000, 0x1000);
        write_erst(&mut mem, 0x1000, &[(0x1100, 16), (0x1300, 16)]);
        let mut ring = GuestEventRing::load(&mem, 0x1000, 2).unwrap();
        let erdp = 0x1100; // driver parked at the first slot

        // 31 writes fill both segments except the slot before ERDP.
        let mut event = Trb::zeroed();
        event.control = (TrbType::PortStatusChangeEvent as u32) << 10;
        for _ in 0..31 {
            assert!(ring.write_event(&mut mem, event, erdp));
        }
        assert!(
            !ring.write_event(&mut mem, event, erdp),
            "the slot ERDP points at is never overwritten"
        );

        // First slot of each segment carries the event with PCS = 1.
        for addr in [0x1100_u64, 0x1300] {
            let trb = trb_at(&mem, addr);
            assert_eq!(trb.decoded_type(), TrbType::PortStatusChangeEvent);
            assert_eq!(trb.control & 1, 1, "PCS 1 on the first lap");
        }

        // The driver consumes everything (ERDP = the one unwritten slot):
        // the next write fills the last slot — still PCS 1 — and wraps the
        // position to segment 0 slot 0, toggling PCS; the write after that
        // lands there with PCS 0.
        let erdp = 0x1300 + 15 * 16;
        assert!(ring.write_event(&mut mem, event, erdp));
        assert_eq!(trb_at(&mem, erdp).control & 1, 1, "last slot of lap one");
        assert!(!ring.cycle_state());
        assert!(ring.write_event(&mut mem, event, erdp));
        let wrapped = trb_at(&mem, 0x1100);
        assert_eq!(wrapped.control & 1, 0, "PCS 0 on the second lap");
    }

    #[test]
    fn flush_to_guest_delivers_queued_events_and_reloads_on_reprogram() {
        let mut mem = VecDmaMemory::new(0x1000, 0x1000);
        write_erst(&mut mem, 0x1000, &[(0x1100, 16)]);
        let mut ir = InterrupterRegisterSet::new();

        // Events queue internally while ERSTBA is unprogrammed.
        assert!(
            ir.event_ring
                .post_port_status_change(1, TrbCompletionCode::Success)
        );
        assert_eq!(ir.flush_to_guest(&mut mem), 0);

        ir.erstsz = 1;
        ir.erstba = 0x1000;
        ir.erdp = 0x1100;
        assert_eq!(ir.flush_to_guest(&mut mem), 1);
        assert!(ir.event_ring.is_empty(), "the internal queue drained");
        let trb = trb_at(&mem, 0x1100);
        assert_eq!(trb.decoded_type(), TrbType::PortStatusChangeEvent);

        // Reprogramming the table rebuilds the writer from the new ERST.
        write_erst(&mut mem, 0x1800, &[(0x1500, 16)]);
        ir.erstba = 0x1800;
        ir.erdp = 0x1500;
        assert!(
            ir.event_ring
                .post_command_completion(0, TrbCompletionCode::Success, 1)
        );
        assert_eq!(ir.flush_to_guest(&mut mem), 1);
        assert_eq!(
            trb_at(&mem, 0x1500).decoded_type(),
            TrbType::CommandCompletionEvent
        );
    }
}
