//! A parallel NOR flash chip (the Intel CFI command set), as OVMF drives it.
//!
//! A firmware's variable store lives in flash, and flash is not RAM: you write a
//! *command* to it, not data. OVMF probes at boot to see what it has. It writes
//! "clear status" and checks the array did not change (RAM would), selects "read
//! status" and checks the byte read back is the status register (zero after a
//! clear), then programs a byte with the value already there and checks the
//! status shows no error. Only then does it use the chip as persistent storage;
//! otherwise it falls back to a volatile in-RAM store and ignores the flash
//! image entirely.
//!
//! This models the commands that traffic uses: read array (`0xFF`), clear status
//! (`0x50`), read status (`0x70`), read identifier (`0x90`), program (`0x10` /
//! `0x40` then the data) and block erase (`0x20` then `0xD0`). The chip's state
//! is the only thing here: the caller owns the memory it fronts, passes it to each
//! operation, and (since a read must return the status register while the chip is
//! not in array mode) maps the memory directly only while [`Pflash::array_mode`]
//! holds.
//!
//! Pure and host-tested.

/// Erase block size (OVMF's firmware block size).
pub const BLOCK_BYTES: usize = 4096;

const CMD_READ_ARRAY: u8 = 0xFF;
const CMD_CLEAR_STATUS: u8 = 0x50;
const CMD_READ_STATUS: u8 = 0x70;
const CMD_READ_ID: u8 = 0x90;
const CMD_PROGRAM: u8 = 0x10;
const CMD_PROGRAM_ALT: u8 = 0x40;
const CMD_ERASE_SETUP: u8 = 0x20;
const CMD_ERASE_CONFIRM: u8 = 0xD0;

/// Status register: device ready.
const STATUS_READY: u8 = 0x80;
/// Status register: erase error (confirm did not follow setup).
const STATUS_ERASE_ERROR: u8 = 0x20;
/// Status register: programming error.
const STATUS_PROGRAM_ERROR: u8 = 0x10;

/// What the chip does with the next access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Reads return the memory.
    Array,
    /// Reads return the status register.
    Status,
    /// Reads return the manufacturer and device identifiers.
    Id,
    /// The next write is the data of a program command.
    ProgramData,
    /// The next write must be the erase confirmation.
    EraseConfirm,
}

/// The chip's command state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pflash {
    mode: Mode,
    status: u8,
    /// Whether programming and erasing are refused (a write-protected chip).
    read_only: bool,
}

impl Default for Pflash {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Pflash {
    /// A chip in read-array mode with a ready status; `read_only` makes every
    /// program and erase fail with an error status, as a write-protected part does.
    #[must_use]
    pub const fn new(read_only: bool) -> Self {
        Self {
            mode: Mode::Array,
            status: STATUS_READY,
            read_only,
        }
    }

    /// Whether reads currently return the memory itself (so the memory may be
    /// mapped for direct reads). Writes are always commands and always trap.
    #[must_use]
    pub const fn array_mode(&self) -> bool {
        matches!(self.mode, Mode::Array)
    }

    /// A `size`-byte read at `offset` of a chip fronting `mem`.
    #[must_use]
    pub fn read(&self, offset: usize, size: u8, mem: &[u8]) -> u64 {
        let size = usize::from(size.min(8));
        let mut v = 0u64;
        for i in 0..size {
            let byte = match self.mode {
                Mode::Array => mem.get(offset + i).copied().unwrap_or(0xFF),
                // Status and identifier reads answer on every byte lane.
                Mode::Status | Mode::ProgramData | Mode::EraseConfirm => self.status,
                Mode::Id => {
                    // Offset 0: manufacturer (Intel), offset 1: device code.
                    if (offset + i) & 1 == 0 { 0x89 } else { 0x18 }
                }
            };
            v |= u64::from(byte) << (8 * i);
        }
        v
    }

    /// A `size`-byte write of `value` at `offset` of a chip fronting `mem`.
    /// Returns whether [`array_mode`](Self::array_mode) changed, so the caller
    /// can remap the memory.
    pub fn write(&mut self, offset: usize, size: u8, value: u64, mem: &mut [u8]) -> bool {
        let was_array = self.array_mode();
        match self.mode {
            Mode::ProgramData => {
                if self.read_only {
                    self.status |= STATUS_READY | STATUS_PROGRAM_ERROR;
                } else {
                    for i in 0..usize::from(size.min(8)) {
                        if let Some(b) = mem.get_mut(offset + i) {
                            *b = value.to_le_bytes()[i];
                        }
                    }
                    self.status |= STATUS_READY;
                }
                self.mode = Mode::Status;
            }
            Mode::EraseConfirm => {
                if value & 0xFF == u64::from(CMD_ERASE_CONFIRM) && !self.read_only {
                    let start = offset / BLOCK_BYTES * BLOCK_BYTES;
                    if let Some(block) = mem.get_mut(start..start + BLOCK_BYTES) {
                        block.fill(0xFF);
                    }
                    self.status |= STATUS_READY;
                } else {
                    self.status |= STATUS_READY | STATUS_ERASE_ERROR;
                }
                self.mode = Mode::Status;
            }
            _ => self.command(u8::try_from(value & 0xFF).unwrap_or(0)),
        }
        was_array != self.array_mode()
    }

    /// A command byte written while no command is half-way through.
    const fn command(&mut self, cmd: u8) {
        match cmd {
            CMD_READ_ARRAY => self.mode = Mode::Array,
            // Clearing the status does not change what reads return.
            CMD_CLEAR_STATUS => self.status = 0,
            CMD_READ_STATUS => self.mode = Mode::Status,
            CMD_READ_ID => self.mode = Mode::Id,
            CMD_PROGRAM | CMD_PROGRAM_ALT => self.mode = Mode::ProgramData,
            CMD_ERASE_SETUP => self.mode = Mode::EraseConfirm,
            // Anything else is ignored, as a real part ignores an unknown command.
            _ => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // byte-wide reads of test fixtures
mod tests {
    use super::*;

    fn mem() -> Vec<u8> {
        (0..3 * BLOCK_BYTES).map(|i| (i % 251) as u8 | 1).collect()
    }

    /// OVMF's `QemuFlashDetected`, step for step, against a chip fronting `m`.
    fn ovmf_probe(f: &mut Pflash, m: &mut [u8], at: usize) -> &'static str {
        let original = f.read(at, 1, m) as u8;
        f.write(at, 1, u64::from(CMD_CLEAR_STATUS), m);
        let probe = f.read(at, 1, m) as u8;
        if original != CMD_CLEAR_STATUS && probe == CMD_CLEAR_STATUS {
            return "RAM";
        }
        f.write(at, 1, u64::from(CMD_READ_STATUS), m);
        let probe = f.read(at, 1, m) as u8;
        if probe == original {
            return "ROM";
        }
        if probe == CMD_READ_STATUS {
            return "RAM";
        }
        if probe != 0 {
            return "unrecognised";
        }
        f.write(at, 1, u64::from(CMD_PROGRAM), m);
        f.write(at, 1, u64::from(original), m);
        f.write(at, 1, u64::from(CMD_READ_STATUS), m);
        let status = f.read(at, 1, m) as u8;
        f.write(at, 1, u64::from(CMD_READ_ARRAY), m);
        if status & STATUS_PROGRAM_ERROR != 0 {
            "write-protected flash"
        } else {
            "writable flash"
        }
    }

    #[test]
    fn ovmf_recognises_it_as_writable_flash() {
        let mut m = mem();
        let before = m.clone();
        let mut f = Pflash::new(false);
        assert_eq!(ovmf_probe(&mut f, &mut m, 0x10), "writable flash");
        assert!(
            f.array_mode(),
            "the probe leaves the chip reading its array"
        );
        assert_eq!(m, before, "and changes nothing");
    }

    #[test]
    fn a_write_protected_chip_is_recognised_as_such() {
        let mut m = mem();
        let mut f = Pflash::new(true);
        assert_eq!(ovmf_probe(&mut f, &mut m, 0x10), "write-protected flash");
    }

    #[test]
    fn plain_memory_would_fail_the_same_probe_as_ram() {
        // A sanity check of the probe itself: RAM echoes the command bytes back.
        let mut m = mem();
        let at = 0x10;
        let original = m[at];
        m[at] = CMD_CLEAR_STATUS;
        assert_eq!(m[at], CMD_CLEAR_STATUS);
        assert_ne!(original, CMD_CLEAR_STATUS);
    }

    #[test]
    fn reads_return_the_array_until_a_command_changes_the_mode() {
        let mut m = mem();
        let mut f = Pflash::new(false);
        assert_eq!(f.read(7, 1, &m), u64::from(m[7]));
        assert_eq!(
            f.read(8, 4, &m),
            u64::from(u32::from_le_bytes([m[8], m[9], m[10], m[11]]))
        );
        f.write(0, 1, u64::from(CMD_READ_STATUS), &mut m);
        assert!(!f.array_mode());
        assert_eq!(f.read(7, 1, &m), 0x80, "ready");
        assert_eq!(f.read(7, 4, &m), 0x8080_8080, "status on every lane");
        f.write(0, 1, u64::from(CMD_READ_ARRAY), &mut m);
        assert!(f.array_mode());
        assert_eq!(f.read(7, 1, &m), u64::from(m[7]));
    }

    #[test]
    fn write_reports_exactly_when_the_array_mode_changes() {
        let mut m = mem();
        let mut f = Pflash::new(false);
        assert!(
            !f.write(0, 1, u64::from(CMD_CLEAR_STATUS), &mut m),
            "no change"
        );
        assert!(
            f.write(0, 1, u64::from(CMD_PROGRAM), &mut m),
            "left array mode"
        );
        assert!(
            !f.write(0, 1, 0x00, &mut m),
            "the data write stays in status mode"
        );
        assert!(
            f.write(0, 1, u64::from(CMD_READ_ARRAY), &mut m),
            "back to array"
        );
        assert!(
            !f.write(0, 1, u64::from(CMD_READ_ARRAY), &mut m),
            "already there"
        );
    }

    #[test]
    fn programming_writes_the_data_the_way_ovmf_does_byte_by_byte() {
        let mut m = mem();
        let mut f = Pflash::new(false);
        let at = BLOCK_BYTES + 100;
        // QemuFlashWrite: for each byte, command then data; then read-array.
        for (i, b) in [0xDE, 0xAD, 0xBE, 0xEF].into_iter().enumerate() {
            f.write(at + i, 1, u64::from(CMD_PROGRAM), &mut m);
            f.write(at + i, 1, b, &mut m);
        }
        f.write(at + 3, 1, u64::from(CMD_READ_ARRAY), &mut m);
        assert_eq!(&m[at..at + 4], [0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(f.read(at, 4, &m), 0xEFBE_ADDE);
        assert_eq!(m[at + 4], mem()[at + 4], "neighbours are untouched");
    }

    #[test]
    fn the_alternate_program_command_and_wide_data_work() {
        let mut m = mem();
        let mut f = Pflash::new(false);
        f.write(16, 4, u64::from(CMD_PROGRAM_ALT), &mut m);
        f.write(16, 4, 0x1122_3344, &mut m);
        f.write(16, 1, u64::from(CMD_READ_ARRAY), &mut m);
        assert_eq!(&m[16..20], [0x44, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn a_block_erase_sets_exactly_that_block_to_ones() {
        let mut m = mem();
        let before = m.clone();
        let mut f = Pflash::new(false);
        // QemuFlashEraseBlock: setup then confirm, at an address in block 1.
        f.write(BLOCK_BYTES + 7, 1, u64::from(CMD_ERASE_SETUP), &mut m);
        f.write(BLOCK_BYTES + 7, 1, u64::from(CMD_ERASE_CONFIRM), &mut m);
        assert!(m[BLOCK_BYTES..2 * BLOCK_BYTES].iter().all(|&b| b == 0xFF));
        assert_eq!(m[..BLOCK_BYTES], before[..BLOCK_BYTES]);
        assert_eq!(m[2 * BLOCK_BYTES..], before[2 * BLOCK_BYTES..]);
        // The chip reports ready, with no error, until told to read the array.
        assert_eq!(f.read(0, 1, &m), 0x80);
        f.write(0, 1, u64::from(CMD_READ_ARRAY), &mut m);
        assert!(f.array_mode());
    }

    #[test]
    fn an_erase_without_its_confirmation_fails_and_changes_nothing() {
        let mut m = mem();
        let before = m.clone();
        let mut f = Pflash::new(false);
        f.write(0, 1, u64::from(CMD_ERASE_SETUP), &mut m);
        f.write(0, 1, 0x55, &mut m);
        assert_eq!(m, before);
        assert_eq!(
            f.read(0, 1, &m) as u8 & STATUS_ERASE_ERROR,
            STATUS_ERASE_ERROR
        );
        // Clear status wipes the error.
        f.write(0, 1, u64::from(CMD_CLEAR_STATUS), &mut m);
        assert_eq!(f.read(0, 1, &m), 0);
    }

    #[test]
    fn a_read_only_chip_refuses_programs_and_erases() {
        let mut m = mem();
        let before = m.clone();
        let mut f = Pflash::new(true);
        f.write(5, 1, u64::from(CMD_PROGRAM), &mut m);
        f.write(5, 1, 0x00, &mut m);
        assert_eq!(
            f.read(5, 1, &m) as u8 & STATUS_PROGRAM_ERROR,
            STATUS_PROGRAM_ERROR
        );
        f.write(0, 1, u64::from(CMD_CLEAR_STATUS), &mut m);
        f.write(0, 1, u64::from(CMD_ERASE_SETUP), &mut m);
        f.write(0, 1, u64::from(CMD_ERASE_CONFIRM), &mut m);
        assert_eq!(m, before, "nothing changed");
        assert_ne!(f.read(0, 1, &m) as u8 & STATUS_ERASE_ERROR, 0);
    }

    #[test]
    fn the_identifier_mode_reports_a_manufacturer_and_device() {
        let mut m = mem();
        let mut f = Pflash::new(false);
        f.write(0, 1, u64::from(CMD_READ_ID), &mut m);
        assert_eq!(f.read(0, 1, &m), 0x89);
        assert_eq!(f.read(1, 1, &m), 0x18);
        assert!(!f.array_mode());
    }

    #[test]
    fn unknown_commands_and_out_of_range_accesses_are_harmless() {
        let mut m = mem();
        let before = m.clone();
        let mut f = Pflash::new(false);
        f.write(0, 1, 0x77, &mut m);
        assert!(f.array_mode());
        // Programming and erasing past the end of the memory touch nothing.
        let end = m.len();
        f.write(end + 5, 1, u64::from(CMD_PROGRAM), &mut m);
        f.write(end + 5, 1, 0, &mut m);
        f.write(end, 1, u64::from(CMD_ERASE_SETUP), &mut m);
        f.write(end, 1, u64::from(CMD_ERASE_CONFIRM), &mut m);
        assert_eq!(m, before);
        // A read past the end of the array reads as erased flash.
        f.write(0, 1, u64::from(CMD_READ_ARRAY), &mut m);
        assert_eq!(f.read(end, 1, &m), 0xFF);
    }
}
