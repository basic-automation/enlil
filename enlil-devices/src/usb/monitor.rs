//! Host USB device enumeration and hot-plug monitoring.
//!
//! The [`UsbMonitor`] tracks all physical USB devices connected to the host,
//! detects connect/disconnect events, and maintains a live device inventory.
//! On bare-metal, this reads from the physical xHCI controller's port status
//! registers. On Linux, it reads from sysfs/udev.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::types::{
    DeviceSpeed, UsbAddress, UsbDeviceClass, UsbDeviceDescriptor, UsbDeviceInfo,
    UsbDeviceState, UsbError, UsbPortPath, UsbResult,
};

// ---------------------------------------------------------------------------
// Hot-plug events
// ---------------------------------------------------------------------------

/// An event from the USB monitor when a device is connected or disconnected.
#[derive(Debug, Clone)]
pub enum UsbHotplugEvent {
    /// A new device was connected.
    Connected(UsbDeviceInfo),
    /// A device was disconnected.
    Disconnected(UsbPortPath),
}

/// Callback type for hot-plug notifications.
pub type HotplugCallback = Box<dyn Fn(&UsbHotplugEvent) + Send + Sync>;

// ---------------------------------------------------------------------------
// USB Monitor
// ---------------------------------------------------------------------------

/// Tracks all physical USB devices and provides hot-plug notifications.
///
/// The monitor maintains a snapshot of the device tree and can be polled
/// or subscribed to for changes.
pub struct UsbMonitor {
    /// Current device inventory, keyed by port path.
    devices: Arc<Mutex<HashMap<UsbPortPath, UsbDeviceInfo>>>,
    /// Registered hot-plug listeners.
    listeners: Arc<Mutex<Vec<HotplugCallback>>>,
    /// Next synthetic device address counter.
    next_address: Arc<Mutex<u8>>,
    /// Last poll timestamp.
    last_poll: Arc<Mutex<Instant>>,
    /// Poll interval.
    poll_interval: Duration,
}

impl UsbMonitor {
    /// Create a new USB monitor with the given poll interval.
    pub fn new(poll_interval: Duration) -> Self {
        Self {
            devices: Arc::new(Mutex::new(HashMap::new())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_address: Arc::new(Mutex::new(1)),
            last_poll: Arc::new(Mutex::new(Instant::now())),
            poll_interval,
        }
    }

    /// Return a snapshot of all currently connected devices.
    pub fn devices(&self) -> HashMap<UsbPortPath, UsbDeviceInfo> {
        self.devices.lock().expect("device lock poisoned").clone()
    }

    /// Return the number of connected devices.
    pub fn device_count(&self) -> usize {
        self.devices.lock().expect("device lock poisoned").len()
    }

    /// Look up a device by its port path.
    pub fn device_by_port(&self, port: &UsbPortPath) -> Option<UsbDeviceInfo> {
        self.devices.lock().expect("device lock poisoned").get(port).cloned()
    }

    /// Look up a device by VID:PID (returns the first match).
    pub fn device_by_vid_pid(&self, vendor_id: u16, product_id: u16) -> Option<UsbDeviceInfo> {
        self.devices
            .lock()
            .expect("device lock poisoned")
            .values()
            .find(|d| {
                d.descriptor.vendor_id == vendor_id && d.descriptor.product_id == product_id
            })
            .cloned()
    }

    /// Register a hot-plug callback.
    pub fn on_hotplug(&self, callback: HotplugCallback) {
        self.listeners.lock().expect("listener lock poisoned").push(callback);
    }

    /// Allocate the next device address.
    fn allocate_address(&self) -> UsbResult<UsbAddress> {
        let mut next = self.next_address.lock().expect("address lock poisoned");
        if *next >= 127 {
            return Err(UsbError::AddressExhausted);
        }
        let addr = UsbAddress::new(*next);
        *next += 1;
        Ok(addr)
    }

    /// Simulate a device connect event (used for testing and bare-metal enumeration).
    ///
    /// In production, the xHCI driver calls this when a port status change
    /// event fires.
    pub fn report_connect(
        &self,
        port_path: UsbPortPath,
        descriptor: UsbDeviceDescriptor,
        speed: DeviceSpeed,
    ) -> UsbResult<UsbDeviceInfo> {
        let address = self.allocate_address()?;
        let info = UsbDeviceInfo {
            address,
            port_path: port_path.clone(),
            descriptor,
            speed,
            state: UsbDeviceState::Configured,
            interfaces: Vec::new(),
        };

        self.devices
            .lock()
            .expect("device lock poisoned")
            .insert(port_path, info.clone());

        let event = UsbHotplugEvent::Connected(info.clone());
        self.notify_listeners(&event);

        Ok(info)
    }

    /// Simulate a device disconnect event.
    pub fn report_disconnect(&self, port_path: &UsbPortPath) -> UsbResult<()> {
        let removed = self
            .devices
            .lock()
            .expect("device lock poisoned")
            .remove(port_path);

        if removed.is_some() {
            let event = UsbHotplugEvent::Disconnected(port_path.clone());
            self.notify_listeners(&event);
            Ok(())
        } else {
            Err(UsbError::DeviceNotFound)
        }
    }

    /// Get the configured poll interval.
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Check if enough time has elapsed for another poll cycle.
    pub fn should_poll(&self) -> bool {
        let last = *self.last_poll.lock().expect("poll lock poisoned");
        last.elapsed() >= self.poll_interval
    }

    /// Record that a poll cycle just completed.
    pub fn mark_polled(&self) {
        *self.last_poll.lock().expect("poll lock poisoned") = Instant::now();
    }

    /// Notify all registered listeners of a hot-plug event.
    fn notify_listeners(&self, event: &UsbHotplugEvent) {
        let listeners = self.listeners.lock().expect("listener lock poisoned");
        for cb in listeners.iter() {
            cb(event);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_descriptor(vid: u16, pid: u16) -> UsbDeviceDescriptor {
        UsbDeviceDescriptor {
            vendor_id: vid,
            product_id: pid,
            device_class: UsbDeviceClass::Hid,
            device_subclass: 0,
            device_protocol: 0,
            manufacturer: Some("TestVendor".into()),
            product: Some("TestDevice".into()),
            serial_number: None,
            max_packet_size_0: 64,
            num_configurations: 1,
            usb_version: (2, 0),
            device_version: (1, 0),
        }
    }

    #[test]
    fn connect_and_enumerate() {
        let mon = UsbMonitor::new(Duration::from_millis(100));
        let port = UsbPortPath::new(1, vec![1]);
        let desc = test_descriptor(0x046d, 0xc077);

        let info = mon.report_connect(port.clone(), desc, DeviceSpeed::High).unwrap();
        assert_eq!(info.address.value(), 1);
        assert_eq!(mon.device_count(), 1);

        let found = mon.device_by_port(&port).unwrap();
        assert_eq!(found.descriptor.vendor_id, 0x046d);
    }

    #[test]
    fn connect_disconnect() {
        let mon = UsbMonitor::new(Duration::from_millis(100));
        let port = UsbPortPath::new(1, vec![2]);
        let desc = test_descriptor(0x046d, 0xc534);

        mon.report_connect(port.clone(), desc, DeviceSpeed::Full).unwrap();
        assert_eq!(mon.device_count(), 1);

        mon.report_disconnect(&port).unwrap();
        assert_eq!(mon.device_count(), 0);
    }

    #[test]
    fn disconnect_nonexistent_fails() {
        let mon = UsbMonitor::new(Duration::from_millis(100));
        let port = UsbPortPath::new(1, vec![3]);
        assert!(mon.report_disconnect(&port).is_err());
    }

    #[test]
    fn lookup_by_vid_pid() {
        let mon = UsbMonitor::new(Duration::from_millis(100));
        let port = UsbPortPath::new(1, vec![1]);
        let desc = test_descriptor(0x1234, 0x5678);

        mon.report_connect(port, desc, DeviceSpeed::Super).unwrap();

        let found = mon.device_by_vid_pid(0x1234, 0x5678);
        assert!(found.is_some());
        assert_eq!(found.unwrap().speed, DeviceSpeed::Super);

        let not_found = mon.device_by_vid_pid(0xFFFF, 0x0000);
        assert!(not_found.is_none());
    }

    #[test]
    fn hotplug_callback_fires() {
        let mon = UsbMonitor::new(Duration::from_millis(100));
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();

        mon.on_hotplug(Box::new(move |_event| {
            c.fetch_add(1, Ordering::Relaxed);
        }));

        let port = UsbPortPath::new(1, vec![1]);
        let desc = test_descriptor(0x046d, 0xc077);

        mon.report_connect(port.clone(), desc, DeviceSpeed::High).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        mon.report_disconnect(&port).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn multiple_devices() {
        let mon = UsbMonitor::new(Duration::from_millis(100));

        for i in 1..=5_u8 {
            let port = UsbPortPath::new(1, vec![i]);
            let desc = test_descriptor(0x1000 + u16::from(i), 0x2000);
            mon.report_connect(port, desc, DeviceSpeed::High).unwrap();
        }

        assert_eq!(mon.device_count(), 5);

        let devices = mon.devices();
        assert_eq!(devices.len(), 5);
    }

    #[test]
    fn address_increments() {
        let mon = UsbMonitor::new(Duration::from_millis(100));

        let p1 = UsbPortPath::new(1, vec![1]);
        let p2 = UsbPortPath::new(1, vec![2]);
        let desc = test_descriptor(0x0001, 0x0001);

        let d1 = mon.report_connect(p1, desc.clone(), DeviceSpeed::Full).unwrap();
        let d2 = mon.report_connect(p2, desc, DeviceSpeed::Full).unwrap();

        assert_eq!(d1.address.value(), 1);
        assert_eq!(d2.address.value(), 2);
    }

    #[test]
    fn poll_interval_tracking() {
        let mon = UsbMonitor::new(Duration::from_millis(10));
        assert_eq!(mon.poll_interval(), Duration::from_millis(10));

        // Just created, so should_poll depends on timing — mark and check.
        mon.mark_polled();
        // Immediately after marking, should not need to poll.
        // (Could be flaky on very slow systems, but 10ms is generous.)
        std::thread::sleep(Duration::from_millis(15));
        assert!(mon.should_poll());
    }
}
