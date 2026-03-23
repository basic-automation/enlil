//! I/O — backed by `enlil_platform::io`.

pub use enlil_platform::io::{Console, PlatformIo, SerialPort, Subsystem};

/// Print to the platform console (no newline).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::io::Console::write_fmt(format_args!($($arg)*))
    };
}

/// Print to the platform console (with newline).
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {
        $crate::io::Console::write_fmt(format_args!("{}\n", format_args!($($arg)*)))
    };
}
