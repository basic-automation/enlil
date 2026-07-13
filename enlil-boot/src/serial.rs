//! Minimal COM1 16550 UART writer for output after `ExitBootServices()`.
//!
//! Once the payload calls `ExitBootServices()` the uefi-rs logger and global
//! allocator are gone, so it can no longer `log::info!`. To still prove it is
//! alive and running under its own control, it writes the liveness banner
//! straight to the COM1 serial port with raw port I/O — the same line the
//! QEMU+OVMF harness asserts on.
//!
//! The divisor arithmetic is a pure function tested on the dev host; the
//! actual port pokes assemble only for the firmware target (`target_os =
//! "uefi"`), so the register programming never runs — or faults — on the
//! hosted test build.

/// COM1 base I/O port.
pub const COM1_BASE: u16 = 0x3F8;

/// The 16550 UART reference input clock, in Hz. The programmed baud rate is
/// this clock divided by the divisor latch value.
pub const UART_CLOCK_HZ: u32 = 115_200;

/// Divisor latch value that yields `baud` from the [`UART_CLOCK_HZ`] clock.
///
/// Clamped to at least 1 (a zero divisor is invalid) and at most
/// [`u16::MAX`] (the latch is 16 bits). A `baud` of 0 is treated as "fastest",
/// i.e. divisor 1.
#[must_use]
pub fn divisor_for_baud(baud: u32) -> u16 {
    if baud == 0 {
        return 1;
    }
    let divisor = (UART_CLOCK_HZ / baud).max(1);
    u16::try_from(divisor).unwrap_or(u16::MAX)
}

#[cfg(target_os = "uefi")]
pub use hw::SerialPort;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{COM1_BASE, UART_CLOCK_HZ, divisor_for_baud};
    use core::arch::asm;

    // 16550 register offsets from the port base.
    const REG_DATA: u16 = 0; // RBR/THR, or the divisor low byte when DLAB=1
    const REG_INT_ENABLE: u16 = 1; // IER, or the divisor high byte when DLAB=1
    const REG_FIFO_CTRL: u16 = 2; // FCR (write-only)
    const REG_LINE_CTRL: u16 = 3; // LCR
    const REG_MODEM_CTRL: u16 = 4; // MCR
    const REG_LINE_STATUS: u16 = 5; // LSR (read-only)

    const LCR_8N1: u8 = 0x03; // 8 data bits, no parity, 1 stop bit
    const LCR_DLAB: u8 = 0x80; // Divisor Latch Access Bit
    const FCR_ENABLE_CLEAR: u8 = 0xC7; // enable FIFO, clear RX/TX, 14-byte trigger
    const MCR_DTR_RTS_OUT2: u8 = 0x0B; // DTR + RTS + OUT2 (needed for IRQs / QEMU)
    const LSR_THR_EMPTY: u8 = 0x20; // transmit holding register empty

    /// Write a byte to an x86 I/O port.
    ///
    /// # Safety
    ///
    /// The caller must ensure `port` is a valid device register and that this
    /// runs at a privilege level allowed to issue `out` (ring 0 / firmware).
    #[inline]
    unsafe fn outb(port: u16, value: u8) {
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// Read a byte from an x86 I/O port.
    ///
    /// # Safety
    ///
    /// The caller must ensure `port` is a valid device register and that this
    /// runs at a privilege level allowed to issue `in`.
    #[inline]
    unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        unsafe {
            asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// A COM1 serial port programmed for polled byte output.
    ///
    /// Constructed after `ExitBootServices()`, when the firmware console is no
    /// longer available, to emit the payload's liveness banner.
    pub struct SerialPort {
        base: u16,
    }

    impl SerialPort {
        /// Initialize COM1 at 115200 baud, 8N1, FIFOs enabled, and return a
        /// handle ready for [`write_str`](Self::write_str).
        #[must_use]
        pub fn com1() -> Self {
            let base = COM1_BASE;
            // Run at 115200 baud (divisor 1) — the rate the QEMU serial line
            // and the boot harness expect.
            let [divisor_lo, divisor_hi] = divisor_for_baud(UART_CLOCK_HZ).to_le_bytes();
            unsafe {
                outb(base + REG_INT_ENABLE, 0x00); // mask all UART interrupts
                outb(base + REG_LINE_CTRL, LCR_DLAB); // unlock the divisor latch
                outb(base + REG_DATA, divisor_lo); // divisor low byte
                outb(base + REG_INT_ENABLE, divisor_hi); // divisor high byte
                outb(base + REG_LINE_CTRL, LCR_8N1); // relock latch, set 8N1
                outb(base + REG_FIFO_CTRL, FCR_ENABLE_CLEAR);
                outb(base + REG_MODEM_CTRL, MCR_DTR_RTS_OUT2);
            }
            Self { base }
        }

        /// Busy-wait until the transmit holding register can accept a byte.
        fn wait_writable(&self) {
            while unsafe { inb(self.base + REG_LINE_STATUS) } & LSR_THR_EMPTY == 0 {}
        }

        /// Write one byte, expanding a line feed to CR+LF so serial terminals
        /// render lines correctly.
        pub fn write_byte(&self, byte: u8) {
            if byte == b'\n' {
                self.wait_writable();
                unsafe { outb(self.base + REG_DATA, b'\r') };
            }
            self.wait_writable();
            unsafe { outb(self.base + REG_DATA, byte) };
        }

        /// Write a string to the port.
        pub fn write_str(&self, text: &str) {
            for &byte in text.as_bytes() {
                self.write_byte(byte);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::divisor_for_baud;

    #[test]
    fn divisor_115200_is_one() {
        assert_eq!(divisor_for_baud(115_200), 1);
    }

    #[test]
    fn divisor_9600_is_twelve() {
        assert_eq!(divisor_for_baud(9_600), 12);
    }

    #[test]
    fn divisor_300_baud() {
        assert_eq!(divisor_for_baud(300), 384);
    }

    #[test]
    fn divisor_clamps_absurdly_low_baud_to_max_latch() {
        // 115200 / 1 = 115200 overflows the 16-bit latch → clamp to u16::MAX.
        assert_eq!(divisor_for_baud(1), u16::MAX);
    }

    #[test]
    fn divisor_zero_baud_defaults_to_one() {
        assert_eq!(divisor_for_baud(0), 1);
    }
}
