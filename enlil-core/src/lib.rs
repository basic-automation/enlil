pub mod acpi;
pub mod affinity;
pub mod device_bus;
pub mod ept;
pub mod error;
pub mod kvm_backend;
pub mod memory;
pub mod run_loop;
pub mod serial;
pub mod stealth_msr;
pub mod timing_stealth;
pub mod usb_routing;
pub mod vcpu;
pub mod vm;

pub use error::{Error, Result};

// The virtual TPM is the canonical `enlil_devices::tpm::VirtualTpm`: the CRB
// MMIO device at 0xFED40000, which also exposes a byte-vec `execute_command`
// front for the management plane (both fronts share one PCR bank / NV store).
// A second `vtpm` module used to sit here with its own PCR and NV types and a
// diverging wire format — a duplicate, so it was removed; its NV-storage
// semantics were folded into the device TPM.
