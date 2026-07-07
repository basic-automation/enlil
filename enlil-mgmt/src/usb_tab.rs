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

use crate::protocol::{GuestId, UsbAction, UsbDeviceEntry};
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
}
