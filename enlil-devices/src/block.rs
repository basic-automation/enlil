//! `VirtIO` Block Device emulation.
//!
//! Implements the `VirtIO` block device specification (virtio-blk).
//! Uses the [`StorageBackend`] trait for pluggable storage backends.

use crate::truncate::usize_of;
use crate::storage::StorageBackend;
use std::sync::Arc;

// VirtIO block request types
pub const VIRTIO_BLK_T_IN: u32 = 0; // Read
pub const VIRTIO_BLK_T_OUT: u32 = 1; // Write
pub const VIRTIO_BLK_T_FLUSH: u32 = 4; // Flush
pub const VIRTIO_BLK_T_GET_ID: u32 = 8; // Get device ID
pub const VIRTIO_BLK_T_DISCARD: u32 = 11; // Discard/trim

// VirtIO block status codes
pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// VirtIO block feature bits
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct BlockFeatures: u64 {
        const SIZE_MAX    = 1 << 1;
        const SEG_MAX     = 1 << 2;
        const GEOMETRY    = 1 << 4;
        const RO          = 1 << 5;
        const BLK_SIZE    = 1 << 6;
        const FLUSH       = 1 << 9;
        const TOPOLOGY    = 1 << 10;
        const CONFIG_WCE  = 1 << 11;
        const DISCARD     = 1 << 13;
        // VirtIO generic feature bits
        const RING_INDIRECT_DESC = 1 << 28;
        const RING_EVENT_IDX     = 1 << 29;
        const VERSION_1          = 1 << 32;
    }
}

/// `VirtIO` block device configuration space.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct BlockConfig {
    /// Capacity in 512-byte sectors.
    pub capacity: u64,
    /// Maximum size of any single segment (if `SIZE_MAX`).
    pub size_max: u32,
    /// Maximum number of segments in a request (if `SEG_MAX`).
    pub seg_max: u32,
    /// Geometry (if `GEOMETRY`).
    pub cylinders: u16,
    pub heads: u8,
    pub sectors: u8,
    /// Block size (if `BLK_SIZE`).
    pub blk_size: u32,
}

/// A `VirtIO` block request header (from guest memory).
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct BlockRequestHeader {
    pub request_type: u32,
    pub reserved: u32,
    pub sector: u64,
}

impl BlockRequestHeader {
    /// Parse a block request header from bytes.
    ///
    /// # Errors
    ///
    /// Returns `None` if the input is less than 16 bytes.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 16 {
            return None;
        }
        Some(Self {
            request_type: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            reserved: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            sector: u64::from_le_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ]),
        })
    }
}

/// `VirtIO` block device.
pub struct VirtioBlockDevice {
    /// The storage backend.
    backend: Arc<dyn StorageBackend>,
    /// Device configuration.
    config: BlockConfig,
    /// Negotiated features.
    features: BlockFeatures,
    /// Device ID string (up to 20 bytes).
    device_id: [u8; 20],
    /// Statistics.
    stats: BlockStats,
}

/// Block device I/O statistics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BlockStats {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub discards: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub errors: u64,
}

impl VirtioBlockDevice {
    /// Create a new `VirtIO` block device with the given storage backend.
    pub fn new(backend: Arc<dyn StorageBackend>, device_id: &str) -> Self {
        let capacity = backend.capacity();
        let readonly = backend.is_readonly();

        let mut features = BlockFeatures::VERSION_1
            | BlockFeatures::SIZE_MAX
            | BlockFeatures::SEG_MAX
            | BlockFeatures::BLK_SIZE
            | BlockFeatures::FLUSH;

        if readonly {
            features |= BlockFeatures::RO;
        }

        features |= BlockFeatures::DISCARD;

        let config = BlockConfig {
            capacity: capacity / 512,
            size_max: 1_048_576, // 1MB max segment
            seg_max: 128,
            cylinders: 0,
            heads: 0,
            sectors: 0,
            blk_size: 512,
        };

        let mut id_bytes = [0u8; 20];
        let id = device_id.as_bytes();
        let len = id.len().min(20);
        id_bytes[..len].copy_from_slice(&id[..len]);

        Self {
            backend,
            config,
            features,
            device_id: id_bytes,
            stats: BlockStats::default(),
        }
    }

    /// Get the device's offered features.
    #[must_use]
    pub const fn features(&self) -> BlockFeatures {
        self.features
    }

    /// Get the device configuration.
    #[must_use]
    pub const fn config(&self) -> &BlockConfig {
        &self.config
    }

    /// Get I/O statistics.
    #[must_use]
    pub const fn stats(&self) -> &BlockStats {
        &self.stats
    }

    /// Read the config space at the given offset.
    ///
    /// # Arguments
    ///
    /// * `offset` - Byte offset within the config space
    /// * `size` - Number of bytes to read (1, 2, 4, or 8)
    ///
    /// # Returns
    ///
    /// The requested value as a `u64`, or 0 if out of bounds.
    #[must_use]
    pub fn read_config(&self, offset: u64, size: u8) -> u64 {
        let config_bytes = self.config_as_bytes();
        let offset = usize_of(offset);
        if offset >= config_bytes.len() {
            return 0;
        }
        match size {
            1 => u64::from(config_bytes.get(offset).copied().unwrap_or(0)),
            2 => {
                let b0 = u64::from(config_bytes.get(offset).copied().unwrap_or(0));
                let b1 = u64::from(config_bytes.get(offset + 1).copied().unwrap_or(0));
                b0 | (b1 << 8)
            }
            4 => {
                let mut val = 0u64;
                for i in 0..4 {
                    val |= u64::from(config_bytes.get(offset + i).copied().unwrap_or(0)) << (i * 8);
                }
                val
            }
            8 => {
                let mut val = 0u64;
                for i in 0..8 {
                    val |= u64::from(config_bytes.get(offset + i).copied().unwrap_or(0)) << (i * 8);
                }
                val
            }
            _ => 0,
        }
    }

    #[must_use]
    fn config_as_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(28);
        bytes.extend_from_slice(&self.config.capacity.to_le_bytes());
        bytes.extend_from_slice(&self.config.size_max.to_le_bytes());
        bytes.extend_from_slice(&self.config.seg_max.to_le_bytes());
        bytes.extend_from_slice(&self.config.cylinders.to_le_bytes());
        bytes.push(self.config.heads);
        bytes.push(self.config.sectors);
        bytes.extend_from_slice(&self.config.blk_size.to_le_bytes());
        bytes
    }

    /// Process a block request.
    ///
    /// # Arguments
    ///
    /// * `header_bytes` - the 16-byte request header
    /// * `data_buf` - the data buffer (for reads: output, for writes: input)
    ///
    /// # Returns
    ///
    /// A tuple of (`status_byte`, `bytes_transferred`).
    pub fn process_request(&mut self, header_bytes: &[u8], data_buf: &mut [u8]) -> (u8, usize) {
        let Some(header) = BlockRequestHeader::from_bytes(header_bytes) else {
            return (VIRTIO_BLK_S_IOERR, 0);
        };

        match header.request_type {
            VIRTIO_BLK_T_IN => self.handle_read(header.sector, data_buf),
            VIRTIO_BLK_T_OUT => self.handle_write(header.sector, data_buf),
            VIRTIO_BLK_T_FLUSH => self.handle_flush(),
            VIRTIO_BLK_T_GET_ID => self.handle_get_id(data_buf),
            VIRTIO_BLK_T_DISCARD => self.handle_discard(
                header.sector,
                u64::from(u32::try_from(data_buf.len()).unwrap_or(u32::MAX)),
            ),
            _ => {
                self.stats.errors += 1;
                (VIRTIO_BLK_S_UNSUPP, 0)
            }
        }
    }

    fn handle_read(&mut self, sector: u64, buf: &mut [u8]) -> (u8, usize) {
        let offset = sector * 512;
        if let Ok(n) = self.backend.read_at(offset, buf) {
            self.stats.reads += 1;
            self.stats.read_bytes += u64::from(u32::try_from(n).unwrap_or(u32::MAX));
            (VIRTIO_BLK_S_OK, n)
        } else {
            self.stats.errors += 1;
            (VIRTIO_BLK_S_IOERR, 0)
        }
    }

    fn handle_write(&mut self, sector: u64, buf: &[u8]) -> (u8, usize) {
        if self.backend.is_readonly() {
            self.stats.errors += 1;
            return (VIRTIO_BLK_S_IOERR, 0);
        }
        let offset = sector * 512;
        if let Ok(n) = self.backend.write_at(offset, buf) {
            self.stats.writes += 1;
            self.stats.write_bytes += u64::from(u32::try_from(n).unwrap_or(u32::MAX));
            (VIRTIO_BLK_S_OK, n)
        } else {
            self.stats.errors += 1;
            (VIRTIO_BLK_S_IOERR, 0)
        }
    }

    fn handle_flush(&mut self) -> (u8, usize) {
        if matches!(self.backend.flush(), Ok(())) {
            self.stats.flushes += 1;
            (VIRTIO_BLK_S_OK, 0)
        } else {
            self.stats.errors += 1;
            (VIRTIO_BLK_S_IOERR, 0)
        }
    }

    fn handle_get_id(&self, buf: &mut [u8]) -> (u8, usize) {
        let len = buf.len().min(20);
        buf[..len].copy_from_slice(&self.device_id[..len]);
        (VIRTIO_BLK_S_OK, len)
    }

    fn handle_discard(&mut self, sector: u64, len_bytes: u64) -> (u8, usize) {
        let offset = sector * 512;
        if matches!(self.backend.trim(offset, len_bytes), Ok(())) {
            self.stats.discards += 1;
            (VIRTIO_BLK_S_OK, 0)
        } else {
            self.stats.errors += 1;
            (VIRTIO_BLK_S_IOERR, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryBackend;

    fn make_device() -> VirtioBlockDevice {
        let backend = Arc::new(MemoryBackend::new(1_048_576)); // 1MB
        VirtioBlockDevice::new(backend, "test-disk")
    }

    fn make_header(req_type: u32, sector: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&req_type.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
        bytes.extend_from_slice(&sector.to_le_bytes());
        bytes
    }

    #[test]
    fn test_read_write() {
        let mut dev = make_device();

        // Write "Hello" to sector 0
        let header = make_header(VIRTIO_BLK_T_OUT, 0);
        let mut data = b"Hello, VirtIO block!".to_vec();
        data.resize(512, 0);
        let (status, written) = dev.process_request(&header, &mut data);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(written, 512);

        // Read it back
        let header = make_header(VIRTIO_BLK_T_IN, 0);
        let mut buf = vec![0u8; 512];
        let (status, read) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(read, 512);
        assert_eq!(&buf[..20], b"Hello, VirtIO block!");
    }

    #[test]
    fn test_flush() {
        let mut dev = make_device();
        let header = make_header(VIRTIO_BLK_T_FLUSH, 0);
        let mut data = vec![];
        let (status, _) = dev.process_request(&header, &mut data);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(dev.stats().flushes, 1);
    }

    #[test]
    fn test_get_id() {
        let mut dev = make_device();
        let header = make_header(VIRTIO_BLK_T_GET_ID, 0);
        let mut buf = vec![0u8; 20];
        let (status, len) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(len, 20);
        assert_eq!(&buf[..9], b"test-disk");
    }

    #[test]
    fn test_discard() {
        let mut dev = make_device();
        // Write data first
        let header = make_header(VIRTIO_BLK_T_OUT, 0);
        let mut data = vec![0xFF; 512];
        let _ = dev.process_request(&header, &mut data);

        // Discard sector 0
        let header = make_header(VIRTIO_BLK_T_DISCARD, 0);
        let mut buf = vec![0u8; 512];
        let (status, _) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(dev.stats().discards, 1);
    }

    #[test]
    fn test_unsupported_request() {
        let mut dev = make_device();
        let header = make_header(255, 0);
        let mut buf = vec![];
        let (status, _) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_UNSUPP);
    }

    #[test]
    fn test_readonly_device() {
        let backend = Arc::new(MemoryBackend::new_readonly(vec![0u8; 1_048_576]));
        let mut dev = VirtioBlockDevice::new(backend, "ro-disk");
        assert!(dev.features().contains(BlockFeatures::RO));

        let header = make_header(VIRTIO_BLK_T_OUT, 0);
        let mut data = vec![0xFF; 512];
        let (status, _) = dev.process_request(&header, &mut data);
        assert_eq!(status, VIRTIO_BLK_S_IOERR);
    }

    #[test]
    fn test_config_read() {
        let dev = make_device();
        // Capacity is at offset 0, 8 bytes = 1MB / 512 = 2048 sectors
        let cap = dev.read_config(0, 8);
        assert_eq!(cap, 2048);
        // blk_size at offset 20, 4 bytes
        let blk_size = dev.read_config(20, 4);
        assert_eq!(blk_size, 512);
    }

    #[test]
    fn test_stats() {
        let mut dev = make_device();
        let header = make_header(VIRTIO_BLK_T_OUT, 0);
        let mut data = vec![0xFF; 512];
        let _ = dev.process_request(&header, &mut data);
        let _ = dev.process_request(&header, &mut data);

        let header = make_header(VIRTIO_BLK_T_IN, 0);
        let mut buf = vec![0u8; 512];
        let _ = dev.process_request(&header, &mut buf);

        assert_eq!(dev.stats().writes, 2);
        assert_eq!(dev.stats().reads, 1);
        assert_eq!(dev.stats().write_bytes, 1024);
        assert_eq!(dev.stats().read_bytes, 512);
    }
}
