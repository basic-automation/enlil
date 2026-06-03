#![deny(clippy::all, clippy::pedantic, clippy::nursery)]
// Curated allow-list for this low-level device-emulation crate. These
// pedantic/nursery lints are either inherent to hardware/register code or are
// low-value churn here; correctness lints (clippy::all) stay denied. Kept
// explicit (rather than dropping the deny groups) so new code is still linted.
#![allow(clippy::module_name_repetitions)]
// Register/byte arithmetic intentionally narrows widths (e.g. u32 field -> u8).
#![allow(clippy::cast_possible_truncation)]
// `from`/`to`, `lo`/`hi` etc. are the natural names in device/register code.
#![allow(clippy::similar_names)]
// Device methods keep `&self` for API/trait consistency and future state even
// when the current body does not read it.
#![allow(clippy::unused_self)]
#![allow(clippy::trivially_copy_pass_by_ref)]
#![allow(clippy::needless_pass_by_value)]
// Most `unwrap()`s here are on infallible fixed-size conversions; documenting a
// `# Panics` section on each would be noise. (Tracked as a doc-polish follow-up.)
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]
// `if let … else` is frequently clearer than `map_or` for register dispatch.
#![allow(clippy::option_if_let_else)]
#![allow(clippy::significant_drop_tightening)]
// Register/command decode tables deliberately keep one match arm per case even
// when bodies coincide, and keep stable `Result`/`Option` signatures.
#![allow(clippy::match_same_arms)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::branches_sharing_code)]
#![allow(clippy::too_many_lines)]
// Building long fixed device tables (e.g. HDA widget lists) reads more clearly
// as `Vec::new()` + pushes than a giant `vec![]` literal.
#![allow(clippy::vec_init_then_push)]

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
pub mod display;
pub mod fw_cfg;
pub mod hda;
pub mod interrupt;
pub mod net;
pub mod pcie;
pub mod ps2;
pub mod smbios;
pub mod stealth;
pub mod storage;
pub mod timer;
pub mod tpm;
pub mod usb;
pub mod virtio;
