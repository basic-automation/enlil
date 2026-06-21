//! Serial console abstraction for the Enlil hypervisor (Phase 1.5).
//!
//! Provides per-guest emulated COM1 serial ports with multiple output modes,
//! UART register emulation, and a multiplexer for routing guest serial I/O.
//!
//! Cross-platform: no OS-specific dependencies.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use enlil_devices::bus::PioDevice;

/// Number of contiguous I/O ports a 16550 UART occupies (`base..base+8`).
pub const SERIAL_PORT_COUNT: u16 = 8;

// ---------------------------------------------------------------------------
// COM port base addresses
// ---------------------------------------------------------------------------

/// Standard x86 COM port base addresses.
pub const COM1: u16 = 0x3F8;
pub const COM2: u16 = 0x2F8;
pub const COM3: u16 = 0x3E8;
pub const COM4: u16 = 0x2E8;

// ---------------------------------------------------------------------------
// UART register offsets
// ---------------------------------------------------------------------------

/// Serial port register offsets (8250/16550 compatible).
pub const DATA_REG: u16 = 0; // RX buffer (read) / TX holding (write)
pub const IER_REG: u16 = 1; // Interrupt Enable Register
pub const IIR_REG: u16 = 2; // Interrupt Identification (read) / FIFO Control (write)
pub const LCR_REG: u16 = 3; // Line Control Register
pub const MCR_REG: u16 = 4; // Modem Control Register
pub const LSR_REG: u16 = 5; // Line Status Register
pub const MSR_REG: u16 = 6; // Modem Status Register
pub const SCR_REG: u16 = 7; // Scratch Register

// ---------------------------------------------------------------------------
// LSR bit flags
// ---------------------------------------------------------------------------

/// Data Ready — set when there is a byte available to read.
pub const LSR_DATA_READY: u8 = 0x01;
/// Overrun Error — set when a received byte was lost because the RX FIFO was
/// full. Sticky until the guest reads the LSR (matching the 16550).
pub const LSR_OVERRUN_ERROR: u8 = 0x02;
/// Transmitter Holding Register Empty — TX is ready to accept a byte.
pub const LSR_THR_EMPTY: u8 = 0x20;
/// Transmitter Empty — both THR and shift register are empty.
pub const LSR_TEMT: u8 = 0x40;

/// Bound on the host→guest RX FIFO. The 16550's hardware FIFO is 16 bytes; we
/// allow a larger host-side queue so pasted/burst console input is not lost
/// under normal operation, but cap it so a guest that never drains the port
/// cannot make the host allocate without bound (rust-vmm `vm-superio` issue
/// #17). When the cap is reached the incoming byte is dropped and the LSR
/// Overrun Error bit is raised, exactly as real hardware reports an overrun.
pub const RX_FIFO_CAPACITY: usize = 4096;

// ---------------------------------------------------------------------------
// IER bit flags (Interrupt Enable Register)
// ---------------------------------------------------------------------------

/// Enable Received Data Available interrupt (ERBFI).
pub const IER_RX_AVAILABLE: u8 = 0x01;
/// Enable Transmitter Holding Register Empty interrupt (ETBEI).
pub const IER_THR_EMPTY: u8 = 0x02;
/// Enable Receiver Line Status interrupt (ELSI).
pub const IER_RX_LINE_STATUS: u8 = 0x04;
/// Enable Modem Status interrupt (EDSSI).
pub const IER_MODEM_STATUS: u8 = 0x08;

// ---------------------------------------------------------------------------
// MCR bit flags (Modem Control Register)
// ---------------------------------------------------------------------------

/// Data Terminal Ready output.
pub const MCR_DTR: u8 = 0x01;
/// Request To Send output.
pub const MCR_RTS: u8 = 0x02;
/// Auxiliary output 1.
pub const MCR_OUT1: u8 = 0x04;
/// Auxiliary output 2.
pub const MCR_OUT2: u8 = 0x08;
/// Diagnostic loopback enable: TX is internally wired to RX and the four MCR
/// control outputs feed the four MSR status inputs (16550 §"Loop" mode). Guest
/// serial drivers and BIOS POST use it to probe the UART.
pub const MCR_LOOP: u8 = 0x10;

// ---------------------------------------------------------------------------
// MSR bit flags (Modem Status Register)
// ---------------------------------------------------------------------------

/// Delta Clear To Send (CTS changed since last MSR read).
pub const MSR_DCTS: u8 = 0x01;
/// Delta Data Set Ready (DSR changed since last MSR read).
pub const MSR_DDSR: u8 = 0x02;
/// Trailing Edge Ring Indicator (RI 1→0 since last MSR read).
pub const MSR_TERI: u8 = 0x04;
/// Delta Data Carrier Detect (DCD changed since last MSR read).
pub const MSR_DDCD: u8 = 0x08;
/// Clear To Send input.
pub const MSR_CTS: u8 = 0x10;
/// Data Set Ready input.
pub const MSR_DSR: u8 = 0x20;
/// Ring Indicator input.
pub const MSR_RI: u8 = 0x40;
/// Data Carrier Detect input.
pub const MSR_DCD: u8 = 0x80;

// ---------------------------------------------------------------------------
// IIR identification values (Interrupt Identification Register, bits 0-3)
// ---------------------------------------------------------------------------

/// No interrupt pending (bit 0 set).
pub const IIR_NO_INTERRUPT: u8 = 0x01;
/// Transmitter Holding Register Empty interrupt pending.
pub const IIR_THR_EMPTY: u8 = 0x02;
/// Received Data Available interrupt pending (higher priority than THRE).
pub const IIR_RX_AVAILABLE: u8 = 0x04;
/// IIR bits 7:6, set when the FIFOs are enabled — `0b11` identifies a working
/// 16550A (vs `0b00` for a FIFO-less 8250/16450), the value a guest's UART
/// autoconfig reads back after writing [`FCR_ENABLE`] to decide the part type.
pub const IIR_FIFO_ENABLED: u8 = 0xC0;

// ---------------------------------------------------------------------------
// FCR bit flags (FIFO Control Register — write side of the IIR port)
// ---------------------------------------------------------------------------

/// Enable the RX/TX FIFOs.
pub const FCR_ENABLE: u8 = 0x01;
/// Clear (reset) the receive FIFO.
pub const FCR_CLEAR_RX: u8 = 0x02;
/// Clear (reset) the transmit FIFO.
pub const FCR_CLEAR_TX: u8 = 0x04;

// ---------------------------------------------------------------------------
// SerialConfig
// ---------------------------------------------------------------------------

/// Configuration for a guest serial console.
#[derive(Debug, Clone)]
pub struct SerialConfig {
    /// Base I/O port (e.g., 0x3F8 for COM1).
    pub base_port: u16,
    /// IRQ line for the serial device.
    pub irq: u8,
    /// Output mode for this serial port.
    pub mode: SerialOutputMode,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            base_port: COM1,
            irq: 4, // COM1 traditionally uses IRQ 4
            mode: SerialOutputMode::Stdout,
        }
    }
}

// ---------------------------------------------------------------------------
// SerialOutputMode
// ---------------------------------------------------------------------------

/// Determines where serial output bytes are routed.
#[derive(Debug, Clone)]
pub enum SerialOutputMode {
    /// Print to host stdout with a guest-name prefix (default).
    Stdout,
    /// Append to a file at the given path.
    File(String),
    /// Discard all output silently.
    Null,
    /// Collect output in an in-memory buffer owned by this sink (testing).
    Buffer,
    /// Append every transmitted byte to a caller-owned shared buffer.
    ///
    /// Unlike [`Self::Buffer`], the handle is held by the caller, so the guest's
    /// serial output stays observable even after the owning device has been
    /// moved into a bus (the host console / logger reads from the same `Arc`).
    Shared(Arc<Mutex<Vec<u8>>>),
}

// ---------------------------------------------------------------------------
// SerialOutput — the per-guest output sink
// ---------------------------------------------------------------------------

/// A serial output sink that routes bytes according to its [`SerialOutputMode`].
///
/// Each guest gets its own `SerialOutput`. It line-buffers internally and
/// flushes on `\n` or `\r` (for Stdout/File modes). Buffer and Null modes
/// store or discard raw bytes respectively.
pub struct SerialOutput {
    prefix: String,
    mode: SerialOutputMode,
    /// Line buffer for Stdout / File modes.
    line_buf: Vec<u8>,
    /// Backing file handle for File mode (opened lazily on first write).
    file: Option<File>,
    /// In-memory buffer for Buffer mode.
    mem_buf: Vec<u8>,
}

impl SerialOutput {
    /// Create a new output sink for the named guest.
    #[must_use]
    pub fn new(guest_name: &str, mode: SerialOutputMode) -> Self {
        Self {
            prefix: format!("[{guest_name}] "),
            mode,
            line_buf: Vec::with_capacity(256),
            file: None,
            mem_buf: Vec::new(),
        }
    }

    /// Convenience: create a Stdout-mode output (backwards compatible).
    #[must_use]
    pub fn new_stdout(guest_name: &str) -> Self {
        Self::new(guest_name, SerialOutputMode::Stdout)
    }

    /// Handle a byte written to the serial data register.
    pub fn write_byte(&mut self, byte: u8) {
        match &self.mode {
            SerialOutputMode::Null => { /* discard */ }

            SerialOutputMode::Buffer => {
                self.mem_buf.push(byte);
            }

            SerialOutputMode::Shared(buf) => {
                if let Ok(mut v) = buf.lock() {
                    v.push(byte);
                }
            }

            SerialOutputMode::Stdout => {
                if byte == b'\n' || byte == b'\r' {
                    if !self.line_buf.is_empty() {
                        let line = String::from_utf8_lossy(&self.line_buf);
                        let _ = writeln!(io::stdout(), "{}{}", self.prefix, line);
                        self.line_buf.clear();
                    }
                } else {
                    self.line_buf.push(byte);
                }
            }

            SerialOutputMode::File(path) => {
                if byte == b'\n' || byte == b'\r' {
                    if !self.line_buf.is_empty() {
                        if let Some(ref mut f) = self.file {
                            let line = String::from_utf8_lossy(&self.line_buf);
                            let _ = writeln!(f, "{}{}", self.prefix, line);
                        } else {
                            // Lazy-open the file.
                            match OpenOptions::new().create(true).append(true).open(path) {
                                Ok(mut f) => {
                                    let line = String::from_utf8_lossy(&self.line_buf);
                                    let _ = writeln!(f, "{}{}", self.prefix, line);
                                    self.file = Some(f);
                                }
                                Err(e) => {
                                    log::error!("serial: failed to open {path}: {e}");
                                }
                            }
                        }
                        self.line_buf.clear();
                    }
                } else {
                    self.line_buf.push(byte);
                }
            }
        }
    }

    /// Flush any partial line remaining in the buffer.
    pub fn flush(&mut self) {
        if self.line_buf.is_empty() {
            return;
        }
        match &self.mode {
            SerialOutputMode::Null => {
                self.line_buf.clear();
            }
            SerialOutputMode::Buffer | SerialOutputMode::Shared(_) => {
                // line_buf isn't used for Buffer/Shared mode, but just in case:
                self.line_buf.clear();
            }
            SerialOutputMode::Stdout => {
                let line = String::from_utf8_lossy(&self.line_buf);
                let _ = writeln!(io::stdout(), "{}{}", self.prefix, line);
                self.line_buf.clear();
            }
            SerialOutputMode::File(_) => {
                if let Some(ref mut f) = self.file {
                    let line = String::from_utf8_lossy(&self.line_buf);
                    let _ = writeln!(f, "{}{}", self.prefix, line);
                }
                self.line_buf.clear();
            }
        }
    }

    /// Return all bytes collected in Buffer mode. Empty for other modes.
    #[must_use]
    pub fn buffer_contents(&self) -> &[u8] {
        &self.mem_buf
    }

    /// Drain and return buffer contents (Buffer mode). Empty for other modes.
    pub fn take_buffer(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.mem_buf)
    }
}

impl Drop for SerialOutput {
    fn drop(&mut self) {
        self.flush();
    }
}

// ---------------------------------------------------------------------------
// IrqLine — the host-side interrupt sink a UART drives
// ---------------------------------------------------------------------------

/// A host-side interrupt line the UART drives to assert/deassert its IRQ.
///
/// The 16550 raises IRQ4 (COM1) whenever an *enabled* interrupt source is
/// pending and lowers it when none remain (see [`UartState::interrupt_pending`]).
/// This trait lets the UART notify the host of that *level change* without
/// knowing how the interrupt is actually delivered: under KVM the
/// implementation forwards to `KVM_IRQ_LINE` (`vmm.set_irq_line(4, level)`)
/// through the in-kernel IRQ chip; in tests a counter observes the edges.
///
/// `set_level` is called only when the asserted level actually changes, so an
/// implementation may treat each call as one edge (matching QEMU's
/// `qemu_set_irq` model and rust-vmm `vm-superio`'s eventfd `Trigger`).
pub trait IrqLine: Send {
    /// Drive the interrupt line: `true` asserts the IRQ, `false` deasserts it.
    fn set_level(&self, level: bool);
}

/// Any `Fn(bool)` doubles as an [`IrqLine`], so the KVM backend can wire one
/// with a closure (e.g. `move |level| vm.set_irq_line(4, level)`).
impl<F: Fn(bool) + Send> IrqLine for F {
    fn set_level(&self, level: bool) {
        self(level);
    }
}

// ---------------------------------------------------------------------------
// UartState — per-guest UART register emulation
// ---------------------------------------------------------------------------

/// Emulated 8250/16550 UART register state for a single guest serial port.
///
/// Tracks the standard register file and an RX input queue so guests can
/// both write (TX) and read (RX) through the serial port.
pub struct UartState {
    /// Interrupt Enable Register.
    pub ier: u8,
    /// Line Control Register.
    pub lcr: u8,
    /// Modem Control Register.
    pub mcr: u8,
    /// Line Status Register (dynamically computed on read).
    lsr_overrides: u8,
    /// Modem Status Register (external modem lines; used when not in loopback).
    pub msr: u8,
    /// Accumulated MSR delta bits (low nibble) while in loopback — set when a
    /// looped-back modem line changes on an MCR write, cleared when the guest
    /// reads the MSR (matching the 16550's read-to-clear delta behaviour).
    msr_loop_delta: u8,
    /// Scratch Register.
    pub scr: u8,
    /// Whether the guest has enabled the FIFOs via the FCR. Reported in the IIR
    /// (bits 7:6) so a guest's autoconfig identifies the part as a 16550A.
    fifo_enabled: bool,
    /// Divisor latch (when DLAB=1, `DATA_REG` and `IER_REG` access this).
    pub divisor: u16,
    /// Receive buffer — bytes injected by the host for the guest to read.
    rx_fifo: VecDeque<u8>,
    /// THR-empty interrupt latch: set when the transmitter holding register
    /// becomes empty (after a TX, or when ETBEI is freshly enabled), cleared
    /// when the guest reads IIR and THRE was the reported source.
    thr_empty_pending: bool,
    /// Optional host interrupt sink (IRQ4). Driven on every level change.
    irq_line: Option<Box<dyn IrqLine>>,
    /// Last level we asserted on `irq_line`, so we only notify on a real edge.
    irq_level: bool,
    /// The output sink for transmitted bytes.
    output: SerialOutput,
}

impl UartState {
    /// Create a new UART with the given output sink.
    #[must_use]
    pub fn new(output: SerialOutput) -> Self {
        Self {
            ier: 0,
            lcr: 0x03, // 8N1 default
            mcr: 0,
            lsr_overrides: 0,
            msr: 0,
            msr_loop_delta: 0,
            scr: 0,
            fifo_enabled: false,
            divisor: 0x000C, // 9600 baud default (115200 / 9600 = 12)
            rx_fifo: VecDeque::with_capacity(64),
            thr_empty_pending: false,
            irq_line: None,
            irq_level: false,
            output,
        }
    }

    /// Attach a host interrupt sink (IRQ4) and synchronise its level to the
    /// UART's current interrupt state.
    ///
    /// The UART subsequently calls [`IrqLine::set_level`] whenever an enabled
    /// interrupt source asserts or clears (see [`Self::interrupt_pending`]).
    pub fn set_irq_line(&mut self, line: Box<dyn IrqLine>) {
        self.irq_line = Some(line);
        // Reset the cached level so the sync below always emits the current one.
        self.irq_level = false;
        self.update_irq();
    }

    // -- Register reads (guest IN instruction) --

    /// Read a UART register. `offset` is 0–7 relative to base port.
    pub fn read_register(&mut self, offset: u16) -> u8 {
        let dlab = self.lcr & 0x80 != 0;

        let value = match offset {
            DATA_REG if dlab => self.divisor as u8,
            DATA_REG => self.read_data(),
            IER_REG if dlab => (self.divisor >> 8) as u8,
            IER_REG => self.ier,
            IIR_REG => self.read_iir(),
            LCR_REG => self.lcr,
            MCR_REG => self.mcr,
            LSR_REG => {
                let lsr = self.compute_lsr();
                // Reading the LSR clears the sticky Overrun Error bit (16550).
                self.lsr_overrides &= !LSR_OVERRUN_ERROR;
                lsr
            }
            MSR_REG => self.read_msr(),
            SCR_REG => self.scr,
            _ => 0xFF, // unmapped
        };
        // Draining RBR (RX) or reading IIR (clears the THRE latch) can change
        // which interrupt is pending; resync the IRQ line.
        self.update_irq();
        value
    }

    // -- Register writes (guest OUT instruction) --

    /// Write a UART register. `offset` is 0–7 relative to base port.
    pub fn write_register(&mut self, offset: u16, value: u8) {
        let dlab = self.lcr & 0x80 != 0;

        match offset {
            DATA_REG if dlab => {
                self.divisor = (self.divisor & 0xFF00) | u16::from(value);
            }
            DATA_REG => self.write_data(value),
            IER_REG if dlab => {
                self.divisor = (self.divisor & 0x00FF) | (u16::from(value) << 8);
            }
            IER_REG => self.write_ier(value),
            IIR_REG => self.write_fcr(value), // offset 2 reads IIR, writes FCR
            LCR_REG => self.lcr = value,
            MCR_REG => self.write_mcr(value & 0x1F),
            SCR_REG => self.scr = value,
            _ => {}
        }
        // An IER change (enable/disable) or a TX (re-arming THRE) can change the
        // pending interrupt; resync the IRQ line.
        self.update_irq();
    }

    // -- TX path --

    fn write_data(&mut self, byte: u8) {
        if self.mcr & MCR_LOOP != 0 {
            // Diagnostic loopback: the transmitted byte is wired straight back
            // into the receiver instead of going out the sink. Honour the same
            // FIFO bound + overrun flag as a real RX (inject_input), then the
            // looped byte can raise the RX-available interrupt.
            if self.rx_fifo.len() >= RX_FIFO_CAPACITY {
                self.lsr_overrides |= LSR_OVERRUN_ERROR;
            } else {
                self.rx_fifo.push_back(byte);
            }
        } else {
            self.output.write_byte(byte);
        }
        // TX completes immediately in emulation, so the transmitter holding
        // register is empty again — re-arm the THRE interrupt.
        self.thr_empty_pending = true;
    }

    /// Write the Modem Control Register. Only the low five bits exist. While
    /// loopback ([`MCR_LOOP`]) is active the four control outputs feed the MSR
    /// status inputs, so a change to a mapped output latches the corresponding
    /// MSR delta bit (set until the guest reads the MSR), exactly as the looped
    /// modem line would on hardware.
    fn write_mcr(&mut self, value: u8) {
        if value & MCR_LOOP != 0 {
            let before = Self::loop_modem_high(self.mcr);
            let after = Self::loop_modem_high(value);
            let changed = before ^ after;
            // CTS/DSR/DCD: any change sets the delta. RI: trailing edge only
            // (1→0), per the 16550 TERI semantics.
            if changed & MSR_CTS != 0 {
                self.msr_loop_delta |= MSR_DCTS;
            }
            if changed & MSR_DSR != 0 {
                self.msr_loop_delta |= MSR_DDSR;
            }
            if changed & MSR_DCD != 0 {
                self.msr_loop_delta |= MSR_DDCD;
            }
            if before & MSR_RI != 0 && after & MSR_RI == 0 {
                self.msr_loop_delta |= MSR_TERI;
            }
        }
        self.mcr = value;
    }

    /// The MSR status-input high nibble produced by loopback from an MCR value:
    /// DTR→DSR, RTS→CTS, OUT1→RI, OUT2→DCD.
    const fn loop_modem_high(mcr: u8) -> u8 {
        let mut status = 0;
        if mcr & MCR_DTR != 0 {
            status |= MSR_DSR;
        }
        if mcr & MCR_RTS != 0 {
            status |= MSR_CTS;
        }
        if mcr & MCR_OUT1 != 0 {
            status |= MSR_RI;
        }
        if mcr & MCR_OUT2 != 0 {
            status |= MSR_DCD;
        }
        status
    }

    /// Read the Modem Status Register. In loopback the status inputs are driven
    /// from the MCR outputs (high nibble) plus the accumulated delta bits, which
    /// the read clears; otherwise the externally-set [`msr`](Self::msr) is
    /// returned unchanged.
    fn read_msr(&mut self) -> u8 {
        if self.mcr & MCR_LOOP != 0 {
            let value = Self::loop_modem_high(self.mcr) | self.msr_loop_delta;
            self.msr_loop_delta = 0; // delta bits clear on read
            value
        } else {
            self.msr
        }
    }

    /// Handle a write to the FIFO Control Register (offset 2, write side).
    ///
    /// Tracks the FIFO-enable bit (reported back in the IIR so a guest detects a
    /// 16550A) and honours the RX/TX FIFO-clear bits. Our RX queue stands in for
    /// the hardware RX FIFO; TX completes immediately so its clear is a no-op.
    fn write_fcr(&mut self, value: u8) {
        self.fifo_enabled = value & FCR_ENABLE != 0;
        if value & FCR_CLEAR_RX != 0 {
            self.rx_fifo.clear();
        }
        // FCR_CLEAR_TX: TX drains immediately in emulation — nothing buffered.
    }

    /// Handle a write to the Interrupt Enable Register.
    ///
    /// Only the low four bits are writable. Enabling the THRE interrupt while
    /// the (always-empty) holding register is empty asserts it once.
    fn write_ier(&mut self, value: u8) {
        let prev = self.ier;
        self.ier = value & 0x0F;
        if self.ier & IER_THR_EMPTY != 0 && prev & IER_THR_EMPTY == 0 {
            self.thr_empty_pending = true;
        }
    }

    // -- RX path --

    fn read_data(&mut self) -> u8 {
        self.rx_fifo.pop_front().unwrap_or(0)
    }

    /// Compute the Line Status Register value dynamically.
    fn compute_lsr(&self) -> u8 {
        let mut lsr: u8 = 0;

        // TX side: we always accept data immediately (no real hardware delay).
        lsr |= LSR_THR_EMPTY | LSR_TEMT;

        // RX side: data ready if there are bytes in the FIFO.
        if !self.rx_fifo.is_empty() {
            lsr |= LSR_DATA_READY;
        }

        lsr | self.lsr_overrides
    }

    // -- Interrupt model (16550: RX-available outranks THR-empty) --

    /// Compute the IIR identification byte: the highest-priority *enabled and
    /// pending* interrupt source, or [`IIR_NO_INTERRUPT`] when none.
    fn compute_iir(&self) -> u8 {
        if self.ier & IER_RX_AVAILABLE != 0 && !self.rx_fifo.is_empty() {
            IIR_RX_AVAILABLE
        } else if self.ier & IER_THR_EMPTY != 0 && self.thr_empty_pending {
            IIR_THR_EMPTY
        } else {
            IIR_NO_INTERRUPT
        }
    }

    /// Read the IIR. Per the 16550, reading IIR acknowledges (clears) a pending
    /// THRE interrupt — but only when THRE is the source actually reported. When
    /// the FIFOs are enabled, bits 7:6 read back as `0b11` so a guest's
    /// autoconfig identifies the part as a 16550A.
    fn read_iir(&mut self) -> u8 {
        let iir = self.compute_iir();
        if iir == IIR_THR_EMPTY {
            self.thr_empty_pending = false;
        }
        if self.fifo_enabled {
            iir | IIR_FIFO_ENABLED
        } else {
            iir
        }
    }

    /// Whether an enabled interrupt source is currently pending — i.e. whether
    /// the IRQ line (IRQ4 for COM1) should be asserted.
    #[must_use]
    pub fn interrupt_pending(&self) -> bool {
        self.compute_iir() != IIR_NO_INTERRUPT
    }

    /// Recompute the interrupt level and notify the attached [`IrqLine`] on a
    /// real edge (no notification when the level is unchanged).
    fn update_irq(&mut self) {
        let level = self.interrupt_pending();
        if level != self.irq_level {
            self.irq_level = level;
            if let Some(line) = &self.irq_line {
                line.set_level(level);
            }
        }
    }

    // -- Host-side helpers --

    /// Inject bytes into the RX FIFO (as if typed on the guest's console).
    ///
    /// The FIFO is bounded at [`RX_FIFO_CAPACITY`]: once full, further incoming
    /// bytes are dropped and the LSR Overrun Error bit is set (the 16550's
    /// overrun behaviour), so a guest that stops draining the port cannot grow
    /// host memory without bound.
    pub fn inject_input(&mut self, data: &[u8]) {
        for &byte in data {
            if self.rx_fifo.len() >= RX_FIFO_CAPACITY {
                // FIFO full: drop the byte and flag the overrun (sticky until
                // the guest reads the LSR). The already-queued bytes are kept.
                self.lsr_overrides |= LSR_OVERRUN_ERROR;
                break;
            }
            self.rx_fifo.push_back(byte);
        }
        // New RX data may assert the RX-available interrupt.
        self.update_irq();
    }

    /// Read a single byte from the RX FIFO, or `None` if empty.
    pub fn read_byte(&mut self) -> Option<u8> {
        let byte = self.rx_fifo.pop_front();
        // Draining the FIFO may deassert the RX-available interrupt.
        self.update_irq();
        byte
    }

    /// Check whether the RX FIFO has data available.
    #[must_use]
    pub fn has_input(&self) -> bool {
        !self.rx_fifo.is_empty()
    }

    /// Return a reference to the underlying output sink.
    #[must_use]
    pub const fn output(&self) -> &SerialOutput {
        &self.output
    }

    /// Return a mutable reference to the underlying output sink.
    pub const fn output_mut(&mut self) -> &mut SerialOutput {
        &mut self.output
    }
}

// ---------------------------------------------------------------------------
// SerialPort — a UART mounted on the device bus as a PIO device
// ---------------------------------------------------------------------------

/// A single 16550 UART exposed on the port-I/O bus as a [`PioDevice`].
///
/// Bundles a [`UartState`] with its COM base port and claims the half-open
/// range `[base, base + 8)`. Guest `IN`/`OUT` on those ports are dispatched here
/// by the bus (see `enlil-core::device_bus::DeviceBus`); the absolute port is
/// translated to the 0–7 register offset the UART expects. Register one on a
/// [`crate::device_bus::DeviceBus`] to give a guest a working serial console.
///
/// The UART supports both **polled** operation (the guest reads the Line Status
/// Register for TX-ready / RX-available) and **interrupt-driven** operation:
/// attach an [`IrqLine`] via [`Self::attach_irq_line`] and the UART asserts /
/// deasserts IRQ4 as enabled sources (RX-available, THR-empty) become pending,
/// honouring the IER. The KVM backend wires that line to `KVM_IRQ_LINE`.
pub struct SerialPort {
    base: u16,
    uart: UartState,
}

impl SerialPort {
    /// Create a serial port at `base` backed by `uart`.
    #[must_use]
    pub const fn new(base: u16, uart: UartState) -> Self {
        Self { base, uart }
    }

    /// Convenience: a COM1 (`0x3F8`) serial port routing TX to `output`.
    #[must_use]
    pub fn com1(output: SerialOutput) -> Self {
        Self::new(COM1, UartState::new(output))
    }

    /// The COM base port this device is mapped at.
    #[must_use]
    pub const fn base(&self) -> u16 {
        self.base
    }

    /// Borrow the underlying UART (e.g. to inject RX input).
    #[must_use]
    pub const fn uart(&self) -> &UartState {
        &self.uart
    }

    /// Mutably borrow the underlying UART (e.g. to inject RX input or read TX).
    pub const fn uart_mut(&mut self) -> &mut UartState {
        &mut self.uart
    }

    /// Attach a host interrupt sink so the UART drives IRQ4 in interrupt mode.
    ///
    /// Forwards to [`UartState::set_irq_line`]; see [`IrqLine`].
    pub fn attach_irq_line(&mut self, line: Box<dyn IrqLine>) {
        self.uart.set_irq_line(line);
    }

    /// Whether the UART currently has an enabled interrupt source pending
    /// (i.e. whether IRQ4 should be asserted). Useful for a poll-after-exit
    /// driver that doesn't use an [`IrqLine`] callback.
    #[must_use]
    pub fn interrupt_pending(&self) -> bool {
        self.uart.interrupt_pending()
    }
}

impl PioDevice for SerialPort {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        // The bus only routes ports within our range here, so `port >= base`.
        let offset = port.wrapping_sub(self.base);
        u32::from(self.uart.read_register(offset))
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        let offset = port.wrapping_sub(self.base);
        // Serial registers are byte-wide; the guest writes the low byte.
        self.uart.write_register(offset, data.to_le_bytes()[0]);
    }

    fn port_range(&self) -> (u16, u16) {
        (self.base, self.base + SERIAL_PORT_COUNT)
    }
}

// ---------------------------------------------------------------------------
// SerialMultiplexer — manages serial ports for all guests
// ---------------------------------------------------------------------------

/// Guest identifier — just a string name for now.
pub type GuestId = String;

/// Manages emulated serial ports for multiple guests.
///
/// Each guest is registered with a unique name and gets its own [`UartState`].
/// The multiplexer routes I/O port reads/writes to the correct guest's UART.
pub struct SerialMultiplexer {
    /// Map from guest name → UART state.
    guests: HashMap<GuestId, UartState>,
}

impl SerialMultiplexer {
    /// Create an empty multiplexer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            guests: HashMap::new(),
        }
    }

    /// Register a guest with the given serial configuration.
    pub fn add_guest(&mut self, guest_id: impl Into<GuestId>, config: &SerialConfig) {
        let id = guest_id.into();
        let output = SerialOutput::new(&id, config.mode.clone());
        let uart = UartState::new(output);
        self.guests.insert(id, uart);
    }

    /// Register a guest with a pre-built [`UartState`] (useful for testing).
    pub fn add_guest_with_uart(&mut self, guest_id: impl Into<GuestId>, uart: UartState) {
        self.guests.insert(guest_id.into(), uart);
    }

    /// Remove a guest, flushing its output.
    pub fn remove_guest(&mut self, guest_id: &str) -> Option<UartState> {
        self.guests.remove(guest_id)
    }

    /// Handle a guest writing to a serial I/O port.
    ///
    /// `offset` is 0–7 relative to the guest's COM base port.
    pub fn handle_write(&mut self, guest_id: &str, offset: u16, value: u8) {
        if let Some(uart) = self.guests.get_mut(guest_id) {
            uart.write_register(offset, value);
        } else {
            log::warn!("serial: write to unknown guest '{guest_id}'");
        }
    }

    /// Handle a guest reading from a serial I/O port.
    ///
    /// `offset` is 0–7 relative to the guest's COM base port.
    pub fn handle_read(&mut self, guest_id: &str, offset: u16) -> u8 {
        self.guests.get_mut(guest_id).map_or_else(
            || {
                log::warn!("serial: read from unknown guest '{guest_id}'");
                0xFF
            },
            |uart| uart.read_register(offset),
        )
    }

    /// Inject input bytes into a guest's RX FIFO.
    pub fn inject_input(&mut self, guest_id: &str, data: &[u8]) {
        if let Some(uart) = self.guests.get_mut(guest_id) {
            uart.inject_input(data);
        }
    }

    /// Get a reference to a guest's UART state.
    #[must_use]
    pub fn get_uart(&self, guest_id: &str) -> Option<&UartState> {
        self.guests.get(guest_id)
    }

    /// Get a mutable reference to a guest's UART state.
    pub fn get_uart_mut(&mut self, guest_id: &str) -> Option<&mut UartState> {
        self.guests.get_mut(guest_id)
    }

    /// Return the number of registered guests.
    #[must_use]
    pub fn guest_count(&self) -> usize {
        self.guests.len()
    }
}

impl Default for SerialMultiplexer {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe wrapper around the multiplexer.
pub type SharedSerialMultiplexer = Arc<Mutex<SerialMultiplexer>>;

/// Create a new shared multiplexer.
#[must_use]
pub fn shared_multiplexer() -> SharedSerialMultiplexer {
    Arc::new(Mutex::new(SerialMultiplexer::new()))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- SerialOutput mode tests --

    #[test]
    fn test_null_mode_discards_output() {
        let mut out = SerialOutput::new("guest0", SerialOutputMode::Null);
        for &b in b"Hello, world!\n" {
            out.write_byte(b);
        }
        // Null mode: buffer_contents is always empty.
        assert!(out.buffer_contents().is_empty());
    }

    #[test]
    fn test_buffer_mode_collects_bytes() {
        let mut out = SerialOutput::new("guest0", SerialOutputMode::Buffer);
        let msg = b"Hello\nWorld\n";
        for &b in msg {
            out.write_byte(b);
        }
        assert_eq!(out.buffer_contents(), msg);
    }

    #[test]
    fn test_shared_mode_appends_to_caller_buffer() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut out = SerialOutput::new("guest0", SerialOutputMode::Shared(Arc::clone(&sink)));
        for &b in b"Hi\n" {
            out.write_byte(b);
        }
        // Shared mode forwards raw bytes (no line buffering) to the caller's Vec.
        assert_eq!(&*sink.lock().unwrap(), b"Hi\n");
    }

    #[test]
    fn test_buffer_take() {
        let mut out = SerialOutput::new("guest0", SerialOutputMode::Buffer);
        out.write_byte(b'A');
        out.write_byte(b'B');
        let taken = out.take_buffer();
        assert_eq!(taken, vec![b'A', b'B']);
        assert!(out.buffer_contents().is_empty());
    }

    #[test]
    fn test_stdout_mode_does_not_panic() {
        // We can't easily capture stdout in a unit test, but we verify
        // that writing doesn't panic.
        let mut out = SerialOutput::new("guest0", SerialOutputMode::Stdout);
        for &b in b"test line\n" {
            out.write_byte(b);
        }
        out.flush();
    }

    #[test]
    fn test_file_mode_writes_to_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("enlil_serial_test.log");
        // Clean up from any previous run.
        let _ = std::fs::remove_file(&path);

        {
            let mut out = SerialOutput::new(
                "guest0",
                SerialOutputMode::File(path.to_string_lossy().into_owned()),
            );
            for &b in b"hello from guest\n" {
                out.write_byte(b);
            }
            out.flush();
        }

        let contents = std::fs::read_to_string(&path).expect("file should exist");
        assert!(
            contents.contains("[guest0] hello from guest"),
            "file contents: {contents:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // -- UartState tests --

    #[test]
    fn test_uart_lsr_reports_tx_ready() {
        let out = SerialOutput::new("g", SerialOutputMode::Null);
        let uart = UartState::new(out);
        let lsr = uart.compute_lsr();
        assert_ne!(lsr & LSR_THR_EMPTY, 0, "THR should be empty");
        assert_ne!(lsr & LSR_TEMT, 0, "transmitter should be empty");
    }

    #[test]
    fn test_uart_lsr_reports_rx_available() {
        let out = SerialOutput::new("g", SerialOutputMode::Null);
        let mut uart = UartState::new(out);

        // No data initially.
        assert_eq!(uart.compute_lsr() & LSR_DATA_READY, 0);

        // Inject a byte.
        uart.inject_input(b"X");
        assert_ne!(uart.compute_lsr() & LSR_DATA_READY, 0);

        // Read it out.
        let b = uart.read_register(DATA_REG);
        assert_eq!(b, b'X');
        assert_eq!(uart.compute_lsr() & LSR_DATA_READY, 0);
    }

    #[test]
    fn test_uart_tx_goes_to_output() {
        let out = SerialOutput::new("g", SerialOutputMode::Buffer);
        let mut uart = UartState::new(out);

        uart.write_register(DATA_REG, b'H');
        uart.write_register(DATA_REG, b'i');

        assert_eq!(uart.output().buffer_contents(), b"Hi");
    }

    #[test]
    fn test_uart_rx_fifo_ordering() {
        let out = SerialOutput::new("g", SerialOutputMode::Null);
        let mut uart = UartState::new(out);

        uart.inject_input(b"ABC");
        assert_eq!(uart.read_byte(), Some(b'A'));
        assert_eq!(uart.read_byte(), Some(b'B'));
        assert_eq!(uart.read_byte(), Some(b'C'));
        assert_eq!(uart.read_byte(), None);
    }

    #[test]
    fn rx_fifo_is_bounded_and_flags_overrun() {
        let mut uart = null_uart();
        // Inject more than the FIFO can hold in one burst.
        let flood = vec![b'.'; RX_FIFO_CAPACITY + 100];
        uart.inject_input(&flood);

        // The FIFO never grows past its cap...
        assert_eq!(uart.rx_fifo.len(), RX_FIFO_CAPACITY);
        // ...and the dropped bytes raised the Overrun Error in the LSR.
        assert_ne!(uart.compute_lsr() & LSR_OVERRUN_ERROR, 0);

        // Reading the LSR clears the sticky Overrun Error bit.
        let lsr = uart.read_register(LSR_REG);
        assert_ne!(lsr & LSR_OVERRUN_ERROR, 0, "OE reported on the read");
        assert_eq!(
            uart.read_register(LSR_REG) & LSR_OVERRUN_ERROR,
            0,
            "OE cleared by the previous LSR read"
        );
    }

    #[test]
    fn rx_overrun_drops_newest_and_keeps_earliest() {
        let mut uart = null_uart();
        // Fill exactly to capacity with a known first byte, then overflow.
        let mut data = vec![b'x'; RX_FIFO_CAPACITY];
        data[0] = b'A';
        uart.inject_input(&data);
        assert_eq!(
            uart.compute_lsr() & LSR_OVERRUN_ERROR,
            0,
            "exactly full, no OE"
        );

        // One more byte overflows: it is dropped (not the earliest), and OE sets.
        uart.inject_input(b"Z");
        assert_ne!(uart.compute_lsr() & LSR_OVERRUN_ERROR, 0);
        // The earliest byte is still first out of the FIFO.
        assert_eq!(uart.read_byte(), Some(b'A'));
        assert_eq!(
            uart.rx_fifo.len(),
            RX_FIFO_CAPACITY - 1,
            "no 'Z' was queued"
        );
    }

    #[test]
    fn test_uart_read_data_returns_zero_when_empty() {
        let out = SerialOutput::new("g", SerialOutputMode::Null);
        let mut uart = UartState::new(out);
        assert_eq!(uart.read_register(DATA_REG), 0);
    }

    #[test]
    fn test_uart_scratch_register() {
        let out = SerialOutput::new("g", SerialOutputMode::Null);
        let mut uart = UartState::new(out);
        uart.write_register(SCR_REG, 0xAB);
        assert_eq!(uart.read_register(SCR_REG), 0xAB);
    }

    #[test]
    fn test_uart_lcr_roundtrip() {
        let out = SerialOutput::new("g", SerialOutputMode::Null);
        let mut uart = UartState::new(out);
        uart.write_register(LCR_REG, 0x1B);
        assert_eq!(uart.read_register(LCR_REG), 0x1B);
    }

    #[test]
    fn test_uart_divisor_latch() {
        let out = SerialOutput::new("g", SerialOutputMode::Buffer);
        let mut uart = UartState::new(out);

        // Set DLAB.
        uart.write_register(LCR_REG, 0x83); // 8N1 + DLAB
                                            // Write divisor low and high.
        uart.write_register(DATA_REG, 0x01); // low byte
        uart.write_register(IER_REG, 0x00); // high byte
        assert_eq!(uart.divisor, 0x0001); // 115200 baud

        // Clear DLAB and verify normal operation resumes.
        uart.write_register(LCR_REG, 0x03);
        uart.write_register(DATA_REG, b'Z');
        assert_eq!(uart.output().buffer_contents(), b"Z");
    }

    #[test]
    fn test_uart_ier_masked_to_4_bits() {
        let out = SerialOutput::new("g", SerialOutputMode::Null);
        let mut uart = UartState::new(out);
        uart.write_register(IER_REG, 0xFF);
        assert_eq!(uart.read_register(IER_REG), 0x0F);
    }

    // -- SerialMultiplexer tests --

    #[test]
    fn test_multiplexer_add_remove_guest() {
        let mut mux = SerialMultiplexer::new();
        assert_eq!(mux.guest_count(), 0);

        let config = SerialConfig {
            mode: SerialOutputMode::Null,
            ..Default::default()
        };
        mux.add_guest("vm1", &config);
        mux.add_guest("vm2", &config);
        assert_eq!(mux.guest_count(), 2);

        mux.remove_guest("vm1");
        assert_eq!(mux.guest_count(), 1);
        assert!(mux.get_uart("vm1").is_none());
        assert!(mux.get_uart("vm2").is_some());
    }

    #[test]
    fn test_multiplexer_routes_tx_to_correct_guest() {
        let mut mux = SerialMultiplexer::new();

        let config_buf = SerialConfig {
            mode: SerialOutputMode::Buffer,
            ..Default::default()
        };
        mux.add_guest("alpha", &config_buf);
        mux.add_guest("beta", &config_buf);

        // Write to alpha.
        mux.handle_write("alpha", DATA_REG, b'A');
        // Write to beta.
        mux.handle_write("beta", DATA_REG, b'B');

        let alpha_out = mux.get_uart("alpha").unwrap().output().buffer_contents();
        let beta_out = mux.get_uart("beta").unwrap().output().buffer_contents();

        assert_eq!(alpha_out, b"A");
        assert_eq!(beta_out, b"B");
    }

    #[test]
    fn test_multiplexer_routes_rx_to_correct_guest() {
        let mut mux = SerialMultiplexer::new();

        let config = SerialConfig {
            mode: SerialOutputMode::Null,
            ..Default::default()
        };
        mux.add_guest("vm1", &config);
        mux.add_guest("vm2", &config);

        mux.inject_input("vm1", b"hello");
        mux.inject_input("vm2", b"world");

        // Read from vm1.
        assert_eq!(mux.handle_read("vm1", DATA_REG), b'h');
        // Read from vm2.
        assert_eq!(mux.handle_read("vm2", DATA_REG), b'w');

        // LSR should show data ready for both.
        let lsr1 = mux.handle_read("vm1", LSR_REG);
        assert_ne!(lsr1 & LSR_DATA_READY, 0);
    }

    #[test]
    fn test_multiplexer_unknown_guest_returns_ff() {
        let mut mux = SerialMultiplexer::new();
        assert_eq!(mux.handle_read("nonexistent", DATA_REG), 0xFF);
    }

    #[test]
    fn test_multiplexer_inject_and_drain() {
        let mut mux = SerialMultiplexer::new();
        let config = SerialConfig {
            mode: SerialOutputMode::Null,
            ..Default::default()
        };
        mux.add_guest("vm", &config);

        mux.inject_input("vm", b"XY");

        let uart = mux.get_uart_mut("vm").unwrap();
        assert_eq!(uart.read_byte(), Some(b'X'));
        assert_eq!(uart.read_byte(), Some(b'Y'));
        assert_eq!(uart.read_byte(), None);
    }

    #[test]
    fn test_multiplexer_guest_isolation() {
        // Verify that writing to one guest's registers doesn't affect another.
        let mut mux = SerialMultiplexer::new();
        let config = SerialConfig {
            mode: SerialOutputMode::Buffer,
            ..Default::default()
        };
        mux.add_guest("a", &config);
        mux.add_guest("b", &config);

        // Write scratch register to 'a'.
        mux.handle_write("a", SCR_REG, 0x42);
        // 'b' scratch should still be 0.
        assert_eq!(mux.handle_read("b", SCR_REG), 0x00);
        assert_eq!(mux.handle_read("a", SCR_REG), 0x42);
    }

    // -- SerialPort (bus device) tests --

    #[test]
    fn test_serial_port_claims_eight_ports_at_base() {
        let port = SerialPort::com1(SerialOutput::new("g", SerialOutputMode::Null));
        assert_eq!(port.base(), COM1);
        assert_eq!(PioDevice::port_range(&port), (0x3F8, 0x400));
    }

    #[test]
    fn test_serial_port_write_reaches_output_at_data_reg() {
        let mut port = SerialPort::com1(SerialOutput::new("g", SerialOutputMode::Buffer));
        // Guest OUTs to the absolute data-register port (base + DATA_REG).
        port.pio_write(COM1, 1, u32::from(b'H'));
        port.pio_write(COM1, 1, u32::from(b'i'));
        assert_eq!(port.uart().output().buffer_contents(), b"Hi");
    }

    #[test]
    fn test_serial_port_maps_absolute_port_to_register_offset() {
        let mut port = SerialPort::com1(SerialOutput::new("g", SerialOutputMode::Null));
        // Scratch register lives at base + 7; a write/read there must hit SCR.
        port.pio_write(COM1 + SCR_REG, 1, 0xAB);
        assert_eq!(port.pio_read(COM1 + SCR_REG, 1), 0xAB);
    }

    #[test]
    fn test_serial_port_lsr_reports_tx_ready() {
        let mut port = SerialPort::com1(SerialOutput::new("g", SerialOutputMode::Null));
        let lsr = port.pio_read(COM1 + LSR_REG, 1);
        assert_ne!(u8::try_from(lsr & 0xFF).unwrap() & LSR_THR_EMPTY, 0);
    }

    #[test]
    fn test_serial_port_rx_injection_round_trips() {
        let mut port = SerialPort::com1(SerialOutput::new("g", SerialOutputMode::Null));
        port.uart_mut().inject_input(b"Z");
        // RX byte is available and reads back through the data register.
        assert_ne!(
            u8::try_from(port.pio_read(COM1 + LSR_REG, 1) & 0xFF).unwrap() & LSR_DATA_READY,
            0
        );
        assert_eq!(port.pio_read(COM1 + DATA_REG, 1), u32::from(b'Z'));
    }

    // -- Interrupt model tests --

    /// Records every IRQ-line edge it is driven through.
    #[derive(Clone)]
    struct EdgeLog(Arc<Mutex<Vec<bool>>>);

    impl IrqLine for EdgeLog {
        fn set_level(&self, level: bool) {
            self.0.lock().unwrap().push(level);
        }
    }

    fn null_uart() -> UartState {
        UartState::new(SerialOutput::new("g", SerialOutputMode::Null))
    }

    #[test]
    fn test_no_interrupt_when_sources_disabled() {
        let mut uart = null_uart();
        // Data is waiting but the RX interrupt is not enabled in the IER.
        uart.inject_input(b"A");
        assert!(!uart.interrupt_pending());
        assert_eq!(uart.read_register(IIR_REG), IIR_NO_INTERRUPT);
    }

    #[test]
    fn test_rx_available_interrupt() {
        let mut uart = null_uart();
        uart.write_register(IER_REG, IER_RX_AVAILABLE);
        assert!(!uart.interrupt_pending(), "no data yet");

        uart.inject_input(b"A");
        assert!(uart.interrupt_pending());
        assert_eq!(uart.read_register(IIR_REG), IIR_RX_AVAILABLE);
        // Reading IIR does NOT clear an RX interrupt — only draining RBR does.
        assert!(uart.interrupt_pending());

        assert_eq!(uart.read_register(DATA_REG), b'A');
        assert!(!uart.interrupt_pending());
        assert_eq!(uart.read_register(IIR_REG), IIR_NO_INTERRUPT);
    }

    #[test]
    fn test_thr_empty_interrupt_set_and_acked_by_iir_read() {
        let mut uart = null_uart();
        // Enabling ETBEI while the (always-empty) THR is empty asserts THRE.
        uart.write_register(IER_REG, IER_THR_EMPTY);
        assert!(uart.interrupt_pending());
        assert_eq!(uart.read_register(IIR_REG), IIR_THR_EMPTY);
        // Reading IIR acknowledged the THRE interrupt.
        assert!(!uart.interrupt_pending());
        assert_eq!(uart.read_register(IIR_REG), IIR_NO_INTERRUPT);

        // Transmitting a byte re-arms THRE (TX completes immediately).
        uart.write_register(DATA_REG, b'X');
        assert!(uart.interrupt_pending());
        assert_eq!(uart.read_register(IIR_REG), IIR_THR_EMPTY);
    }

    #[test]
    fn test_rx_outranks_thr_empty_in_iir() {
        let mut uart = null_uart();
        uart.write_register(IER_REG, IER_RX_AVAILABLE | IER_THR_EMPTY);
        // ETBEI-enable armed THRE; now RX data arrives too.
        uart.inject_input(b"Z");
        // Higher-priority RX source is reported first.
        assert_eq!(uart.read_register(IIR_REG), IIR_RX_AVAILABLE);
        // Draining RX exposes the still-pending THRE interrupt.
        assert_eq!(uart.read_register(DATA_REG), b'Z');
        assert_eq!(uart.read_register(IIR_REG), IIR_THR_EMPTY);
    }

    #[test]
    fn test_irq_line_edges_on_rx() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut uart = null_uart();
        uart.set_irq_line(Box::new(EdgeLog(Arc::clone(&log))));
        // Attaching with no pending interrupt emits no edge.
        assert!(log.lock().unwrap().is_empty());

        uart.write_register(IER_REG, IER_RX_AVAILABLE);
        assert!(log.lock().unwrap().is_empty(), "no data, no edge");

        uart.inject_input(b"A"); // rising edge
        uart.read_register(DATA_REG); // falling edge
        assert_eq!(&*log.lock().unwrap(), &[true, false]);
    }

    #[test]
    fn test_irq_line_accepts_a_closure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let asserts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asserts);
        let mut uart = null_uart();
        uart.set_irq_line(Box::new(move |level: bool| {
            if level {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }));
        // THRE asserts the line once when ETBEI is enabled.
        uart.write_register(IER_REG, IER_THR_EMPTY);
        assert_eq!(asserts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_serial_port_drives_irq_line() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut port = SerialPort::com1(SerialOutput::new("g", SerialOutputMode::Null));
        port.attach_irq_line(Box::new(EdgeLog(Arc::clone(&log))));
        assert!(!port.interrupt_pending());

        // The guest enables the RX interrupt through the data-bus register write.
        port.pio_write(COM1 + IER_REG, 1, u32::from(IER_RX_AVAILABLE));
        port.uart_mut().inject_input(b"!");
        assert!(port.interrupt_pending());
        assert_eq!(&*log.lock().unwrap(), &[true]);
    }

    // -- Loopback (MCR bit 4) tests --

    #[test]
    fn test_loopback_routes_tx_back_to_rx() {
        let mut uart = null_uart();
        // Without loopback, a TX byte goes to the sink, not the RX FIFO.
        uart.write_register(DATA_REG, b'X');
        assert!(!uart.has_input(), "no loopback: TX does not reach RX");

        // Enable loopback; a TX byte now appears in the RX FIFO and reads back.
        uart.write_register(MCR_REG, MCR_LOOP);
        uart.write_register(DATA_REG, b'L');
        assert!(uart.has_input(), "loopback: TX wired to RX");
        assert_eq!(uart.read_register(DATA_REG), b'L');
    }

    #[test]
    fn test_loopback_maps_mcr_outputs_to_msr_inputs() {
        let mut uart = null_uart();
        // Not in loopback: MSR reads the (default-zero) external lines.
        assert_eq!(uart.read_register(MSR_REG), 0);

        // The Linux 8250 autoconfig loopback probe: MCR = LOOP | OUT2 | RTS,
        // then expects MSR & 0xF0 == DCD | CTS == 0x90.
        uart.write_register(MCR_REG, MCR_LOOP | MCR_OUT2 | MCR_RTS);
        let msr = uart.read_register(MSR_REG);
        assert_eq!(msr & 0xF0, MSR_DCD | MSR_CTS, "RTS→CTS, OUT2→DCD");

        // All four outputs map to all four inputs.
        uart.write_register(MCR_REG, MCR_LOOP | MCR_DTR | MCR_RTS | MCR_OUT1 | MCR_OUT2);
        let msr = uart.read_register(MSR_REG);
        assert_eq!(
            msr & 0xF0,
            MSR_DSR | MSR_CTS | MSR_RI | MSR_DCD,
            "DTR→DSR, RTS→CTS, OUT1→RI, OUT2→DCD"
        );
    }

    #[test]
    fn test_loopback_msr_delta_bits_set_and_clear_on_read() {
        let mut uart = null_uart();
        uart.write_register(MCR_REG, MCR_LOOP); // enter loopback, all inputs low
        let _ = uart.read_register(MSR_REG); // clear any initial deltas

        // Raise RTS (→CTS): DCTS delta latches; the read returns it and clears.
        uart.write_register(MCR_REG, MCR_LOOP | MCR_RTS);
        let msr = uart.read_register(MSR_REG);
        assert_ne!(msr & MSR_DCTS, 0, "CTS change set the DCTS delta");
        assert_ne!(msr & MSR_CTS, 0, "CTS input asserted");
        // Delta is read-to-clear: a second read shows no delta, input still set.
        let msr2 = uart.read_register(MSR_REG);
        assert_eq!(msr2 & MSR_DCTS, 0, "delta cleared on read");
        assert_ne!(msr2 & MSR_CTS, 0, "input level persists");

        // Dropping RI (OUT1 1→0) latches the trailing-edge TERI delta.
        uart.write_register(MCR_REG, MCR_LOOP | MCR_OUT1);
        let _ = uart.read_register(MSR_REG);
        uart.write_register(MCR_REG, MCR_LOOP); // OUT1 1→0
        assert_ne!(uart.read_register(MSR_REG) & MSR_TERI, 0, "RI trailing edge");
    }

    #[test]
    fn test_loopback_looped_byte_raises_rx_interrupt() {
        let mut uart = null_uart();
        uart.write_register(IER_REG, IER_RX_AVAILABLE);
        uart.write_register(MCR_REG, MCR_LOOP);
        assert!(!uart.interrupt_pending());
        // A looped-back TX byte is RX data — it raises the RX-available IRQ.
        uart.write_register(DATA_REG, b'!');
        assert!(uart.interrupt_pending());
        assert_eq!(uart.read_register(IIR_REG), IIR_RX_AVAILABLE);
    }

    // -- FIFO control (FCR / IIR bits 7:6) tests --

    #[test]
    fn test_fifo_detection_reports_16550a_when_enabled() {
        let mut uart = null_uart();
        // FIFOs off by default: IIR bits 7:6 read back 0 (a guest sees 8250).
        assert_eq!(uart.read_register(IIR_REG) & 0xC0, 0);
        // Enabling the FIFO (FCR write to offset 2) makes IIR bits 7:6 read
        // 0b11 — the 16550A signature a guest's autoconfig looks for.
        uart.write_register(IIR_REG, FCR_ENABLE);
        assert_eq!(uart.read_register(IIR_REG) & 0xC0, IIR_FIFO_ENABLED);
        // Bit 0 still reflects "no interrupt pending".
        assert_ne!(uart.read_register(IIR_REG) & IIR_NO_INTERRUPT, 0);
    }

    #[test]
    fn test_fcr_clear_rx_flushes_the_receive_fifo() {
        let mut uart = null_uart();
        uart.inject_input(b"stale");
        assert!(uart.has_input());
        // FCR with the RX-clear bit empties the receive FIFO.
        uart.write_register(IIR_REG, FCR_ENABLE | FCR_CLEAR_RX);
        assert!(!uart.has_input(), "RX FIFO cleared by FCR");
    }

    #[test]
    fn test_shared_multiplexer() {
        let shared = shared_multiplexer();
        {
            let mut mux = shared.lock().unwrap();
            let config = SerialConfig {
                mode: SerialOutputMode::Null,
                ..Default::default()
            };
            mux.add_guest("vm1", &config);
        }
        {
            let mux = shared.lock().unwrap();
            assert_eq!(mux.guest_count(), 1);
            drop(mux);
        }
    }
}
