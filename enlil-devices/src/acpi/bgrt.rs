//! BGRT (Boot Graphics Resource Table) builder
//!
//! Windows expects this table to locate the OEM boot logo image. It contains
//! a pointer to a BMP image in memory, display coordinates, and status flags.
//! When no image is configured (address 0) the table is still emitted, but its
//! "Displayed" status bit is cleared so it does not claim a boot graphic was
//! shown from physical address 0.

use super::tables::{AcpiSdtHeader, OemInfo};

/// BGRT image type
pub const BGRT_IMAGE_TYPE_BMP: u8 = 0;

/// BGRT status: image is valid and was displayed
pub const BGRT_STATUS_DISPLAYED: u8 = 1;

/// BGRT table builder
pub struct BgrtBuilder {
    oem: OemInfo,
    version: u16,
    status: u8,
    image_type: u8,
    image_address: u64,
    image_offset_x: u32,
    image_offset_y: u32,
}

impl BgrtBuilder {
    /// Create a new BGRT builder with sensible defaults
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            version: 1,
            status: BGRT_STATUS_DISPLAYED,
            image_type: BGRT_IMAGE_TYPE_BMP,
            image_address: 0,
            image_offset_x: 0,
            image_offset_y: 0,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub const fn image_address(mut self, addr: u64) -> Self {
        self.image_address = addr;
        self
    }

    #[must_use]
    pub const fn image_offset(mut self, x: u32, y: u32) -> Self {
        self.image_offset_x = x;
        self.image_offset_y = y;
        self
    }

    #[must_use]
    pub const fn status(mut self, status: u8) -> Self {
        self.status = status;
        self
    }

    /// Build the BGRT table as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        // BGRT is 56 bytes: 36-byte header + 20 bytes of fields
        let total_length: u32 = 56;
        let mut buf = Vec::with_capacity(total_length as usize);

        let header = AcpiSdtHeader::new(*b"BGRT", total_length, 1, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Offset 36: Version (2 bytes)
        buf.extend_from_slice(&self.version.to_le_bytes());

        // Offset 38: Status (1 byte). The "Displayed" bit asserts a boot graphic
        // was actually shown; with no image (address 0) that is incoherent — a
        // BGRT consumer would try to read a BMP at physical address 0 — so clear
        // it unless a real image is present. A BGRT with Displayed=0 is the valid
        // "no boot logo shown" state.
        let status = if self.image_address == 0 {
            self.status & !BGRT_STATUS_DISPLAYED
        } else {
            self.status
        };
        buf.push(status);

        // Offset 39: Image Type (1 byte)
        buf.push(self.image_type);

        // Offset 40: Image Address (8 bytes)
        buf.extend_from_slice(&self.image_address.to_le_bytes());

        // Offset 48: Image Offset X (4 bytes)
        buf.extend_from_slice(&self.image_offset_x.to_le_bytes());

        // Offset 52: Image Offset Y (4 bytes)
        buf.extend_from_slice(&self.image_offset_y.to_le_bytes());

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for BgrtBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgrt_builds() {
        let bgrt = BgrtBuilder::new().build();
        assert_eq!(bgrt.len(), 56);
        assert_eq!(&bgrt[0..4], b"BGRT");
    }

    #[test]
    fn bgrt_checksum() {
        let bgrt = BgrtBuilder::new().build();
        let sum: u8 = bgrt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn bgrt_version() {
        let bgrt = BgrtBuilder::new().build();
        let version = u16::from_le_bytes(bgrt[36..38].try_into().unwrap());
        assert_eq!(version, 1);
    }

    #[test]
    fn bgrt_status_is_not_displayed_without_an_image() {
        // Default builder has image_address 0: the Displayed bit must be clear so
        // the table does not claim a graphic was shown from address 0.
        let bgrt = BgrtBuilder::new().build();
        assert_eq!(bgrt[38] & BGRT_STATUS_DISPLAYED, 0);
        let addr = u64::from_le_bytes(bgrt[40..48].try_into().unwrap());
        assert_eq!(addr, 0);
    }

    #[test]
    fn bgrt_status_displayed_with_a_real_image() {
        // With a real image address the Displayed bit is preserved.
        let bgrt = BgrtBuilder::new().image_address(0x8000_0000).build();
        assert_eq!(bgrt[38] & BGRT_STATUS_DISPLAYED, BGRT_STATUS_DISPLAYED);
    }

    #[test]
    fn bgrt_image_type_bmp() {
        let bgrt = BgrtBuilder::new().build();
        assert_eq!(bgrt[39], BGRT_IMAGE_TYPE_BMP);
    }

    #[test]
    fn bgrt_image_address() {
        let bgrt = BgrtBuilder::new().image_address(0xDEAD_BEEF_0000).build();
        let addr = u64::from_le_bytes(bgrt[40..48].try_into().unwrap());
        assert_eq!(addr, 0xDEAD_BEEF_0000);
    }

    #[test]
    fn bgrt_custom_offset() {
        let bgrt = BgrtBuilder::new().image_offset(100, 200).build();
        let x = u32::from_le_bytes(bgrt[48..52].try_into().unwrap());
        let y = u32::from_le_bytes(bgrt[52..56].try_into().unwrap());
        assert_eq!(x, 100);
        assert_eq!(y, 200);
    }

    #[test]
    fn bgrt_length_field() {
        let bgrt = BgrtBuilder::new().build();
        let length = u32::from_le_bytes(bgrt[4..8].try_into().unwrap());
        assert_eq!(length, 56);
    }
}
