//! Network device backends.
//!
//! Provides VirtIO-net emulation and a virtual switch
//! for inter-guest and guest-to-host networking.

/// Configuration for a virtual NIC.
#[derive(Debug, Clone)]
pub struct NetDeviceConfig {
    /// MAC address (6 bytes).
    pub mac: [u8; 6],
    /// Name of the TAP interface on the host (Linux only).
    pub tap_name: Option<String>,
    /// MTU size.
    pub mtu: u16,
}

impl Default for NetDeviceConfig {
    fn default() -> Self {
        Self {
            mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
            tap_name: None,
            mtu: 1500,
        }
    }
}

/// A virtual network device.
pub struct NetDevice {
    config: NetDeviceConfig,
}

impl NetDevice {
    pub fn new(config: NetDeviceConfig) -> Self {
        Self { config }
    }

    pub fn mac(&self) -> &[u8; 6] {
        &self.config.mac
    }

    pub fn mac_string(&self) -> String {
        self.config
            .mac
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(":")
    }
}
