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
pub mod emulated;
#[cfg(target_os = "linux")]
pub mod forwarder;
pub mod hotplug;
pub mod monitor;
pub mod registry;
pub mod routing;
pub mod types;
pub mod xhci;

pub use controller::{
    MSIX_PBA_BAR_OFFSET, MSIX_TABLE_BAR_OFFSET, SharedXhci, VirtualXhciController, XHCI_MSIX_VECTORS,
    XhciMmio,
};
pub use device::{UsbClass, UsbDevice, UsbDeviceId, UsbDeviceState, UsbSpeed};
pub use emulated::{
    EmulatedKeyboard, KEYBOARD_INTERRUPT_ENDPOINT, LoopbackDevice, UsbDeviceModel,
    UsbTransferResult,
};
#[cfg(target_os = "linux")]
pub use forwarder::LibusbDevice;
pub use hotplug::{HotplugDispatcher, HotplugOutcome};
pub use monitor::{UsbHotplugEvent, UsbMonitor};
pub use registry::{AttachOutcome, DevicePlacement, RegistryError, XhciRegistry};
pub use routing::{RoutingRule, RoutingState, RoutingTable};
pub use xhci::{
    CONTROL_DCI, CapabilityRegisters, CommandRing, CommandTrb, DmaMemory, DoorbellArray,
    DoorbellTarget, EventRing, EventTrb, InterrupterRegisterSet, NormalTrb, OperationalRegisters,
    PortRegisterSet, PortState, RuntimeRegisters, SetupPacket, TransferRing, TransferTrb,
    TransferType, Trb, TrbCompletionCode, TrbRing, TrbType, VecDmaMemory,
};
