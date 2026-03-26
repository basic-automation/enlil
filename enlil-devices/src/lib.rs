#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! enlil-devices: Virtual device backends
//!
//! This crate implements the virtual hardware layer that guests interact with:
//! - Block devices (VirtIO-blk)
//! - Network devices (VirtIO-net, virtual switch)
//! - Interrupt controllers (LAPIC, IOAPIC, MSI)
//! - Timer devices (PIT, HPET, TSC, paravirt clocks)
//! - Display compositor (Enlil Zones)
//! - Inter-guest communication (Enlil Bridge)
//! - VirtIO transport layer
//! - Device bus abstractions (PIO/MMIO dispatch)

pub mod block;
pub mod bridge;
pub mod bus;
pub mod display;
pub mod interrupt;
pub mod net;
pub mod storage;
pub mod timer;
pub mod virtio;
