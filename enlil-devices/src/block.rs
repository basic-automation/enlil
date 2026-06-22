//! `VirtIO` Block Device emulation.
//!
//! Implements the `VirtIO` block device specification (virtio-blk).
//! Uses the [`StorageBackend`] trait for pluggable storage backends.

use crate::storage::StorageBackend;
use crate::truncate::usize_of;
use std::sync::Arc;

// VirtIO block request types
pub const VIRTIO_BLK_T_IN: u32 = 0; // Read
pub const VIRTIO_BLK_T_OUT: u32 = 1; // Write
pub const VIRTIO_BLK_T_FLUSH: u32 = 4; // Flush
pub const VIRTIO_BLK_T_GET_ID: u32 = 8; // Get device ID
pub const VIRTIO_BLK_T_DISCARD: u32 = 11; // Discard/trim
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13; // Write zeroes

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
        const WRITE_ZEROES = 1 << 14;
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
    /// Max discard size in 512-byte sectors (if `DISCARD`).
    pub max_discard_sectors: u32,
    /// Max number of discard segments per request (if `DISCARD`).
    pub max_discard_seg: u32,
    /// Discard alignment in 512-byte sectors (if `DISCARD`).
    pub discard_sector_alignment: u32,
    /// Max write-zeroes size in 512-byte sectors (if `WRITE_ZEROES`).
    pub max_write_zeroes_sectors: u32,
    /// Max number of write-zeroes segments per request (if `WRITE_ZEROES`).
    pub max_write_zeroes_seg: u32,
    /// Whether write-zeroes may deallocate (unmap) the range (if `WRITE_ZEROES`).
    /// We always write real zeros, so this is 0.
    pub write_zeroes_may_unmap: u8,
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
    pub const fn from_bytes(bytes: &[u8]) -> Option<Self> {
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

/// Per-segment `unmap` flag in a discard / write-zeroes descriptor: the device
/// may deallocate the range rather than write zeros. Reserved (must be 0) for
/// discard requests.
pub const VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP: u32 = 0x1;

/// One `virtio_blk_discard_write_zeroes` segment (16 bytes, little-endian).
///
/// Carried in a discard or write-zeroes request's data buffer. The target range
/// comes from *here*, not from the request header — the header `sector` field is
/// unused for these commands.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct DiscardWriteZeroesSegment {
    /// First sector of the range (512-byte units).
    pub sector: u64,
    /// Number of 512-byte sectors in the range.
    pub num_sectors: u32,
    /// Flags ([`VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP`]); other bits reserved 0.
    pub flags: u32,
}

impl DiscardWriteZeroesSegment {
    /// Size of one on-the-wire segment.
    pub const SIZE: usize = 16;

    /// Parse one segment from a 16-byte little-endian slice.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            sector: u64::from_le_bytes(b[0..8].try_into().ok()?),
            num_sectors: u32::from_le_bytes(b[8..12].try_into().ok()?),
            flags: u32::from_le_bytes(b[12..16].try_into().ok()?),
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
    pub write_zeroes: u64,
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

        features |= BlockFeatures::DISCARD | BlockFeatures::WRITE_ZEROES;

        let config = BlockConfig {
            capacity: capacity / 512,
            size_max: 1_048_576, // 1MB max segment
            seg_max: 128,
            cylinders: 0,
            heads: 0,
            sectors: 0,
            blk_size: 512,
            // Advertise usable limits for the DISCARD/WRITE_ZEROES features
            // above: a guest reads these config fields and treats a zero limit
            // as "feature present but unusable", so they must be non-zero. We
            // parse multiple per-request segment descriptors, so advertise a
            // realistic multi-segment limit; alignment 1 sector = no special
            // alignment.
            max_discard_sectors: 0x0040_0000, // 4M sectors (2 GiB) per request
            max_discard_seg: 256,
            discard_sector_alignment: 1,
            max_write_zeroes_sectors: 0x0040_0000,
            max_write_zeroes_seg: 256,
            write_zeroes_may_unmap: 0, // we always write real zeros
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
        // Lay the fields out at their fixed `struct virtio_blk_config` offsets.
        // The discard/write-zeroes fields live at offsets 36..57, so the
        // intervening topology (24..32), writeback (32), num_queues (34..36)
        // are emitted as zeros to keep the later offsets correct even though we
        // do not advertise those features.
        let mut bytes = Vec::with_capacity(60);
        bytes.extend_from_slice(&self.config.capacity.to_le_bytes()); // 0
        bytes.extend_from_slice(&self.config.size_max.to_le_bytes()); // 8
        bytes.extend_from_slice(&self.config.seg_max.to_le_bytes()); // 12
        bytes.extend_from_slice(&self.config.cylinders.to_le_bytes()); // 16
        bytes.push(self.config.heads); // 18
        bytes.push(self.config.sectors); // 19
        bytes.extend_from_slice(&self.config.blk_size.to_le_bytes()); // 20
        bytes.extend_from_slice(&[0u8; 8]); // 24: topology (unadvertised)
        bytes.push(0); // 32: writeback
        bytes.push(0); // 33: unused0
        bytes.extend_from_slice(&0u16.to_le_bytes()); // 34: num_queues
        bytes.extend_from_slice(&self.config.max_discard_sectors.to_le_bytes()); // 36
        bytes.extend_from_slice(&self.config.max_discard_seg.to_le_bytes()); // 40
        bytes.extend_from_slice(&self.config.discard_sector_alignment.to_le_bytes()); // 44
        bytes.extend_from_slice(&self.config.max_write_zeroes_sectors.to_le_bytes()); // 48
        bytes.extend_from_slice(&self.config.max_write_zeroes_seg.to_le_bytes()); // 52
        bytes.push(self.config.write_zeroes_may_unmap); // 56
        bytes.extend_from_slice(&[0u8; 3]); // 57: unused1[3]
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
            // The range(s) for these come from segment descriptors in the data
            // buffer, not the header — see `DiscardWriteZeroesSegment`.
            VIRTIO_BLK_T_DISCARD => self.handle_discard(data_buf),
            VIRTIO_BLK_T_WRITE_ZEROES => self.handle_write_zeroes(data_buf),
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

    /// Parse the segment descriptors carried in a discard / write-zeroes data
    /// buffer. The buffer must be a non-empty whole number of 16-byte segments,
    /// no more than `max_seg` of them. Returns `None` on a malformed buffer.
    fn parse_dwz_segments(buf: &[u8], max_seg: u32) -> Option<Vec<DiscardWriteZeroesSegment>> {
        let stride = DiscardWriteZeroesSegment::SIZE;
        if buf.is_empty() || !buf.len().is_multiple_of(stride) {
            return None;
        }
        let count = buf.len() / stride;
        if count > usize_of(max_seg) {
            return None;
        }
        (0..count)
            .map(|i| DiscardWriteZeroesSegment::from_bytes(&buf[i * stride..]))
            .collect()
    }

    /// Resolve and validate a segment to a byte range within the device.
    /// `max_sectors` is the advertised per-segment cap. Returns `None` if the
    /// range is out of bounds or too large.
    fn dwz_range(&self, seg: DiscardWriteZeroesSegment, max_sectors: u32) -> Option<(u64, u64)> {
        if seg.num_sectors == 0 || seg.num_sectors > max_sectors {
            return None;
        }
        let offset = seg.sector.checked_mul(512)?;
        let len = u64::from(seg.num_sectors).checked_mul(512)?;
        let end = offset.checked_add(len)?;
        if end > self.backend.capacity() {
            return None;
        }
        Some((offset, len))
    }

    /// Discard (trim) the ranges named by the segment descriptors. Per the spec
    /// the header `sector` is unused; each range comes from a
    /// [`DiscardWriteZeroesSegment`]. The `unmap` flag is reserved for discard
    /// and must be 0.
    fn handle_discard(&mut self, seg_bytes: &[u8]) -> (u8, usize) {
        if self.backend.is_readonly() {
            self.stats.errors += 1;
            return (VIRTIO_BLK_S_IOERR, 0);
        }
        let Some(segs) = Self::parse_dwz_segments(seg_bytes, self.config.max_discard_seg) else {
            self.stats.errors += 1;
            return (VIRTIO_BLK_S_IOERR, 0);
        };
        for seg in segs {
            // `unmap` (and any other flag) is reserved 0 for discard.
            let Some((offset, len)) = (seg.flags == 0)
                .then(|| self.dwz_range(seg, self.config.max_discard_sectors))
                .flatten()
            else {
                self.stats.errors += 1;
                return (VIRTIO_BLK_S_IOERR, 0);
            };
            if self.backend.trim(offset, len).is_err() {
                self.stats.errors += 1;
                return (VIRTIO_BLK_S_IOERR, 0);
            }
        }
        self.stats.discards += 1;
        (VIRTIO_BLK_S_OK, 0)
    }

    /// Zero the ranges named by the segment descriptors. A modern guest issues
    /// `VIRTIO_BLK_T_WRITE_ZEROES` to clear a range (mkfs, partition wipes, swap
    /// init) far more efficiently than streaming an all-zero payload. We always
    /// write real zeros (so it is correct for every backend), in capped chunks
    /// so a large request never allocates a huge buffer. The only accepted flag
    /// is `unmap`, which we honour by still producing zeros.
    fn handle_write_zeroes(&mut self, seg_bytes: &[u8]) -> (u8, usize) {
        if self.backend.is_readonly() {
            self.stats.errors += 1;
            return (VIRTIO_BLK_S_IOERR, 0);
        }
        let Some(segs) = Self::parse_dwz_segments(seg_bytes, self.config.max_write_zeroes_seg)
        else {
            self.stats.errors += 1;
            return (VIRTIO_BLK_S_IOERR, 0);
        };
        let chunk_size: u64 = 64 * 1024;
        let zeros = vec![0u8; usize_of(chunk_size)];
        for seg in segs {
            // Only the `unmap` bit is defined; reject any other reserved bits.
            if seg.flags & !VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP != 0 {
                self.stats.errors += 1;
                return (VIRTIO_BLK_S_IOERR, 0);
            }
            let Some((offset, len)) = self.dwz_range(seg, self.config.max_write_zeroes_sectors)
            else {
                self.stats.errors += 1;
                return (VIRTIO_BLK_S_IOERR, 0);
            };
            let mut at = offset;
            let mut remaining = len;
            while remaining > 0 {
                let this = usize_of(remaining.min(chunk_size));
                match self.backend.write_at(at, &zeros[..this]) {
                    Ok(n) if n == this => {}
                    _ => {
                        self.stats.errors += 1;
                        return (VIRTIO_BLK_S_IOERR, 0);
                    }
                }
                at += this as u64;
                remaining -= this as u64;
            }
        }
        self.stats.write_zeroes += 1;
        (VIRTIO_BLK_S_OK, 0)
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

    /// Build a `virtio_blk_discard_write_zeroes` segment buffer (16 bytes each).
    fn make_dwz(segments: &[(u64, u32, u32)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(sector, num_sectors, flags) in segments {
            bytes.extend_from_slice(&sector.to_le_bytes());
            bytes.extend_from_slice(&num_sectors.to_le_bytes());
            bytes.extend_from_slice(&flags.to_le_bytes());
        }
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
        // The discard range comes from the data-buffer descriptor, not the
        // header sector — discard sectors 0..2 (1 KiB).
        let header = make_header(VIRTIO_BLK_T_DISCARD, 0);
        let mut buf = make_dwz(&[(0, 2, 0)]);
        let (status, _) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(dev.stats().discards, 1);
    }

    #[test]
    fn test_discard_multiple_segments() {
        let mut dev = make_device();
        let header = make_header(VIRTIO_BLK_T_DISCARD, 0);
        let mut buf = make_dwz(&[(0, 1, 0), (8, 1, 0), (16, 2, 0)]);
        let (status, _) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(dev.stats().discards, 1);
    }

    #[test]
    fn test_discard_rejects_unmap_flag_and_bad_buffer() {
        let mut dev = make_device();
        let header = make_header(VIRTIO_BLK_T_DISCARD, 0);
        // unmap is reserved for discard.
        let mut buf = make_dwz(&[(0, 1, VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP)]);
        assert_eq!(dev.process_request(&header, &mut buf).0, VIRTIO_BLK_S_IOERR);
        // A buffer that is not a whole number of segments is malformed.
        let mut bad = vec![0u8; 17];
        assert_eq!(dev.process_request(&header, &mut bad).0, VIRTIO_BLK_S_IOERR);
        // Out-of-range sector.
        let mut oob = make_dwz(&[(1_000_000, 1, 0)]);
        assert_eq!(dev.process_request(&header, &mut oob).0, VIRTIO_BLK_S_IOERR);
    }

    #[test]
    fn test_write_zeroes_clears_a_range() {
        let mut dev = make_device();
        assert!(dev.features().contains(BlockFeatures::WRITE_ZEROES));

        // Fill sectors 0 and 1 (1 KiB) with 0xFF.
        let header = make_header(VIRTIO_BLK_T_OUT, 0);
        let mut data = vec![0xFF; 1024];
        let (status, _) = dev.process_request(&header, &mut data);
        assert_eq!(status, VIRTIO_BLK_S_OK);

        // WRITE_ZEROES sectors 0..2 — the range comes from the descriptor.
        let header = make_header(VIRTIO_BLK_T_WRITE_ZEROES, 0);
        let mut buf = make_dwz(&[(0, 2, 0)]);
        let (status, _) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(dev.stats().write_zeroes, 1);

        // The range now reads back as zeros.
        let header = make_header(VIRTIO_BLK_T_IN, 0);
        let mut readback = vec![0xAAu8; 1024];
        let (status, n) = dev.process_request(&header, &mut readback);
        assert_eq!(status, VIRTIO_BLK_S_OK);
        assert_eq!(n, 1024);
        assert!(readback.iter().all(|&b| b == 0), "range was zeroed");
    }

    #[test]
    fn test_write_zeroes_accepts_unmap_flag() {
        let mut dev = make_device();
        let header = make_header(VIRTIO_BLK_T_OUT, 0);
        let mut data = vec![0xFF; 512];
        let _ = dev.process_request(&header, &mut data);

        // unmap is a valid flag for write-zeroes; we still produce zeros.
        let header = make_header(VIRTIO_BLK_T_WRITE_ZEROES, 0);
        let mut buf = make_dwz(&[(0, 1, VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP)]);
        let (status, _) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_OK);

        let header = make_header(VIRTIO_BLK_T_IN, 0);
        let mut readback = vec![0xAAu8; 512];
        dev.process_request(&header, &mut readback);
        assert!(readback.iter().all(|&b| b == 0), "unmap range was zeroed");

        // A reserved (non-unmap) flag bit is rejected.
        let header = make_header(VIRTIO_BLK_T_WRITE_ZEROES, 0);
        let mut bad = make_dwz(&[(0, 1, 0x2)]);
        assert_eq!(dev.process_request(&header, &mut bad).0, VIRTIO_BLK_S_IOERR);
    }

    #[test]
    fn test_write_zeroes_rejected_on_readonly() {
        let backend = Arc::new(MemoryBackend::new_readonly(vec![0xFFu8; 1_048_576]));
        let mut dev = VirtioBlockDevice::new(backend, "ro-disk");
        let header = make_header(VIRTIO_BLK_T_WRITE_ZEROES, 0);
        let mut buf = make_dwz(&[(0, 1, 0)]);
        let (status, _) = dev.process_request(&header, &mut buf);
        assert_eq!(status, VIRTIO_BLK_S_IOERR);
        assert_eq!(dev.stats().write_zeroes, 0);
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
    fn test_config_reports_discard_and_write_zeroes_limits() {
        let dev = make_device();
        // We advertise DISCARD + WRITE_ZEROES, so a guest reads their limits
        // from the fixed config offsets; they must be non-zero or the guest
        // treats the feature as unusable.
        assert!(dev.features().contains(BlockFeatures::DISCARD));
        assert!(dev.features().contains(BlockFeatures::WRITE_ZEROES));
        // max_discard_sectors @ 36, max_discard_seg @ 40, alignment @ 44.
        assert_eq!(dev.read_config(36, 4), 0x0040_0000);
        assert_eq!(dev.read_config(40, 4), 256);
        assert_eq!(dev.read_config(44, 4), 1);
        // max_write_zeroes_sectors @ 48, seg @ 52, may_unmap @ 56.
        assert_eq!(dev.read_config(48, 4), 0x0040_0000);
        assert_eq!(dev.read_config(52, 4), 256);
        assert_eq!(dev.read_config(56, 1), 0);
        // Earlier fields are unmoved: blk_size still at 20.
        assert_eq!(dev.read_config(20, 4), 512);
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
