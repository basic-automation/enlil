//! KVM-backed hypervisor implementation for Linux.
//!
//! This module provides the concrete hypervisor backend for KVM, connecting
//! the abstract HypervisorBackend trait to the Linux KVM API. Includes a
//! full run-loop with exit dispatch, memory region management, dirty page
//! logging, register access, CPUID configuration, and MSR filtering.

#[cfg(target_os = "linux")]
use kvm_bindings::{
    kvm_regs, kvm_sregs, kvm_userspace_memory_region, CpuId,
    KVM_API_VERSION, KVM_MEM_LOG_DIRTY_PAGES,
};
#[cfg(target_os = "linux")]
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};

#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Arc;

use crate::error::Error;

use log::{debug, error, info, trace, warn};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ KvmExit ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Exit reasons from KVM, converted from `kvm_ioctls::VcpuExit` into
/// an owned enum that can be passed across function boundaries.
#[derive(Debug, Clone)]
pub enum KvmExit {
    /// IN instruction -- caller must supply the response data.
    IoIn { port: u16, size: u8 },
    /// OUT instruction -- data is copied from the shared kvm_run page.
    IoOut { port: u16, size: u8, data: Vec<u8> },
    /// MMIO read -- caller must supply the response data.
    MmioRead { address: u64, size: u8 },
    /// MMIO write -- data is copied from the shared kvm_run page.
    MmioWrite { address: u64, size: u8, data: Vec<u8> },
    /// HLT instruction.
    Halt,
    /// Triple-fault / shutdown.
    Shutdown,
    /// CPUID instruction (only when trapping is active).
    Cpuid { leaf: u32, subleaf: u32 },
    /// RDMSR exit (requires MSR filtering).
    Rdmsr { index: u32 },
    /// WRMSR exit (requires MSR filtering).
    Wrmsr { index: u32, data: u64 },
    /// EPT / memory-fault violation.
    EptViolation { flags: u64, gpa: u64, size: u64 },
    /// Hypercall (VMCALL / VMMCALL).
    Hypercall { nr: u64, args: [u64; 3] },
    /// Debug breakpoint / single-step.
    Debug { pc: u64 },
    /// KVM internal error.
    InternalError,
    /// Failed VM entry.
    FailEntry { hardware_entry_failure_reason: u64, cpu: u32 },
    /// System event (e.g. S3/S4 from ACPI).
    SystemEvent { event_type: u32 },
    /// Anything else we do not handle.
    Other(String),
}
