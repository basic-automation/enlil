//! Serial console abstraction.
//!
//! Provides a COM1 serial port for each guest VM.
//! On Linux with KVM, this wraps vm-superio's Serial device.
//! For bare-metal, we'll implement direct UART emulation.

use std::io::{self, Write};

/// Standard x86 COM port base addresses.
pub const COM1: u16 = 0x3F8;
pub const COM2: u16 = 0x2F8;
pub const COM3: u16 = 0x3E8;
pub const COM4: u16 = 0x2E8;

/// Serial port register offsets.
pub const DATA_REG: u16 = 0;
pub const IER_REG: u16 = 1;
pub const IIR_REG: u16 = 2;
pub const LCR_REG: u16 = 3;
pub const MCR_REG: u16 = 4;
pub const LSR_REG: u16 = 5;
pub const MSR_REG: u16 = 6;

/// Configuration for a guest serial console.
#[derive(Debug, Clone)]
pub struct SerialConfig {
    /// Base I/O port (e.g., 0x3F8 for COM1).
    pub base_port: u16,
    /// IRQ line for the serial device.
    pub irq: u8,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            base_port: COM1,
            irq: 4, // COM1 traditionally uses IRQ 4
        }
    }
}

/// A simple serial output sink that writes to the host's stdout.
/// Each guest gets its own, prefixed with the guest name.
pub struct SerialOutput {
    prefix: String,
    buffer: Vec<u8>,
}

impl SerialOutput {
    pub fn new(guest_name: &str) -> Self {
        Self {
            prefix: format!("[{}] ", guest_name),
            buffer: Vec::with_capacity(256),
        }
    }

    /// Handle a byte written to the serial data register.
    pub fn write_byte(&mut self, byte: u8) {
        if byte == b'\n' || byte == b'\r' {
            if !self.buffer.is_empty() {
                let line = String::from_utf8_lossy(&self.buffer);
                let _ = writeln!(io::stdout(), "{}{}", self.prefix, line);
                self.buffer.clear();
            }
        } else {
            self.buffer.push(byte);
        }
    }
}
