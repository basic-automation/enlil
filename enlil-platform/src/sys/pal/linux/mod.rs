//! Linux PAL: `std::sys`-style bindings backed by the hosted OS.
//!
//! Thin, same-signature wrappers over `std` and `libc` so portable code in
//! `enlil-std` and elsewhere programs against `sys::{area}` without caring
//! which backend is active. The twin implementations live in
//! [`super::enlil`].

pub mod env;
pub mod fs;
pub mod io;
pub mod net;
pub mod os;
pub mod process;
pub mod random;
pub mod sync;
pub mod thread;
pub mod time;
