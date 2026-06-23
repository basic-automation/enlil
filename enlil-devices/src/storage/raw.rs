//! Raw file storage backend.
//!
//! Provides direct file I/O for raw disk images (.img, .raw).

use super::StorageBackend;
use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

/// A storage backend backed by a raw file on disk.
pub struct RawFileBackend {
    file: Mutex<File>,
    capacity: u64,
    readonly: bool,
}

impl RawFileBackend {
    /// Open a raw disk image file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file operation fails.
    pub fn open(path: &str, readonly: bool) -> Result<Self> {
        let file = if readonly {
            File::open(path)?
        } else {
            OpenOptions::new().read(true).write(true).open(path)?
        };
        let capacity = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            capacity,
            readonly,
        })
    }

    /// Create a new raw disk image of the given size.
    ///
    /// # Errors
    ///
    /// Returns an error if the file operation fails.
    pub fn create(path: &str, size_bytes: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(size_bytes)?;
        Ok(Self {
            file: Mutex::new(file),
            capacity: size_bytes,
            readonly: false,
        })
    }
}

impl StorageBackend for RawFileBackend {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut file = self.file.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        file.seek(SeekFrom::Start(offset))?;
        // A single `read` may return fewer bytes than requested, which would
        // leave part of a guest sector holding stale buffer data. Loop until the
        // buffer is full or we hit EOF, then zero-fill the remainder: a read
        // inside a fixed-capacity (possibly sparse) image returns zeros for any
        // not-yet-written region, exactly like a real disk.
        let mut total = 0;
        while total < buf.len() {
            match file.read(&mut buf[total..]) {
                Ok(0) => break, // EOF (sparse/short image): the rest reads as 0.
                Ok(n) => total += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        buf[total..].fill(0);
        drop(file);
        Ok(buf.len())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        if self.readonly {
            anyhow::bail!("backend is read-only");
        }
        let mut file = self.file.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        file.seek(SeekFrom::Start(offset))?;
        // `write_all` loops over short writes so a whole sector is never left
        // partially written.
        file.write_all(buf)?;
        drop(file);
        Ok(buf.len())
    }

    fn flush(&self) -> Result<()> {
        let file = self.file.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        file.sync_all()?;
        drop(file);
        Ok(())
    }

    fn trim(&self, _offset: u64, _len: u64) -> Result<()> {
        // Raw files don't support trim/punch-hole portably
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn is_readonly(&self) -> bool {
        self.readonly
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    #[test]
    fn test_create_and_rw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.raw");
        let path_str = path.to_str().unwrap();

        let backend = RawFileBackend::create(path_str, 4096).unwrap();
        assert_eq!(backend.capacity(), 4096);
        assert!(!backend.is_readonly());

        // Write
        let data = b"Hello, raw backend!";
        let written = backend.write_at(0, data).unwrap();
        assert_eq!(written, data.len());
        backend.flush().unwrap();

        // Read back
        let mut buf = vec![0u8; data.len()];
        let read = backend.read_at(0, &mut buf).unwrap();
        assert_eq!(read, data.len());
        assert_eq!(&buf, data);
    }

    #[test]
    fn read_past_written_data_returns_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.raw");
        let backend = RawFileBackend::create(path.to_str().unwrap(), 4096).unwrap();
        // Write a marker near the start; the rest of the image is a sparse hole.
        backend.write_at(0, b"abcd").unwrap();

        // A read of a never-written, in-capacity region returns all zeros and
        // fills the whole buffer (not a short read of stale data).
        let mut buf = [0xFFu8; 64];
        assert_eq!(backend.read_at(2000, &mut buf).unwrap(), 64);
        assert!(
            buf.iter().all(|&b| b == 0),
            "unwritten region reads as zeros"
        );

        // A read spanning the end of the image is zero-filled past EOF.
        let mut tail = [0xFFu8; 256];
        assert_eq!(backend.read_at(4000, &mut tail).unwrap(), 256); // 4000+256 > 4096
        assert!(tail.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_open_readonly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.raw");

        // Create the file first
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(&vec![0xAA; 512]).unwrap();
        }

        let backend = RawFileBackend::open(path.to_str().unwrap(), true).unwrap();
        assert!(backend.is_readonly());
        assert_eq!(backend.capacity(), 512);

        // Read should work
        let mut buf = vec![0u8; 512];
        backend.read_at(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));

        // Write should fail
        assert!(backend.write_at(0, &[0]).is_err());
    }
}
