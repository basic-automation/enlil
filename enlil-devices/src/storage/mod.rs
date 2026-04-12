//! Storage backend abstractions.
//!
//! Defines the `StorageBackend` trait and concrete implementations:
//! - `RawFileBackend` — raw disk image files
//! - `QcowBackend` — qcow2 disk images (read-only)

pub mod qcow;
pub mod raw;

pub use qcow::QcowBackend;
pub use raw::RawFileBackend;

use anyhow::Result;

/// Abstract storage backend for block devices.
///
/// All operations are synchronous (we use blocking file I/O wrapped in
/// `spawn_blocking` at the caller level if needed). This keeps the trait
/// object-safe and avoids async-trait overhead in the hot path.
pub trait StorageBackend: Send + Sync {
    /// Read bytes from the backend at the given byte offset.
    ///
    /// Returns the number of bytes actually read.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation fails.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Write bytes to the backend at the given byte offset.
    ///
    /// Returns the number of bytes actually written.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation fails.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize>;

    /// Flush any buffered writes to stable storage.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation fails.
    fn flush(&self) -> Result<()>;

    /// Inform the backend that the given range is no longer needed (trim/discard).
    ///
    /// Backends that don't support trim can return `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation fails.
    fn trim(&self, offset: u64, len: u64) -> Result<()> {
        let _ = (offset, len);
        Ok(())
    }

    /// Total capacity of the storage in bytes.
    fn capacity(&self) -> u64;

    /// Whether this backend is read-only.
    fn is_readonly(&self) -> bool;
}

/// A fixed-size in-memory storage backend, useful for testing.
pub struct MemoryBackend {
    data: std::sync::RwLock<Vec<u8>>,
    readonly: bool,
}

impl MemoryBackend {
    /// Create a new memory backend with the given size, initialized to zero.
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self {
            data: std::sync::RwLock::new(vec![0u8; size]),
            readonly: false,
        }
    }

    /// Create a memory backend from existing data.
    #[must_use]
    pub const fn from_data(data: Vec<u8>) -> Self {
        Self {
            data: std::sync::RwLock::new(data),
            readonly: false,
        }
    }

    /// Create a read-only memory backend.
    #[must_use]
    pub const fn new_readonly(data: Vec<u8>) -> Self {
        Self {
            data: std::sync::RwLock::new(data),
            readonly: true,
        }
    }
}

impl StorageBackend for MemoryBackend {
    #[allow(clippy::cast_possible_truncation)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let data = self.data.read().map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let offset = offset as usize;
        if offset >= data.len() {
            return Ok(0);
        }
        let available = data.len() - offset;
        let to_read = buf.len().min(available);
        buf[..to_read].copy_from_slice(&data[offset..offset + to_read]);
        drop(data);
        Ok(to_read)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        if self.readonly {
            anyhow::bail!("backend is read-only");
        }
        let mut data = self.data.write().map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let offset = offset as usize;
        if offset >= data.len() {
            return Ok(0);
        }
        let available = data.len() - offset;
        let to_write = buf.len().min(available);
        data[offset..offset + to_write].copy_from_slice(&buf[..to_write]);
        drop(data);
        Ok(to_write)
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    #[allow(clippy::cast_possible_truncation)]
    fn trim(&self, offset: u64, len: u64) -> Result<()> {
        if self.readonly {
            anyhow::bail!("backend is read-only");
        }
        let mut data = self.data.write().map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let start = (offset as usize).min(data.len());
        let end = ((offset + len) as usize).min(data.len());
        data[start..end].fill(0);
        drop(data);
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.data.read().map_or(0, |d| d.len() as u64)
    }

    fn is_readonly(&self) -> bool {
        self.readonly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_read_write() {
        let backend = MemoryBackend::new(1024);
        let write_data = b"Hello, enlil!";
        let written = backend.write_at(0, write_data).unwrap();
        assert_eq!(written, write_data.len());

        let mut buf = vec![0u8; write_data.len()];
        let read = backend.read_at(0, &mut buf).unwrap();
        assert_eq!(read, write_data.len());
        assert_eq!(&buf, write_data);
    }

    #[test]
    fn memory_backend_read_past_end() {
        let backend = MemoryBackend::new(16);
        let mut buf = vec![0u8; 32];
        let read = backend.read_at(8, &mut buf).unwrap();
        assert_eq!(read, 8);
    }

    #[test]
    fn memory_backend_write_past_end() {
        let backend = MemoryBackend::new(16);
        let data = vec![0xFFu8; 32];
        let written = backend.write_at(8, &data).unwrap();
        assert_eq!(written, 8);
    }

    #[test]
    fn memory_backend_readonly() {
        let backend = MemoryBackend::new_readonly(vec![0u8; 64]);
        assert!(backend.is_readonly());
        assert!(backend.write_at(0, &[1, 2, 3]).is_err());
    }

    #[test]
    fn memory_backend_trim() {
        let backend = MemoryBackend::from_data(vec![0xFFu8; 64]);
        backend.trim(16, 16).unwrap();
        let mut buf = vec![0u8; 16];
        backend.read_at(16, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        // Data outside trim range should be untouched
        backend.read_at(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn memory_backend_capacity() {
        let backend = MemoryBackend::new(4096);
        assert_eq!(backend.capacity(), 4096);
    }
}
