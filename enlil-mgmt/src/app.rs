#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! Interactive ratatui app host for the `enlil-mgmt` console (T-4.5).
//!
//! This is the piece the CLI-only binary was missing: a terminal event loop
//! plus the socket glue that drives the [`GuestTabController`] and
//! [`UsbTabController`] view-models built for phases 3.5 and 4.5. [`App`] is
//! the headless core — keystrokes in, [`ClientMessage`]s out, [`ServerMessage`]s
//! folded in, and a ratatui [`render`](App::render) — so all of the interaction
//! logic is unit-testable without a TTY. [`run_tui`] is the thin async host
//! around it: it owns the TCP [`Connection`](crate::protocol::Connection) to
//! `enlil-core`, a crossterm key-reader thread, a socket-reader task, a
//! periodic refresh tick, and terminal setup/teardown.

use crate::guest_tab::GuestTabController;
use crate::protocol::{ClientMessage, Connection, GuestId, ServerMessage};
use crate::usb_tab::UsbTabController;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs};
use ratatui::{Frame, Terminal};
use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

/// Channel depth for keystrokes flowing from the reader thread to the loop.
const KEY_QUEUE: usize = 32;
/// Channel depth for server messages flowing from the socket task to the loop.
const NET_QUEUE: usize = 32;
/// How often the key-reader thread re-checks for shutdown.
const KEY_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How often the active tab's data is re-requested while the console runs.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// How many status lines the footer history keeps.
const MAX_STATUS: usize = 8;
/// How many hot-plug notices the USB tab shows.
const MAX_NOTICES_SHOWN: u16 = 4;

/// Which console tab is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTab {
    Guests,
    Usb,
}

impl ActiveTab {
    /// Index of the tab in the tab bar.
    const fn index(self) -> usize {
        match self {
            Self::Guests => 0,
            Self::Usb => 1,
        }
    }
}

/// Events arriving from the socket-reader task.
#[derive(Debug)]
enum NetEvent {
    Message(ServerMessage),
    Disconnected,
}

/// Headless interactive-console state: the two tab controllers, the active tab,
/// a bounded status-line history, and the quit/connected flags.
///
/// Keystrokes go in through [`on_key`](App::on_key) (which returns the
/// [`ClientMessage`]s the host must send), server traffic comes in through
/// [`on_server_message`](App::on_server_message), and the screen is produced by
/// [`render`](App::render). Nothing here touches a real terminal, so it is all
/// unit-testable.
#[derive(Debug)]
pub struct App {
    guests: GuestTabController,
    usb: UsbTabController,
    tab: ActiveTab,
    status: VecDeque<String>,
    connected: bool,
    quit: bool,
    addr: String,
}

impl App {
    /// A fresh console for the `enlil-core` endpoint at `addr` (display only;
    /// the actual socket is owned by [`run_tui`]).
    #[must_use]
    pub fn new(addr: &str) -> Self {
        Self {
            guests: GuestTabController::new(),
            usb: UsbTabController::new(),
            tab: ActiveTab::Guests,
            status: VecDeque::new(),
            connected: true,
            quit: false,
            addr: addr.to_string(),
        }
    }

    /// The guest tab controller (for tests and the render layer).
    #[must_use]
    pub const fn guest_tab(&self) -> &GuestTabController {
        &self.guests
    }

    /// The USB tab controller (for tests and the render layer).
    #[must_use]
    pub const fn usb_tab(&self) -> &UsbTabController {
        &self.usb
    }

    /// Whether the socket to `enlil-core` is still up.
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    /// Whether the operator asked to quit (q / Esc / Ctrl+C).
    #[must_use]
    pub const fn should_quit(&self) -> bool {
        self.quit
    }

    /// The newest status line, if any.
    #[must_use]
    pub fn latest_status(&self) -> Option<&str> {
        self.status.back().map(String::as_str)
    }

    /// Handle one keystroke, returning the client messages the host must send
    /// over the socket (empty when the key only moved local state).
    #[must_use]
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<ClientMessage> {
        // Ignore key-release/repeat events so one press is one action.
        if !matches!(key.kind, KeyEventKind::Press) {
            return Vec::new();
        }
        // Ctrl+C quits from anywhere.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            self.quit = true;
            return Vec::new();
        }
        // Ignore any other modified key.
        if !key.modifiers.is_empty() {
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('q' | 'Q') | KeyCode::Esc => {
                self.quit = true;
            }
            KeyCode::Tab => return self.cycle_tab(),
            KeyCode::Char('1') => return self.goto_tab(ActiveTab::Guests),
            KeyCode::Char('2') => return self.goto_tab(ActiveTab::Usb),
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.select_prev(),
            KeyCode::Char('R') => return vec![self.refresh_request()],
            other => return self.on_tab_key(other),
        }
        Vec::new()
    }

    /// Fold an inbound server message into the tabs and the status line.
    pub fn on_server_message(&mut self, msg: &ServerMessage) {
        self.guests.handle_server_message(msg);
        self.usb.handle_server_message(msg);
        match msg {
            ServerMessage::CommandResponse {
                success, message, ..
            } => {
                let outcome = if *success { "ok" } else { "FAILED" };
                self.push_status(format!("command {outcome}: {message}"));
            }
            ServerMessage::UsbHotplugNotice { message, .. } => {
                self.push_status(format!("usb: {message}"));
            }
            ServerMessage::StatusUpdate(_)
            | ServerMessage::UsbDeviceList(_)
            | ServerMessage::SerialData { .. } => {}
        }
    }

    /// Record that the socket died; the loop exits and the footer explains why.
    pub fn note_disconnect(&mut self) {
        self.connected = false;
        self.push_status("disconnected from enlil-core".to_string());
    }

    /// The data request for the currently visible tab (sent on tab switches
    /// and on the periodic refresh tick).
    const fn refresh_request(&self) -> ClientMessage {
        match self.tab {
            ActiveTab::Guests => ClientMessage::RequestStatus,
            ActiveTab::Usb => ClientMessage::RequestUsbDevices,
        }
    }

    /// Switch to `tab`, returning its data request so the view populates.
    fn goto_tab(&mut self, tab: ActiveTab) -> Vec<ClientMessage> {
        self.tab = tab;
        vec![self.refresh_request()]
    }

    /// Advance to the next tab, returning its data request.
    fn cycle_tab(&mut self) -> Vec<ClientMessage> {
        let next = match self.tab {
            ActiveTab::Guests => ActiveTab::Usb,
            ActiveTab::Usb => ActiveTab::Guests,
        };
        self.goto_tab(next)
    }

    /// Move the active tab's cursor down (wrapping).
    const fn select_next(&mut self) {
        match self.tab {
            ActiveTab::Guests => self.guests.state_mut().select_next(),
            ActiveTab::Usb => self.usb.state_mut().select_next(),
        }
    }

    /// Move the active tab's cursor up (wrapping).
    const fn select_prev(&mut self) {
        match self.tab {
            ActiveTab::Guests => self.guests.state_mut().select_prev(),
            ActiveTab::Usb => self.usb.state_mut().select_prev(),
        }
    }

    /// Tab-local keystrokes: guest lifecycle on the guest tab, USB routing on
    /// the USB tab.
    fn on_tab_key(&mut self, code: KeyCode) -> Vec<ClientMessage> {
        match self.tab {
            ActiveTab::Guests => match code {
                KeyCode::Char('s') => self.guests.start_selected().into_iter().collect(),
                KeyCode::Char('x') => self.guests.stop_selected().into_iter().collect(),
                KeyCode::Char('r') => self.guests.reboot_selected().into_iter().collect(),
                _ => Vec::new(),
            },
            ActiveTab::Usb => match code {
                KeyCode::Char('c') => {
                    let ids: Vec<GuestId> = self
                        .guests
                        .state()
                        .guests()
                        .iter()
                        .map(|guest| guest.id.clone())
                        .collect();
                    self.usb.cycle_selected(&ids).into_iter().collect()
                }
                KeyCode::Char('d') => self.usb.detach_selected().into_iter().collect(),
                _ => Vec::new(),
            },
        }
    }

    /// Append a status line, keeping the history bounded.
    fn push_status(&mut self, line: String) {
        if self.status.len() == MAX_STATUS {
            self.status.pop_front();
        }
        self.status.push_back(line);
    }

    /// Draw the whole console: title bar, tab bar, active tab, footer.
    pub fn render(&self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(2),
            ])
            .split(frame.area());

        let conn_style = if self.connected {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        };
        let conn_label = if self.connected {
            "connected"
        } else {
            "DISCONNECTED"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " enlil ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("management console · "),
                Span::raw(self.addr.as_str()),
                Span::raw(" · "),
                Span::styled(conn_label, conn_style),
            ])),
            chunks[0],
        );

        frame.render_widget(
            Tabs::new(vec![" Guests ", " USB "])
                .block(Block::default().borders(Borders::ALL))
                .select(self.tab.index())
                .highlight_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            chunks[1],
        );

        match self.tab {
            ActiveTab::Guests => self.render_guests(frame, chunks[2]),
            ActiveTab::Usb => self.render_usb(frame, chunks[2]),
        }

        let hints = match self.tab {
            ActiveTab::Guests => {
                "1/2 tabs · j/k move · s start · x stop · r reboot · R refresh · q quit"
            }
            ActiveTab::Usb => "1/2 tabs · j/k move · c cycle guest · d detach · R refresh · q quit",
        };
        let status = self.latest_status().unwrap_or("waiting for data");
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(hints, Style::default().fg(Color::DarkGray))),
                Line::from(vec![
                    Span::styled("status: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(status),
                ]),
            ]),
            chunks[3],
        );
    }

    /// Draw the guest tab: per-guest id/name/state/CPU/memory/uptime table.
    fn render_guests(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(["Id", "Name", "State", "CPU%", "Mem (MiB)", "Uptime"])
            .style(Style::default().add_modifier(Modifier::BOLD));
        let rows = self.guests.state().guests().iter().map(|guest| {
            let state_style = match guest.state {
                crate::protocol::GuestState::Running => Style::default().fg(Color::Green),
                crate::protocol::GuestState::Stopped => Style::default().fg(Color::DarkGray),
                crate::protocol::GuestState::Paused => Style::default().fg(Color::Yellow),
            };
            Row::new([
                Cell::from(guest.id.clone()),
                Cell::from(guest.name.clone()),
                Cell::from(Span::styled(guest.state.to_string(), state_style)),
                Cell::from(format!("{:.1}", guest.cpu_percent)),
                Cell::from(format!(
                    "{}/{}",
                    guest.memory_used_mib, guest.memory_total_mib
                )),
                Cell::from(format_uptime(guest.uptime_secs)),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(22),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(14),
                Constraint::Min(8),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Guests "))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut table_state = TableState::default();
        table_state.select(Some(self.guests.state().selected()));
        frame.render_stateful_widget(table, area, &mut table_state);
    }

    /// Draw the USB tab: device inventory table plus the hot-plug notice ring.
    fn render_usb(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(6),
                Constraint::Length(MAX_NOTICES_SHOWN + 2),
            ])
            .split(area);

        let header = Row::new(["Bus", "VID:PID", "Product", "Speed", "Assigned", "Port"])
            .style(Style::default().add_modifier(Modifier::BOLD));
        let rows = self.usb.state().devices().iter().map(|dev| {
            Row::new([
                Cell::from(format!("{:02}", dev.bus_addr)),
                Cell::from(format!("{:04x}:{:04x}", dev.vendor_id, dev.product_id)),
                Cell::from(
                    dev.product
                        .clone()
                        .unwrap_or_else(|| "Unknown Device".to_string()),
                ),
                Cell::from(dev.speed.clone()),
                Cell::from(
                    dev.assigned_guest
                        .clone()
                        .unwrap_or_else(|| "unassigned".to_string()),
                ),
                Cell::from(dev.port_path.clone().unwrap_or_else(|| "-".to_string())),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Length(10),
                Constraint::Length(26),
                Constraint::Length(8),
                Constraint::Length(14),
                Constraint::Min(6),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" USB devices "),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut table_state = TableState::default();
        table_state.select(Some(self.usb.state().selected()));
        frame.render_stateful_widget(table, chunks[0], &mut table_state);

        let notices: Vec<Line> = self
            .usb
            .state()
            .notices()
            .iter()
            .rev()
            .take(usize::from(MAX_NOTICES_SHOWN))
            .map(|notice| Line::from(notice.as_str()))
            .collect();
        frame.render_widget(
            Paragraph::new(notices).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Hot-plug notices "),
            ),
            chunks[1],
        );
    }
}

/// Format seconds as `H:MM:SS` for the guest table's uptime column.
fn format_uptime(secs: u64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// RAII guard: entering the alternate screen + raw mode on construction,
/// restoring the terminal on drop (including on panic unwind).
struct TerminalGuard;

impl TerminalGuard {
    /// Put the terminal into raw mode and switch to the alternate screen.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if raw mode or the alternate
    /// screen cannot be enabled.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Blocking key reader: polls crossterm and forwards key events to the loop.
/// Exits when the receiver is dropped (the loop has ended).
fn key_reader(tx: &mpsc::Sender<KeyEvent>) {
    while !tx.is_closed() {
        if !matches!(crossterm::event::poll(KEY_POLL_INTERVAL), Ok(true)) {
            continue;
        }
        if let Ok(Event::Key(key)) = crossterm::event::read() {
            if tx.blocking_send(key).is_err() {
                break;
            }
        }
    }
}

/// Run the interactive console against the `enlil-core` management socket at
/// `addr` (`host:port`).
///
/// The loop: draw [`App`], then `select!` over keystrokes (turned into
/// [`ClientMessage`]s by [`App::on_key`] and sent over the write half),
/// inbound [`ServerMessage`]s from the socket-reader task driving the read
/// half (folded into the tabs by [`App::on_server_message`]), and a periodic
/// tick that re-requests the active tab's data. The stream is split up front
/// so the reader task and the loop never share a lock. Quitting
/// (`q`/Esc/Ctrl+C) or a socket disconnect ends the loop; the
/// [`TerminalGuard`] restores the terminal on the way out.
///
/// # Errors
///
/// Returns an error if the TCP connection cannot be established, the terminal
/// cannot enter raw mode / the alternate screen, or a frame fails to draw.
pub async fn run_tui(addr: &str) -> anyhow::Result<()> {
    // Split the stream up front: the reader task owns the read half while the
    // loop owns the write half, so keystroke commands and server messages flow
    // concurrently with no lock shared between them.
    let (mut reader, mut writer) = Connection::connect_tcp(addr).await?.into_split();
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = App::new(addr);

    // Seed both tabs so the first paint is not empty.
    writer.send(&ClientMessage::RequestStatus).await?;
    writer.send(&ClientMessage::RequestUsbDevices).await?;

    let (key_tx, mut key_rx) = mpsc::channel::<KeyEvent>(KEY_QUEUE);
    tokio::task::spawn_blocking(move || key_reader(&key_tx));

    let (net_tx, mut net_rx) = mpsc::channel::<NetEvent>(NET_QUEUE);
    tokio::spawn(async move {
        loop {
            if let Ok(msg) = reader.recv().await {
                if net_tx.send(NetEvent::Message(msg)).await.is_err() {
                    break;
                }
            } else {
                let _ = net_tx.send(NetEvent::Disconnected).await;
                break;
            }
        }
    });

    let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
    refresh.tick().await; // consume the immediate first tick

    'run: loop {
        terminal.draw(|frame| app.render(frame))?;
        tokio::select! {
            Some(key) = key_rx.recv() => {
                let outbound = app.on_key(key);
                for msg in outbound {
                    if writer.send(&msg).await.is_err() {
                        app.note_disconnect();
                        break 'run;
                    }
                }
                if app.should_quit() {
                    break 'run;
                }
            }
            Some(event) = net_rx.recv() => {
                match event {
                    NetEvent::Message(msg) => app.on_server_message(&msg),
                    NetEvent::Disconnected => {
                        app.note_disconnect();
                        break 'run;
                    }
                }
            }
            _ = refresh.tick() => {
                if writer.send(&app.refresh_request()).await.is_err() {
                    app.note_disconnect();
                    break 'run;
                }
            }
        }
    }

    if !app.is_connected() {
        eprintln!("enlil-mgmt: disconnected from enlil-core at {addr}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        encode, GuestAction, GuestState, GuestStatus, UsbAction, UsbDeviceEntry,
    };
    use ratatui::backend::TestBackend;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn guest(id: &str, state: GuestState) -> GuestStatus {
        GuestStatus {
            id: id.to_string(),
            name: format!("{id} name"),
            state,
            cpu_percent: 12.5,
            memory_used_mib: 512,
            memory_total_mib: 1024,
            uptime_secs: 3661,
        }
    }

    fn device(bus_addr: u8, assigned: Option<&str>) -> UsbDeviceEntry {
        UsbDeviceEntry {
            bus_addr,
            vendor_id: 0x046d,
            product_id: 0xc077,
            product: Some("Test Mouse".into()),
            manufacturer: None,
            serial: None,
            port_path: Some("1-1".into()),
            speed: "Low".into(),
            assigned_guest: assigned.map(String::from),
            guest_port: assigned.map(|_| 0),
        }
    }

    /// Fold two guests into the app (also seeds the USB tab's guest list).
    fn app_with_guests() -> App {
        let mut app = App::new("127.0.0.1:5150");
        app.on_server_message(&ServerMessage::StatusUpdate(vec![
            guest("vm1", GuestState::Running),
            guest("vm2", GuestState::Stopped),
        ]));
        app
    }

    #[test]
    fn quit_keys_end_the_app_without_sending() {
        for key in [
            press(KeyCode::Char('q')),
            press(KeyCode::Char('Q')),
            press(KeyCode::Esc),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut app = App::new("addr");
            let outbound = app.on_key(key);
            assert!(app.should_quit(), "key {key:?} should quit");
            assert!(outbound.is_empty(), "quit sends nothing");
        }
    }

    #[test]
    fn key_release_events_are_ignored() {
        let mut app = App::new("addr");
        let mut key = press(KeyCode::Char('q'));
        key.kind = KeyEventKind::Release;
        assert!(app.on_key(key).is_empty());
        assert!(!app.should_quit(), "release must not quit");
    }

    #[test]
    fn tab_switch_keys_request_tab_data() {
        let mut app = App::new("addr");
        let outbound = app.on_key(press(KeyCode::Char('2')));
        assert!(matches!(
            outbound.as_slice(),
            [ClientMessage::RequestUsbDevices]
        ));
        let outbound = app.on_key(press(KeyCode::Char('1')));
        assert!(matches!(
            outbound.as_slice(),
            [ClientMessage::RequestStatus]
        ));
        let outbound = app.on_key(press(KeyCode::Tab));
        assert!(matches!(
            outbound.as_slice(),
            [ClientMessage::RequestUsbDevices]
        ));
    }

    #[test]
    fn navigation_moves_the_active_tab_cursor() {
        let mut app = app_with_guests();
        assert_eq!(app.guest_tab().state().selected(), 0);
        let _ = app.on_key(press(KeyCode::Char('j')));
        assert_eq!(app.guest_tab().state().selected(), 1);
        let _ = app.on_key(press(KeyCode::Down));
        assert_eq!(app.guest_tab().state().selected(), 0, "wraps around");
        let _ = app.on_key(press(KeyCode::Char('k')));
        assert_eq!(app.guest_tab().state().selected(), 1, "wraps the other way");
    }

    #[test]
    fn guest_lifecycle_keys_build_commands() {
        let mut app = app_with_guests(); // vm1 Running selected
                                         // Starting a running guest is a no-op.
        assert!(app.on_key(press(KeyCode::Char('s'))).is_empty());
        // Stop it: GuestCommand Stop, id 0.
        match app.on_key(press(KeyCode::Char('x'))).as_slice() {
            [ClientMessage::GuestCommand {
                id,
                guest_id,
                action,
            }] => {
                assert_eq!(*id, 0);
                assert_eq!(guest_id, "vm1");
                assert_eq!(*action, GuestAction::Stop);
            }
            other => panic!("expected a stop command, got {other:?}"),
        }
        // Move to vm2 (Stopped) and start it: id 1.
        let _ = app.on_key(press(KeyCode::Char('j')));
        match app.on_key(press(KeyCode::Char('s'))).as_slice() {
            [ClientMessage::GuestCommand {
                id,
                guest_id,
                action,
            }] => {
                assert_eq!(*id, 1, "command ids keep rising");
                assert_eq!(guest_id, "vm2");
                assert_eq!(*action, GuestAction::Start);
            }
            other => panic!("expected a start command, got {other:?}"),
        }
        // Rebooting a stopped guest is a no-op.
        assert!(app.on_key(press(KeyCode::Char('r'))).is_empty());
    }

    #[test]
    fn usb_cycle_key_reassigns_through_the_guest_list() {
        let mut app = app_with_guests();
        app.on_server_message(&ServerMessage::UsbDeviceList(vec![device(3, Some("vm1"))]));
        let _ = app.on_key(press(KeyCode::Char('2'))); // USB tab
        match app.on_key(press(KeyCode::Char('c'))).as_slice() {
            [ClientMessage::UsbCommand { id, action }] => {
                assert_eq!(*id, 0);
                assert_eq!(
                    *action,
                    UsbAction::Reassign {
                        bus_addr: 3,
                        target_guest: "vm2".into()
                    }
                );
            }
            other => panic!("expected a reassign command, got {other:?}"),
        }
        // Detach the device back to the hypervisor.
        match app.on_key(press(KeyCode::Char('d'))).as_slice() {
            [ClientMessage::UsbCommand { id, action }] => {
                assert_eq!(*id, 1);
                assert_eq!(*action, UsbAction::Detach { bus_addr: 3 });
            }
            other => panic!("expected a detach command, got {other:?}"),
        }
    }

    #[test]
    fn guest_tab_keys_are_ignored_on_the_usb_tab() {
        let mut app = app_with_guests();
        let _ = app.on_key(press(KeyCode::Char('2'))); // USB tab, empty device list
        assert!(app.on_key(press(KeyCode::Char('s'))).is_empty());
        assert!(app.on_key(press(KeyCode::Char('x'))).is_empty());
        assert!(app.on_key(press(KeyCode::Char('c'))).is_empty());
    }

    #[test]
    fn command_responses_and_hotplug_land_on_the_status_line() {
        let mut app = App::new("addr");
        assert!(app.latest_status().is_none());
        app.on_server_message(&ServerMessage::CommandResponse {
            id: 3,
            success: false,
            message: "no such guest".into(),
        });
        assert_eq!(app.latest_status(), Some("command FAILED: no such guest"));
        app.on_server_message(&ServerMessage::UsbHotplugNotice {
            message: "mouse attached".into(),
            device: None,
        });
        assert_eq!(app.latest_status(), Some("usb: mouse attached"));
        // The USB tab's own notice ring got it too.
        assert_eq!(
            app.usb_tab().state().latest_notice(),
            Some("mouse attached")
        );
    }

    #[test]
    fn disconnect_marks_the_app_and_reports() {
        let mut app = App::new("addr");
        assert!(app.is_connected());
        app.note_disconnect();
        assert!(!app.is_connected());
        assert_eq!(app.latest_status(), Some("disconnected from enlil-core"));
    }

    #[test]
    fn format_uptime_renders_h_mm_ss() {
        assert_eq!(format_uptime(0), "0:00:00");
        assert_eq!(format_uptime(61), "0:01:01");
        assert_eq!(format_uptime(3661), "1:01:01");
    }

    /// Render the full console into a headless backend and check the content.
    #[test]
    fn render_shows_guests_usb_devices_and_hints() {
        let mut app = app_with_guests();
        app.on_server_message(&ServerMessage::UsbDeviceList(vec![device(3, Some("vm1"))]));
        app.on_server_message(&ServerMessage::UsbHotplugNotice {
            message: "Test Mouse attached".into(),
            device: None,
        });

        // Guest tab.
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("vm1"), "guest id rendered");
        assert!(text.contains("1:01:01"), "uptime rendered");
        assert!(text.contains("s start"), "guest key hints rendered");

        // USB tab.
        let _ = app.on_key(press(KeyCode::Char('2')));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("Test Mouse"), "device rendered");
        assert!(text.contains("046d:c077"), "VID:PID rendered");
        assert!(text.contains("Test Mouse attached"), "notice rendered");
        assert!(text.contains("c cycle guest"), "USB key hints rendered");
    }

    /// Headless socket-glue check: the app's `Connection` talks to a fake
    /// `enlil-core` over loopback TCP — send a request, fold the reply. Uses
    /// the split halves exactly like [`run_tui`] does.
    #[tokio::test]
    async fn socket_glue_round_trip_over_loopback_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read one length-prefixed frame from the console.
            let mut len_buf = [0u8; 4];
            sock.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            sock.read_exact(&mut payload).await.unwrap();
            let msg: ClientMessage = serde_json::from_slice(&payload).unwrap();
            assert!(matches!(msg, ClientMessage::RequestStatus));
            // Answer with a status snapshot.
            let reply = encode(&ServerMessage::StatusUpdate(vec![guest(
                "loop-vm",
                GuestState::Running,
            )]))
            .unwrap();
            sock.write_all(&reply).await.unwrap();
        });

        let conn = Connection::connect_tcp(&addr).await.unwrap();
        let (mut reader, mut writer) = conn.into_split();
        writer.send(&ClientMessage::RequestStatus).await.unwrap();
        let msg = reader.recv().await.unwrap();

        let mut app = App::new(&addr);
        app.on_server_message(&msg);
        let guests = app.guest_tab().state().guests();
        assert_eq!(guests.len(), 1);
        assert_eq!(guests[0].id, "loop-vm");
    }
}
