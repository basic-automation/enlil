pub mod acpi;
pub mod affinity;
pub mod cpuid;
pub mod ept;
pub mod error;
pub mod exit_handler;
pub mod kvm_backend;
pub mod memory;
pub mod serial;
pub mod smbios;
pub mod timing_stealth;
pub mod vcpu;
pub mod vm;
pub mod vtpm;

pub use error::{Error, Result};
