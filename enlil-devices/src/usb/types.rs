//! Core USB types shared across the USB subsystem.
//!
//! Defines device identifiers, descriptors, transfer types, endpoint
//! representations, and address types used by enumeration, routing,
//! and the virtual xHCI controller.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Guest identification
// ---------------------------------------------------------------------------

/// Guest VM identifier used by the routing engine.
pub type GuestId = String;

// ---------------------------------------------------------------------------
// USB device address
// ---------------------------------------------------------------------------

/// A USB device address (1–127) assigned during enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbAddress(u8);

impl UsbAddress {
    /// Create a new USB address.
    #[must_use]
    pub const fn new(addr: u8) -> Self {
        Self(addr)
    }

    /// Return the raw address value.
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
// Device identification (rich — used by routing engine)
// ---------------------------------------------------------------------------

/// USB Vendor ID.
pub type VendorId = u16;

/// USB Product ID.
pub type ProductId = u16;

/// A device identity used by the routing engine for rule matching.
///
/// Contains all the fields a routing rule might want to match on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbDeviceId {
    /// Vendor ID.
    pub vendor_id: u16,
    /// Product ID.
    pub product_id: u16,
    /// Device class.
    pub class: UsbDeviceClass,
    /// Connection speed.
    pub speed: UsbSpeed,
    /// Serial number string (if available).
    pub serial: Option<String>,
    /// Manufacturer string (if available).
    pub manufacturer: Option<String>,
    /// Product name string (if available).
    pub product: Option<String>,
    /// Physical port path (if available).
    pub port_path: Option<String>,
}

impl UsbDeviceId {
    /// Create a minimal device ID from vendor and product.
    #[must_use]
    pub fn new(vendor_id: u16, product_id: u16) -> Self {
        Self {
            vendor_id,
            product_id,
            class: UsbDeviceClass::PerInterface,
            speed: UsbSpeed::Full,
            serial: None,
            manufacturer: None,
            product: None,
            port_path: None,
        }
    }

    /// Parse from a `"VVVV:PPPP"` hex string.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the string is not in valid `VVVV:PPPP` hex format.
    pub fn parse(s: &str) -> Result<Self, UsbError> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 2 {
            return Err(UsbError::InvalidDeviceId(s.to_string()));
        }
        let vendor_id = u16::from_str_radix(parts[0], 16)
            .map_err(|_| UsbError::InvalidDeviceId(s.to_string()))?;
        let product_id = u16::from_str_radix(parts[1], 16)
            .map_err(|_| UsbError::InvalidDeviceId(s.to_string()))?;
        Ok(Self::new(vendor_id, product_id))
    }
}

impl fmt::Display for UsbDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor_id, self.product_id)
    }
}

// ---------------------------------------------------------------------------
// Physical port path
// ---------------------------------------------------------------------------

/// Physical USB port path (e.g. `1-2.3` = bus 1, port 2, hub port 3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbPortPath {
    /// Bus number.
    pub bus: u8,
    /// Port numbers along the topology tree.
    pub ports: Vec<u8>,
}

impl UsbPortPath {
    /// Create a new port path.
    #[must_use]
    pub fn new(bus: u8, ports: Vec<u8>) -> Self {
        Self { bus, ports }
    }

    /// Parse from a string like `"1-2"` or `"1-2.3.4"`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the string is not valid port path syntax.
    pub fn parse(s: &str) -> Result<Self, UsbError> {
        let dash = s
            .find('-')
            .ok_or_else(|| UsbError::InvalidPortPath(s.to_string()))?;
        let bus: u8 = s[..dash]
            .parse()
            .map_err(|_| UsbError::InvalidPortPath(s.to_string()))?;
        let port_str = &s[dash + 1..];
        let ports: Result<Vec<u8>, _> = port_str.split('.').map(str::parse).collect();
        let ports = ports.map_err(|_| UsbError::InvalidPortPath(s.to_string()))?;
        if ports.is_empty() {
            return Err(UsbError::InvalidPortPath(s.to_string()));
        }
        Ok(Self { bus, ports })
    }
}

impl fmt::Display for UsbPortPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-", self.bus)?;
        for (i, port) in self.ports.iter().enumerate() {
            if i > 0 {
                write!(f, ".")?;
            }
            write!(f, "{port}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// USB speed
// ---------------------------------------------------------------------------

/// USB device speed class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UsbSpeed {
    /// USB 1.0 — 1.5 Mbps.
    Low,
    /// USB 1.1 — 12 Mbps.
    Full,
    /// USB 2.0 — 480 Mbps.
    High,
    /// USB 3.0 — 5 Gbps.
    Super,
    /// USB 3.1 — 10 Gbps.
    SuperPlus,
    /// USB 3.2 — 20 Gbps.
    SuperPlusx2,
}

impl UsbSpeed {
    /// Maximum bandwidth in bits per second.
    #[must_use]
    pub const fn max_bandwidth_bps(self) -> u64 {
        match self {
            Self::Low => 1_500_000,
            Self::Full => 12_000_000,
            Self::High => 480_000_000,
            Self::Super => 5_000_000_000,
            Self::SuperPlus => 10_000_000_000,
            Self::SuperPlusx2 => 20_000_000_000,
        }
    }

    /// xHCI slot speed value (PSI-based encoding).
    #[must_use]
    pub const fn xhci_slot_speed(self) -> u8 {
        match self {
            Self::Low => 2,
            Self::Full => 1,
            Self::High => 3,
            Self::Super => 4,
            Self::SuperPlus | Self::SuperPlusx2 => 5,
        }
    }
}

impl fmt::Display for UsbSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Low => write!(f, "Low Speed (1.5 Mbps)"),
            Self::Full => write!(f, "Full Speed (12 Mbps)"),
            Self::High => write!(f, "High Speed (480 Mbps)"),
            Self::Super => write!(f, "SuperSpeed (5 Gbps)"),
            Self::SuperPlus => write!(f, "SuperSpeed+ (10 Gbps)"),
            Self::SuperPlusx2 => write!(f, "SuperSpeed+ 2x2 (20 Gbps)"),
        }
    }
}

/// Alias used by the monitor layer.
pub type DeviceSpeed = UsbSpeed;

// ---------------------------------------------------------------------------
// USB class codes
// ---------------------------------------------------------------------------

/// Standard USB device class codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum UsbDeviceClass {
    /// Device class defined at interface level.
    PerInterface = 0x00,
    /// Audio device.
    Audio = 0x01,
    /// Communications and CDC control.
    Cdc = 0x02,
    /// Human Interface Device (keyboard, mouse, etc.).
    Hid = 0x03,
    /// Physical device.
    Physical = 0x05,
    /// Image (scanner, camera).
    Image = 0x06,
    /// Printer.
    Printer = 0x07,
    /// Mass storage.
    MassStorage = 0x08,
    /// USB hub.
    Hub = 0x09,
    /// CDC data.
    CdcData = 0x0A,
    /// Smart card.
    SmartCard = 0x0B,
    /// Video device.
    Video = 0x0E,
    /// Personal healthcare.
    PersonalHealthcare = 0x0F,
    /// Audio/video.
    AudioVideo = 0x10,
    /// Billboard device.
    Billboard = 0x11,
    /// USB Type-C bridge.
    TypeCBridge = 0x12,
    /// Wireless controller.
    Wireless = 0xE0,
    /// Miscellaneous.
    Misc = 0xEF,
    /// Application specific.
    ApplicationSpecific = 0xFE,
    /// Vendor specific.
    VendorSpecific = 0xFF,
}

impl UsbDeviceClass {
    /// Create from a raw class code byte.
    #[must_use]
    pub const fn from_raw(value: u8) -> Option<Self> {
        match value {
            0x00 => Some(Self::PerInterface),
            0x01 => Some(Self::Audio),
            0x02 => Some(Self::Cdc),
            0x03 => Some(Self::Hid),
            0x05 => Some(Self::Physical),
            0x06 => Some(Self::Image),
            0x07 => Some(Self::Printer),
            0x08 => Some(Self::MassStorage),
            0x09 => Some(Self::Hub),
            0x0A => Some(Self::CdcData),
            0x0B => Some(Self::SmartCard),
            0x0E => Some(Self::Video),
            0x0F => Some(Self::PersonalHealthcare),
            0x10 => Some(Self::AudioVideo),
            0x11 => Some(Self::Billboard),
            0x12 => Some(Self::TypeCBridge),
            0xE0 => Some(Self::Wireless),
            0xEF => Some(Self::Misc),
            0xFE => Some(Self::ApplicationSpecific),
            0xFF => Some(Self::VendorSpecific),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// USB device descriptor
// ---------------------------------------------------------------------------

/// Standard USB device descriptor (baked from the 18-byte on-wire format).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDeviceDescriptor {
    /// Vendor ID.
    pub vendor_id: u16,
    /// Product ID.
    pub product_id: u16,
    /// Device class code.
    pub device_class: UsbDeviceClass,
    /// Device subclass code.
    pub device_subclass: u8,
    /// Device protocol code.
    pub device_protocol: u8,
    /// Manufacturer string (if available).
    pub manufacturer: Option<String>,
    /// Product string (if available).
    pub product: Option<String>,
    /// Serial number string (if available).
    pub serial_number: Option<String>,
    /// Max packet size for endpoint 0 (8, 16, 32, or 64).
    pub max_packet_size_0: u8,
    /// Number of configurations.
    pub num_configurations: u8,
    /// USB specification version `(major, minor)`.
    pub usb_version: (u8, u8),
    /// Device release version `(major, minor)`.
    pub device_version: (u8, u8),
}

// ---------------------------------------------------------------------------
// USB device state machine
// ---------------------------------------------------------------------------

/// USB device state per the USB specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UsbDeviceState {
    /// Device is attached but not yet reset.
    Attached,
    /// Device has been reset and is in the default state.
    Default,
    /// Device has been assigned an address.
    Addressed,
    /// Device has been configured and is operational.
    Configured,
    /// Device is suspended (low power).
    Suspended,
}

impl fmt::Display for UsbDeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Attached => write!(f, "Attached"),
            Self::Default => write!(f, "Default"),
            Self::Addressed => write!(f, "Addressed"),
            Self::Configured => write!(f, "Configured"),
            Self::Suspended => write!(f, "Suspended"),
        }
    }
}

// ---------------------------------------------------------------------------
// USB interface descriptor (simplified)
// ---------------------------------------------------------------------------

/// A USB interface descriptor (simplified).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbInterfaceInfo {
    /// Interface number.
    pub number: u8,
    /// Alternate setting.
    pub alternate_setting: u8,
    /// Interface class.
    pub class: UsbDeviceClass,
    /// Interface subclass.
    pub subclass: u8,
    /// Interface protocol.
    pub protocol: u8,
    /// Endpoints.
    pub endpoints: Vec<UsbEndpointInfo>,
}

/// A USB endpoint descriptor (simplified, serializable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbEndpointInfo {
    /// Endpoint address (0-15, direction encoded in bit 7).
    pub address: u8,
    /// Transfer type.
    pub transfer_type: TransferType,
    /// Max packet size.
    pub max_packet_size: u16,
    /// Polling interval (for interrupt/isochronous endpoints).
    pub interval: u8,
}

// ---------------------------------------------------------------------------
// Device info (used by monitor)
// ---------------------------------------------------------------------------

/// Complete information about a tracked USB device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDeviceInfo {
    /// Assigned device address.
    pub address: UsbAddress,
    /// Physical port path.
    pub port_path: UsbPortPath,
    /// Device descriptor.
    pub descriptor: UsbDeviceDescriptor,
    /// Connection speed.
    pub speed: DeviceSpeed,
    /// Current device state.
    pub state: UsbDeviceState,
    /// Interfaces exposed by the device.
    pub interfaces: Vec<UsbInterfaceInfo>,
}

impl UsbDeviceInfo {
    /// Human-readable description for display.
    #[must_use]
    pub fn display_name(&self) -> String {
        if let Some(ref product) = self.descriptor.product {
            if let Some(ref mfr) = self.descriptor.manufacturer {
                return format!("{mfr} {product}");
            }
            return product.clone();
        }
        format!(
            "USB Device {:04x}:{:04x}",
            self.descriptor.vendor_id, self.descriptor.product_id
        )
    }
}

// ---------------------------------------------------------------------------
// Transfer types
// ---------------------------------------------------------------------------

/// USB transfer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransferType {
    /// Control transfers (endpoint 0).
    Control,
    /// Isochronous (guaranteed bandwidth, no retry).
    Isochronous,
    /// Bulk (large data, retry on error).
    Bulk,
    /// Interrupt (small, periodic, guaranteed latency).
    Interrupt,
}

/// Direction of a USB transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransferDirection {
    /// Host to device.
    Out,
    /// Device to host.
    In,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from USB subsystem operations.
#[derive(Debug, thiserror::Error)]
pub enum UsbError {
    /// Invalid VID:PID string.
    #[error("invalid USB device ID: {0}")]
    InvalidDeviceId(String),
    /// Invalid port path string.
    #[error("invalid USB port path: {0}")]
    InvalidPortPath(String),
    /// Device not found.
    #[error("USB device not found")]
    DeviceNotFound,
    /// Device already routed to a guest.
    #[error("device already routed to guest")]
    AlreadyRouted,
    /// Guest not found.
    #[error("guest not found")]
    GuestNotFound,
    /// xHCI controller error.
    #[error("xHCI error: {0}")]
    XhciError(String),
    /// Transfer failed.
    #[error("USB transfer error: {0}")]
    TransferError(String),
    /// Enumeration error.
    #[error("USB enumeration error: {0}")]
    EnumerationError(String),
    /// Configuration error.
    #[error("USB configuration error: {0}")]
    ConfigError(String),
    /// Device address space exhausted (127 max).
    #[error("USB address space exhausted")]
    AddressExhausted,
    /// Slot not available.
    #[error("no free xHCI device slots")]
    NoFreeSlots,
    /// Invalid slot state for the requested operation.
    #[error("invalid slot state: {0}")]
    InvalidSlotState(String),
    /// Invalid endpoint.
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    /// Ring full.
    #[error("TRB ring full")]
    RingFull,
    /// Ring empty.
    #[error("TRB ring empty")]
    RingEmpty,
}

/// Convenience result type for USB operations.
pub type UsbResult<T> = Result<T, UsbError>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_device_id() {
        let id = UsbDeviceId::parse("046d:c077").unwrap();
        assert_eq!(id.vendor_id, 0x046d);
        assert_eq!(id.product_id, 0xc077);
        assert_eq!(id.to_string(), "046d:c077");
    }

    #[test]
    fn parse_device_id_uppercase() {
        let id = UsbDeviceId::parse("046D:C077").unwrap();
        assert_eq!(id.vendor_id, 0x046d);
        assert_eq!(id.product_id, 0xc077);
    }

    #[test]
    fn parse_device_id_invalid() {
        assert!(UsbDeviceId::parse("not-valid").is_err());
        assert!(UsbDeviceId::parse("046d").is_err());
        assert!(UsbDeviceId::parse("046d:xyz0").is_err());
        assert!(UsbDeviceId::parse("").is_err());
    }

    #[test]
    fn parse_port_path_simple() {
        let path = UsbPortPath::parse("1-2").unwrap();
        assert_eq!(path.bus, 1);
        assert_eq!(path.ports, vec![2]);
        assert_eq!(path.to_string(), "1-2");
    }

    #[test]
    fn parse_port_path_hub() {
        let path = UsbPortPath::parse("2-1.3.4").unwrap();
        assert_eq!(path.bus, 2);
        assert_eq!(path.ports, vec![1, 3, 4]);
        assert_eq!(path.to_string(), "2-1.3.4");
    }

    #[test]
    fn parse_port_path_invalid() {
        assert!(UsbPortPath::parse("").is_err());
        assert!(UsbPortPath::parse("no-dash-here").is_err());
        assert!(UsbPortPath::parse("x-1").is_err());
        assert!(UsbPortPath::parse("1-").is_err());
    }

    #[test]
    fn usb_speed_bandwidth() {
        assert_eq!(UsbSpeed::Low.max_bandwidth_bps(), 1_500_000);
        assert_eq!(UsbSpeed::Super.max_bandwidth_bps(), 5_000_000_000);
    }

    #[test]
    fn usb_speed_xhci_slot() {
        assert_eq!(UsbSpeed::Low.xhci_slot_speed(), 2);
        assert_eq!(UsbSpeed::Full.xhci_slot_speed(), 1);
        assert_eq!(UsbSpeed::High.xhci_slot_speed(), 3);
        assert_eq!(UsbSpeed::Super.xhci_slot_speed(), 4);
        assert_eq!(UsbSpeed::SuperPlus.xhci_slot_speed(), 5);
    }

    #[test]
    fn usb_speed_display() {
        assert!(UsbSpeed::High.to_string().contains("480"));
        assert!(UsbSpeed::Super.to_string().contains("5 Gbps"));
    }

    #[test]
    fn usb_class_from_raw() {
        assert_eq!(UsbDeviceClass::from_raw(0x03), Some(UsbDeviceClass::Hid));
        assert_eq!(
            UsbDeviceClass::from_raw(0x08),
            Some(UsbDeviceClass::MassStorage)
        );
        assert_eq!(UsbDeviceClass::from_raw(0x04), None);
    }

    #[test]
    fn usb_address_value() {
        let addr = UsbAddress::new(42);
        assert_eq!(addr.value(), 42);
        assert_eq!(addr.to_string(), "42");
    }

    #[test]
    fn device_state_display() {
        assert_eq!(UsbDeviceState::Configured.to_string(), "Configured");
        assert_eq!(UsbDeviceState::Suspended.to_string(), "Suspended");
    }
}
