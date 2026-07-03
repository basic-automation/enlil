//! Linux sysfs USB enumeration backend.
//!
//! The [`UsbMonitor`](super::monitor::UsbMonitor) exposes
//! [`report_connect`](super::monitor::UsbMonitor::report_connect) /
//! [`report_disconnect`](super::monitor::UsbMonitor::report_disconnect) as the
//! seam a host device source drives; this module is the concrete Linux source
//! that populates it from the kernel's USB device tree under
//! `/sys/bus/usb/devices`. Each attached device is a directory there whose name
//! *is* its port path (`B-P.P` for an attached device, `usbN` for a root hub),
//! carrying the descriptor as plain attribute files (`idVendor`, `idProduct`,
//! `bDeviceClass`, `manufacturer`, `product`, `serial`, `speed`, …).
//!
//! The parsing is split from the I/O so it is unit-testable without a real
//! `/sys`: [`parse_device`] takes the directory name plus an attribute-reader
//! closure and yields the enumerated device; [`scan_devices`] is the thin
//! `std::fs` walk that supplies that closure from real files; and
//! [`sync_monitor`] reconciles a fresh scan against the monitor's inventory,
//! firing exactly the connect/disconnect events the tree changed by.
//!
//! Only the filesystem walk is Linux-specific in intent; the code itself
//! compiles everywhere (`std::fs` on a host with no `/sys` simply yields an
//! empty scan), which keeps the Windows dev build honest.

use std::collections::HashSet;
use std::path::Path;

use super::monitor::UsbMonitor;
use super::types::{DeviceSpeed, UsbDeviceClass, UsbDeviceDescriptor, UsbPortPath, UsbResult};

/// The canonical sysfs root the kernel exposes the USB device tree under.
pub const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Parse a sysfs device-directory name into a [`UsbPortPath`].
///
/// Root hubs are named `usbN` (bus `N`, no downstream ports); an attached
/// device is named `B-P[.P]*` (bus `B` then its hub-port chain, e.g. `1-2.3`).
/// Interface directories carry a `:` (`1-2.3:1.0`) and are not devices, so they
/// yield `None`, as does any name that does not parse.
#[must_use]
pub fn parse_port_path(name: &str) -> Option<UsbPortPath> {
    if name.contains(':') {
        return None; // an interface, not a device
    }
    if let Some(bus) = name.strip_prefix("usb") {
        return bus
            .parse::<u8>()
            .ok()
            .map(|b| UsbPortPath::new(b, Vec::new()));
    }
    let (bus_str, ports_str) = name.split_once('-')?;
    let bus = bus_str.parse::<u8>().ok()?;
    let ports = ports_str
        .split('.')
        .map(str::parse::<u8>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if ports.is_empty() {
        return None;
    }
    Some(UsbPortPath::new(bus, ports))
}

/// Map a sysfs `speed` attribute (link speed in Mbps, as a string) to a
/// [`DeviceSpeed`]. Unknown/blank values yield `None`.
#[must_use]
pub fn parse_speed(raw: &str) -> Option<DeviceSpeed> {
    match raw.trim() {
        "1.5" => Some(DeviceSpeed::Low),
        "12" => Some(DeviceSpeed::Full),
        "480" => Some(DeviceSpeed::High),
        "5000" => Some(DeviceSpeed::Super),
        // 10 Gbps (USB 3.1 Gen2) and 20 Gbps (USB 3.2 Gen2x2) both attach over
        // the SuperSpeed+ protocol.
        "10000" | "20000" => Some(DeviceSpeed::SuperPlus),
        _ => None,
    }
}

/// Parse a dotted BCD-style version string (`" 2.00"`, `"01.10"`) into a
/// `(major, minor)` pair, tolerant of leading whitespace. Missing/garbled input
/// yields `(0, 0)`.
fn parse_version(raw: Option<&str>) -> (u8, u8) {
    let Some(raw) = raw else { return (0, 0) };
    let trimmed = raw.trim();
    let (maj, min) = trimmed.split_once('.').unwrap_or((trimmed, "0"));
    (
        maj.trim().parse::<u8>().unwrap_or(0),
        min.trim().parse::<u8>().unwrap_or(0),
    )
}

/// Build a device from its sysfs directory `name` and an `attr` closure.
///
/// Yields the device's [`UsbPortPath`], [`UsbDeviceDescriptor`] and negotiated
/// [`DeviceSpeed`]; `attr` reads one attribute file by name (already trimmed),
/// or `None` if absent.
///
/// Returns `None` when `name` is not an attached-device directory or the
/// mandatory `idVendor`/`idProduct` attributes are missing — that filters out
/// interface nodes and non-device entries without the caller special-casing
/// them. String attributes (`manufacturer`/`product`/`serial`) and the version
/// fields are best-effort: absent ones simply stay `None`/`0`.
#[must_use]
pub fn parse_device(
    name: &str,
    attr: impl Fn(&str) -> Option<String>,
) -> Option<(UsbPortPath, UsbDeviceDescriptor, DeviceSpeed)> {
    let port_path = parse_port_path(name)?;

    let hex16 = |s: String| u16::from_str_radix(s.trim(), 16).ok();
    let hex8 = |s: String| u8::from_str_radix(s.trim(), 16).ok();
    let dec8 = |s: String| s.trim().parse::<u8>().ok();

    // idVendor/idProduct are mandatory; their absence means this is not an
    // enumerable device directory (a bare hub node or a stale entry).
    let vendor_id = attr("idVendor").and_then(hex16)?;
    let product_id = attr("idProduct").and_then(hex16)?;

    let device_class = attr("bDeviceClass")
        .and_then(hex8)
        .map_or(UsbDeviceClass::from_code(0), UsbDeviceClass::from_code);
    let device_subclass = attr("bDeviceSubClass").and_then(hex8).unwrap_or(0);
    let device_protocol = attr("bDeviceProtocol").and_then(hex8).unwrap_or(0);
    let max_packet_size_0 = attr("bMaxPacketSize0").and_then(dec8).unwrap_or(64);
    let num_configurations = attr("bNumConfigurations").and_then(dec8).unwrap_or(1);
    let usb_version = parse_version(attr("version").as_deref());
    let device_version = parse_version(attr("bcdDevice").as_deref());

    // sysfs represents the "no string descriptor" case by the file being
    // absent; a present-but-empty string is likewise treated as no value.
    let opt_str = |a: &str| attr(a).filter(|s| !s.is_empty());

    let descriptor = UsbDeviceDescriptor {
        vendor_id,
        product_id,
        device_class,
        device_subclass,
        device_protocol,
        manufacturer: opt_str("manufacturer"),
        product: opt_str("product"),
        serial_number: opt_str("serial"),
        max_packet_size_0,
        num_configurations,
        usb_version,
        device_version,
    };

    let speed = attr("speed")
        .as_deref()
        .and_then(parse_speed)
        .unwrap_or(DeviceSpeed::Full);

    Some((port_path, descriptor, speed))
}

/// Read one trimmed sysfs attribute file from `dir`, or `None` if it is absent
/// or unreadable.
fn read_attr(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Walk the sysfs USB device tree rooted at `root` (normally
/// [`SYSFS_USB_DEVICES`]) and return every attached device it enumerates.
///
/// A `root` that does not exist or cannot be read yields an empty vector rather
/// than an error — on a host with no `/sys` (or a bare-metal target that has not
/// mounted one) there are simply no sysfs devices to report, which the caller
/// treats identically to "nothing attached".
#[must_use]
pub fn scan_devices(
    root: impl AsRef<Path>,
) -> Vec<(UsbPortPath, UsbDeviceDescriptor, DeviceSpeed)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.as_ref()) else {
        return out;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let dir = entry.path();
        if let Some(dev) = parse_device(name, |a| read_attr(&dir, a)) {
            out.push(dev);
        }
    }
    out
}

/// Reconcile `monitor`'s inventory against a fresh scan of the sysfs tree.
///
/// Fires the hot-plug events the tree changed by: a
/// [`report_connect`](UsbMonitor::report_connect) for every device now present
/// that the monitor did not know, and a
/// [`report_disconnect`](UsbMonitor::report_disconnect) for every device the
/// monitor tracked that has vanished.
///
/// Returns `(connected, disconnected)` — the number of each event fired — so a
/// poll loop can tell whether anything moved. Calling it repeatedly is the poll
/// cycle: the first call connects the whole current tree, subsequent calls emit
/// only the deltas.
///
/// # Errors
/// Propagates [`report_connect`](UsbMonitor::report_connect)'s
/// [`AddressExhausted`](super::types::UsbError::AddressExhausted) if the monitor
/// runs out of USB addresses. A disconnect that races another remover is
/// ignored (the device is already gone, which is the intended end state).
pub fn sync_monitor(monitor: &UsbMonitor, root: impl AsRef<Path>) -> UsbResult<(usize, usize)> {
    let scanned = scan_devices(root);
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

#[cfg(test)]
mod tests {
    use super::super::monitor::UsbHotplugEvent;
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parses_attached_device_and_root_hub_port_paths() {
        assert_eq!(
            parse_port_path("1-2.3"),
            Some(UsbPortPath::new(1, vec![2, 3]))
        );
        assert_eq!(parse_port_path("2-1"), Some(UsbPortPath::new(2, vec![1])));
        assert_eq!(
            parse_port_path("usb1"),
            Some(UsbPortPath::new(1, Vec::new()))
        );
        // Interfaces and junk are not devices.
        assert_eq!(parse_port_path("1-2.3:1.0"), None);
        assert_eq!(parse_port_path("not-a-path"), None);
    }

    #[test]
    fn maps_all_link_speeds() {
        assert_eq!(parse_speed("1.5"), Some(DeviceSpeed::Low));
        assert_eq!(parse_speed("12"), Some(DeviceSpeed::Full));
        assert_eq!(parse_speed(" 480 "), Some(DeviceSpeed::High));
        assert_eq!(parse_speed("5000"), Some(DeviceSpeed::Super));
        assert_eq!(parse_speed("10000"), Some(DeviceSpeed::SuperPlus));
        assert_eq!(parse_speed("20000"), Some(DeviceSpeed::SuperPlus));
        assert_eq!(parse_speed(""), None);
    }

    fn attrs(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |a: &str| map.get(a).cloned()
    }

    #[test]
    fn parses_a_full_device_descriptor_from_sysfs_attributes() {
        // A Logitech Unifying receiver on bus 1, port 2: HID class, high speed.
        let attr = attrs(&[
            ("idVendor", "046d"),
            ("idProduct", "c52b"),
            ("bDeviceClass", "00"),
            ("bDeviceSubClass", "00"),
            ("bDeviceProtocol", "00"),
            ("bMaxPacketSize0", "64"),
            ("bNumConfigurations", "1"),
            ("version", " 2.00"),
            ("bcdDevice", "12.01"),
            ("manufacturer", "Logitech"),
            ("product", "USB Receiver"),
            ("serial", "ABC123"),
            ("speed", "480"),
        ]);
        let (path, desc, speed) = parse_device("1-2", attr).expect("device parses");
        assert_eq!(path, UsbPortPath::new(1, vec![2]));
        assert_eq!(desc.vendor_id, 0x046d);
        assert_eq!(desc.product_id, 0xc52b);
        assert_eq!(desc.device_class, UsbDeviceClass::from_code(0));
        assert_eq!(desc.max_packet_size_0, 64);
        assert_eq!(desc.num_configurations, 1);
        assert_eq!(desc.usb_version, (2, 0));
        assert_eq!(desc.device_version, (12, 1));
        assert_eq!(desc.manufacturer.as_deref(), Some("Logitech"));
        assert_eq!(desc.product.as_deref(), Some("USB Receiver"));
        assert_eq!(desc.serial_number.as_deref(), Some("ABC123"));
        assert_eq!(speed, DeviceSpeed::High);
    }

    #[test]
    fn rejects_a_node_without_vendor_product_and_tolerates_missing_strings() {
        // A hub-internal node with no idVendor is not an enumerable device.
        assert!(parse_device("1-0", attrs(&[("bDeviceClass", "09")])).is_none());

        // Mandatory IDs present but every optional attribute absent: still a
        // device, with defaults and no string descriptors.
        let (_, desc, speed) =
            parse_device("3-4", attrs(&[("idVendor", "1234"), ("idProduct", "5678")]))
                .expect("minimal device parses");
        assert_eq!(desc.max_packet_size_0, 64);
        assert_eq!(desc.num_configurations, 1);
        assert!(desc.manufacturer.is_none());
        assert!(desc.serial_number.is_none());
        assert_eq!(speed, DeviceSpeed::Full);
    }

    #[test]
    fn scan_of_a_missing_root_is_empty() {
        assert!(scan_devices("/definitely/not/a/real/sysfs/path").is_empty());
    }

    #[test]
    fn sync_monitor_fires_connect_then_disconnect_deltas() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        // Build a fake sysfs tree in a scratch dir under the target's temp area.
        let base = std::env::temp_dir().join(format!("enlil-sysfs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("devices");
        let write_device = |name: &str, attrs: &[(&str, &str)]| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            for (k, v) in attrs {
                std::fs::write(dir.join(k), v).unwrap();
            }
        };
        // Two real devices plus an interface node (must be ignored).
        write_device(
            "1-1",
            &[
                ("idVendor", "046d"),
                ("idProduct", "c52b"),
                ("speed", "480"),
            ],
        );
        write_device(
            "1-2",
            &[("idVendor", "8087"), ("idProduct", "0029"), ("speed", "12")],
        );
        write_device("1-1:1.0", &[("bInterfaceClass", "03")]);

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

        // First sync: both devices connect, the interface node is skipped.
        assert_eq!(sync_monitor(&monitor, &root).unwrap(), (2, 0));
        assert_eq!(monitor.device_count(), 2);
        assert_eq!(connects.load(Ordering::SeqCst), 2);

        // Idempotent: a second sync of the unchanged tree fires nothing.
        assert_eq!(sync_monitor(&monitor, &root).unwrap(), (0, 0));

        // Unplug one device: the next sync fires exactly one disconnect.
        std::fs::remove_dir_all(root.join("1-2")).unwrap();
        assert_eq!(sync_monitor(&monitor, &root).unwrap(), (0, 1));
        assert_eq!(monitor.device_count(), 1);
        assert_eq!(disconnects.load(Ordering::SeqCst), 1);
        assert!(monitor.device_by_vid_pid(0x046d, 0xc52b).is_some());

        let _ = std::fs::remove_dir_all(&base);
    }
}
