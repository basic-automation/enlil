//! I/O bindings: standard streams and pipes — mirrors `std::sys::io`'s role.

use crate::io::{IoError, IoErrorKind, IoResult};

/// Standard input / output / error streams.
pub mod stdio {
    use super::{IoError, IoResult};

    /// Standard input handle.
    #[derive(Debug)]
    pub struct Stdin {
        inner: std::io::Stdin,
    }

    /// Returns a handle to standard input.
    #[must_use]
    pub fn stdin() -> Stdin {
        Stdin {
            inner: std::io::stdin(),
        }
    }

    impl Stdin {
        /// Reads bytes into `buf`, returning the number read.
        ///
        /// # Errors
        ///
        /// Returns an error if the read fails.
        pub fn read(&self, buf: &mut [u8]) -> IoResult<usize> {
            use std::io::Read;
            self.inner.lock().read(buf).map_err(IoError::from)
        }
    }

    /// Standard output handle.
    #[derive(Debug)]
    pub struct Stdout {
        inner: std::io::Stdout,
    }

    /// Returns a handle to standard output.
    #[must_use]
    pub fn stdout() -> Stdout {
        Stdout {
            inner: std::io::stdout(),
        }
    }

    impl Stdout {
        /// Writes `buf`, returning the number of bytes written.
        ///
        /// # Errors
        ///
        /// Returns an error if the write fails.
        pub fn write(&self, buf: &[u8]) -> IoResult<usize> {
            use std::io::Write;
            self.inner.lock().write(buf).map_err(IoError::from)
        }

        /// Flushes buffered output.
        ///
        /// # Errors
        ///
        /// Returns an error if the flush fails.
        pub fn flush(&self) -> IoResult<()> {
            use std::io::Write;
            self.inner.lock().flush().map_err(IoError::from)
        }
    }

    /// Standard error handle.
    #[derive(Debug)]
    pub struct Stderr {
        inner: std::io::Stderr,
    }

    /// Returns a handle to standard error.
    #[must_use]
    pub fn stderr() -> Stderr {
        Stderr {
            inner: std::io::stderr(),
        }
    }

    impl Stderr {
        /// Writes `buf`, returning the number of bytes written.
        ///
        /// # Errors
        ///
        /// Returns an error if the write fails.
        pub fn write(&self, buf: &[u8]) -> IoResult<usize> {
            use std::io::Write;
            self.inner.lock().write(buf).map_err(IoError::from)
        }

        /// Flushes buffered output.
        ///
        /// # Errors
        ///
        /// Returns an error if the flush fails.
        pub fn flush(&self) -> IoResult<()> {
            use std::io::Write;
            self.inner.lock().flush().map_err(IoError::from)
        }
    }
}

/// Anonymous pipes, backed by the OS `pipe(2)`.
pub mod pipe {
    use super::{IoError, IoErrorKind, IoResult};

    /// Read half of an anonymous pipe. Closes its fd on drop.
    #[derive(Debug)]
    pub struct PipeReader {
        fd: libc::c_int,
    }

    /// Write half of an anonymous pipe. Closes its fd on drop.
    #[derive(Debug)]
    pub struct PipeWriter {
        fd: libc::c_int,
    }

    /// Creates an anonymous pipe, returning `(reader, writer)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS refuses to create the pipe.
    pub fn pipe() -> IoResult<(PipeReader, PipeWriter)> {
        let mut fds: [libc::c_int; 2] = [0; 2];
        // SAFETY: `pipe` writes exactly two fds into the array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(IoError::from(std::io::Error::last_os_error()));
        }
        Ok((PipeReader { fd: fds[0] }, PipeWriter { fd: fds[1] }))
    }

    impl PipeReader {
        /// Reads up to `buf.len()` bytes; `Ok(0)` means the writer was dropped.
        ///
        /// # Errors
        ///
        /// Returns an error if the read fails.
        pub fn read(&self, buf: &mut [u8]) -> IoResult<usize> {
            loop {
                // SAFETY: `read` fills at most `buf.len()` bytes of `buf`.
                let n = unsafe {
                    libc::read(self.fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len())
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(IoError::from(err));
                }
                return usize::try_from(n).map_err(|_| IoError::new(IoErrorKind::Other));
            }
        }
    }

    impl Drop for PipeReader {
        fn drop(&mut self) {
            // SAFETY: fd came from `pipe()` and is closed exactly once here.
            unsafe {
                libc::close(self.fd);
            }
        }
    }

    impl PipeWriter {
        /// Writes all of `buf`, returning the bytes written.
        ///
        /// # Errors
        ///
        /// Returns an error if the write fails (e.g. the reader was dropped).
        pub fn write(&self, buf: &[u8]) -> IoResult<usize> {
            let mut written = 0;
            while written < buf.len() {
                // SAFETY: `write` reads at most the remaining bytes of `buf`.
                let n = unsafe {
                    libc::write(
                        self.fd,
                        buf[written..].as_ptr().cast::<libc::c_void>(),
                        buf.len() - written,
                    )
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(IoError::from(err));
                }
                written += usize::try_from(n).map_err(|_| IoError::new(IoErrorKind::Other))?;
            }
            Ok(written)
        }
    }

    impl Drop for PipeWriter {
        fn drop(&mut self) {
            // SAFETY: fd came from `pipe()` and is closed exactly once here.
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{pipe, stdio};

    #[test]
    fn pipe_roundtrip() {
        let (r, w) = pipe::pipe().unwrap();
        assert_eq!(w.write(b"hello").unwrap(), 5);
        let mut buf = [0u8; 8];
        assert_eq!(r.read(&mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn pipe_eof_when_writer_dropped() {
        let (r, w) = pipe::pipe().unwrap();
        drop(w);
        let mut buf = [0u8; 8];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn stdio_writes_succeed() {
        assert_eq!(stdio::stdout().write(b"").unwrap(), 0);
        stdio::stdout().flush().unwrap();
        stdio::stderr().flush().unwrap();
    }
}
