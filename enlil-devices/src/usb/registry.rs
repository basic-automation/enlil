//! The routing→controller binding: a registry of per-guest virtual xHCI
//! controllers that turns [`RoutingState`](super::routing::RoutingState)
//! decisions into actual port attachments.
//!
//! This is the layer the Phase 4.5 milestone exercises: devices plug in, the
//! policy engine picks a guest, and the device lands on **that guest's**
//! controller (hot-plug events and all); the management console moves a
//! device between guests at runtime by calling [`reassign`](XhciRegistry::reassign),
//! which virtually unplugs it from one controller and replugs it on another.

use std::collections::HashMap;

use super::controller::SharedXhci;
use super::emulated::UsbDeviceModel;
use super::routing::{RoutingDecision, RoutingState};
use super::types::{GuestId, UsbDeviceId};

/// Where a routed device currently sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePlacement {
    /// The guest whose controller holds the device.
    pub guest: GuestId,
    /// 0-based root-hub port index on that guest's controller.
    pub port: usize,
}

/// Outcome of routing a newly arrived device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachOutcome {
    /// The device was attached to the guest's controller at the port.
    Attached(DevicePlacement),
    /// No routing rule (and no default) claimed the device; it stays with
    /// the hypervisor.
    Unassigned,
}

/// Errors from the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The routing decision targeted a guest with no registered controller.
    #[error("no xHCI controller registered for guest {0:?}")]
    NoController(GuestId),

    /// Every protocol-compatible port on the target controller is occupied.
    #[error("no free port on guest {0:?}'s controller for this device")]
    NoFreePort(GuestId),

    /// The device is not currently placed on any controller.
    #[error("device at bus address {0} is not attached")]
    NotAttached(u8),
}

/// Registry of per-guest virtual xHCI controllers, keeping the routing
/// engine's decisions and the controllers' port state in lockstep.
///
/// Holds the same controller handles the platform's MMIO adapters hold, so
/// an attach here raises the Port Status Change interrupt the target
/// guest's driver is waiting on.
pub struct XhciRegistry {
    /// One controller handle per guest.
    controllers: HashMap<GuestId, SharedXhci>,
    /// Live placements by device bus address.
    placements: HashMap<u8, DevicePlacement>,
    /// The policy engine consulted on every arrival.
    routing: RoutingState,
}

impl XhciRegistry {
    /// An empty registry driven by `routing`.
    #[must_use]
    pub fn new(routing: RoutingState) -> Self {
        Self {
            controllers: HashMap::new(),
            placements: HashMap::new(),
            routing,
        }
    }

    /// Register `guest`'s virtual xHCI controller (the same shared handle
    /// its MMIO window uses). Replaces any previous handle for that guest.
    pub fn register_controller(&mut self, guest: impl Into<GuestId>, controller: SharedXhci) {
        self.controllers.insert(guest.into(), controller);
    }

    /// Route a newly arrived device and attach it to the decided guest's
    /// controller, on the lowest free port matching the device's protocol.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NoController`] if the routing decision targets an
    /// unregistered guest, [`RegistryError::NoFreePort`] if that guest's
    /// compatible ports are all occupied. In both cases the routing-state
    /// assignment is rolled back so the device stays unassigned.
    pub fn attach(
        &mut self,
        bus_addr: u8,
        device: &UsbDeviceId,
    ) -> Result<AttachOutcome, RegistryError> {
        self.attach_with_model(bus_addr, device, None)
    }

    /// [`attach`](Self::attach), additionally parking a device model at the
    /// chosen port so Address Device binds it to the guest's slot — the
    /// path for devices with a backing (emulated today, libusb later).
    ///
    /// # Errors
    ///
    /// As for [`attach`](Self::attach).
    pub fn attach_with_model(
        &mut self,
        bus_addr: u8,
        device: &UsbDeviceId,
        model: Option<Box<dyn UsbDeviceModel>>,
    ) -> Result<AttachOutcome, RegistryError> {
        let RoutingDecision::RouteToGuest(guest) = self.routing.assign_device(bus_addr, device)
        else {
            return Ok(AttachOutcome::Unassigned);
        };
        match self.connect_to_guest(&guest, device, model) {
            Ok(port) => {
                let placement = DevicePlacement { guest, port };
                self.placements.insert(bus_addr, placement.clone());
                Ok(AttachOutcome::Attached(placement))
            }
            Err(err) => {
                let _ = self.routing.unassign_device(bus_addr);
                Err(err)
            }
        }
    }

    /// Detach a device (host-side unplug or guest shutdown): disconnects
    /// the controller port and clears the routing assignment.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotAttached`] if the device has no placement.
    pub fn detach(&mut self, bus_addr: u8) -> Result<DevicePlacement, RegistryError> {
        let placement = self
            .placements
            .remove(&bus_addr)
            .ok_or(RegistryError::NotAttached(bus_addr))?;
        let _ = self.disconnect_port(&placement);
        let _ = self.routing.unassign_device(bus_addr);
        Ok(placement)
    }

    /// Live-reassign a device to `new_guest` (the management console's
    /// "move device" action): virtually unplug from the current guest's
    /// controller, replug on the new one, and update the routing state.
    /// Both guests see real hot-plug events.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotAttached`] if the device is not placed;
    /// [`RegistryError::NoController`] / [`RegistryError::NoFreePort`] if
    /// the target cannot take it — the device is then replugged on its
    /// original guest, so a failed move never strands it.
    pub fn reassign(
        &mut self,
        bus_addr: u8,
        new_guest: impl Into<GuestId>,
        device: &UsbDeviceId,
    ) -> Result<DevicePlacement, RegistryError> {
        let new_guest: GuestId = new_guest.into();
        let old = self
            .placements
            .get(&bus_addr)
            .cloned()
            .ok_or(RegistryError::NotAttached(bus_addr))?;
        // A model still parked at the old port (i.e. the guest had not
        // addressed it into a slot) moves with the device.
        let model = self.disconnect_port(&old);
        match self.connect_to_guest(&new_guest, device, model) {
            Ok(port) => {
                let placement = DevicePlacement {
                    guest: new_guest.clone(),
                    port,
                };
                self.placements.insert(bus_addr, placement.clone());
                let _ = self.routing.reassign_device(bus_addr, new_guest);
                Ok(placement)
            }
            Err(err) => {
                // Roll back: replug on the original guest (its compatible
                // port is free again, so this cannot fail). The model was
                // consumed by the failed attempt only if a port was found,
                // which it was not — but it cannot be recovered through the
                // error path, so the replug is modelless; the caller can
                // re-park one via attach_with_model after a detach.
                if let Ok(port) = self.connect_to_guest(&old.guest, device, None) {
                    self.placements.insert(
                        bus_addr,
                        DevicePlacement {
                            guest: old.guest,
                            port,
                        },
                    );
                }
                Err(err)
            }
        }
    }

    /// The current placement of a device, if any.
    #[must_use]
    pub fn placement(&self, bus_addr: u8) -> Option<&DevicePlacement> {
        self.placements.get(&bus_addr)
    }

    /// The registered controller handle for a guest.
    #[must_use]
    pub fn controller(&self, guest: &str) -> Option<&SharedXhci> {
        self.controllers.get(guest)
    }

    /// All live placements (for the management console's USB tab).
    #[must_use]
    pub const fn placements(&self) -> &HashMap<u8, DevicePlacement> {
        &self.placements
    }

    /// Attach `device` to `guest`'s controller at the lowest free
    /// protocol-compatible port, parking `model` there if one is given.
    fn connect_to_guest(
        &self,
        guest: &GuestId,
        device: &UsbDeviceId,
        model: Option<Box<dyn UsbDeviceModel>>,
    ) -> Result<usize, RegistryError> {
        let controller = self
            .controllers
            .get(guest)
            .ok_or_else(|| RegistryError::NoController(guest.clone()))?;
        let mut controller = controller.borrow_mut();
        match model {
            Some(model) => controller.attach_device_with_model(device.speed, model),
            None => controller.attach_device(device.speed),
        }
        .ok_or_else(|| RegistryError::NoFreePort(guest.clone()))
    }

    /// Disconnect the placement's port (a no-op if the guest's controller
    /// was unregistered in the meantime), returning any model still parked
    /// there so a reassignment can carry it along.
    fn disconnect_port(&self, placement: &DevicePlacement) -> Option<Box<dyn UsbDeviceModel>> {
        self.controllers.get(&placement.guest).and_then(|handle| {
            let mut controller = handle.borrow_mut();
            let model = controller.take_port_model(placement.port);
            controller.disconnect_device(placement.port);
            model
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::super::controller::VirtualXhciController;
    use super::super::routing::{DeviceMatcher, RoutingTable};
    use super::super::types::{DeviceSpeed, UsbDeviceClass};
    use super::*;

    fn running_controller(ports: u8) -> SharedXhci {
        let mut c = VirtualXhciController::new(ports);
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x38, 8); // CONFIG: MaxSlotsEn
        c.write_register(op, 1); // USBCMD: R/S
        Rc::new(RefCell::new(c))
    }

    fn device(vid: u16, pid: u16, speed: DeviceSpeed, class: UsbDeviceClass) -> UsbDeviceId {
        UsbDeviceId {
            vendor_id: vid,
            product_id: pid,
            class,
            speed,
            serial: None,
            manufacturer: None,
            product: None,
            port_path: None,
        }
    }

    /// Two guests, two mice + two keyboards (the Phase 4.5 milestone in
    /// miniature): VID:PID rules send one of each to each guest.
    fn milestone_registry() -> XhciRegistry {
        let mut table = RoutingTable::new();
        table.add_rule(
            1,
            DeviceMatcher::VidPid {
                vendor_id: 0x046D,
                product_id: 0xC077,
            },
            "linux1".into(),
        );
        table.add_rule(
            1,
            DeviceMatcher::VidPid {
                vendor_id: 0x046D,
                product_id: 0xC534,
            },
            "linux1".into(),
        );
        table.add_rule(
            1,
            DeviceMatcher::VidPid {
                vendor_id: 0x1532,
                product_id: 0x0084,
            },
            "linux2".into(),
        );
        table.add_rule(
            1,
            DeviceMatcher::VidPid {
                vendor_id: 0x1532,
                product_id: 0x010E,
            },
            "linux2".into(),
        );
        let mut registry = XhciRegistry::new(RoutingState::new(table));
        registry.register_controller("linux1", running_controller(4));
        registry.register_controller("linux2", running_controller(4));
        registry
    }

    use super::super::routing::RoutingState;

    #[test]
    fn devices_land_on_the_guest_the_policy_picked() {
        let mut registry = milestone_registry();
        let mouse1 = device(0x046D, 0xC077, DeviceSpeed::Low, UsbDeviceClass::Hid);
        let kbd1 = device(0x046D, 0xC534, DeviceSpeed::Full, UsbDeviceClass::Hid);
        let mouse2 = device(0x1532, 0x0084, DeviceSpeed::Full, UsbDeviceClass::Hid);
        let kbd2 = device(0x1532, 0x010E, DeviceSpeed::Full, UsbDeviceClass::Hid);

        // Each device lands on its guest's controller, ports filling lowest
        // first per controller.
        for (addr, dev, guest, port) in [
            (1, &mouse1, "linux1", 0),
            (2, &kbd1, "linux1", 1),
            (3, &mouse2, "linux2", 0),
            (4, &kbd2, "linux2", 1),
        ] {
            match registry.attach(addr, dev) {
                Ok(AttachOutcome::Attached(p)) => {
                    assert_eq!(p.guest, guest);
                    assert_eq!(p.port, port);
                }
                other => panic!("expected attach to {guest}, got {other:?}"),
            }
        }

        // The controllers really saw the connects: CCS set on both ports,
        // and the hot-plug events are pending on each guest's event ring.
        for guest in ["linux1", "linux2"] {
            let handle = registry.controller(guest).unwrap();
            let mut c = handle.borrow_mut();
            for port in [0_u32, 1] {
                let portsc = c.read_register(0x20 + 0x400 + port * 16);
                assert_eq!(portsc & 1, 1, "{guest} port {port} CCS");
            }
            assert!(c.pop_event().is_some(), "{guest} has hot-plug events");
        }
    }

    #[test]
    fn unmatched_device_stays_with_the_hypervisor() {
        let mut registry = milestone_registry();
        let stranger = device(
            0xAAAA,
            0xBBBB,
            DeviceSpeed::High,
            UsbDeviceClass::MassStorage,
        );
        assert_eq!(
            registry.attach(9, &stranger).unwrap(),
            AttachOutcome::Unassigned
        );
        assert!(registry.placement(9).is_none());
    }

    #[test]
    fn live_reassignment_moves_the_device_between_controllers() {
        let mut registry = milestone_registry();
        let mouse = device(0x046D, 0xC077, DeviceSpeed::Low, UsbDeviceClass::Hid);
        let _ = registry.attach(1, &mouse).unwrap();
        // Drain linux1's attach event so the next one is the disconnect.
        let _ = registry
            .controller("linux1")
            .unwrap()
            .borrow_mut()
            .pop_event();

        // The management console moves the mouse to linux2.
        let placement = registry.reassign(1, "linux2", &mouse).unwrap();
        assert_eq!(placement.guest, "linux2");

        // linux1 saw the unplug (port 0 CCS clear + a port-change event);
        // linux2 saw the plug.
        let linux1 = registry.controller("linux1").unwrap();
        {
            let mut c = linux1.borrow_mut();
            assert_eq!(c.read_register(0x20 + 0x400) & 1, 0, "linux1 CCS clear");
            assert!(c.pop_event().is_some(), "linux1 disconnect event");
        }
        let linux2 = registry.controller("linux2").unwrap();
        {
            let mut c = linux2.borrow_mut();
            assert_eq!(c.read_register(0x20 + 0x400) & 1, 1, "linux2 CCS set");
            assert!(c.pop_event().is_some(), "linux2 connect event");
        }
    }

    #[test]
    fn failed_reassignment_replugs_on_the_original_guest() {
        let mut registry = milestone_registry();
        let mouse = device(0x046D, 0xC077, DeviceSpeed::Low, UsbDeviceClass::Hid);
        let _ = registry.attach(1, &mouse).unwrap();

        // Fill every USB2 port on linux2 (4 ports → 2 are USB2).
        {
            let linux2 = registry.controller("linux2").unwrap();
            let mut c = linux2.borrow_mut();
            assert!(c.attach_device(DeviceSpeed::Full).is_some());
            assert!(c.attach_device(DeviceSpeed::Full).is_some());
        }

        let err = registry.reassign(1, "linux2", &mouse).unwrap_err();
        assert!(matches!(err, RegistryError::NoFreePort(_)));
        // The mouse is back on linux1, still tracked.
        let placement = registry.placement(1).unwrap();
        assert_eq!(placement.guest, "linux1");
        let linux1 = registry.controller("linux1").unwrap();
        assert_eq!(
            linux1.borrow().read_register(0x20 + 0x400) & 1,
            1,
            "replugged on linux1"
        );
    }

    #[test]
    fn reassignment_to_an_unknown_guest_fails_and_rolls_back() {
        let mut registry = milestone_registry();
        let mouse = device(0x046D, 0xC077, DeviceSpeed::Low, UsbDeviceClass::Hid);
        let _ = registry.attach(1, &mouse).unwrap();

        let err = registry.reassign(1, "windows-ghost", &mouse).unwrap_err();
        assert!(matches!(err, RegistryError::NoController(_)));
        assert_eq!(registry.placement(1).unwrap().guest, "linux1");
    }

    #[test]
    fn reassignment_carries_the_parked_device_model() {
        use super::super::emulated::LoopbackDevice;
        let mut registry = milestone_registry();
        let mouse = device(0x046D, 0xC077, DeviceSpeed::Low, UsbDeviceClass::Hid);
        let outcome = registry
            .attach_with_model(
                1,
                &mouse,
                Some(Box::new(LoopbackDevice::new(0x046D, 0xC077))),
            )
            .unwrap();
        let AttachOutcome::Attached(placement) = outcome else {
            panic!("expected attach");
        };

        // The model rides the live reassignment to linux2's controller and
        // is parked at the new port for the guest's Address Device.
        let moved = registry.reassign(1, "linux2", &mouse).unwrap();
        assert_eq!(moved.guest, "linux2");
        let linux1 = registry.controller("linux1").unwrap();
        assert!(
            linux1
                .borrow_mut()
                .take_port_model(placement.port)
                .is_none(),
            "old port no longer holds the model"
        );
        let linux2 = registry.controller("linux2").unwrap();
        assert!(
            linux2.borrow_mut().take_port_model(moved.port).is_some(),
            "model parked at the new guest's port"
        );
    }

    #[test]
    fn detach_clears_the_port_and_the_assignment() {
        let mut registry = milestone_registry();
        let mouse = device(0x046D, 0xC077, DeviceSpeed::Low, UsbDeviceClass::Hid);
        let _ = registry.attach(1, &mouse).unwrap();

        let placement = registry.detach(1).unwrap();
        assert_eq!(placement.guest, "linux1");
        assert!(registry.placement(1).is_none());
        assert!(matches!(
            registry.detach(1),
            Err(RegistryError::NotAttached(1))
        ));

        // The port is free again: the next matching device takes port 0.
        match registry.attach(2, &mouse).unwrap() {
            AttachOutcome::Attached(p) => assert_eq!(p.port, 0),
            AttachOutcome::Unassigned => panic!("expected re-attach, got Unassigned"),
        }
    }

    #[test]
    fn attach_to_unregistered_guest_rolls_back_the_assignment() {
        let mut table = RoutingTable::new();
        table.add_rule(1, DeviceMatcher::Any, "ghost".into());
        let mut registry = XhciRegistry::new(RoutingState::new(table));

        let dev = device(1, 2, DeviceSpeed::High, UsbDeviceClass::Hid);
        let err = registry.attach(1, &dev).unwrap_err();
        assert!(matches!(err, RegistryError::NoController(_)));
        assert!(registry.placement(1).is_none());
    }
}
