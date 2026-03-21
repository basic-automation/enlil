//! enlil-devices: Virtual device backends
//!
//! This crate implements the virtual hardware layer that guests interact with:
//! - Block devices (VirtIO-blk, NVMe passthrough)
//! - Network devices (VirtIO-net, virtual switch)
//! - Device bus abstractions (PIO/MMIO dispatch)
//!
//! On Linux/KVM, these wrap rust-vmm device crates.
//! For bare-metal, we'll implement direct hardware emulation.

pub mod block;
pub mod bus;
pub mod net;
