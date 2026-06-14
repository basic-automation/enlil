//! Virtual xHCI (USB 3.0) host controller.
//!
//! Implements the xHCI specification registers, TRB rings, port state
//! machines, and command/event processing. Each guest VM gets its own
//! virtual xHCI controller instance that mediates access to physical
//! USB devices routed by the [`super::routing`] engine.
//!
//! # xHCI Register Model
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │              Capability Registers (RO)                │
//! │  CAPLENGTH │ HCIVERSION │ HCSPARAMS1-3 │ HCCPARAMS1-2│
//! ├──────────────────────────────────────────────────────┤
//! │              Operational Registers                    │
//! │  USBCMD │ USBSTS │ CRCR │ DCBAAP │ CONFIG            │
//! ├──────────────────────────────────────────────────────┤
//! │              Port Register Sets                       │
//! │  PORTSC[0..N] │ PORTPMSC │ PORTLI │ PORTHLPMC         │
//! ├──────────────────────────────────────────────────────┤
//! │              Runtime Registers                        │
//! │  MFINDEX │ Interrupter Register Sets                  │
//! ├──────────────────────────────────────────────────────┤
//! │              Doorbell Registers                       │
//! │  DB[0..MAX_SLOTS]                                     │
//! └──────────────────────────────────────────────────────┘
//! ```

pub mod context;
pub mod doorbell;
pub mod event;
pub mod registers;
pub mod ring;
pub mod transfer;
pub mod trb;

pub use context::{
    EndpointContext, EndpointType, EpState, InputControlContext, SlotContext, SlotState,
    device_context_entry_offset, device_context_pointer,
};
pub use doorbell::{DoorbellArray, DoorbellTarget};
pub use event::{EventRing, EventRingSegment, GuestEventRing, InterrupterRegisterSet};
pub use registers::{
    CapabilityRegisters, OperationalRegisters, PortRegisterSet, PortState, RuntimeRegisters,
};
pub use ring::{CommandRing, TransferRing, TrbRing};
pub use transfer::{
    CONTROL_DCI, DmaMemory, SetupPacket, TransferTrb, TransferType, VecDmaMemory,
    gather_transfer_td,
};
pub use trb::{CommandTrb, EventTrb, NormalTrb, Trb, TrbCompletionCode, TrbType};
