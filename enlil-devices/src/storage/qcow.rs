//! Qcow2 disk image backend.
//!
//! Parses the qcow2 header and L1/L2 tables to resolve guest cluster offsets
//! to host file offsets. Opened read-only via [`QcowBackend::open`] or
//! read-write via [`QcowBackend::open_rw`]; a read-write backend can overwrite
//! data in **already-allocated** clusters (writes that land in an unallocated
//! cluster still need cluster allocation — a separate step).

use super::{RawFileBackend, StorageBackend};
use crate::truncate::{u8_of, u16_of, u32_of, usize_of};
use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

/// Qcow2 magic number: "QFI\xfb"
const QCOW2_MAGIC: u32 = 0x5146_49FB;

/// L1/L2 entry mask for the host cluster offset (bits 9..55).
const L2_OFFSET_MASK: u64 = 0x00FF_FFFF_FFFF_FE00;

/// L2 entry bit 0 (qcow2 v3): the cluster reads as all zeros.
const QCOW_OFLAG_ZERO: u64 = 0x1;

/// L1/L2 entry bit 63: the referenced cluster has refcount exactly 1 (a "copied"
/// cluster the guest may write in place). Cleared when a cluster is shared (e.g.
/// with a snapshot), set when a fresh single-owner cluster is allocated.
const QCOW_OFLAG_COPIED: u64 = 1u64 << 63;

/// Refcount-table entry mask for a refcount-block host offset (bits 9..63; bits
/// 0..9 are reserved). Distinct from [`L2_OFFSET_MASK`], whose top byte is
/// reserved for L2 flags.
const REFT_OFFSET_MASK: u64 = 0xFFFF_FFFF_FFFF_FE00;

/// Where a guest cluster's data lives.
enum ClusterLoc {
    /// Present in this image at the given host file offset.
    Mapped(u64),
    /// Explicitly zeroed in this image (v3 zero flag) — reads as zeros, does
    /// not fall through to a backing file.
    Zero,
    /// Not present in this image — read from the backing file if any, else zeros.
    Unallocated,
}

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
    /// `log2` of the refcount entry width in bits (v3 field at offset 96).
    /// Always 4 (16-bit refcounts) for v2 and the qemu default for v3.
    pub refcount_order: u32,
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
            // refcount_order is a v3-only field (offset 96). v2 is fixed at 16-bit
            // refcounts (order 4); fall back to that if the field is absent.
            refcount_order: if version >= 3 && bytes.len() >= 100 {
                u32::from_be_bytes(bytes[96..100].try_into()?)
            } else {
                4
            },
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

    /// Width of one refcount entry, in bits (`1 << refcount_order`).
    #[must_use]
    pub const fn refcount_bits(&self) -> u32 {
        1u32 << self.refcount_order
    }

    /// Number of refcount entries one refcount block (a single cluster) holds.
    #[must_use]
    pub const fn refcount_block_entries(&self) -> u64 {
        (self.cluster_size() * 8) / self.refcount_bits() as u64
    }

    /// Number of u64 entries the refcount *table* holds.
    #[must_use]
    pub const fn refcount_table_entries(&self) -> u64 {
        (self.refcount_table_clusters as u64 * self.cluster_size()) / 8
    }
}

/// Qcow2 backend.
///
/// Supports reading from qcow2 images by walking L1 → L2 → data cluster.
/// Clusters not present in this image are read from its backing image (the
/// overlay mechanism), or return zeros if there is none; a v3 zero-flagged
/// cluster reads as zeros without consulting the backing. When opened
/// read-write ([`open_rw`](Self::open_rw)), overwrites of data in
/// already-allocated clusters are persisted to the image.
pub struct QcowBackend {
    file: Mutex<File>,
    header: QcowHeader,
    /// In-memory copy of the L1 table, kept in sync with the on-disk table when
    /// allocation grows it (`Mutex` so a write can update it through `&self`).
    /// Always lock `file` before `l1_table` to keep a consistent lock order.
    l1_table: Mutex<Vec<u64>>,
    /// `true` if the underlying file was opened read-write; gates `write_at`.
    writable: bool,
    /// Read-only backing image: clusters not present in this (overlay) image are
    /// read from here. `None` for a standalone image. This is the qcow2 overlay
    /// mechanism behind non-destructive testing (a read-only base + a writable
    /// overlay).
    backing: Option<Box<dyn StorageBackend>>,
}

impl QcowBackend {
    /// Open a qcow2 image file **read-only**.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or contains invalid qcow2 data.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open qcow2: {}", path.display()))?;
        Self::from_file(file, false, path)
    }

    /// Open a qcow2 image file **read-write**, so overwrites of already-allocated
    /// clusters are persisted.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened read-write or contains
    /// invalid qcow2 data.
    pub fn open_rw(path: &std::path::Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open qcow2 read-write: {}", path.display()))?;
        Self::from_file(file, true, path)
    }

    /// Create a fresh qcow2 v3 image at `path` with the given virtual size,
    /// optionally as an overlay over `backing` (the read-only base of a
    /// non-destructive-test pair). The new image maps no clusters: reads fall
    /// through to the backing (or read as zeros with none) and writes allocate
    /// copy-on-write via [`open_rw`](Self::open_rw). 64 KiB clusters, 16-bit
    /// refcounts; the image is written refcount-consistent.
    ///
    /// # Errors
    ///
    /// Returns an error if the geometry would exceed a single refcount block,
    /// the backing path does not fit in the header cluster, or the file cannot
    /// be written.
    pub fn create(path: &Path, virtual_size: u64, backing: Option<&Path>) -> Result<()> {
        const CLUSTER_BITS: u32 = 16;
        let cluster_size: u64 = 1 << CLUSTER_BITS;
        let l2_entries = cluster_size / 8;

        // One L1 entry covers l2_entries * cluster_size of guest address space.
        let bytes_per_l1 = l2_entries * cluster_size;
        let l1_size = virtual_size.div_ceil(bytes_per_l1).max(1);
        let l1_clusters = (l1_size * 8).div_ceil(cluster_size);

        // Cluster layout: 0 header, [1..] L1 table, refcount table, refcount block.
        let reftable_cluster = 1 + l1_clusters;
        let refblock_cluster = reftable_cluster + 1;
        let metadata_clusters = refblock_cluster + 1;
        let refcount_block_entries = (cluster_size * 8) / 16; // 16-bit refcounts
        if metadata_clusters > refcount_block_entries {
            bail!("virtual size {virtual_size} needs more metadata than one refcount block holds");
        }

        let l1_offset = cluster_size;
        let reftable_offset = reftable_cluster * cluster_size;
        let refblock_offset = refblock_cluster * cluster_size;

        let mut img = vec![0u8; usize_of(metadata_clusters * cluster_size)];
        img[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        img[4..8].copy_from_slice(&3u32.to_be_bytes()); // version 3
        if let Some(backing) = backing {
            let name = backing.to_string_lossy();
            let bytes = name.as_bytes();
            let off: u64 = 0x200; // within cluster 0, past the 104-byte header
            if usize_of(off) + bytes.len() > usize_of(cluster_size) {
                bail!("backing path too long for the header cluster");
            }
            let len = u32::try_from(bytes.len()).context("backing path too long")?;
            img[8..16].copy_from_slice(&off.to_be_bytes());
            img[16..20].copy_from_slice(&len.to_be_bytes());
            img[usize_of(off)..usize_of(off) + bytes.len()].copy_from_slice(bytes);
        }
        img[20..24].copy_from_slice(&CLUSTER_BITS.to_be_bytes());
        img[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        img[36..40].copy_from_slice(&u32_of(l1_size).to_be_bytes());
        img[40..48].copy_from_slice(&l1_offset.to_be_bytes());
        img[48..56].copy_from_slice(&reftable_offset.to_be_bytes());
        img[56..60].copy_from_slice(&1u32.to_be_bytes()); // refcount_table_clusters
        img[96..100].copy_from_slice(&4u32.to_be_bytes()); // refcount_order = 4
        img[100..104].copy_from_slice(&104u32.to_be_bytes()); // header_length

        // Refcount table entry 0 → the first refcount block.
        let rt = usize_of(reftable_offset);
        img[rt..rt + 8].copy_from_slice(&refblock_offset.to_be_bytes());
        // Refcount block: every metadata cluster has refcount 1.
        let rb = usize_of(refblock_offset);
        for c in 0..usize_of(metadata_clusters) {
            img[rb + c * 2..rb + c * 2 + 2].copy_from_slice(&1u16.to_be_bytes());
        }
        // L1 table is left all-zero (nothing mapped yet).

        std::fs::write(path, &img)
            .with_context(|| format!("failed to write new qcow2: {}", path.display()))?;
        Ok(())
    }

    /// Parse the header + L1 table from an already-opened file, and open its
    /// backing image (read-only) if the header names one. `image_path` is used
    /// to resolve a relative backing-file path against the image's directory.
    fn from_file(mut file: File, writable: bool, image_path: &Path) -> Result<Self> {
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

        let backing = Self::open_backing(&mut file, &header, image_path)?;

        Ok(Self {
            file: Mutex::new(file),
            header,
            l1_table: Mutex::new(l1_table),
            writable,
            backing,
        })
    }

    /// Open the backing image named in the header (read-only), resolving a
    /// relative path against `image_path`'s directory, or `None` if the header
    /// names no backing file. The backing image's format is detected by magic:
    /// a qcow2 backing (which may itself chain) or a raw file.
    fn open_backing(
        file: &mut File,
        header: &QcowHeader,
        image_path: &Path,
    ) -> Result<Option<Box<dyn StorageBackend>>> {
        if header.backing_file_offset == 0 || header.backing_file_size == 0 {
            return Ok(None);
        }
        let len = usize_of(u64::from(header.backing_file_size));
        let mut name = vec![0u8; len];
        file.seek(SeekFrom::Start(header.backing_file_offset))?;
        file.read_exact(&mut name)?;
        let name = String::from_utf8(name).context("backing file name is not valid UTF-8")?;

        // Resolve relative paths against the overlay image's directory.
        let backing_path = Path::new(&name);
        let resolved = if backing_path.is_absolute() {
            backing_path.to_path_buf()
        } else {
            image_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(backing_path)
        };

        // Detect the backing format by magic so a raw base image works too.
        let mut magic = [0u8; 4];
        let read = File::open(&resolved)
            .with_context(|| format!("failed to open backing file: {}", resolved.display()))?
            .read(&mut magic)?;
        let is_qcow2 = read == 4 && u32::from_be_bytes(magic) == QCOW2_MAGIC;
        let backend: Box<dyn StorageBackend> = if is_qcow2 {
            Box::new(Self::open(&resolved)?)
        } else {
            Box::new(RawFileBackend::open(
                resolved
                    .to_str()
                    .context("backing path is not valid UTF-8")?,
                true,
            )?)
        };
        Ok(Some(backend))
    }

    /// Resolve a guest byte offset to its cluster location.
    fn resolve_cluster(&self, guest_offset: u64, file: &mut File) -> Result<ClusterLoc> {
        let cluster_size = self.header.cluster_size();
        let l2_entries = self.header.l2_entries();

        // L1 index = guest_offset / (l2_entries * cluster_size)
        let l1_index = guest_offset / (l2_entries * cluster_size);
        let l1 = self
            .l1_table
            .lock()
            .map_err(|e| anyhow::anyhow!("l1 lock: {e}"))?;
        if l1_index >= l1.len() as u64 {
            return Ok(ClusterLoc::Unallocated);
        }
        let l1_entry = l1[usize_of(l1_index)];
        drop(l1);
        // Bits 9..55 contain the offset of the L2 table
        let l2_table_offset = l1_entry & L2_OFFSET_MASK;
        if l2_table_offset == 0 {
            return Ok(ClusterLoc::Unallocated); // L2 table not allocated
        }

        // L2 index
        let l2_index = (guest_offset / cluster_size) % l2_entries;

        // Read L2 entry
        file.seek(SeekFrom::Start(l2_table_offset + l2_index * 8))?;
        let mut buf = [0u8; 8];
        file.read_exact(&mut buf)?;
        let l2_entry = u64::from_be_bytes(buf);

        // The qcow2 v3 "all zeroes" flag (bit 0): the cluster reads as zeros and
        // must NOT fall through to a backing file. (Reserved 0 in v2.)
        if l2_entry & QCOW_OFLAG_ZERO != 0 {
            return Ok(ClusterLoc::Zero);
        }

        // Bits 9..55 contain the host cluster offset
        let host_cluster_offset = l2_entry & L2_OFFSET_MASK;
        if host_cluster_offset == 0 {
            return Ok(ClusterLoc::Unallocated); // not present in this layer
        }

        // Offset within cluster
        let in_cluster_offset = guest_offset & (cluster_size - 1);
        Ok(ClusterLoc::Mapped(host_cluster_offset + in_cluster_offset))
    }

    /// Locate the refcount block covering host cluster `cluster_index`, or
    /// `None` when the refcount table has no block for it yet (the cluster is
    /// free / unmanaged). Shared by the refcount read, write, and pre-check.
    fn refcount_block_offset(&self, cluster_index: u64, file: &mut File) -> Result<Option<u64>> {
        let rb_entries = self.header.refcount_block_entries();
        let rt_index = cluster_index / rb_entries;
        if rt_index >= self.header.refcount_table_entries() {
            return Ok(None); // beyond the refcount table → unmanaged → free
        }
        file.seek(SeekFrom::Start(
            self.header.refcount_table_offset + rt_index * 8,
        ))?;
        let mut buf = [0u8; 8];
        file.read_exact(&mut buf)?;
        let block_offset = u64::from_be_bytes(buf) & REFT_OFFSET_MASK;
        Ok((block_offset != 0).then_some(block_offset))
    }

    /// Read the on-disk refcount of the host cluster at index `cluster_index`
    /// (host-file offset `cluster_index * cluster_size`). Returns 0 when no
    /// refcount block covers the cluster (i.e. it is free), mirroring qemu.
    fn read_refcount(&self, cluster_index: u64, file: &mut File) -> Result<u64> {
        let Some(block_offset) = self.refcount_block_offset(cluster_index, file)? else {
            return Ok(0);
        };

        // Read the entry within the block. Widths < 8 bits are packed
        // big-endian-first within a byte (qcow2 spec §refcounts).
        let block_index = cluster_index % self.header.refcount_block_entries();
        let refcount_bits = self.header.refcount_bits();
        match refcount_bits {
            1 | 2 | 4 => {
                let per_byte = u64::from(8 / refcount_bits);
                file.seek(SeekFrom::Start(block_offset + block_index / per_byte))?;
                let mut b = [0u8; 1];
                file.read_exact(&mut b)?;
                let within = block_index % per_byte;
                let shift = (per_byte - 1 - within) * u64::from(refcount_bits);
                let mask = (1u64 << refcount_bits) - 1;
                Ok((u64::from(b[0]) >> shift) & mask)
            }
            8 => {
                file.seek(SeekFrom::Start(block_offset + block_index))?;
                let mut b = [0u8; 1];
                file.read_exact(&mut b)?;
                Ok(u64::from(b[0]))
            }
            16 => {
                file.seek(SeekFrom::Start(block_offset + block_index * 2))?;
                let mut b = [0u8; 2];
                file.read_exact(&mut b)?;
                Ok(u64::from(u16::from_be_bytes(b)))
            }
            32 => {
                file.seek(SeekFrom::Start(block_offset + block_index * 4))?;
                let mut b = [0u8; 4];
                file.read_exact(&mut b)?;
                Ok(u64::from(u32::from_be_bytes(b)))
            }
            64 => {
                file.seek(SeekFrom::Start(block_offset + block_index * 8))?;
                let mut b = [0u8; 8];
                file.read_exact(&mut b)?;
                Ok(u64::from_be_bytes(b))
            }
            other => bail!("unsupported refcount width: {other} bits"),
        }
    }

    /// Walk every metadata structure reachable from the header and tally how
    /// many times each host cluster is referenced — the refcounts the image
    /// *should* have. This is the model side of [`check_consistency`].
    ///
    /// Snapshots are not modelled; an image carrying any is rejected so we never
    /// report a false "leak" for snapshot-owned clusters.
    fn compute_expected_refcounts(&self, file: &mut File) -> Result<Vec<u64>> {
        if self.header.nb_snapshots != 0 {
            bail!(
                "refcount check does not model snapshots ({} present)",
                self.header.nb_snapshots
            );
        }
        let cluster_size = self.header.cluster_size();
        let file_len = file.seek(SeekFrom::End(0))?;
        let total_clusters = file_len.div_ceil(cluster_size);
        let mut expected = vec![0u64; usize_of(total_clusters)];

        let mut bump = |host_offset: u64| -> Result<()> {
            let idx = host_offset / cluster_size;
            if !host_offset.is_multiple_of(cluster_size) {
                bail!("metadata offset {host_offset:#x} is not cluster-aligned");
            }
            let idx = usize_of(idx);
            if idx >= expected.len() {
                bail!("metadata offset {host_offset:#x} points past end of file");
            }
            expected[idx] += 1;
            Ok(())
        };

        // The header always lives in cluster 0.
        bump(0)?;

        // L1 table (may span several clusters).
        let l1_bytes = u64::from(self.header.l1_size) * 8;
        for c in 0..l1_bytes.div_ceil(cluster_size) {
            bump(self.header.l1_table_offset + c * cluster_size)?;
        }

        // Each L2 table and the data clusters it maps.
        let l2_entries = self.header.l2_entries();
        let l1_snapshot = self
            .l1_table
            .lock()
            .map_err(|e| anyhow::anyhow!("l1 lock: {e}"))?
            .clone();
        for &l1_entry in &l1_snapshot {
            let l2_off = l1_entry & L2_OFFSET_MASK;
            if l2_off == 0 {
                continue;
            }
            bump(l2_off)?;
            for i in 0..l2_entries {
                file.seek(SeekFrom::Start(l2_off + i * 8))?;
                let mut b = [0u8; 8];
                file.read_exact(&mut b)?;
                let l2_entry = u64::from_be_bytes(b);
                // Zero-flagged or unmapped clusters own no host cluster.
                if l2_entry & QCOW_OFLAG_ZERO != 0 {
                    continue;
                }
                let data_off = l2_entry & L2_OFFSET_MASK;
                if data_off != 0 {
                    bump(data_off)?;
                }
            }
        }

        // Refcount table clusters and every refcount block they point at.
        for c in 0..u64::from(self.header.refcount_table_clusters) {
            bump(self.header.refcount_table_offset + c * cluster_size)?;
        }
        for i in 0..self.header.refcount_table_entries() {
            file.seek(SeekFrom::Start(self.header.refcount_table_offset + i * 8))?;
            let mut b = [0u8; 8];
            file.read_exact(&mut b)?;
            let block_off = u64::from_be_bytes(b) & REFT_OFFSET_MASK;
            if block_off != 0 {
                bump(block_off)?;
            }
        }

        Ok(expected)
    }

    /// `qemu-img check`-style consistency check: every host cluster's stored
    /// refcount must equal the number of times the metadata actually references
    /// it. Catches refcount leaks (stored > reachable) and, more dangerously,
    /// under-counts (stored < reachable, which lets a later allocation reuse a
    /// live cluster). Read-only; intended for tests and diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error describing the first mismatch, or any I/O / structural
    /// problem encountered while walking the image.
    pub fn check_consistency(&self) -> Result<()> {
        let mut file = self.file.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let expected = self.compute_expected_refcounts(&mut file)?;
        for (idx, &want) in expected.iter().enumerate() {
            let got = self.read_refcount(idx as u64, &mut file)?;
            if got != want {
                drop(file);
                bail!(
                    "refcount mismatch at cluster {idx} (host offset {:#x}): \
                     stored {got}, reachable {want}",
                    idx as u64 * self.header.cluster_size()
                );
            }
        }
        self.check_copied_flags(&mut file)?;
        drop(file);
        Ok(())
    }

    /// Verify the `OFLAG_COPIED` invariant qemu maintains: an L1/L2 entry has bit
    /// 63 set **iff** the cluster it points at has refcount exactly 1. A wrong
    /// COPIED flag is the bug that lets a guest write in place into a cluster
    /// that is actually shared, so it is worth checking alongside the refcounts.
    fn check_copied_flags(&self, file: &mut File) -> Result<()> {
        let l2_entries = self.header.l2_entries();
        let l1_snapshot = self
            .l1_table
            .lock()
            .map_err(|e| anyhow::anyhow!("l1 lock: {e}"))?
            .clone();
        for &l1_entry in &l1_snapshot {
            let l2_off = l1_entry & L2_OFFSET_MASK;
            if l2_off == 0 {
                continue;
            }
            self.verify_copied(l1_entry, l2_off, "L1 entry", file)?;
            for i in 0..l2_entries {
                file.seek(SeekFrom::Start(l2_off + i * 8))?;
                let mut b = [0u8; 8];
                file.read_exact(&mut b)?;
                let l2_entry = u64::from_be_bytes(b);
                if l2_entry & QCOW_OFLAG_ZERO != 0 {
                    continue;
                }
                let data_off = l2_entry & L2_OFFSET_MASK;
                if data_off != 0 {
                    self.verify_copied(l2_entry, data_off, "L2 entry", file)?;
                }
            }
        }
        Ok(())
    }

    /// Assert one L1/L2 entry's `OFLAG_COPIED` bit matches the pointed-at
    /// cluster's refcount (set iff refcount == 1).
    fn verify_copied(&self, entry: u64, host_off: u64, what: &str, file: &mut File) -> Result<()> {
        let refcount = self.read_refcount(host_off / self.header.cluster_size(), file)?;
        let copied = entry & QCOW_OFLAG_COPIED != 0;
        if copied != (refcount == 1) {
            bail!(
                "{what} at host offset {host_off:#x}: OFLAG_COPIED={copied} but refcount={refcount}"
            );
        }
        Ok(())
    }

    /// Write the on-disk refcount of host cluster `cluster_index`. Fails if no
    /// refcount block covers the cluster yet (block / table growth are later
    /// steps). Writing sub-byte refcounts is unsupported — every image we
    /// produce uses 16-bit refcounts (`refcount_order` 4).
    fn write_refcount(&self, cluster_index: u64, value: u64, file: &mut File) -> Result<()> {
        let block_offset = self
            .refcount_block_offset(cluster_index, file)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no refcount block for cluster {cluster_index} \
                 (refcount block allocation not yet implemented)"
                )
            })?;
        let block_index = cluster_index % self.header.refcount_block_entries();
        match self.header.refcount_bits() {
            8 => {
                file.seek(SeekFrom::Start(block_offset + block_index))?;
                file.write_all(&[u8_of(value)])?;
            }
            16 => {
                file.seek(SeekFrom::Start(block_offset + block_index * 2))?;
                file.write_all(&u16_of(value).to_be_bytes())?;
            }
            32 => {
                file.seek(SeekFrom::Start(block_offset + block_index * 4))?;
                file.write_all(&u32_of(value).to_be_bytes())?;
            }
            64 => {
                file.seek(SeekFrom::Start(block_offset + block_index * 8))?;
                file.write_all(&value.to_be_bytes())?;
            }
            other => bail!("writing {other}-bit refcounts is not supported"),
        }
        Ok(())
    }

    /// Append a fresh, zero-filled host cluster at the end of the image and give
    /// it refcount 1. Returns its host file offset. If the next slot is not yet
    /// covered by a refcount block, one is allocated first (which itself appends
    /// a cluster), so images can grow past a single block's reach.
    fn allocate_host_cluster(&self, file: &mut File) -> Result<u64> {
        let cluster_size = self.header.cluster_size();
        loop {
            let len = file.seek(SeekFrom::End(0))?;
            let offset = len.next_multiple_of(cluster_size);
            let index = offset / cluster_size;
            if self.refcount_block_offset(index, file)?.is_some() {
                file.set_len(offset + cluster_size)?; // zero-extends the new cluster
                self.write_refcount(index, 1, file)?;
                return Ok(offset);
            }
            // No block covers this slot yet — allocate one (it lands here and
            // covers itself) and retry; the data cluster lands just after it.
            self.allocate_refcount_block(index, file)?;
        }
    }

    /// Allocate a new, zeroed refcount block covering host cluster
    /// `cluster_index` and record it in the refcount table. The block is
    /// appended at EOF, where it falls inside its own coverage, so it can record
    /// its own refcount (1) inside itself. Returns the block's host offset.
    ///
    /// Growing the refcount *table* itself (more than `refcount_table_entries`
    /// blocks) is a separate, larger step and is reported as an error here.
    fn allocate_refcount_block(&self, cluster_index: u64, file: &mut File) -> Result<u64> {
        let rb_entries = self.header.refcount_block_entries();
        let rt_index = cluster_index / rb_entries;
        if rt_index >= self.header.refcount_table_entries() {
            bail!(
                "refcount table too small for cluster {cluster_index} \
                 (refcount table growth not yet implemented)"
            );
        }
        if let Some(off) = self.refcount_block_offset(cluster_index, file)? {
            return Ok(off); // already present
        }

        let cluster_size = self.header.cluster_size();
        let len = file.seek(SeekFrom::End(0))?;
        let block_off = len.next_multiple_of(cluster_size);
        file.set_len(block_off + cluster_size)?; // a fresh, all-zero block
        let block_self_index = block_off / cluster_size;
        // We rely on the new block falling inside its own coverage so it can
        // hold its own refcount; assert it so a wrong call site fails loudly.
        if block_self_index / rb_entries != rt_index {
            bail!(
                "refcount block self-coverage broken: block at cluster \
                 {block_self_index} does not fall in table slot {rt_index}"
            );
        }
        // Publish the block in the refcount table, then record its own refcount.
        file.seek(SeekFrom::Start(
            self.header.refcount_table_offset + rt_index * 8,
        ))?;
        file.write_all(&block_off.to_be_bytes())?;
        self.write_refcount(block_self_index, 1, file)?;
        Ok(block_off)
    }

    /// Allocate a new (zeroed) L2 table for L1 slot `l1_index`: refcount it,
    /// record it in the on-disk L1 table and the in-memory cache, and return its
    /// host offset. The L1 entry is marked COPIED (the L2 table's refcount is 1).
    fn allocate_l2_table(&self, l1_index: u64, file: &mut File) -> Result<u64> {
        let l2_off = self.allocate_host_cluster(file)?;
        let entry = l2_off | QCOW_OFLAG_COPIED;
        file.seek(SeekFrom::Start(self.header.l1_table_offset + l1_index * 8))?;
        file.write_all(&entry.to_be_bytes())?;
        // Keep the in-memory cache coherent with what we just wrote to disk.
        let mut l1 = self
            .l1_table
            .lock()
            .map_err(|e| anyhow::anyhow!("l1 lock: {e}"))?;
        l1[usize_of(l1_index)] = entry;
        drop(l1);
        Ok(l2_off)
    }

    /// Allocate a data cluster for a write that lands in an unallocated (or
    /// zero-flagged) cluster, wiring up the L2 entry and refcounts — allocating
    /// the L2 table first if the L1 slot is still empty. Copy-on-write from the
    /// backing image is honoured so bytes the guest does not overwrite still
    /// read correctly. Returns the host offset of the new data cluster.
    fn allocate_data_cluster(&self, guest_offset: u64, file: &mut File) -> Result<u64> {
        let cluster_size = self.header.cluster_size();
        let l2_entries = self.header.l2_entries();
        let l1_index = guest_offset / (l2_entries * cluster_size);

        // Find the L2 table for this L1 slot, allocating it if absent.
        let existing = {
            let l1 = self
                .l1_table
                .lock()
                .map_err(|e| anyhow::anyhow!("l1 lock: {e}"))?;
            if l1_index >= l1.len() as u64 {
                bail!("guest offset {guest_offset:#x} is beyond the L1 table");
            }
            l1[usize_of(l1_index)] & L2_OFFSET_MASK
        };
        let l2_table_offset = if existing == 0 {
            self.allocate_l2_table(l1_index, file)?
        } else {
            existing
        };
        let l2_index = (guest_offset / cluster_size) % l2_entries;
        let l2_entry_off = l2_table_offset + l2_index * 8;

        // Inspect the current L2 entry to decide whether we copy-on-write.
        file.seek(SeekFrom::Start(l2_entry_off))?;
        let mut b = [0u8; 8];
        file.read_exact(&mut b)?;
        let old = u64::from_be_bytes(b);
        let was_zero = old & QCOW_OFLAG_ZERO != 0;
        let had_data = old & L2_OFFSET_MASK != 0;

        let data_off = self.allocate_host_cluster(file)?;

        // Copy-on-write: a plain unallocated cluster (no zero flag, no prior
        // data) backed by a lower layer must inherit that layer's contents, so
        // bytes outside the guest's write still read through correctly. A
        // zero-flagged cluster reads as zeros and the fresh cluster already is.
        if !was_zero
            && !had_data
            && let Some(backing) = &self.backing
        {
            let cluster_start = guest_offset & !(cluster_size - 1);
            let mut tmp = vec![0u8; usize_of(cluster_size)];
            backing.read_at(cluster_start, &mut tmp)?;
            file.seek(SeekFrom::Start(data_off))?;
            file.write_all(&tmp)?;
        }

        // Point the L2 entry at the new cluster; COPIED because refcount is 1.
        file.seek(SeekFrom::Start(l2_entry_off))?;
        file.write_all(&(data_off | QCOW_OFLAG_COPIED).to_be_bytes())?;
        Ok(data_off)
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

            let dst = &mut buf[total_read..total_read + chunk];
            match self.resolve_cluster(current_offset, &mut file)? {
                ClusterLoc::Mapped(host_offset) => {
                    file.seek(SeekFrom::Start(host_offset))?;
                    file.read_exact(dst)?;
                }
                ClusterLoc::Zero => dst.fill(0),
                ClusterLoc::Unallocated => {
                    // Not in this image: read from the backing image if any
                    // (the overlay mechanism), else zeros. Zero first so a
                    // short backing read leaves the tail zeroed.
                    dst.fill(0);
                    if let Some(backing) = &self.backing {
                        backing.read_at(current_offset, dst)?;
                    }
                }
            }

            total_read += chunk;
            current_offset += chunk as u64;
            remaining -= chunk;
        }

        drop(file);
        Ok(total_read)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        if !self.writable {
            bail!("qcow2 backend opened read-only");
        }
        if offset >= self.header.size {
            return Ok(0);
        }
        let mut file = self.file.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let cluster_size = self.header.cluster_size();
        let writable_len = buf.len().min(usize_of(self.header.size - offset));

        // Pass 1: resolve every target cluster, allocating one (copy-on-write
        // from any backing image) when the write lands in an unallocated or
        // zero-flagged cluster whose L2 table already exists. Allocation that
        // would need a new L2 table or refcount block is reported as an error
        // by the helpers rather than half-applied.
        let mut plan: Vec<(u64, usize, usize)> = Vec::new(); // (host_offset, buf_start, len)
        let mut done = 0usize;
        let mut current_offset = offset;
        while done < writable_len {
            let in_cluster = usize_of(current_offset % cluster_size);
            let chunk = (writable_len - done).min(usize_of(cluster_size) - in_cluster);
            match self.resolve_cluster(current_offset, &mut file)? {
                ClusterLoc::Mapped(host_offset) => plan.push((host_offset, done, chunk)),
                ClusterLoc::Zero | ClusterLoc::Unallocated => {
                    let data_off = self.allocate_data_cluster(current_offset, &mut file)?;
                    plan.push((
                        data_off + (current_offset & (cluster_size - 1)),
                        done,
                        chunk,
                    ));
                }
            }
            done += chunk;
            current_offset += chunk as u64;
        }

        // Pass 2: every cluster is backed — apply the writes.
        for (host_offset, buf_start, len) in plan {
            file.seek(SeekFrom::Start(host_offset))?;
            file.write_all(&buf[buf_start..buf_start + len])?;
        }
        drop(file);
        Ok(writable_len)
    }

    fn flush(&self) -> Result<()> {
        if !self.writable {
            return Ok(()); // read-only, nothing to flush
        }
        let mut file = self.file.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        file.flush()?;
        drop(file);
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.header.size
    }

    fn is_readonly(&self) -> bool {
        !self.writable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::truncate::u8_of;

    /// Build a fully refcounted qcow2 v3 image (`cluster_bits`=16, 1MB virtual):
    /// one guest cluster mapped, with a real refcount table + block so the image
    /// passes [`QcowBackend::check_consistency`]. Cluster layout:
    /// 0 header · 1 refcount table · 2 refcount block · 3 L1 · 4 L2 · 5 data.
    /// `extra_clusters` zero clusters are appended so allocation tests have room
    /// to grow the file in place without the builder caring how.
    fn make_refcounted_qcow2(extra_clusters: usize) -> Vec<u8> {
        let cluster_bits: u32 = 16;
        let cs: usize = 1 << cluster_bits;
        let virtual_size: u64 = 1024 * 1024;

        let reftable_off = cs as u64; // cluster 1
        let refblock_off = 2 * cs as u64; // cluster 2
        let l1_off = 3 * cs as u64; // cluster 3
        let l2_off = 4 * cs as u64; // cluster 4
        let data_off = 5 * cs as u64; // cluster 5
        let used_clusters = 6;

        let mut img = vec![0u8; (used_clusters + extra_clusters) * cs];

        // Header (v3, 104 bytes).
        img[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        img[4..8].copy_from_slice(&3u32.to_be_bytes()); // version 3
        img[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        img[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        img[36..40].copy_from_slice(&1u32.to_be_bytes()); // l1_size = 1
        img[40..48].copy_from_slice(&l1_off.to_be_bytes());
        img[48..56].copy_from_slice(&reftable_off.to_be_bytes());
        img[56..60].copy_from_slice(&1u32.to_be_bytes()); // refcount_table_clusters = 1
        img[96..100].copy_from_slice(&4u32.to_be_bytes()); // refcount_order = 4 (16-bit)
        img[100..104].copy_from_slice(&104u32.to_be_bytes()); // header_length

        // Refcount table: entry 0 → refcount block.
        let rt = usize_of(reftable_off);
        img[rt..rt + 8].copy_from_slice(&refblock_off.to_be_bytes());

        // Refcount block: 16-bit entries; clusters 0..6 each have refcount 1.
        let rb = usize_of(refblock_off);
        for c in 0..used_clusters {
            img[rb + c * 2..rb + c * 2 + 2].copy_from_slice(&1u16.to_be_bytes());
        }

        // L1[0] → L2 table; L2[0] → data cluster (both COPIED, refcount==1).
        let l1 = usize_of(l1_off);
        img[l1..l1 + 8].copy_from_slice(&(l2_off | QCOW_OFLAG_COPIED).to_be_bytes());
        let l2 = usize_of(l2_off);
        img[l2..l2 + 8].copy_from_slice(&(data_off | QCOW_OFLAG_COPIED).to_be_bytes());

        // Data cluster: i&0xFF pattern.
        let d = usize_of(data_off);
        for i in 0..cs {
            img[d + i] = u8_of(i & 0xFF);
        }

        img
    }

    /// A fully refcounted qcow2 v3 overlay that maps *no* data clusters (its L2
    /// table exists but is empty) and names `backing_path`. Reads fall through to
    /// the backing image; a write triggers copy-on-write allocation. Cluster
    /// layout: 0 header (+ backing path) · 1 refcount table · 2 refcount block ·
    /// 3 L1 · 4 L2 (empty). Passes [`QcowBackend::check_consistency`] as built.
    fn make_refcounted_overlay(backing_path: &str) -> Vec<u8> {
        let cluster_bits: u32 = 16;
        let cs: usize = 1 << cluster_bits;
        let virtual_size: u64 = 1024 * 1024;
        let reftable_off = cs as u64;
        let refblock_off = 2 * cs as u64;
        let l1_off = 3 * cs as u64;
        let l2_off = 4 * cs as u64;
        let used_clusters = 5; // no data cluster
        let backing_off: u64 = 0x200; // past the v3 header, inside cluster 0
        let path = backing_path.as_bytes();

        let mut img = vec![0u8; used_clusters * cs];
        img[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        img[4..8].copy_from_slice(&3u32.to_be_bytes());
        img[8..16].copy_from_slice(&backing_off.to_be_bytes());
        img[16..20].copy_from_slice(&u32::try_from(path.len()).unwrap().to_be_bytes());
        img[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        img[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        img[36..40].copy_from_slice(&1u32.to_be_bytes()); // l1_size = 1
        img[40..48].copy_from_slice(&l1_off.to_be_bytes());
        img[48..56].copy_from_slice(&reftable_off.to_be_bytes());
        img[56..60].copy_from_slice(&1u32.to_be_bytes());
        img[96..100].copy_from_slice(&4u32.to_be_bytes());
        img[100..104].copy_from_slice(&104u32.to_be_bytes());
        img[usize_of(backing_off)..usize_of(backing_off) + path.len()].copy_from_slice(path);

        let rt = usize_of(reftable_off);
        img[rt..rt + 8].copy_from_slice(&refblock_off.to_be_bytes());
        let rb = usize_of(refblock_off);
        for c in 0..used_clusters {
            img[rb + c * 2..rb + c * 2 + 2].copy_from_slice(&1u16.to_be_bytes());
        }
        let l1 = usize_of(l1_off);
        img[l1..l1 + 8].copy_from_slice(&(l2_off | QCOW_OFLAG_COPIED).to_be_bytes());
        // L2 table (cluster 4) is left empty: no guest cluster mapped.
        img
    }

    /// A standalone, empty (nothing mapped) refcounted qcow2 v3 image with tiny
    /// 512-byte clusters, so one refcount block covers only 256 host clusters
    /// (128 KiB) — a few hundred KiB of writes crosses that boundary and forces
    /// a *second* refcount block to be allocated. Clusters: 0 header · 1 refcount
    /// table · 2 refcount block · 3 L1 (all empty). 1 MiB virtual (32 L1 slots).
    fn make_small_cluster_qcow2() -> Vec<u8> {
        let cluster_bits: u32 = 9; // 512-byte clusters
        let cs: usize = 1 << cluster_bits;
        let virtual_size: u64 = 1024 * 1024; // 32 L1 slots of 32 KiB each
        let reftable_off = cs as u64; // cluster 1
        let refblock_off = 2 * cs as u64; // cluster 2
        let l1_off = 3 * cs as u64; // cluster 3
        let used_clusters = 4; // header, reftable, refblock, L1

        let mut img = vec![0u8; used_clusters * cs];
        img[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        img[4..8].copy_from_slice(&3u32.to_be_bytes());
        img[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        img[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        img[36..40].copy_from_slice(&32u32.to_be_bytes()); // l1_size = 32
        img[40..48].copy_from_slice(&l1_off.to_be_bytes());
        img[48..56].copy_from_slice(&reftable_off.to_be_bytes());
        img[56..60].copy_from_slice(&1u32.to_be_bytes()); // refcount_table_clusters
        img[96..100].copy_from_slice(&4u32.to_be_bytes()); // refcount_order = 4
        img[100..104].copy_from_slice(&104u32.to_be_bytes());

        let rt = usize_of(reftable_off);
        img[rt..rt + 8].copy_from_slice(&refblock_off.to_be_bytes());
        let rb = usize_of(refblock_off);
        for c in 0..used_clusters {
            img[rb + c * 2..rb + c * 2 + 2].copy_from_slice(&1u16.to_be_bytes());
        }
        // L1 table (cluster 3) left empty: every write allocates L2 + data.
        img
    }

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

    // A qcow2 overlay with NO allocated clusters that references `backing_path`,
    // so every read falls through to the backing image. Cluster 0 holds the
    // header and the backing-path string; cluster 1 holds an L1 table whose
    // single entry is 0 (no L2 allocated).
    fn make_overlay_qcow2(backing_path: &str) -> Vec<u8> {
        let cluster_bits: u32 = 16;
        let cluster_size: usize = 1 << cluster_bits;
        let virtual_size: u64 = 1024 * 1024;
        let l1_offset: u64 = cluster_size as u64;
        let backing_off: u64 = 0x100; // within cluster 0, past the v3 header
        let path = backing_path.as_bytes();

        let mut img = vec![0u8; 2 * cluster_size];
        img[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        img[4..8].copy_from_slice(&2u32.to_be_bytes()); // version
        img[8..16].copy_from_slice(&backing_off.to_be_bytes()); // backing_file_offset
        img[16..20].copy_from_slice(&u32::try_from(path.len()).unwrap().to_be_bytes()); // size
        img[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        img[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        img[36..40].copy_from_slice(&1u32.to_be_bytes()); // l1_size = 1
        img[40..48].copy_from_slice(&l1_offset.to_be_bytes());
        // backing path string
        img[usize_of(backing_off)..usize_of(backing_off) + path.len()].copy_from_slice(path);
        // L1 table entry 0 stays 0 → no L2 → every cluster unallocated here.
        img
    }

    #[test]
    fn reads_fall_through_to_a_qcow2_backing_image() {
        // Base image: cluster 0 carries the i&0xFF pattern; cluster 1 is empty.
        let base = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(base.path(), make_minimal_qcow2()).unwrap();
        // Overlay: nothing allocated, backing = base.
        let overlay = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            overlay.path(),
            make_overlay_qcow2(base.path().to_str().unwrap()),
        )
        .unwrap();

        let backend = QcowBackend::open(overlay.path()).unwrap();
        assert!(backend.is_readonly());
        assert_eq!(backend.capacity(), 1024 * 1024);

        // Cluster 0 is unallocated in the overlay → served from the base's
        // allocated cluster 0 (the pattern).
        let mut buf = vec![0u8; 256];
        backend.read_at(0, &mut buf).unwrap();
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, u8_of(i & 0xFF), "byte {i} comes from the backing image");
        }

        // Cluster 1 is unallocated in BOTH layers → zeros.
        let mut buf2 = vec![0xFFu8; 256];
        backend.read_at(65536, &mut buf2).unwrap();
        assert!(
            buf2.iter().all(|&b| b == 0),
            "absent in base+overlay → zeros"
        );
    }

    #[test]
    fn reads_fall_through_to_a_raw_backing_image() {
        // A qcow2 overlay over a *raw* base file (format detected by magic).
        let base = tempfile::NamedTempFile::new().unwrap();
        let mut raw = vec![0u8; 1024 * 1024];
        for (i, b) in raw.iter_mut().take(256).enumerate() {
            *b = u8_of((i ^ 0x5A) & 0xFF);
        }
        std::fs::write(base.path(), &raw).unwrap();

        let overlay = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            overlay.path(),
            make_overlay_qcow2(base.path().to_str().unwrap()),
        )
        .unwrap();

        let backend = QcowBackend::open(overlay.path()).unwrap();
        let mut buf = vec![0u8; 256];
        backend.read_at(0, &mut buf).unwrap();
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, u8_of((i ^ 0x5A) & 0xFF), "byte {i} from the raw backing");
        }
    }

    #[test]
    fn reads_traverse_a_multi_level_qcow2_chain() {
        // base.qcow2 (cluster 0 = pattern) <- mid.qcow2 (empty) <- top.qcow2.
        let base = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(base.path(), make_minimal_qcow2()).unwrap();
        let mid = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            mid.path(),
            make_overlay_qcow2(base.path().to_str().unwrap()),
        )
        .unwrap();
        let top = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(top.path(), make_overlay_qcow2(mid.path().to_str().unwrap())).unwrap();

        let backend = QcowBackend::open(top.path()).unwrap();
        let mut buf = vec![0u8; 256];
        backend.read_at(0, &mut buf).unwrap();
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, u8_of(i & 0xFF), "byte {i} traverses top->mid->base");
        }
    }

    #[test]
    fn overlay_write_to_allocated_cluster_does_not_touch_backing() {
        // Build an overlay that *has* its own cluster 0 (via make_minimal_qcow2,
        // pattern data) but also names a backing image. A write to that
        // allocated cluster must persist in the overlay, and the backing file's
        // bytes must be untouched.
        let base = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(base.path(), make_minimal_qcow2()).unwrap();
        let base_before = std::fs::read(base.path()).unwrap();

        // An overlay image that allocates cluster 0 itself: reuse the minimal
        // builder, then patch in a backing pointer.
        let mut overlay_img = make_minimal_qcow2();
        let backing_off: u64 = 0x100;
        let path = base.path().to_str().unwrap().as_bytes();
        overlay_img[8..16].copy_from_slice(&backing_off.to_be_bytes());
        overlay_img[16..20].copy_from_slice(&u32::try_from(path.len()).unwrap().to_be_bytes());
        overlay_img[usize_of(backing_off)..usize_of(backing_off) + path.len()]
            .copy_from_slice(path);
        let overlay = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(overlay.path(), &overlay_img).unwrap();

        let backend = QcowBackend::open_rw(overlay.path()).unwrap();
        backend.write_at(0, &[0xC3; 64]).unwrap();
        backend.flush().unwrap();
        let mut buf = [0u8; 64];
        backend.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0xC3; 64], "overlay write persisted");
        // The backing file on disk is byte-for-byte unchanged.
        assert_eq!(std::fs::read(base.path()).unwrap(), base_before);
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
    fn write_rejected_when_opened_read_only() {
        let img = make_minimal_qcow2();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();

        let backend = QcowBackend::open(tmp.path()).unwrap();
        assert!(backend.is_readonly());
        assert!(backend.write_at(0, &[1, 2, 3]).is_err());
    }

    #[test]
    fn write_to_allocated_cluster_round_trips() {
        let img = make_minimal_qcow2();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();

        let backend = QcowBackend::open_rw(tmp.path()).unwrap();
        assert!(!backend.is_readonly());

        // Cluster 0 (guest offset 0) is allocated in the minimal image; an
        // overwrite there persists and reads back.
        let payload: Vec<u8> = (0..512u32).map(|i| u8_of((i ^ 0xA5) as usize)).collect();
        let n = backend.write_at(100, &payload).unwrap();
        assert_eq!(n, payload.len());
        backend.flush().unwrap();

        let mut buf = vec![0u8; payload.len()];
        backend.read_at(100, &mut buf).unwrap();
        assert_eq!(buf, payload, "written bytes read back");

        // Reopening the file proves the bytes hit disk, not just a cache.
        drop(backend);
        let reopened = QcowBackend::open(tmp.path()).unwrap();
        let mut buf2 = vec![0u8; payload.len()];
        reopened.read_at(100, &mut buf2).unwrap();
        assert_eq!(buf2, payload, "written bytes persisted to the image file");
    }

    #[test]
    fn allocates_a_data_cluster_into_an_existing_l2_table() {
        // Guest cluster 1 (offset 64KB) is unmapped but its L2 table exists in
        // the refcounted image — a write there must allocate, persist, and keep
        // the image refcount-consistent.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), make_refcounted_qcow2(0)).unwrap();
        let backend = QcowBackend::open_rw(tmp.path()).unwrap();

        let payload: Vec<u8> = (0..1024u32).map(|i| u8_of((i ^ 0x3C) as usize)).collect();
        let n = backend.write_at(65536, &payload).unwrap();
        assert_eq!(n, payload.len());
        backend.flush().unwrap();

        let mut buf = vec![0u8; payload.len()];
        backend.read_at(65536, &mut buf).unwrap();
        assert_eq!(buf, payload, "freshly allocated cluster reads back");

        backend
            .check_consistency()
            .expect("allocation must keep refcounts consistent");

        // Persisted across reopen, and still consistent.
        drop(backend);
        let reopened = QcowBackend::open(tmp.path()).unwrap();
        let mut buf2 = vec![0u8; payload.len()];
        reopened.read_at(65536, &mut buf2).unwrap();
        assert_eq!(buf2, payload, "allocation persisted to disk");
        reopened.check_consistency().unwrap();
    }

    #[test]
    fn partial_write_to_a_fresh_cluster_zero_fills_the_rest() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), make_refcounted_qcow2(0)).unwrap();
        let backend = QcowBackend::open_rw(tmp.path()).unwrap();

        // Write 32 bytes near the middle of the (unallocated) guest cluster 1.
        backend.write_at(65536 + 1000, &[0x7E; 32]).unwrap();
        backend.flush().unwrap();

        // The written window holds the payload; bytes on either side read zero.
        let mut buf = vec![0xABu8; 64];
        backend.read_at(65536 + 980, &mut buf).unwrap();
        assert!(buf[..20].iter().all(|&b| b == 0), "pre-write bytes zeroed");
        assert!(buf[20..52].iter().all(|&b| b == 0x7E), "payload present");
        assert!(buf[52..].iter().all(|&b| b == 0), "post-write bytes zeroed");
        backend.check_consistency().unwrap();
    }

    #[test]
    fn multiple_allocations_stay_consistent() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), make_refcounted_qcow2(0)).unwrap();
        let backend = QcowBackend::open_rw(tmp.path()).unwrap();

        // Allocate guest clusters 1, 2 and 3 (all share the one existing L2).
        for c in 1..=3u64 {
            backend.write_at(c * 65536, &[u8_of(c); 256]).unwrap();
        }
        backend.flush().unwrap();
        backend.check_consistency().unwrap();

        for c in 1..=3u64 {
            let mut buf = [0u8; 256];
            backend.read_at(c * 65536, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == u8_of(c)), "cluster {c}");
        }
    }

    #[test]
    fn allocates_a_new_l2_table_when_the_l1_slot_is_empty() {
        // Widen the refcounted image to two L1 entries (1 GiB virtual); L1[1] is
        // still empty, so a write into its range must allocate the L2 table too.
        let mut img = make_refcounted_qcow2(0);
        img[36..40].copy_from_slice(&2u32.to_be_bytes()); // l1_size = 2
        img[24..32].copy_from_slice(&(1024u64 * 1024 * 1024).to_be_bytes()); // 1 GiB
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();
        let backend = QcowBackend::open_rw(tmp.path()).unwrap();

        // 512 MiB == the first guest byte covered by L1 entry 1.
        let off = 8192u64 * 65536;
        let payload: Vec<u8> = (0..2048u32).map(|i| u8_of((i ^ 0x99) as usize)).collect();
        backend.write_at(off, &payload).unwrap();
        backend.flush().unwrap();

        let mut buf = vec![0u8; payload.len()];
        backend.read_at(off, &mut buf).unwrap();
        assert_eq!(buf, payload, "data through a freshly allocated L2 table");
        backend
            .check_consistency()
            .expect("L2-table allocation must keep refcounts consistent");

        // Persisted: reopen and confirm both data and consistency.
        drop(backend);
        let reopened = QcowBackend::open(tmp.path()).unwrap();
        let mut buf2 = vec![0u8; payload.len()];
        reopened.read_at(off, &mut buf2).unwrap();
        assert_eq!(buf2, payload, "L2 table + data persisted");
        reopened.check_consistency().unwrap();
    }

    #[test]
    fn allocating_over_a_backing_image_copies_on_write() {
        // Base holds the i&0xFF pattern in guest cluster 0. The overlay maps
        // nothing and names the base. A partial write to cluster 0 must allocate
        // a cluster, pull the rest of the cluster in from the base (so untouched
        // bytes still read the base pattern), and leave the base untouched.
        let base = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(base.path(), make_minimal_qcow2()).unwrap();
        let base_before = std::fs::read(base.path()).unwrap();

        let overlay = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            overlay.path(),
            make_refcounted_overlay(base.path().to_str().unwrap()),
        )
        .unwrap();

        let backend = QcowBackend::open_rw(overlay.path()).unwrap();
        // Before writing, cluster 0 reads straight from the base.
        let mut pre = vec![0u8; 256];
        backend.read_at(0, &mut pre).unwrap();
        for (i, &b) in pre.iter().enumerate() {
            assert_eq!(b, u8_of(i & 0xFF), "pre-write read falls through to base");
        }

        // Overwrite the first 64 bytes only.
        backend.write_at(0, &[0xD7; 64]).unwrap();
        backend.flush().unwrap();

        let mut post = vec![0u8; 256];
        backend.read_at(0, &mut post).unwrap();
        assert!(post[..64].iter().all(|&b| b == 0xD7), "written window");
        for (i, &b) in post.iter().enumerate().skip(64) {
            assert_eq!(b, u8_of(i & 0xFF), "byte {i} copied-on-write from base");
        }

        backend
            .check_consistency()
            .expect("copy-on-write allocation stays consistent");
        // The base image on disk is byte-for-byte unchanged.
        assert_eq!(std::fs::read(base.path()).unwrap(), base_before);
    }

    #[test]
    fn refcounted_overlay_is_consistent_as_built() {
        let base = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(base.path(), make_minimal_qcow2()).unwrap();
        let overlay = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            overlay.path(),
            make_refcounted_overlay(base.path().to_str().unwrap()),
        )
        .unwrap();
        QcowBackend::open(overlay.path())
            .unwrap()
            .check_consistency()
            .expect("empty refcounted overlay must be consistent");
    }

    #[test]
    fn growing_past_one_refcount_block_allocates_another() {
        // 512-byte clusters → one refcount block covers 256 clusters (128 KiB).
        // A ~200 KiB write crosses that, forcing a second refcount block.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), make_small_cluster_qcow2()).unwrap();
        let backend = QcowBackend::open_rw(tmp.path()).unwrap();

        let payload: Vec<u8> = (0..204_800u32).map(|i| u8_of((i % 251) as usize)).collect();
        backend.write_at(0, &payload).unwrap();
        backend.flush().unwrap();

        // Data reads back, and the image stays refcount-consistent through the
        // newly allocated block(s) — the qemu-img-check substitute proving the
        // self-referencing block was refcounted correctly.
        let mut buf = vec![0u8; payload.len()];
        backend.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, payload, "data spanning multiple refcount blocks");
        backend.check_consistency().unwrap();
        drop(backend);

        // Prove a second refcount block really was allocated: the file grew past
        // one block's 128 KiB reach and refcount-table entry 1 is now populated.
        let raw = std::fs::read(tmp.path()).unwrap();
        assert!(raw.len() > 256 * 512, "file crossed one block's coverage");
        let rt_entry1 = u64::from_be_bytes(raw[512 + 8..512 + 16].try_into().unwrap());
        assert_ne!(
            rt_entry1 & REFT_OFFSET_MASK,
            0,
            "second refcount block allocated"
        );

        // Reopen and re-check, proving it all hit disk consistently.
        let reopened = QcowBackend::open(tmp.path()).unwrap();
        reopened.check_consistency().unwrap();
    }

    #[test]
    fn write_spanning_into_unallocated_cluster_is_rejected_atomically() {
        let img = make_minimal_qcow2();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();

        let backend = QcowBackend::open_rw(tmp.path()).unwrap();
        // Cluster 0 (0..64KB) is allocated; cluster 1 (64KB..) is not. The
        // minimal image carries no refcount table, so cluster 1 cannot be
        // allocated — the straddling write must fail without partially applying,
        // leaving the allocated half untouched (allocation pre-flights the
        // refcount slot before growing the file).
        let cluster_size = 1usize << 16;
        let start = cluster_size - 8;
        let payload = [0xEEu8; 16];
        assert!(backend.write_at(start as u64, &payload).is_err());

        let mut buf = [0xFFu8; 16];
        backend.read_at(start as u64, &mut buf).unwrap();
        // Original cluster-0 data is the i&0xFF pattern; the last 8 bytes of
        // cluster 0 are bytes (cluster_size-8 .. cluster_size).
        for (k, b) in buf.iter().take(8).enumerate() {
            assert_eq!(*b, u8_of((start + k) & 0xFF), "allocated half untouched");
        }
    }

    #[test]
    fn invalid_magic_rejected() {
        let mut img = make_minimal_qcow2();
        img[0] = 0; // corrupt magic
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();
        assert!(QcowBackend::open(tmp.path()).is_err());
    }

    #[test]
    fn create_makes_a_consistent_standalone_image() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        QcowBackend::create(tmp.path(), 4 * 1024 * 1024, None).unwrap();

        let backend = QcowBackend::open_rw(tmp.path()).unwrap();
        assert_eq!(backend.capacity(), 4 * 1024 * 1024);
        backend
            .check_consistency()
            .expect("a freshly created image must be consistent");

        // Unwritten regions read as zeros; a write allocates and round-trips.
        let mut zeros = vec![0xFFu8; 256];
        backend.read_at(0, &mut zeros).unwrap();
        assert!(zeros.iter().all(|&b| b == 0), "fresh image reads zero");

        let payload: Vec<u8> = (0..4096u32).map(|i| u8_of((i ^ 0x2D) as usize)).collect();
        backend.write_at(123_456, &payload).unwrap();
        backend.flush().unwrap();
        let mut buf = vec![0u8; payload.len()];
        backend.read_at(123_456, &mut buf).unwrap();
        assert_eq!(buf, payload, "write into a created image round-trips");
        backend.check_consistency().unwrap();
    }

    #[test]
    fn create_overlay_reads_through_and_writes_copy_on_write() {
        // Read-only base with a known pattern in guest cluster 0.
        let base = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(base.path(), make_minimal_qcow2()).unwrap();
        let base_before = std::fs::read(base.path()).unwrap();

        // Create an overlay over it — the non-destructive-test primitive.
        let overlay = tempfile::NamedTempFile::new().unwrap();
        QcowBackend::create(overlay.path(), 1024 * 1024, Some(base.path())).unwrap();
        QcowBackend::open(overlay.path())
            .unwrap()
            .check_consistency()
            .expect("created overlay must be consistent");

        let backend = QcowBackend::open_rw(overlay.path()).unwrap();
        // Reads fall through to the base.
        let mut buf = vec![0u8; 256];
        backend.read_at(0, &mut buf).unwrap();
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, u8_of(i & 0xFF), "byte {i} read through to base");
        }
        // A write copies-on-write into the overlay, base untouched.
        backend.write_at(0, &[0x5C; 32]).unwrap();
        backend.flush().unwrap();
        let mut after = vec![0u8; 256];
        backend.read_at(0, &mut after).unwrap();
        assert!(after[..32].iter().all(|&b| b == 0x5C), "overlay write");
        for (i, &b) in after.iter().enumerate().skip(32) {
            assert_eq!(b, u8_of(i & 0xFF), "byte {i} still from base");
        }
        backend.check_consistency().unwrap();
        assert_eq!(
            std::fs::read(base.path()).unwrap(),
            base_before,
            "base untouched"
        );
    }

    #[test]
    fn parses_v3_refcount_order() {
        let img = make_refcounted_qcow2(0);
        let header = QcowHeader::from_bytes(&img).unwrap();
        assert_eq!(header.version, 3);
        assert_eq!(header.refcount_order, 4);
        assert_eq!(header.refcount_bits(), 16);
        assert_eq!(header.refcount_block_entries(), 65536 * 8 / 16);
        assert_eq!(header.refcount_table_entries(), 65536 / 8);
    }

    #[test]
    fn refcounted_image_passes_consistency_check() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), make_refcounted_qcow2(0)).unwrap();
        let backend = QcowBackend::open(tmp.path()).unwrap();

        // Reads still work through the refcounted layout.
        let mut buf = vec![0u8; 256];
        backend.read_at(0, &mut buf).unwrap();
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, u8_of(i & 0xFF), "byte {i}");
        }

        backend
            .check_consistency()
            .expect("hand-built image must be internally consistent");
    }

    #[test]
    fn read_refcount_reports_used_and_free_clusters() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), make_refcounted_qcow2(0)).unwrap();
        let backend = QcowBackend::open(tmp.path()).unwrap();
        let mut file = backend.file.lock().unwrap();
        // Clusters 0..6 (header, reftable, refblock, L1, L2, data) are used.
        for c in 0..6 {
            assert_eq!(
                backend.read_refcount(c, &mut file).unwrap(),
                1,
                "cluster {c}"
            );
        }
        // Cluster 6 onward has no reference (still inside the covered block range).
        assert_eq!(backend.read_refcount(6, &mut file).unwrap(), 0);
        assert_eq!(backend.read_refcount(100, &mut file).unwrap(), 0);
    }

    #[test]
    fn consistency_check_detects_a_refcount_leak() {
        // Bump the data cluster's stored refcount to 2 while only one reference
        // exists — qemu-img would call this a leak; so must we.
        let mut img = make_refcounted_qcow2(0);
        let cs = 1usize << 16;
        let refblock = 2 * cs;
        let data_cluster_index = 5;
        img[refblock + data_cluster_index * 2..refblock + data_cluster_index * 2 + 2]
            .copy_from_slice(&2u16.to_be_bytes());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();
        let backend = QcowBackend::open(tmp.path()).unwrap();
        let err = backend.check_consistency().unwrap_err();
        assert!(
            err.to_string().contains("cluster 5"),
            "leak should be reported at cluster 5, got: {err}"
        );
    }

    #[test]
    fn consistency_check_detects_an_undercount() {
        // Drop a live cluster's refcount to 0 — the dangerous case: a later
        // allocation could hand out a cluster that is still in use.
        let mut img = make_refcounted_qcow2(0);
        let cs = 1usize << 16;
        let refblock = 2 * cs;
        let l2_cluster_index = 4;
        img[refblock + l2_cluster_index * 2..refblock + l2_cluster_index * 2 + 2]
            .copy_from_slice(&0u16.to_be_bytes());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &img).unwrap();
        let backend = QcowBackend::open(tmp.path()).unwrap();
        let err = backend.check_consistency().unwrap_err();
        assert!(
            err.to_string().contains("cluster 4"),
            "undercount should be reported at cluster 4, got: {err}"
        );
    }
}
