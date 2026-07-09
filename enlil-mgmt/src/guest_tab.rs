//! View-model for the management console's guest tab (Phase 3.5).
//!
//! The pure, headless state the ratatui guest tab renders and drives: the live
//! per-guest status list ([`GuestStatus`]), a selection cursor that sticks to a
//! guest across status refreshes, and builders that turn a start/stop/reboot
//! keystroke into the [`GuestAction`](crate::protocol::GuestAction) sent to
//! `enlil-core`. Like [`usb_tab`](crate::usb_tab) it is free of any terminal
//! type so the lifecycle logic is unit-testable without a TTY; the render layer
//! is thin glue over [`guests`](GuestTabState::guests) and
//! [`selected_guest`](GuestTabState::selected_guest).

use crate::protocol::{
    ClientMessage, GuestAction, GuestId, GuestState, GuestStatus, ServerMessage,
};

/// State backing the console's guest tab.
#[derive(Debug, Clone, Default)]
pub struct GuestTabState {
    guests: Vec<GuestStatus>,
    selected: usize,
}

impl GuestTabState {
    /// A fresh, empty tab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the guest list (from a `StatusUpdate` message), keeping the
    /// cursor on the *same guest* by `id` when it is still present so a status
    /// refresh does not make the selection jump; otherwise clamps the cursor
    /// into range.
    pub fn set_guests(&mut self, guests: Vec<GuestStatus>) {
        let anchor = self.selected_guest().map(|g| g.id.clone());
        self.guests = guests;
        self.selected = anchor
            .and_then(|id| self.guests.iter().position(|g| g.id == id))
            .unwrap_or_else(|| self.selected.min(self.guests.len().saturating_sub(1)));
    }

    /// The current guest list, in display order.
    #[must_use]
    pub fn guests(&self) -> &[GuestStatus] {
        &self.guests
    }

    /// Index of the selected row (0 when empty).
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// The selected guest, or `None` when the list is empty.
    #[must_use]
    pub fn selected_guest(&self) -> Option<&GuestStatus> {
        self.guests.get(self.selected)
    }

    /// Move the selection down one row (wrapping to the top), a no-op when empty.
    pub const fn select_next(&mut self) {
        if !self.guests.is_empty() {
            self.selected = (self.selected + 1) % self.guests.len();
        }
    }

    /// Move the selection up one row (wrapping to the bottom), a no-op when empty.
    pub const fn select_prev(&mut self) {
        if !self.guests.is_empty() {
            self.selected = (self.selected + self.guests.len() - 1) % self.guests.len();
        }
    }

    /// The `(guest_id, Start)` action for the selected guest, or `None` when
    /// nothing is selected or it is already running (start would be a no-op).
    #[must_use]
    pub fn start_action(&self) -> Option<(GuestId, GuestAction)> {
        let guest = self.selected_guest()?;
        (guest.state != GuestState::Running).then(|| (guest.id.clone(), GuestAction::Start))
    }

    /// The `(guest_id, Stop)` action for the selected guest, or `None` when
    /// nothing is selected or it is already stopped.
    #[must_use]
    pub fn stop_action(&self) -> Option<(GuestId, GuestAction)> {
        let guest = self.selected_guest()?;
        (guest.state != GuestState::Stopped).then(|| (guest.id.clone(), GuestAction::Stop))
    }

    /// The `(guest_id, Reboot)` action for the selected guest, or `None` when
    /// nothing is selected or it is not running (a stopped guest cannot reboot).
    #[must_use]
    pub fn reboot_action(&self) -> Option<(GuestId, GuestAction)> {
        let guest = self.selected_guest()?;
        (guest.state == GuestState::Running).then(|| (guest.id.clone(), GuestAction::Reboot))
    }
}

/// Bridges the [`GuestTabState`] view-model to the wire protocol.
///
/// Folds inbound [`ServerMessage::StatusUpdate`]s into the state and turns
/// start/stop/reboot keystrokes into the [`ClientMessage::GuestCommand`]s the
/// console sends, owning the command-id sequence so each command matches its
/// [`CommandResponse`](crate::protocol::ServerMessage::CommandResponse).
#[derive(Debug, Default)]
pub struct GuestTabController {
    state: GuestTabState,
    next_id: u64,
}

impl GuestTabController {
    /// A fresh controller with an empty tab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The backing view-model, for the renderer.
    #[must_use]
    pub const fn state(&self) -> &GuestTabState {
        &self.state
    }

    /// The backing view-model, for cursor-navigation keystrokes.
    pub const fn state_mut(&mut self) -> &mut GuestTabState {
        &mut self.state
    }

    /// Fold a server message into the tab, returning `true` if it changed
    /// guest-tab state (i.e. the tab should be redrawn). Non-status messages
    /// are ignored and return `false`.
    pub fn handle_server_message(&mut self, msg: &ServerMessage) -> bool {
        match msg {
            ServerMessage::StatusUpdate(guests) => {
                self.state.set_guests(guests.clone());
                true
            }
            _ => false,
        }
    }

    /// The message that (re)requests a status snapshot — sent on entering the
    /// tab and to refresh.
    #[must_use]
    pub const fn request_status() -> ClientMessage {
        ClientMessage::RequestStatus
    }

    /// Build a [`ClientMessage::GuestCommand`] starting the selected guest, or
    /// `None` when nothing is selected or it is already running.
    pub fn start_selected(&mut self) -> Option<ClientMessage> {
        let (guest_id, action) = self.state.start_action()?;
        Some(self.command(guest_id, action))
    }

    /// Build a [`ClientMessage::GuestCommand`] stopping the selected guest, or
    /// `None` when nothing is selected or it is already stopped.
    pub fn stop_selected(&mut self) -> Option<ClientMessage> {
        let (guest_id, action) = self.state.stop_action()?;
        Some(self.command(guest_id, action))
    }

    /// Build a [`ClientMessage::GuestCommand`] rebooting the selected guest, or
    /// `None` when nothing is selected or it is not running.
    pub fn reboot_selected(&mut self) -> Option<ClientMessage> {
        let (guest_id, action) = self.state.reboot_action()?;
        Some(self.command(guest_id, action))
    }

    /// Wrap a [`GuestAction`] in a command with the next id in sequence.
    const fn command(&mut self, guest_id: GuestId, action: GuestAction) -> ClientMessage {
        let id = self.next_id;
        self.next_id += 1;
        ClientMessage::GuestCommand {
            id,
            guest_id,
            action,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guest(id: &str, state: GuestState) -> GuestStatus {
        GuestStatus {
            id: id.to_string(),
            name: id.to_string(),
            state,
            cpu_percent: 0.0,
            memory_used_mib: 0,
            memory_total_mib: 1024,
            uptime_secs: 0,
        }
    }

    #[test]
    fn selection_sticks_to_the_same_guest_across_refreshes() {
        let mut tab = GuestTabState::new();
        tab.set_guests(vec![
            guest("a", GuestState::Running),
            guest("b", GuestState::Running),
            guest("c", GuestState::Stopped),
        ]);
        tab.select_next(); // now on "b"
        assert_eq!(tab.selected_guest().unwrap().id, "b");

        // A refresh that reorders and drops "a" keeps the cursor on "b".
        tab.set_guests(vec![
            guest("c", GuestState::Stopped),
            guest("b", GuestState::Running),
        ]);
        assert_eq!(tab.selected_guest().unwrap().id, "b");
    }

    #[test]
    fn navigation_wraps_and_is_a_noop_when_empty() {
        let mut tab = GuestTabState::new();
        tab.select_next(); // no panic when empty
        assert_eq!(tab.selected(), 0);
        tab.set_guests(vec![
            guest("a", GuestState::Running),
            guest("b", GuestState::Stopped),
        ]);
        tab.select_prev(); // wrap to the bottom
        assert_eq!(tab.selected_guest().unwrap().id, "b");
        tab.select_next(); // wrap back to the top
        assert_eq!(tab.selected_guest().unwrap().id, "a");
    }

    #[test]
    fn lifecycle_actions_respect_current_state() {
        let mut tab = GuestTabState::new();
        tab.set_guests(vec![
            guest("run", GuestState::Running),
            guest("stop", GuestState::Stopped),
        ]);

        // Running guest: start is a no-op; stop and reboot are valid.
        assert_eq!(tab.start_action(), None);
        assert_eq!(
            tab.stop_action(),
            Some(("run".to_string(), GuestAction::Stop))
        );
        assert_eq!(
            tab.reboot_action(),
            Some(("run".to_string(), GuestAction::Reboot))
        );

        // Stopped guest: start is valid; stop and reboot are no-ops.
        tab.select_next();
        assert_eq!(
            tab.start_action(),
            Some(("stop".to_string(), GuestAction::Start))
        );
        assert_eq!(tab.stop_action(), None);
        assert_eq!(tab.reboot_action(), None);
    }

    #[test]
    fn controller_folds_status_and_builds_commands_with_rising_ids() {
        let mut ctl = GuestTabController::new();
        // A non-status message is ignored.
        assert!(!ctl.handle_server_message(&ServerMessage::UsbDeviceList(vec![])));
        // A status update folds in and asks for a redraw.
        assert!(ctl.handle_server_message(&ServerMessage::StatusUpdate(vec![
            guest("run", GuestState::Running),
            guest("stop", GuestState::Stopped),
        ])));

        // Selected "run" (Running): start is a no-op, stop issues id 0.
        assert!(ctl.start_selected().is_none());
        match ctl.stop_selected() {
            Some(ClientMessage::GuestCommand {
                id,
                guest_id,
                action,
            }) => {
                assert_eq!(id, 0);
                assert_eq!(guest_id, "run");
                assert_eq!(action, GuestAction::Stop);
            }
            other => panic!("expected a stop command, got {other:?}"),
        }
        // Next command takes the next id.
        ctl.state_mut().select_next(); // "stop" (Stopped)
        match ctl.start_selected() {
            Some(ClientMessage::GuestCommand { id, action, .. }) => {
                assert_eq!(id, 1);
                assert_eq!(action, GuestAction::Start);
            }
            other => panic!("expected a start command, got {other:?}"),
        }
    }
}
