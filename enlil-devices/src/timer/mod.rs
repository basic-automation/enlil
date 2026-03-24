//! Virtual timer and clock devices.
//!
//! Provides emulated timer hardware for guest VMs:
//! - PIT (i8254) — legacy programmable interval timer
//! - HPET — High Precision Event Timer
//! - TSC management — per-vCPU TSC offset and scaling
//! - Paravirt clocks — KVM clock (Linux) and Hyper-V reference TSC (Windows)

pub mod pit;
pub mod hpet;
pub mod tsc;
pub mod paravirt;

pub use pit::Pit;
pub use hpet::Hpet;
pub use tsc::TscManager;
pub use paravirt::{KvmClock, HyperVReferenceTsc};
