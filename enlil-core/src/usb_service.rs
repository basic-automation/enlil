//! Daemon-side application of the management console's USB commands
//! (Phase 4.7).
//!
//! The console (`enlil-mgmt` TUI) builds `ClientMessage::UsbCommand`s from
//! operator keystrokes; this service owns the live [`XhciRegistry`] and turns
//! those commands into real port unplug/replug operations on the guests'
//! virtual xHCI controllers. It is the piece that makes the Phase 4
//! milestone — two mice + two keyboards routed to different guests with live
//! TUI reassignment — actually move hardware: the TUI speaks, the service
//! acts, and the guests see genuine hot-plug events.
//!
//! The management-socket task serves this service: it feeds it each decoded
//! `ClientMessage` and writes the returned `ServerMessage`s back to the
//! console.

use std::collections::HashMap;

use enlil_devices::usb::registry::{AttachOutcome, RegistryError, XhciRegistry};
use enlil_devices::usb::types::{DeviceSpeed, GuestId, UsbDeviceId};
use enlil_mgmt_proto::{ClientMessage, ServerMessage, UsbAction, UsbDeviceEntry};

/// Display label for a negotiated USB speed, matching the console's USB tab.
const fn speed_label(speed: DeviceSpeed) -> &'static str {
    match speed {
        DeviceSpeed::Low => "Low",
        DeviceSpeed::Full => "Full",
        DeviceSpeed::High => "High",
        DeviceSpeed::Super => "Super",
        DeviceSpeed::SuperPlus => "SuperSpeed+",
    }
}

/// The daemon-side USB service: live [`XhciRegistry`] plus the device
/// descriptors the routing engine needs to move devices at runtime.
///
/// The registry tracks *placements* (bus address → guest + port) but not the
/// [`UsbDeviceId`] descriptors, while [`XhciRegistry::reassign`] needs the
/// descriptor to replug the device on the target controller. This service
/// keeps that descriptor table alongside the registry so a console command
/// naming only a bus address can be applied directly.
pub struct UsbService {
    registry: XhciRegistry,
    /// Known device descriptors by host bus address (populated on connect,
    /// forgotten on physical disconnect).
    devices: HashMap<u8, UsbDeviceId>,
}

impl UsbService {
    /// Wrap a live registry. Devices the monitor already reported should be
    /// recorded with [`note_device`](Self::note_device) afterwards.
    #[must_use]
    pub fn new(registry: XhciRegistry) -> Self {
        Self {
            registry,
            devices: HashMap::new(),
        }
    }

    /// The live registry (placements, controllers).
    #[must_use]
    pub const fn registry(&self) -> &XhciRegistry {
        &self.registry
    }

    /// The live registry, mutably (hot-plug dispatch path).
    pub const fn registry_mut(&mut self) -> &mut XhciRegistry {
        &mut self.registry
    }

    /// Record a device descriptor without attaching it (the monitor's connect
    /// path records here; the dispatcher attaches through the registry).
    pub fn note_device(&mut self, bus_addr: u8, device: UsbDeviceId) {
        self.devices.insert(bus_addr, device);
    }

    /// Route a newly seen device through the policy engine and attach it to
    /// the decided guest's controller, recording its descriptor so later
    /// console commands can move it.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NoController`] when the routing decision targets a
    /// guest with no registered controller, [`RegistryError::NoFreePort`]
    /// when that guest's compatible ports are all occupied.
    pub fn attach_device(
        &mut self,
        bus_addr: u8,
        device: UsbDeviceId,
    ) -> Result<AttachOutcome, RegistryError> {
        let outcome = self.registry.attach(bus_addr, &device);
        if outcome.is_ok() {
            self.devices.insert(bus_addr, device);
        }
        outcome
    }

    /// Handle a physical disconnect: detach from whichever guest holds the
    /// device and forget its descriptor.
    pub fn note_disconnect(&mut self, bus_addr: u8) {
        let _ = self.registry.detach(bus_addr);
        self.devices.remove(&bus_addr);
    }

    /// Apply one console message, returning the reply messages for the
    /// socket task to send back:
    ///
    /// * `ClientMessage::RequestUsbDevices` → one `ServerMessage::UsbDeviceList`.
    /// * `ClientMessage::UsbCommand` → one `ServerMessage::CommandResponse`
    ///   (with the command's `id`), followed by a fresh
    ///   `ServerMessage::UsbDeviceList` when the command changed routing state.
    /// * anything else → no reply (this service only speaks USB).
    #[must_use]
    pub fn handle_client_message(&mut self, msg: &ClientMessage) -> Vec<ServerMessage> {
        match msg {
            ClientMessage::RequestUsbDevices => {
                vec![ServerMessage::UsbDeviceList(self.device_list())]
            }
            ClientMessage::UsbCommand { id, action } => self.apply_usb_command(*id, action),
            _ => Vec::new(),
        }
    }

    /// The current device inventory for the console's USB tab, ordered by
    /// bus address.
    #[must_use]
    pub fn device_list(&self) -> Vec<UsbDeviceEntry> {
        let mut entries: Vec<UsbDeviceEntry> = self
            .devices
            .iter()
            .map(|(bus_addr, device)| {
                let placement = self.registry.placement(*bus_addr);
                UsbDeviceEntry {
                    bus_addr: *bus_addr,
                    vendor_id: device.vendor_id,
                    product_id: device.product_id,
                    product: device.product.clone(),
                    manufacturer: device.manufacturer.clone(),
                    serial: device.serial.clone(),
                    port_path: device.port_path.clone(),
                    speed: speed_label(device.speed).to_owned(),
                    assigned_guest: placement.map(|p| p.guest.clone()),
                    guest_port: placement.map(|p| p.port),
                }
            })
            .collect();
        entries.sort_by_key(|entry| entry.bus_addr);
        entries
    }

    /// Apply a [`UsbAction`] from the console and build its replies.
    fn apply_usb_command(&mut self, id: u64, action: &UsbAction) -> Vec<ServerMessage> {
        let outcome = match action {
            UsbAction::Reassign {
                bus_addr,
                target_guest,
            } => self.reassign(*bus_addr, target_guest),
            UsbAction::Detach { bus_addr } => self.detach(*bus_addr),
        };
        let (success, message) = match outcome {
            Ok(ok) => (true, ok),
            Err(err) => (false, err),
        };
        let mut replies = vec![ServerMessage::CommandResponse {
            id,
            success,
            message,
        }];
        if success {
            replies.push(ServerMessage::UsbDeviceList(self.device_list()));
        }
        replies
    }

    /// Live-move `bus_addr` to `target_guest`: virtual unplug from the
    /// current holder, replug on the target. Both guests see hot-plug events.
    fn reassign(&mut self, bus_addr: u8, target_guest: &GuestId) -> Result<String, String> {
        let device = self
            .devices
            .get(&bus_addr)
            .ok_or_else(|| format!("no known USB device at bus address {bus_addr}"))?;
        match self
            .registry
            .reassign(bus_addr, target_guest.clone(), device)
        {
            Ok(placement) => Ok(format!(
                "device {bus_addr} moved to {target_guest} (port {})",
                placement.port
            )),
            Err(err) => Err(format!("reassign failed: {err}")),
        }
    }

    /// Detach `bus_addr` from its guest, back to the hypervisor. The device
    /// stays physically present, so its descriptor is kept for a later
    /// re-attach.
    fn detach(&mut self, bus_addr: u8) -> Result<String, String> {
        match self.registry.detach(bus_addr) {
            Ok(placement) => Ok(format!(
                "device {bus_addr} detached from {} (was port {})",
                placement.guest, placement.port
            )),
            Err(err) => Err(format!("detach failed: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enlil_devices::usb::controller::VirtualXhciController;
    use enlil_devices::usb::routing::{DeviceMatcher, RoutingState, RoutingTable};
    use enlil_devices::usb::types::UsbDeviceClass;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn hid_device(vendor_id: u16, product_id: u16, product: &str) -> UsbDeviceId {
        UsbDeviceId {
            vendor_id,
            product_id,
            serial: None,
            port_path: Some(format!("1-{product_id:04x}")),
            class: UsbDeviceClass::Hid,
            speed: DeviceSpeed::Low,
            manufacturer: Some("Test".to_owned()),
            product: Some(product.to_owned()),
        }
    }

    /// Two guests, each with a virtual xHCI controller, and a mouse routed to
    /// guest-a.
    fn two_guest_service() -> (UsbService, UsbDeviceId) {
        let mut table = RoutingTable::new();
        table.add_rule(
            10,
            DeviceMatcher::VidPid {
                vendor_id: 0x046d,
                product_id: 0xc077,
            },
            "guest-a".to_owned(),
        );
        let routing = RoutingState::new(table);
        let mut registry = XhciRegistry::new(routing);
        for guest in ["guest-a", "guest-b"] {
            registry
                .register_controller(guest, Rc::new(RefCell::new(VirtualXhciController::new(4))));
        }
        let mut service = UsbService::new(registry);
        let mouse = hid_device(0x046d, 0xc077, "Test Mouse");
        service
            .attach_device(1, mouse.clone())
            .expect("mouse attaches to guest-a");
        (service, mouse)
    }

    fn command_response(replies: &[ServerMessage]) -> (u64, bool, &str) {
        match &replies[0] {
            ServerMessage::CommandResponse {
                id,
                success,
                message,
            } => (*id, *success, message.as_str()),
            other => panic!("expected CommandResponse, got {other:?}"),
        }
    }

    #[test]
    fn request_usb_devices_returns_inventory() {
        let (mut service, _) = two_guest_service();
        let replies = service.handle_client_message(&ClientMessage::RequestUsbDevices);
        assert_eq!(replies.len(), 1);
        let ServerMessage::UsbDeviceList(entries) = &replies[0] else {
            panic!("expected UsbDeviceList");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].bus_addr, 1);
        assert_eq!(entries[0].assigned_guest.as_deref(), Some("guest-a"));
        assert_eq!(entries[0].speed, "Low");
    }

    #[test]
    fn reassign_moves_device_between_guests() {
        let (mut service, _) = two_guest_service();
        let msg = ClientMessage::UsbCommand {
            id: 7,
            action: UsbAction::Reassign {
                bus_addr: 1,
                target_guest: "guest-b".to_owned(),
            },
        };
        let replies = service.handle_client_message(&msg);
        // CommandResponse + fresh device list.
        assert_eq!(replies.len(), 2);
        let (id, success, message) = command_response(&replies);
        assert_eq!(id, 7);
        assert!(success, "reassign failed: {message}");
        assert_eq!(
            service.registry().placement(1).map(|p| p.guest.as_str()),
            Some("guest-b")
        );
        let ServerMessage::UsbDeviceList(entries) = &replies[1] else {
            panic!("expected UsbDeviceList");
        };
        assert_eq!(entries[0].assigned_guest.as_deref(), Some("guest-b"));
    }

    #[test]
    fn reassign_unknown_device_fails_cleanly() {
        let (mut service, _) = two_guest_service();
        let msg = ClientMessage::UsbCommand {
            id: 3,
            action: UsbAction::Reassign {
                bus_addr: 99,
                target_guest: "guest-b".to_owned(),
            },
        };
        let replies = service.handle_client_message(&msg);
        // Failure: response only, no device-list refresh.
        assert_eq!(replies.len(), 1);
        let (_, success, message) = command_response(&replies);
        assert!(!success);
        assert!(message.contains("99"), "message: {message}");
    }

    #[test]
    fn reassign_to_guest_without_controller_fails() {
        let (mut service, _) = two_guest_service();
        let msg = ClientMessage::UsbCommand {
            id: 4,
            action: UsbAction::Reassign {
                bus_addr: 1,
                target_guest: "guest-c".to_owned(),
            },
        };
        let replies = service.handle_client_message(&msg);
        let (_, success, _) = command_response(&replies);
        assert!(!success);
        // The device was replugged on its original guest, never stranded.
        assert_eq!(
            service.registry().placement(1).map(|p| p.guest.as_str()),
            Some("guest-a")
        );
    }

    #[test]
    fn detach_returns_device_to_hypervisor() {
        let (mut service, _) = two_guest_service();
        let msg = ClientMessage::UsbCommand {
            id: 5,
            action: UsbAction::Detach { bus_addr: 1 },
        };
        let replies = service.handle_client_message(&msg);
        assert_eq!(replies.len(), 2);
        let (_, success, message) = command_response(&replies);
        assert!(success, "detach failed: {message}");
        assert!(service.registry().placement(1).is_none());
        let ServerMessage::UsbDeviceList(entries) = &replies[1] else {
            panic!("expected UsbDeviceList");
        };
        assert_eq!(entries[0].assigned_guest, None);
    }

    #[test]
    fn detach_unattached_device_fails() {
        let (mut service, _) = two_guest_service();
        // Disconnect the device physically first (no placement, no descriptor).
        service.note_disconnect(1);
        let msg = ClientMessage::UsbCommand {
            id: 6,
            action: UsbAction::Detach { bus_addr: 1 },
        };
        let replies = service.handle_client_message(&msg);
        let (_, success, _) = command_response(&replies);
        assert!(!success);
    }

    #[test]
    fn non_usb_message_gets_no_reply() {
        let (mut service, _) = two_guest_service();
        let replies = service.handle_client_message(&ClientMessage::RequestStatus);
        assert!(replies.is_empty());
    }
}
