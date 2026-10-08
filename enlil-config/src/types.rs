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
    /// USB peripheral routing (`[usb]`).
    #[serde(default, skip_serializing_if = "UsbConfig::is_empty")]
    pub usb: UsbConfig,
}

/// USB peripheral-routing configuration (`[usb]`).
///
/// A default guest plus an ordered list of match→guest rules the routing engine
/// applies to each physical device (Phase 4.3).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsbConfig {
    /// Guest that receives any device matching no rule (`None` = unassigned,
    /// i.e. the device stays with the hypervisor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_guest: Option<String>,
    /// Ordered routing rules (`[[usb.routing]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routing: Vec<UsbRoutingRule>,
    /// VFIO whole-controller passthrough fallback (`[usb.vfio_fallback]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vfio_fallback: Option<UsbVfioFallback>,
}

/// VFIO whole-controller USB fallback (`[usb.vfio_fallback]`, Phase 4.4).
///
/// The escape hatch when per-device forwarding cannot handle a device: the
/// host xHCI controller itself is passed through to one guest via the IOMMU.
/// Disabled by default; when enabled, the controller address must be a valid
/// PCI BDF and the target must name a defined guest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsbVfioFallback {
    /// Master switch — the fallback is only recommended/attempted when true.
    #[serde(default)]
    pub enabled: bool,
    /// Host xHCI controller PCI address (`"dddd:bb:dd.f"`).
    #[serde(default)]
    pub bdf: String,
    /// Guest that receives the whole controller.
    #[serde(default)]
    pub target_guest: String,
    /// Proceed even when the controller's FLR is unadvertised or on the
    /// known-unstable list. This is the operator explicitly accepting that a
    /// bad reset can hang the controller and take the bus down with it.
    #[serde(default)]
    pub allow_unstable_flr: bool,
}

impl UsbConfig {
    /// Whether no USB routing is configured (used to omit `[usb]` from written
    /// TOML so a config with no USB policy stays clean).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.default_guest.is_none() && self.routing.is_empty() && self.vfio_fallback.is_none()
    }
}

/// A single `[[usb.routing]]` rule: a device match spec, a target guest, and a
/// priority (lower is evaluated first).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbRoutingRule {
    /// Match spec: `VVVV:PPPP` (VID:PID), `VVVV:*` (vendor only), `port:<path>`,
    /// `serial:<s>`, `class:<name>`, or `*` (any). Parsed by
    /// [`parse_usb_match`].
    #[serde(rename = "match")]
    pub match_spec: String,
    /// Target guest ID — must name a defined `[guest.*]`.
    pub target: String,
    /// Priority — lower numbers are evaluated first. Defaults to the lowest
    /// priority so an unprioritized rule sits behind explicit ones.
    #[serde(default = "default_usb_priority")]
    pub priority: u32,
}

impl UsbRoutingRule {
    /// Parse this rule's [`match_spec`](Self::match_spec) into a structured
    /// [`UsbMatchKind`].
    ///
    /// # Errors
    /// Propagates [`parse_usb_match`]'s error message for a malformed spec.
    pub fn parsed_match(&self) -> Result<UsbMatchKind, String> {
        parse_usb_match(&self.match_spec)
    }
}

const fn default_usb_priority() -> u32 {
    1000
}

/// A parsed USB device match criterion.
///
/// The config-layer mirror of the device crate's `DeviceMatcher`. The run loop
/// maps this into that enum when it builds the routing table (kept here so
/// `enlil-config` stays independent of `enlil-devices`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbMatchKind {
    /// Match a specific vendor + product ID.
    VidPid { vendor_id: u16, product_id: u16 },
    /// Match any product from a vendor.
    VendorOnly { vendor_id: u16 },
    /// Match a physical port path (e.g. `1-1`, `2-3.1`).
    PortPath(String),
    /// Match a device serial number.
    Serial(String),
    /// Match a USB device-class token (e.g. `hid`), validated against the device
    /// crate's class set when the routing table is built.
    DeviceClass(String),
    /// Match every device (default routing).
    Any,
}

/// Parse a `[[usb.routing]]` match spec into a [`UsbMatchKind`].
///
/// Accepted forms: `*` (any), `VVVV:PPPP` (hex VID:PID), `VVVV:*` (vendor only),
/// `port:<path>`, `serial:<value>`, `class:<name>`.
///
/// # Errors
/// Returns a human-readable message when the spec is not a recognized form, or
/// carries a malformed VID/PID or an empty `port:`/`serial:`/`class:` selector.
pub fn parse_usb_match(spec: &str) -> Result<UsbMatchKind, String> {
    let spec = spec.trim();
    if spec == "*" {
        return Ok(UsbMatchKind::Any);
    }
    if let Some(path) = spec.strip_prefix("port:") {
        let path = path.trim();
        if path.is_empty() {
            return Err("port: match needs a non-empty path".into());
        }
        return Ok(UsbMatchKind::PortPath(path.to_string()));
    }
    if let Some(serial) = spec.strip_prefix("serial:") {
        let serial = serial.trim();
        if serial.is_empty() {
            return Err("serial: match needs a non-empty value".into());
        }
        return Ok(UsbMatchKind::Serial(serial.to_string()));
    }
    if let Some(class) = spec.strip_prefix("class:") {
        let class = class.trim();
        if class.is_empty() {
            return Err("class: match needs a non-empty class name".into());
        }
        return Ok(UsbMatchKind::DeviceClass(class.to_string()));
    }
    if let Some((vid, pid)) = spec.split_once(':') {
        let vendor_id = u16::from_str_radix(vid.trim(), 16)
            .map_err(|_| format!("invalid vendor id '{vid}' (expected up to 4 hex digits)"))?;
        if pid.trim() == "*" {
            return Ok(UsbMatchKind::VendorOnly { vendor_id });
        }
        let product_id = u16::from_str_radix(pid.trim(), 16).map_err(|_| {
            format!("invalid product id '{pid}' (expected up to 4 hex digits or *)")
        })?;
        return Ok(UsbMatchKind::VidPid {
            vendor_id,
            product_id,
        });
    }
    Err(format!("unrecognized USB match spec '{spec}'"))
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

const fn default_reserved_memory() -> u64 {
    512
}
fn default_log_level() -> String {
    "info".into()
}
const fn default_mgmt_port() -> u16 {
    9100
}

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
    /// Optional guest NIC MAC address, six `:`/`-`-separated hex octets (e.g.
    /// `de:ad:be:ef:00:01`). Validated for transparency: a malformed, multicast,
    /// or virtualization-vendor-OUI address is rejected (Phase 5.8). Leave unset
    /// to let the hypervisor synthesize a safe locally-administered address.
    #[serde(default)]
    pub mac: Option<String>,
    /// Virtual UEFI firmware (OVMF) for this guest (`[guest.<id>.firmware]`).
    /// When set, the guest boots through its own virtual firmware instead of
    /// direct kernel boot; `scripts/make-guest-esp.sh` stages the images onto
    /// the stick's ESP at `EFI/enlil/firmware/<id>/`. Leave unset for direct
    /// kernel/initrd boot.
    #[serde(default)]
    pub firmware: Option<FirmwareConfig>,
}

/// Virtual UEFI firmware images for one guest (`[guest.<id>.firmware]`).
///
/// Guests get their own virtual UEFI (per the README's USB-stick model): the
/// stick carries an OVMF CODE + VARS pair per guest, and the hypervisor maps
/// them as the guest's firmware flash at launch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareConfig {
    /// Path to the read-only OVMF CODE image (e.g. `OVMF_CODE.fd`). May be
    /// shared between guests: code flash is never written at runtime.
    pub code: PathBuf,
    /// Path to the OVMF VARS template (`OVMF_VARS.fd`). The layout script
    /// copies this to a per-guest `OVMF_VARS.fd` on the ESP, and the launcher
    /// copies that template to a writable per-boot vars file: the running
    /// variable store is writable, so it must never be the same file for two
    /// guests or for the code image.
    pub vars: PathBuf,
}

fn default_cmdline() -> String {
    "console=ttyS0".into()
}

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

const fn default_true() -> bool {
    true
}
fn default_serial_output() -> String {
    "stdout".into()
}
