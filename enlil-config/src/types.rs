use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Top-level Enlil configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnlilConfig {
    /// Hypervisor-level settings.
    #[serde(default)]
    pub hypervisor: HypervisorConfig,
    /// Guest VM definitions, keyed by ID.
    #[serde(default)]
    pub guest: HashMap<String, GuestConfig>,
}

/// Hypervisor-wide configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HypervisorConfig {
    /// Memory reserved for the hypervisor itself (MB).
    #[serde(default = "default_reserved_memory")]
    pub reserved_memory_mb: u64,
    /// Total host memory available (MB). If 0, auto-detect.
    #[serde(default)]
    pub total_memory_mb: u64,
    /// Log level: trace, debug, info, warn, error.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Management console port.
    #[serde(default = "default_mgmt_port")]
    pub management_port: u16,
}

impl Default for HypervisorConfig {
    fn default() -> Self {
        Self {
            reserved_memory_mb: default_reserved_memory(),
            total_memory_mb: 0,
            log_level: default_log_level(),
            management_port: default_mgmt_port(),
        }
    }
}

const fn default_reserved_memory() -> u64 { 512 }
fn default_log_level() -> String { "info".into() }
const fn default_mgmt_port() -> u16 { 9100 }

/// Configuration for a single guest VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestConfig {
    /// Human-readable name.
    pub name: String,
    /// Physical CPU cores to pin vCPUs to.
    pub cpus: Vec<u32>,
    /// Guest memory in megabytes.
    pub memory_mb: u64,
    /// Path to kernel image (bzImage for Linux).
    pub kernel: Option<PathBuf>,
    /// Path to initrd/initramfs.
    pub initrd: Option<PathBuf>,
    /// Kernel command line.
    #[serde(default = "default_cmdline")]
    pub cmdline: String,
    /// CPU scheduling mode.
    #[serde(default)]
    pub scheduling: SchedulingMode,
    /// Block devices.
    #[serde(default)]
    pub disks: Vec<DiskConfig>,
    /// Serial console configuration.
    #[serde(default)]
    pub serial: SerialPortConfig,
}

fn default_cmdline() -> String { "console=ttyS0".into() }

/// CPU scheduling strategy.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SchedulingMode {
    Dedicated,
    Timeslice,
    #[default]
    Auto,
}

/// Block device configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskConfig {
    /// Path to disk image or raw device.
    pub path: PathBuf,
    /// Mount read-only.
    #[serde(default)]
    pub readonly: bool,
}

/// Serial port configuration for a guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerialPortConfig {
    /// Whether serial console is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Output mode: "stdout", "pty", "file:<path>", "null".
    #[serde(default = "default_serial_output")]
    pub output: String,
}

impl Default for SerialPortConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            output: default_serial_output(),
        }
    }
}

const fn default_true() -> bool { true }
fn default_serial_output() -> String { "stdout".into() }
