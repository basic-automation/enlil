//! Virtual timer and clock devices.
//!
//! Provides emulated timer hardware for guest VMs:
//! - PIT (i8254) — legacy programmable interval timer
//! - HPET — High Precision Event Timer
//! - TSC management — per-vCPU TSC offset and scaling
//! - Paravirt clocks — KVM clock (Linux) and Hyper-V reference TSC (Windows)

pub mod hpet;
pub mod paravirt;
pub mod pit;
pub mod tsc;

pub use hpet::Hpet;
pub use paravirt::{HyperVReferenceTsc, KvmClock};
pub use pit::Pit;
pub use tsc::TscManager;
