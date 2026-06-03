#![deny(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(clippy::module_name_repetitions)]

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
