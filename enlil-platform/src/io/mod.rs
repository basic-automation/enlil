//! I/O Trait Layer — Platform-abstracted I/O primitives
//!
//! Provides a trait-based I/O registry so that `print!`, `log::info!()`, etc.
//! work from day one, regardless of whether we're on Linux or bare-metal.
//!
//! # Backends
//!
//! - **Linux:** Delegates to `std::io` (stdout/stderr).
//! - **Bare-metal:** Serial port (COM1) for early boot, framebuffer console later.

use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "platform-baremetal")]
use alloc::string::String;

// ===========================================================================
// IoError — a small no_std I/O error type
// ===========================================================================

/// The kind of an [`IoError`].
///
/// A deliberately small subset of [`std::io::ErrorKind`]: just the variants
/// the platform layer's I/O backends can actually produce. `Copy` so it stays
/// cheap to pass around in `no_std` contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IoErrorKind {
    /// Entity not found (device, resource).
    NotFound,
    /// Permission denied.
    PermissionDenied,
    /// Operation interrupted; it may be retried.
    Interrupted,
    /// Invalid input parameter.
    InvalidInput,
    /// Invalid data encountered.
    InvalidData,
    /// Write of length zero.
    WriteZero,
    /// Operation not supported by this device.
    Unsupported,
    /// Any other error.
    Other,
}

/// I/O error type that works in `no_std` environments.
///
/// This is the error half of every [`PlatformIo`] method's result, replacing
/// `std::io::Error` so the whole `io` module cross-compiles for the
/// bare-metal target. On Linux the backends convert from `std::io::Error` via
/// the [`From`] impl, keeping behavior identical to the old `std::io::Result`
/// signatures (the success paths are untouched; only the error is re-wrapped,
/// keeping its kind and dropping any heap-allocated message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoError {
    kind: IoErrorKind,
    message: Option<&'static str>,
}

impl IoError {
    /// Create an error of the given kind, with no message.
    #[must_use]
    pub const fn new(kind: IoErrorKind) -> Self {
        Self {
            kind,
            message: None,
        }
    }

    /// Create an error of the given kind with a static message.
    #[must_use]
    pub const fn with_message(kind: IoErrorKind, message: &'static str) -> Self {
        Self {
            kind,
            message: Some(message),
        }
    }

    /// The kind of this error.
    #[must_use]
    pub const fn kind(&self) -> IoErrorKind {
        self.kind
    }

    /// The static message attached to this error, if any.
    #[must_use]
    pub const fn message(&self) -> Option<&'static str> {
        self.message
    }
}

impl core::fmt::Display for IoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.message {
            Some(msg) => write!(f, "{:?}: {msg}", self.kind),
            None => write!(f, "{:?}", self.kind),
        }
    }
}

impl core::error::Error for IoError {}

/// Convenience alias for the results returned by [`PlatformIo`] methods.
pub type IoResult<T> = Result<T, IoError>;

/// Convert a `std::io::Error` into the platform error, preserving its kind.
///
/// Only the kind survives the conversion: the original message may own heap
/// memory, which has no `no_std` representation here.
#[cfg(feature = "platform-linux")]
impl From<std::io::Error> for IoError {
    fn from(err: std::io::Error) -> Self {
        let kind = match err.kind() {
            std::io::ErrorKind::NotFound => IoErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => IoErrorKind::PermissionDenied,
            std::io::ErrorKind::Interrupted => IoErrorKind::Interrupted,
            std::io::ErrorKind::InvalidInput => IoErrorKind::InvalidInput,
            std::io::ErrorKind::InvalidData => IoErrorKind::InvalidData,
            std::io::ErrorKind::WriteZero => IoErrorKind::WriteZero,
            std::io::ErrorKind::Unsupported => IoErrorKind::Unsupported,
            _ => IoErrorKind::Other,
        };
        Self::new(kind)
    }
}

/// Convert a platform error back into a `std::io::Error` (Linux only).
#[cfg(feature = "platform-linux")]
impl From<IoError> for std::io::Error {
    fn from(err: IoError) -> Self {
        let kind = match err.kind() {
            IoErrorKind::NotFound => std::io::ErrorKind::NotFound,
            IoErrorKind::PermissionDenied => std::io::ErrorKind::PermissionDenied,
            IoErrorKind::Interrupted => std::io::ErrorKind::Interrupted,
            IoErrorKind::InvalidInput => std::io::ErrorKind::InvalidInput,
            IoErrorKind::InvalidData => std::io::ErrorKind::InvalidData,
            IoErrorKind::WriteZero => std::io::ErrorKind::WriteZero,
            IoErrorKind::Unsupported => std::io::ErrorKind::Unsupported,
            IoErrorKind::Other => std::io::ErrorKind::Other,
        };
        err.message()
            .map_or_else(|| Self::from(kind), |msg| Self::new(kind, msg))
    }
}

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
    fn read(&self, buf: &mut [u8]) -> IoResult<usize>;

    /// Write bytes to this I/O device.
    ///
    /// # Errors
    ///
    /// Returns an error if the write operation fails, such as when the device
    /// is unavailable, the write is interrupted, or the device is full.
    fn write(&self, buf: &[u8]) -> IoResult<usize>;

    /// Flush any buffered output.
    ///
    /// # Errors
    ///
    /// Returns an error if the flush operation fails, such as when the device
    /// is unavailable or the underlying I/O system encounters an error.
    fn flush(&self) -> IoResult<()> {
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
    pub fn write_fmt(args: core::fmt::Arguments<'_>) {
        use core::fmt::Write;
        let mut writer = ConsoleWriter;
        let _ = writer.write_fmt(args);
    }
}

/// Internal writer that implements `core::fmt::Write`.
struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
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
    fn read(&self, buf: &mut [u8]) -> IoResult<usize> {
        #[cfg(feature = "platform-linux")]
        {
            use std::io::Read;
            std::io::stdin().read(buf).map_err(IoError::from)
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            // Bare-metal: read from UART data register.
            let _ = buf;
            Ok(0)
        }
    }

    fn write(&self, buf: &[u8]) -> IoResult<usize> {
        #[cfg(feature = "platform-linux")]
        {
            use std::io::Write;
            std::io::stdout().write(buf).map_err(IoError::from)
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            // Bare-metal: write to UART data register.
            let _ = buf;
            Ok(0)
        }
    }

    fn flush(&self) -> IoResult<()> {
        #[cfg(feature = "platform-linux")]
        {
            use std::io::Write;
            std::io::stdout().flush().map_err(IoError::from)
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
    fn read(&self, _buf: &mut [u8]) -> IoResult<usize> {
        // Framebuffer is output-only.
        Err(IoError::with_message(
            IoErrorKind::Unsupported,
            "framebuffer is output-only",
        ))
    }

    fn write(&self, buf: &[u8]) -> IoResult<usize> {
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

impl core::fmt::Display for Subsystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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
        assert_eq!(result.unwrap_err().kind(), IoErrorKind::Unsupported);
    }

    #[test]
    fn platform_logger_creation() {
        let logger = PlatformLogger::new(log::LevelFilter::Info);
        assert!(logger.enabled(&log::Metadata::builder().build()));
    }

    #[test]
    fn io_error_kind_and_message() {
        let err = IoError::new(IoErrorKind::NotFound);
        assert_eq!(err.kind(), IoErrorKind::NotFound);
        assert_eq!(err.message(), None);
        assert_eq!(format!("{err}"), "NotFound");

        let err = IoError::with_message(IoErrorKind::Unsupported, "framebuffer is output-only");
        assert_eq!(err.kind(), IoErrorKind::Unsupported);
        assert_eq!(err.message(), Some("framebuffer is output-only"));
        assert_eq!(format!("{err}"), "Unsupported: framebuffer is output-only");

        // Copy semantics: cheap to pass around in no_std code.
        let copied = err;
        assert_eq!(copied, err);
    }

    #[test]
    fn io_result_alias() {
        let ok: IoResult<usize> = Ok(42);
        assert!(matches!(ok, Ok(42)));
        let err: IoResult<usize> = Err(IoError::new(IoErrorKind::Interrupted));
        assert!(matches!(err, Err(e) if e.kind() == IoErrorKind::Interrupted));
    }

    #[cfg(feature = "platform-linux")]
    #[test]
    fn io_error_std_round_trip() {
        // std -> IoError preserves the kind.
        let std_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = IoError::from(std_err);
        assert_eq!(err.kind(), IoErrorKind::PermissionDenied);

        // Unknown std kinds collapse to Other.
        let std_err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
        assert_eq!(IoError::from(std_err).kind(), IoErrorKind::Other);

        // IoError -> std preserves kind and message.
        let back = std::io::Error::from(IoError::with_message(IoErrorKind::WriteZero, "zero"));
        assert_eq!(back.kind(), std::io::ErrorKind::WriteZero);
        assert!(format!("{back}").contains("zero"));
    }
}
