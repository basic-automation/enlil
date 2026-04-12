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

