//! Doorbell Register Array for xHCI.
//!
//! The doorbell array provides a mechanism for software to notify the xHCI
//! controller that work is available. Writing to a doorbell register "rings"
//! it, signaling the controller to process new TRBs on the specified ring.
//!
//! Doorbell 0 is for the Host Controller (command ring).
//! Doorbells 1–MaxSlots are for device endpoints.

use std::fmt;

// ---------------------------------------------------------------------------
// Doorbell target encoding
// ---------------------------------------------------------------------------

/// Decoded doorbell target — indicates which ring has new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoorbellTarget {
    /// Host controller command ring (doorbell 0, target 0).
    HostCommand,
    /// Control endpoint 0 for a device slot.
    ControlEndpoint { slot_id: u8 },
    /// A specific endpoint for a device slot.
    Endpoint { slot_id: u8, endpoint_id: u8 },
    /// Stream transfer ring for a device endpoint.
    Stream {
        slot_id: u8,
        endpoint_id: u8,
        stream_id: u16,
    },
}

impl DoorbellTarget {
    /// Decode a doorbell write into a target.
    ///
    /// - `doorbell_index`: which doorbell register (0 = HC, 1–N = slot)
    /// - `value`: the 32-bit value written to the doorbell register
    ///   - Bits [7:0]: DB Target (endpoint ID or 0 for command ring)
    ///   - Bits [31:16]: DB Stream ID
    #[must_use]
    pub const fn decode(doorbell_index: u8, value: u32) -> Self {
        let target = (value & 0xFF) as u8;
        let stream_id = ((value >> 16) & 0xFFFF) as u16;

        if doorbell_index == 0 {
            return Self::HostCommand;
        }

        let slot_id = doorbell_index;
        if target == 0 {
            Self::ControlEndpoint { slot_id }
        } else if stream_id != 0 {
            Self::Stream {
                slot_id,
                endpoint_id: target,
                stream_id,
            }
        } else {
            Self::Endpoint {
                slot_id,
                endpoint_id: target,
            }
        }
    }
}

impl fmt::Display for DoorbellTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostCommand => write!(f, "HC Command Ring"),
            Self::ControlEndpoint { slot_id } => write!(f, "Slot {slot_id} EP0"),
            Self::Endpoint {
                slot_id,
                endpoint_id,
            } => {
                write!(f, "Slot {slot_id} EP{endpoint_id}")
            }
            Self::Stream {
                slot_id,
                endpoint_id,
                stream_id,
            } => {
                write!(f, "Slot {slot_id} EP{endpoint_id} Stream {stream_id}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Doorbell Array
// ---------------------------------------------------------------------------

/// The doorbell register array — one 32-bit register per device slot + 1 for HC.
///
/// When software writes to a doorbell register, the controller should check
/// the corresponding ring for new TRBs. In our virtual controller, we capture
/// these writes and process the TRBs from guest memory.
#[derive(Debug)]
pub struct DoorbellArray {
    /// Doorbell register values. Index 0 = HC, `1..=max_slots` = device slots.
    registers: Vec<u32>,
    /// Pending doorbell rings (slot indices that have been written since last check).
    pending: Vec<bool>,
    /// Maximum number of device slots (not counting doorbell 0).
    max_slots: u8,
}

impl DoorbellArray {
    /// Create a new doorbell array for the given number of device slots.
    ///
    /// Total doorbells = `max_slots` + 1 (doorbell 0 is for the HC).
    #[must_use]
    pub fn new(max_slots: u8) -> Self {
        let count = usize::from(max_slots) + 1;
        Self {
            registers: vec![0; count],
            pending: vec![false; count],
            max_slots,
        }
    }

    /// Write a value to a doorbell register.
    ///
    /// Returns the decoded target if the doorbell index is valid, or `None`
    /// if the index is out of range.
    pub fn write(&mut self, index: u8, value: u32) -> Option<DoorbellTarget> {
        let idx = usize::from(index);
        if idx >= self.registers.len() {
            return None;
        }
        self.registers[idx] = value;
        self.pending[idx] = true;
        Some(DoorbellTarget::decode(index, value))
    }

    /// Read a doorbell register (always returns 0 per xHCI spec — doorbells are write-only).
    #[must_use]
    pub const fn read(&self, _index: u8) -> u32 {
        0
    }

    /// Check if a specific doorbell has been rung since last clear.
    #[must_use]
    pub fn is_pending(&self, index: u8) -> bool {
        self.pending
            .get(usize::from(index))
            .copied()
            .unwrap_or(false)
    }

    /// Clear the pending state for a doorbell.
    pub fn clear_pending(&mut self, index: u8) {
        if let Some(p) = self.pending.get_mut(usize::from(index)) {
            *p = false;
        }
    }

    /// Drain all pending doorbells, returning their targets.
    pub fn drain_pending(&mut self) -> Vec<DoorbellTarget> {
        let mut targets = Vec::new();
        for i in 0..self.pending.len() {
            if self.pending[i] {
                self.pending[i] = false;
                let value = self.registers[i];
                targets.push(DoorbellTarget::decode(i as u8, value));
            }
        }
        targets
    }

    /// Number of doorbells in the array.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.registers.len()
    }

    /// Maximum device slot number.
    #[must_use]
    pub const fn max_slots(&self) -> u8 {
        self.max_slots
    }

    /// Reset all doorbells.
    pub fn reset(&mut self) {
        self.registers.fill(0);
        self.pending.fill(false);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_host_command() {
        let target = DoorbellTarget::decode(0, 0);
        assert_eq!(target, DoorbellTarget::HostCommand);
    }

    #[test]
    fn decode_control_endpoint() {
        let target = DoorbellTarget::decode(1, 0);
        assert_eq!(target, DoorbellTarget::ControlEndpoint { slot_id: 1 });
    }

    #[test]
    fn decode_endpoint() {
        // Endpoint ID 3 on slot 2, no stream
        let target = DoorbellTarget::decode(2, 3);
        assert_eq!(
            target,
            DoorbellTarget::Endpoint {
                slot_id: 2,
                endpoint_id: 3
            }
        );
    }

    #[test]
    fn decode_stream() {
        // Endpoint ID 4 on slot 1, stream ID 7
        let value = 4 | (7 << 16);
        let target = DoorbellTarget::decode(1, value);
        assert_eq!(
            target,
            DoorbellTarget::Stream {
                slot_id: 1,
                endpoint_id: 4,
                stream_id: 7
            }
        );
    }

    #[test]
    fn doorbell_array_write_and_pending() {
        let mut db = DoorbellArray::new(8);
        assert!(!db.is_pending(0));
        assert!(!db.is_pending(1));

        let target = db.write(0, 0);
        assert_eq!(target, Some(DoorbellTarget::HostCommand));
        assert!(db.is_pending(0));

        db.clear_pending(0);
        assert!(!db.is_pending(0));
    }

    #[test]
    fn doorbell_array_out_of_range() {
        let mut db = DoorbellArray::new(4);
        assert!(db.write(5, 0).is_none());
    }

    #[test]
    fn doorbell_read_returns_zero() {
        let db = DoorbellArray::new(4);
        assert_eq!(db.read(0), 0);
        assert_eq!(db.read(1), 0);
        assert_eq!(db.read(255), 0);
    }

    #[test]
    fn drain_pending() {
        let mut db = DoorbellArray::new(4);
        db.write(0, 0); // HC command
        db.write(2, 3); // Slot 2, EP3

        let targets = db.drain_pending();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0], DoorbellTarget::HostCommand);
        assert_eq!(
            targets[1],
            DoorbellTarget::Endpoint {
                slot_id: 2,
                endpoint_id: 3
            }
        );

        // All cleared now
        assert!(!db.is_pending(0));
        assert!(!db.is_pending(2));
    }

    #[test]
    fn doorbell_reset() {
        let mut db = DoorbellArray::new(4);
        db.write(0, 0);
        db.write(1, 1);
        db.reset();
        assert!(!db.is_pending(0));
        assert!(!db.is_pending(1));
    }

    #[test]
    fn target_display() {
        assert_eq!(DoorbellTarget::HostCommand.to_string(), "HC Command Ring");
        assert_eq!(
            DoorbellTarget::Endpoint {
                slot_id: 1,
                endpoint_id: 3
            }
            .to_string(),
            "Slot 1 EP3"
        );
    }
}
