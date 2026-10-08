//! Bare-metal xHCI port-scan host-source backend.
//!
//! The [`sysfs`](super::sysfs) module covers the Linux dev path by walking
//! `/sys/bus/usb/devices`; this module is the bare-metal counterpart that
//! drives the same [`UsbMonitor`](super::monitor::UsbMonitor) seam from the
//! host xHCI controller's own port registers. On bare metal there is no
//! kernel USB stack to ask — the port-scan is how the hypervisor learns what
//! is plugged in, one PORTSC read per root-hub port.
//!
//! Like the sysfs backend, the parsing is split from the I/O so it is
//! unit-testable without real hardware: [`parse_portsc`] decodes one PORTSC
//! dword into an [`XhciPortStatus`], [`scan_xhci_ports`] folds a whole
//! controller's worth into the `(port path, descriptor, speed)` triples the
//! monitor consumes, and [`sync_xhci_monitor`] reconciles a fresh scan
//! against the monitor's inventory, firing exactly the connect/disconnect
//! events the port set changed by. [`poll_xhci`] is the cadence-respecting
//! drop-in the run loop calls every iteration.
//!
//! The only hardware touch is [`XhciMmioPortReader`], the [`XhciPortReader`]
//! implementation that reads PORTSC straight from the controller's MMIO
//! window — constructed with the BAR0 base the PCI walk already discovered
//! (see [`pci_discovery::xhci_mmio_base`](crate::pci_discovery::xhci_mmio_base))
//! and the port count the driver's capability-register walk decoded
//! (HCSPARAMS1 bits 31:24). Tests drive everything through a canned reader,
//! so the whole module verifies on a machine with no xHCI at all.
//!
//! A port-scan cannot issue control transfers — that needs the full host xHCI
//! driver (Phase 6.5). Devices found this way therefore carry a
//! [`provisional_descriptor`]: zeroed VID/PID, no strings, USB 2.0-era
//! defaults. Port-path and default routing rules can act on those immediately
//! (the common bare-metal case: route a port to a guest), and the driver
//! replaces the descriptor with the real one when it enumerates the port.

use std::collections::HashSet;

use super::monitor::UsbMonitor;
use super::types::{DeviceSpeed, UsbDeviceClass, UsbDeviceDescriptor, UsbPortPath, UsbResult};

// ---------------------------------------------------------------------------
// xHCI MMIO layout constants (xHCI §5.4.8)
// ---------------------------------------------------------------------------

/// Byte offset from the operational-register base to the first port register
/// set (PORTSC of port 1).
pub const XHCI_PORT_REGS_OFFSET: u64 = 0x400;

/// Byte stride between consecutive port register sets. Each set is four
/// dwords: PORTSC, PORTPMSC, PORTLI, PORTHLPMC.
pub const XHCI_PORT_REGS_STRIDE: u64 = 0x10;

/// PORTSC bit 0 — Current Connect Status: a device is attached to the port.
pub const PORTSC_CCS: u32 = 1 << 0;

/// PORTSC bit 1 — Port Enabled/Disabled: set only once the port has completed
/// reset and the device is usable.
pub const PORTSC_PED: u32 = 1 << 1;

/// PORTSC bit 9 — Port Power: the port is powered.
pub const PORTSC_PP: u32 = 1 << 9;

/// PORTSC bits 13:10 — Port Speed ID field shift.
pub const PORTSC_SPEED_SHIFT: u32 = 10;

/// PORTSC bits 13:10 — Port Speed ID field mask.
pub const PORTSC_SPEED_MASK: u32 = 0xF << PORTSC_SPEED_SHIFT;

// ---------------------------------------------------------------------------
// Port register reader abstraction
// ---------------------------------------------------------------------------

/// How the port-scan reads PORTSC registers.
///
/// The trait keeps the scan testable (a canned reader stands in for hardware)
/// and leaves the MMIO policy to the caller: one reader per physical xHCI
/// controller, each with its own bus number so devices on different
/// controllers never collide in the monitor's inventory.
pub trait XhciPortReader {
    /// Number of root-hub ports on this controller.
    fn port_count(&self) -> u8;

    /// The USB bus number devices on this controller appear under in
    /// [`UsbPortPath`]. Defaults to 0; the run loop assigns distinct bus
    /// numbers when the machine has more than one xHCI controller.
    fn bus_number(&self) -> u8 {
        0
    }

    /// Read the raw PORTSC dword for 0-based `port`, or `None` when the port
    /// cannot be read (past the end of a partially-mapped window, for
    /// example). A `None` port is skipped by the scan, never an error.
    fn read_portsc(&self, port: u8) -> Option<u32>;
}

/// Reads PORTSC directly from a host xHCI controller's MMIO register window.
///
/// Constructed with the controller's operational-register base (BAR0, as
/// reported by [`pci_discovery::xhci_mmio_base`](crate::pci_discovery::xhci_mmio_base))
/// and the port count the driver's capability-register walk already decoded
/// (HCSPARAMS1 bits 31:24).
///
/// # Safety
///
/// The constructor is `unsafe` because it trusts the caller that `mmio_base`
/// is a mapped, live xHCI register window on this machine for the lifetime of
/// the reader. Building one with a stale or unmapped base and reading through
/// it is immediate undefined behavior — the Linux dev path must never do this.
#[derive(Debug, Clone, Copy)]
pub struct XhciMmioPortReader {
    mmio_base: u64,
    port_count: u8,
    bus_number: u8,
}

impl XhciMmioPortReader {
    /// Build a reader for the xHCI window at `mmio_base`.
    ///
    /// `port_count` is the controller's root-hub port count and `bus_number`
    /// the USB bus number its devices appear under (distinct per controller
    /// on multi-controller machines).
    ///
    /// # Safety
    ///
    /// `mmio_base` must be the operational-register base of a mapped, live
    /// xHCI controller for as long as this reader exists.
    #[must_use]
    pub const unsafe fn new(mmio_base: u64, port_count: u8, bus_number: u8) -> Self {
        Self {
            mmio_base,
            port_count,
            bus_number,
        }
    }

    /// The physical address of the PORTSC register for 0-based `port`
    /// (xHCI §5.4.8: operational base + 0x400 + index × 0x10).
    #[must_use]
    pub const fn portsc_address(&self, port: u8) -> u64 {
        self.mmio_base + XHCI_PORT_REGS_OFFSET + port as u64 * XHCI_PORT_REGS_STRIDE
    }
}

impl XhciPortReader for XhciMmioPortReader {
    fn port_count(&self) -> u8 {
        self.port_count
    }

    fn bus_number(&self) -> u8 {
        self.bus_number
    }

    fn read_portsc(&self, port: u8) -> Option<u32> {
        if port >= self.port_count {
            return None;
        }
        // SAFETY: the constructor's contract guarantees this window is a
        // mapped, live xHCI register block on this machine.
        unsafe {
            Some(core::ptr::read_volatile(
                self.portsc_address(port) as *const u32
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// PORTSC decoding
// ---------------------------------------------------------------------------

/// Decode an xHCI Port Speed ID (PORTSC bits 13:10) into a [`DeviceSpeed`].
///
/// Follows the default speed-ID assignment (xHCI §7.2.2.1.1), the inverse of
/// [`DeviceSpeed::xhci_speed_id`]: FS=1, LS=2, HS=3, SS=4, SSP=5. ID 0 (no
/// device, or a device that has not negotiated yet) and reserved IDs yield
/// `None`; callers degrade those to [`DeviceSpeed::Full`] so a
/// present-but-mid-reset device still reports a usable speed.
#[must_use]
pub const fn decode_port_speed(speed_id: u8) -> Option<DeviceSpeed> {
    match speed_id {
        1 => Some(DeviceSpeed::Full),
        2 => Some(DeviceSpeed::Low),
        3 => Some(DeviceSpeed::High),
        4 => Some(DeviceSpeed::Super),
        5 => Some(DeviceSpeed::SuperPlus),
        _ => None,
    }
}

/// One xHCI root-hub port as the port-scan sees it: the decoded PORTSC state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XhciPortStatus {
    /// 0-based port index.
    pub port: u8,
    /// The raw PORTSC dword the scan read.
    pub portsc: u32,
    /// A device is attached (CCS, bit 0).
    pub connected: bool,
    /// The port finished reset and is usable (PED, bit 1).
    pub enabled: bool,
    /// Port power is on (PP, bit 9).
    pub powered: bool,
    /// Negotiated speed, when the Speed ID field holds a valid ID.
    pub speed: Option<DeviceSpeed>,
}

/// Decode one PORTSC dword for 0-based `port` into an [`XhciPortStatus`].
#[must_use]
pub const fn parse_portsc(port: u8, portsc: u32) -> XhciPortStatus {
    let speed_id = ((portsc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8;
    XhciPortStatus {
        port,
        portsc,
        connected: portsc & PORTSC_CCS != 0,
        enabled: portsc & PORTSC_PED != 0,
        powered: portsc & PORTSC_PP != 0,
        speed: decode_port_speed(speed_id),
    }
}

/// Provisional descriptor for a device the port-scan found.
///
/// A port-scan cannot issue control transfers — that needs the full host xHCI
/// driver (Phase 6.5). Until the driver re-enumerates the port and replaces
/// it, the device carries a zeroed VID/PID, no strings, and USB 2.0-era
/// defaults. VID/PID-keyed routing rules cannot match such a device; port-path
/// and default rules can, which is exactly what the routing engine needs to
/// park the device with the hypervisor until its identity resolves.
#[must_use]
pub const fn provisional_descriptor() -> UsbDeviceDescriptor {
    UsbDeviceDescriptor {
        vendor_id: 0,
        product_id: 0,
        device_class: UsbDeviceClass::PerInterface,
        device_subclass: 0,
        device_protocol: 0,
        manufacturer: None,
        product: None,
        serial_number: None,
        max_packet_size_0: 64,
        num_configurations: 1,
        usb_version: (2, 0),
        device_version: (0, 0),
    }
}

// ---------------------------------------------------------------------------
// Scan / sync / poll
// ---------------------------------------------------------------------------

/// Scan the controller behind `reader` for attached devices.
///
/// A device is reported for every port whose CCS bit is set — a port with a
/// device present but not yet enabled (mid-reset) reports too, its speed
/// defaulting to [`DeviceSpeed::Full`] until the Speed ID field is valid. The
/// port path is `bus-(port+1)`: xHCI ports are 1-based in the register model
/// while the reader is 0-based. Ports the reader cannot read are skipped.
#[must_use]
pub fn scan_xhci_ports(
    reader: &dyn XhciPortReader,
) -> Vec<(UsbPortPath, UsbDeviceDescriptor, DeviceSpeed)> {
    let mut out = Vec::new();
    for port in 0..reader.port_count() {
        let Some(portsc) = reader.read_portsc(port) else {
            continue;
        };
        let status = parse_portsc(port, portsc);
        if !status.connected {
            continue;
        }
        let path = UsbPortPath::new(reader.bus_number(), vec![port + 1]);
        let speed = status.speed.unwrap_or(DeviceSpeed::Full);
        out.push((path, provisional_descriptor(), speed));
    }
    out
}

/// Reconcile `monitor`'s inventory against a fresh port-scan of the controller
/// behind `reader`.
///
/// Fires the hot-plug events the port set changed by: a
/// [`report_connect`](UsbMonitor::report_connect) for every attached device
/// the monitor did not know, and a
/// [`report_disconnect`](UsbMonitor::report_disconnect) for every device the
/// monitor tracked that has vanished. Calling it repeatedly is the poll cycle:
/// the first call connects the whole current port set, subsequent calls emit
/// only the deltas.
///
/// Returns `(connected, disconnected)` — the number of each event fired — so
/// a poll loop can tell whether anything moved.
///
/// # Errors
/// Propagates [`report_connect`](UsbMonitor::report_connect)'s
/// [`AddressExhausted`](super::types::UsbError::AddressExhausted) if the
/// monitor runs out of USB addresses. A disconnect that races another remover
/// is ignored (the device is already gone, which is the intended end state).
pub fn sync_xhci_monitor(
    monitor: &UsbMonitor,
    reader: &dyn XhciPortReader,
) -> UsbResult<(usize, usize)> {
    let scanned = scan_xhci_ports(reader);
    let scanned_paths: HashSet<&UsbPortPath> = scanned.iter().map(|(p, _, _)| p).collect();
    let current = monitor.devices();

    let mut connected = 0;
    for (path, descriptor, speed) in &scanned {
        if !current.contains_key(path) {
            monitor.report_connect(path.clone(), descriptor.clone(), *speed)?;
            connected += 1;
        }
    }

    let mut disconnected = 0;
    for path in current.keys() {
        if !scanned_paths.contains(path) {
            // A NotFound here means it was already removed concurrently, which
            // is the state we want — treat it as a successful disconnect.
            let _ = monitor.report_disconnect(path);
            disconnected += 1;
        }
    }

    Ok((connected, disconnected))
}

/// Cadence-respecting poll cycle: run a [`sync_xhci_monitor`] only if the
/// monitor's poll interval has elapsed since its last poll, then record the
/// poll.
///
/// This is the drop-in the run loop calls every iteration: it self-throttles
/// to the monitor's configured [`poll_interval`](UsbMonitor::poll_interval) via
/// [`should_poll`](UsbMonitor::should_poll) /
/// [`mark_polled`](UsbMonitor::mark_polled), so a PORTSC walk (volatile MMIO
/// reads) happens at most once per interval no matter how often the loop
/// ticks. Returns `Some((connected, disconnected))` with the deltas when a
/// scan ran, or `None` when the interval had not yet elapsed and nothing was
/// scanned.
///
/// # Errors
/// Propagates [`sync_xhci_monitor`]'s error when a scan runs.
pub fn poll_xhci(
    monitor: &UsbMonitor,
    reader: &dyn XhciPortReader,
) -> UsbResult<Option<(usize, usize)>> {
    if !monitor.should_poll() {
        return Ok(None);
    }
    let deltas = sync_xhci_monitor(monitor, reader)?;
    monitor.mark_polled();
    Ok(Some(deltas))
}

#[cfg(test)]
mod tests {
    use super::super::monitor::UsbHotplugEvent;
    use super::*;
    use std::time::Duration;

    /// Canned reader: entry `i` is the PORTSC dword for 0-based port `i`;
    /// a reader shorter than the port count makes the tail unreadable.
    struct FakeReader {
        ports: Vec<u32>,
        bus: u8,
    }

    impl FakeReader {
        fn new(bus: u8, ports: &[u32]) -> Self {
            Self {
                bus,
                ports: ports.to_vec(),
            }
        }
    }

    impl XhciPortReader for FakeReader {
        fn port_count(&self) -> u8 {
            // Claim one more port than we have values for, so the scan also
            // exercises the unreadable-port skip.
            u8::try_from(self.ports.len())
                .expect("test port count fits in u8")
                .saturating_add(1)
        }

        fn bus_number(&self) -> u8 {
            self.bus
        }

        fn read_portsc(&self, port: u8) -> Option<u32> {
            self.ports.get(usize::from(port)).copied()
        }
    }

    /// Build a PORTSC value: CCS/PED from `connected`/`enabled`, PP always on,
    /// and the given Speed ID in bits 13:10.
    fn portsc(connected: bool, enabled: bool, speed_id: u8) -> u32 {
        let mut v = PORTSC_PP | (u32::from(speed_id) << PORTSC_SPEED_SHIFT);
        if connected {
            v |= PORTSC_CCS;
        }
        if enabled {
            v |= PORTSC_PED;
        }
        v
    }

    #[test]
    fn decodes_all_default_speed_ids() {
        assert_eq!(decode_port_speed(1), Some(DeviceSpeed::Full));
        assert_eq!(decode_port_speed(2), Some(DeviceSpeed::Low));
        assert_eq!(decode_port_speed(3), Some(DeviceSpeed::High));
        assert_eq!(decode_port_speed(4), Some(DeviceSpeed::Super));
        assert_eq!(decode_port_speed(5), Some(DeviceSpeed::SuperPlus));
        // ID 0 (no device / not yet negotiated) and reserved IDs are unknown.
        assert_eq!(decode_port_speed(0), None);
        assert_eq!(decode_port_speed(6), None);
        assert_eq!(decode_port_speed(15), None);
    }

    #[test]
    fn speed_ids_round_trip_with_xhci_speed_id() {
        // The decode is the exact inverse of DeviceSpeed::xhci_speed_id.
        for speed in [
            DeviceSpeed::Low,
            DeviceSpeed::Full,
            DeviceSpeed::High,
            DeviceSpeed::Super,
            DeviceSpeed::SuperPlus,
        ] {
            assert_eq!(decode_port_speed(speed.xhci_speed_id()), Some(speed));
        }
    }

    #[test]
    fn parses_portsc_flags_and_speed() {
        let status = parse_portsc(2, portsc(true, true, 3));
        assert_eq!(status.port, 2);
        assert!(status.connected);
        assert!(status.enabled);
        assert!(status.powered);
        assert_eq!(status.speed, Some(DeviceSpeed::High));

        // A port with nothing attached decodes as disconnected even if the
        // power bit is set (typical unpopulated-port state).
        let empty = parse_portsc(0, PORTSC_PP);
        assert!(!empty.connected);
        assert!(!empty.enabled);
        assert!(empty.powered);
        assert_eq!(empty.speed, None);
    }

    #[test]
    fn scan_reports_connected_ports_with_1based_port_numbers() {
        // Port 0: high-speed device, reset complete. Port 1: empty.
        // Port 2: SuperSpeed device present but not yet enabled (mid-reset).
        let reader = FakeReader::new(
            0,
            &[
                portsc(true, true, 3),
                portsc(false, false, 0),
                portsc(true, false, 4),
            ],
        );
        // (The reader claims a 4th port it cannot serve; the scan must skip
        // it without error.)

        let found = scan_xhci_ports(&reader);
        assert_eq!(found.len(), 2);

        let (path0, desc0, speed0) = &found[0];
        assert_eq!(*path0, UsbPortPath::new(0, vec![1]));
        assert_eq!(*speed0, DeviceSpeed::High);
        assert_eq!(desc0.vendor_id, 0);
        assert_eq!(desc0.product_id, 0);
        assert_eq!(desc0.max_packet_size_0, 64);
        assert!(desc0.manufacturer.is_none());

        let (path1, _, speed1) = &found[1];
        assert_eq!(*path1, UsbPortPath::new(0, vec![3]));
        assert_eq!(*speed1, DeviceSpeed::Super);
    }

    #[test]
    fn mid_reset_device_reports_with_default_speed() {
        // CCS set but the Speed ID field not yet valid: the device is present,
        // and the scan reports it at the Full default rather than skipping it.
        let reader = FakeReader::new(0, &[portsc(true, false, 0)]);
        let found = scan_xhci_ports(&reader);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, UsbPortPath::new(0, vec![1]));
        assert_eq!(found[0].2, DeviceSpeed::Full);
    }

    #[test]
    fn bus_number_comes_from_the_reader() {
        // A second physical controller appears under its own bus number so its
        // ports never collide with the first controller's in the inventory.
        let reader = FakeReader::new(2, &[portsc(true, true, 3)]);
        let found = scan_xhci_ports(&reader);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, UsbPortPath::new(2, vec![1]));
    }

    #[test]
    fn mmio_reader_offset_math_matches_xhci_port_layout() {
        // A heap allocation stands in for the MMIO window: PORTSC of port i
        // lives at base + 0x400 + i * 0x10 (byte offsets → /4 for u32 words).
        let mut window = vec![0u32; 512];
        let base = window.as_ptr() as u64;
        let port_word = |i: usize| 0x400 / 4 + i * (0x10 / 4);
        window[port_word(0)] = portsc(true, true, 3);
        window[port_word(2)] = portsc(true, false, 4);

        let reader = unsafe { XhciMmioPortReader::new(base, 4, 0) };
        assert_eq!(reader.portsc_address(0), base + 0x400);
        assert_eq!(reader.portsc_address(2), base + 0x400 + 2 * 0x10);

        assert_eq!(reader.read_portsc(0), Some(portsc(true, true, 3)));
        assert_eq!(reader.read_portsc(1), Some(0));
        assert_eq!(reader.read_portsc(2), Some(portsc(true, false, 4)));
        // Past the claimed port count: unreadable, not an error.
        assert_eq!(reader.read_portsc(4), None);

        let found = scan_xhci_ports(&reader);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, UsbPortPath::new(0, vec![1]));
        assert_eq!(found[1].0, UsbPortPath::new(0, vec![3]));
        assert_eq!(found[1].2, DeviceSpeed::Super);
    }

    #[test]
    fn sync_xhci_monitor_fires_connect_then_disconnect_deltas() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Two devices: a high-speed device on port 1 and a full-speed device
        // on port 2.
        let present = |p2: bool| {
            FakeReader::new(
                0,
                &[
                    portsc(true, true, 3),
                    if p2 {
                        portsc(true, true, 1)
                    } else {
                        portsc(false, false, 0)
                    },
                ],
            )
        };

        let monitor = UsbMonitor::new(Duration::from_millis(0));
        let connects = Arc::new(AtomicUsize::new(0));
        let disconnects = Arc::new(AtomicUsize::new(0));
        {
            let c = Arc::clone(&connects);
            let d = Arc::clone(&disconnects);
            monitor.on_hotplug(Box::new(move |ev| match ev {
                UsbHotplugEvent::Connected(_) => {
                    c.fetch_add(1, Ordering::SeqCst);
                }
                UsbHotplugEvent::Disconnected(_) => {
                    d.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }

        // First sync: both devices connect.
        assert_eq!(sync_xhci_monitor(&monitor, &present(true)).unwrap(), (2, 0));
        assert_eq!(monitor.device_count(), 2);
        assert_eq!(connects.load(Ordering::SeqCst), 2);

        // Idempotent: a second sync of the unchanged port set fires nothing.
        assert_eq!(sync_xhci_monitor(&monitor, &present(true)).unwrap(), (0, 0));

        // Unplug the port-2 device: exactly one disconnect fires, and the
        // surviving device keeps its provisional identity.
        assert_eq!(
            sync_xhci_monitor(&monitor, &present(false)).unwrap(),
            (0, 1)
        );
        assert_eq!(monitor.device_count(), 1);
        assert_eq!(disconnects.load(Ordering::SeqCst), 1);
        let remaining = monitor
            .device_by_port(&UsbPortPath::new(0, vec![1]))
            .unwrap();
        assert_eq!(remaining.speed, DeviceSpeed::High);
        assert_eq!(remaining.descriptor.vendor_id, 0);
    }

    #[test]
    fn poll_xhci_respects_the_monitor_cadence() {
        // A monitor with a long interval was just constructed, so a poll is
        // not yet due: poll_xhci scans nothing and reports None.
        let slow = UsbMonitor::new(Duration::from_secs(3600));
        let reader = FakeReader::new(0, &[portsc(true, true, 3)]);
        assert_eq!(poll_xhci(&slow, &reader).unwrap(), None, "not due yet");
        assert_eq!(slow.device_count(), 0, "nothing scanned when not due");

        // A zero-interval monitor is always due: poll_xhci runs the sync and
        // reports the deltas.
        let eager = UsbMonitor::new(Duration::from_millis(0));
        assert_eq!(poll_xhci(&eager, &reader).unwrap(), Some((1, 0)));
        assert_eq!(eager.device_count(), 1);
    }
}
