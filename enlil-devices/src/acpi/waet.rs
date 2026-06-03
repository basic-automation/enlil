//! WAET (Windows ACPI Emulated devices Table) builder
//!
//! Tells Windows that the RTC and PM timer are emulated/virtualized,
//! allowing it to skip expensive calibration loops during boot.
//! This significantly speeds up Windows boot in VMs.

use super::tables::{AcpiSdtHeader, OemInfo};

/// WAET emulation flags
pub const WAET_RTC_EMULATED: u32 = 1 << 0;
/// PM timer is emulated (no need for triple-read workaround)
pub const WAET_PM_TIMER_EMULATED: u32 = 1 << 1;

/// WAET table builder
pub struct WaetBuilder {
    oem: OemInfo,
    emulated_device_flags: u32,
}

impl WaetBuilder {
    /// Create a new WAET builder with both RTC and PM timer marked as emulated
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            emulated_device_flags: WAET_RTC_EMULATED | WAET_PM_TIMER_EMULATED,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub const fn flags(mut self, flags: u32) -> Self {
        self.emulated_device_flags = flags;
        self
    }

    /// Build the WAET table as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let total_length: u32 = 40; // 36-byte header + 4-byte flags
        let mut buf = Vec::with_capacity(total_length as usize);

        let header = AcpiSdtHeader::new(*b"WAET", total_length, 1, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Offset 36: Emulated Device Flags (4 bytes)
        buf.extend_from_slice(&self.emulated_device_flags.to_le_bytes());

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for WaetBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waet_builds() {
        let waet = WaetBuilder::new().build();
        assert_eq!(waet.len(), 40);
        assert_eq!(&waet[0..4], b"WAET");
    }

    #[test]
    fn waet_checksum() {
        let waet = WaetBuilder::new().build();
        let sum: u8 = waet.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn waet_flags() {
        let waet = WaetBuilder::new().build();
        let flags = u32::from_le_bytes(waet[36..40].try_into().unwrap());
        assert_eq!(flags, WAET_RTC_EMULATED | WAET_PM_TIMER_EMULATED);
    }

    #[test]
    fn waet_custom_flags() {
        let waet = WaetBuilder::new().flags(WAET_RTC_EMULATED).build();
        let flags = u32::from_le_bytes(waet[36..40].try_into().unwrap());
        assert_eq!(flags, WAET_RTC_EMULATED);
    }
}
