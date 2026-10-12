//! View-model for the management console's display tab (Phase 8.3).
//!
//! This is the pure, headless state the ratatui display tab renders and
//! drives: the live physical-monitor inventory, a selection cursor, a bounded
//! ring of hot-plug notices, and builders that turn an operator keystroke into
//! the [`DisplayAction`](crate::protocol::DisplayAction) sent to `enlil-core`.
//! It mirrors the USB tab's structure (inventory + cursor + notices +
//! protocol bridge), but routes *monitors* to guests and adds the KVM-switch
//! keystroke that jumps straight to the selected monitor's guest. Keeping it
//! free of any terminal/`ratatui` types lets the assignment and hot-plug logic
//! be unit-tested without a TTY.

use crate::protocol::{ClientMessage, DisplayAction, DisplayMonitorEntry, GuestId, ServerMessage};
use std::collections::VecDeque;

/// How many hot-plug notices the tab keeps for display before dropping the
/// oldest. Bounded so a long-running console does not grow without limit.
const MAX_NOTICES: usize = 64;

/// State backing the console's display tab.
#[derive(Debug, Clone, Default)]
pub struct DisplayTabState {
    monitors: Vec<DisplayMonitorEntry>,
    selected: usize,
    notices: VecDeque<String>,
}

impl DisplayTabState {
    /// A fresh, empty tab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the monitor inventory (from a `DisplayMonitorList` message),
    /// keeping the cursor on the *same physical monitor* by `id` when it is
    /// still present, so a hot-plug refresh does not make the selection jump.
    /// Falls back to clamping the cursor into range.
    pub fn set_monitors(&mut self, monitors: Vec<DisplayMonitorEntry>) {
        let anchor = self.selected_monitor().map(|m| m.id);
        self.monitors = monitors;
        self.selected = anchor
            .and_then(|id| self.monitors.iter().position(|m| m.id == id))
            .unwrap_or_else(|| self.selected.min(self.monitors.len().saturating_sub(1)));
    }

    /// The current monitor inventory, in display order.
    #[must_use]
    pub fn monitors(&self) -> &[DisplayMonitorEntry] {
        &self.monitors
    }

    /// Index of the selected row (0 when empty).
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// The selected monitor, or `None` when the inventory is empty.
    #[must_use]
    pub fn selected_monitor(&self) -> Option<&DisplayMonitorEntry> {
        self.monitors.get(self.selected)
    }

    /// Move the selection down one row (wrapping to the top), a no-op when empty.
    pub const fn select_next(&mut self) {
        if !self.monitors.is_empty() {
            self.selected = (self.selected + 1) % self.monitors.len();
        }
    }

    /// Move the selection up one row (wrapping to the bottom), a no-op when empty.
    pub const fn select_prev(&mut self) {
        if !self.monitors.is_empty() {
            self.selected = (self.selected + self.monitors.len() - 1) % self.monitors.len();
        }
    }

    /// Record a hot-plug / routing-change notice for display, evicting the
    /// oldest once [`MAX_NOTICES`] is reached.
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

    /// Build a [`DisplayAction::Assign`] giving the selected monitor to
    /// `target`, or `None` when nothing is selected or it is already on
    /// `target` (no-op move).
    #[must_use]
    pub fn assign_action(&self, target: &GuestId) -> Option<DisplayAction> {
        let mon = self.selected_monitor()?;
        if mon.assigned_guest.as_ref() == Some(target) {
            return None;
        }
        Some(DisplayAction::Assign {
            monitor_id: mon.id,
            target_guest: target.clone(),
        })
    }

    /// Build a [`DisplayAction::Release`] for the selected monitor, or `None`
    /// when nothing is selected or it is already with the hypervisor.
    #[must_use]
    pub fn release_action(&self) -> Option<DisplayAction> {
        let mon = self.selected_monitor()?;
        mon.assigned_guest.as_ref()?; // already with the hypervisor → nothing to release
        Some(DisplayAction::Release { monitor_id: mon.id })
    }

    /// Build a [`DisplayAction::KvmSwitchTo`] for the selected monitor's
    /// guest — the console's KVM-switch keystroke — or `None` when nothing is
    /// selected or the monitor is unassigned.
    #[must_use]
    pub fn kvm_switch_action(&self) -> Option<DisplayAction> {
        let mon = self.selected_monitor()?;
        mon.assigned_guest
            .as_ref()
            .map(|guest_id| DisplayAction::KvmSwitchTo {
                guest_id: guest_id.clone(),
            })
    }

    /// Pick the next guest to route the selected monitor to, rotating through
    /// `guests` and skipping the monitor's current holder — the target a
    /// "cycle assignment" keystroke advances to. `None` when nothing is
    /// selected or there is no guest other than the current holder to move to.
    #[must_use]
    pub fn cycle_target(&self, guests: &[GuestId]) -> Option<GuestId> {
        let mon = self.selected_monitor()?;
        if guests.is_empty() {
            return None;
        }
        // Start scanning just past the current holder (or at the front when the
        // monitor is unassigned / its holder is no longer in the list).
        let start = mon
            .assigned_guest
            .as_ref()
            .and_then(|cur| guests.iter().position(|g| g == cur))
            .map_or(0, |i| i + 1);
        for offset in 0..guests.len() {
            let candidate = &guests[(start + offset) % guests.len()];
            if mon.assigned_guest.as_ref() != Some(candidate) {
                return Some(candidate.clone());
            }
        }
        None
    }
}

/// Bridges the [`DisplayTabState`] view-model to the wire protocol.
///
/// Folds inbound [`ServerMessage`]s into the state and turns operator
/// keystrokes into the [`ClientMessage`]s the console sends to `enlil-core`.
/// This is the piece the render/event loop drives; it owns the command-id
/// sequence so each [`DisplayAction`] is matched to its `CommandResponse`.
#[derive(Debug, Default)]
pub struct DisplayTabController {
    state: DisplayTabState,
    next_id: u64,
}

impl DisplayTabController {
    /// A fresh controller with an empty tab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The backing view-model, for the renderer.
    #[must_use]
    pub const fn state(&self) -> &DisplayTabState {
        &self.state
    }

    /// The backing view-model, for cursor-navigation keystrokes
    /// ([`DisplayTabState::select_next`] / [`select_prev`](DisplayTabState::select_prev)).
    pub const fn state_mut(&mut self) -> &mut DisplayTabState {
        &mut self.state
    }

    /// Fold a server message into the tab, returning `true` if it changed
    /// display-tab state (i.e. the tab should be redrawn). Non-display messages
    /// are ignored and return `false`.
    pub fn handle_server_message(&mut self, msg: &ServerMessage) -> bool {
        match msg {
            ServerMessage::DisplayMonitorList(monitors) => {
                self.state.set_monitors(monitors.clone());
                true
            }
            ServerMessage::DisplayHotplugNotice { message, .. } => {
                self.state.record_hotplug(message);
                true
            }
            _ => false,
        }
    }

    /// The message that (re)requests the full monitor inventory — sent on
    /// entering the tab and to refresh.
    #[must_use]
    pub const fn request_monitors() -> ClientMessage {
        ClientMessage::RequestDisplayMonitors
    }

    /// Build a [`ClientMessage::DisplayCommand`] assigning the selected monitor
    /// to `target`, with a fresh command id. `None` when nothing is selected
    /// or the move would be a no-op (already on `target`).
    pub fn assign_selected(&mut self, target: &GuestId) -> Option<ClientMessage> {
        let action = self.state.assign_action(target)?;
        Some(self.command(action))
    }

    /// Build a [`ClientMessage::DisplayCommand`] releasing the selected
    /// monitor back to the hypervisor. `None` when nothing is selected or it
    /// is already unassigned.
    pub fn release_selected(&mut self) -> Option<ClientMessage> {
        let action = self.state.release_action()?;
        Some(self.command(action))
    }

    /// Advance the selected monitor's assignment to the next guest in `guests`
    /// (skipping its current holder) and build the reassignment command — the
    /// "cycle assignment" keystroke. `None` when nothing is selected or there
    /// is no other guest to move to.
    pub fn cycle_selected(&mut self, guests: &[GuestId]) -> Option<ClientMessage> {
        let target = self.state.cycle_target(guests)?;
        self.assign_selected(&target)
    }

    /// Build a [`ClientMessage::DisplayCommand`] KVM-switching to the selected
    /// monitor's guest. `None` when nothing is selected or the monitor is
    /// unassigned.
    pub fn switch_selected_guest(&mut self) -> Option<ClientMessage> {
        let action = self.state.kvm_switch_action()?;
        Some(self.command(action))
    }

    /// Wrap a [`DisplayAction`] in a command with the next id in sequence.
    const fn command(&mut self, action: DisplayAction) -> ClientMessage {
        let id = self.next_id;
        self.next_id += 1;
        ClientMessage::DisplayCommand { id, action }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::VideoModeEntry;

    fn mon(id: u32, assigned: Option<&str>) -> DisplayMonitorEntry {
        DisplayMonitorEntry {
            id,
            connector: "HDMI".into(),
            name: Some(format!("Monitor {id}")),
            manufacturer: Some("ENC".into()),
            preferred: VideoModeEntry {
                width: 2560,
                height: 1440,
                refresh_hz: 144,
            },
            mode_count: 6,
            assigned_guest: assigned.map(String::from),
            plugged: true,
        }
    }

    #[test]
    fn selection_wraps_and_anchors_by_monitor_id() {
        let mut t = DisplayTabState::new();
        t.set_monitors(vec![mon(0, None), mon(1, None), mon(2, None)]);
        t.select_prev();
        assert_eq!(t.selected(), 2, "up from the top wraps to the bottom");
        t.select_next();
        assert_eq!(t.selected(), 0);
        // A refresh keeps the cursor on the same physical monitor.
        t.select_next(); // on monitor 1
        t.set_monitors(vec![mon(1, None), mon(2, None), mon(3, None)]);
        assert_eq!(t.selected_monitor().unwrap().id, 1);
    }

    #[test]
    fn empty_tab_has_no_actions_and_ignores_nav() {
        let mut t = DisplayTabState::new();
        t.select_next();
        t.select_prev();
        assert_eq!(t.selected(), 0);
        assert!(t.selected_monitor().is_none());
        assert!(t.assign_action(&"vm1".to_string()).is_none());
        assert!(t.release_action().is_none());
        assert!(t.kvm_switch_action().is_none());
        assert!(t.cycle_target(&["vm1".to_string()]).is_none());
    }

    #[test]
    fn assign_release_and_switch_actions() {
        let mut t = DisplayTabState::new();
        t.set_monitors(vec![mon(0, Some("vm1"))]);
        // Already on vm1 → assigning to vm1 is a no-op.
        assert!(t.assign_action(&"vm1".to_string()).is_none());
        assert_eq!(
            t.assign_action(&"vm2".to_string()),
            Some(DisplayAction::Assign {
                monitor_id: 0,
                target_guest: "vm2".into()
            })
        );
        assert_eq!(
            t.release_action(),
            Some(DisplayAction::Release { monitor_id: 0 })
        );
        assert_eq!(
            t.kvm_switch_action(),
            Some(DisplayAction::KvmSwitchTo {
                guest_id: "vm1".into()
            })
        );

        // Unassigned: release and switch are no-ops, assign works.
        t.set_monitors(vec![mon(0, None)]);
        assert!(t.release_action().is_none());
        assert!(t.kvm_switch_action().is_none());
        assert_eq!(
            t.assign_action(&"vm1".to_string()),
            Some(DisplayAction::Assign {
                monitor_id: 0,
                target_guest: "vm1".into()
            })
        );
    }

    #[test]
    fn cycle_target_rotates_and_skips_current_holder() {
        let guests = vec!["vm1".to_string(), "vm2".to_string()];
        let mut t = DisplayTabState::new();
        t.set_monitors(vec![mon(0, None)]);
        assert_eq!(t.cycle_target(&guests), Some("vm1".to_string()));
        t.set_monitors(vec![mon(0, Some("vm1"))]);
        assert_eq!(t.cycle_target(&guests), Some("vm2".to_string()));
        t.set_monitors(vec![mon(0, Some("vm2"))]);
        assert_eq!(t.cycle_target(&guests), Some("vm1".to_string()));
        assert!(t.cycle_target(&["vm2".to_string()]).is_none());
    }

    #[test]
    fn hotplug_notices_are_bounded() {
        let mut t = DisplayTabState::new();
        for i in 0..(MAX_NOTICES + 5) {
            t.record_hotplug(&format!("event {i}"));
        }
        assert_eq!(t.notices().len(), MAX_NOTICES);
        assert_eq!(
            t.latest_notice(),
            Some(format!("event {}", MAX_NOTICES + 4).as_str())
        );
    }

    #[test]
    fn controller_folds_server_messages_and_builds_commands() {
        let mut c = DisplayTabController::new();
        assert!(
            c.handle_server_message(&ServerMessage::DisplayMonitorList(vec![
                mon(0, Some("vm1")),
                mon(1, None),
            ]))
        );
        assert_eq!(c.state().monitors().len(), 2);
        assert!(
            c.handle_server_message(&ServerMessage::DisplayHotplugNotice {
                message: "monitor 1 unplugged".into(),
                monitor: None,
            })
        );
        assert_eq!(c.state().latest_notice(), Some("monitor 1 unplugged"));
        assert!(!c.handle_server_message(&ServerMessage::CommandResponse {
            id: 0,
            success: true,
            message: String::new(),
        }));

        // Assign selected (monitor 0, on vm1) to vm2 → DisplayCommand id 0.
        match c
            .assign_selected(&"vm2".to_string())
            .expect("assign command")
        {
            ClientMessage::DisplayCommand { id, action } => {
                assert_eq!(id, 0);
                assert_eq!(
                    action,
                    DisplayAction::Assign {
                        monitor_id: 0,
                        target_guest: "vm2".into()
                    }
                );
            }
            other => panic!("expected DisplayCommand, got {other:?}"),
        }
        // Release → id 1.
        match c.release_selected().expect("release command") {
            ClientMessage::DisplayCommand { id, action } => {
                assert_eq!(id, 1, "command ids increase");
                assert_eq!(action, DisplayAction::Release { monitor_id: 0 });
            }
            other => panic!("expected DisplayCommand, got {other:?}"),
        }
        // KVM switch to vm1 → id 2.
        match c.switch_selected_guest().expect("switch command") {
            ClientMessage::DisplayCommand { id, action } => {
                assert_eq!(id, 2);
                assert_eq!(
                    action,
                    DisplayAction::KvmSwitchTo {
                        guest_id: "vm1".into()
                    }
                );
            }
            other => panic!("expected DisplayCommand, got {other:?}"),
        }
        // Cycling from vm1 with guests [vm1, vm2] targets vm2 → id 3.
        match c
            .cycle_selected(&["vm1".to_string(), "vm2".to_string()])
            .expect("cycle command")
        {
            ClientMessage::DisplayCommand { id, action } => {
                assert_eq!(id, 3);
                assert_eq!(
                    action,
                    DisplayAction::Assign {
                        monitor_id: 0,
                        target_guest: "vm2".into()
                    }
                );
            }
            other => panic!("expected DisplayCommand, got {other:?}"),
        }
    }

    #[test]
    fn request_monitors_is_the_inventory_request() {
        assert!(matches!(
            DisplayTabController::request_monitors(),
            ClientMessage::RequestDisplayMonitors
        ));
    }
}
