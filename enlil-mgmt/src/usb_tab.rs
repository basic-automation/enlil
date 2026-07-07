//! View-model for the management console's USB tab (Phase 4.5).
//!
//! This is the pure, headless state the ratatui USB tab renders and drives: the
//! live device inventory, a selection cursor, a bounded ring of hot-plug
//! notices, and builders that turn an operator keystroke into the
//! [`UsbAction`](crate::protocol::UsbAction) sent to `enlil-core`. Keeping it
//! free of any terminal/`ratatui` types lets the reassignment and hot-plug logic
//! be unit-tested without a TTY; the render layer is thin glue over
//! [`devices`](UsbTabState::devices), [`selected`](UsbTabState::selected), and
//! [`notices`](UsbTabState::notices).

use crate::protocol::{ClientMessage, GuestId, ServerMessage, UsbAction, UsbDeviceEntry};
use std::collections::VecDeque;

/// How many hot-plug notices the tab keeps for display before dropping the
/// oldest. Bounded so a long-running console does not grow without limit.
const MAX_NOTICES: usize = 64;

/// State backing the console's USB tab.
#[derive(Debug, Clone, Default)]
pub struct UsbTabState {
    devices: Vec<UsbDeviceEntry>,
    selected: usize,
    notices: VecDeque<String>,
}

impl UsbTabState {
    /// A fresh, empty tab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the device inventory (from a `UsbDeviceList` message), keeping the
    /// cursor on the *same physical device* by `bus_addr` when it is still
    /// present, so a hot-plug refresh does not make the selection jump. Falls back
    /// to clamping the cursor into range.
    pub fn set_devices(&mut self, devices: Vec<UsbDeviceEntry>) {
        let anchor = self.selected_device().map(|d| d.bus_addr);
        self.devices = devices;
        self.selected = anchor
            .and_then(|bus| self.devices.iter().position(|d| d.bus_addr == bus))
            .unwrap_or_else(|| self.selected.min(self.devices.len().saturating_sub(1)));
    }

    /// The current device inventory, in display order.
    #[must_use]
    pub fn devices(&self) -> &[UsbDeviceEntry] {
        &self.devices
    }

    /// Index of the selected row (0 when empty).
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// The selected device, or `None` when the inventory is empty.
    #[must_use]
    pub fn selected_device(&self) -> Option<&UsbDeviceEntry> {
        self.devices.get(self.selected)
    }

    /// Move the selection down one row (wrapping to the top), a no-op when empty.
    pub const fn select_next(&mut self) {
        if !self.devices.is_empty() {
            self.selected = (self.selected + 1) % self.devices.len();
        }
    }

    /// Move the selection up one row (wrapping to the bottom), a no-op when empty.
    pub const fn select_prev(&mut self) {
        if !self.devices.is_empty() {
            self.selected = (self.selected + self.devices.len() - 1) % self.devices.len();
        }
    }

    /// Record a hot-plug / routing-change notice for display, evicting the oldest
    /// once [`MAX_NOTICES`] is reached. `message` is the text carried by a
    /// [`ServerMessage::UsbHotplugNotice`](crate::protocol::ServerMessage), which
    /// the render loop destructures before calling this.
    pub fn record_hotplug(&mut self, message: &str) {
        if self.notices.len() == MAX_NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back(message.to_string());
    }

    /// The retained hot-plug notices, oldest first.
    #[must_use]
    pub const fn notices(&self) -> &VecDeque<String> {
        &self.notices
    }

    /// The most recent notice, if any (what a status line shows).
    #[must_use]
    pub fn latest_notice(&self) -> Option<&str> {
        self.notices.back().map(String::as_str)
    }

    /// Build a [`UsbAction::Reassign`] moving the selected device to `target`, or
    /// `None` when nothing is selected or it is already on `target` (no-op move).
    #[must_use]
    pub fn reassign_action(&self, target: &GuestId) -> Option<UsbAction> {
        let dev = self.selected_device()?;
        if dev.assigned_guest.as_ref() == Some(target) {
            return None;
        }
        Some(UsbAction::Reassign {
            bus_addr: dev.bus_addr,
            target_guest: target.clone(),
        })
    }

    /// Build a [`UsbAction::Detach`] for the selected device, or `None` when
    /// nothing is selected or it is already with the hypervisor (unassigned).
    #[must_use]
    pub fn detach_action(&self) -> Option<UsbAction> {
        let dev = self.selected_device()?;
        dev.assigned_guest.as_ref()?; // already with the hypervisor → nothing to detach
        Some(UsbAction::Detach {
            bus_addr: dev.bus_addr,
        })
    }

    /// Pick the next guest to route the selected device to, rotating through
    /// `guests` and skipping the device's current holder — the target a "cycle
    /// assignment" keystroke advances to. `None` when nothing is selected or
    /// there is no guest other than the current holder to move to.
    #[must_use]
    pub fn cycle_target(&self, guests: &[GuestId]) -> Option<GuestId> {
        let dev = self.selected_device()?;
        if guests.is_empty() {
            return None;
        }
        // Start scanning just past the current holder (or at the front when the
        // device is unassigned / its holder is no longer in the list).
        let start = dev
            .assigned_guest
            .as_ref()
            .and_then(|cur| guests.iter().position(|g| g == cur))
            .map_or(0, |i| i + 1);
        for offset in 0..guests.len() {
            let candidate = &guests[(start + offset) % guests.len()];
            if dev.assigned_guest.as_ref() != Some(candidate) {
                return Some(candidate.clone());
            }
        }
        None
    }
}

/// Bridges the [`UsbTabState`] view-model to the wire protocol.
///
/// Folds inbound [`ServerMessage`]s into the state and turns operator keystrokes
/// into the [`ClientMessage`]s the console sends to `enlil-core`. This is the
/// piece the render/event loop drives; it owns the command-id sequence so each
/// [`UsbAction`] is matched to its `CommandResponse`.
#[derive(Debug, Default)]
pub struct UsbTabController {
    state: UsbTabState,
    next_id: u64,
}

impl UsbTabController {
    /// A fresh controller with an empty tab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The backing view-model, for the renderer.
    #[must_use]
    pub const fn state(&self) -> &UsbTabState {
        &self.state
    }

    /// The backing view-model, for cursor-navigation keystrokes
    /// ([`UsbTabState::select_next`] / [`select_prev`](UsbTabState::select_prev)).
    pub const fn state_mut(&mut self) -> &mut UsbTabState {
        &mut self.state
    }

    /// Fold a server message into the tab, returning `true` if it changed
    /// USB-tab state (i.e. the tab should be redrawn). Non-USB messages are
    /// ignored and return `false`.
    pub fn handle_server_message(&mut self, msg: &ServerMessage) -> bool {
        match msg {
            ServerMessage::UsbDeviceList(devices) => {
                self.state.set_devices(devices.clone());
                true
            }
            ServerMessage::UsbHotplugNotice { message, .. } => {
                self.state.record_hotplug(message);
                true
            }
            _ => false,
        }
    }

    /// The message that (re)requests the full USB inventory — sent on entering
    /// the tab and to refresh.
    #[must_use]
    pub const fn request_devices() -> ClientMessage {
        ClientMessage::RequestUsbDevices
    }

    /// Build a [`ClientMessage::UsbCommand`] reassigning the selected device to
    /// `target`, with a fresh command id. `None` when nothing is selected or the
    /// move would be a no-op (already on `target`).
    pub fn reassign_selected(&mut self, target: &GuestId) -> Option<ClientMessage> {
        let action = self.state.reassign_action(target)?;
        Some(self.command(action))
    }

    /// Build a [`ClientMessage::UsbCommand`] detaching the selected device back to
    /// the hypervisor. `None` when nothing is selected or it is already
    /// unassigned.
    pub fn detach_selected(&mut self) -> Option<ClientMessage> {
        let action = self.state.detach_action()?;
        Some(self.command(action))
    }

    /// Advance the selected device's assignment to the next guest in `guests`
    /// (skipping its current holder) and build the reassignment command — the
    /// "cycle assignment" keystroke. `None` when nothing is selected or there is
    /// no other guest to move to.
    pub fn cycle_selected(&mut self, guests: &[GuestId]) -> Option<ClientMessage> {
        let target = self.state.cycle_target(guests)?;
        self.reassign_selected(&target)
    }

    /// Wrap a [`UsbAction`] in a command with the next id in sequence.
    const fn command(&mut self, action: UsbAction) -> ClientMessage {
        let id = self.next_id;
        self.next_id += 1;
        ClientMessage::UsbCommand { id, action }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(bus_addr: u8, assigned: Option<&str>) -> UsbDeviceEntry {
        UsbDeviceEntry {
            bus_addr,
            vendor_id: 0x1234,
            product_id: 0x5678,
            product: Some(format!("Device {bus_addr}")),
            manufacturer: None,
            serial: None,
            port_path: Some(format!("1-{bus_addr}")),
            speed: "High".into(),
            assigned_guest: assigned.map(String::from),
            guest_port: assigned.map(|_| 0),
        }
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut t = UsbTabState::new();
        t.set_devices(vec![dev(1, None), dev(2, None), dev(3, None)]);
        assert_eq!(t.selected(), 0);
        t.select_prev();
        assert_eq!(t.selected(), 2, "up from the top wraps to the bottom");
        t.select_next();
        assert_eq!(t.selected(), 0, "down from the bottom wraps to the top");
    }

    #[test]
    fn selection_sticks_to_the_device_across_refresh() {
        let mut t = UsbTabState::new();
        t.set_devices(vec![dev(1, None), dev(2, None), dev(3, None)]);
        t.select_next(); // now on bus_addr 2
        assert_eq!(t.selected_device().unwrap().bus_addr, 2);
        // A refresh where device 1 disappeared must keep the cursor on device 2.
        t.set_devices(vec![dev(2, None), dev(3, None)]);
        assert_eq!(t.selected_device().unwrap().bus_addr, 2);
    }

    #[test]
    fn refresh_clamps_when_selected_device_gone() {
        let mut t = UsbTabState::new();
        t.set_devices(vec![dev(1, None), dev(2, None), dev(3, None)]);
        t.select_next();
        t.select_next(); // on index 2 (bus_addr 3)
        t.set_devices(vec![dev(1, None)]); // device 3 gone, only one left
        assert_eq!(t.selected(), 0);
        assert_eq!(t.selected_device().unwrap().bus_addr, 1);
    }

    #[test]
    fn empty_tab_has_no_actions_and_ignores_nav() {
        let mut t = UsbTabState::new();
        t.select_next();
        t.select_prev();
        assert_eq!(t.selected(), 0);
        assert!(t.selected_device().is_none());
        assert!(t.detach_action().is_none());
        assert!(t.reassign_action(&"vm1".to_string()).is_none());
        assert!(t.cycle_target(&["vm1".to_string()]).is_none());
    }

    #[test]
    fn reassign_and_detach_actions_are_no_ops_when_redundant() {
        let mut t = UsbTabState::new();
        t.set_devices(vec![dev(7, Some("vm1"))]);
        // Already on vm1 → reassigning to vm1 is a no-op.
        assert!(t.reassign_action(&"vm1".to_string()).is_none());
        // Moving to vm2 yields a Reassign.
        assert_eq!(
            t.reassign_action(&"vm2".to_string()),
            Some(UsbAction::Reassign {
                bus_addr: 7,
                target_guest: "vm2".into()
            })
        );
        // Detach yields a Detach.
        assert_eq!(t.detach_action(), Some(UsbAction::Detach { bus_addr: 7 }));

        // An unassigned device cannot be detached.
        t.set_devices(vec![dev(7, None)]);
        assert!(t.detach_action().is_none());
    }

    #[test]
    fn cycle_target_rotates_and_skips_current_holder() {
        let guests = vec!["vm1".to_string(), "vm2".to_string(), "vm3".to_string()];
        let mut t = UsbTabState::new();

        // Unassigned device → first guest.
        t.set_devices(vec![dev(1, None)]);
        assert_eq!(t.cycle_target(&guests), Some("vm1".to_string()));

        // Held by vm2 → next is vm3.
        t.set_devices(vec![dev(1, Some("vm2"))]);
        assert_eq!(t.cycle_target(&guests), Some("vm3".to_string()));

        // Held by vm3 (last) → wraps to vm1.
        t.set_devices(vec![dev(1, Some("vm3"))]);
        assert_eq!(t.cycle_target(&guests), Some("vm1".to_string()));

        // Only one guest and the device already holds it → nowhere to move.
        t.set_devices(vec![dev(1, Some("vm1"))]);
        assert_eq!(t.cycle_target(&["vm1".to_string()]), None);
    }

    #[test]
    fn hotplug_notices_are_bounded_and_ordered() {
        let mut t = UsbTabState::new();
        for i in 0..(MAX_NOTICES + 10) {
            t.record_hotplug(&format!("event {i}"));
        }
        assert_eq!(t.notices().len(), MAX_NOTICES, "ring is bounded");
        assert_eq!(
            t.latest_notice(),
            Some(format!("event {}", MAX_NOTICES + 9).as_str())
        );
        // Oldest retained is the first one that was not evicted.
        assert_eq!(t.notices().front().map(String::as_str), Some("event 10"));
    }

    #[test]
    fn controller_folds_server_messages_into_the_tab() {
        let mut c = UsbTabController::new();
        // A device list populates the tab and asks for a redraw.
        let changed = c.handle_server_message(&ServerMessage::UsbDeviceList(vec![
            dev(1, Some("vm1")),
            dev(2, None),
        ]));
        assert!(changed);
        assert_eq!(c.state().devices().len(), 2);
        // A hotplug notice is recorded and asks for a redraw.
        assert!(c.handle_server_message(&ServerMessage::UsbHotplugNotice {
            message: "device 3 attached".into(),
            device: None,
        }));
        assert_eq!(c.state().latest_notice(), Some("device 3 attached"));
        // An unrelated (non-USB) message changes nothing.
        assert!(!c.handle_server_message(&ServerMessage::CommandResponse {
            id: 0,
            success: true,
            message: String::new(),
        }));
    }

    #[test]
    fn controller_builds_commands_with_increasing_ids() {
        let mut c = UsbTabController::new();
        c.handle_server_message(&ServerMessage::UsbDeviceList(vec![dev(5, Some("vm1"))]));

        // Reassign selected → UsbCommand with id 0 and a Reassign action.
        let msg = c.reassign_selected(&"vm2".to_string()).expect("reassign");
        match msg {
            ClientMessage::UsbCommand { id, action } => {
                assert_eq!(id, 0);
                assert_eq!(
                    action,
                    UsbAction::Reassign {
                        bus_addr: 5,
                        target_guest: "vm2".into()
                    }
                );
            }
            other => panic!("expected UsbCommand, got {other:?}"),
        }

        // Detach → next id 1.
        let msg = c.detach_selected().expect("detach");
        match msg {
            ClientMessage::UsbCommand { id, action } => {
                assert_eq!(id, 1, "command ids increase");
                assert_eq!(action, UsbAction::Detach { bus_addr: 5 });
            }
            other => panic!("expected UsbCommand, got {other:?}"),
        }

        // A redundant reassign (already on vm1) yields no command and no id burn.
        assert!(c.reassign_selected(&"vm1".to_string()).is_none());
    }

    #[test]
    fn controller_cycle_selected_advances_the_target() {
        let mut c = UsbTabController::new();
        c.handle_server_message(&ServerMessage::UsbDeviceList(vec![dev(1, Some("vm1"))]));
        let guests = vec!["vm1".to_string(), "vm2".to_string()];
        // Cycling from vm1 targets vm2.
        let msg = c.cycle_selected(&guests).expect("cycle produces a command");
        match msg {
            ClientMessage::UsbCommand { id, action } => {
                assert_eq!(id, 0);
                assert_eq!(
                    action,
                    UsbAction::Reassign {
                        bus_addr: 1,
                        target_guest: "vm2".into()
                    }
                );
            }
            other => panic!("expected UsbCommand, got {other:?}"),
        }
    }

    #[test]
    fn request_devices_is_the_inventory_request() {
        assert!(matches!(
            UsbTabController::request_devices(),
            ClientMessage::RequestUsbDevices
        ));
    }
}
