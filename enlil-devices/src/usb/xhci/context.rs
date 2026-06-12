//! Device-context structures read out of (and written into) guest memory
//! (xHCI §6.2).
//!
//! The controller advertises 32-byte context entries (`HCCPARAMS1.CSZ` = 0),
//! so a Device Context is an array of 32-byte entries indexed by DCI — the
//! Slot Context at index 0, each endpoint context at its DCI (§6.2.1) — and
//! an *Input* Context prepends the Input Control Context, shifting every
//! entry up by one (§6.2.5). Address Device and Configure Endpoint hand the
//! controller an input-context pointer; these codecs are how the commands
//! read what the guest's driver wrote there.

use super::transfer::{DmaMemory, dci_is_in};
use crate::truncate::{u8_of, u16_of};

/// Context entry size in bytes (`HCCPARAMS1.CSZ` = 0 → 32-byte contexts).
pub const CONTEXT_SIZE: u64 = 32;

/// The highest Device Context Index (the Device Context holds the slot
/// context plus endpoint contexts DCI 1..=31, xHCI §6.2.1).
pub const MAX_DCI: u8 = 31;

/// Offset of DCI `n`'s entry within an *input* context: the Input Control
/// Context occupies the first 32 bytes (xHCI §6.2.5), so the slot context
/// (DCI 0) is at `+0x20`, EP0 (DCI 1) at `+0x40`, and so on.
#[must_use]
pub fn input_context_entry_offset(dci: u8) -> u64 {
    CONTEXT_SIZE * (1 + u64::from(dci))
}

/// Read one raw 32-byte context entry at guest physical `addr` (`None` if
/// the range is unbacked — the command then fails as a TRB error).
pub fn read_context_entry(mem: &dyn DmaMemory, addr: u64) -> Option<[u8; 32]> {
    let mut bytes = [0_u8; 32];
    mem.read(addr, &mut bytes).then_some(bytes)
}

// ---------------------------------------------------------------------------
// Input Control Context
// ---------------------------------------------------------------------------

/// The Input Control Context (xHCI §6.2.5.1): dword 0 carries the Drop
/// Context flags, dword 1 the Add Context flags — which device-context
/// entries the command shall evaluate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputControlContext {
    /// Drop Context flags D0–D31 (D0/D1 are reserved — the slot and EP0
    /// cannot be dropped).
    pub drop_flags: u32,
    /// Add Context flags A0–A31.
    pub add_flags: u32,
}

impl InputControlContext {
    /// Read the control context from the head of the input context at
    /// `input_context_ptr` (`None` if the range is unbacked).
    pub fn read(mem: &dyn DmaMemory, input_context_ptr: u64) -> Option<Self> {
        let mut bytes = [0_u8; 8];
        mem.read(input_context_ptr, &mut bytes).then(|| Self {
            drop_flags: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            add_flags: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }

    /// Whether the Add Context flag for `dci` is set.
    #[must_use]
    pub const fn adds(self, dci: u8) -> bool {
        dci <= MAX_DCI && self.add_flags & (1 << dci) != 0
    }

    /// Whether the Drop Context flag for `dci` is set.
    #[must_use]
    pub const fn drops(self, dci: u8) -> bool {
        dci <= MAX_DCI && self.drop_flags & (1 << dci) != 0
    }
}

// ---------------------------------------------------------------------------
// Endpoint Context
// ---------------------------------------------------------------------------

/// Endpoint Context EP Type field (xHCI Table 6-9). Value 0 ("Not Valid")
/// has no variant — parsing it fails, which Configure Endpoint reports as a
/// parameter error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EndpointType {
    /// Isochronous OUT.
    IsochOut = 1,
    /// Bulk OUT.
    BulkOut = 2,
    /// Interrupt OUT.
    InterruptOut = 3,
    /// Control (bidirectional — EP0 only).
    Control = 4,
    /// Isochronous IN.
    IsochIn = 5,
    /// Bulk IN.
    BulkIn = 6,
    /// Interrupt IN.
    InterruptIn = 7,
}

impl EndpointType {
    /// Decode from the raw 3-bit field (0 = Not Valid → `None`).
    #[must_use]
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::IsochOut),
            2 => Some(Self::BulkOut),
            3 => Some(Self::InterruptOut),
            4 => Some(Self::Control),
            5 => Some(Self::IsochIn),
            6 => Some(Self::BulkIn),
            7 => Some(Self::InterruptIn),
            _ => None,
        }
    }

    /// Whether this is an IN endpoint type (Control is bidirectional and
    /// answers `false`; it is only valid at DCI 1).
    #[must_use]
    pub const fn is_in(self) -> bool {
        matches!(self, Self::IsochIn | Self::BulkIn | Self::InterruptIn)
    }

    /// Whether this type may occupy `dci`: Control belongs to DCI 1 alone,
    /// and every other type's direction must match the DCI's parity
    /// (odd = IN, xHCI §4.5.1).
    #[must_use]
    pub const fn valid_for_dci(self, dci: u8) -> bool {
        match dci {
            0 => false,
            1 => matches!(self, Self::Control),
            _ => dci <= MAX_DCI && !matches!(self, Self::Control) && self.is_in() == dci_is_in(dci),
        }
    }
}

/// A decoded 32-byte Endpoint Context (xHCI §6.2.3).
///
/// Carries the endpoint's declared transfer characteristics and where its
/// transfer ring lives in guest memory. Fields the controller does not yet
/// act on (Mult, `MaxPStreams`, LSA, Max ESIT Payload) are not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointContext {
    /// EP Type (dword 1, bits 5:3).
    pub endpoint_type: EndpointType,
    /// Max Packet Size in bytes (dword 1, bits 31:16).
    pub max_packet_size: u16,
    /// Max Burst Size (dword 1, bits 15:8).
    pub max_burst_size: u8,
    /// `CErr` — the bus-error retry budget (dword 1, bits 2:1).
    pub error_count: u8,
    /// Interval: the service period is 125 µs × 2^interval (dword 0,
    /// bits 23:16).
    pub interval: u8,
    /// TR Dequeue Pointer (dwords 2–3, bits 63:4): the guest physical
    /// address of the endpoint's transfer ring.
    pub tr_dequeue_pointer: u64,
    /// Dequeue Cycle State (dword 2, bit 0).
    pub dequeue_cycle_state: bool,
    /// Average TRB Length (dword 4, bits 15:0).
    pub average_trb_length: u16,
}

impl EndpointContext {
    /// Decode a 32-byte context entry. `None` if the EP Type field is 0
    /// ("Not Valid") — a parameter error on the command that read it.
    #[must_use]
    pub fn parse(bytes: &[u8; 32]) -> Option<Self> {
        let dword = |i: usize| {
            u32::from_le_bytes([
                bytes[4 * i],
                bytes[4 * i + 1],
                bytes[4 * i + 2],
                bytes[4 * i + 3],
            ])
        };
        let d0 = dword(0);
        let d1 = dword(1);
        let endpoint_type = EndpointType::from_raw(u8_of((d1 >> 3) & 0x7))?;
        let dequeue = u64::from(dword(2)) | u64::from(dword(3)) << 32;
        Some(Self {
            endpoint_type,
            max_packet_size: u16_of(d1 >> 16),
            max_burst_size: u8_of((d1 >> 8) & 0xFF),
            error_count: u8_of((d1 >> 1) & 0x3),
            interval: u8_of((d0 >> 16) & 0xFF),
            tr_dequeue_pointer: dequeue & !0xF,
            dequeue_cycle_state: dequeue & 1 != 0,
            average_trb_length: u16_of(dword(4) & 0xFFFF),
        })
    }

    /// Encode into a 32-byte context entry (the inverse of
    /// [`parse`](Self::parse)) — how the controller writes output device
    /// contexts back to guest memory, and how tests author input contexts.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 32] {
        let d0 = u32::from(self.interval) << 16;
        let d1 = u32::from(self.error_count & 0x3) << 1
            | u32::from(self.endpoint_type as u8) << 3
            | u32::from(self.max_burst_size) << 8
            | u32::from(self.max_packet_size) << 16;
        let dequeue = (self.tr_dequeue_pointer & !0xF) | u64::from(self.dequeue_cycle_state);
        let mut bytes = [0_u8; 32];
        bytes[0..4].copy_from_slice(&d0.to_le_bytes());
        bytes[4..8].copy_from_slice(&d1.to_le_bytes());
        bytes[8..16].copy_from_slice(&dequeue.to_le_bytes());
        bytes[16..20].copy_from_slice(&u32::from(self.average_trb_length).to_le_bytes());
        bytes
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::transfer::VecDmaMemory;
    use super::*;

    /// A keyboard-shaped interrupt IN endpoint context.
    const INTERRUPT_IN: EndpointContext = EndpointContext {
        endpoint_type: EndpointType::InterruptIn,
        max_packet_size: 8,
        max_burst_size: 0,
        error_count: 3,
        interval: 7,
        tr_dequeue_pointer: 0x0002_3450,
        dequeue_cycle_state: true,
        average_trb_length: 8,
    };

    #[test]
    fn endpoint_context_round_trips_the_spec_layout() {
        let bytes = INTERRUPT_IN.to_bytes();
        // Dword 0: interval in bits 23:16.
        assert_eq!(bytes[2], 7);
        // Dword 1: CErr 2:1, EP Type 5:3, max packet 31:16.
        assert_eq!(bytes[4], 3 << 1 | 7 << 3);
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 8);
        // Dwords 2-3: TR dequeue pointer with DCS in bit 0.
        assert_eq!(
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            0x0002_3450 | 1
        );
        assert_eq!(EndpointContext::parse(&bytes), Some(INTERRUPT_IN));
    }

    #[test]
    fn parse_rejects_the_not_valid_type_and_masks_the_pointer() {
        // EP Type 0 = Not Valid.
        assert_eq!(EndpointContext::parse(&[0_u8; 32]), None);

        // Low pointer bits beyond DCS are reserved and read back zero.
        let mut ctx = INTERRUPT_IN;
        ctx.tr_dequeue_pointer = 0x1234_5678;
        let parsed = EndpointContext::parse(&ctx.to_bytes()).unwrap();
        assert_eq!(parsed.tr_dequeue_pointer, 0x1234_5670);
    }

    #[test]
    fn endpoint_type_dci_compatibility() {
        // Control belongs to DCI 1 alone.
        assert!(EndpointType::Control.valid_for_dci(1));
        assert!(!EndpointType::Control.valid_for_dci(2));
        assert!(!EndpointType::BulkOut.valid_for_dci(1));
        // Direction must match DCI parity: EP1 OUT = DCI 2, EP1 IN = DCI 3.
        assert!(EndpointType::BulkOut.valid_for_dci(2));
        assert!(!EndpointType::BulkIn.valid_for_dci(2));
        assert!(EndpointType::InterruptIn.valid_for_dci(3));
        assert!(!EndpointType::InterruptOut.valid_for_dci(3));
        // DCI 0 is the slot context; past MAX_DCI is out of range.
        assert!(!EndpointType::BulkOut.valid_for_dci(0));
        assert!(!EndpointType::BulkIn.valid_for_dci(33));
    }

    #[test]
    fn input_control_context_reads_flags_from_guest_memory() {
        let mut mem = VecDmaMemory::new(0x1000, 0x40);
        assert!(mem.write(0x1000, &(1_u32 << 3).to_le_bytes())); // drop D3
        assert!(mem.write(0x1004, &0x5_u32.to_le_bytes())); // add A0|A2

        let control = InputControlContext::read(&mem, 0x1000).unwrap();
        assert!(control.drops(3) && !control.drops(2));
        assert!(control.adds(0) && control.adds(2) && !control.adds(1));
        // Out-of-range DCIs are never flagged.
        assert!(!control.adds(32) && !control.drops(40));

        // An unbacked pointer reads nothing.
        assert!(InputControlContext::read(&mem, 0xDEAD_0000).is_none());
    }

    #[test]
    fn input_context_entries_follow_the_control_context() {
        assert_eq!(input_context_entry_offset(0), 0x20); // slot context
        assert_eq!(input_context_entry_offset(1), 0x40); // EP0
        assert_eq!(input_context_entry_offset(3), 0x80); // EP1 IN
    }

    #[test]
    fn read_context_entry_requires_backed_memory() {
        let mut mem = VecDmaMemory::new(0x1000, 0x40);
        assert!(mem.write(0x1020, &[0xAB]));
        let entry = read_context_entry(&mem, 0x1020).unwrap();
        assert_eq!(entry[0], 0xAB);
        assert!(
            read_context_entry(&mem, 0x1030).is_none(),
            "runs off the end"
        );
    }
}
