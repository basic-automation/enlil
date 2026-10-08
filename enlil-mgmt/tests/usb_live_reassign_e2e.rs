//! Phase 4.7 milestone: two mice + two keyboards routed to different guests,
//! live-reassigned between them through the exact path the operator uses.
//!
//! The daemon side is a real [`enlil_core::usb_service::UsbService`] over two
//! real virtual xHCI controllers; the console side is the real
//! [`enlil_mgmt::app::App`]. Every move goes keystroke → `on_key` → wire
//! encode/decode (the management socket's framing) → service → registry,
//! and the replies flow back into the TUI the way the event loop delivers
//! them. No TTY, no KVM, no physical hardware — but no mocks of the path
//! under test either.

use std::cell::RefCell;
use std::rc::Rc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use enlil_core::usb_service::UsbService;
use enlil_devices::usb::controller::VirtualXhciController;
use enlil_devices::usb::registry::XhciRegistry;
use enlil_devices::usb::routing::{DeviceMatcher, RoutingState, RoutingTable};
use enlil_devices::usb::types::{DeviceSpeed, UsbDeviceClass, UsbDeviceId};
use enlil_mgmt::app::App;
use enlil_mgmt_proto::{
    encode, ClientMessage, FrameDecoder, GuestState, GuestStatus, ServerMessage, UsbDeviceEntry,
};

// ---------------------------------------------------------------------------
// Fixture: two guests, four HID devices, routed two-and-two
// ---------------------------------------------------------------------------

/// (vendor_id, product_id, product name) for the milestone's four devices.
const MOUSE_A: (u16, u16, &str) = (0x046d, 0xc077, "Logitech Mouse A");
const KBD_A: (u16, u16, &str) = (0x046d, 0xc31c, "Logitech Keyboard A");
const MOUSE_B: (u16, u16, &str) = (0x046d, 0xc52b, "Logitech Mouse B");
const KBD_B: (u16, u16, &str) = (0x046d, 0xc534, "Logitech Keyboard B");

fn hid_device(vendor_id: u16, product_id: u16, product: &str) -> UsbDeviceId {
    UsbDeviceId {
        vendor_id,
        product_id,
        serial: None,
        port_path: Some(format!("1-{product_id:04x}")),
        class: UsbDeviceClass::Hid,
        speed: DeviceSpeed::Low,
        manufacturer: Some("Logitech".to_owned()),
        product: Some(product.to_owned()),
    }
}

fn guest_status(id: &str) -> GuestStatus {
    GuestStatus {
        id: id.to_owned(),
        name: format!("{id} workstation"),
        state: GuestState::Running,
        cpu_percent: 2.0,
        memory_used_mib: 1024,
        memory_total_mib: 4096,
        uptime_secs: 300,
    }
}

/// Daemon side: two guests with virtual xHCI controllers, the four devices
/// routed two-and-two by VID:PID, all attached (the hot-plug path).
fn daemon() -> UsbService {
    let mut table = RoutingTable::new();
    for (priority, (vid, pid, _), target) in [
        (10, MOUSE_A, "guest-a"),
        (20, KBD_A, "guest-a"),
        (30, MOUSE_B, "guest-b"),
        (40, KBD_B, "guest-b"),
    ] {
        table.add_rule(
            priority,
            DeviceMatcher::VidPid {
                vendor_id: vid,
                product_id: pid,
            },
            target.to_owned(),
        );
    }
    let mut registry = XhciRegistry::new(RoutingState::new(table));
    for guest in ["guest-a", "guest-b"] {
        registry.register_controller(guest, Rc::new(RefCell::new(VirtualXhciController::new(8))));
    }
    let mut service = UsbService::new(registry);
    for (bus_addr, (vid, pid, product)) in [MOUSE_A, KBD_A, MOUSE_B, KBD_B].into_iter().enumerate()
    {
        service
            .attach_device(bus_addr as u8 + 1, hid_device(vid, pid, product))
            .expect("milestone device attaches");
    }
    service
}

/// Console side: the TUI app with both guests known and the live inventory
/// loaded, sitting on the USB tab the way an operator would.
fn console(service: &UsbService) -> App {
    let mut app = App::new("127.0.0.1:5150");
    app.on_server_message(&ServerMessage::StatusUpdate(vec![
        guest_status("guest-a"),
        guest_status("guest-b"),
    ]));
    app.on_server_message(&ServerMessage::UsbDeviceList(service.device_list()));
    // Onto the USB tab (key `2`); the returned refresh request is irrelevant here.
    let _ = app.on_key(key(KeyCode::Char('2')));
    app
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

/// Press a key in the TUI; expect it to build exactly one console→daemon message.
fn press_send(app: &mut App, code: KeyCode) -> ClientMessage {
    let msgs = app.on_key(key(code));
    assert_eq!(msgs.len(), 1, "expected one ClientMessage, got {msgs:?}");
    msgs.into_iter().next().unwrap()
}

/// Press a key in the TUI; expect it to move only local state (no message).
fn press_none(app: &mut App, code: KeyCode) {
    let msgs = app.on_key(key(code));
    assert!(msgs.is_empty(), "expected no message, got {msgs:?}");
}

/// Run one console→daemon→console round trip over the wire framing, feeding
/// the replies back into the TUI the way the event loop does. Returns the
/// daemon's replies.
fn round_trip(app: &mut App, service: &mut UsbService, msg: &ClientMessage) -> Vec<ServerMessage> {
    // The management socket's framing: length-prefixed JSON.
    let bytes = encode(msg).expect("command encodes");
    let mut decoder = FrameDecoder::new();
    decoder.push(&bytes);
    let decoded: ClientMessage = decoder
        .decode()
        .expect("command decodes")
        .expect("complete frame");
    let replies = service.handle_client_message(&decoded);
    assert!(!replies.is_empty(), "daemon answered the console command");
    for reply in &replies {
        app.on_server_message(reply);
    }
    replies
}

fn command_response(replies: &[ServerMessage]) -> (u64, bool, &str) {
    match &replies[0] {
        ServerMessage::CommandResponse {
            id,
            success,
            message,
        } => (*id, *success, message.as_str()),
        other => panic!("expected CommandResponse first, got {other:?}"),
    }
}

/// Where the daemon currently places each bus address: `None` = hypervisor.
fn placements(service: &UsbService) -> Vec<(u8, Option<String>)> {
    service
        .device_list()
        .into_iter()
        .map(|entry: UsbDeviceEntry| (entry.bus_addr, entry.assigned_guest))
        .collect()
}

/// Where the TUI shows each bus address.
fn shown(app: &App) -> Vec<(u8, Option<String>)> {
    app.usb_tab()
        .state()
        .devices()
        .iter()
        .map(|d| (d.bus_addr, d.assigned_guest.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// The milestone
// ---------------------------------------------------------------------------

#[test]
fn two_mice_two_keyboards_live_reassign_between_guests() {
    let mut service = daemon();
    let mut app = console(&service);

    // Initial routing: mouse-A + kbd-A on guest-a, mouse-B + kbd-B on guest-b.
    let initial = vec![
        (1, Some("guest-a".to_owned())),
        (2, Some("guest-a".to_owned())),
        (3, Some("guest-b".to_owned())),
        (4, Some("guest-b".to_owned())),
    ];
    assert_eq!(placements(&service), initial);
    // The TUI shows the same assignments the daemon holds.
    assert_eq!(shown(&app), initial);

    // --- Operator moves Mouse A (bus 1, selected) to guest B with `c`
    // (cycle assignment). ---
    let msg = press_send(&mut app, KeyCode::Char('c'));
    let replies = round_trip(&mut app, &mut service, &msg);
    let (_, success, text) = command_response(&replies);
    assert!(success, "daemon rejected the move: {text}");
    assert_eq!(
        service.registry().placement(1).map(|p| p.guest.as_str()),
        Some("guest-b"),
        "mouse A now sits on guest B's controller"
    );
    // The fresh device list the daemon pushed updated the TUI's assignment
    // column, and the status line reports the move.
    assert_eq!(
        app.usb_tab().state().devices()[0].assigned_guest.as_deref(),
        Some("guest-b")
    );
    assert!(
        app.latest_status().unwrap_or("").starts_with("command ok:"),
        "status: {:?}",
        app.latest_status()
    );

    // --- Operator moves Keyboard A (bus 2) to guest B: down, then `c`. ---
    press_none(&mut app, KeyCode::Down);
    let msg = press_send(&mut app, KeyCode::Char('c'));
    let replies = round_trip(&mut app, &mut service, &msg);
    let (_, success, text) = command_response(&replies);
    assert!(success, "daemon rejected the move: {text}");

    // --- Operator moves Mouse A back to guest A: up, then `c`
    // (cycles guest-b -> guest-a). ---
    press_none(&mut app, KeyCode::Up);
    let msg = press_send(&mut app, KeyCode::Char('c'));
    let replies = round_trip(&mut app, &mut service, &msg);
    let (_, success, text) = command_response(&replies);
    assert!(success, "daemon rejected the move: {text}");
    assert_eq!(
        service.registry().placement(1).map(|p| p.guest.as_str()),
        Some("guest-a")
    );

    // --- Operator detaches Keyboard B (bus 4) back to the hypervisor: `d`. ---
    for _ in 0..3 {
        press_none(&mut app, KeyCode::Down);
    }
    assert_eq!(
        app.usb_tab().state().devices()[app.usb_tab().state().selected()].bus_addr,
        4
    );
    let msg = press_send(&mut app, KeyCode::Char('d'));
    let replies = round_trip(&mut app, &mut service, &msg);
    let (_, success, text) = command_response(&replies);
    assert!(success, "daemon rejected the detach: {text}");

    // Final state: guest-a has mouse A, guest-b has mouse B + keyboard A,
    // keyboard B sits with the hypervisor.
    let expected = vec![
        (1, Some("guest-a".to_owned())),
        (2, Some("guest-b".to_owned())),
        (3, Some("guest-b".to_owned())),
        (4, None),
    ];
    assert_eq!(placements(&service), expected);
    // And the TUI shows exactly that.
    assert_eq!(shown(&app), expected);
}

#[test]
fn reassign_unknown_device_reports_failure_to_console() {
    let mut service = daemon();
    let mut app = console(&service);

    // A command for a bus address the daemon never saw (stale console state).
    let msg = ClientMessage::UsbCommand {
        id: 99,
        action: enlil_mgmt_proto::UsbAction::Reassign {
            bus_addr: 42,
            target_guest: "guest-b".to_owned(),
        },
    };
    let replies = round_trip(&mut app, &mut service, &msg);
    let (id, success, _) = command_response(&replies);
    assert_eq!(id, 99);
    assert!(!success);
    // The console surfaces the failure on its status line; nothing moved.
    assert!(
        app.latest_status()
            .unwrap_or("")
            .starts_with("command FAILED:"),
        "status: {:?}",
        app.latest_status()
    );
    assert_eq!(
        placements(&service),
        vec![
            (1, Some("guest-a".to_owned())),
            (2, Some("guest-a".to_owned())),
            (3, Some("guest-b".to_owned())),
            (4, Some("guest-b".to_owned())),
        ]
    );
}
