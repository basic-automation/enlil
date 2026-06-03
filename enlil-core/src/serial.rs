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
/// Transmitter Holding Register Empty — TX is ready to accept a byte.
pub const LSR_THR_EMPTY: u8 = 0x20;
/// Transmitter Empty — both THR and shift register are empty.
pub const LSR_TEMT: u8 = 0x40;

// ---------------------------------------------------------------------------
// IIR bit patterns
// ---------------------------------------------------------------------------

/// No interrupt pending.
pub const IIR_NO_INTERRUPT: u8 = 0x01;

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
    /// Collect output in an in-memory buffer (useful for testing).
    Buffer,
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
            SerialOutputMode::Buffer => {
                // line_buf isn't used for Buffer mode, but just in case:
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
// UartState — per-guest UART register emulation
// ---------------------------------------------------------------------------

/// Emulated 8250/16550 UART register state for a single guest serial port.
///
/// Tracks the standard register file and an RX input queue so guests can
/// both write (TX) and read (RX) through the serial port.
pub struct UartState {
    /// Interrupt Enable Register.
    pub ier: u8,
    /// Interrupt Identification Register (read-only to guest).
    pub iir: u8,
    /// Line Control Register.
    pub lcr: u8,
    /// Modem Control Register.
    pub mcr: u8,
    /// Line Status Register (dynamically computed on read).
    lsr_overrides: u8,
    /// Modem Status Register.
    pub msr: u8,
    /// Scratch Register.
    pub scr: u8,
    /// Divisor latch (when DLAB=1, `DATA_REG` and `IER_REG` access this).
    pub divisor: u16,
    /// Receive buffer — bytes injected by the host for the guest to read.
    rx_fifo: VecDeque<u8>,
    /// The output sink for transmitted bytes.
    output: SerialOutput,
}

impl UartState {
    /// Create a new UART with the given output sink.
    #[must_use]
    pub fn new(output: SerialOutput) -> Self {
        Self {
            ier: 0,
            iir: IIR_NO_INTERRUPT,
            lcr: 0x03, // 8N1 default
            mcr: 0,
            lsr_overrides: 0,
            msr: 0,
            scr: 0,
            divisor: 0x000C, // 9600 baud default (115200 / 9600 = 12)
            rx_fifo: VecDeque::with_capacity(64),
            output,
        }
    }

    // -- Register reads (guest IN instruction) --

    /// Read a UART register. `offset` is 0–7 relative to base port.
    pub fn read_register(&mut self, offset: u16) -> u8 {
        let dlab = self.lcr & 0x80 != 0;

        match offset {
            DATA_REG if dlab => self.divisor as u8,
            DATA_REG => self.read_data(),
            IER_REG if dlab => (self.divisor >> 8) as u8,
            IER_REG => self.ier,
            IIR_REG => self.iir,
            LCR_REG => self.lcr,
            MCR_REG => self.mcr,
            LSR_REG => self.compute_lsr(),
            MSR_REG => self.msr,
            SCR_REG => self.scr,
            _ => 0xFF, // unmapped
        }
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
            IER_REG => self.ier = value & 0x0F,
            LCR_REG => self.lcr = value,
            MCR_REG => self.mcr = value & 0x1F,
            SCR_REG => self.scr = value,
            _ => {}
        }
    }

    // -- TX path --

    fn write_data(&mut self, byte: u8) {
        self.output.write_byte(byte);
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

    // -- Host-side helpers --

    /// Inject bytes into the RX FIFO (as if typed on the guest's console).
    pub fn inject_input(&mut self, data: &[u8]) {
        self.rx_fifo.extend(data);
    }

    /// Read a single byte from the RX FIFO, or `None` if empty.
    pub fn read_byte(&mut self) -> Option<u8> {
        self.rx_fifo.pop_front()
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
