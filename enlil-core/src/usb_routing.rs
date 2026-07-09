//! Adapter from the [`enlil_config`] USB routing schema to the
//! [`enlil_devices`] routing engine.
//!
//! `enlil-config` stays independent of `enlil-devices` (the layer boundary), so
//! the config crate parses `[usb.routing]` into structured
//! [`UsbMatchKind`](enlil_config::UsbMatchKind)s and this module — which sees
//! both crates — maps a parsed [`UsbConfig`] into a live
//! [`RoutingTable`](enlil_devices::usb::routing::RoutingTable). This is the
//! bridge that makes a configured `[usb.routing]` block actually drive the
//! hot-plug router (Phase 4.3).

use enlil_config::{UsbConfig, UsbMatchKind};
use enlil_devices::usb::registry::XhciRegistry;
use enlil_devices::usb::routing::{DeviceMatcher, RoutingState, RoutingTable};
use enlil_devices::usb::types::{GuestId, UsbDeviceClass};
use enlil_devices::usb::SharedXhci;

/// Map a config-layer USB device-class token to a [`UsbDeviceClass`].
///
/// Accepts the common names (`hid`, `hub`, `audio`, `cdc`, `printer`, `image`,
/// `mass-storage`/`storage`, `physical`, `vendor`/`vendor-specific`,
/// `per-interface`) case-insensitively, plus a raw class code as `0xNN` or a
/// decimal byte.
///
/// # Errors
/// Returns a message when the token is neither a known name nor a valid class
/// code byte.
fn device_class_from_token(token: &str) -> Result<UsbDeviceClass, String> {
    let normalized = token.trim().to_ascii_lowercase();
    let class = match normalized.as_str() {
        "per-interface" | "perinterface" => UsbDeviceClass::PerInterface,
        "audio" => UsbDeviceClass::Audio,
        "cdc" | "comm" | "communications" => UsbDeviceClass::Cdc,
        "hid" => UsbDeviceClass::Hid,
        "physical" => UsbDeviceClass::Physical,
        "image" => UsbDeviceClass::Image,
        "printer" => UsbDeviceClass::Printer,
        "mass-storage" | "massstorage" | "storage" | "msc" => UsbDeviceClass::MassStorage,
        "hub" => UsbDeviceClass::Hub,
        "vendor" | "vendor-specific" | "vendorspecific" => UsbDeviceClass::VendorSpecific,
        other => {
            let code = other
                .strip_prefix("0x")
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .or_else(|| other.parse::<u8>().ok())
                .ok_or_else(|| format!("unknown USB device class '{token}'"))?;
            UsbDeviceClass::from_code(code)
        }
    };
    Ok(class)
}

/// Map a parsed config match kind into the device crate's [`DeviceMatcher`].
fn matcher_from_kind(kind: UsbMatchKind) -> Result<DeviceMatcher, String> {
    Ok(match kind {
        UsbMatchKind::VidPid {
            vendor_id,
            product_id,
        } => DeviceMatcher::VidPid {
            vendor_id,
            product_id,
        },
        UsbMatchKind::VendorOnly { vendor_id } => DeviceMatcher::VendorOnly { vendor_id },
        UsbMatchKind::PortPath(path) => DeviceMatcher::PortPath(path),
        UsbMatchKind::Serial(serial) => DeviceMatcher::Serial(serial),
        UsbMatchKind::DeviceClass(name) => {
            DeviceMatcher::DeviceClass(device_class_from_token(&name)?)
        }
        UsbMatchKind::Any => DeviceMatcher::Any,
    })
}

/// Convert a parsed [`UsbConfig`] into a live
/// [`RoutingTable`](enlil_devices::usb::routing::RoutingTable): its
/// `default_guest` becomes the table default, and each `[[usb.routing]]` rule is
/// added at its configured priority.
///
/// # Errors
/// Returns a message if a rule's match spec fails to parse or names an unknown
/// device-class token. On a config already accepted by
/// [`enlil_config::validate_config`] (which checks specs parse and targets
/// exist) the only remaining failure is a device-class token the config layer
/// intentionally leaves unchecked.
pub fn routing_table_from_config(usb: &UsbConfig) -> Result<RoutingTable, String> {
    let mut table = RoutingTable::new();
    table.set_default_guest(usb.default_guest.clone());
    for (i, rule) in usb.routing.iter().enumerate() {
        let kind = rule
            .parsed_match()
            .map_err(|e| format!("USB rule {i}: {e}"))?;
        let matcher = matcher_from_kind(kind).map_err(|e| format!("USB rule {i}: {e}"))?;
        table.add_rule(rule.priority, matcher, rule.target.clone());
    }
    Ok(table)
}

/// Build a live [`RoutingState`](enlil_devices::usb::routing::RoutingState) from
/// a parsed [`UsbConfig`], ready to hand to an
/// [`XhciRegistry`](enlil_devices::usb::registry::XhciRegistry) /
/// [`HotplugDispatcher`](enlil_devices::usb::hotplug::HotplugDispatcher) in the
/// guest-setup path.
///
/// This is the caller-ready form of [`routing_table_from_config`]: it wraps the
/// built [`RoutingTable`] in the thread-safe `RoutingState` the hot-plug router
/// and the management console share, so a configured `[usb.routing]` block
/// directly populates the live routing surface (Phase 4.3). An empty/absent
/// `[usb.routing]` yields an empty table with no default guest — every plugged
/// device then lands on [`RoutingDecision::NoRoute`](enlil_devices::usb::routing::RoutingDecision::NoRoute),
/// exactly as an unconfigured host behaves.
///
/// # Errors
/// As [`routing_table_from_config`].
pub fn routing_state_from_config(usb: &UsbConfig) -> Result<RoutingState, String> {
    Ok(RoutingState::new(routing_table_from_config(usb)?))
}

/// Every guest a parsed `[usb.routing]` block can send a device to: the
/// `target` of each routing rule plus the `default_guest`, deduplicated and
/// sorted.
///
/// The guest-setup path uses this to know which guests must have a virtual
/// xHCI controller registered before the router goes live — a rule targeting a
/// guest with no controller can never attach
/// ([`RegistryError::NoController`](enlil_devices::usb::registry::RegistryError::NoController)).
///
/// # Errors
/// As [`routing_table_from_config`] — the config must parse before its targets
/// can be enumerated.
pub fn routing_targets(usb: &UsbConfig) -> Result<Vec<GuestId>, String> {
    let table = routing_table_from_config(usb)?;
    let mut targets: Vec<GuestId> = table
        .rules()
        .iter()
        .map(|rule| rule.target.clone())
        .chain(table.default_guest().cloned())
        .collect();
    targets.sort();
    targets.dedup();
    Ok(targets)
}

/// Assemble the live USB hot-plug registry for a running configuration: build
/// the routing engine from `[usb.routing]` and bind it to each guest's virtual
/// xHCI controller.
///
/// `controllers` supplies the per-guest [`SharedXhci`] handles — the same
/// handles the guests' MMIO windows use — so an attach the router decides
/// raises the Port Status Change interrupt on the right guest's controller.
/// This is the guest-setup caller Phase 4.3 was waiting on the top-level
/// orchestrator (item 3.12) to unblock: a configured `[usb.routing]` block now
/// populates a ready-to-service [`XhciRegistry`], not just a bare
/// [`RoutingState`].
///
/// Every routing target — each rule's `target` and the `default_guest` — must
/// have a registered controller; otherwise the registry would answer a
/// matching plug-in with
/// [`RegistryError::NoController`](enlil_devices::usb::registry::RegistryError::NoController)
/// at runtime. This checks that invariant up front and rejects a config that
/// routes to a guest with no controller, so the misconfiguration surfaces at
/// setup rather than on the first hot-plug.
///
/// # Errors
/// - the USB config fails to parse (as [`routing_table_from_config`]); or
/// - a routing target names a guest absent from `controllers`.
pub fn usb_registry_from_config<I, S>(
    usb: &UsbConfig,
    controllers: I,
) -> Result<XhciRegistry, String>
where
    I: IntoIterator<Item = (S, SharedXhci)>,
    S: Into<GuestId>,
{
    let mut registry = XhciRegistry::new(routing_state_from_config(usb)?);
    for (guest, controller) in controllers {
        registry.register_controller(guest, controller);
    }
    let missing: Vec<GuestId> = routing_targets(usb)?
        .into_iter()
        .filter(|guest| registry.controller(guest.as_str()).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "USB routing targets have no registered xHCI controller: {}",
            missing.join(", ")
        ));
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use enlil_config::UsbRoutingRule;

    fn rule(match_spec: &str, target: &str, priority: u32) -> UsbRoutingRule {
        UsbRoutingRule {
            match_spec: match_spec.into(),
            target: target.into(),
            priority,
        }
    }

    #[test]
    fn builds_a_routing_table_from_config_rules() {
        let usb = UsbConfig {
            default_guest: Some("linux1".into()),
            routing: vec![
                rule("046d:c52b", "linux1", 10),
                rule("046d:*", "linux2", 20),
                rule("port:1-1", "windows1", 30),
                rule("serial:ABC123", "linux1", 40),
                rule("class:hid", "linux1", 50),
                rule("*", "linux1", 1000),
            ],
        };
        let table = routing_table_from_config(&usb).expect("build table");
        assert_eq!(table.default_guest().map(String::as_str), Some("linux1"));

        let rules = table.rules();
        assert_eq!(rules.len(), 6);
        // Rules are stored sorted by priority; check each mapped matcher via its
        // Display form and its target.
        let by_display: Vec<(String, &str, u32)> = rules
            .iter()
            .map(|r| (r.matcher.to_string(), r.target.as_str(), r.priority))
            .collect();
        assert_eq!(by_display[0], ("046d:c52b".into(), "linux1", 10));
        assert_eq!(by_display[1], ("046d:*".into(), "linux2", 20));
        assert_eq!(by_display[2], ("port:1-1".into(), "windows1", 30));
        assert_eq!(by_display[3], ("serial:ABC123".into(), "linux1", 40));
        assert_eq!(by_display[4], ("class:Hid".into(), "linux1", 50));
        assert_eq!(by_display[5], ("*".into(), "linux1", 1000));
    }

    #[test]
    fn maps_device_class_tokens_including_numeric_codes() {
        assert_eq!(device_class_from_token("hid"), Ok(UsbDeviceClass::Hid));
        assert_eq!(
            device_class_from_token("MASS-STORAGE"),
            Ok(UsbDeviceClass::MassStorage)
        );
        // Numeric class code: 0x08 = Mass Storage.
        assert_eq!(
            device_class_from_token("0x08"),
            Ok(UsbDeviceClass::MassStorage)
        );
        assert_eq!(device_class_from_token("9"), Ok(UsbDeviceClass::Hub));
        assert!(device_class_from_token("nonsense").is_err());
    }

    #[test]
    fn empty_config_yields_an_empty_table_with_no_default() {
        let table = routing_table_from_config(&UsbConfig::default()).expect("build");
        assert!(table.default_guest().is_none());
        assert!(table.rules().is_empty());
    }

    #[test]
    fn routes_real_devices_through_the_built_table() {
        use enlil_devices::usb::routing::RoutingDecision;
        use enlil_devices::usb::types::{UsbDeviceClass, UsbDeviceId, UsbSpeed};

        let device = |vid: u16, pid: u16, class: UsbDeviceClass| UsbDeviceId {
            vendor_id: vid,
            product_id: pid,
            class,
            speed: UsbSpeed::High,
            serial: None,
            manufacturer: None,
            product: None,
            port_path: None,
        };

        let usb = UsbConfig {
            default_guest: Some("linux1".into()),
            routing: vec![
                rule("046d:c52b", "windows1", 10), // a specific mouse
                rule("class:hid", "linux2", 50),   // any other HID
            ],
        };
        let table = routing_table_from_config(&usb).expect("build");

        // The specific VID:PID wins over the class rule (lower priority number).
        assert_eq!(
            table.route(&device(0x046d, 0xc52b, UsbDeviceClass::Hid)),
            RoutingDecision::RouteToGuest("windows1".into())
        );
        // A different HID falls to the class rule.
        assert_eq!(
            table.route(&device(0x1234, 0x5678, UsbDeviceClass::Hid)),
            RoutingDecision::RouteToGuest("linux2".into())
        );
        // A non-HID matches no rule and lands on the default guest.
        assert_eq!(
            table.route(&device(0x1234, 0x5678, UsbDeviceClass::MassStorage)),
            RoutingDecision::RouteToGuest("linux1".into())
        );
    }

    #[test]
    fn builds_a_live_routing_state_that_assigns_a_plugged_device() {
        use enlil_devices::usb::routing::RoutingDecision;
        use enlil_devices::usb::types::{UsbDeviceClass, UsbDeviceId, UsbSpeed};

        let usb = UsbConfig {
            default_guest: Some("linux1".into()),
            routing: vec![rule("046d:c52b", "windows1", 10)],
        };
        let state = routing_state_from_config(&usb).expect("build state");

        let mouse = UsbDeviceId {
            vendor_id: 0x046d,
            product_id: 0xc52b,
            class: UsbDeviceClass::Hid,
            speed: UsbSpeed::High,
            serial: None,
            manufacturer: None,
            product: None,
            port_path: None,
        };
        // Plugging the configured mouse routes it to its guest and records the
        // assignment in the live state — the hot-plug path's exact operation.
        assert_eq!(
            state.assign_device(3, &mouse),
            RoutingDecision::RouteToGuest("windows1".into())
        );
        assert_eq!(state.get_assignment(3).as_deref(), Some("windows1"));
    }

    #[test]
    fn an_unknown_class_token_is_an_error() {
        let usb = UsbConfig {
            default_guest: None,
            routing: vec![rule("class:teleporter", "linux1", 10)],
        };
        // `RoutingTable` isn't `Debug`; drop the Ok payload before unwrap_err.
        let err = routing_table_from_config(&usb).map(|_| ()).unwrap_err();
        assert!(
            err.contains("teleporter"),
            "error names the bad token: {err}"
        );
    }

    /// A running xHCI controller mirroring the registry's own test helper:
    /// MaxSlotsEn set and Run/Stop enabled so it accepts an attach.
    fn running_controller(ports: u8) -> SharedXhci {
        use std::cell::RefCell;
        use std::rc::Rc;

        use enlil_devices::usb::VirtualXhciController;

        let mut c = VirtualXhciController::new(ports);
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x38, 8); // CONFIG: MaxSlotsEn
        c.write_register(op, 1); // USBCMD: R/S
        Rc::new(RefCell::new(c))
    }

    #[test]
    fn routing_targets_are_sorted_deduped_and_include_the_default() {
        let usb = UsbConfig {
            default_guest: Some("linux1".into()),
            routing: vec![
                rule("046d:c52b", "windows1", 10),
                rule("046d:*", "linux2", 20),
                // A second rule to the same guest must not duplicate it.
                rule("1532:0084", "linux2", 30),
            ],
        };
        let targets = routing_targets(&usb).expect("enumerate targets");
        assert_eq!(targets, vec!["linux1", "linux2", "windows1"]);
    }

    #[test]
    fn registry_binds_config_routing_to_the_target_guests_controller() {
        use enlil_devices::usb::types::{UsbDeviceClass, UsbDeviceId, UsbSpeed};

        let usb = UsbConfig {
            default_guest: Some("linux1".into()),
            routing: vec![rule("046d:c52b", "windows1", 10)],
        };
        // Both the rule target (windows1) and the default (linux1) have
        // controllers, so the assembly succeeds.
        let mut registry = usb_registry_from_config(
            &usb,
            [
                ("linux1", running_controller(4)),
                ("windows1", running_controller(4)),
            ],
        )
        .expect("assemble registry");

        let mouse = UsbDeviceId {
            vendor_id: 0x046d,
            product_id: 0xc52b,
            class: UsbDeviceClass::Hid,
            speed: UsbSpeed::High,
            serial: None,
            manufacturer: None,
            product: None,
            port_path: None,
        };
        // The plugged mouse lands on windows1's controller, exactly where the
        // config routed it.
        let outcome = registry.attach(3, &mouse).expect("attach routed device");
        assert!(
            matches!(
                outcome,
                enlil_devices::usb::AttachOutcome::Attached(ref p) if p.guest == "windows1"
            ),
            "routed device attaches to its configured guest: {outcome:?}"
        );
    }

    #[test]
    fn registry_rejects_a_routing_target_without_a_controller() {
        let usb = UsbConfig {
            default_guest: None,
            routing: vec![rule("046d:c52b", "windows1", 10)],
        };
        // windows1 has no registered controller — the assembly must refuse.
        let err = usb_registry_from_config(&usb, [("linux1", running_controller(4))])
            .map(|_| ())
            .unwrap_err();
        assert!(
            err.contains("windows1"),
            "error names the target lacking a controller: {err}"
        );
    }
}
