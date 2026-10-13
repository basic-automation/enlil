//! I/O bindings for the bare-metal kernel: serial console + in-memory pipes.

use crate::io::{IoErrorKind, IoResult};

/// Standard input / output / error streams.
///
/// The kernel has no stdin device; stdout/stderr write to the platform
/// console (serial port today, via [`crate::io::Console`]).
pub mod stdio {
    use crate::io::{Console, IoResult};
    use crate::sys::unsupported;

    /// Standard input — not present on bare metal.
    #[derive(Debug, Clone, Copy)]
    pub struct Stdin;

    /// Returns the (absent) standard input handle.
    #[must_use]
    pub const fn stdin() -> Stdin {
        Stdin
    }

    impl Stdin {
        /// Always fails: the kernel has no stdin device.
        ///
        /// # Errors
        ///
        /// Always returns [`IoErrorKind::Unsupported`].
        pub const fn read(&self, _buf: &mut [u8]) -> IoResult<usize> {
            unsupported("no stdin on bare-metal enlil")
        }
    }

    /// Standard output handle: the platform console.
    #[derive(Debug, Clone, Copy)]
    pub struct Stdout;

    /// Returns a handle to standard output.
    #[must_use]
    pub const fn stdout() -> Stdout {
        Stdout
    }

    impl Stdout {
        /// Writes `buf` to the platform console, returning the bytes written.
        ///
        /// # Errors
        ///
        /// Only fails if the console itself is unavailable; the current
        /// backend never fails.
        pub fn write(&self, buf: &[u8]) -> IoResult<usize> {
            let text = core::str::from_utf8(buf).unwrap_or("<non-utf8>");
            Console::write_str(text);
            Ok(buf.len())
        }

        /// No-op: the console is unbuffered.
        ///
        /// # Errors
        ///
        /// This backend never fails; the `IoResult` is for API parity.
        pub const fn flush(&self) -> IoResult<()> {
            Ok(())
        }
    }

    /// Standard error handle: the platform console.
    #[derive(Debug, Clone, Copy)]
    pub struct Stderr;

    /// Returns a handle to standard error.
    #[must_use]
    pub const fn stderr() -> Stderr {
        Stderr
    }

    impl Stderr {
        /// Writes `buf` to the platform console, returning the bytes written.
        ///
        /// # Errors
        ///
        /// Only fails if the console itself is unavailable; the current
        /// backend never fails.
        pub fn write(&self, buf: &[u8]) -> IoResult<usize> {
            let text = core::str::from_utf8(buf).unwrap_or("<non-utf8>");
            Console::write_str(text);
            Ok(buf.len())
        }

        /// No-op: the console is unbuffered.
        ///
        /// # Errors
        ///
        /// This backend never fails; the `IoResult` is for API parity.
        pub const fn flush(&self) -> IoResult<()> {
            Ok(())
        }
    }
}

/// Anonymous in-memory pipes, chunked over the platform MPSC channel.
///
/// Both halves block — cooperatively spinning, no syscalls — when the
/// pipe is empty/full, mirroring blocking OS pipe semantics. Bounded to
/// [`PIPE_CHUNKS`] chunks so kernel memory use stays predictable.
pub mod pipe {
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    use super::{IoErrorKind, IoResult};
    use crate::io::IoError;
    use crate::sync::{Receiver, Sender, channel};

    /// Number of buffered chunks; bounds kernel memory use.
    const PIPE_CHUNKS: usize = 16;

    /// Read half of an in-memory pipe.
    pub struct PipeReader {
        rx: Receiver<Vec<u8>>,
        pending: VecDeque<u8>,
    }

    /// Write half of an in-memory pipe.
    pub struct PipeWriter {
        tx: Sender<Vec<u8>>,
    }

    /// Creates an in-memory pipe, returning `(reader, writer)`.
    ///
    /// # Errors
    ///
    /// This backend never fails; the `IoResult` is for API parity with Linux.
    pub fn pipe() -> IoResult<(PipeReader, PipeWriter)> {
        let (tx, rx) = channel(PIPE_CHUNKS);
        Ok((
            PipeReader {
                rx,
                pending: VecDeque::new(),
            },
            PipeWriter { tx },
        ))
    }

    impl PipeReader {
        /// Reads up to `buf.len()` bytes; `Ok(0)` means the writer was
        /// dropped and the pipe drained.
        ///
        /// Blocks (cooperatively) while the pipe is empty.
        ///
        /// # Errors
        ///
        /// Returns an error if the internal channel reports an unexpected
        /// state.
        pub fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            if self.pending.is_empty() {
                match self.rx.recv() {
                    Ok(chunk) => self.pending.extend(chunk),
                    Err(_) => return Ok(0),
                }
            }
            let n = self.pending.len().min(buf.len());
            for (slot, byte) in buf[..n].iter_mut().zip(self.pending.drain(..n)) {
                *slot = byte;
            }
            Ok(n)
        }
    }

    impl PipeWriter {
        /// Writes all of `buf`, blocking (cooperatively) while the pipe is
        /// full. Returns the bytes written.
        ///
        /// # Errors
        ///
        /// Returns an error if the reader was dropped.
        pub fn write(&self, buf: &[u8]) -> IoResult<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            self.tx
                .send(buf.to_vec())
                .map_err(|_| IoError::with_message(IoErrorKind::Other, "pipe reader dropped"))?;
            Ok(buf.len())
        }
    }
}
