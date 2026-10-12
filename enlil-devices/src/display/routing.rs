//! Physical display routing: monitors, EDID, hot-plug, KVM-switch (roadmap 8.3).
//!
//! The Zones compositor (Phase 3.6) tiles *virtual* zones; this module routes
//! *physical* outputs. [`PhysicalMonitor`] models one host connector with its
//! EDID, [`Edid::parse`] decodes a 128-byte base block and
//! [`Edid::synthesize`] builds a valid block the hypervisor can hand a guest,
//! and [`DisplayRouter`] keeps the monitor → guest mapping, folds hot-plug
//! events into it, and performs the KVM-switch hotkey rotation that swaps which
//! guest owns the keyboard, mouse, and its assigned monitors.
//!
//! Everything here is `no_std`-free host-side bookkeeping with no hardware
//! access, so it is fully unit-testable: a hot-plug is just a
//! [`MonitorEvent`], and the router answers with [`RoutingEvent`]s the caller
//! applies (re-tile the compositor, notify the guest, ...).

use super::{Monitor, ZoneLayoutEngine};
use crate::truncate::{u8_of, u16_of};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// A single display timing from an EDID block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl VideoMode {
    /// A new mode; all three fields are display geometry, so no validation is
    /// applied — a zero field simply means "unspecified".
    #[must_use]
    pub const fn new(width: u32, height: u32, refresh_hz: u32) -> Self {
        Self {
            width,
            height,
            refresh_hz,
        }
    }
}

impl std::fmt::Display for VideoMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}@{}", self.width, self.height, self.refresh_hz)
    }
}

/// Which physical connector a monitor hangs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnectorKind {
    DisplayPort,
    Hdmi,
    UsbC,
    Thunderbolt,
    Vga,
    EmbeddedDisplay,
    Unknown,
}

impl std::fmt::Display for ConnectorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::DisplayPort => "DP",
            Self::Hdmi => "HDMI",
            Self::UsbC => "USB-C",
            Self::Thunderbolt => "Thunderbolt",
            Self::Vga => "VGA",
            Self::EmbeddedDisplay => "eDP",
            Self::Unknown => "?",
        };
        write!(f, "{label}")
    }
}

/// The pieces of an EDID 1.x base block the router actually needs: identity,
/// the mode list, and the preferred (native) mode. Parsed from, or synthesized
/// into, a 128-byte block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edid {
    /// Three-letter manufacturer code, e.g. `"DEL"`.
    pub manufacturer: String,
    pub product_code: u16,
    pub serial: u32,
    pub monitor_name: Option<String>,
    /// All decoded timings (detailed + standard), preferred first when known.
    pub modes: Vec<VideoMode>,
    /// The panel's native timing (first detailed timing descriptor).
    pub preferred: Option<VideoMode>,
}

/// EDID base-block size in bytes.
const EDID_BLOCK_LEN: usize = 128;
/// The fixed 8-byte EDID header magic.
const EDID_MAGIC: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
/// Byte offset of the first detailed-timing descriptor.
const DETAILED_BASE: usize = 54;
/// Descriptor length and count in a base block.
const DESCRIPTOR_LEN: usize = 18;
const DESCRIPTOR_COUNT: usize = 4;
/// Display-descriptor tag for the monitor-name string.
const TAG_MONITOR_NAME: u8 = 0xFC;
/// Byte offset of the standard-timing list.
const STANDARD_BASE: usize = 38;
const STANDARD_COUNT: usize = 4;

impl Edid {
    /// Decode a 128-byte EDID 1.x base block. Returns `None` when `bytes` is
    /// short or the header magic is wrong; a bad checksum alone does not
    /// reject the block (real firmware ships broken checksums).
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < EDID_BLOCK_LEN || bytes[..8] != EDID_MAGIC {
            return None;
        }
        let manufacturer = decode_manufacturer(u16::from_be_bytes([bytes[8], bytes[9]]));
        let product_code = u16::from_le_bytes([bytes[10], bytes[11]]);
        let serial = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);

        let mut monitor_name = None;
        let mut preferred = None;
        let mut modes = Vec::new();
        for i in 0..DESCRIPTOR_COUNT {
            let off = DETAILED_BASE + i * DESCRIPTOR_LEN;
            let desc = &bytes[off..off + DESCRIPTOR_LEN];
            let pixel_clock = u16::from_le_bytes([desc[0], desc[1]]);
            if pixel_clock == 0 {
                // A display descriptor (first three bytes zero); only the
                // monitor-name string is interesting here.
                if desc[2] == 0 && desc[3] == TAG_MONITOR_NAME {
                    monitor_name = Some(decode_descriptor_text(&desc[5..18]));
                }
                continue;
            }
            if let Some(mode) = decode_detailed_timing(pixel_clock, desc) {
                if preferred.is_none() {
                    preferred = Some(mode);
                }
                if !modes.contains(&mode) {
                    modes.push(mode);
                }
            }
        }
        for i in 0..STANDARD_COUNT {
            let off = STANDARD_BASE + i * 2;
            if (bytes[off], bytes[off + 1]) == (0x01, 0x01) {
                continue; // unused slot
            }
            modes.push(decode_standard_timing(bytes[off], bytes[off + 1]));
        }
        // The preferred (native) timing leads the mode list.
        if let Some(pref) = preferred
            && let Some(pos) = modes.iter().position(|m| *m == pref)
        {
            modes.swap(0, pos);
        }
        Some(Self {
            manufacturer,
            product_code,
            serial,
            monitor_name,
            modes,
            preferred,
        })
    }

    /// Build a valid 128-byte EDID 1.x block: header magic, identity, the
    /// preferred timing as the first detailed descriptor, the monitor name,
    /// and a correct checksum. `modes[0]` is the native timing; extra modes
    /// are recorded as standard timings when they fit the 8 allowed slots'
    /// encoding (a mode wider than 2488 px or with a non-multiple-of-8 width
    /// is skipped). Round-trips through [`parse`](Self::parse) for the
    /// preferred mode and the name.
    #[must_use]
    pub fn synthesize(
        manufacturer: &str,
        product_code: u16,
        serial: u32,
        name: &str,
        modes: &[VideoMode],
    ) -> Vec<u8> {
        let mut block = vec![0u8; EDID_BLOCK_LEN];
        block[..8].copy_from_slice(&EDID_MAGIC);
        let id = encode_manufacturer(manufacturer);
        block[8..10].copy_from_slice(&id.to_be_bytes());
        block[10..12].copy_from_slice(&product_code.to_le_bytes());
        block[12..16].copy_from_slice(&serial.to_le_bytes());
        block[18] = 0x01; // EDID 1.4
        block[19] = 0x04;
        block[20] = 0xB0; // digital input
        block[24] = 0x0A; // RGB, preferred timing included

        if let Some(pref) = modes.first() {
            encode_detailed_timing(
                &mut block[DETAILED_BASE..DETAILED_BASE + DESCRIPTOR_LEN],
                pref,
            );
        }
        // Monitor-name descriptor.
        let name_off = DETAILED_BASE + DESCRIPTOR_LEN;
        block[name_off + 3] = TAG_MONITOR_NAME;
        let name_bytes = name.as_bytes();
        let take = name_bytes.len().min(13);
        block[name_off + 5..name_off + 5 + take].copy_from_slice(&name_bytes[..take]);
        if take < 13 {
            block[name_off + 5 + take] = 0x0A;
            for b in block
                .iter_mut()
                .take(name_off + 18)
                .skip(name_off + 5 + take + 1)
            {
                *b = 0x20;
            }
        }

        // Checksum: the whole block sums to 0 mod 256.
        let sum: u32 = block[..EDID_BLOCK_LEN - 1]
            .iter()
            .map(|b| u32::from(*b))
            .sum();
        block[EDID_BLOCK_LEN - 1] = u8_of(256u32.wrapping_sub(sum) & 0xFF);
        block
    }

    /// Whether the block's own checksum validates (sum of all 128 bytes ≡ 0).
    #[must_use]
    pub fn checksum_valid(bytes: &[u8]) -> bool {
        bytes.len() >= EDID_BLOCK_LEN && {
            let sum: u32 = bytes[..EDID_BLOCK_LEN].iter().map(|b| u32::from(*b)).sum();
            u8_of(sum) == 0
        }
    }
}

/// Decode the 15-bit manufacturer id into a 3-letter code (`'A'` = 1).
fn decode_manufacturer(id: u16) -> String {
    [(id >> 10) & 0x1F, (id >> 5) & 0x1F, id & 0x1F]
        .into_iter()
        .map(|c| char::from((u8_of(c) + b'A' - 1).clamp(b'A', b'Z')))
        .collect()
}

/// Encode a 3-letter code; short codes are padded with `'A'`, long ones cut.
fn encode_manufacturer(code: &str) -> u16 {
    let mut chars = code.bytes().map(|b| b.to_ascii_uppercase());
    let mut id: u16 = 0;
    for shift in [10, 5, 0] {
        let c = chars.next().unwrap_or(b'A');
        let five = u16::from(c.clamp(b'A', b'Z').saturating_sub(b'A') + 1);
        id |= five << shift;
    }
    id
}

/// Decode one 18-byte detailed timing into a [`VideoMode`], or `None` when the
/// totals are degenerate.
fn decode_detailed_timing(pixel_clock_10khz: u16, desc: &[u8]) -> Option<VideoMode> {
    let h_active = u32::from(desc[2]) | (u32::from(desc[4] & 0xF0) << 4);
    let h_blank = u32::from(desc[3]) | (u32::from(desc[4] & 0x0F) << 8);
    let v_active = u32::from(desc[5]) | (u32::from(desc[7] & 0xF0) << 4);
    let v_blank = u32::from(desc[6]) | (u32::from(desc[7] & 0x0F) << 8);
    let h_total = h_active + h_blank;
    let v_total = v_active + v_blank;
    if h_total == 0 || v_total == 0 {
        return None;
    }
    let refresh =
        (u64::from(pixel_clock_10khz) * 10_000) / (u64::from(h_total) * u64::from(v_total));
    Some(VideoMode::new(
        h_active,
        v_active,
        u32::try_from(refresh).unwrap_or(u32::MAX),
    ))
}

/// Encode a [`VideoMode`] as an 18-byte detailed timing. Blanking is a modest
/// CVT-style estimate; the pixel clock is rounded *up* so the parsed refresh
/// rate equals the requested one exactly (see the unit test).
fn encode_detailed_timing(desc: &mut [u8], mode: &VideoMode) {
    let h_blank = (mode.width / 10).max(8);
    let v_blank = (mode.height / 20).max(2);
    let h_total = mode.width + h_blank;
    let v_total = mode.height + v_blank;
    // Round up: `floor(ceil(X/10000)*10000 / total) == refresh` because
    // `total > 9999` always, so the overshoot stays below one extra hertz.
    let pixel_clock =
        (u64::from(h_total) * u64::from(v_total) * u64::from(mode.refresh_hz)).div_ceil(10_000);
    let pc = u16_of(pixel_clock);
    desc[0..2].copy_from_slice(&pc.to_le_bytes());
    desc[2] = u8_of(mode.width);
    desc[3] = u8_of(h_blank);
    desc[4] = u8_of((mode.width >> 4) & 0xF0) | u8_of((h_blank >> 8) & 0x0F);
    desc[5] = u8_of(mode.height);
    desc[6] = u8_of(v_blank);
    desc[7] = u8_of((mode.height >> 4) & 0xF0) | u8_of((v_blank >> 8) & 0x0F);
}

/// Decode a 2-byte standard timing (width multiple of 8, aspect-coded height).
fn decode_standard_timing(b0: u8, b1: u8) -> VideoMode {
    let width = (u32::from(b0) + 31) * 8;
    let height = match (b1 >> 6) & 0x03 {
        0b00 => width * 10 / 16, // 16:10
        0b01 => width * 3 / 4,   // 4:3
        0b10 => width * 4 / 5,   // 5:4
        _ => width * 9 / 16,     // 16:9
    };
    VideoMode::new(width, height, u32::from(b1 & 0x3F) + 60)
}

/// Decode a 13-byte display-descriptor text field (0x0A/NUL terminated).
fn decode_descriptor_text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|&b| b == 0x0A || b == 0x00)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end])
        .trim_end()
        .to_string()
}

/// One physical output on the host: connector, EDID, and its placement in the
/// virtual desktop (the `(x, y)` origin [`Monitor`] tiles against).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalMonitor {
    pub id: u32,
    pub connector: ConnectorKind,
    pub edid: Edid,
    /// Whether the cable is currently plugged in.
    pub plugged: bool,
    /// Top-left corner in virtual-desktop coordinates.
    pub x: u32,
    pub y: u32,
}

impl PhysicalMonitor {
    /// A plugged-in monitor at the desktop origin; reposition with
    /// [`set_position`](Self::set_position).
    #[must_use]
    pub const fn new(id: u32, connector: ConnectorKind, edid: Edid) -> Self {
        Self {
            id,
            connector,
            edid,
            plugged: true,
            x: 0,
            y: 0,
        }
    }

    /// Move the monitor's top-left corner in virtual-desktop coordinates.
    pub const fn set_position(&mut self, x: u32, y: u32) {
        self.x = x;
        self.y = y;
    }

    /// The panel's native timing, or `None` when the EDID had no detailed
    /// timing (fall back to a mode from [`Edid::modes`]).
    #[must_use]
    pub const fn preferred_mode(&self) -> Option<VideoMode> {
        self.edid.preferred
    }

    /// `(width, height)` the compositor should tile: native timing, else the
    /// first known mode, else 1920×1080.
    #[must_use]
    pub fn tile_size(&self) -> (u32, u32) {
        self.preferred_mode()
            .or_else(|| self.edid.modes.first().copied())
            .map_or((1920, 1080), |m| (m.width, m.height))
    }

    /// One-line label for the management console, e.g. `DP "Dell U2720Q"
    /// 3840x2160@60`.
    #[must_use]
    pub fn label(&self) -> String {
        let name = self
            .edid
            .monitor_name
            .as_deref()
            .unwrap_or(self.edid.manufacturer.as_str());
        let mode = self
            .preferred_mode()
            .map_or_else(|| "no signal".to_string(), |m| m.to_string());
        format!("{} \"{name}\" {mode}", self.connector)
    }
}

/// Something the host's display stack reported: a cable event or an EDID
/// change (KVM switches and docks re-send EDID on input change).
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    /// A (new or re-plugged) monitor appeared with its EDID.
    Plugged { monitor: PhysicalMonitor },
    /// The cable was pulled; the monitor keeps its last EDID but is dark.
    Unplugged { monitor_id: u32 },
    /// Same cable, new EDID (mode list or name changed).
    EdidChanged { monitor_id: u32, edid: Edid },
}

/// What the router did about an event or command: the caller applies these
/// (re-tile the compositor, tell the guest, log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingEvent {
    MonitorPlugged {
        monitor_id: u32,
    },
    MonitorUnplugged {
        monitor_id: u32,
        released_guest: Option<String>,
    },
    Assigned {
        monitor_id: u32,
        guest_id: String,
    },
    Released {
        monitor_id: u32,
        guest_id: String,
    },
    ModeChanged {
        monitor_id: u32,
        mode: VideoMode,
    },
    KvmSwitched {
        guest_id: String,
    },
}

/// Why [`DisplayRouter::assign`] refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingError {
    #[error("no monitor with id {0}")]
    UnknownMonitor(u32),
    #[error("monitor {0} is unplugged")]
    MonitorUnplugged(u32),
}

/// KVM-switch state: the ordered guest list the hotkey rotates through and
/// which guest currently owns the switched inputs.
#[derive(Debug, Clone, Default)]
pub struct KvmSwitch {
    order: Vec<String>,
    active: Option<usize>,
}

impl KvmSwitch {
    /// An empty switch with no active guest.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a guest (idempotent); does not change the active guest.
    pub fn register_guest(&mut self, guest_id: &str) {
        if !self.order.iter().any(|g| g == guest_id) {
            self.order.push(guest_id.to_string());
        }
    }

    /// Drop a guest from the rotation. When the removed guest was active, the
    /// guest sliding into its slot becomes active; removing the tail clears
    /// the active guest.
    pub fn unregister_guest(&mut self, guest_id: &str) {
        let Some(pos) = self.order.iter().position(|g| g == guest_id) else {
            return;
        };
        self.order.remove(pos);
        match self.active {
            Some(a) if a == pos => {
                self.active = (pos < self.order.len()).then_some(pos);
            }
            Some(a) if a > pos => self.active = Some(a - 1),
            _ => {}
        }
    }

    /// Advance to the next registered guest, wrapping; the first guest when
    /// none is active yet. `None` with no registered guests.
    #[must_use]
    pub fn switch_next(&mut self) -> Option<String> {
        if self.order.is_empty() {
            return None;
        }
        let next = self.active.map_or(0, |a| (a + 1) % self.order.len());
        self.active = Some(next);
        Some(self.order[next].clone())
    }

    /// Jump straight to `guest_id`; `false` when it is not registered (the
    /// active guest is unchanged).
    #[must_use]
    pub fn switch_to(&mut self, guest_id: &str) -> bool {
        let Some(pos) = self.order.iter().position(|g| g == guest_id) else {
            return false;
        };
        self.active = Some(pos);
        true
    }

    /// The guest currently owning the switched inputs, if any.
    #[must_use]
    pub fn active(&self) -> Option<&str> {
        self.active
            .and_then(|a| self.order.get(a))
            .map(String::as_str)
    }

    /// The rotation order.
    #[must_use]
    pub fn guests(&self) -> &[String] {
        &self.order
    }
}

/// Chord detector for the KVM-switch hotkey: remembers which scancodes are
/// down and fires *once* when the chord becomes complete, re-arming only
/// after a chord key is released (no auto-repeat while held).
#[derive(Debug, Clone, Default)]
pub struct KvmSwitchHotkey {
    pressed: HashSet<u16>,
    fired: bool,
}

impl KvmSwitchHotkey {
    /// A fresh detector with nothing pressed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a key press; `true` exactly once per chord completion.
    pub fn key_down(&mut self, scancode: u16, chord: &[u16]) -> bool {
        self.pressed.insert(scancode);
        if self.fired || !chord.iter().all(|c| self.pressed.contains(c)) {
            return false;
        }
        self.fired = true;
        true
    }

    /// Record a key release; releasing any chord key re-arms the detector.
    pub fn key_up(&mut self, scancode: u16, chord: &[u16]) {
        self.pressed.remove(&scancode);
        if chord.contains(&scancode) {
            self.fired = false;
        }
    }
}

/// Routes physical monitors to guests: the monitor → guest mapping, hot-plug
/// folding, and the KVM-switch hotkey rotation.
#[derive(Debug, Default)]
pub struct DisplayRouter {
    monitors: HashMap<u32, PhysicalMonitor>,
    /// `monitor_id -> guest_id`; a monitor shows at most one guest, a guest
    /// may own several monitors.
    assignments: HashMap<u32, String>,
    kvm: KvmSwitch,
    /// Externally produced events (e.g. from a platform hot-plug callback),
    /// drained by [`drain_queued`](Self::drain_queued). Bounded so a flapping
    /// cable cannot grow memory without limit.
    queued: VecDeque<RoutingEvent>,
}

/// How many externally produced routing events the queue keeps before
/// dropping the oldest.
const QUEUE_CAP: usize = 256;

impl DisplayRouter {
    /// An empty router: no monitors, no guests, nothing switched.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) a monitor; plugging state comes from the monitor.
    pub fn register_monitor(&mut self, monitor: PhysicalMonitor) {
        self.monitors.insert(monitor.id, monitor);
    }

    /// Fold a host display event into the routing state, returning what the
    /// caller must apply. Unplugging a monitor releases its guest (the guest
    /// keeps running headless); an EDID change emits [`RoutingEvent::ModeChanged`]
    /// only when the native timing actually changed.
    #[must_use]
    pub fn handle_event(&mut self, event: MonitorEvent) -> Vec<RoutingEvent> {
        let mut out = Vec::new();
        match event {
            MonitorEvent::Plugged { mut monitor } => {
                let id = monitor.id;
                monitor.plugged = true;
                self.monitors.insert(id, monitor);
                out.push(RoutingEvent::MonitorPlugged { monitor_id: id });
            }
            MonitorEvent::Unplugged { monitor_id } => {
                if let Some(mon) = self.monitors.get_mut(&monitor_id) {
                    mon.plugged = false;
                }
                let released_guest = self.assignments.remove(&monitor_id);
                out.push(RoutingEvent::MonitorUnplugged {
                    monitor_id,
                    released_guest,
                });
            }
            MonitorEvent::EdidChanged { monitor_id, edid } => {
                if let Some(mon) = self.monitors.get_mut(&monitor_id) {
                    let before = mon.edid.preferred;
                    mon.edid = edid;
                    mon.plugged = true;
                    if mon.edid.preferred != before
                        && let Some(mode) = mon.edid.preferred
                    {
                        out.push(RoutingEvent::ModeChanged { monitor_id, mode });
                    }
                }
            }
        }
        out
    }

    /// Give `guest_id` exclusive use of `monitor_id` (replacing any previous
    /// holder, which is released first). The guest joins the KVM-switch
    /// rotation.
    ///
    /// # Errors
    ///
    /// [`RoutingError::UnknownMonitor`] when the id was never registered,
    /// [`RoutingError::MonitorUnplugged`] when the cable is out.
    pub fn assign(
        &mut self,
        monitor_id: u32,
        guest_id: &str,
    ) -> Result<Vec<RoutingEvent>, RoutingError> {
        let monitor = self
            .monitors
            .get(&monitor_id)
            .ok_or(RoutingError::UnknownMonitor(monitor_id))?;
        if !monitor.plugged {
            return Err(RoutingError::MonitorUnplugged(monitor_id));
        }
        self.kvm.register_guest(guest_id);
        let mut out = Vec::new();
        match self.assignments.get(&monitor_id) {
            Some(current) if current == guest_id => return Ok(out), // no-op
            Some(current) => out.push(RoutingEvent::Released {
                monitor_id,
                guest_id: current.clone(),
            }),
            None => {}
        }
        self.assignments.insert(monitor_id, guest_id.to_string());
        out.push(RoutingEvent::Assigned {
            monitor_id,
            guest_id: guest_id.to_string(),
        });
        Ok(out)
    }

    /// Take `monitor_id` back from its guest (the hypervisor owns it again).
    /// Returns the [`RoutingEvent::Released`] when something was holding it.
    #[must_use]
    pub fn release(&mut self, monitor_id: u32) -> Vec<RoutingEvent> {
        self.assignments
            .remove(&monitor_id)
            .map_or_else(Vec::new, |guest_id| {
                vec![RoutingEvent::Released {
                    monitor_id,
                    guest_id,
                }]
            })
    }

    /// The guest holding `monitor_id`, if any.
    #[must_use]
    pub fn guest_for_monitor(&self, monitor_id: u32) -> Option<&str> {
        self.assignments.get(&monitor_id).map(String::as_str)
    }

    /// All plugged or unplugged monitors currently holding `guest_id`, in id
    /// order.
    #[must_use]
    pub fn monitors_for_guest(&self, guest_id: &str) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .assignments
            .iter()
            .filter(|(_, g)| g.as_str() == guest_id)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// A monitor by id (plugged or not).
    #[must_use]
    pub fn monitor(&self, monitor_id: u32) -> Option<&PhysicalMonitor> {
        self.monitors.get(&monitor_id)
    }

    /// All known monitors, in id order.
    #[must_use]
    pub fn monitors(&self) -> Vec<&PhysicalMonitor> {
        let mut mons: Vec<&PhysicalMonitor> = self.monitors.values().collect();
        mons.sort_by_key(|m| m.id);
        mons
    }

    /// Snapshot of the current monitor → guest mapping.
    #[must_use]
    pub fn assignments(&self) -> HashMap<u32, String> {
        self.assignments.clone()
    }

    /// The KVM-switch rotation (guest registration lives here so `assign`
    /// keeps it in sync).
    #[must_use]
    pub const fn kvm(&self) -> &KvmSwitch {
        &self.kvm
    }

    /// The KVM-switch rotation, mutably (to register guests with no monitor,
    /// e.g. headless ones that still want the switched keyboard).
    pub const fn kvm_mut(&mut self) -> &mut KvmSwitch {
        &mut self.kvm
    }

    /// Advance the KVM switch to the next guest; `None` when no guest is
    /// registered.
    #[must_use]
    pub fn kvm_switch_next(&mut self) -> Option<RoutingEvent> {
        self.kvm
            .switch_next()
            .map(|guest_id| RoutingEvent::KvmSwitched { guest_id })
    }

    /// Switch straight to `guest_id`; `None` when it is not registered.
    #[must_use]
    pub fn kvm_switch_to(&mut self, guest_id: &str) -> Option<RoutingEvent> {
        self.kvm
            .switch_to(guest_id)
            .then(|| RoutingEvent::KvmSwitched {
                guest_id: guest_id.to_string(),
            })
    }

    /// Feed one raw scancode through the KVM-switch chord detector: on press,
    /// a completed chord advances the switch and yields
    /// [`RoutingEvent::KvmSwitched`]; anything else yields `None`.
    pub fn handle_scancode(
        &mut self,
        scancode: u16,
        pressed: bool,
        detector: &mut KvmSwitchHotkey,
        chord: &[u16],
    ) -> Option<RoutingEvent> {
        if pressed {
            detector
                .key_down(scancode, chord)
                .then(|| self.kvm_switch_next())
                .flatten()
        } else {
            detector.key_up(scancode, chord);
            None
        }
    }

    /// Tile every *plugged* monitor with its own `rows` × `cols` zone grid —
    /// the Phase 3.6 [`ZoneLayoutEngine::tile_monitors`] driven by the live
    /// physical topology, so a hot-plug re-tiles the compositor onto exactly
    /// the connected displays.
    ///
    /// # Panics
    ///
    /// Panics if the layout engine's internal lock is poisoned.
    #[must_use]
    pub fn tile_plugged_monitors(
        &self,
        engine: &ZoneLayoutEngine,
        rows: u32,
        cols: u32,
    ) -> Vec<u32> {
        let plans: Vec<(Monitor, u32, u32)> = {
            let mut mons: Vec<&PhysicalMonitor> =
                self.monitors.values().filter(|m| m.plugged).collect();
            mons.sort_by_key(|m| m.id);
            mons.iter()
                .map(|m| {
                    let (w, h) = m.tile_size();
                    (Monitor::new(m.id, m.x, m.y, w, h), rows, cols)
                })
                .collect()
        };
        engine.tile_monitors(&plans)
    }

    /// Pending routing events queued by [`queue_event`](Self::queue_event);
    /// drains the queue.
    pub fn drain_queued(&mut self) -> Vec<RoutingEvent> {
        self.queued.drain(..).collect()
    }

    /// Stash an externally produced event (e.g. from a platform hot-plug
    /// callback) for the next [`drain_queued`](Self::drain_queued); drops the
    /// oldest once [`QUEUE_CAP`] is reached.
    pub fn queue_event(&mut self, event: RoutingEvent) {
        if self.queued.len() == QUEUE_CAP {
            self.queued.pop_front();
        }
        self.queued.push_back(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_edid(name: &str, mode: VideoMode) -> Edid {
        Edid::parse(&Edid::synthesize("ENC", 0x1234, 0xABCD, name, &[mode])).unwrap()
    }

    fn monitor(id: u32, name: &str) -> PhysicalMonitor {
        PhysicalMonitor::new(
            id,
            ConnectorKind::DisplayPort,
            sample_edid(name, VideoMode::new(1920, 1080, 60)),
        )
    }

    #[test]
    fn edid_synthesize_parse_roundtrip() {
        let bytes = Edid::synthesize(
            "DEL",
            0xA1B2,
            0x0102_0304,
            "Dell U2720Q",
            &[VideoMode::new(3840, 2160, 60)],
        );
        assert_eq!(bytes.len(), 128);
        assert!(Edid::checksum_valid(&bytes), "synthesized checksum holds");
        let edid = Edid::parse(&bytes).expect("parses");
        assert_eq!(edid.manufacturer, "DEL");
        assert_eq!(edid.product_code, 0xA1B2);
        assert_eq!(edid.serial, 0x0102_0304);
        assert_eq!(edid.monitor_name.as_deref(), Some("Dell U2720Q"));
        assert_eq!(
            edid.preferred,
            Some(VideoMode::new(3840, 2160, 60)),
            "preferred timing round-trips exactly, refresh included"
        );
    }

    #[test]
    fn edid_parse_rejects_garbage() {
        assert!(Edid::parse(&[]).is_none());
        assert!(Edid::parse(&[0u8; 64]).is_none(), "short block");
        assert!(Edid::parse(&[0xAA; 128]).is_none(), "bad magic");
    }

    #[test]
    fn edid_decode_detailed_timing_math() {
        // Hand-built descriptor: 25.00 MHz pixel clock, 1250x1000 total ->
        // exactly 20 Hz, exercising the active/blanking bit packing.
        let mut desc = [0u8; 18];
        desc[0..2].copy_from_slice(&2500u16.to_le_bytes());
        desc[2] = 0x20; // h_active low  = 0x20  (800)
        desc[3] = 0xC2; // h_blank low  = 0xC2  (450)
        desc[4] = 0x31; // hi nibbles: h_active 0x3, h_blank 0x1
        desc[5] = 0x58; // v_active low  = 0x58  (600)
        desc[6] = 0x90; // v_blank low   = 0x90  (400)
        desc[7] = 0x21; // hi nibbles: v_active 0x2, v_blank 0x1
        let mode = decode_detailed_timing(2500, &desc).unwrap();
        assert_eq!(mode, VideoMode::new(0x320, 0x258, 20));
    }

    #[test]
    fn standard_timings_decode() {
        // 0xC1: (193+31)*8 = 1792 wide; 0xC1: aspect 16:9, refresh 60+1.
        let mode = decode_standard_timing(0xC1, 0xC1);
        assert_eq!(mode.width, 1792);
        assert_eq!(mode.height, 1792 * 9 / 16);
        assert_eq!(mode.refresh_hz, 61);
    }

    #[test]
    fn assign_release_and_lookup() {
        let mut router = DisplayRouter::new();
        router.register_monitor(monitor(0, "Left"));
        router.register_monitor(monitor(1, "Right"));

        let events = router.assign(0, "vm1").unwrap();
        assert_eq!(
            events,
            [RoutingEvent::Assigned {
                monitor_id: 0,
                guest_id: "vm1".into()
            }]
        );
        // Re-assigning to the same guest is a silent no-op.
        assert!(router.assign(0, "vm1").unwrap().is_empty());
        // Stealing the monitor releases the old holder first.
        let events = router.assign(0, "vm2").unwrap();
        assert_eq!(
            events,
            [
                RoutingEvent::Released {
                    monitor_id: 0,
                    guest_id: "vm1".into()
                },
                RoutingEvent::Assigned {
                    monitor_id: 0,
                    guest_id: "vm2".into()
                },
            ]
        );
        assert_eq!(router.guest_for_monitor(0), Some("vm2"));
        assert_eq!(router.monitors_for_guest("vm2"), vec![0]);

        let released = router.release(0);
        assert_eq!(
            released,
            [RoutingEvent::Released {
                monitor_id: 0,
                guest_id: "vm2".into()
            }]
        );
        assert!(router.release(0).is_empty(), "double release is quiet");
        assert_eq!(router.guest_for_monitor(0), None);
    }

    #[test]
    fn assign_rejects_unknown_and_unplugged() {
        let mut router = DisplayRouter::new();
        assert_eq!(
            router.assign(9, "vm1").unwrap_err(),
            RoutingError::UnknownMonitor(9)
        );
        let mut mon = monitor(0, "Left");
        mon.plugged = false;
        router.register_monitor(mon);
        assert_eq!(
            router.assign(0, "vm1").unwrap_err(),
            RoutingError::MonitorUnplugged(0)
        );
    }

    #[test]
    fn unplug_releases_the_guest_and_replug_keeps_it_released() {
        let mut router = DisplayRouter::new();
        router.register_monitor(monitor(0, "Left"));
        router.assign(0, "vm1").unwrap();

        let events = router.handle_event(MonitorEvent::Unplugged { monitor_id: 0 });
        assert_eq!(
            events,
            [RoutingEvent::MonitorUnplugged {
                monitor_id: 0,
                released_guest: Some("vm1".into())
            }]
        );
        assert_eq!(router.guest_for_monitor(0), None);

        // Re-plugging does not silently re-attach the guest.
        let events = router.handle_event(MonitorEvent::Plugged {
            monitor: monitor(0, "Left"),
        });
        assert_eq!(events, [RoutingEvent::MonitorPlugged { monitor_id: 0 }]);
        assert_eq!(router.guest_for_monitor(0), None);
    }

    #[test]
    fn edid_change_reports_only_real_mode_changes() {
        let mut router = DisplayRouter::new();
        router.register_monitor(monitor(0, "Left"));

        // Same timing, new name: no event.
        let same = sample_edid("Left (renamed)", VideoMode::new(1920, 1080, 60));
        assert!(
            router
                .handle_event(MonitorEvent::EdidChanged {
                    monitor_id: 0,
                    edid: same,
                })
                .is_empty()
        );

        // New native timing: ModeChanged.
        let faster = sample_edid("Left", VideoMode::new(1920, 1080, 144));
        let events = router.handle_event(MonitorEvent::EdidChanged {
            monitor_id: 0,
            edid: faster,
        });
        assert_eq!(
            events,
            [RoutingEvent::ModeChanged {
                monitor_id: 0,
                mode: VideoMode::new(1920, 1080, 144)
            }]
        );
    }

    #[test]
    fn kvm_switch_rotates_registered_guests() {
        let mut kvm = KvmSwitch::new();
        assert_eq!(kvm.active(), None);
        assert!(kvm.switch_next().is_none(), "empty rotation");

        kvm.register_guest("vm1");
        kvm.register_guest("vm2");
        kvm.register_guest("vm1"); // idempotent
        assert_eq!(kvm.guests(), ["vm1", "vm2"]);

        assert_eq!(kvm.switch_next().as_deref(), Some("vm1"));
        assert_eq!(kvm.active(), Some("vm1"));
        assert_eq!(kvm.switch_next().as_deref(), Some("vm2"));
        assert_eq!(kvm.switch_next().as_deref(), Some("vm1"), "wraps");

        assert!(kvm.switch_to("vm2"));
        assert_eq!(kvm.active(), Some("vm2"));
        assert!(!kvm.switch_to("nope"), "unknown guest rejected");
        assert_eq!(kvm.active(), Some("vm2"), "failed switch keeps active");

        // Removing the active guest hands the slot to the next one in line.
        kvm.register_guest("vm3");
        kvm.unregister_guest("vm2");
        assert_eq!(kvm.guests(), ["vm1", "vm3"]);
        assert_eq!(kvm.active(), Some("vm3"), "slot handed to vm3");
        // Removing the tail (now active) clears the active guest.
        kvm.unregister_guest("vm3");
        assert_eq!(kvm.active(), None);
        kvm.unregister_guest("vm1");
        kvm.unregister_guest("ghost"); // no panic
        assert!(kvm.guests().is_empty());
    }

    #[test]
    fn kvm_hotkey_fires_once_per_chord() {
        let chord = [29u16, 56, 37]; // Ctrl+Alt+K
        let mut hotkey = KvmSwitchHotkey::new();
        assert!(!hotkey.key_down(29, &chord));
        assert!(!hotkey.key_down(56, &chord));
        assert!(hotkey.key_down(37, &chord), "chord complete: fires");
        assert!(
            !hotkey.key_down(37, &chord),
            "no repeat while held (auto-repeat)"
        );
        hotkey.key_up(37, &chord); // release K: re-arms
        assert!(hotkey.key_down(37, &chord), "fires again after release");
        hotkey.key_up(29, &chord);
        hotkey.key_up(56, &chord);
        hotkey.key_up(37, &chord);
        // Partial chord never fires.
        assert!(!hotkey.key_down(29, &chord));
        assert!(!hotkey.key_down(56, &chord));
    }

    #[test]
    fn scancode_chord_drives_the_switch() {
        let chord = [29u16, 56, 37];
        let mut router = DisplayRouter::new();
        router.register_monitor(monitor(0, "Left"));
        router.assign(0, "vm1").unwrap();
        router.kvm_mut().register_guest("vm2");
        let mut detector = KvmSwitchHotkey::new();

        for sc in [29u16, 56] {
            assert!(
                router
                    .handle_scancode(sc, true, &mut detector, &chord)
                    .is_none()
            );
        }
        assert_eq!(
            router.handle_scancode(37, true, &mut detector, &chord),
            Some(RoutingEvent::KvmSwitched {
                guest_id: "vm1".into()
            })
        );
        for sc in [29u16, 56, 37] {
            assert!(
                router
                    .handle_scancode(sc, false, &mut detector, &chord)
                    .is_none()
            );
        }
        // Second full chord press advances to vm2.
        for sc in [29u16, 56, 37] {
            let _ = router.handle_scancode(sc, true, &mut detector, &chord);
        }
        assert_eq!(router.kvm().active(), Some("vm2"));
    }

    #[test]
    fn tile_plugged_monitors_drives_the_zone_engine() {
        let mut router = DisplayRouter::new();
        let mut left = monitor(0, "Left");
        left.set_position(0, 0);
        let mut right = monitor(1, "Right");
        right.set_position(1920, 0);
        router.register_monitor(left);
        router.register_monitor(right);
        // A third monitor with its cable out is skipped.
        let mut dark = monitor(2, "Dark");
        dark.plugged = false;
        router.register_monitor(dark);

        let engine = ZoneLayoutEngine::new(3840, 1080);
        let ids = router.tile_plugged_monitors(&engine, 1, 1);
        assert_eq!(ids.len(), 2, "only the plugged monitors tile");
        let layout = engine.get_layout();
        assert_eq!(layout.zones.len(), 2);
        assert_eq!(layout.zones[0].x, 0);
        assert_eq!(layout.zones[1].x, 1920);
        assert_eq!(layout.zones[1].width, 1920);
    }

    #[test]
    fn external_event_queue_is_bounded_and_drains() {
        let mut router = DisplayRouter::new();
        router.queue_event(RoutingEvent::MonitorPlugged { monitor_id: 1 });
        router.queue_event(RoutingEvent::MonitorPlugged { monitor_id: 2 });
        let drained = router.drain_queued();
        assert_eq!(
            drained,
            [
                RoutingEvent::MonitorPlugged { monitor_id: 1 },
                RoutingEvent::MonitorPlugged { monitor_id: 2 },
            ]
        );
        assert!(router.drain_queued().is_empty(), "drain empties the queue");

        // Overfilling evicts the oldest.
        for i in 0..QUEUE_CAP + 10 {
            router.queue_event(RoutingEvent::KvmSwitched {
                guest_id: format!("vm{i}"),
            });
        }
        let drained = router.drain_queued();
        assert_eq!(drained.len(), QUEUE_CAP, "queue is bounded");
        assert_eq!(
            drained.first(),
            Some(&RoutingEvent::KvmSwitched {
                guest_id: "vm10".into()
            }),
            "the first 10 events were evicted"
        );
    }

    #[test]
    fn guest_with_two_monitors_and_labels() {
        let mut router = DisplayRouter::new();
        router.register_monitor(monitor(0, "Left"));
        router.register_monitor(monitor(1, "Right"));
        router.assign(0, "vm1").unwrap();
        router.assign(1, "vm1").unwrap();
        assert_eq!(router.monitors_for_guest("vm1"), vec![0, 1]);
        let label = router.monitor(0).unwrap().label();
        assert!(label.contains("Left"), "label carries the name: {label}");
        assert!(
            label.contains("1920x1080@60"),
            "label carries the mode: {label}"
        );
        // A monitor whose EDID has no timing still tiles at a sane default.
        let bare = PhysicalMonitor::new(
            7,
            ConnectorKind::Hdmi,
            Edid {
                manufacturer: "???".into(),
                product_code: 0,
                serial: 0,
                monitor_name: None,
                modes: vec![],
                preferred: None,
            },
        );
        assert_eq!(bare.tile_size(), (1920, 1080));
        assert!(bare.label().contains("no signal"));
    }
}
