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

/// Why executing a bios-linker-loader command stream failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoaderError {
    /// A command referenced a file that was never added to the executor.
    UnknownFile(String),
    /// A command's field/offset fell outside the referenced file.
    OutOfBounds {
        /// The file the access was against.
        file: String,
        /// The byte offset that was out of range.
        offset: usize,
    },
    /// An `ADD_POINTER`/`WRITE_POINTER` declared an unsupported pointer width.
    BadPointerSize(u8),
    /// The stream length was not a whole number of 128-byte commands.
    TruncatedStream(usize),
    /// An unknown command opcode.
    UnknownCommand(u32),
}

/// A minimal in-process executor of a bios-linker-loader command stream — the
/// *firmware* side of the `etc/table-loader` contract.
///
/// It exists to validate a generated loader (and the base-0 tables it
/// relocates) without a full OVMF/SeaBIOS boot: place each delivered file at a
/// caller-chosen base with [`add_file`](Self::add_file), [`execute`](Self::execute)
/// the stream, and inspect the relocated bytes with [`file`](Self::file). The
/// `ADD_POINTER` and `ADD_CHECKSUM` semantics mirror QEMU/edk2:
/// `*(dest+offset) += base(src)` over a little-endian pointer, and the checksum
/// byte is decremented by the sum of its covered range (so the range sums to 0).
#[derive(Debug, Clone, Default)]
pub struct LoaderExecutor {
    /// name -> (allocation base, file image).
    files: std::collections::HashMap<String, (u64, Vec<u8>)>,
}

impl LoaderExecutor {
    /// A new executor with no files.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Place `data` (a delivered `fw_cfg` file) at allocation address `base`.
    pub fn add_file(&mut self, name: &str, base: u64, data: Vec<u8>) {
        self.files.insert(name.to_string(), (base, data));
    }

    /// The (relocated) image of a previously added file.
    #[must_use]
    pub fn file(&self, name: &str) -> Option<&[u8]> {
        self.files.get(name).map(|(_, d)| d.as_slice())
    }

    /// The allocation base a file was placed at.
    #[must_use]
    pub fn base(&self, name: &str) -> Option<u64> {
        self.files.get(name).map(|(b, _)| *b)
    }

    fn base_of(&self, name: &str) -> Result<u64, LoaderError> {
        self.files
            .get(name)
            .map(|(b, _)| *b)
            .ok_or_else(|| LoaderError::UnknownFile(name.to_string()))
    }

    /// Execute every command in `stream`, mutating the added file images in
    /// place exactly as the firmware would.
    ///
    /// # Errors
    /// Returns a [`LoaderError`] if the stream is malformed, names an unknown
    /// file, addresses outside a file, or uses an unsupported command/size.
    pub fn execute(&mut self, stream: &[u8]) -> Result<(), LoaderError> {
        if !stream.len().is_multiple_of(ENTRY_SIZE) {
            return Err(LoaderError::TruncatedStream(stream.len()));
        }
        for e in stream.chunks_exact(ENTRY_SIZE) {
            match read_u32(e, 0x00) {
                COMMAND_ALLOCATE => {
                    // The caller models allocation by pre-placing files; just
                    // confirm the target exists.
                    self.base_of(&read_name(e, 0x04))?;
                }
                COMMAND_ADD_POINTER => self.add_pointer(e)?,
                COMMAND_ADD_CHECKSUM => self.add_checksum(e)?,
                COMMAND_WRITE_POINTER => self.write_pointer(e)?,
                other => return Err(LoaderError::UnknownCommand(other)),
            }
        }
        Ok(())
    }

    fn add_pointer(&mut self, e: &[u8]) -> Result<(), LoaderError> {
        let dest = read_name(e, 0x04);
        let src = read_name(e, 0x38);
        let offset = read_u32(e, 0x6C) as usize;
        let size = e[0x70];
        let src_base = self.base_of(&src)?;
        let (_, data) = self
            .files
            .get_mut(&dest)
            .ok_or_else(|| LoaderError::UnknownFile(dest.clone()))?;
        let cur = read_le(data, offset, size).ok_or_else(|| LoaderError::OutOfBounds {
            file: dest.clone(),
            offset,
        })?;
        write_le(data, offset, size, cur.wrapping_add(src_base))
    }

    fn write_pointer(&mut self, e: &[u8]) -> Result<(), LoaderError> {
        let dest = read_name(e, 0x04);
        let src = read_name(e, 0x38);
        let dst_offset = read_u32(e, 0x6C) as usize;
        let src_offset = u64::from(read_u32(e, 0x70));
        let size = e[0x74];
        let src_base = self.base_of(&src)?;
        let (_, data) = self
            .files
            .get_mut(&dest)
            .ok_or_else(|| LoaderError::UnknownFile(dest.clone()))?;
        write_le(data, dst_offset, size, src_base.wrapping_add(src_offset))
    }

    fn add_checksum(&mut self, e: &[u8]) -> Result<(), LoaderError> {
        let file = read_name(e, 0x04);
        let offset = read_u32(e, 0x3C) as usize;
        let start = read_u32(e, 0x40) as usize;
        let length = read_u32(e, 0x44) as usize;
        let (_, data) = self
            .files
            .get_mut(&file)
            .ok_or_else(|| LoaderError::UnknownFile(file.clone()))?;
        if offset >= data.len() || start.checked_add(length).is_none_or(|end| end > data.len()) {
            return Err(LoaderError::OutOfBounds { file, offset });
        }
        let sum = data[start..start + length]
            .iter()
            .fold(0u8, |a, &b| a.wrapping_add(b));
        data[offset] = data[offset].wrapping_sub(sum);
        Ok(())
    }
}

const fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn read_name(b: &[u8], off: usize) -> String {
    let field = &b[off..off + FILE_NAME_SIZE];
    let end = field.iter().position(|&c| c == 0).unwrap_or(FILE_NAME_SIZE);
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Read a `size`-byte (1/2/4/8) little-endian value at `offset`, or `None` if
/// it would read past the end of `data`.
fn read_le(data: &[u8], offset: usize, size: u8) -> Option<u64> {
    let n = size as usize;
    let slice = data.get(offset..offset.checked_add(n)?)?;
    let mut v = 0u64;
    for (i, &b) in slice.iter().enumerate() {
        v |= u64::from(b) << (8 * i);
    }
    Some(v)
}

fn write_le(data: &mut [u8], offset: usize, size: u8, value: u64) -> Result<(), LoaderError> {
    let n = match size {
        1 | 2 | 4 | 8 => size as usize,
        other => return Err(LoaderError::BadPointerSize(other)),
    };
    let end = offset.checked_add(n).filter(|&e| e <= data.len());
    let Some(end) = end else {
        return Err(LoaderError::OutOfBounds {
            file: String::new(),
            offset,
        });
    };
    let bytes = value.to_le_bytes();
    data[offset..end].copy_from_slice(&bytes[..n]);
    Ok(())
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

    #[test]
    fn executor_relocates_a_pointer_by_the_source_base() {
        // A "tables" file with an 8-byte field at offset 0 holding the in-blob
        // offset 0x10; ALLOCATE it at base 0x4000_0000, then ADD_POINTER should
        // make the field read 0x4000_0010.
        let mut l = BiosLinkerLoader::new();
        l.allocate("t", 1, AllocZone::High)
            .add_pointer("t", "t", 0, 8);

        let mut data = vec![0u8; 16];
        data[0..8].copy_from_slice(&0x10u64.to_le_bytes());
        let mut exec = LoaderExecutor::new();
        exec.add_file("t", 0x4000_0000, data);
        exec.execute(l.as_bytes()).unwrap();

        let got = u64::from_le_bytes(exec.file("t").unwrap()[0..8].try_into().unwrap());
        assert_eq!(got, 0x4000_0010);
    }

    #[test]
    fn executor_checksum_makes_the_range_sum_to_zero() {
        // ADD_CHECKSUM over a 4-byte range with the checksum byte inside it must
        // leave the range summing to 0 (mod 256), like an ACPI table checksum.
        let mut l = BiosLinkerLoader::new();
        l.allocate("t", 1, AllocZone::High)
            .add_checksum("t", 0, 0, 4); // checksum byte at 0, over [0,4)

        let mut exec = LoaderExecutor::new();
        exec.add_file("t", 0, vec![0x00, 0x11, 0x22, 0x33]);
        exec.execute(l.as_bytes()).unwrap();

        let f = exec.file("t").unwrap();
        let sum = f[0..4].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn executor_rejects_truncated_and_unknown_files() {
        let mut exec = LoaderExecutor::new();
        assert_eq!(
            exec.execute(&[0u8; 7]),
            Err(LoaderError::TruncatedStream(7))
        );

        let mut l = BiosLinkerLoader::new();
        l.add_pointer("missing", "alsogone", 0, 8);
        assert!(matches!(
            exec.execute(l.as_bytes()),
            Err(LoaderError::UnknownFile(_))
        ));
    }
}
