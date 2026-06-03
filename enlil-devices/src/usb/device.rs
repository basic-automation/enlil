//! USB device abstraction.
//!
//! Provides the [`UsbDevice`] type representing a physical USB device that has
//! been enumerated and is available for routing to a guest VM. This module
//! re-exports key types from [`super::types`] for convenience.

use std::fmt;

pub use super::types::{
    DeviceSpeed as UsbSpeed, UsbDeviceClass as UsbClass, UsbDeviceDescriptor, UsbDeviceId,
    UsbDeviceInfo, UsbDeviceState, UsbPortPath,
};

// ---------------------------------------------------------------------------
// USB Device
// ---------------------------------------------------------------------------

/// A physical USB device that has been enumerated on the host.
///
/// This is the routing-layer view of a device: it tracks the device identity,
/// current state, and which guest (if any) the device is assigned to.
#[derive(Debug, Clone)]
pub struct UsbDevice {
    /// Device identification (VID:PID, serial, port path, class).
    pub id: UsbDeviceId,
    /// Full device descriptor.
    pub descriptor: UsbDeviceDescriptor,
    /// Negotiated speed.
    pub speed: UsbSpeed,
    /// Current device state.
    pub state: UsbDeviceState,
    /// Guest this device is currently routed to (if any).
    pub assigned_guest: Option<String>,
    /// The host bus address assigned during enumeration.
    pub bus_address: u8,
}

impl UsbDevice {
    /// Create a new device from enumeration data.
    #[must_use]
    pub fn new(info: &UsbDeviceInfo) -> Self {
        Self {
            id: UsbDeviceId {
                vendor_id: info.descriptor.vendor_id,
                product_id: info.descriptor.product_id,
                serial: info.descriptor.serial_number.clone(),
                port_path: Some(info.port_path.to_string()),
                class: info.descriptor.device_class.clone(),
                speed: info.speed,
                manufacturer: info.descriptor.manufacturer.clone(),
                product: info.descriptor.product.clone(),
            },
            descriptor: info.descriptor.clone(),
            speed: info.speed,
            state: info.state.clone(),
            assigned_guest: None,
            bus_address: info.address.value(),
        }
    }

    /// Check if this device is currently assigned to a guest.
    #[must_use]
    pub fn is_assigned(&self) -> bool {
        self.assigned_guest.is_some()
    }

    /// Assign this device to a guest.
    pub fn assign_to(&mut self, guest_id: &str) {
        self.assigned_guest = Some(guest_id.to_string());
    }

    /// Unassign this device from its current guest.
    pub fn unassign(&mut self) -> Option<String> {
        self.assigned_guest.take()
    }

    /// Get a human-readable product name.
    #[must_use]
    pub fn product_name(&self) -> &str {
        self.descriptor
            .product
            .as_deref()
            .unwrap_or("Unknown Device")
    }

    /// Get a human-readable manufacturer name.
    #[must_use]
    pub fn manufacturer(&self) -> &str {
        self.descriptor.manufacturer.as_deref().unwrap_or("Unknown")
    }
}

impl fmt::Display for UsbDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:04x} {} ({}) [{}]",
            self.id.vendor_id,
            self.id.product_id,
            self.product_name(),
            self.speed,
            if let Some(ref g) = self.assigned_guest {
                g.as_str()
            } else {
                "unassigned"
            }
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::types::{UsbAddress, UsbDeviceClass};
    use super::*;

    fn make_info() -> UsbDeviceInfo {
        UsbDeviceInfo {
            address: UsbAddress::new(1),
            port_path: UsbPortPath::new(1, vec![1]),
            descriptor: UsbDeviceDescriptor {
                vendor_id: 0x046d,
                product_id: 0xc077,
                device_class: UsbDeviceClass::Hid,
                device_subclass: 0,
                device_protocol: 0,
                manufacturer: Some("Logitech".into()),
                product: Some("M105 Mouse".into()),
                serial_number: Some("ABC123".into()),
                max_packet_size_0: 64,
                num_configurations: 1,
                usb_version: (2, 0),
                device_version: (1, 0),
            },
            speed: UsbSpeed::High,
            state: UsbDeviceState::Configured,
            interfaces: Vec::new(),
        }
    }

    #[test]
    fn device_from_info() {
        let info = make_info();
        let dev = UsbDevice::new(&info);
        assert_eq!(dev.id.vendor_id, 0x046d);
        assert_eq!(dev.product_name(), "M105 Mouse");
        assert_eq!(dev.manufacturer(), "Logitech");
        assert!(!dev.is_assigned());
    }

    #[test]
    fn assign_unassign() {
        let info = make_info();
        let mut dev = UsbDevice::new(&info);
        dev.assign_to("linux1");
        assert!(dev.is_assigned());
        assert_eq!(dev.assigned_guest.as_deref(), Some("linux1"));

        let old = dev.unassign();
        assert_eq!(old.as_deref(), Some("linux1"));
        assert!(!dev.is_assigned());
    }

    #[test]
    fn device_display() {
        let info = make_info();
        let dev = UsbDevice::new(&info);
        let s = dev.to_string();
        assert!(s.contains("046d:c077"));
        assert!(s.contains("M105 Mouse"));
        assert!(s.contains("unassigned"));
    }
}
