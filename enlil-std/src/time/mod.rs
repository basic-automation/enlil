//! Time — backed by `enlil_platform::sys::time`.

pub use core::time::Duration;
pub use enlil_platform::sys::time::{Instant, sleep};
pub use enlil_platform::time::Stopwatch;
