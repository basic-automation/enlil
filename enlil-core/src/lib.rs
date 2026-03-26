#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! Enlil hypervisor core — VMM entry, vCPU management, memory partitioning.

pub mod affinity;
pub mod cpuid;
pub mod ept;
pub mod error;
pub mod memory;
pub mod serial;
pub mod vcpu;
pub mod vm;

pub use error::{Error, Result};
