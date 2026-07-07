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

/// Size of the `boot_params` "zero page" the kernel is entered with.
pub const BOOT_PARAMS_SIZE: usize = 4096;

// Offsets within `boot_params` (Linux boot protocol / `struct boot_params`).
/// `e820_entries` — number of E820 map entries (`u8`).
const BP_E820_ENTRIES: usize = 0x1E8;
/// Start of the setup header inside `boot_params` (mirrors the `bzImage` layout).
const BP_SETUP_HEADER: usize = 0x1F1;
/// End of the setup header region copied from the image (exclusive).
const BP_SETUP_HEADER_END: usize = 0x268;
/// `type_of_loader` (`u8`).
const BP_TYPE_OF_LOADER: usize = 0x210;
/// `ramdisk_image` (`u32`) — guest-physical address of the initrd (0 = none).
const BP_RAMDISK_IMAGE: usize = 0x218;
/// `ramdisk_size` (`u32`) — initrd size in bytes.
const BP_RAMDISK_SIZE: usize = 0x21C;
/// `cmd_line_ptr` (`u32`) — guest-physical address of the NUL-terminated cmdline.
const BP_CMD_LINE_PTR: usize = 0x228;
/// Start of the E820 table — an array of 20-byte `(addr u64, size u64, type u32)`.
const BP_E820_TABLE: usize = 0x2D0;
/// One E820 map entry as `boot_params` stores it.
const E820_ENTRY_SIZE: usize = 20;
/// Max E820 entries the zero page holds (`E820_MAX_ENTRIES_ZEROPAGE`).
const MAX_E820_ENTRIES: usize = 128;
/// `type_of_loader` value for an undefined/unregistered bootloader.
const LOADER_TYPE_UNDEFINED: u8 = 0xFF;

/// Build the `boot_params` "zero page" for a `bzImage`: copy the setup header
/// from the image, mark an undefined bootloader, point `cmd_line_ptr` at the
/// guest cmdline, record the initrd (`ramdisk_image`/`ramdisk_size`, both 0 for
/// none), and write the E820 memory map. `e820` entries are `(base, size, type)`
/// using the E820 type codes (1 usable, 2 reserved, …).
///
/// This is the block the protected-mode kernel is entered with (RSI → its
/// guest-physical address).
///
/// Returns `None` if `image` is too short to contain the setup header.
#[must_use]
pub fn build_boot_params(
    image: &[u8],
    cmd_line_ptr: u32,
    ramdisk_image: u32,
    ramdisk_size: u32,
    e820: &[(u64, u64, u32)],
) -> Option<[u8; BOOT_PARAMS_SIZE]> {
    let header = image.get(BP_SETUP_HEADER..BP_SETUP_HEADER_END)?;
    let mut bp = [0u8; BOOT_PARAMS_SIZE];
    // Copy the setup header first; the overrides below sit inside its range.
    bp[BP_SETUP_HEADER..BP_SETUP_HEADER_END].copy_from_slice(header);
    bp[BP_TYPE_OF_LOADER] = LOADER_TYPE_UNDEFINED;
    bp[BP_CMD_LINE_PTR..BP_CMD_LINE_PTR + 4].copy_from_slice(&cmd_line_ptr.to_le_bytes());
    bp[BP_RAMDISK_IMAGE..BP_RAMDISK_IMAGE + 4].copy_from_slice(&ramdisk_image.to_le_bytes());
    bp[BP_RAMDISK_SIZE..BP_RAMDISK_SIZE + 4].copy_from_slice(&ramdisk_size.to_le_bytes());

    let n = e820.len().min(MAX_E820_ENTRIES);
    bp[BP_E820_ENTRIES] = u8::try_from(n).unwrap_or(u8::MAX);
    for (i, &(base, size, kind)) in e820.iter().take(n).enumerate() {
        let off = BP_E820_TABLE + i * E820_ENTRY_SIZE;
        bp[off..off + 8].copy_from_slice(&base.to_le_bytes());
        bp[off + 8..off + 16].copy_from_slice(&size.to_le_bytes());
        bp[off + 16..off + 20].copy_from_slice(&kind.to_le_bytes());
    }
    Some(bp)
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
    fn builds_boot_params_with_header_cmdline_and_e820() {
        // A header big enough to hold the full setup-header region, with a couple
        // of recognizable field values to prove the copy.
        let mut img = vec![0u8; 0x300];
        img[0x1FE..0x200].copy_from_slice(&BOOT_FLAG.to_le_bytes());
        img[0x202..0x206].copy_from_slice(SETUP_HEADER_MAGIC);
        img[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
        img[0x1F1] = 0x04; // setup_sects
        img[0x211] = 0x81; // loadflags — a header field that must survive the copy

        let bp = build_boot_params(
            &img,
            0x9_0000,
            0x800_0000,
            0x20_0000,
            &[(0, 0xA_0000, 1), (0x10_0000, 0x1000_0000, 1)],
        )
        .expect("header long enough");

        // Setup-header fields copied verbatim.
        assert_eq!(bp[0x1F1], 0x04, "setup_sects copied");
        assert_eq!(bp[0x211], 0x81, "loadflags copied");
        // Overrides applied after the copy.
        assert_eq!(bp[0x210], LOADER_TYPE_UNDEFINED, "type_of_loader set");
        assert_eq!(
            u32::from_le_bytes(bp[0x228..0x22C].try_into().unwrap()),
            0x9_0000,
            "cmd_line_ptr set"
        );
        assert_eq!(
            u32::from_le_bytes(bp[0x218..0x21C].try_into().unwrap()),
            0x800_0000,
            "ramdisk_image set"
        );
        assert_eq!(
            u32::from_le_bytes(bp[0x21C..0x220].try_into().unwrap()),
            0x20_0000,
            "ramdisk_size set"
        );
        // E820 map: two entries, first is [0, 0xA0000) usable.
        assert_eq!(bp[0x1E8], 2, "e820_entries count");
        assert_eq!(u64::from_le_bytes(bp[0x2D0..0x2D8].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(bp[0x2D8..0x2E0].try_into().unwrap()),
            0xA_0000
        );
        assert_eq!(u32::from_le_bytes(bp[0x2E0..0x2E4].try_into().unwrap()), 1);
    }

    #[test]
    fn build_boot_params_rejects_a_short_image() {
        assert!(build_boot_params(&[0u8; 0x100], 0, 0, 0, &[]).is_none());
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
