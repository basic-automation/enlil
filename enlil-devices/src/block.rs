//! Block device backends.
//!
//! Provides VirtIO-blk emulation for guest disk access.
//! Supports raw disk images and (future) qcow2.

use std::path::PathBuf;

/// Configuration for a block device backend.
#[derive(Debug, Clone)]
pub struct BlockDeviceConfig {
    /// Path to the disk image or raw device.
    pub path: PathBuf,
    /// Whether the device is read-only.
    pub readonly: bool,
    /// Logical sector size in bytes (typically 512).
    pub sector_size: u32,
}

impl BlockDeviceConfig {
    pub fn new(path: PathBuf, readonly: bool) -> Self {
        Self {
            path,
            readonly,
            sector_size: 512,
        }
    }
}

/// A block device backend that serves I/O requests.
pub struct BlockDevice {
    config: BlockDeviceConfig,
    size_bytes: u64,
}

impl BlockDevice {
    /// Create a new block device from config.
    /// Does NOT open the file yet — call `open()` to start serving.
    pub fn new(config: BlockDeviceConfig) -> Self {
        Self {
            config,
            size_bytes: 0,
        }
    }

    pub fn config(&self) -> &BlockDeviceConfig {
        &self.config
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub fn sector_count(&self) -> u64 {
        self.size_bytes / self.config.sector_size as u64
    }
}
