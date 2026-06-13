//! Transfer Request Block (TRB) definitions for xHCI.
//!
//! TRBs are the fundamental data structure in xHCI — 16-byte records
//! that describe USB transfers, commands, and events. They flow through
//! ring buffers between the host controller and software.

use std::fmt;

// ---------------------------------------------------------------------------
// TRB base
// ---------------------------------------------------------------------------

/// Raw 16-byte Transfer Request Block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trb {
    /// Parameter field (bytes 0–7).
    pub parameter: u64,
    /// Status field (bytes 8–11).
    pub status: u32,
    /// Control field (bytes 12–15): type, cycle bit, flags.
    pub control: u32,
}

impl Trb {
    /// Size of a single TRB in bytes.
    pub const SIZE: usize = 16;

    /// Create a zeroed TRB.
    #[must_use]
    pub const fn zeroed() -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: 0,
        }
    }

    /// Create a TRB with the given type and all other fields zeroed.
    #[must_use]
    pub const fn new(trb_type: TrbType) -> Self {
        let mut trb = Self::zeroed();
        trb.set_trb_type(trb_type);
        trb
    }

    /// Get the TRB type from bits [15:10] of the control field.
    #[must_use]
    pub const fn trb_type(&self) -> u8 {
        ((self.control >> 10) & 0x3F) as u8
    }

    /// Get the cycle bit (bit 0 of control).
    #[must_use]
    pub const fn cycle_bit(&self) -> bool {
        (self.control & 1) != 0
    }

    /// Set the cycle bit.
    pub const fn set_cycle_bit(&mut self, cycle: bool) {
        if cycle {
            self.control |= 1;
        } else {
            self.control &= !1;
        }
    }

    /// Alias for [`set_cycle_bit`](Self::set_cycle_bit) — used by ring implementations.
    pub const fn set_cycle(&mut self, cycle: bool) {
        self.set_cycle_bit(cycle);
    }

    /// Set the TRB type in bits [15:10].
    pub const fn set_trb_type(&mut self, trb_type: TrbType) {
        self.control = (self.control & !(0x3F << 10)) | ((trb_type as u32) << 10);
    }

    /// Decode the TRB type field into a known variant.
    #[must_use]
    pub const fn decoded_type(&self) -> TrbType {
        TrbType::from_raw(self.trb_type())
    }

    /// Encode this TRB to a 16-byte array (little-endian).
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&self.parameter.to_le_bytes());
        buf[8..12].copy_from_slice(&self.status.to_le_bytes());
        buf[12..16].copy_from_slice(&self.control.to_le_bytes());
        buf
    }

    /// Decode a TRB from a 16-byte array (little-endian).
    #[must_use]
    pub const fn from_bytes(buf: &[u8; 16]) -> Self {
        Self {
            parameter: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            status: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            control: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        }
    }
}

impl Default for Trb {
    fn default() -> Self {
        Self::zeroed()
    }
}

// ---------------------------------------------------------------------------
// TRB types (xHCI Table 6-91)
// ---------------------------------------------------------------------------

/// Known TRB type codes from the xHCI specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrbType {
    // Transfer TRBs
    Normal = 1,
    SetupStage = 2,
    DataStage = 3,
    StatusStage = 4,
    Isoch = 5,
    Link = 6,
    EventData = 7,
    NoOp = 8,

    // Command TRBs
    EnableSlotCommand = 9,
    DisableSlotCommand = 10,
    AddressDeviceCommand = 11,
    ConfigureEndpointCommand = 12,
    EvaluateContextCommand = 13,
    ResetEndpointCommand = 14,
    StopEndpointCommand = 15,
    SetTrDequeuePointerCommand = 16,
    ResetDeviceCommand = 17,
    NoOpCommand = 23,

    // Event TRBs
    TransferEvent = 32,
    CommandCompletionEvent = 33,
    PortStatusChangeEvent = 34,
    HostControllerEvent = 37,

    /// Unrecognized or reserved TRB type.
    Unknown = 0,
}

impl TrbType {
    /// Convert a raw 6-bit type code to a `TrbType`.
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Normal,
            2 => Self::SetupStage,
            3 => Self::DataStage,
            4 => Self::StatusStage,
            5 => Self::Isoch,
            6 => Self::Link,
            7 => Self::EventData,
            8 => Self::NoOp,
            9 => Self::EnableSlotCommand,
            10 => Self::DisableSlotCommand,
            11 => Self::AddressDeviceCommand,
            12 => Self::ConfigureEndpointCommand,
            13 => Self::EvaluateContextCommand,
            14 => Self::ResetEndpointCommand,
            15 => Self::StopEndpointCommand,
            16 => Self::SetTrDequeuePointerCommand,
            17 => Self::ResetDeviceCommand,
            23 => Self::NoOpCommand,
            32 => Self::TransferEvent,
            33 => Self::CommandCompletionEvent,
            34 => Self::PortStatusChangeEvent,
            37 => Self::HostControllerEvent,
            _ => Self::Unknown,
        }
    }

    /// Whether this is a transfer-ring TRB (types 1–8).
    #[must_use]
    pub const fn is_transfer(&self) -> bool {
        (*self as u8) >= 1 && (*self as u8) <= 8
    }

    /// Whether this is a command TRB (types 9–23).
    #[must_use]
    pub const fn is_command(&self) -> bool {
        (*self as u8) >= 9 && (*self as u8) <= 23
    }

    /// Whether this is an event TRB (types 32–37).
    #[must_use]
    pub const fn is_event(&self) -> bool {
        (*self as u8) >= 32 && (*self as u8) <= 37
    }
}

impl fmt::Display for TrbType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Normal => write!(f, "Normal"),
            Self::SetupStage => write!(f, "SetupStage"),
            Self::DataStage => write!(f, "DataStage"),
            Self::StatusStage => write!(f, "StatusStage"),
            Self::Isoch => write!(f, "Isoch"),
            Self::Link => write!(f, "Link"),
            Self::EventData => write!(f, "EventData"),
            Self::NoOp => write!(f, "NoOp"),
            Self::EnableSlotCommand => write!(f, "EnableSlotCommand"),
            Self::DisableSlotCommand => write!(f, "DisableSlotCommand"),
            Self::AddressDeviceCommand => write!(f, "AddressDeviceCommand"),
            Self::ConfigureEndpointCommand => write!(f, "ConfigureEndpointCommand"),
            Self::EvaluateContextCommand => write!(f, "EvaluateContextCommand"),
            Self::ResetEndpointCommand => write!(f, "ResetEndpointCommand"),
            Self::StopEndpointCommand => write!(f, "StopEndpointCommand"),
            Self::SetTrDequeuePointerCommand => write!(f, "SetTRDequeuePointerCommand"),
            Self::ResetDeviceCommand => write!(f, "ResetDeviceCommand"),
            Self::NoOpCommand => write!(f, "NoOpCommand"),
            Self::TransferEvent => write!(f, "TransferEvent"),
            Self::CommandCompletionEvent => write!(f, "CommandCompletionEvent"),
            Self::PortStatusChangeEvent => write!(f, "PortStatusChangeEvent"),
            Self::HostControllerEvent => write!(f, "HostControllerEvent"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

// ---------------------------------------------------------------------------
// Completion codes (xHCI Table 6-90)
// ---------------------------------------------------------------------------

/// TRB completion status codes from the xHCI spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrbCompletionCode {
    /// Successful completion.
    Success = 1,
    /// Data buffer error.
    DataBufferError = 2,
    /// Babble detected.
    BabbleDetectedError = 3,
    /// USB transaction error.
    UsbTransactionError = 4,
    /// TRB error (malformed).
    TrbError = 5,
    /// Stall error.
    StallError = 6,
    /// Resource error (out of internal resources).
    ResourceError = 7,
    /// Bandwidth error.
    BandwidthError = 8,
    /// No slots available.
    NoSlotsAvailableError = 9,
    /// Short packet (fewer bytes than expected — often not an error).
    ShortPacket = 13,
    /// Ring underrun.
    RingUnderrun = 14,
    /// Ring overrun.
    RingOverrun = 15,
    /// Parameter error (malformed context or command parameter).
    ParameterError = 17,
    /// Context state error: a command found a slot/endpoint in a state that
    /// does not permit it (e.g. Set TR Dequeue Pointer on a non-stopped or
    /// unconfigured endpoint).
    ContextStateError = 19,
    /// Command ring stopped.
    CommandRingStopped = 24,
    /// Command aborted.
    CommandAborted = 25,
    /// Stopped (endpoint).
    Stopped = 26,
    /// Invalid or unknown code.
    Invalid = 0,
}

impl TrbCompletionCode {
    /// Convert from a raw 8-bit code.
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Success,
            2 => Self::DataBufferError,
            3 => Self::BabbleDetectedError,
            4 => Self::UsbTransactionError,
            5 => Self::TrbError,
            6 => Self::StallError,
            7 => Self::ResourceError,
            8 => Self::BandwidthError,
            9 => Self::NoSlotsAvailableError,
            13 => Self::ShortPacket,
            14 => Self::RingUnderrun,
            15 => Self::RingOverrun,
            17 => Self::ParameterError,
            19 => Self::ContextStateError,
            24 => Self::CommandRingStopped,
            25 => Self::CommandAborted,
            26 => Self::Stopped,
            _ => Self::Invalid,
        }
    }

    /// Whether this code indicates success (or a non-fatal short packet).
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success | Self::ShortPacket)
    }
}

impl fmt::Display for TrbCompletionCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

// ---------------------------------------------------------------------------
// Typed TRB wrappers
// ---------------------------------------------------------------------------

/// A Normal transfer TRB (type 1) — the workhorse for bulk/interrupt transfers.
#[derive(Debug, Clone, Copy)]
pub struct NormalTrb {
    /// Physical address of the data buffer.
    pub data_buffer_pointer: u64,
    /// Transfer length in bytes.
    pub transfer_length: u32,
    /// Interrupt-on-completion.
    pub ioc: bool,
    /// Interrupt-on-short-packet.
    pub isp: bool,
    /// Chain bit (part of a multi-TRB TD).
    pub chain: bool,
}

impl NormalTrb {
    /// Encode into a raw TRB.
    #[must_use]
    pub const fn to_trb(&self, cycle: bool) -> Trb {
        let mut control: u32 = (TrbType::Normal as u32) << 10;
        if cycle {
            control |= 1;
        }
        if self.ioc {
            control |= 1 << 5;
        }
        if self.isp {
            control |= 1 << 2;
        }
        if self.chain {
            control |= 1 << 4;
        }
        Trb {
            parameter: self.data_buffer_pointer,
            status: self.transfer_length & 0x0001_FFFF,
            control,
        }
    }
}

/// A command TRB submitted on the command ring.
#[derive(Debug, Clone, Copy)]
pub enum CommandTrb {
    /// Enable a device slot.
    EnableSlot,
    /// Disable a device slot.
    DisableSlot { slot_id: u8 },
    /// Address a device (assign USB address).
    AddressDevice { slot_id: u8, input_context_ptr: u64 },
    /// Configure endpoints.
    ConfigureEndpoint {
        slot_id: u8,
        input_context_ptr: u64,
        /// Deconfigure (DC, control bit 9): drop every endpoint but EP0;
        /// the input context pointer is not referenced (xHCI §6.4.3.5).
        deconfigure: bool,
    },
    /// Evaluate a device context (xHCI §4.6.7): re-evaluate the input
    /// context's slot/EP0 fields (Max Exit Latency, EP0 Max Packet Size)
    /// without changing endpoint or slot state — issued mid-enumeration once
    /// the driver has read the device descriptor.
    EvaluateContext { slot_id: u8, input_context_ptr: u64 },
    /// Reset an endpoint.
    ResetEndpoint { slot_id: u8, endpoint_id: u8 },
    /// Stop an endpoint.
    StopEndpoint { slot_id: u8, endpoint_id: u8 },
    /// Set TR Dequeue Pointer (xHCI §4.6.10): repoint an endpoint's transfer
    /// ring after a halt/stop, carrying the new dequeue pointer and the
    /// Dequeue Cycle State the consumer resumes with.
    SetTrDequeuePointer {
        slot_id: u8,
        endpoint_id: u8,
        /// New TR Dequeue Pointer (16-byte aligned guest address).
        dequeue_ptr: u64,
        /// Dequeue Cycle State (parameter bit 0).
        dcs: bool,
    },
    /// No-op command (for testing).
    NoOp,
}

impl CommandTrb {
    /// Encode into a raw TRB.
    #[must_use]
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut trb = Trb::zeroed();
        trb.set_cycle_bit(cycle);
        match self {
            Self::EnableSlot => {
                trb.set_trb_type(TrbType::EnableSlotCommand);
            }
            Self::DisableSlot { slot_id } => {
                trb.set_trb_type(TrbType::DisableSlotCommand);
                trb.control |= u32::from(*slot_id) << 24;
            }
            Self::AddressDevice {
                slot_id,
                input_context_ptr,
            } => {
                trb.set_trb_type(TrbType::AddressDeviceCommand);
                trb.parameter = *input_context_ptr;
                trb.control |= u32::from(*slot_id) << 24;
            }
            Self::ConfigureEndpoint {
                slot_id,
                input_context_ptr,
                deconfigure,
            } => {
                trb.set_trb_type(TrbType::ConfigureEndpointCommand);
                trb.parameter = *input_context_ptr;
                trb.control |= u32::from(*slot_id) << 24;
                if *deconfigure {
                    trb.control |= 1 << 9;
                }
            }
            Self::EvaluateContext {
                slot_id,
                input_context_ptr,
            } => {
                trb.set_trb_type(TrbType::EvaluateContextCommand);
                trb.parameter = *input_context_ptr;
                trb.control |= u32::from(*slot_id) << 24;
            }
            Self::ResetEndpoint {
                slot_id,
                endpoint_id,
            } => {
                trb.set_trb_type(TrbType::ResetEndpointCommand);
                trb.control |= u32::from(*slot_id) << 24;
                trb.control |= u32::from(*endpoint_id) << 16;
            }
            Self::StopEndpoint {
                slot_id,
                endpoint_id,
            } => {
                trb.set_trb_type(TrbType::StopEndpointCommand);
                trb.control |= u32::from(*slot_id) << 24;
                trb.control |= u32::from(*endpoint_id) << 16;
            }
            Self::SetTrDequeuePointer {
                slot_id,
                endpoint_id,
                dequeue_ptr,
                dcs,
            } => {
                trb.set_trb_type(TrbType::SetTrDequeuePointerCommand);
                trb.parameter = (*dequeue_ptr & !0xF) | u64::from(*dcs);
                trb.control |= u32::from(*slot_id) << 24;
                trb.control |= u32::from(*endpoint_id) << 16;
            }
            Self::NoOp => {
                trb.set_trb_type(TrbType::NoOpCommand);
            }
        }
        trb
    }
}

impl CommandTrb {
    /// Attempt to decode a command TRB from a raw TRB fetched off the
    /// command ring (the inverse of [`to_trb`](Self::to_trb)).
    #[must_use]
    pub const fn from_trb(trb: &Trb) -> Option<Self> {
        let slot_id = (trb.control >> 24) as u8;
        let endpoint_id = ((trb.control >> 16) & 0x1F) as u8;
        match trb.decoded_type() {
            TrbType::EnableSlotCommand => Some(Self::EnableSlot),
            TrbType::DisableSlotCommand => Some(Self::DisableSlot { slot_id }),
            TrbType::AddressDeviceCommand => Some(Self::AddressDevice {
                slot_id,
                input_context_ptr: trb.parameter,
            }),
            TrbType::ConfigureEndpointCommand => Some(Self::ConfigureEndpoint {
                slot_id,
                input_context_ptr: trb.parameter,
                deconfigure: trb.control & (1 << 9) != 0,
            }),
            TrbType::EvaluateContextCommand => Some(Self::EvaluateContext {
                slot_id,
                input_context_ptr: trb.parameter,
            }),
            TrbType::ResetEndpointCommand => Some(Self::ResetEndpoint {
                slot_id,
                endpoint_id,
            }),
            TrbType::StopEndpointCommand => Some(Self::StopEndpoint {
                slot_id,
                endpoint_id,
            }),
            TrbType::SetTrDequeuePointerCommand => Some(Self::SetTrDequeuePointer {
                slot_id,
                endpoint_id,
                dequeue_ptr: trb.parameter & !0xF,
                dcs: trb.parameter & 1 != 0,
            }),
            TrbType::NoOpCommand => Some(Self::NoOp),
            _ => None,
        }
    }
}

/// An event TRB produced by the controller on the event ring.
#[derive(Debug, Clone, Copy)]
pub enum EventTrb {
    /// Transfer completion event.
    TransferEvent {
        trb_pointer: u64,
        completion_code: TrbCompletionCode,
        transfer_length: u32,
        slot_id: u8,
        endpoint_id: u8,
    },
    /// Command completion event.
    CommandCompletion {
        command_trb_pointer: u64,
        completion_code: TrbCompletionCode,
        slot_id: u8,
    },
    /// Port status change event.
    PortStatusChange { port_id: u8 },
    /// Host controller event (error or other notification).
    HostController { completion_code: TrbCompletionCode },
}

impl EventTrb {
    /// Encode into a raw TRB.
    #[must_use]
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut trb = Trb::zeroed();
        trb.set_cycle_bit(cycle);
        match self {
            Self::TransferEvent {
                trb_pointer,
                completion_code,
                transfer_length,
                slot_id,
                endpoint_id,
            } => {
                trb.set_trb_type(TrbType::TransferEvent);
                trb.parameter = *trb_pointer;
                trb.status = (*transfer_length & 0x00FF_FFFF) | ((*completion_code as u32) << 24);
                trb.control |= u32::from(*slot_id) << 24;
                trb.control |= u32::from(*endpoint_id) << 16;
            }
            Self::CommandCompletion {
                command_trb_pointer,
                completion_code,
                slot_id,
            } => {
                trb.set_trb_type(TrbType::CommandCompletionEvent);
                trb.parameter = *command_trb_pointer;
                trb.status = (*completion_code as u32) << 24;
                trb.control |= u32::from(*slot_id) << 24;
            }
            Self::PortStatusChange { port_id } => {
                trb.set_trb_type(TrbType::PortStatusChangeEvent);
                trb.parameter = u64::from(*port_id) << 24;
            }
            Self::HostController { completion_code } => {
                trb.set_trb_type(TrbType::HostControllerEvent);
                trb.status = (*completion_code as u32) << 24;
            }
        }
        trb
    }

    /// Attempt to decode an event TRB from a raw TRB.
    #[must_use]
    pub const fn from_trb(trb: &Trb) -> Option<Self> {
        match trb.decoded_type() {
            TrbType::TransferEvent => Some(Self::TransferEvent {
                trb_pointer: trb.parameter,
                completion_code: TrbCompletionCode::from_raw((trb.status >> 24) as u8),
                transfer_length: trb.status & 0x00FF_FFFF,
                slot_id: (trb.control >> 24) as u8,
                endpoint_id: ((trb.control >> 16) & 0x1F) as u8,
            }),
            TrbType::CommandCompletionEvent => Some(Self::CommandCompletion {
                command_trb_pointer: trb.parameter,
                completion_code: TrbCompletionCode::from_raw((trb.status >> 24) as u8),
                slot_id: (trb.control >> 24) as u8,
            }),
            TrbType::PortStatusChangeEvent => Some(Self::PortStatusChange {
                port_id: ((trb.parameter >> 24) & 0xFF) as u8,
            }),
            TrbType::HostControllerEvent => Some(Self::HostController {
                completion_code: TrbCompletionCode::from_raw((trb.status >> 24) as u8),
            }),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trb_round_trip_bytes() {
        let trb = Trb {
            parameter: 0xDEAD_BEEF_CAFE_BABE,
            status: 0x1234_5678,
            control: 0xABCD_EF01,
        };
        let bytes = trb.to_bytes();
        let decoded = Trb::from_bytes(&bytes);
        assert_eq!(trb, decoded);
    }

    #[test]
    fn trb_type_extraction() {
        let mut trb = Trb::zeroed();
        trb.set_trb_type(TrbType::Normal);
        assert_eq!(trb.decoded_type(), TrbType::Normal);
        assert_eq!(trb.trb_type(), 1);

        trb.set_trb_type(TrbType::CommandCompletionEvent);
        assert_eq!(trb.decoded_type(), TrbType::CommandCompletionEvent);
        assert_eq!(trb.trb_type(), 33);
    }

    #[test]
    fn cycle_bit() {
        let mut trb = Trb::zeroed();
        assert!(!trb.cycle_bit());
        trb.set_cycle_bit(true);
        assert!(trb.cycle_bit());
        trb.set_cycle_bit(false);
        assert!(!trb.cycle_bit());
    }

    #[test]
    fn trb_type_classification() {
        assert!(TrbType::Normal.is_transfer());
        assert!(TrbType::Link.is_transfer());
        assert!(!TrbType::Normal.is_command());

        assert!(TrbType::EnableSlotCommand.is_command());
        assert!(TrbType::NoOpCommand.is_command());
        assert!(!TrbType::EnableSlotCommand.is_event());

        assert!(TrbType::TransferEvent.is_event());
        assert!(TrbType::HostControllerEvent.is_event());
        assert!(!TrbType::TransferEvent.is_transfer());
    }

    #[test]
    fn normal_trb_encode() {
        let normal = NormalTrb {
            data_buffer_pointer: 0x1000_0000,
            transfer_length: 512,
            ioc: true,
            isp: false,
            chain: false,
        };
        let trb = normal.to_trb(true);
        assert_eq!(trb.decoded_type(), TrbType::Normal);
        assert!(trb.cycle_bit());
        assert_eq!(trb.parameter, 0x1000_0000);
        assert_eq!(trb.status & 0x1FFFF, 512);
        // IOC is bit 5
        assert_ne!(trb.control & (1 << 5), 0);
    }

    #[test]
    fn command_trb_enable_slot() {
        let cmd = CommandTrb::EnableSlot;
        let trb = cmd.to_trb(true);
        assert_eq!(trb.decoded_type(), TrbType::EnableSlotCommand);
        assert!(trb.cycle_bit());
    }

    #[test]
    fn command_trb_set_tr_dequeue_pointer_round_trips() {
        let trb = CommandTrb::SetTrDequeuePointer {
            slot_id: 3,
            endpoint_id: 4,
            dequeue_ptr: 0x8_0000,
            dcs: true,
        }
        .to_trb(true);
        assert_eq!(trb.decoded_type(), TrbType::SetTrDequeuePointerCommand);
        match CommandTrb::from_trb(&trb) {
            Some(CommandTrb::SetTrDequeuePointer {
                slot_id,
                endpoint_id,
                dequeue_ptr,
                dcs,
            }) => {
                assert_eq!(slot_id, 3);
                assert_eq!(endpoint_id, 4);
                assert_eq!(dequeue_ptr, 0x8_0000);
                assert!(dcs);
            }
            other => panic!("expected SetTrDequeuePointer, got {other:?}"),
        }
    }

    #[test]
    fn command_trb_evaluate_context_round_trips() {
        let trb = CommandTrb::EvaluateContext {
            slot_id: 5,
            input_context_ptr: 0x1_2340,
        }
        .to_trb(true);
        assert_eq!(trb.decoded_type(), TrbType::EvaluateContextCommand);
        match CommandTrb::from_trb(&trb) {
            Some(CommandTrb::EvaluateContext {
                slot_id,
                input_context_ptr,
            }) => {
                assert_eq!(slot_id, 5);
                assert_eq!(input_context_ptr, 0x1_2340);
            }
            other => panic!("expected EvaluateContext, got {other:?}"),
        }
    }

    #[test]
    fn event_trb_roundtrip() {
        let event = EventTrb::CommandCompletion {
            command_trb_pointer: 0x2000,
            completion_code: TrbCompletionCode::Success,
            slot_id: 3,
        };
        let trb = event.to_trb(true);
        let decoded = EventTrb::from_trb(&trb).unwrap();
        match decoded {
            EventTrb::CommandCompletion {
                command_trb_pointer,
                completion_code,
                slot_id,
            } => {
                assert_eq!(command_trb_pointer, 0x2000);
                assert_eq!(completion_code, TrbCompletionCode::Success);
                assert_eq!(slot_id, 3);
            }
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn completion_code_success() {
        assert!(TrbCompletionCode::Success.is_success());
        assert!(TrbCompletionCode::ShortPacket.is_success());
        assert!(!TrbCompletionCode::StallError.is_success());
        assert!(!TrbCompletionCode::Invalid.is_success());
    }

    #[test]
    fn port_status_change_event() {
        let event = EventTrb::PortStatusChange { port_id: 5 };
        let trb = event.to_trb(false);
        assert!(!trb.cycle_bit());
        let decoded = EventTrb::from_trb(&trb).unwrap();
        match decoded {
            EventTrb::PortStatusChange { port_id } => assert_eq!(port_id, 5),
            _ => panic!("wrong event type"),
        }
    }
}
