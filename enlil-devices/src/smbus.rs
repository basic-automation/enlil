//! ICH9 `SMBus` host controller (`D31:F3`) — the "i801" register model.
//!
//! Every ICH-generation southbridge carries an `SMBus` 2.0 host controller at
//! `00:1F.3`; its register interface (Linux's `i2c-i801` driver) has been
//! stable since the original 82801. A machine whose `D31` has *no* function 3
//! is a southbridge SKU that never shipped — so the controller must exist for
//! the chipset identity to hold up, even though nothing interesting hangs off
//! the bus.
//!
//! The model is a faithful **empty `SMBus`**: the full host register file
//! (status, control, command, address, data, the 32-byte block buffer with its
//! shared internal pointer) behaves per the ICH9 datasheet (§19, "`SMBus`
//! Controller Registers"), and every started transaction completes immediately
//! with `DEV_ERR` — exactly what a real controller reports when no slave acks
//! the address. A guest running `i2cdetect`/`sensors-detect` sees a working
//! controller on a bus with nothing on it, not open-bus `0xFF` status reads.
//!
//! Interrupts: the register model stores `INTREN`, but nothing delivers the
//! completion interrupt yet — the PCI `INTx` plumbing (`assert_pci_intx` at
//! the platform layer) is driven by the run loop, and `i2c-i801` falls back to
//! polling on timeout. Wiring the completion through the live PIRQ routing is
//! a follow-up.

use crate::bus::PioDevice;
use crate::truncate::u8_of;

/// I/O base the platform firmware assigns to the `SMBus` host register block
/// (`SMB_BASE`, the controller's BAR4). 32 bytes, inside the PCI I/O window
/// the DSDT's `PCI0._CRS` produces (`0x0D00..0xFFFF`).
pub const SMBUS_IO_BASE: u16 = 0xB100;

/// Size of the `SMBus` host I/O register block (BAR4 decodes 32 bytes).
pub const SMBUS_IO_SIZE: u16 = 0x20;

/// Host status register (`HST_STS`, offset `0x00`).
pub const HST_STS: u16 = 0x00;
/// Host control register (`HST_CNT`, offset `0x02`).
pub const HST_CNT: u16 = 0x02;
/// Host command register (`HST_CMD`, offset `0x03`).
pub const HST_CMD: u16 = 0x03;
/// Transmit slave address register (`XMIT_SLVA`, offset `0x04`).
pub const XMIT_SLVA: u16 = 0x04;
/// Host data 0 register (`HST_D0`, offset `0x05`).
pub const HST_D0: u16 = 0x05;
/// Host data 1 register (`HST_D1`, offset `0x06`).
pub const HST_D1: u16 = 0x06;
/// Host block data byte register (`HOST_BLOCK_DB`, offset `0x07`).
pub const HOST_BLOCK_DB: u16 = 0x07;
/// Packet error check register (`PEC`, offset `0x08`).
pub const PEC: u16 = 0x08;
/// Auxiliary status register (`AUX_STS`, offset `0x0C`).
pub const AUX_STS: u16 = 0x0C;
/// Auxiliary control register (`AUX_CTL`, offset `0x0D`).
pub const AUX_CTL: u16 = 0x0D;

/// `HST_STS` bit 1: interrupt/completion status (write 1 to clear).
pub const STS_INTR: u8 = 1 << 1;
/// `HST_STS` bit 2: device error — the slave did not ack (write 1 to clear).
pub const STS_DEV_ERR: u8 = 1 << 2;
/// `HST_STS` bit 3: bus error — collision (write 1 to clear).
pub const STS_BUS_ERR: u8 = 1 << 3;
/// `HST_STS` bit 4: failed — transaction killed by `HST_CNT.KILL` (write 1 to
/// clear).
pub const STS_FAILED: u8 = 1 << 4;
/// `HST_STS` bit 6: the in-use semaphore.
///
/// Reading `HST_STS` returns the current value and then *sets* this bit;
/// writing 1 clears it. Software uses it to arbitrate the controller between
/// ACPI AML and the OS driver.
pub const STS_INUSE: u8 = 1 << 6;
/// `HST_STS` bit 7: byte-done (block-transfer byte interrupt; write 1 to clear).
pub const STS_BYTE_DONE: u8 = 1 << 7;
/// The write-1-to-clear `HST_STS` bits.
const STS_W1C: u8 = STS_INTR | STS_DEV_ERR | STS_BUS_ERR | STS_FAILED | (1 << 5) | STS_BYTE_DONE;

/// `HST_CNT` bit 0: interrupt enable.
pub const CNT_INTREN: u8 = 1 << 0;
/// `HST_CNT` bit 1: kill the current transaction (sets `FAILED`).
pub const CNT_KILL: u8 = 1 << 1;
/// `HST_CNT` bit 6: start the transaction described by bits 4:2.
pub const CNT_START: u8 = 1 << 6;

/// The `HST_STS` bits that assert the host interrupt while `INTREN` is set
/// (ICH9 datasheet §19.1: completion, the three error sources, and the
/// block-transfer byte-done status).
const STS_INTERRUPT_SOURCES: u8 = STS_INTR | STS_DEV_ERR | STS_BUS_ERR | STS_FAILED | STS_BYTE_DONE;

/// Size of the host block-data buffer behind [`HOST_BLOCK_DB`].
const BLOCK_BUF_LEN: usize = 32;

/// The ICH9 `SMBus` host controller register file, as a bus [`PioDevice`]
/// claiming the 32-byte window at [`SMBUS_IO_BASE`].
///
/// All registers are byte-wide; an access decodes the addressed byte
/// regardless of the access size, like the other fixed-function chipset ports.
pub struct SmbusHost {
    /// `HST_STS` (less the in-use semaphore, kept separately).
    status: u8,
    /// The in-use semaphore bit (`HST_STS[6]`): set by reads, cleared by
    /// writing 1.
    in_use: bool,
    /// `HST_CNT` sticky bits (`INTREN`, `SMB_CMD`, `LAST_BYTE`, `PEC_EN`) —
    /// `START`/`KILL` are command edges and always read back 0.
    control: u8,
    /// `HST_CMD`.
    command: u8,
    /// `XMIT_SLVA` (slave address 7:1, read/write bit 0).
    slave_address: u8,
    /// `HST_D0` / `HST_D1`.
    data: [u8; 2],
    /// `PEC`.
    pec: u8,
    /// `AUX_CTL` (`AAC`/`E32B`).
    aux_control: u8,
    /// The 32-byte block buffer behind `HOST_BLOCK_DB`.
    block_buf: [u8; BLOCK_BUF_LEN],
    /// The shared internal byte pointer into [`Self::block_buf`]; reading
    /// `HST_CNT` resets it.
    block_index: usize,
    /// The level sink the platform wires to this function's `INTB#`: called
    /// with the interrupt level (`INTREN` and an unserviced status bit) after
    /// every register write, like a real level-triggered `INTx` line.
    interrupt_line: Option<Box<dyn Fn(bool)>>,
}

impl Default for SmbusHost {
    fn default() -> Self {
        Self::new()
    }
}

impl SmbusHost {
    /// A controller in its reset state: idle, no status bits set, block
    /// pointer at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            status: 0,
            in_use: false,
            control: 0,
            command: 0,
            slave_address: 0,
            data: [0; 2],
            pec: 0,
            aux_control: 0,
            block_buf: [0; BLOCK_BUF_LEN],
            block_index: 0,
            interrupt_line: None,
        }
    }

    /// Wire the function's `INTB#` line: `line` is called with the computed
    /// interrupt level after every register write (assert on completion or
    /// error while `INTREN` is set, deassert once the driver clears the
    /// status), so the platform can route it through the live PIRQ routing.
    pub fn set_interrupt_line(&mut self, line: impl Fn(bool) + 'static) {
        self.interrupt_line = Some(Box::new(line));
    }

    /// The current `INTx` level: a pending interrupt source with `INTREN` set.
    const fn interrupt_level(&self) -> bool {
        self.control & CNT_INTREN != 0 && self.status & STS_INTERRUPT_SOURCES != 0
    }

    /// Push the current interrupt level into the wired sink (if any).
    fn sync_interrupt_line(&self) {
        if let Some(line) = &self.interrupt_line {
            line(self.interrupt_level());
        }
    }

    /// Execute a started transaction. The bus has no slaves, so every address
    /// goes un-acked and the transaction completes immediately with `DEV_ERR`
    /// — the controller is never left busy, which is what a polling driver
    /// (`i2c-i801`'s default path) spins on.
    const fn start_transaction(&mut self) {
        self.status |= STS_DEV_ERR;
    }

    const fn read_register(&mut self, offset: u16) -> u8 {
        match offset {
            HST_STS => {
                // Reading the status register returns the current value and
                // then sets the in-use semaphore (ICH9 datasheet §19.1.1).
                let value = self.status | if self.in_use { STS_INUSE } else { 0 };
                self.in_use = true;
                value
            }
            HST_CNT => {
                // A read of HST_CNT resets the block-buffer byte pointer; the
                // START/KILL command bits always read back 0.
                self.block_index = 0;
                self.control
            }
            HST_CMD => self.command,
            XMIT_SLVA => self.slave_address,
            HST_D0 => self.data[0],
            HST_D1 => self.data[1],
            HOST_BLOCK_DB => {
                let value = self.block_buf[self.block_index % BLOCK_BUF_LEN];
                self.block_index = (self.block_index + 1) % BLOCK_BUF_LEN;
                value
            }
            PEC => self.pec,
            AUX_CTL => self.aux_control,
            // AUX_STS (no CRC errors ever) and the unmodelled slave-interface
            // registers read as zero, matching their reset state.
            _ => 0,
        }
    }

    const fn write_register(&mut self, offset: u16, value: u8) {
        match offset {
            HST_STS => {
                // Write-1-to-clear status bits; bit 6 clears the semaphore.
                self.status &= !(value & STS_W1C);
                if value & STS_INUSE != 0 {
                    self.in_use = false;
                }
            }
            HST_CNT => {
                // Sticky control bits read back; START/KILL are edges.
                self.control = value & !(CNT_START | CNT_KILL);
                if value & CNT_KILL != 0 {
                    self.status |= STS_FAILED;
                } else if value & CNT_START != 0 {
                    self.start_transaction();
                }
            }
            HST_CMD => self.command = value,
            XMIT_SLVA => self.slave_address = value,
            HST_D0 => self.data[0] = value,
            HST_D1 => self.data[1] = value,
            HOST_BLOCK_DB => {
                self.block_buf[self.block_index % BLOCK_BUF_LEN] = value;
                self.block_index = (self.block_index + 1) % BLOCK_BUF_LEN;
            }
            PEC => self.pec = value,
            // Only AAC [0] and E32B [1] are writable; bits [7:2] are reserved
            // and read 0 (real hardware reads them 0, so a guest must not be
            // able to store them and read them back — a hypervisor tell).
            AUX_CTL => self.aux_control = value & 0x03,
            _ => {}
        }
    }
}

impl PioDevice for SmbusHost {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        u32::from(self.read_register(port.wrapping_sub(SMBUS_IO_BASE)))
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        self.write_register(port.wrapping_sub(SMBUS_IO_BASE), u8_of(data));
        // A write may have started/killed a transaction (raising a status bit)
        // or cleared one — recompute the level-triggered INTx state either way.
        self.sync_interrupt_line();
    }

    fn port_range(&self) -> (u16, u16) {
        (SMBUS_IO_BASE, SMBUS_IO_BASE + SMBUS_IO_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(smb: &mut SmbusHost, offset: u16) -> u8 {
        u8_of(smb.pio_read(SMBUS_IO_BASE + offset, 1))
    }

    fn write(smb: &mut SmbusHost, offset: u16, value: u8) {
        smb.pio_write(SMBUS_IO_BASE + offset, 1, u32::from(value));
    }

    #[test]
    fn claims_the_32_byte_window_at_the_firmware_base() {
        let smb = SmbusHost::new();
        assert_eq!(smb.port_range(), (0xB100, 0xB120));
    }

    #[test]
    fn reset_state_is_idle_and_the_inuse_semaphore_arms_on_read() {
        let mut smb = SmbusHost::new();
        // First read: idle, semaphore not yet taken...
        assert_eq!(read(&mut smb, HST_STS), 0);
        // ...second read: the first read took the semaphore.
        assert_eq!(read(&mut smb, HST_STS), STS_INUSE);
        // Writing 1 to bit 6 releases it.
        write(&mut smb, HST_STS, STS_INUSE);
        assert_eq!(read(&mut smb, HST_STS), 0);
    }

    #[test]
    fn a_transaction_on_the_empty_bus_fails_with_dev_err() {
        let mut smb = SmbusHost::new();
        // A byte-data read of slave 0x50 (i2cdetect's probe of a DIMM SPD).
        write(&mut smb, XMIT_SLVA, (0x50 << 1) | 1);
        write(&mut smb, HST_CMD, 0x00);
        write(&mut smb, HST_CNT, CNT_START | (0b010 << 2));
        let status = read(&mut smb, HST_STS);
        // No slave acked: DEV_ERR, and the controller is not left busy.
        assert_eq!(status & STS_DEV_ERR, STS_DEV_ERR);
        assert_eq!(status & 1, 0, "HOST_BUSY must be clear after completion");
        // The driver clears the error and the controller is idle again.
        write(&mut smb, HST_STS, STS_DEV_ERR);
        assert_eq!(read(&mut smb, HST_STS) & STS_DEV_ERR, 0);
    }

    #[test]
    fn kill_sets_failed_and_start_is_an_edge() {
        let mut smb = SmbusHost::new();
        write(&mut smb, HST_CNT, CNT_KILL);
        assert_eq!(read(&mut smb, HST_STS) & STS_FAILED, STS_FAILED);
        // The sticky control bits read back; the START/KILL edges read 0.
        write(&mut smb, HST_CNT, CNT_START | CNT_INTREN | (0b011 << 2));
        assert_eq!(read(&mut smb, HST_CNT), CNT_INTREN | (0b011 << 2));
    }

    #[test]
    fn block_buffer_pointer_is_shared_and_resets_on_hst_cnt_read() {
        let mut smb = SmbusHost::new();
        for &b in &[0xAA, 0xBB, 0xCC] {
            write(&mut smb, HOST_BLOCK_DB, b);
        }
        // Reading HST_CNT rewinds the shared pointer; the bytes read back.
        let _ = read(&mut smb, HST_CNT);
        assert_eq!(read(&mut smb, HOST_BLOCK_DB), 0xAA);
        assert_eq!(read(&mut smb, HOST_BLOCK_DB), 0xBB);
        assert_eq!(read(&mut smb, HOST_BLOCK_DB), 0xCC);
    }

    #[test]
    fn the_intx_line_follows_intren_and_the_status_bits() {
        use std::cell::Cell;
        use std::rc::Rc;

        let level = Rc::new(Cell::new(false));
        let mut smb = SmbusHost::new();
        {
            let level = Rc::clone(&level);
            smb.set_interrupt_line(move |l| level.set(l));
        }

        // A failed transaction with INTREN clear raises no interrupt.
        write(&mut smb, HST_CNT, CNT_START);
        assert!(!level.get());
        write(&mut smb, HST_STS, STS_DEV_ERR);

        // With INTREN set the DEV_ERR completion asserts the line (level)...
        write(&mut smb, HST_CNT, CNT_START | CNT_INTREN);
        assert!(level.get());
        // ...and stays asserted until the driver clears the status.
        write(&mut smb, HST_CMD, 0x00);
        assert!(level.get());
        write(&mut smb, HST_STS, STS_DEV_ERR);
        assert!(!level.get());
    }

    #[test]
    fn data_registers_hold_their_values() {
        let mut smb = SmbusHost::new();
        write(&mut smb, HST_CMD, 0x12);
        write(&mut smb, HST_D0, 0x34);
        write(&mut smb, HST_D1, 0x56);
        write(&mut smb, PEC, 0x78);
        write(&mut smb, AUX_CTL, 0x02); // E32B
        assert_eq!(read(&mut smb, HST_CMD), 0x12);
        assert_eq!(read(&mut smb, HST_D0), 0x34);
        assert_eq!(read(&mut smb, HST_D1), 0x56);
        assert_eq!(read(&mut smb, PEC), 0x78);
        assert_eq!(read(&mut smb, AUX_CTL), 0x02);
    }

    #[test]
    fn aux_ctl_reserved_bits_read_back_zero() {
        let mut smb = SmbusHost::new();
        // A guest writes all-ones; only AAC [0] and E32B [1] are writable.
        write(&mut smb, AUX_CTL, 0xFF);
        assert_eq!(read(&mut smb, AUX_CTL), 0x03, "AUX_CTL reserved bits read 0");
    }
}
