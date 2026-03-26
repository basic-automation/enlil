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
//! ## Phase 1.11 Milestone
//!
//! This crate proves that our platform layer can support the full `std` API
//! surface needed by the hypervisor: threads, sync primitives, collections,
//! async/await, time, and formatted I/O.

pub mod thread;
pub mod sync;
pub mod time;
pub mod io;
pub mod collections;
pub mod future;

/// Re-export the platform layer for direct access when needed.
pub use enlil_platform as platform;
