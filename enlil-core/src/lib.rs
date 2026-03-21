//! Enlil Core — hypervisor core logic
//!
//! This crate contains the VMM entry point, vCPU management, memory layout,
//! and the abstractions that sit above the hardware virtualization layer
//! (KVM on Linux dev, bare-metal VMX/SVM in production).

pub mod cpuid;
pub mod error;
pub mod memory;
pub mod serial;
pub mod vcpu;
pub mod vm;

pub use error::{Error, Result};
