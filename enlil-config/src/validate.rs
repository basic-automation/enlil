use crate::EnlilConfig;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The log levels the hypervisor accepts (matched case-insensitively), per
/// `HypervisorConfig::log_level`.
const VALID_LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

/// Validate CPU-set assignments across all guests (LOCKED PRINCIPLE 5 —
/// isolation): every guest has cores, no core is duplicated within a guest, and
/// a core a guest holds with Dedicated/Auto scheduling is owned *exclusively* —
/// no other guest may list it, neither another dedicated guest nor a time-sliced
/// (`Timeslice`) guest, or the dedication is a lie (a `Timeslice` guest sharing
/// a dedicated core steals cycles the dedicated guest is promised). Two
/// `Timeslice` guests MAY share cores — that is what time-slicing a shared pool
/// means — so their overlap is allowed. Returns the list of violations.
fn validate_cpu_isolation(config: &EnlilConfig) -> Vec<String> {
    let mut errors = Vec::new();
    let mut dedicated_owner: HashMap<u32, &str> = HashMap::new();

    for (id, guest) in &config.guest {
        if guest.cpus.is_empty() {
            errors.push(format!("Guest '{id}': no CPUs assigned"));
        }

        // Duplicates within this guest.
        let mut local = HashSet::new();
        for &cpu in &guest.cpus {
            if !local.insert(cpu) {
                errors.push(format!("Guest '{id}': duplicate CPU {cpu}"));
            }
        }

        // Claim exclusive ownership of each core for a dedicated guest, flagging
        // a second dedicated claim on the same core.
        if guest.scheduling == crate::SchedulingMode::Dedicated
            || guest.scheduling == crate::SchedulingMode::Auto
        {
            for &cpu in &guest.cpus {
                if dedicated_owner.insert(cpu, id.as_str()).is_some() {
                    errors.push(format!(
                        "Guest '{id}': CPU {cpu} already assigned to another guest"
                    ));
                }
            }
        }
    }

    // Second pass (after every dedicated claim is recorded): a time-sliced guest
    // must not intrude on a core dedicated to a *different* guest.
    for (id, guest) in &config.guest {
        if guest.scheduling != crate::SchedulingMode::Timeslice {
            continue;
        }
        for &cpu in &guest.cpus {
            if let Some(&owner) = dedicated_owner.get(&cpu)
                && owner != id.as_str()
            {
                errors.push(format!(
                    "Guest '{id}': CPU {cpu} is dedicated to guest '{owner}' and \
                     cannot be time-sliced by another guest"
                ));
            }
        }
    }

    errors
}

/// Validate the `[usb]` routing block (Phase 4.3): every rule's match spec must
/// parse, and every rule target — plus the optional `default_guest` — must name
/// a defined guest, or a device would be routed to a guest that does not exist.
fn validate_usb_routing(config: &EnlilConfig) -> Vec<String> {
    let mut errors = Vec::new();
    let usb = &config.usb;

    if let Some(default_guest) = &usb.default_guest
        && !config.guest.contains_key(default_guest)
    {
        errors.push(format!(
            "USB default_guest '{default_guest}' is not a defined guest"
        ));
    }

    for (i, rule) in usb.routing.iter().enumerate() {
        if let Err(e) = crate::parse_usb_match(&rule.match_spec) {
            errors.push(format!(
                "USB routing rule {i} (match '{}'): {e}",
                rule.match_spec
            ));
        }
        if !config.guest.contains_key(&rule.target) {
            errors.push(format!(
                "USB routing rule {i} targets guest '{}', which is not defined",
                rule.target
            ));
        }
    }

    errors
}

/// Validate an Enlil configuration. Returns a list of errors (empty = valid).
#[must_use]
pub fn validate_config(config: &EnlilConfig) -> Vec<String> {
    let mut errors = Vec::new();

    // Non-overlapping CPU sets (LOCKED PRINCIPLE 5 — isolation).
    errors.append(&mut validate_cpu_isolation(config));

    // USB peripheral routing (Phase 4.3): match specs parse, targets exist.
    errors.append(&mut validate_usb_routing(config));

    // Check memory
    let total_guest_memory: u64 = config.guest.values().map(|g| g.memory_mb).sum();
    let total_with_hypervisor = total_guest_memory + config.hypervisor.reserved_memory_mb;
    // Warn if total exceeds 256GB (sanity check)
    if total_with_hypervisor > 256 * 1024 {
        errors.push(format!(
            "Total memory {total_with_hypervisor}MB exceeds 256GB sanity limit"
        ));
    }
    // When the host total is pinned (non-zero; 0 means auto-detect), the guests
    // plus the hypervisor reservation must actually fit in it — otherwise the
    // configuration overcommits RAM the host does not have and a guest is starved
    // or fails to map its memory at start.
    let host_total = config.hypervisor.total_memory_mb;
    if host_total > 0 && total_with_hypervisor > host_total {
        errors.push(format!(
            "Total memory {total_with_hypervisor}MB (guests {total_guest_memory}MB + \
             hypervisor reserve {}MB) exceeds host total_memory_mb={host_total}MB",
            config.hypervisor.reserved_memory_mb
        ));
    }

    // Check each guest has at least some memory
    for (id, guest) in &config.guest {
        if guest.memory_mb == 0 {
            errors.push(format!("Guest '{id}': memory_mb cannot be 0"));
        }
        if guest.memory_mb < 64 {
            errors.push(format!(
                "Guest '{}': memory_mb={} is below 64MB minimum",
                id, guest.memory_mb
            ));
        }
    }

    // Check guest names are non-empty
    for (id, guest) in &config.guest {
        if guest.name.trim().is_empty() {
            errors.push(format!("Guest '{id}': name cannot be empty"));
        }
    }

    // The hypervisor log level must be one of the documented levels (matched
    // case-insensitively, like the `log` crate's own filter parser).
    let level = config.hypervisor.log_level.trim();
    if !VALID_LOG_LEVELS
        .iter()
        .any(|v| level.eq_ignore_ascii_case(v))
    {
        errors.push(format!(
            "Hypervisor log_level '{level}' is not one of trace, debug, info, warn, error"
        ));
    }

    // Each guest's serial output must name a supported sink: stdout, pty, null,
    // or file:<non-empty path> (see SerialPortConfig::output).
    for (id, guest) in &config.guest {
        let out = guest.serial.output.trim();
        let valid = matches!(out, "stdout" | "pty" | "null")
            || out
                .strip_prefix("file:")
                .is_some_and(|path| !path.trim().is_empty());
        if !valid {
            errors.push(format!(
                "Guest '{id}': serial output '{out}' must be stdout, pty, null, or file:<path>"
            ));
        }
    }

    // A writable disk image must not be shared. If the same path is mounted by
    // more than one disk entry (across guests or twice in one guest) and any of
    // those mounts is writable, the holders race each other and corrupt the
    // image. Read-only sharing is fine. (Exact-path comparison; symlink/relative
    // aliasing is out of scope for a static check that never touches the disk.)
    let mut mounts: HashMap<&Path, (usize, bool, Vec<&str>)> = HashMap::new();
    for (id, guest) in &config.guest {
        for disk in &guest.disks {
            let entry = mounts
                .entry(disk.path.as_path())
                .or_insert((0, false, Vec::new()));
            entry.0 += 1;
            entry.1 |= !disk.readonly;
            entry.2.push(id.as_str());
        }
    }
    for (path, (count, writable, ids)) in mounts {
        if count >= 2 && writable {
            let mut who = ids;
            who.sort_unstable();
            errors.push(format!(
                "Disk '{}' is mounted {count} times (by {}) with a writable handle; \
                 a shared writable image corrupts",
                path.display(),
                who.join(", ")
            ));
        }
    }

    // A management port of 0 means the console binds to an OS-chosen ephemeral
    // port, so a client has no fixed port to connect to — the console is
    // effectively unreachable. A real console needs a pinned port.
    if config.hypervisor.management_port == 0 {
        errors.push(
            "Hypervisor management_port is 0; the console needs a fixed port to bind to".into(),
        );
    }

    // A configuration with no guests has nothing to run. Flag it so an empty or
    // mistyped `[guest.*]` table is caught rather than silently starting an idle
    // hypervisor.
    if config.guest.is_empty() {
        errors.push("No guests configured; the hypervisor has nothing to run".into());
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use std::collections::HashMap;

    fn minimal_config() -> EnlilConfig {
        let mut guests = HashMap::new();
        guests.insert(
            "vm1".into(),
            GuestConfig {
                name: "Test VM 1".into(),
                cpus: vec![0, 1],
                memory_mb: 2048,
                kernel: None,
                initrd: None,
                cmdline: "console=ttyS0".into(),
                scheduling: SchedulingMode::Dedicated,
                disks: vec![],
                serial: SerialPortConfig::default(),
            },
        );
        EnlilConfig {
            hypervisor: HypervisorConfig::default(),
            guest: guests,
            usb: UsbConfig::default(),
        }
    }

    #[test]
    fn valid_config_passes() {
        let config = minimal_config();
        let errors = validate_config(&config);
        assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");
    }

    #[test]
    fn valid_usb_routing_passes() {
        let mut config = minimal_config();
        config.usb = UsbConfig {
            default_guest: Some("vm1".into()),
            routing: vec![
                UsbRoutingRule {
                    match_spec: "046d:c52b".into(),
                    target: "vm1".into(),
                    priority: 10,
                },
                UsbRoutingRule {
                    match_spec: "class:hid".into(),
                    target: "vm1".into(),
                    priority: 20,
                },
                UsbRoutingRule {
                    match_spec: "*".into(),
                    target: "vm1".into(),
                    priority: 1000,
                },
            ],
        };
        let errors = validate_config(&config);
        assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");
    }

    #[test]
    fn detects_usb_rule_with_unknown_target() {
        let mut config = minimal_config();
        config.usb.routing.push(UsbRoutingRule {
            match_spec: "046d:c52b".into(),
            target: "ghost".into(),
            priority: 10,
        });
        let errors = validate_config(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("ghost") && e.contains("not defined")),
            "unknown routing target should be flagged: {errors:?}"
        );
    }

    #[test]
    fn detects_malformed_usb_match_spec() {
        let mut config = minimal_config();
        config.usb.routing.push(UsbRoutingRule {
            match_spec: "not-a-match".into(),
            target: "vm1".into(),
            priority: 10,
        });
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.contains("not-a-match")),
            "malformed match spec should be flagged: {errors:?}"
        );
    }

    #[test]
    fn detects_unknown_usb_default_guest() {
        let mut config = minimal_config();
        config.usb.default_guest = Some("ghost".into());
        let errors = validate_config(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("default_guest") && e.contains("ghost")),
            "unknown default_guest should be flagged: {errors:?}"
        );
    }

    #[test]
    fn usb_routing_parses_from_toml() {
        let toml = r#"
            [hypervisor]

            [guest.vm1]
            name = "VM 1"
            cpus = [0]
            memory_mb = 512

            [usb]
            default_guest = "vm1"

            [[usb.routing]]
            match = "046d:*"
            target = "vm1"
            priority = 5

            [[usb.routing]]
            match = "port:1-1"
            target = "vm1"
        "#;
        let config: EnlilConfig = toml::from_str(toml).expect("parse");
        assert_eq!(config.usb.default_guest.as_deref(), Some("vm1"));
        assert_eq!(config.usb.routing.len(), 2);
        assert_eq!(config.usb.routing[0].priority, 5);
        // The second rule takes the default priority.
        assert_eq!(config.usb.routing[1].priority, 1000);
        assert_eq!(
            config.usb.routing[0].parsed_match(),
            Ok(UsbMatchKind::VendorOnly { vendor_id: 0x046d })
        );
        assert_eq!(
            config.usb.routing[1].parsed_match(),
            Ok(UsbMatchKind::PortPath("1-1".into()))
        );
        assert!(validate_config(&config).is_empty());
    }

    #[test]
    fn parse_usb_match_covers_every_form() {
        assert_eq!(parse_usb_match("*"), Ok(UsbMatchKind::Any));
        assert_eq!(
            parse_usb_match("046d:c52b"),
            Ok(UsbMatchKind::VidPid {
                vendor_id: 0x046d,
                product_id: 0xc52b
            })
        );
        assert_eq!(
            parse_usb_match("046d:*"),
            Ok(UsbMatchKind::VendorOnly { vendor_id: 0x046d })
        );
        assert_eq!(
            parse_usb_match("port:2-3.1"),
            Ok(UsbMatchKind::PortPath("2-3.1".into()))
        );
        assert_eq!(
            parse_usb_match("serial:ABC123"),
            Ok(UsbMatchKind::Serial("ABC123".into()))
        );
        assert_eq!(
            parse_usb_match("class:hid"),
            Ok(UsbMatchKind::DeviceClass("hid".into()))
        );
        assert!(parse_usb_match("garbage").is_err());
        assert!(parse_usb_match("zzzz:0001").is_err());
        assert!(parse_usb_match("port:").is_err());
    }

    #[test]
    fn detects_cpu_overlap() {
        let mut config = minimal_config();
        config.guest.insert(
            "vm2".into(),
            GuestConfig {
                name: "Test VM 2".into(),
                cpus: vec![1, 2], // CPU 1 overlaps with vm1
                memory_mb: 2048,
                kernel: None,
                initrd: None,
                cmdline: "console=ttyS0".into(),
                scheduling: SchedulingMode::Dedicated,
                disks: vec![],
                serial: SerialPortConfig::default(),
            },
        );
        let errors = validate_config(&config);
        assert!(errors.iter().any(|e| e.contains("CPU 1 already assigned")));
    }

    #[test]
    fn detects_timeslice_guest_intruding_on_a_dedicated_core() {
        // vm1 (from minimal_config) holds CPUs 0,1 as Dedicated. A Timeslice
        // guest that also lists CPU 1 would steal cycles the dedicated guest is
        // promised — the isolation the Dedicated mode guarantees.
        let mut config = minimal_config();
        config.guest.insert(
            "vm2".into(),
            GuestConfig {
                name: "Test VM 2".into(),
                cpus: vec![1, 2], // CPU 1 is dedicated to vm1
                memory_mb: 2048,
                kernel: None,
                initrd: None,
                cmdline: "console=ttyS0".into(),
                scheduling: SchedulingMode::Timeslice,
                disks: vec![],
                serial: SerialPortConfig::default(),
            },
        );
        let errors = validate_config(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("CPU 1 is dedicated to guest 'vm1'")
                    && e.contains("cannot be time-sliced")),
            "expected a dedicated-core intrusion error, got: {errors:?}"
        );
    }

    #[test]
    fn timeslice_guests_may_share_cores() {
        // Two time-sliced guests sharing a core pool is exactly what Timeslice
        // scheduling is for, so overlapping their CPU sets must NOT be flagged.
        let mut config = minimal_config();
        config.guest.get_mut("vm1").unwrap().scheduling = SchedulingMode::Timeslice;
        config.guest.insert(
            "vm2".into(),
            GuestConfig {
                name: "Test VM 2".into(),
                cpus: vec![1, 2], // overlaps vm1's CPU 1, but both are Timeslice
                memory_mb: 2048,
                kernel: None,
                initrd: None,
                cmdline: "console=ttyS0".into(),
                scheduling: SchedulingMode::Timeslice,
                disks: vec![],
                serial: SerialPortConfig::default(),
            },
        );
        let errors = validate_config(&config);
        assert!(
            !errors.iter().any(|e| e.contains("CPU")),
            "time-sliced guests may share cores, got: {errors:?}"
        );
    }

    #[test]
    fn detects_zero_management_port() {
        let mut config = minimal_config();
        config.hypervisor.management_port = 0;
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.contains("management_port is 0")),
            "expected a zero-port error, got: {errors:?}"
        );
    }

    #[test]
    fn detects_no_guests() {
        let mut config = minimal_config();
        config.guest.clear();
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.contains("No guests configured")),
            "expected a no-guests error, got: {errors:?}"
        );
    }

    #[test]
    fn detects_zero_memory() {
        let mut config = minimal_config();
        config.guest.get_mut("vm1").unwrap().memory_mb = 0;
        let errors = validate_config(&config);
        assert!(errors.iter().any(|e| e.contains("cannot be 0")));
    }

    #[test]
    fn detects_memory_overcommit_against_pinned_host_total() {
        let mut config = minimal_config();
        // vm1 needs 2048MB; with the 512MB default reserve that is 2560MB. Pin the
        // host total below that so the guests no longer fit.
        config.hypervisor.total_memory_mb = 2048;
        let errors = validate_config(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("exceeds host total_memory_mb")),
            "expected an overcommit error, got: {errors:?}"
        );
    }

    #[test]
    fn host_total_zero_means_auto_detect_and_does_not_overcommit() {
        let mut config = minimal_config();
        // 0 is the auto-detect sentinel: no overcommit check should fire.
        config.hypervisor.total_memory_mb = 0;
        let errors = validate_config(&config);
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("exceeds host total_memory_mb")),
            "auto-detect host total must not trigger an overcommit error: {errors:?}"
        );
    }

    #[test]
    fn guests_that_fit_the_pinned_host_total_pass() {
        let mut config = minimal_config();
        // 2048 (guest) + 512 (reserve) = 2560; a 4096MB host has room.
        config.hypervisor.total_memory_mb = 4096;
        let errors = validate_config(&config);
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
    }

    #[test]
    fn detects_invalid_log_level() {
        let mut config = minimal_config();
        config.hypervisor.log_level = "verbose".into();
        let errors = validate_config(&config);
        assert!(errors.iter().any(|e| e.contains("log_level")), "{errors:?}");
    }

    #[test]
    fn accepts_log_levels_case_insensitively() {
        let mut config = minimal_config();
        config.hypervisor.log_level = "WARN".into();
        let errors = validate_config(&config);
        assert!(
            !errors.iter().any(|e| e.contains("log_level")),
            "{errors:?}"
        );
    }

    #[test]
    fn detects_invalid_serial_output() {
        let mut config = minimal_config();
        config.guest.get_mut("vm1").unwrap().serial.output = "serialport".into();
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.contains("serial output")),
            "{errors:?}"
        );
    }

    #[test]
    fn detects_empty_file_serial_output() {
        let mut config = minimal_config();
        config.guest.get_mut("vm1").unwrap().serial.output = "file:".into();
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.contains("serial output")),
            "{errors:?}"
        );
    }

    #[test]
    fn accepts_file_serial_output_with_a_path() {
        let mut config = minimal_config();
        config.guest.get_mut("vm1").unwrap().serial.output = "file:/var/log/vm1.log".into();
        let errors = validate_config(&config);
        assert!(
            !errors.iter().any(|e| e.contains("serial output")),
            "{errors:?}"
        );
    }

    fn guest_with_disks(name: &str, cpus: Vec<u32>, disks: Vec<DiskConfig>) -> GuestConfig {
        GuestConfig {
            name: name.into(),
            cpus,
            memory_mb: 2048,
            kernel: None,
            initrd: None,
            cmdline: "console=ttyS0".into(),
            scheduling: SchedulingMode::Dedicated,
            disks,
            serial: SerialPortConfig::default(),
        }
    }

    #[test]
    fn detects_a_writable_disk_shared_across_guests() {
        let mut config = minimal_config();
        let shared = DiskConfig {
            path: "/images/shared.qcow2".into(),
            readonly: false,
        };
        config.guest.get_mut("vm1").unwrap().disks = vec![shared.clone()];
        config.guest.insert(
            "vm2".into(),
            guest_with_disks("VM2", vec![2, 3], vec![shared]),
        );
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.contains("writable handle")),
            "{errors:?}"
        );
    }

    #[test]
    fn read_only_disk_sharing_is_allowed() {
        let mut config = minimal_config();
        let shared = DiskConfig {
            path: "/images/golden.qcow2".into(),
            readonly: true,
        };
        config.guest.get_mut("vm1").unwrap().disks = vec![shared.clone()];
        config.guest.insert(
            "vm2".into(),
            guest_with_disks("VM2", vec![2, 3], vec![shared]),
        );
        let errors = validate_config(&config);
        assert!(
            !errors.iter().any(|e| e.contains("writable handle")),
            "read-only sharing must be permitted: {errors:?}"
        );
    }

    #[test]
    fn detects_the_same_writable_disk_mounted_twice_in_one_guest() {
        let mut config = minimal_config();
        config.guest.get_mut("vm1").unwrap().disks = vec![
            DiskConfig {
                path: "/images/d.raw".into(),
                readonly: false,
            },
            DiskConfig {
                path: "/images/d.raw".into(),
                readonly: false,
            },
        ];
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.contains("writable handle")),
            "{errors:?}"
        );
    }
}
