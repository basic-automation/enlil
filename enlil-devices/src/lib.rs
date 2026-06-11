#![deny(clippy::all, clippy::pedantic, clippy::nursery)]
// Curated allow-list for this low-level device-emulation crate. These
// pedantic/nursery lints are either inherent to hardware/register code or are
// low-value churn here; correctness lints (clippy::all) stay denied. Kept
// explicit (rather than dropping the deny groups) so new code is still linted.
// Register/byte arithmetic intentionally narrows widths (e.g. u32 field -> u8).
// `from`/`to`, `lo`/`hi` etc. are the natural names in device/register code.
// Device methods keep `&self` for API/trait consistency and future state even
// when the current body does not read it.
// Most `unwrap()`s here are on infallible fixed-size conversions; documenting a
// `# Panics` section on each would be noise. (Tracked as a doc-polish follow-up.)
// `if let … else` is frequently clearer than `map_or` for register dispatch.
// Register/command decode tables deliberately keep one match arm per case even
// when bodies coincide, and keep stable `Result`/`Option` signatures.
// Building long fixed device tables (e.g. HDA widget lists) reads more clearly
// as `Vec::new()` + pushes than a giant `vec![]` literal.

//! enlil-devices: Virtual device backends
//!
//! This crate implements the virtual hardware layer that guests interact with:
//! - ACPI table synthesis (RSDP, XSDT, FADT, MADT, DSDT, SSDT, MCFG, HPET)
//! - SMBIOS/DMI table synthesis
//! - Block devices (VirtIO-blk)
//! - Network devices (VirtIO-net, virtual switch)
//! - Interrupt controllers (LAPIC, IOAPIC, MSI)
//! - Timer devices (PIT, HPET, TSC, paravirt clocks)
//! - Display compositor (Enlil Zones)
//! - Inter-guest communication (Enlil Bridge)
//! - `VirtIO` transport layer
//! - Device bus abstractions (PIO/MMIO dispatch)
//! - USB routing and virtual xHCI
//! - Virtual TPM 2.0
//! - PS/2 keyboard/mouse
//! - Intel HDA audio controller
//! - PCI Express root complex
//! - QEMU `fw_cfg` firmware configuration

pub mod acpi;
pub mod block;
pub mod bridge;
pub mod bus;
pub mod chipset;
pub mod crypto;
pub mod display;
pub mod dma;
pub mod fw_cfg;
pub mod hda;
pub mod interrupt;
pub mod net;
pub mod pcie;
pub mod ps2;
pub mod smbios;
pub mod smbus;
pub mod stealth;
pub mod storage;
pub mod timer;
pub mod tpm;
mod truncate;
pub mod usb;
pub mod virtio;
