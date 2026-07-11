#![cfg_attr(target_os = "uefi", no_std)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery)]
//! enlil-boot: UEFI boot payload (Phase 6.1).
//!
//! This crate is the firmware boot application. Built for the
//! `x86_64-unknown-uefi` target it exposes an `efi_main` entry point (see
//! `main.rs`) that runs as the UEFI boot payload, collects the platform
//! resources the enlil kernel needs (memory map, ACPI RSDP, GOP
//! framebuffer), calls `ExitBootServices()`, and hands off to the kernel.
//!
//! Compiled for the Linux/Windows dev host the firmware-specific code is
//! gated out behind `cfg(target_os = "uefi")`, so the host-agnostic handoff
//! model below still builds and is unit-tested on the dev toolchain.

pub mod handoff;

/// Console/serial banner the payload emits once it has taken control.
///
/// This is the first live-boot signal the QEMU+OVMF harness asserts on the
/// serial line after `ExitBootServices()` (the Phase 6.1 sub-milestone).
pub const BOOT_BANNER: &str = "enlil kernel alive";
