//! Qcow2 disk image backend (read-only).
//!
//! Parses the qcow2 header and L1/L2 tables to resolve guest cluster
//! offsets to host file offsets. Write support is deferred to a later phase.

use super::StorageBackend;
use crate::truncate::usize_of;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

/// Qcow2 magic number: "QFI\xfb"
const QCOW2_MAGIC: u32 = 0x5146_49FB;

/// Qcow2 header (v2/v3).
#[derive(Debug, Clone)]
pub struct QcowHeader {
    pub magic: u32,
    pub version: u32,
    pub backing_file_offset: u64,
    pub backing_file_size: u32,
    pub cluster_bits: u32,
    pub size: u64, // virtual size in bytes
    pub crypt_method: u32,
    pub l1_size: u32, // number of L1 table entries
    pub l1_table_offset: u64,
    pub refcount_table_offset: u64,
    pub refcount_table_clusters: u32,
    pub nb_snapshots: u32,
    pub snapshots_offset: u64,
}

impl QcowHeader {
    /// Parse a qcow2 header from raw bytes (must be at least 72 bytes).
    ///
    /// # Errors
    ///
    /// Returns an error if the header is too short or has an invalid magic number.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 72 {
            bail!("qcow2 header too short: {} bytes", bytes.len());
        }
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != QCOW2_MAGIC {
            bail!("not a qcow2 file: magic 0x{magic:08x}");
        }
        let version = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != 2 && version != 3 {
            bail!("unsupported qcow2 version: {version}");
        }
        Ok(Self {
            magic,
            version,
            backing_file_offset: u64::from_be_bytes(bytes[8..16].try_into()?),
            backing_file_size: u32::from_be_bytes(bytes[16..20].try_into()?),
            cluster_bits: u32::from_be_bytes(bytes[20..24].try_into()?),
            size: u64::from_be_bytes(bytes[24..32].try_into()?),
            crypt_method: u32::from_be_bytes(bytes[32..36].try_into()?),
            l1_size: u32::from_be_bytes(bytes[36..40].try_into()?),
            l1_table_offset: u64::from_be_bytes(bytes[40..48].try_into()?),
            refcount_table_offset: u64::from_be_bytes(bytes[48..56].try_into()?),
            refcount_table_clusters: u32::from_be_bytes(bytes[56..60].try_into()?),
            nb_snapshots: u32::from_be_bytes(bytes[60..64].try_into()?),
            snapshots_offset: u64::from_be_bytes(bytes[64..72].try_into()?),
        })
    }

    /// Cluster size in bytes.
    #[must_use]
    pub const fn cluster_size(&self) -> u64 {
        1u64 << self.cluster_bits
    }

    /// Number of L2 entries per L2 table.
    #[must_use]
    pub const fn l2_entries(&self) -> u64 {
        self.cluster_size() / 8
    }
}

/// Read-only qcow2 backend.
///
/// Supports reading from qcow2 images by walking L1 → L2 → data cluster.
/// Unallocated clusters return zeroes. No backing file chain support yet.
pub struct QcowBackend {
    file: Mutex<File>,
    header: QcowHeader,
    l1_table: Vec<u64>,
}

impl QcowBackend {
    /// Open a qcow2 image file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or contains invalid qcow2 data.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let mut file = File::open(path)
            .with_context(|| format!("failed to open qcow2: {}", path.display()))?;

        // Read header
        let mut header_buf = [0u8; 104]; // v3 header is up to 104 bytes
        let n = file.read(&mut header_buf)?;
        if n < 72 {
            bail!("file too small for qcow2 header");
        }
        let header = QcowHeader::from_bytes(&header_buf)?;

        // Read L1 table
        file.seek(SeekFrom::Start(header.l1_table_offset))?;
        let mut l1_table = Vec::with_capacity(header.l1_size as usize);
        for _ in 0..header.l1_size {
            let mut buf = [0u8; 8];
            file.read_exact(&mut buf)?;
            l1_table.push(u64::from_be_bytes(buf));
        }

        Ok(Self {
            file: Mutex::new(file),
            header,
            l1_table,
        })
    }

    /// Resolve a guest byte offset to a host file offset.
    /// Returns `None` if the cluster is unallocated (read as zeroes).
    fn resolve_offset(&self, guest_offset: u64, file: &mut File) -> Result<Option<u64>> {
        let cluster_size = self.header.cluster_size();
        let l2_entries = self.header.l2_entries();

        // L1 index = guest_offset / (l2_entries * cluster_size)
        let l1_index = guest_offset / (l2_entries * cluster_size);
        if l1_index >= self.l1_table.len() as u64 {
            return Ok(None);
        }

        let l1_entry = self.l1_table[usize_of(l1_index)];
        // Bits 9..55 contain the offset of the L2 table
        let l2_table_offset = l1_entry & 0x00FF_FFFF_FFFF_FE00;
        if l2_table_offset == 0 {
            return Ok(None); // L2 table not allocated
        }

        // L2 index
        let l2_index = (guest_offset / cluster_size) % l2_entries;

        // Read L2 entry
        file.seek(SeekFrom::Start(l2_table_offset + l2_index * 8))?;
        let mut buf = [0u8; 8];
        file.read_exact(&mut buf)?;
        let l2_entry = u64::from_be_bytes(buf);

        // Bits 9..55 contain the host cluster offset
        let host_cluster_offset = l2_entry & 0x00FF_FFFF_FFFF_FE00;
        if host_cluster_offset == 0 {
            return Ok(None); // Cluster not allocated
        }

        // Offset within cluster
        let in_cluster_offset = guest_offset & (cluster_size - 1);
        Ok(Some(host_cluster_offset + in_cluster_offset))
    }
}

impl StorageBackend for QcowBackend {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.header.size {
            return Ok(0);
        }
        let mut file = self.file.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let cluster_size = self.header.cluster_size();
        let mut total_read = 0usize;
        let mut remaining = buf.len().min(usize_of(self.header.size - offset));
        let mut current_offset = offset;

        while remaining > 0 {
            // How many bytes until the end of this cluster?
            let in_cluster = usize_of(current_offset % cluster_size);
            let chunk = remaining.min(usize_of(cluster_size) - in_cluster);

            match self.resolve_offset(current_offset, &mut file)? {
                Some(host_offset) => {
                    file.seek(SeekFrom::Start(host_offset))?;
                    file.read_exact(&mut buf[total_read..total_read + chunk])?;
                }
                None => {
                    // Unallocated cluster → zeroes
                    buf[total_read..total_read + chunk].fill(0);
                }
            }

            total_read += chunk;
            current_offset += chunk as u64;
            remaining -= chunk;
        }

        drop(file);
        Ok(total_read)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        bail!("qcow2 backend is read-only (write support not yet implemented)")
    }

    fn flush(&self) -> Result<()> {
        Ok(()) // read-only, nothing to flush
    }

    fn capacity(&self) -> u64 {
        self.header.size
    }

    fn is_readonly(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::truncate::u8_of;

    fn make_minimal_qcow2() -> Vec<u8> {
        // Build a minimal valid qcow2 v2 image:
        // - 1MB virtual size
        // - cluster_bits = 16 (64KB clusters)
        // - 1 L1 entry, 1 L2 table, 1 data cluster
        let cluster_bits: u32 = 16;
        let cluster_size: usize = 1 << cluster_bits;
        let virtual_size: u64 = 1024 * 1024; // 1MB

        // Layout:
        // Cluster 0: header
        // Cluster 1: L1 table
        // Cluster 2: L2 table
        // Cluster 3: data cluster (first 64KB of guest)
        let l1_offset: u64 = cluster_size as u64;
        let l2_offset: u64 = 2 * cluster_size as u64;
        let data_offset: u64 = 3 * cluster_size as u64;

        let total_size = 4 * cluster_size;
        let mut img = vec![0u8; total_size];

        // Header
        img[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        img[4..8].copy_from_slice(&2u32.to_be_bytes()); // version
        img[8..16].copy_from_slice(&0u64.to_be_bytes()); // backing_file_offset
        img[16..20].copy_from_slice(&0u32.to_be_bytes()); // backing_file_size
        img[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        img[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        img[32..36].copy_from_slice(&0u32.to_be_bytes()); // crypt_method
        img[36..40].copy_from_slice(&1u32.to_be_bytes()); // l1_size = 1
        img[40..48].copy_from_slice(&l1_offset.to_be_bytes());
        img[48..56].copy_from_slice(&0u64.to_be_bytes()); // refcount_table_offset
        img[56..60].copy_from_slice(&0u32.to_be_bytes()); // refcount_table_clusters
        img[60..64].copy_from_slice(&0u32.to_be_bytes()); // nb_snapshots
        img[64..72].copy_from_slice(&0u64.to_be_bytes()); // snapshots_offset

        // L1 table: entry 0 → L2 table at cluster 2
        let l1_start = cluster_size;
        img[l1_start..l1_start + 8].copy_from_slice(&l2_offset.to_be_bytes());

        // L2 table: entry 0 → data cluster at cluster 3
        let l2_start = 2 * cluster_size;
        img[l2_start..l2_start + 8].copy_from_slice(&data_offset.to_be_bytes());

        // Data cluster: fill with a pattern
        let data_start = 3 * cluster_size;
        for i in 0..cluster_size {
            img[data_start + i] = u8_of(i & 0xFF);
        }

        img
    }

    #[test]
    fn parse_qcow2_header() {
        let img = make_minimal_qcow2();
        let header = QcowHeader::from_bytes(&img).unwrap();
        assert_eq!(header.magic, QCOW2_MAGIC);
        assert_eq!(header.version, 2);
        assert_eq!(header.cluster_bits, 16);
        assert_eq!(header.size, 1024 * 1024);
        assert_eq!(header.l1_size, 1);
    }

    #[test]
    fn read_allocated_cluster() {
        let img = make_minimal_qcow2();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();

        let backend = QcowBackend::open(tmp.path()).unwrap();
        assert_eq!(backend.capacity(), 1024 * 1024);
        assert!(backend.is_readonly());

        // Read first 256 bytes — should match our pattern
        let mut buf = vec![0u8; 256];
        let n = backend.read_at(0, &mut buf).unwrap();
        assert_eq!(n, 256);
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, u8_of(i & 0xFF), "mismatch at offset {i}");
        }
    }

    #[test]
    fn read_unallocated_returns_zeroes() {
        let img = make_minimal_qcow2();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();

        let backend = QcowBackend::open(tmp.path()).unwrap();

        // Cluster 1 (offset 64KB) has no L2 entry → should be zeroes
        let mut buf = vec![0xFFu8; 256];
        let n = backend.read_at(65536, &mut buf).unwrap();
        assert_eq!(n, 256);
        assert!(
            buf.iter().all(|&b| b == 0),
            "unallocated cluster should be zeroes"
        );
    }

    #[test]
    fn write_rejected() {
        let img = make_minimal_qcow2();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();

        let backend = QcowBackend::open(tmp.path()).unwrap();
        assert!(backend.write_at(0, &[1, 2, 3]).is_err());
    }

    #[test]
    fn invalid_magic_rejected() {
        let mut img = make_minimal_qcow2();
        img[0] = 0; // corrupt magic
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();
        assert!(QcowBackend::open(tmp.path()).is_err());
    }
}
