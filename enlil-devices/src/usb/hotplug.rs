//! The hot-plug bridge: monitor events → routing → controller attachments.
//!
//! [`UsbMonitor`](super::monitor::UsbMonitor) callbacks fire on the
//! monitor's (potentially separate) thread, while the per-guest controllers
//! live behind single-threaded [`SharedXhci`](super::controller::SharedXhci)
//! handles on the run loop. This module is the channel between them the
//! controller documentation promises: the dispatcher's subscription sends
//! every hot-plug event into an mpsc channel, and the run loop drains it
//! with [`service`](HotplugDispatcher::service), applying each event to the
//! [`XhciRegistry`](super::registry::XhciRegistry) — so routing decisions
//! and guest-visible port changes always happen on the run loop's thread.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use super::device::UsbDevice;
use super::monitor::{UsbHotplugEvent, UsbMonitor};
use super::registry::{AttachOutcome, RegistryError, XhciRegistry};
use super::types::UsbPortPath;

/// What servicing one hot-plug event did — the management console's
/// notification feed.
#[derive(Debug)]
pub enum HotplugOutcome {
    /// A connected device was routed and attached.
    Attached {
        /// Host bus address the monitor assigned.
        bus_addr: u8,
        /// The guest it landed on.
        guest: String,
        /// 0-based root-hub port on that guest's controller.
        port: usize,
    },
    /// A connected device matched no routing rule; it stays with the
    /// hypervisor.
    Unassigned {
        /// Host bus address the monitor assigned.
        bus_addr: u8,
    },
    /// A connected device was routed but could not be attached.
    AttachFailed {
        /// Host bus address the monitor assigned.
        bus_addr: u8,
        /// Why the attachment failed.
        error: RegistryError,
    },
    /// A disconnected device was detached from its guest.
    Detached {
        /// Host bus address the device had.
        bus_addr: u8,
        /// The guest it was unplugged from.
        guest: String,
    },
    /// A disconnect arrived for a port the dispatcher never saw connect.
    UnknownDisconnect {
        /// The physical port path the event named.
        port_path: UsbPortPath,
    },
}

/// Drains monitor hot-plug events into registry actions on the run loop's
/// thread.
pub struct HotplugDispatcher {
    /// The run-loop end of the event channel.
    events: Receiver<UsbHotplugEvent>,
    /// Physical port path → host bus address, so a disconnect (which only
    /// names the port) finds the placement the connect created.
    by_port: HashMap<UsbPortPath, u8>,
}

impl HotplugDispatcher {
    /// Subscribe to `monitor`'s hot-plug events. Events fired from any
    /// thread queue up until [`service`](Self::service) drains them.
    #[must_use]
    pub fn subscribe(monitor: &UsbMonitor) -> Self {
        let (sender, events) = channel();
        // mpsc senders are Send but not Sync; the callback list is shared,
        // so serialize sends through a mutex.
        let sender: Mutex<Sender<UsbHotplugEvent>> = Mutex::new(sender);
        monitor.on_hotplug(Box::new(move |event| {
            if let Ok(sender) = sender.lock() {
                // A send only fails when the dispatcher is gone; the
                // subscription then just discards events.
                let _ = sender.send(event.clone());
            }
        }));
        Self {
            events,
            by_port: HashMap::new(),
        }
    }

    /// Drain every queued event, applying each to `registry`: connects are
    /// routed and attached to the decided guest's controller, disconnects
    /// detach from wherever the device sits. Returns one outcome per event,
    /// in order.
    pub fn service(&mut self, registry: &mut XhciRegistry) -> Vec<HotplugOutcome> {
        let mut outcomes = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            outcomes.push(match event {
                UsbHotplugEvent::Connected(info) => {
                    let device = UsbDevice::new(&info);
                    self.by_port.insert(info.port_path, device.bus_address);
                    match registry.attach(device.bus_address, &device.id) {
                        Ok(AttachOutcome::Attached(placement)) => HotplugOutcome::Attached {
                            bus_addr: device.bus_address,
                            guest: placement.guest,
                            port: placement.port,
                        },
                        Ok(AttachOutcome::Unassigned) => HotplugOutcome::Unassigned {
                            bus_addr: device.bus_address,
                        },
                        Err(error) => HotplugOutcome::AttachFailed {
                            bus_addr: device.bus_address,
                            error,
                        },
                    }
                }
                UsbHotplugEvent::Disconnected(port_path) => {
                    self.by_port.remove(&port_path).map_or(
                        HotplugOutcome::UnknownDisconnect { port_path },
                        |bus_addr| match registry.detach(bus_addr) {
                            Ok(placement) => HotplugOutcome::Detached {
                                bus_addr,
                                guest: placement.guest,
                            },
                            // Connected but never placed (unassigned or a
                            // failed attach) — nothing to detach.
                            Err(_) => HotplugOutcome::Unassigned { bus_addr },
                        },
                    )
                }
            });
        }
        outcomes
    }

    /// The bus address of the device connected at a physical port, if the
    /// dispatcher has seen it connect.
    #[must_use]
    pub fn bus_addr_at(&self, port_path: &UsbPortPath) -> Option<u8> {
        self.by_port.get(port_path).copied()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use super::super::controller::{SharedXhci, VirtualXhciController};
    use super::super::routing::{DeviceMatcher, RoutingState, RoutingTable};
    use super::super::types::{DeviceSpeed, UsbDeviceClass, UsbDeviceDescriptor};
    use super::*;

    fn running_controller(ports: u8) -> SharedXhci {
        let mut c = VirtualXhciController::new(ports);
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x38, 8);
        c.write_register(op, 1);
        Rc::new(RefCell::new(c))
    }

    fn descriptor(vid: u16, pid: u16) -> UsbDeviceDescriptor {
        UsbDeviceDescriptor {
            vendor_id: vid,
            product_id: pid,
            device_class: UsbDeviceClass::Hid,
            device_subclass: 0,
            device_protocol: 0,
            manufacturer: None,
            product: None,
            serial_number: None,
            max_packet_size_0: 64,
            num_configurations: 1,
            usb_version: (2, 0),
            device_version: (1, 0),
        }
    }

    fn registry_routing_logitech_to(guest: &str) -> XhciRegistry {
        let mut table = RoutingTable::new();
        table.add_rule(
            1,
            DeviceMatcher::VendorOnly { vendor_id: 0x046D },
            guest.into(),
        );
        let mut registry = XhciRegistry::new(RoutingState::new(table));
        registry.register_controller(guest, running_controller(4));
        registry
    }

    #[test]
    fn monitor_connect_lands_on_the_routed_guest_via_service() {
        let monitor = UsbMonitor::new(Duration::from_millis(100));
        let mut dispatcher = HotplugDispatcher::subscribe(&monitor);
        let mut registry = registry_routing_logitech_to("linux1");

        let port = UsbPortPath::new(1, vec![1]);
        let info = monitor
            .report_connect(port.clone(), descriptor(0x046D, 0xC077), DeviceSpeed::Low)
            .unwrap();

        // Nothing reaches the controller until the run loop services the
        // channel.
        assert_eq!(registry.placements().len(), 0);

        let outcomes = dispatcher.service(&mut registry);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            HotplugOutcome::Attached {
                bus_addr,
                guest,
                port,
            } => {
                assert_eq!(*bus_addr, info.address.value());
                assert_eq!(guest, "linux1");
                assert_eq!(*port, 0);
            }
            other => panic!("expected an attach, got {other:?}"),
        }
        // The guest's controller really saw the plug.
        let handle = registry.controller("linux1").unwrap();
        assert_eq!(handle.borrow().read_register(0x20 + 0x400) & 1, 1);
        assert_eq!(dispatcher.bus_addr_at(&port), Some(info.address.value()));
    }

    #[test]
    fn monitor_disconnect_detaches_by_port_path() {
        let monitor = UsbMonitor::new(Duration::from_millis(100));
        let mut dispatcher = HotplugDispatcher::subscribe(&monitor);
        let mut registry = registry_routing_logitech_to("linux1");

        let port = UsbPortPath::new(1, vec![1]);
        monitor
            .report_connect(port.clone(), descriptor(0x046D, 0xC077), DeviceSpeed::Low)
            .unwrap();
        let _ = dispatcher.service(&mut registry);

        monitor.report_disconnect(&port).unwrap();
        let outcomes = dispatcher.service(&mut registry);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            HotplugOutcome::Detached { guest, .. } if guest == "linux1"
        ));
        assert_eq!(registry.placements().len(), 0);
        assert_eq!(dispatcher.bus_addr_at(&port), None);
        // The controller port is free again (CCS clear).
        let handle = registry.controller("linux1").unwrap();
        assert_eq!(handle.borrow().read_register(0x20 + 0x400) & 1, 0);
    }

    #[test]
    fn unmatched_device_reports_unassigned() {
        let monitor = UsbMonitor::new(Duration::from_millis(100));
        let mut dispatcher = HotplugDispatcher::subscribe(&monitor);
        let mut registry = registry_routing_logitech_to("linux1");

        monitor
            .report_connect(
                UsbPortPath::new(1, vec![2]),
                descriptor(0xAAAA, 0xBBBB),
                DeviceSpeed::High,
            )
            .unwrap();
        let outcomes = dispatcher.service(&mut registry);
        assert!(matches!(outcomes[0], HotplugOutcome::Unassigned { .. }));
        assert_eq!(registry.placements().len(), 0);
    }

    #[test]
    fn events_from_another_thread_queue_until_serviced() {
        let monitor = std::sync::Arc::new(UsbMonitor::new(Duration::from_millis(100)));
        let mut dispatcher = HotplugDispatcher::subscribe(&monitor);
        let mut registry = registry_routing_logitech_to("linux1");

        let remote = std::sync::Arc::clone(&monitor);
        std::thread::spawn(move || {
            remote
                .report_connect(
                    UsbPortPath::new(2, vec![4]),
                    descriptor(0x046D, 0xC534),
                    DeviceSpeed::Full,
                )
                .unwrap();
        })
        .join()
        .unwrap();

        let outcomes = dispatcher.service(&mut registry);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], HotplugOutcome::Attached { .. }));
    }

    #[test]
    fn disconnect_for_an_unknown_port_is_reported_not_panicked() {
        let monitor = UsbMonitor::new(Duration::from_millis(100));
        let mut dispatcher = HotplugDispatcher::subscribe(&monitor);
        let mut registry = registry_routing_logitech_to("linux1");

        // A disconnect the dispatcher never saw connect (e.g. subscribed
        // after the device was already up): surfaced, not dropped.
        monitor
            .report_connect(
                UsbPortPath::new(3, vec![1]),
                descriptor(0x046D, 0xC077),
                DeviceSpeed::Low,
            )
            .unwrap();
        // Steal the connect so the dispatcher only sees the disconnect.
        let _ = dispatcher.service(&mut registry);
        let mut fresh = HotplugDispatcher::subscribe(&monitor);
        monitor
            .report_disconnect(&UsbPortPath::new(3, vec![1]))
            .unwrap();
        let outcomes = fresh.service(&mut registry);
        assert!(matches!(
            &outcomes[0],
            HotplugOutcome::UnknownDisconnect { port_path } if *port_path == UsbPortPath::new(3, vec![1])
        ));
    }
}
