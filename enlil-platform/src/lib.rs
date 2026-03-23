//! Enlil Platform Layer
//!
//! This crate provides the foundational platform abstractions that enable
//! full `std`-like functionality for the Enlil hypervisor, whether running
//! on Linux (KVM-backed development) or bare-metal (production).
//!
//! # Architecture
//!
//! All Enlil crates build against this platform layer rather than directly
//! against OS primitives. The platform layer has two backends, selected
//! at compile time via Cargo features:
//!
//! - `platform-linux`: Backends to Linux syscalls (mmap, pthreads, futex, etc.)
//! - `platform-baremetal`: Backends to bare-metal primitives (buddy allocator,
//!   per-CPU scheduler, spinlocks, TSC, etc.)
//!
//! This means all code above the platform layer is identical in both modes.

pub mod memory;
pub mod threading;
pub mod sync;
pub mod async_rt;
pub mod time;
pub mod io;

/// Platform initialization — must be called before any other platform services.
///
/// On Linux: validates environment, initializes logging backend.
/// On bare-metal: sets up GDT, IDT, page tables, APIC, per-CPU areas.
pub fn init() {
    #[cfg(feature = "platform-linux")]
    linux_init();

    #[cfg(feature = "platform-baremetal")]
    baremetal_init();

    log::info!("enlil-platform initialized (backend: {})", backend_name());
}

/// Returns the name of the active platform backend.
pub fn backend_name() -> &'static str {
    #[cfg(feature = "platform-linux")]
    { "linux" }
    #[cfg(feature = "platform-baremetal")]
    { "baremetal" }
    #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
    { "none" }
}

#[cfg(feature = "platform-linux")]
fn linux_init() {
    // Linux backend: environment is already set up by the OS.
    // We just validate we have the capabilities we need.
    log::debug!("Linux platform backend: validating environment");
}

#[cfg(feature = "platform-baremetal")]
fn baremetal_init() {
    // Bare-metal backend: full hardware initialization.
    // This is stubbed for Phase 1 — real implementation in Phase 6.
    log::debug!("Bare-metal platform backend: hardware init (stub)");
}
