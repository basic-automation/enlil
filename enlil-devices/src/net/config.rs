//! Network device configuration.
//!
//! Maps to the TOML config:
//! ```toml
//! [guest.linux1.net]
//! eth0 = { mode = "bridge", bridge = "br0", mac = "52:54:00:01:00:01" }
//! ```

use std::fmt;

/// MAC address as 6 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddress(pub [u8; 6]);

impl MacAddress {
    pub const BROADCAST: Self = Self([0xff; 6]);
    pub const ZERO: Self = Self([0x00; 6]);

    /// Parse a MAC address from "aa:bb:cc:dd:ee:ff" format.
    pub fn parse(s: &str) -> Result<Self, MacParseError> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 6 {
            return Err(MacParseError::InvalidFormat(s.to_string()));
        }
        let mut bytes = [0u8; 6];
        for (i, part) in parts.iter().enumerate() {
            bytes[i] =
                u8::from_str_radix(part, 16).map_err(|_| MacParseError::InvalidByte(i, part.to_string()))?;
        }
        Ok(Self(bytes))
    }

    /// Returns true if this is a multicast address (bit 0 of first octet set).
    pub fn is_multicast(&self) -> bool {
        self.0[0] & 0x01 != 0
    }

    /// Returns true if this is the broadcast address (ff:ff:ff:ff:ff:ff).
    pub fn is_broadcast(&self) -> bool {
        *self == Self::BROADCAST
    }

    /// Returns true if this is a unicast address.
    pub fn is_unicast(&self) -> bool {
        !self.is_multicast()
    }

    /// Returns the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

impl From<[u8; 6]> for MacAddress {
    fn from(bytes: [u8; 6]) -> Self {
        Self(bytes)
    }
}

/// Errors from parsing a MAC address string.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MacParseError {
    #[error("invalid MAC format: {0}")]
    InvalidFormat(String),
    #[error("invalid byte at position {0}: {1}")]
    InvalidByte(usize, String),
}

/// Network backend mode.
#[derive(Debug, Clone, PartialEq)]
pub enum NetBackendMode {
    /// Bridge mode — connect to a host bridge via TAP.
    Bridge { bridge: String },
    /// NAT mode — host-side NAT (future).
    Nat,
    /// Null mode — packets are dropped, for testing.
    Null,
}

/// Configuration for a virtual NIC.
#[derive(Debug, Clone)]
pub struct NetDeviceConfig {
    /// Interface name inside the guest config (e.g. "eth0").
    pub name: String,
    /// MAC address (6 bytes).
    pub mac: MacAddress,
    /// Backend mode.
    pub mode: NetBackendMode,
    /// Name of the TAP interface on the host (Linux only).
    pub tap_name: Option<String>,
    /// MTU size.
    pub mtu: u16,
}

impl NetDeviceConfig {
    /// Create a new config with the given name and MAC.
    pub fn new(name: &str, mac: MacAddress) -> Self {
        Self {
            name: name.to_string(),
            mac,
            mode: NetBackendMode::Null,
            tap_name: None,
            mtu: 1500,
        }
    }

    /// Create a bridge-mode config.
    pub fn bridge(name: &str, mac: MacAddress, bridge: &str) -> Self {
        Self {
            name: name.to_string(),
            mac,
            mode: NetBackendMode::Bridge {
                bridge: bridge.to_string(),
            },
            tap_name: None,
            mtu: 1500,
        }
    }
}

impl Default for NetDeviceConfig {
    fn default() -> Self {
        Self {
            name: "eth0".to_string(),
            mac: MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
            mode: NetBackendMode::Null,
            tap_name: None,
            mtu: 1500,
        }
    }
}
