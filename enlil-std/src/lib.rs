#![cfg_attr(feature = "platform-baremetal", no_std)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! # enlil-std — Standard Library Compatibility Layer
//!
//! This crate provides `std`-compatible APIs backed by `enlil-platform`.
//! It serves as the bridge between Rust's standard library API surface
//! and our custom platform implementations.
//!
//! ## Usage
//!
//! Instead of `use std::sync::Mutex`, use `use enlil_std::sync::Mutex`.
//! The APIs are intentionally identical so code can migrate between them.
//!
//! ## `no_std` on bare metal
//!
//! Under the `platform-baremetal` feature the crate is `#![no_std]` + `alloc`
//! so the same source builds for the custom `x86_64-unknown-enlil` target
//! (Phase 1.11): collections fall back to `alloc` (plus `hashbrown` for
//! `HashMap`/`HashSet`), `Arc`/`Weak` to `alloc::sync`, atomics to
//! `core::sync::atomic`, and thread spawning to the platform's
//! `BareMetalScheduler`.
//!
//! ## Phase 1.11 Milestone
//!
//! This crate proves that our platform layer can support the full `std` API
//! surface needed by the hypervisor: threads, sync primitives, collections,
//! async/await, time, and formatted I/O.

#[cfg(feature = "platform-baremetal")]
extern crate alloc;

pub mod collections;
pub mod future;
pub mod io;
pub mod sync;
pub mod thread;
pub mod time;

/// Re-export the platform layer for direct access when needed.
pub use enlil_platform as platform;
