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
pub mod rtc;
pub mod speaker;
pub mod tsc;

pub use hpet::{HPET_MMIO_BASE, HPET_MMIO_SIZE, Hpet, HpetMmio, SharedHpet};
pub use paravirt::{HyperVReferenceTsc, KvmClock};
pub use pit::{IrqLine, Pit, PitPort, SharedPit};
pub use rtc::{RTC_DATA, RTC_INDEX, RTC_IRQ, Rtc146818, RtcPort, RtcTime, SharedRtc};
pub use speaker::{PORT_B, SystemControlPortB};
pub use tsc::TscManager;
