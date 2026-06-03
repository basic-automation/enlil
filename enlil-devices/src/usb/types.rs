//! Common USB types shared across the USB subsystem.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Device speed
// ---------------------------------------------------------------------------

/// USB device speed classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceSpeed {
    /// Low speed (1.5 Mbps) — keyboards, mice.
    Low,
    /// Full speed (12 Mbps) — USB 1.1.
    Full,
    /// High speed (480 Mbps) — USB 2.0.
    High,
    /// `SuperSpeed` (5 Gbps) — USB 3.0.
    Super,
    /// `SuperSpeed`+ (10 Gbps) — USB 3.1 Gen 2.
    SuperPlus,
}

impl fmt::Display for DeviceSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Low => write!(f, "Low Speed"),
            Self::Full => write!(f, "Full Speed"),
            Self::High => write!(f, "High Speed"),
            Self::Super => write!(f, "Super Speed"),
            Self::SuperPlus => write!(f, "Super Speed+"),
        }
    }
}

/// Convenience alias used by the routing subsystem.
pub type UsbSpeed = DeviceSpeed;

// ---------------------------------------------------------------------------
// Device class
// ---------------------------------------------------------------------------

/// USB device class codes (`bDeviceClass`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UsbDeviceClass {
    /// Device class defined at interface level.
    PerInterface,
    /// Audio device.
    Audio,
    /// Communications / CDC Control.
    Cdc,
    /// Human Interface Device (keyboard, mouse, gamepad).
    Hid,
    /// Physical device.
    Physical,
    /// Image device (scanner, camera).
    Image,
    /// Printer.
    Printer,
    /// Mass storage.
    MassStorage,
    /// USB hub.
    Hub,
    /// Vendor-specific.
    VendorSpecific,
    /// Other / unknown class code.
    Other(u8),
}

impl UsbDeviceClass {
    /// Create from a USB class code byte.
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0x00 => Self::PerInterface,
            0x01 => Self::Audio,
            0x02 => Self::Cdc,
            0x03 => Self::Hid,
            0x05 => Self::Physical,
            0x06 => Self::Image,
            0x07 => Self::Printer,
            0x08 => Self::MassStorage,
            0x09 => Self::Hub,
            0xFF => Self::VendorSpecific,
            other => Self::Other(other),
        }
    }

    /// Return the numeric USB class code.
    #[must_use]
    pub const fn code(&self) -> u8 {
        match self {
            Self::PerInterface => 0x00,
            Self::Audio => 0x01,
            Self::Cdc => 0x02,
            Self::Hid => 0x03,
            Self::Physical => 0x05,
            Self::Image => 0x06,
            Self::Printer => 0x07,
            Self::MassStorage => 0x08,
            Self::Hub => 0x09,
            Self::VendorSpecific => 0xFF,
            Self::Other(c) => *c,
        }
    }
}

impl fmt::Display for UsbDeviceClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PerInterface => write!(f, "Per-Interface"),
            Self::Audio => write!(f, "Audio"),
            Self::Cdc => write!(f, "CDC"),
            Self::Hid => write!(f, "HID"),
            Self::Physical => write!(f, "Physical"),
            Self::Image => write!(f, "Image"),
            Self::Printer => write!(f, "Printer"),
            Self::MassStorage => write!(f, "Mass Storage"),
            Self::Hub => write!(f, "Hub"),
            Self::VendorSpecific => write!(f, "Vendor Specific"),
            Self::Other(c) => write!(f, "Other(0x{c:02x})"),
        }
    }
}

// ---------------------------------------------------------------------------
// Interface descriptor
// ---------------------------------------------------------------------------

/// USB interface descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbInterfaceDescriptor {
    /// Interface number.
    pub interface_number: u8,
    /// Alternate setting.
    pub alternate_setting: u8,
    /// Interface class.
    pub interface_class: UsbDeviceClass,
    /// Interface subclass.
    pub interface_subclass: u8,
    /// Interface protocol.
    pub interface_protocol: u8,
    /// Number of endpoints.
    pub num_endpoints: u8,
    /// Interface string descriptor.
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Device descriptor
// ---------------------------------------------------------------------------

/// USB device descriptor — the standard 18-byte descriptor plus string fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbDeviceDescriptor {
    /// Vendor ID.
    pub vendor_id: u16,
    /// Product ID.
    pub product_id: u16,
    /// Device class.
    pub device_class: UsbDeviceClass,
    /// Device subclass.
    pub device_subclass: u8,
    /// Device protocol.
    pub device_protocol: u8,
    /// Manufacturer string (from string descriptor).
    pub manufacturer: Option<String>,
    /// Product string (from string descriptor).
    pub product: Option<String>,
    /// Serial number string (from string descriptor).
    pub serial_number: Option<String>,
    /// Maximum packet size for endpoint 0.
    pub max_packet_size_0: u8,
    /// Number of configurations.
    pub num_configurations: u8,
    /// USB specification version (major, minor).
    pub usb_version: (u8, u8),
    /// Device release version (major, minor).
    pub device_version: (u8, u8),
}

// ---------------------------------------------------------------------------
// Device identity
// ---------------------------------------------------------------------------

/// Uniquely identifies a USB device for routing purposes.
///
/// This is the lightweight identity struct used by the routing engine to
/// match devices against rules.  It carries enough context for every
/// [`DeviceMatcher`](super::routing::DeviceMatcher) variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbDeviceId {
    /// Vendor ID.
    pub vendor_id: u16,
    /// Product ID.
    pub product_id: u16,
    /// Serial number (if available).
    pub serial: Option<String>,
    /// Physical port path string (e.g., "1-2.3").
    pub port_path: Option<String>,
    /// Device class.
    pub class: UsbDeviceClass,
    /// Negotiated speed.
    pub speed: DeviceSpeed,
    /// Manufacturer string (if available).
    pub manufacturer: Option<String>,
    /// Product name string (if available).
    pub product: Option<String>,
}

// ---------------------------------------------------------------------------
// Port path
// ---------------------------------------------------------------------------

/// Physical USB port path (bus number + hub chain).
///
/// For example, a device on bus 1, port 2, hub port 3 would be "1-2.3".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbPortPath {
    /// Root bus number.
    pub bus: u8,
    /// Port chain (e.g., `[2, 3]` for port 2, hub port 3).
    pub ports: Vec<u8>,
}

impl UsbPortPath {
    /// Create a new port path.
    #[must_use]
    pub const fn new(bus: u8, ports: Vec<u8>) -> Self {
        Self { bus, ports }
    }
}

impl fmt::Display for UsbPortPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.bus)?;
        for (i, port) in self.ports.iter().enumerate() {
            if i == 0 {
                write!(f, "-{port}")?;
            } else {
                write!(f, ".{port}")?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Device address
// ---------------------------------------------------------------------------

/// A USB device address (1–127).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbAddress(u8);

impl UsbAddress {
    /// Create a new USB address.
    #[must_use]
    pub const fn new(addr: u8) -> Self {
        Self(addr)
    }

    /// Get the raw address value.
    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl fmt::Display for UsbAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Device state
// ---------------------------------------------------------------------------

/// USB device state in the routing lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UsbDeviceState {
    /// Device has been detected but not yet addressed.
    Attached,
    /// Device has been assigned an address.
    Addressed,
    /// Device has been configured and is operational.
    Configured,
    /// Device is suspended.
    Suspended,
}

impl fmt::Display for UsbDeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Attached => write!(f, "Attached"),
            Self::Addressed => write!(f, "Addressed"),
            Self::Configured => write!(f, "Configured"),
            Self::Suspended => write!(f, "Suspended"),
        }
    }
}

// ---------------------------------------------------------------------------
// Device info (enumeration result)
// ---------------------------------------------------------------------------

/// Complete information about an enumerated USB device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbDeviceInfo {
    /// Assigned device address.
    pub address: UsbAddress,
    /// Physical port path.
    pub port_path: UsbPortPath,
    /// Device descriptor.
    pub descriptor: UsbDeviceDescriptor,
    /// Negotiated speed.
    pub speed: DeviceSpeed,
    /// Current state.
    pub state: UsbDeviceState,
    /// Interface descriptors for each active interface.
    pub interfaces: Vec<UsbInterfaceDescriptor>,
}

// ---------------------------------------------------------------------------
// Guest ID
// ---------------------------------------------------------------------------

/// Identifies a guest VM for routing purposes.
pub type GuestId = String;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// USB subsystem errors.
#[derive(Debug, Error)]
pub enum UsbError {
    /// All 127 device addresses are in use.
    #[error("USB address space exhausted (127 devices)")]
    AddressExhausted,

    /// The specified device was not found.
    #[error("device not found")]
    DeviceNotFound,

    /// The device is already assigned to a guest.
    #[error("device already assigned to guest '{0}'")]
    AlreadyAssigned(String),

    /// The device is not assigned to any guest.
    #[error("device is not assigned")]
    NotAssigned,

    /// A routing rule was not found.
    #[error("routing rule not found: {0}")]
    RuleNotFound(u32),

    /// Controller error.
    #[error("xHCI controller error: {0}")]
    Controller(String),

    /// General I/O error.
    #[error("USB I/O error: {0}")]
    Io(String),
}

/// Convenience result type for USB operations.
pub type UsbResult<T> = Result<T, UsbError>;
