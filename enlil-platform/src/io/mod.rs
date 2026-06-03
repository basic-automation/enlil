//! I/O Trait Layer — Platform-abstracted I/O primitives
//!
//! Provides a trait-based I/O registry so that `print!`, `log::info!()`, etc.
//! work from day one, regardless of whether we're on Linux or bare-metal.
//!
//! # Backends
//!
//! - **Linux:** Delegates to `std::io` (stdout/stderr).
//! - **Bare-metal:** Serial port (COM1) for early boot, framebuffer console later.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

// ===========================================================================
// PlatformIo trait
// ===========================================================================

/// Core I/O trait — backends register themselves as implementations.
///
/// This is the lowest-level I/O abstraction. Serial ports, framebuffer
/// consoles, and any other output device implement this trait.
pub trait PlatformIo: Send + Sync {
    /// Read bytes from this I/O device.
    ///
    /// # Errors
    ///
    /// Returns an error if the read operation fails, such as when the device
    /// is unavailable, the read is interrupted, or the device does not support reading.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Write bytes to this I/O device.
    ///
    /// # Errors
    ///
    /// Returns an error if the write operation fails, such as when the device
    /// is unavailable, the write is interrupted, or the device is full.
    fn write(&self, buf: &[u8]) -> io::Result<usize>;

    /// Flush any buffered output.
    ///
    /// # Errors
    ///
    /// Returns an error if the flush operation fails, such as when the device
    /// is unavailable or the underlying I/O system encounters an error.
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    /// Returns a human-readable name for this I/O device.
    fn name(&self) -> &str;
}

// ===========================================================================
// Console — the global output sink
// ===========================================================================

static CONSOLE_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Platform console — routes output to the appropriate backend.
///
/// On Linux: writes to stdout.
/// On bare-metal: writes to serial port (COM1), later framebuffer.
pub struct Console;

impl Console {
    /// Initialize the console subsystem.
    pub fn init() {
        CONSOLE_INITIALIZED.store(true, Ordering::SeqCst);
    }

    /// Returns true if the console has been initialized.
    pub fn is_initialized() -> bool {
        CONSOLE_INITIALIZED.load(Ordering::SeqCst)
    }

    /// Write a string to the console.
    pub fn write_str(s: &str) {
        #[cfg(feature = "platform-linux")]
        {
            use std::io::Write;
            let _ = std::io::stdout().write_all(s.as_bytes());
        }
        #[cfg(feature = "platform-baremetal")]
        {
            // Would write to serial port via port I/O.
            // Stubbed for Phase 1.
            let _ = s;
        }
    }

    /// Write a formatted string to the console.
    pub fn write_fmt(args: std::fmt::Arguments<'_>) {
        use std::fmt::Write;
        let mut writer = ConsoleWriter;
        let _ = writer.write_fmt(args);
    }
}

/// Internal writer that implements `std::fmt::Write`.
struct ConsoleWriter;

impl std::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        Console::write_str(s);
        Ok(())
    }
}

// ===========================================================================
// Serial Port (bare-metal backend)
// ===========================================================================

/// 16550 UART serial port for bare-metal console output.
///
/// On Linux this is unused (we use stdout). On bare-metal this is the
/// primary early-boot console.
pub struct SerialPort {
    /// I/O port base address (e.g., 0x3F8 for COM1).
    base: u16,
    /// Whether the port has been initialized.
    initialized: AtomicBool,
}

impl SerialPort {
    /// Standard COM port base addresses.
    pub const COM1: u16 = 0x3F8;
    pub const COM2: u16 = 0x2F8;
    pub const COM3: u16 = 0x3E8;
    pub const COM4: u16 = 0x2E8;

    /// Create a new serial port handle.
    #[must_use]
    pub const fn new(base: u16) -> Self {
        Self {
            base,
            initialized: AtomicBool::new(false),
        }
    }

    /// Initialize the UART (set baud rate, line control, etc.).
    ///
    /// On bare-metal, this programs the actual hardware registers.
    /// On Linux, this is a no-op.
    pub fn init(&self) {
        self.initialized.store(true, Ordering::SeqCst);
        log::debug!("Serial port 0x{:X} initialized", self.base);
    }

    /// Returns the base I/O port address.
    pub const fn base(&self) -> u16 {
        self.base
    }

    /// Returns true if the port has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::SeqCst)
    }
}

impl PlatformIo for SerialPort {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(feature = "platform-linux")]
        {
            use std::io::Read;
            std::io::stdin().read(buf)
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            // Bare-metal: read from UART data register.
            let _ = buf;
            Ok(0)
        }
    }

    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(feature = "platform-linux")]
        {
            use std::io::Write;
            std::io::stdout().write(buf)
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            // Bare-metal: write to UART data register.
            let _ = buf;
            Ok(0)
        }
    }

    fn flush(&self) -> io::Result<()> {
        #[cfg(feature = "platform-linux")]
        {
            use std::io::Write;
            std::io::stdout().flush()
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            Ok(())
        }
    }

    fn name(&self) -> &'static str {
        "serial"
    }
}

// ===========================================================================
// Framebuffer Console (bare-metal backend, post-UEFI)
// ===========================================================================

/// Framebuffer-based text console for bare-metal display output.
///
/// Renders text to a linear framebuffer obtained from UEFI GOP.
/// Not used on Linux (we have a real terminal).
pub struct FramebufferConsole {
    /// Base address of the framebuffer.
    base: usize,
    /// Width in pixels.
    width: usize,
    /// Height in pixels.
    height: usize,
    /// Stride (bytes per row).
    stride: usize,
    /// Current cursor position (column, row) in character cells.
    cursor_col: usize,
    cursor_row: usize,
}

impl FramebufferConsole {
    /// Character cell dimensions (8x16 font).
    pub const CHAR_WIDTH: usize = 8;
    pub const CHAR_HEIGHT: usize = 16;

    /// Create a new framebuffer console.
    #[must_use]
    pub const fn new(base: usize, width: usize, height: usize, stride: usize) -> Self {
        Self {
            base,
            width,
            height,
            stride,
            cursor_col: 0,
            cursor_row: 0,
        }
    }

    /// Returns the number of character columns.
    #[must_use]
    pub const fn cols(&self) -> usize {
        self.width / Self::CHAR_WIDTH
    }

    /// Returns the number of character rows.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.height / Self::CHAR_HEIGHT
    }

    /// Returns the framebuffer base address.
    #[must_use]
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Returns the framebuffer dimensions.
    #[must_use]
    pub const fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}

impl PlatformIo for FramebufferConsole {
    fn read(&self, _buf: &mut [u8]) -> io::Result<usize> {
        // Framebuffer is output-only.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "framebuffer is output-only",
        ))
    }

    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        // On bare-metal: render characters to framebuffer.
        // Stubbed — real pixel rendering in Phase 6.
        // Each byte would be rendered as a glyph at the current cursor position
        // using stride, cursor_col, cursor_row to determine framebuffer offset.
        let _ = (self.stride, self.cursor_col, self.cursor_row, self.base);
        Ok(buf.len())
    }

    fn name(&self) -> &'static str {
        "framebuffer"
    }
}

// ===========================================================================
// Log integration
// ===========================================================================

/// Platform logger that integrates with the `log` crate.
///
/// Routes log messages through the Console subsystem.
pub struct PlatformLogger {
    level: log::LevelFilter,
}

impl PlatformLogger {
    /// Create a new platform logger with the given level filter.
    #[must_use]
    pub const fn new(level: log::LevelFilter) -> Self {
        Self { level }
    }

    /// Install this as the global logger.
    ///
    /// # Errors
    ///
    /// Returns a `log::SetLoggerError` if a logger has already been installed
    /// for this process. Only one logger can be active at a time.
    pub fn install(level: log::LevelFilter) -> Result<(), log::SetLoggerError> {
        static LOGGER: PlatformLogger = PlatformLogger::new(log::LevelFilter::Trace);
        log::set_logger(&LOGGER)?;
        log::set_max_level(level);
        Ok(())
    }
}

impl log::Log for PlatformLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            Console::write_fmt(format_args!(
                "[{:<5} {}] {}\n",
                record.level(),
                record.target(),
                record.args()
            ));
        }
    }

    fn flush(&self) {
        #[cfg(feature = "platform-linux")]
        {
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
}

// ===========================================================================
// Structured logging — per-subsystem tags
// ===========================================================================

/// Subsystem identifier for structured logging.
///
/// Used to tag log messages with their originating component,
/// e.g., `[vcpu:0]`, `[usb]`, `[fabric]`, `[memory]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subsystem {
    /// vCPU with index
    Vcpu(usize),
    /// Memory management
    Memory,
    /// USB subsystem
    Usb,
    /// Network
    Net,
    /// Storage / block devices
    Storage,
    /// Compute fabric
    Fabric,
    /// Management console
    Mgmt,
    /// Platform internals
    Platform,
    /// Custom subsystem
    Custom(String),
}

impl std::fmt::Display for Subsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vcpu(id) => write!(f, "vcpu:{id}"),
            Self::Memory => write!(f, "memory"),
            Self::Usb => write!(f, "usb"),
            Self::Net => write!(f, "net"),
            Self::Storage => write!(f, "storage"),
            Self::Fabric => write!(f, "fabric"),
            Self::Mgmt => write!(f, "mgmt"),
            Self::Platform => write!(f, "platform"),
            Self::Custom(s) => write!(f, "{s}"),
        }
    }
}

/// Log a message with a subsystem tag.
/// Usage: `subsystem_log!(Level::Info, Subsystem::Vcpu(0), "started execution");`
#[macro_export]
macro_rules! subsystem_log {
    ($level:expr, $subsystem:expr, $($arg:tt)*) => {
        log::log!($level, "[{}] {}", $subsystem, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! platform_info {
    ($subsystem:expr, $($arg:tt)*) => {
        $crate::subsystem_log!(log::Level::Info, $subsystem, $($arg)*)
    };
}

#[macro_export]
macro_rules! platform_debug {
    ($subsystem:expr, $($arg:tt)*) => {
        $crate::subsystem_log!(log::Level::Debug, $subsystem, $($arg)*)
    };
}

#[macro_export]
macro_rules! platform_warn {
    ($subsystem:expr, $($arg:tt)*) => {
        $crate::subsystem_log!(log::Level::Warn, $subsystem, $($arg)*)
    };
}

#[macro_export]
macro_rules! platform_error {
    ($subsystem:expr, $($arg:tt)*) => {
        $crate::subsystem_log!(log::Level::Error, $subsystem, $($arg)*)
    };
}

// ===========================================================================
// Macros
// ===========================================================================

/// Print to the platform console (no newline).
#[macro_export]
macro_rules! platform_print {
    ($($arg:tt)*) => {
        $crate::io::Console::write_fmt(format_args!($($arg)*))
    };
}

/// Print to the platform console (with newline).
#[macro_export]
macro_rules! platform_println {
    () => { $crate::platform_print!("\n") };
    ($($arg:tt)*) => {
        $crate::io::Console::write_fmt(format_args!("{}\n", format_args!($($arg)*)))
    };
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn console_init() {
        Console::init();
        assert!(Console::is_initialized());
    }

    #[test]
    fn console_write() {
        Console::init();
        Console::write_str("test output\n");
    }

    #[test]
    fn serial_port_creation() {
        let port = SerialPort::new(SerialPort::COM1);
        assert_eq!(port.base(), 0x3F8);
        assert!(!port.is_initialized());
        port.init();
        assert!(port.is_initialized());
    }

    #[test]
    fn serial_port_io_trait() {
        let port = SerialPort::new(SerialPort::COM1);
        port.init();
        assert_eq!(port.name(), "serial");

        let data = b"hello serial";
        let written = port.write(data).unwrap();
        assert!(written > 0);
    }

    #[test]
    fn framebuffer_console_creation() {
        let fb = FramebufferConsole::new(0xB800_0000, 1920, 1080, 7680);
        assert_eq!(fb.cols(), 1920 / 8);
        assert_eq!(fb.rows(), 1080 / 16);
        assert_eq!(fb.base(), 0xB800_0000);
        assert_eq!(fb.dimensions(), (1920, 1080));
    }

    #[test]
    fn framebuffer_io_trait() {
        let fb = FramebufferConsole::new(0xB800_0000, 1920, 1080, 7680);
        assert_eq!(fb.name(), "framebuffer");

        // Write should succeed (stubbed).
        let written = fb.write(b"hello fb").unwrap();
        assert_eq!(written, 8);

        // Read should fail (output-only).
        let mut buf = [0u8; 16];
        let result = fb.read(&mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn platform_logger_creation() {
        let logger = PlatformLogger::new(log::LevelFilter::Info);
        assert!(logger.enabled(&log::Metadata::builder().build()));
    }
}
