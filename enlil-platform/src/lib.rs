#![cfg_attr(feature = "platform-baremetal", no_std)]
// The bare-metal interrupt event source (`async_rt::interrupts`) defines real
// `extern "x86-interrupt"` handlers, which still need the language feature on
// this toolchain. Gated exactly like that module (bare-metal backend on
// x86-64, plus host test builds so the pure dispatch logic is unit-tested).
#![cfg_attr(
    all(target_arch = "x86_64", any(feature = "platform-baremetal", test)),
    feature(abi_x86_interrupt)
)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery)]
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
//!
//! # `no_std` on bare metal
//!
//! Under the `platform-baremetal` feature the crate is `#![no_std]` + `alloc`
//! so it cross-compiles for the custom `x86_64-unknown-enlil` target (Phase
//! 1.2). The modules whose backend is already host-agnostic — [`memory`]
//! (buddy/slab/heap + the `map`/`paging` builders), [`sync`] (spin-backed
//! Mutex/RwLock/Condvar + the bounded MPSC channel), [`time`], [`io`]
//! (whose [`PlatformIo`](io::PlatformIo) trait reports the `no_std`
//! [`IoError`](io::IoError) instead of `std::io::Error`), and [`async_rt`]
//! (executor + reactor on `alloc`'s `BTreeMap`, `spin` locks, and
//! `alloc::task`'s `Wake` — only its Linux epoll event source and
//! [`Executor::run_with_poller`](async_rt::Executor::run_with_poller) stay
//! gated to `platform-linux`) — build for bare metal today.

extern crate alloc;

pub mod memory;
pub mod sync;
pub mod threading;
pub mod time;

pub mod async_rt;
pub mod io;
// Freestanding `memcpy`/`memmove`/`memset`/`memcmp` for the bare-metal target.
// Gated on the target OS (not just the feature) so a host build of the
// bare-metal backend never exports `memcpy` next to libc's.
#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
pub mod mem;

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
#[must_use]
pub const fn backend_name() -> &'static str {
    #[cfg(feature = "platform-linux")]
    {
        "linux"
    }
    #[cfg(feature = "platform-baremetal")]
    {
        "baremetal"
    }
    #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
    {
        "none"
    }
}

#[cfg(feature = "platform-linux")]
fn linux_init() {
    // Linux backend: environment is already set up by the OS.
    // We just validate we have the capabilities we need.
    log::debug!("Linux platform backend: validating environment");
}

#[cfg(feature = "platform-baremetal")]
fn baremetal_init() {
    // Bare-metal backend: full hardware initialization (Phase 6.2).
    //
    // The first step, once the boot payload's `BootHandoff` is threaded in, is
    // to install the global heap from the firmware memory map via
    // `memory::init_global_heap_from_uefi` — every later step (per-CPU areas,
    // IDT/APIC, the VMX/SVM backend) needs an allocator. GDT/IDT/page-table/APIC
    // bring-up follows.
    log::debug!("Bare-metal platform backend: hardware init (stub)");
}
