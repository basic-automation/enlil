//! Network device backends.
//!
//! Provides VirtIO-net emulation, a virtual switch for inter-guest networking,
//! and pluggable backends (TAP on Linux, null for testing).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────┐   ┌─────────┐
//! │ Guest A │   │ Guest B │
//! │ virtio  │   │ virtio  │
//! └────┬────┘   └────┬────┘
//!      │             │
//!      ▼             ▼
//! ┌──────────────────────┐
//! │    VirtualSwitch     │
//! │  (MAC learning, fwd) │
//! └──────────┬───────────┘
//!            │
//!            ▼
//!     ┌─────────────┐
//!     │ TAP Backend  │  (or NullBackend)
//!     └─────────────┘
//! ```

mod backend;
mod config;
mod device;
mod features;
mod header;
mod switch;
mod virtqueue;

#[cfg(target_os = "linux")]
mod tap;

pub use backend::{LoopbackBackend, NetBackend, NullBackend, PipeBackend};
pub use config::NetDeviceConfig;
pub use device::{DeviceStatus, VirtioNetDevice};
pub use features::NetFeatures;
pub use header::VirtioNetHeader;
pub use switch::{PortId, VirtualSwitch};
pub use virtqueue::{Virtqueue, VirtqueueError};

#[cfg(target_os = "linux")]
pub use tap::TapBackend;
