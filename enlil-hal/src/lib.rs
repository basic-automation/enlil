#![no_std]
#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! # enlil-hal
//!
//! Hardware Abstraction Layer for the Enlil hypervisor.
//!
//! Provides architecture-neutral traits that each platform backend
//! (KVM, WHP, Hypervisor.framework, …) must implement.
//!
//! This crate is `no_std` + `alloc` so the very same traits compile for both
//! the Linux/KVM dev host and the bare-metal `x86_64-unknown-enlil` kernel
//! target (LOCKED PRINCIPLE 2 — the HAL is the sole ISA seam). `HalError`'s
//! `impl core::error::Error` is identical to `std::error::Error` on the host
//! (std re-exports the core trait), so nothing above the HAL changes.

extern crate alloc;

pub mod svm;
pub mod vmx;

use alloc::string::String;
use core::fmt;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by HAL operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HalError {
    /// vCPU creation failed.
    VCpuCreation(String),

    /// vCPU run failed.
    VCpuRun(String),

    /// Guest memory mapping failed.
    MemoryMap(String),

    /// Interrupt injection failed.
    InterruptInject(String),

    /// The requested operation is unsupported on this backend.
    Unsupported(String),

    /// Any other backend error.
    Other(String),
}

impl fmt::Display for HalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VCpuCreation(m) => write!(f, "vCPU creation failed: {m}"),
            Self::VCpuRun(m) => write!(f, "vCPU run failed: {m}"),
            Self::MemoryMap(m) => write!(f, "memory mapping failed: {m}"),
            Self::InterruptInject(m) => write!(f, "interrupt injection failed: {m}"),
            Self::Unsupported(m) => write!(f, "unsupported operation: {m}"),
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl core::error::Error for HalError {}

pub type HalResult<T> = Result<T, HalError>;

// ---------------------------------------------------------------------------
// VCpu configuration
// ---------------------------------------------------------------------------

/// Initial configuration for a virtual CPU.
#[derive(Debug, Clone)]
pub struct VCpuConfig {
    /// vCPU index (0-based).
    pub id: u32,

    /// Entry-point instruction pointer.
    pub entry_addr: u64,

    /// Initial stack pointer.
    pub stack_addr: u64,

    /// Boot-time argument (e.g. pointer to boot params).
    pub boot_arg: u64,
}

// ---------------------------------------------------------------------------
// VM-exit reasons
// ---------------------------------------------------------------------------

/// Architecture-neutral representation of a VM exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmExit {
    /// Guest executed an IN instruction.
    IoIn { port: u16, size: u8 },

    /// Guest executed an OUT instruction.
    IoOut { port: u16, size: u8, data: u32 },

    /// Guest performed an MMIO read.
    MmioRead { address: u64, size: u8 },

    /// Guest performed an MMIO write.
    MmioWrite { address: u64, size: u8, data: u64 },

    /// Guest executed a HLT instruction.
    Hlt,

    /// Guest requested shutdown (e.g. triple-fault, ACPI power-off).
    Shutdown,

    /// Exit reason not yet mapped.
    Unknown(u32),
}

impl fmt::Display for VmExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IoIn { port, size } => write!(f, "IoIn(port=0x{port:04x}, size={size})"),
            Self::IoOut { port, size, data } => {
                write!(
                    f,
                    "IoOut(port=0x{port:04x}, size={size}, data=0x{data:08x})"
                )
            }
            Self::MmioRead { address, size } => {
                write!(f, "MmioRead(addr=0x{address:016x}, size={size})")
            }
            Self::MmioWrite {
                address,
                size,
                data,
            } => write!(
                f,
                "MmioWrite(addr=0x{address:016x}, size={size}, data=0x{data:016x})"
            ),
            Self::Hlt => write!(f, "Hlt"),
            Self::Shutdown => write!(f, "Shutdown"),
            Self::Unknown(code) => write!(f, "Unknown({code})"),
        }
    }
}

// ---------------------------------------------------------------------------
// Hypervisor backend trait (arch-neutral)
// ---------------------------------------------------------------------------

/// The core abstraction every platform hypervisor backend must implement.
///
/// Associated types let each backend supply its own concrete vCPU handle,
/// page-table manager, and interrupt controller without boxing.
pub trait HypervisorBackend: Send + Sync {
    /// Handle to a virtual CPU.
    type VCpu: Send;

    /// Guest physical → host virtual page-table manager.
    type PageTable: Send;

    /// Platform interrupt controller interface.
    type InterruptController: Send;

    /// Create a new vCPU with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if vCPU creation fails (e.g., insufficient resources,
    /// unsupported configuration).
    fn create_vcpu(&self, config: &VCpuConfig) -> HalResult<Self::VCpu>;

    /// Enter the guest and run until the next VM exit.
    ///
    /// # Errors
    ///
    /// Returns an error if the vCPU execution fails (e.g., hardware error,
    /// invalid state).
    fn run_vcpu(&self, vcpu: &mut Self::VCpu) -> HalResult<VmExit>;

    /// Process a VM exit, returning `true` if the guest should continue.
    ///
    /// # Errors
    ///
    /// Returns an error if the exit handler fails (e.g., invalid operation,
    /// unsupported exit reason).
    fn handle_exit(&self, vcpu: &mut Self::VCpu, exit: &VmExit) -> HalResult<bool>;

    /// Map a region of guest physical memory.
    ///
    /// # Arguments
    ///
    /// * `guest_addr` — guest physical base address (page-aligned).
    /// * `host_addr`  — host virtual address backing the region.
    /// * `size`       — region size in bytes (page-aligned).
    /// * `writable`   — whether the guest may write to this region.
    ///
    /// # Errors
    ///
    /// Returns an error if memory mapping fails (e.g., invalid address,
    /// insufficient memory).
    fn map_guest_memory(
        &self,
        page_table: &mut Self::PageTable,
        guest_addr: u64,
        host_addr: u64,
        size: u64,
        writable: bool,
    ) -> HalResult<()>;

    /// Inject an interrupt / exception into the vCPU.
    ///
    /// # Arguments
    ///
    /// * `irq` — interrupt vector number.
    ///
    /// # Errors
    ///
    /// Returns an error if interrupt injection fails (e.g., invalid vector,
    /// vCPU in unsupported state).
    fn inject_interrupt(
        &self,
        vcpu: &mut Self::VCpu,
        controller: &Self::InterruptController,
        irq: u32,
    ) -> HalResult<()>;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    // -- Unit tests for VCpuConfig ------------------------------------------

    #[test]
    fn vcpu_config_defaults() {
        let cfg = VCpuConfig {
            id: 0,
            entry_addr: 0x1000,
            stack_addr: 0x8000,
            boot_arg: 0,
        };
        assert_eq!(cfg.id, 0);
        assert_eq!(cfg.entry_addr, 0x1000);
    }

    #[test]
    fn vcpu_config_clone() {
        let cfg = VCpuConfig {
            id: 3,
            entry_addr: 0xFFFF,
            stack_addr: 0xAAAA,
            boot_arg: 42,
        };
        let cfg2 = cfg.clone();
        assert_eq!(cfg2.id, cfg.id);
        assert_eq!(cfg2.boot_arg, 42);
    }

    // -- Unit tests for VmExit ----------------------------------------------

    #[test]
    fn vm_exit_display() {
        let exit = VmExit::IoIn {
            port: 0x3F8,
            size: 1,
        };
        assert!(exit.to_string().contains("0x03f8"));

        let exit = VmExit::Hlt;
        assert_eq!(exit.to_string(), "Hlt");

        let exit = VmExit::Unknown(99);
        assert_eq!(exit.to_string(), "Unknown(99)");
    }

    #[test]
    fn vm_exit_equality() {
        assert_eq!(VmExit::Hlt, VmExit::Hlt);
        assert_eq!(VmExit::Shutdown, VmExit::Shutdown);
        assert_ne!(VmExit::Hlt, VmExit::Shutdown);
        assert_eq!(
            VmExit::IoOut {
                port: 0x60,
                size: 1,
                data: 0xAB
            },
            VmExit::IoOut {
                port: 0x60,
                size: 1,
                data: 0xAB
            },
        );
    }

    // -- Unit tests for HalError --------------------------------------------

    #[test]
    fn hal_error_display() {
        let e = HalError::VCpuCreation("out of resources".into());
        assert!(e.to_string().contains("out of resources"));

        let e = HalError::Unsupported("AVX-512".into());
        assert!(e.to_string().contains("AVX-512"));
    }

    // -- Trait object-safety check ------------------------------------------

    /// Compile-time proof that `HypervisorBackend` is usable as a trait bound.
    /// We define a dummy backend and exercise it.
    struct DummyVCpu;
    struct DummyPageTable;
    struct DummyInterruptCtrl;

    struct DummyBackend;

    impl HypervisorBackend for DummyBackend {
        type VCpu = DummyVCpu;
        type PageTable = DummyPageTable;
        type InterruptController = DummyInterruptCtrl;

        fn create_vcpu(&self, _config: &VCpuConfig) -> HalResult<Self::VCpu> {
            Ok(DummyVCpu)
        }

        fn run_vcpu(&self, _vcpu: &mut Self::VCpu) -> HalResult<VmExit> {
            Ok(VmExit::Hlt)
        }

        fn handle_exit(&self, _vcpu: &mut Self::VCpu, exit: &VmExit) -> HalResult<bool> {
            match exit {
                VmExit::Hlt | VmExit::Shutdown => Ok(false),
                _ => Ok(true),
            }
        }

        fn map_guest_memory(
            &self,
            _pt: &mut Self::PageTable,
            _guest: u64,
            _host: u64,
            _size: u64,
            _writable: bool,
        ) -> HalResult<()> {
            Ok(())
        }

        fn inject_interrupt(
            &self,
            _vcpu: &mut Self::VCpu,
            _ctrl: &Self::InterruptController,
            _irq: u32,
        ) -> HalResult<()> {
            Ok(())
        }
    }

    // Verify Send + Sync bounds are satisfied.
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn dummy_backend_is_send_sync() {
        assert_send_sync::<DummyBackend>();
    }

    #[test]
    fn dummy_backend_create_and_run() {
        let backend = DummyBackend;
        let cfg = VCpuConfig {
            id: 0,
            entry_addr: 0x1000,
            stack_addr: 0x8000,
            boot_arg: 0,
        };
        let mut vcpu = backend.create_vcpu(&cfg).unwrap();
        let exit = backend.run_vcpu(&mut vcpu).unwrap();
        assert_eq!(exit, VmExit::Hlt);

        let cont = backend.handle_exit(&mut vcpu, &exit).unwrap();
        assert!(!cont); // Hlt → stop
    }

    #[test]
    fn dummy_backend_map_memory() {
        let backend = DummyBackend;
        let mut pt = DummyPageTable;
        backend
            .map_guest_memory(&mut pt, 0x0, 0x1000_0000, 4096, true)
            .unwrap();
    }

    #[test]
    fn dummy_backend_inject_interrupt() {
        let backend = DummyBackend;
        let cfg = VCpuConfig {
            id: 0,
            entry_addr: 0,
            stack_addr: 0,
            boot_arg: 0,
        };
        let mut vcpu = backend.create_vcpu(&cfg).unwrap();
        let ctrl = DummyInterruptCtrl;
        backend.inject_interrupt(&mut vcpu, &ctrl, 0x20).unwrap();
    }
}
