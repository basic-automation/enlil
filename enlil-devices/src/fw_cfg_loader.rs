//! QEMU `etc/table-loader` (bios-linker-loader) command-stream emitter.
//!
//! `fw_cfg` only *delivers* the ACPI/SMBIOS blobs (`etc/acpi/*`,
//! `etc/smbios/*`); it cannot tell the firmware where in guest RAM to place
//! them or how to fix up the inter-table pointers, because those addresses are
//! not known until the firmware allocates the buffers. QEMU solves this with a
//! second `fw_cfg` file, `etc/table-loader`, whose contents are a stream of
//! fixed-size *bios-linker-loader* commands the firmware (OVMF/SeaBIOS)
//! executes in order:
//!
//! - **`ALLOCATE`** — read a named `fw_cfg` file into a freshly allocated,
//!   alignment-constrained buffer in a zone (high memory or the `0xF` segment).
//! - **`ADD_POINTER`** — once both files are allocated, take the little-endian
//!   pointer-sized field at `offset` inside `dest_file`, add the runtime base
//!   address of `src_file`, and write it back. This is how the placeholder
//!   zero addresses our synthesizers leave (RSDP→XSDT, XSDT→tables,
//!   FADT→DSDT/FACS, the SMBIOS 3.0 anchor→structure table) become real.
//! - **`ADD_CHECKSUM`** — recompute a one-byte checksum over a span of a file and
//!   store it at `offset`, after the pointers are patched (so the ACPI table
//!   checksums are valid for the final, relocated tables).
//! - **`WRITE_POINTER`** — write a `src_file`-relative pointer *back into a
//!   `fw_cfg` file* (used for the NVDIMM/`hardware_errors` reverse link);
//!   included for completeness.
//!
//! ## ABI (transcribed from `qemu/hw/acpi/bios-linker-loader.c`)
//!
//! Each command is a **128-byte** `QEMU_PACKED` entry: a `u32` `command` at
//! offset `0x00` followed by a 124-byte union. All multi-byte integers are
//! **little-endian** (`cpu_to_le32`). File-name fields are 56 bytes,
//! NUL-padded. The per-command field offsets are encoded in
//! [`BiosLinkerLoader`]'s builders and asserted byte-for-byte in the tests.
//!
//! This module is the pure encoder. Wiring it to the concrete `fw_cfg` file set
//! (which pointers exist, at which offsets) is the caller's job — see
//! [`crate::fw_cfg::FwCfgDevice`].

/// Size in bytes of one bios-linker-loader command entry.
pub const ENTRY_SIZE: usize = 128;

/// Size in bytes of a file-name field inside a command (NUL-padded).
pub const FILE_NAME_SIZE: usize = 56;

/// Command opcodes (`BIOS_LINKER_LOADER_COMMAND_*`).
const COMMAND_ALLOCATE: u32 = 0x1;
const COMMAND_ADD_POINTER: u32 = 0x2;
const COMMAND_ADD_CHECKSUM: u32 = 0x3;
const COMMAND_WRITE_POINTER: u32 = 0x4;

/// Allocation zone for an `ALLOCATE` command
/// (`BIOS_LINKER_LOADER_ALLOC_ZONE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AllocZone {
    /// High memory — anywhere the firmware likes (the ACPI tables).
    High = 0x1,
    /// The legacy `0xF_0000`–`0xF_FFFF` segment, below 1 MB (the RSDP, which a
    /// BIOS-era OS scans for in that window).
    FSeg = 0x2,
}

/// Builds the `etc/table-loader` bios-linker-loader command stream.
///
/// Append commands in firmware-execution order, then take the bytes with
/// [`into_bytes`](Self::into_bytes) / [`as_bytes`](Self::as_bytes) and register
/// them as the `etc/table-loader` `fw_cfg` file. The canonical order is: every
/// `ALLOCATE` first (the firmware must have all buffers before any pointer can
/// be resolved), then the `ADD_POINTER` fix-ups, then the `ADD_CHECKSUM`
/// commands last (checksums cover the already-relocated tables).
#[derive(Debug, Clone, Default)]
pub struct BiosLinkerLoader {
    stream: Vec<u8>,
}

impl BiosLinkerLoader {
    /// A new, empty command stream.
    #[must_use]
    pub const fn new() -> Self {
        Self { stream: Vec::new() }
    }

    /// Number of commands appended so far.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.stream.len() / ENTRY_SIZE
    }

    /// Whether no commands have been appended.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.stream.is_empty()
    }

    /// `ALLOCATE`: load `file` into a `align`-aligned buffer in `zone`.
    ///
    /// `align` must be a power of two; the firmware aligns the allocation to it.
    pub fn allocate(&mut self, file: &str, align: u32, zone: AllocZone) -> &mut Self {
        let mut e = Entry::new(COMMAND_ALLOCATE);
        e.put_name(0x04, file);
        e.put_u32(0x3C, align);
        e.put_u8(0x40, zone as u8);
        self.push(&e)
    }

    /// `ADD_POINTER`: relocate the `size`-byte little-endian pointer at `offset`
    /// inside `dest_file` by adding the runtime base address of `src_file`.
    ///
    /// `size` is the pointer width in bytes (1, 2, 4 or 8). Both files must have
    /// been `ALLOCATE`d already.
    pub fn add_pointer(
        &mut self,
        dest_file: &str,
        src_file: &str,
        offset: u32,
        size: u8,
    ) -> &mut Self {
        debug_assert!(
            matches!(size, 1 | 2 | 4 | 8),
            "pointer size must be 1, 2, 4 or 8 bytes"
        );
        let mut e = Entry::new(COMMAND_ADD_POINTER);
        e.put_name(0x04, dest_file);
        e.put_name(0x38, src_file);
        e.put_u32(0x6C, offset);
        e.put_u8(0x70, size);
        self.push(&e)
    }

    /// `ADD_CHECKSUM`: store, at `offset` in `file`, the one-byte two's-complement
    /// checksum over the `length` bytes of `file` beginning at `start`.
    ///
    /// Issued after the `ADD_POINTER` fix-ups so the checksum covers the final,
    /// relocated table bytes (e.g. an ACPI table's standard byte-sum checksum).
    pub fn add_checksum(&mut self, file: &str, offset: u32, start: u32, length: u32) -> &mut Self {
        let mut e = Entry::new(COMMAND_ADD_CHECKSUM);
        e.put_name(0x04, file);
        e.put_u32(0x3C, offset);
        e.put_u32(0x40, start);
        e.put_u32(0x44, length);
        self.push(&e)
    }

    /// `WRITE_POINTER`: write the runtime address of `src_file` (plus
    /// `src_offset`) into the `size`-byte field at `dst_offset` of the
    /// *`fw_cfg` file* `dest_file`, i.e. back out to the host via a `fw_cfg`
    /// DMA write. The reverse-link command (NVDIMM `hardware_errors`); included
    /// for ABI completeness.
    pub fn write_pointer(
        &mut self,
        dest_file: &str,
        src_file: &str,
        dst_offset: u32,
        src_offset: u32,
        size: u8,
    ) -> &mut Self {
        debug_assert!(
            matches!(size, 1 | 2 | 4 | 8),
            "pointer size must be 1, 2, 4 or 8 bytes"
        );
        let mut e = Entry::new(COMMAND_WRITE_POINTER);
        e.put_name(0x04, dest_file);
        e.put_name(0x38, src_file);
        e.put_u32(0x6C, dst_offset);
        e.put_u32(0x70, src_offset);
        e.put_u8(0x74, size);
        self.push(&e)
    }

    /// Borrow the encoded command stream.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.stream
    }

    /// Take the encoded command stream (the `etc/table-loader` file contents).
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.stream
    }

    fn push(&mut self, e: &Entry) -> &mut Self {
        self.stream.extend_from_slice(&e.0);
        self
    }
}

/// One 128-byte command entry, built field by field at absolute offsets.
struct Entry([u8; ENTRY_SIZE]);

impl Entry {
    fn new(command: u32) -> Self {
        let mut e = Self([0u8; ENTRY_SIZE]);
        e.put_u32(0x00, command);
        e
    }

    const fn put_u8(&mut self, off: usize, v: u8) {
        self.0[off] = v;
    }

    fn put_u32(&mut self, off: usize, v: u32) {
        self.0[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Write a NUL-padded file name into the 56-byte field at `off`. Names are
    /// truncated to 55 bytes so the field always stays NUL-terminated, matching
    /// QEMU's fixed-size name fields.
    fn put_name(&mut self, off: usize, name: &str) {
        let bytes = name.as_bytes();
        let n = bytes.len().min(FILE_NAME_SIZE - 1);
        self.0[off..off + n].copy_from_slice(&bytes[..n]);
        // Remaining bytes (incl. the terminator) are already zero.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le32(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
    }

    fn name(b: &[u8], off: usize) -> String {
        let field = &b[off..off + FILE_NAME_SIZE];
        let end = field.iter().position(|&c| c == 0).unwrap_or(FILE_NAME_SIZE);
        String::from_utf8(field[..end].to_vec()).unwrap()
    }

    #[test]
    fn empty_stream() {
        let l = BiosLinkerLoader::new();
        assert!(l.is_empty());
        assert_eq!(l.len(), 0);
        assert!(l.as_bytes().is_empty());
    }

    #[test]
    fn allocate_layout() {
        let mut l = BiosLinkerLoader::new();
        l.allocate("etc/acpi/tables", 0x40, AllocZone::High);
        let b = l.as_bytes();
        assert_eq!(b.len(), ENTRY_SIZE);
        assert_eq!(l.len(), 1);
        assert_eq!(le32(b, 0x00), COMMAND_ALLOCATE);
        assert_eq!(name(b, 0x04), "etc/acpi/tables");
        assert_eq!(le32(b, 0x3C), 0x40); // align
        assert_eq!(b[0x40], AllocZone::High as u8);
    }

    #[test]
    fn allocate_fseg_zone_value() {
        let mut l = BiosLinkerLoader::new();
        l.allocate("etc/acpi/rsdp", 0x10, AllocZone::FSeg);
        let b = l.as_bytes();
        assert_eq!(b[0x40], 0x2);
        assert_eq!(AllocZone::High as u8, 0x1);
        assert_eq!(AllocZone::FSeg as u8, 0x2);
    }

    #[test]
    fn add_pointer_layout() {
        let mut l = BiosLinkerLoader::new();
        l.add_pointer("etc/acpi/rsdp", "etc/acpi/tables", 0x10, 8);
        let b = l.as_bytes();
        assert_eq!(le32(b, 0x00), COMMAND_ADD_POINTER);
        assert_eq!(name(b, 0x04), "etc/acpi/rsdp"); // dest_file
        assert_eq!(name(b, 0x38), "etc/acpi/tables"); // src_file
        assert_eq!(le32(b, 0x6C), 0x10); // offset
        assert_eq!(b[0x70], 8); // size
    }

    #[test]
    fn add_checksum_layout() {
        let mut l = BiosLinkerLoader::new();
        l.add_checksum("etc/acpi/tables", 0x09, 0x00, 0x100);
        let b = l.as_bytes();
        assert_eq!(le32(b, 0x00), COMMAND_ADD_CHECKSUM);
        assert_eq!(name(b, 0x04), "etc/acpi/tables");
        assert_eq!(le32(b, 0x3C), 0x09); // offset (the checksum byte)
        assert_eq!(le32(b, 0x40), 0x00); // start
        assert_eq!(le32(b, 0x44), 0x100); // length
    }

    #[test]
    fn write_pointer_layout() {
        let mut l = BiosLinkerLoader::new();
        l.write_pointer("etc/hardware_errors", "etc/acpi/tables", 0x04, 0x20, 8);
        let b = l.as_bytes();
        assert_eq!(le32(b, 0x00), COMMAND_WRITE_POINTER);
        assert_eq!(name(b, 0x04), "etc/hardware_errors"); // dest_file
        assert_eq!(name(b, 0x38), "etc/acpi/tables"); // src_file
        assert_eq!(le32(b, 0x6C), 0x04); // dst_offset
        assert_eq!(le32(b, 0x70), 0x20); // src_offset
        assert_eq!(b[0x74], 8); // size
    }

    #[test]
    fn commands_concatenate_in_order() {
        let mut l = BiosLinkerLoader::new();
        l.allocate("etc/acpi/tables", 0x40, AllocZone::High)
            .allocate("etc/acpi/rsdp", 0x10, AllocZone::FSeg)
            .add_pointer("etc/acpi/rsdp", "etc/acpi/tables", 0x10, 8)
            .add_checksum("etc/acpi/rsdp", 0x09, 0x00, 0x14);
        assert_eq!(l.len(), 4);
        let b = l.as_bytes();
        assert_eq!(b.len(), 4 * ENTRY_SIZE);
        // Each entry's command opcode is at its own 128-byte boundary, in order.
        assert_eq!(le32(b, 0), COMMAND_ALLOCATE);
        assert_eq!(le32(b, ENTRY_SIZE), COMMAND_ALLOCATE);
        assert_eq!(le32(b, 2 * ENTRY_SIZE), COMMAND_ADD_POINTER);
        assert_eq!(le32(b, 3 * ENTRY_SIZE), COMMAND_ADD_CHECKSUM);
    }

    #[test]
    fn long_name_truncated_and_nul_terminated() {
        // 60 chars — longer than the 56-byte field.
        let long = "a".repeat(60);
        let mut l = BiosLinkerLoader::new();
        l.allocate(&long, 1, AllocZone::High);
        let b = l.as_bytes();
        // Truncated to 55 bytes, and byte 55 (the last of the field) is the NUL.
        assert_eq!(name(b, 0x04).len(), FILE_NAME_SIZE - 1);
        assert_eq!(b[0x04 + FILE_NAME_SIZE - 1], 0);
    }

    #[test]
    fn into_bytes_matches_as_bytes() {
        let mut l = BiosLinkerLoader::new();
        l.allocate("etc/acpi/tables", 0x40, AllocZone::High);
        let borrowed = l.as_bytes().to_vec();
        assert_eq!(l.into_bytes(), borrowed);
    }
}
