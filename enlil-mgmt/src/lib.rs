#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! enlil-mgmt: management-console library for the Enlil hypervisor.
//!
//! The binary (`src/main.rs`) is the operator-facing console; this library
//! carries the pieces it shares with the `enlil-core` daemon side — most
//! importantly the [`protocol`] spoken between them.

pub mod app;
pub mod guest_tab;
/// The management wire protocol, shared with the `enlil-core` daemon side —
/// re-exported from the `enlil-mgmt-proto` crate so both ends of the socket
/// speak the same types without a dependency cycle.
pub use enlil_mgmt_proto as protocol;
pub mod usb_tab;
