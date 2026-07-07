//! Linux `bzImage` boot-protocol parsing — the first step of the guest kernel
//! loader (items 3.12 / 5.6).
//!
//! A `bzImage` begins with a real-mode setup area whose *setup header* (the Linux
//! boot protocol, `Documentation/x86/boot.rst`) describes how to load the
//! protected-mode kernel that follows it. This module parses that header;
//! writing the protected-mode kernel and a `boot_params` block into guest RAM
//! and entering the kernel is the next slice (it can build on
//! `GuestRuntime::write_guest_bytes`).
//!
//! Pure parsing — no KVM — so it is unit-testable without hardware.

/// Setup-header magic `"HdrS"` at offset `0x202`.
pub const SETUP_HEADER_MAGIC: &[u8; 4] = b"HdrS";
/// Boot signature `0xAA55` at offset `0x1FE` of the boot sector.
pub const BOOT_FLAG: u16 = 0xAA55;
/// Where a `bzImage`'s protected-mode kernel is loaded in guest memory (1 MiB) —
/// the fixed load address for a "big" kernel in the Linux boot protocol.
pub const PROTECTED_MODE_LOAD_ADDR: u64 = 0x10_0000;

/// Parsed fields from a `bzImage` setup header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BzImageInfo {
    /// Boot-protocol version, `major.minor` packed as `(major << 8) | minor`
    /// (e.g. `0x020F` = 2.15).
    pub protocol_version: u16,
    /// Number of 512-byte setup sectors preceding the protected-mode kernel (the
    /// on-disk `setup_sects`, with the legacy `0` normalized to `4`).
    pub setup_sects: u8,
    /// Byte offset within the image where the protected-mode kernel begins:
    /// `(setup_sects + 1) * 512` (the boot sector plus the setup sectors).
    pub protected_mode_kernel_offset: usize,
}

/// Parse a `bzImage`'s setup header.
///
/// Returns `None` if the image is too short to contain the header, lacks the
/// `0xAA55` boot signature, or lacks the `"HdrS"` magic (i.e. it is not a
/// boot-protocol kernel image).
#[must_use]
pub fn parse_bzimage_header(image: &[u8]) -> Option<BzImageInfo> {
    // Need through the protocol-version field at 0x206..0x208.
    if image.len() < 0x208 {
        return None;
    }
    if u16::from_le_bytes([image[0x1FE], image[0x1FF]]) != BOOT_FLAG {
        return None;
    }
    if &image[0x202..0x206] != SETUP_HEADER_MAGIC {
        return None;
    }
    let protocol_version = u16::from_le_bytes([image[0x206], image[0x207]]);
    // setup_sects at 0x1F1; the legacy value 0 means 4 sectors.
    let setup_sects = match image[0x1F1] {
        0 => 4,
        n => n,
    };
    let protected_mode_kernel_offset = (usize::from(setup_sects) + 1) * 512;
    Some(BzImageInfo {
        protocol_version,
        setup_sects,
        protected_mode_kernel_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal, valid `bzImage`-like header: boot signature, `"HdrS"`
    /// magic, a protocol version, and a `setup_sects` count.
    fn header(setup_sects: u8, version: u16) -> Vec<u8> {
        let mut img = vec![0u8; 0x208];
        img[0x1F1] = setup_sects;
        img[0x1FE..0x200].copy_from_slice(&BOOT_FLAG.to_le_bytes());
        img[0x202..0x206].copy_from_slice(SETUP_HEADER_MAGIC);
        img[0x206..0x208].copy_from_slice(&version.to_le_bytes());
        img
    }

    #[test]
    fn parses_a_valid_header() {
        let info = parse_bzimage_header(&header(4, 0x020F)).expect("valid header");
        assert_eq!(info.protocol_version, 0x020F);
        assert_eq!(info.setup_sects, 4);
        // (4 + 1) * 512 = 0xA00.
        assert_eq!(info.protected_mode_kernel_offset, 0xA00);
    }

    #[test]
    fn setup_sects_zero_normalizes_to_four() {
        let info = parse_bzimage_header(&header(0, 0x0200)).unwrap();
        assert_eq!(info.setup_sects, 4, "legacy 0 means 4 setup sectors");
        assert_eq!(info.protected_mode_kernel_offset, 0xA00);
    }

    #[test]
    fn rejects_a_non_kernel_image() {
        // Right length but no boot signature / magic.
        assert!(parse_bzimage_header(&vec![0u8; 0x208]).is_none());

        // Boot signature but wrong magic.
        let mut img = header(1, 0x0200);
        img[0x202..0x206].copy_from_slice(b"XXXX");
        assert!(parse_bzimage_header(&img).is_none());

        // Too short.
        assert!(parse_bzimage_header(&[0u8; 16]).is_none());
    }
}
