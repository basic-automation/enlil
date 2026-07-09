#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! enlil-mgmt: management-console library for the Enlil hypervisor.
//!
//! The binary (`src/main.rs`) is the operator-facing console; this library
//! carries the pieces it shares with the `enlil-core` daemon side — most
//! importantly the [`protocol`] spoken between them.

pub mod guest_tab;
pub mod protocol;
pub mod usb_tab;
