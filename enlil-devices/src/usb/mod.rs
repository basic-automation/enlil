//! USB peripheral routing subsystem.
//!
//! Implements the full USB routing stack for Enlil:
//! - Host USB device enumeration and hot-plug monitoring
//! - Policy-driven per-device routing (VID:PID, port path, class, default)
//! - Virtual xHCI controller per guest with TRB-level interception
//! - Live device reassignment between guests
//!
//! # Architecture
//!
//! ```text
//! Physical USB Devices
//!        │
//!   ┌────┴────┐
//!   │   USB   │  Enumerates all physical USB devices via host xHCI
//!   │ Monitor │
//!   └────┬────┘
//!        │
//!   ┌────┴─────┐
//!   │ Routing  │  Policy engine: maps VID:PID or bus:port → guest
//!   │ Engine   │
//!   └────┬─────┘
//!        │
//!   ┌────┴───────────┬───────────────┐
//!   │ vXHCI Guest 1  │ vXHCI Guest 2 │
//!   └────────────────┴───────────────┘
//! ```

pub mod controller;
pub mod device;
pub mod monitor;
pub mod routing;
pub mod types;
pub mod xhci;

pub use controller::VirtualXhciController;
pub use device::{UsbClass, UsbDevice, UsbDeviceId, UsbDeviceState, UsbSpeed};
pub use monitor::{UsbHotplugEvent, UsbMonitor};
pub use routing::{RoutingRule, RoutingState, RoutingTable};
pub use xhci::{
    CapabilityRegisters, CommandRing, CommandTrb, DoorbellArray, DoorbellTarget, EventRing,
    EventTrb, InterrupterRegisterSet, NormalTrb, OperationalRegisters, PortRegisterSet, PortState,
    RuntimeRegisters, TransferRing, Trb, TrbCompletionCode, TrbRing, TrbType,
};
