//! Filesystem bindings — mirrors `std::sys::fs`'s role.

use crate::io::{IoError, IoResult};
/// Re-exported so `sys::fs::SeekFrom` names the same type on every backend.
pub use crate::sys::SeekFrom;

const fn map_seek(pos: SeekFrom) -> std::io::SeekFrom {
    match pos {
        SeekFrom::Start(n) => std::io::SeekFrom::Start(n),
        SeekFrom::End(n) => std::io::SeekFrom::End(n),
        SeekFrom::Current(n) => std::io::SeekFrom::Current(n),
    }
}

/// File open options, mirroring `std::fs::OpenOptions`.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    inner: std::fs::OpenOptions,
}

impl OpenOptions {
    /// Empty option set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: std::fs::OpenOptions::new(),
        }
    }

    /// Enable/disable read access.
    #[must_use]
    pub fn read(mut self, read: bool) -> Self {
        self.inner.read(read);
        self
    }

    /// Enable/disable write access.
    #[must_use]
    pub fn write(mut self, write: bool) -> Self {
        self.inner.write(write);
        self
    }

    /// Enable/disable append mode.
    #[must_use]
    pub fn append(mut self, append: bool) -> Self {
        self.inner.append(append);
        self
    }

    /// Truncate a pre-existing file on open.
    #[must_use]
    pub fn truncate(mut self, truncate: bool) -> Self {
        self.inner.truncate(truncate);
        self
    }

    /// Create the file if it does not exist.
    #[must_use]
    pub fn create(mut self, create: bool) -> Self {
        self.inner.create(create);
        self
    }

    /// Fail if the file already exists (`create` must also be set).
    #[must_use]
    pub fn create_new(mut self, create_new: bool) -> Self {
        self.inner.create_new(create_new);
        self
    }

    /// Opens the file at `path` with these options.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened.
    pub fn open(&self, path: &str) -> IoResult<File> {
        self.inner
            .open(path)
            .map(|inner| File { inner })
            .map_err(IoError::from)
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// An open file.
#[derive(Debug)]
pub struct File {
    inner: std::fs::File,
}

impl File {
    /// Opens the file at `path` for reading.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened.
    pub fn open(path: &str) -> IoResult<Self> {
        std::fs::File::open(path)
            .map(|inner| Self { inner })
            .map_err(IoError::from)
    }

    /// Creates (or truncates) the file at `path` for writing.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created.
    pub fn create(path: &str) -> IoResult<Self> {
        std::fs::File::create(path)
            .map(|inner| Self { inner })
            .map_err(IoError::from)
    }

    /// Reads bytes into `buf`, returning the number read.
    ///
    /// # Errors
    ///
    /// Returns an error if the read fails.
    pub fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        use std::io::Read;
        self.inner.read(buf).map_err(IoError::from)
    }

    /// Writes `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        use std::io::Write;
        self.inner.write(buf).map_err(IoError::from)
    }

    /// Flushes buffered output.
    ///
    /// # Errors
    ///
    /// Returns an error if the flush fails.
    pub fn flush(&mut self) -> IoResult<()> {
        use std::io::Write;
        self.inner.flush().map_err(IoError::from)
    }

    /// Seeks to `pos`, returning the new absolute position.
    ///
    /// # Errors
    ///
    /// Returns an error if the seek fails.
    pub fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        use std::io::Seek;
        self.inner.seek(map_seek(pos)).map_err(IoError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> String {
        std::env::temp_dir()
            .join(name)
            .to_str()
            .expect("temp dir is not UTF-8")
            .to_owned()
    }

    #[test]
    fn file_roundtrip() {
        let path = tmp_path("enlil-sys-fs-test.txt");
        {
            let mut f = File::create(&path).unwrap();
            assert_eq!(f.write(b"data").unwrap(), 4);
            f.flush().unwrap();
        }
        {
            let mut f = File::open(&path).unwrap();
            let mut buf = [0u8; 8];
            assert_eq!(f.read(&mut buf).unwrap(), 4);
            assert_eq!(&buf[..4], b"data");
            assert_eq!(f.seek(SeekFrom::Start(0)).unwrap(), 0);
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_missing_fails() {
        let path = tmp_path("enlil-sys-fs-definitely-missing.txt");
        let _ = std::fs::remove_file(&path);
        assert!(File::open(&path).is_err());
    }

    #[test]
    fn open_options_builder() {
        let path = tmp_path("enlil-sys-fs-opts-test.txt");
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        assert_eq!(f.write(b"xy").unwrap(), 2);
        drop(f);
        std::fs::remove_file(&path).unwrap();
    }
}
