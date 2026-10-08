//! Interactive ratatui host for the management console (Phase 4.6).
//!
//! This is the thin render + event glue over the headless view-models in
//! [`crate::usb_tab`] and [`crate::guest_tab`]: it draws the USB tab — the
//! physical device inventory with each device's current guest assignment, plus
//! the bounded hot-plug notice ring — turns operator keystrokes into the
//! [`ClientMessage`](crate::protocol::ClientMessage)s those view-models build,
//! and pumps the messages over the management socket to `enlil-core`.
//!
//! Keystroke dispatch lives in the pure [`App::handle_key`], so the whole
//! interaction model is unit-testable without a TTY; rendering is exercised
//! against ratatui's `TestBackend`.

use crate::guest_tab::GuestTabController;
use crate::protocol::{ClientMessage, Connection, GuestId, ServerMessage};
use crate::usb_tab::UsbTabController;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs};
use ratatui::{Frame, Terminal};
use std::io;

/// The console's top-level tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    Guests,
    #[default]
    Usb,
}

impl Tab {
    /// Index into the tab-bar widget.
    #[must_use]
    pub const fn as_index(self) -> usize {
        match self {
            Self::Guests => 0,
            Self::Usb => 1,
        }
    }
}

/// What one keystroke asks the event loop to do.
#[derive(Debug)]
pub enum KeyAction {
    /// Leave the console.
    Quit,
    /// Send this message to `enlil-core` over the management socket.
    Send(ClientMessage),
    /// Nothing (navigation, tab switch, or a no-op that set the status line).
    None,
}

/// The console application: tab controllers plus transient UI state.
#[derive(Debug, Default)]
pub struct App {
    guests: GuestTabController,
    usb: UsbTabController,
    active: Tab,
    status: Option<String>,
}

impl App {
    /// A fresh console with empty tabs.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently shown tab.
    #[must_use]
    pub const fn active(&self) -> Tab {
        self.active
    }

    /// The USB tab controller, for the renderer.
    #[must_use]
    pub const fn usb(&self) -> &UsbTabController {
        &self.usb
    }

    /// The USB tab controller, for tests that drive keystrokes.
    pub const fn usb_mut(&mut self) -> &mut UsbTabController {
        &mut self.usb
    }

    /// The guest tab controller, for tests that drive keystrokes.
    pub const fn guests_mut(&mut self) -> &mut GuestTabController {
        &mut self.guests
    }

    /// The current status line, if any.
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Replace the status line shown at the bottom of the console.
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
    }

    /// Fold a server message into the tabs, and surface command responses on
    /// the status line.
    pub fn handle_server_message(&mut self, msg: &ServerMessage) {
        self.usb.handle_server_message(msg);
        self.guests.handle_server_message(msg);
        if let ServerMessage::CommandResponse {
            success, message, ..
        } = msg
        {
            self.status = Some(if *success {
                format!("ok: {message}")
            } else {
                format!("FAILED: {message}")
            });
        }
    }

    /// Turn a keystroke into an action. Pure: no TTY, no socket — the event
    /// loop interprets the returned [`KeyAction`].
    #[must_use]
    pub fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        match key.code {
            KeyCode::Char('q' | 'Q') => KeyAction::Quit,
            KeyCode::Tab | KeyCode::BackTab => {
                self.active = match self.active {
                    Tab::Guests => Tab::Usb,
                    Tab::Usb => Tab::Guests,
                };
                KeyAction::None
            }
            KeyCode::F(1) => {
                self.active = Tab::Guests;
                KeyAction::None
            }
            KeyCode::F(2) => {
                self.active = Tab::Usb;
                KeyAction::None
            }
            _ => match self.active {
                Tab::Guests => self.guest_key(key),
                Tab::Usb => self.usb_key(key),
            },
        }
    }

    /// Keystrokes for the Guests tab.
    fn guest_key(&mut self, key: KeyEvent) -> KeyAction {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.guests.state_mut().select_prev();
                KeyAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.guests.state_mut().select_next();
                KeyAction::None
            }
            KeyCode::Char('s') => {
                let msg = self.guests.start_selected();
                self.send_or_note(msg, "start: no guest selected or already running")
            }
            KeyCode::Char('x') => {
                let msg = self.guests.stop_selected();
                self.send_or_note(msg, "stop: no guest selected or already stopped")
            }
            KeyCode::Char('r') => {
                let msg = self.guests.reboot_selected();
                self.send_or_note(msg, "reboot: no guest selected or not running")
            }
            KeyCode::Char('R') => KeyAction::Send(GuestTabController::request_status()),
            _ => KeyAction::None,
        }
    }

    /// Keystrokes for the USB tab: navigation, reassignment, detach, cycle.
    fn usb_key(&mut self, key: KeyEvent) -> KeyAction {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.usb.state_mut().select_prev();
                KeyAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.usb.state_mut().select_next();
                KeyAction::None
            }
            KeyCode::Char('c') => {
                let guests = self.guest_ids();
                let msg = self.usb.cycle_selected(&guests);
                self.send_or_note(
                    msg,
                    "cycle: no device selected or no other guest to move to",
                )
            }
            KeyCode::Char('x' | 'd') => {
                let msg = self.usb.detach_selected();
                self.send_or_note(
                    msg,
                    "detach: no device selected or already with the hypervisor",
                )
            }
            KeyCode::Char(digit @ '1'..='9') => {
                let target = ('1'..='9')
                    .zip(self.guest_ids())
                    .find(|(d, _)| *d == digit)
                    .map(|(_, id)| id);
                if let Some(id) = target {
                    let msg = self.usb.reassign_selected(&id);
                    self.send_or_note(msg, "reassign: no device selected or already on that guest")
                } else {
                    self.set_status(format!("reassign: no guest #{digit}"));
                    KeyAction::None
                }
            }
            KeyCode::Char('R') => KeyAction::Send(UsbTabController::request_devices()),
            _ => KeyAction::None,
        }
    }

    /// The guest ids the USB reassignment keystrokes rotate through, in the
    /// order the Guests tab lists them.
    fn guest_ids(&self) -> Vec<GuestId> {
        self.guests
            .state()
            .guests()
            .iter()
            .map(|g| g.id.clone())
            .collect()
    }

    /// Send the built command, or note on the status line why there is none.
    fn send_or_note(&mut self, msg: Option<ClientMessage>, why: &str) -> KeyAction {
        msg.map_or_else(
            || {
                self.set_status(why);
                KeyAction::None
            },
            KeyAction::Send,
        )
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Draw the whole console: tab bar, active tab, status line.
pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(4),
        ])
        .split(frame.area());

    let tabs = Tabs::new(vec!["Guests (F1)", "USB (F2)"])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Enlil management console"),
        )
        .select(app.active.as_index())
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::raw(" | "));
    frame.render_widget(tabs, chunks[0]);

    match app.active {
        Tab::Guests => render_guests(frame, app, chunks[1]),
        Tab::Usb => render_usb(frame, app, chunks[1]),
    }

    render_status(frame, app, chunks[2]);
}

/// The Guests tab: one row per guest with its live status.
fn render_guests(frame: &mut Frame, app: &App, area: Rect) {
    let state = app.guests.state();
    let rows: Vec<Row> = state
        .guests()
        .iter()
        .map(|g| {
            Row::new(vec![
                Cell::from(g.id.clone()),
                Cell::from(g.name.clone()),
                Cell::from(g.state.to_string()),
                Cell::from(format!("{:.1}%", g.cpu_percent)),
                Cell::from(format!("{}/{} MiB", g.memory_used_mib, g.memory_total_mib)),
                Cell::from(format!("{}s", g.uptime_secs)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Min(12),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(16),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(["Guest", "Name", "State", "CPU", "Memory", "Uptime"])
            .style(Style::default().add_modifier(Modifier::BOLD))
            .bottom_margin(1),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Guests ({})", state.guests().len())),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol(">> ");
    let mut table_state = TableState::default();
    table_state.select(if state.guests().is_empty() {
        None
    } else {
        Some(state.selected())
    });
    frame.render_stateful_widget(table, area, &mut table_state);
}

/// The USB tab: the physical device inventory with current guest assignment,
/// plus the hot-plug notice ring underneath.
fn render_usb(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(7)])
        .split(area);
    render_usb_table(frame, app, chunks[0]);
    render_notices(frame, app, chunks[1]);
}

/// The device table: bus / VID:PID / product / speed / assigned guest / port.
fn render_usb_table(frame: &mut Frame, app: &App, area: Rect) {
    let state = app.usb.state();
    let rows: Vec<Row> = state
        .devices()
        .iter()
        .map(|d| {
            Row::new(vec![
                Cell::from(d.bus_addr.to_string()),
                Cell::from(format!("{:04x}:{:04x}", d.vendor_id, d.product_id)),
                Cell::from(
                    d.product
                        .clone()
                        .unwrap_or_else(|| "Unknown device".to_string()),
                ),
                Cell::from(d.speed.clone()),
                Cell::from(
                    d.assigned_guest
                        .clone()
                        .unwrap_or_else(|| "hypervisor".to_string()),
                ),
                Cell::from(d.port_path.clone().unwrap_or_else(|| "-".to_string())),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(11),
            Constraint::Min(16),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new([
            "Bus",
            "VID:PID",
            "Product",
            "Speed",
            "Assigned guest",
            "Port",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD))
        .bottom_margin(1),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("USB devices ({})", state.devices().len())),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol(">> ");
    let mut table_state = TableState::default();
    table_state.select(if state.devices().is_empty() {
        None
    } else {
        Some(state.selected())
    });
    frame.render_stateful_widget(table, area, &mut table_state);
}

/// The hot-plug notice ring: the most recent notices, oldest first.
fn render_notices(frame: &mut Frame, app: &App, area: Rect) {
    let notices: Vec<Line> = app
        .usb
        .state()
        .notices()
        .iter()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|n| Line::from(n.as_str()))
        .collect();
    let paragraph = Paragraph::new(notices).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Hot-plug notices"),
    );
    frame.render_widget(paragraph, area);
}

/// Bottom bar: per-tab keystroke help plus the status line.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let help = match app.active {
        Tab::Guests => "↑/↓ select · s start · x stop · r reboot · R refresh · Tab switch tab · q quit",
        Tab::Usb => {
            "↑/↓ select · c cycle guest · 1-9 assign to guest · x detach · R refresh · Tab switch tab · q quit"
        }
    };
    let status = app.status().unwrap_or("");
    let paragraph = Paragraph::new(vec![
        Line::from(Span::styled(
            help,
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::from(Span::styled(status, Style::default().fg(Color::Yellow))),
    ])
    .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Connect to the `enlil-core` management socket at `addr` and run the
/// interactive console until the operator quits or the connection drops.
///
/// # Errors
///
/// Returns an error when the socket cannot be reached, the terminal cannot be
/// put into raw mode, or the event loop itself fails.
pub async fn run_console(addr: &str) -> anyhow::Result<()> {
    let mut conn = Connection::connect_tcp(addr).await.map_err(|e| {
        anyhow::anyhow!("cannot reach the enlil-core management socket at {addr}: {e}")
    })?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, &mut conn).await;

    // Restore the terminal even if the loop errored.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

/// The async pump: keyboard events from a reader thread plus server messages
/// from the socket, redrawing after every turn.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    conn: &mut Connection,
) -> anyhow::Result<()> {
    let mut app = App::new();
    conn.send(&ClientMessage::RequestStatus).await?;
    conn.send(&UsbTabController::request_devices()).await?;

    // Blocking `crossterm::event::read` does not fit the async select loop, so
    // a reader thread forwards press events over a channel.
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<KeyEvent>();
    std::thread::spawn(move || loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) => {
                if key.kind == KeyEventKind::Press && key_tx.send(key).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    terminal.draw(|frame| render(frame, &app))?;
    loop {
        tokio::select! {
            key = key_rx.recv() => {
                let Some(key) = key else { break };
                match app.handle_key(key) {
                    KeyAction::Quit => break,
                    KeyAction::Send(msg) => {
                        if let Err(e) = conn.send(&msg).await {
                            app.set_status(format!("send failed: {e}"));
                        }
                    }
                    KeyAction::None => {}
                }
            }
            incoming = conn.recv() => match incoming {
                Ok(msg) => app.handle_server_message(&msg),
                Err(e) => {
                    app.set_status(format!("connection lost: {e}"));
                    break;
                }
            },
        }
        terminal.draw(|frame| render(frame, &app))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{GuestState, GuestStatus, UsbDeviceEntry};
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell as BufferCell;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn guest(id: &str, state: GuestState) -> GuestStatus {
        GuestStatus {
            id: id.to_string(),
            name: format!("{id} name"),
            state,
            cpu_percent: 1.5,
            memory_used_mib: 512,
            memory_total_mib: 1024,
            uptime_secs: 60,
        }
    }

    fn dev(bus_addr: u8, assigned: Option<&str>) -> UsbDeviceEntry {
        UsbDeviceEntry {
            bus_addr,
            vendor_id: 0x1234,
            product_id: 0x5678,
            product: Some("Test Mouse".to_string()),
            manufacturer: None,
            serial: None,
            port_path: Some("1-2".to_string()),
            speed: "High".to_string(),
            assigned_guest: assigned.map(str::to_string),
            guest_port: assigned.map(|_| 0),
        }
    }

    fn usb_app() -> App {
        let mut app = App::new();
        app.handle_server_message(&ServerMessage::StatusUpdate(vec![
            guest("vm1", GuestState::Running),
            guest("vm2", GuestState::Stopped),
        ]));
        app.handle_server_message(&ServerMessage::UsbDeviceList(vec![dev(3, None)]));
        app.active = Tab::Usb;
        app
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(BufferCell::symbol)
            .collect()
    }

    #[test]
    fn cycle_keystroke_sends_reassign_to_next_guest() {
        let mut app = usb_app();
        let action = app.handle_key(key(KeyCode::Char('c')));
        match action {
            KeyAction::Send(ClientMessage::UsbCommand { action, .. }) => {
                assert_eq!(
                    action,
                    crate::protocol::UsbAction::Reassign {
                        bus_addr: 3,
                        target_guest: "vm1".to_string(),
                    }
                );
            }
            other => panic!("expected UsbCommand, got {other:?}"),
        }
    }

    #[test]
    fn digit_keystroke_reassigns_to_numbered_guest() {
        let mut app = usb_app();
        let action = app.handle_key(key(KeyCode::Char('2')));
        match action {
            KeyAction::Send(ClientMessage::UsbCommand { action, .. }) => {
                assert_eq!(
                    action,
                    crate::protocol::UsbAction::Reassign {
                        bus_addr: 3,
                        target_guest: "vm2".to_string(),
                    }
                );
            }
            other => panic!("expected UsbCommand, got {other:?}"),
        }
    }

    #[test]
    fn detach_keystroke_sends_detach_for_assigned_device() {
        let mut app = usb_app();
        app.handle_server_message(&ServerMessage::UsbDeviceList(vec![dev(3, Some("vm1"))]));
        let action = app.handle_key(key(KeyCode::Char('x')));
        match action {
            KeyAction::Send(ClientMessage::UsbCommand { action, .. }) => {
                assert_eq!(action, crate::protocol::UsbAction::Detach { bus_addr: 3 });
            }
            other => panic!("expected UsbCommand, got {other:?}"),
        }
    }

    #[test]
    fn redundant_keystrokes_set_the_status_line() {
        let mut app = usb_app();
        // The device is unassigned, so detach is a no-op that explains itself.
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('x'))),
            KeyAction::None
        ));
        assert!(
            app.status().is_some_and(|s| s.contains("detach")),
            "status explains the no-op"
        );
    }

    #[test]
    fn tab_key_switches_between_tabs() {
        let mut app = usb_app();
        assert_eq!(app.active(), Tab::Usb);
        let _ = app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.active(), Tab::Guests);
        let _ = app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.active(), Tab::Usb);
    }

    #[test]
    fn quit_keystroke_requests_exit() {
        let mut app = usb_app();
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('q'))),
            KeyAction::Quit
        ));
    }

    #[test]
    fn usb_table_renders_devices_with_assignment() {
        let mut app = usb_app();
        app.handle_server_message(&ServerMessage::UsbDeviceList(vec![
            dev(3, Some("vm1")),
            dev(5, None),
        ]));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(text.contains("1234:5678"), "VID:PID column renders");
        assert!(text.contains("Test Mouse"), "product column renders");
        assert!(text.contains("vm1"), "assigned guest renders");
        assert!(
            text.contains("hypervisor"),
            "unassigned device shows hypervisor"
        );
        assert!(text.contains("1-2"), "port path renders");
        assert!(
            text.contains("USB devices (2)"),
            "table title counts devices"
        );
    }

    #[test]
    fn hotplug_notices_render_in_the_notice_ring() {
        let mut app = usb_app();
        app.handle_server_message(&ServerMessage::UsbHotplugNotice {
            message: "USB device 7 attached at 2-1".to_string(),
            device: None,
        });
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(
            text.contains("USB device 7 attached at 2-1"),
            "notice renders"
        );
        assert!(text.contains("Hot-plug notices"), "notice block renders");
    }

    #[test]
    fn command_responses_surface_on_the_status_line() {
        let mut app = usb_app();
        app.handle_server_message(&ServerMessage::CommandResponse {
            id: 0,
            success: false,
            message: "no such device".to_string(),
        });
        assert_eq!(app.status(), Some("FAILED: no such device"));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("draw succeeds");
        assert!(buffer_text(&terminal).contains("FAILED: no such device"));
    }

    #[test]
    fn guests_tab_renders_guest_status_rows() {
        let mut app = usb_app();
        app.active = Tab::Guests;
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(text.contains("vm1"), "guest id renders");
        assert!(text.contains("Running"), "guest state renders");
        assert!(text.contains("Stopped"), "second guest state renders");
    }
}
