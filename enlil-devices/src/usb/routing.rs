//! USB routing policy engine.
//!
//! Maps physical USB devices to guest VMs based on configurable rules.
//! Supports routing by VID:PID, serial number, physical port path,
//! and device class, with a configurable default policy.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::types::{GuestId, UsbDeviceClass, UsbDeviceId};

// ---------------------------------------------------------------------------
// Routing rules
// ---------------------------------------------------------------------------

/// A single routing rule that matches USB devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    /// Unique rule identifier (for ordering and management).
    pub id: u32,
    /// Priority (lower = higher priority). Rules are evaluated in order.
    pub priority: u32,
    /// The match criteria.
    pub matcher: DeviceMatcher,
    /// Target guest to route matched devices to.
    pub target: GuestId,
    /// Whether this rule is currently active.
    pub enabled: bool,
}

/// Criteria for matching a USB device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeviceMatcher {
    /// Match by vendor and product ID.
    VidPid { vendor_id: u16, product_id: u16 },
    /// Match by vendor ID only (any product from this vendor).
    VendorOnly { vendor_id: u16 },
    /// Match by physical port path (e.g., "1-1", "2-3.1").
    PortPath(String),
    /// Match by serial number.
    Serial(String),
    /// Match by device class.
    DeviceClass(UsbDeviceClass),
    /// Match all devices (used for default routing).
    Any,
}

impl DeviceMatcher {
    /// Test whether a device matches this criteria.
    #[must_use]
    pub fn matches(&self, device: &UsbDeviceId) -> bool {
        match self {
            Self::VidPid {
                vendor_id,
                product_id,
            } => device.vendor_id == *vendor_id && device.product_id == *product_id,
            Self::VendorOnly { vendor_id } => device.vendor_id == *vendor_id,
            Self::PortPath(path) => device.port_path.as_deref() == Some(path.as_str()),
            Self::Serial(serial) => device.serial.as_deref() == Some(serial.as_str()),
            Self::DeviceClass(class) => device.class == *class,
            Self::Any => true,
        }
    }
}

impl fmt::Display for DeviceMatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VidPid {
                vendor_id,
                product_id,
            } => {
                write!(f, "{vendor_id:04x}:{product_id:04x}")
            }
            Self::VendorOnly { vendor_id } => write!(f, "{vendor_id:04x}:*"),
            Self::PortPath(path) => write!(f, "port:{path}"),
            Self::Serial(serial) => write!(f, "serial:{serial}"),
            Self::DeviceClass(class) => write!(f, "class:{class:?}"),
            Self::Any => write!(f, "*"),
        }
    }
}

// ---------------------------------------------------------------------------
// Routing decision
// ---------------------------------------------------------------------------

/// Result of a routing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingDecision {
    /// Route the device to a specific guest.
    RouteToGuest(GuestId),
    /// Device is not assigned to any guest (remains with hypervisor).
    Unassigned,
}

// ---------------------------------------------------------------------------
// Routing table
// ---------------------------------------------------------------------------

/// The routing policy engine.
///
/// Maintains an ordered list of routing rules and evaluates them against
/// incoming USB devices to determine guest assignment.
pub struct RoutingTable {
    /// Rules sorted by priority (ascending — lower number = higher priority).
    rules: Vec<RoutingRule>,
    /// Default guest for devices that match no rules (`None` = unassigned).
    default_guest: Option<GuestId>,
    /// Next rule ID.
    next_id: u32,
}

impl RoutingTable {
    /// Create a new empty routing table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rules: Vec::new(),
            default_guest: None,
            next_id: 1,
        }
    }

    /// Set the default guest for unmatched devices.
    pub fn set_default_guest(&mut self, guest: Option<GuestId>) {
        self.default_guest = guest;
    }

    /// Get the current default guest.
    #[must_use]
    pub const fn default_guest(&self) -> Option<&GuestId> {
        self.default_guest.as_ref()
    }

    /// Add a routing rule. Returns the assigned rule ID.
    pub fn add_rule(&mut self, priority: u32, matcher: DeviceMatcher, target: GuestId) -> u32 {
        let id = self.next_id;
        self.next_id += 1;

        let rule = RoutingRule {
            id,
            priority,
            matcher,
            target,
            enabled: true,
        };

        // Insert maintaining sorted order by priority.
        let pos = self.rules.partition_point(|r| r.priority < priority);
        self.rules.insert(pos, rule);
        id
    }

    /// Remove a rule by ID. Returns the removed rule, if found.
    pub fn remove_rule(&mut self, id: u32) -> Option<RoutingRule> {
        if let Some(pos) = self.rules.iter().position(|r| r.id == id) {
            Some(self.rules.remove(pos))
        } else {
            None
        }
    }

    /// Enable or disable a rule by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the rule ID is not found.
    pub fn set_rule_enabled(&mut self, id: u32, enabled: bool) -> Result<(), RoutingError> {
        let rule = self
            .rules
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or(RoutingError::RuleNotFound(id))?;
        rule.enabled = enabled;
        Ok(())
    }

    /// Get all rules (read-only).
    #[must_use]
    pub fn rules(&self) -> &[RoutingRule] {
        &self.rules
    }

    /// Determine which guest a device should be routed to.
    #[must_use]
    pub fn route(&self, device: &UsbDeviceId) -> RoutingDecision {
        for rule in &self.rules {
            if rule.enabled && rule.matcher.matches(device) {
                return RoutingDecision::RouteToGuest(rule.target.clone());
            }
        }

        match &self.default_guest {
            Some(guest) => RoutingDecision::RouteToGuest(guest.clone()),
            None => RoutingDecision::Unassigned,
        }
    }

    /// Return the number of active (enabled) rules.
    #[must_use]
    pub fn active_rule_count(&self) -> usize {
        self.rules.iter().filter(|r| r.enabled).count()
    }

    /// Return the total number of rules.
    #[must_use]
    pub const fn total_rule_count(&self) -> usize {
        self.rules.len()
    }
}

impl Default for RoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Live routing state — tracks current device→guest assignments
// ---------------------------------------------------------------------------

/// Tracks the current routing state of all known devices.
/// Thread-safe for use from the monitor and management console.
pub struct RoutingState {
    inner: Arc<Mutex<RoutingStateInner>>,
}

struct RoutingStateInner {
    /// Current device assignments: device bus address → guest.
    assignments: HashMap<u8, GuestId>,
    /// Routing table.
    table: RoutingTable,
}

impl RoutingState {
    /// Create new routing state with the given table.
    #[must_use]
    pub fn new(table: RoutingTable) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RoutingStateInner {
                assignments: HashMap::new(),
                table,
            })),
        }
    }

    /// Get a clone handle for thread-safe sharing.
    #[must_use]
    pub fn clone_handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Assign a device to a guest based on current routing rules.
    /// Returns the routing decision.
    #[must_use]
    pub fn assign_device(&self, bus_addr: u8, device: &UsbDeviceId) -> RoutingDecision {
        let mut inner = self.inner.lock().expect("routing state poisoned");
        let decision = inner.table.route(device);
        if let RoutingDecision::RouteToGuest(ref guest) = decision {
            inner.assignments.insert(bus_addr, guest.clone());
        }
        decision
    }

    /// Manually re-route a device to a different guest.
    ///
    /// # Errors
    ///
    /// Returns an error if the device is not currently assigned.
    pub fn reassign_device(
        &self,
        bus_addr: u8,
        new_guest: GuestId,
    ) -> Result<GuestId, RoutingError> {
        let mut inner = self.inner.lock().expect("routing state poisoned");
        let old = inner
            .assignments
            .insert(bus_addr, new_guest)
            .ok_or(RoutingError::DeviceNotAssigned(bus_addr))?;
        Ok(old)
    }

    /// Remove a device assignment (e.g., on disconnect).
    #[must_use]
    pub fn unassign_device(&self, bus_addr: u8) -> Option<GuestId> {
        let mut inner = self.inner.lock().expect("routing state poisoned");
        inner.assignments.remove(&bus_addr)
    }

    /// Get the current guest assignment for a device.
    #[must_use]
    pub fn get_assignment(&self, bus_addr: u8) -> Option<GuestId> {
        let inner = self.inner.lock().expect("routing state poisoned");
        inner.assignments.get(&bus_addr).cloned()
    }

    /// Get all current assignments.
    #[must_use]
    pub fn all_assignments(&self) -> HashMap<u8, GuestId> {
        let inner = self.inner.lock().expect("routing state poisoned");
        inner.assignments.clone()
    }

    /// Access the routing table for rule management.
    pub fn with_table<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut RoutingTable) -> R,
    {
        let mut inner = self.inner.lock().expect("routing state poisoned");
        f(&mut inner.table)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the routing engine.
#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    /// The specified rule ID was not found.
    #[error("routing rule not found: {0}")]
    RuleNotFound(u32),

    /// The device is not currently assigned to any guest.
    #[error("device at bus address {0} is not assigned")]
    DeviceNotAssigned(u8),
}

// ---------------------------------------------------------------------------
// Config parsing helpers
// ---------------------------------------------------------------------------

/// Parse a VID:PID routing rule from a config string like "046d:c077".
///
/// # Errors
///
/// Returns `None` if the string is not a valid VID:PID pair.
#[must_use]
pub fn parse_vid_pid(s: &str) -> Option<(u16, u16)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let vid = u16::from_str_radix(parts[0], 16).ok()?;
    let pid = u16::from_str_radix(parts[1], 16).ok()?;
    Some((vid, pid))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::types::UsbSpeed;

    fn make_device(vid: u16, pid: u16) -> UsbDeviceId {
        UsbDeviceId {
            vendor_id: vid,
            product_id: pid,
            class: UsbDeviceClass::Hid,
            speed: UsbSpeed::High,
            serial: None,
            manufacturer: None,
            product: None,
            port_path: None,
        }
    }

    fn make_device_with_port(vid: u16, pid: u16, port: &str) -> UsbDeviceId {
        UsbDeviceId {
            port_path: Some(port.into()),
            ..make_device(vid, pid)
        }
    }

    fn make_device_with_serial(vid: u16, pid: u16, serial: &str) -> UsbDeviceId {
        UsbDeviceId {
            serial: Some(serial.into()),
            ..make_device(vid, pid)
        }
    }

    #[test]
    fn vid_pid_matcher() {
        let matcher = DeviceMatcher::VidPid {
            vendor_id: 0x046d,
            product_id: 0xc077,
        };
        let mouse = make_device(0x046d, 0xc077);
        let keyboard = make_device(0x046d, 0xc534);
        assert!(matcher.matches(&mouse));
        assert!(!matcher.matches(&keyboard));
    }

    #[test]
    fn vendor_only_matcher() {
        let matcher = DeviceMatcher::VendorOnly { vendor_id: 0x046d };
        let mouse = make_device(0x046d, 0xc077);
        let keyboard = make_device(0x046d, 0xc534);
        let other = make_device(0x1234, 0x5678);
        assert!(matcher.matches(&mouse));
        assert!(matcher.matches(&keyboard));
        assert!(!matcher.matches(&other));
    }

    #[test]
    fn port_path_matcher() {
        let matcher = DeviceMatcher::PortPath("1-1".into());
        let dev_on_port = make_device_with_port(0x046d, 0xc077, "1-1");
        let dev_other_port = make_device_with_port(0x046d, 0xc077, "2-1");
        let dev_no_port = make_device(0x046d, 0xc077);
        assert!(matcher.matches(&dev_on_port));
        assert!(!matcher.matches(&dev_other_port));
        assert!(!matcher.matches(&dev_no_port));
    }

    #[test]
    fn serial_matcher() {
        let matcher = DeviceMatcher::Serial("ABC123".into());
        let dev_match = make_device_with_serial(0x046d, 0xc077, "ABC123");
        let dev_no = make_device_with_serial(0x046d, 0xc077, "XYZ789");
        let dev_none = make_device(0x046d, 0xc077);
        assert!(matcher.matches(&dev_match));
        assert!(!matcher.matches(&dev_no));
        assert!(!matcher.matches(&dev_none));
    }

    #[test]
    fn class_matcher() {
        let matcher = DeviceMatcher::DeviceClass(UsbDeviceClass::Hid);
        let hid = make_device(0x046d, 0xc077);
        let storage = UsbDeviceId {
            class: UsbDeviceClass::MassStorage,
            ..make_device(0x1234, 0x5678)
        };
        assert!(matcher.matches(&hid));
        assert!(!matcher.matches(&storage));
    }

    #[test]
    fn any_matcher_matches_everything() {
        let matcher = DeviceMatcher::Any;
        assert!(matcher.matches(&make_device(0, 0)));
        assert!(matcher.matches(&make_device(0xFFFF, 0xFFFF)));
    }

    #[test]
    fn routing_table_priority_ordering() {
        let mut table = RoutingTable::new();
        table.add_rule(10, DeviceMatcher::Any, "linux2".into());
        table.add_rule(
            1,
            DeviceMatcher::VidPid {
                vendor_id: 0x046d,
                product_id: 0xc077,
            },
            "linux1".into(),
        );

        // The VID:PID rule has higher priority (lower number).
        let mouse = make_device(0x046d, 0xc077);
        assert_eq!(
            table.route(&mouse),
            RoutingDecision::RouteToGuest("linux1".into())
        );

        // Other devices fall through to the Any rule.
        let other = make_device(0x1234, 0x5678);
        assert_eq!(
            table.route(&other),
            RoutingDecision::RouteToGuest("linux2".into())
        );
    }

    #[test]
    fn routing_table_default_guest() {
        let mut table = RoutingTable::new();
        table.set_default_guest(Some("linux1".into()));

        let dev = make_device(0x1234, 0x5678);
        assert_eq!(
            table.route(&dev),
            RoutingDecision::RouteToGuest("linux1".into())
        );
    }

    #[test]
    fn routing_table_no_match_no_default() {
        let table = RoutingTable::new();
        let dev = make_device(0x1234, 0x5678);
        assert_eq!(table.route(&dev), RoutingDecision::Unassigned);
    }

    #[test]
    fn routing_table_disabled_rule_skipped() {
        let mut table = RoutingTable::new();
        let rule_id = table.add_rule(1, DeviceMatcher::Any, "linux1".into());
        table.set_rule_enabled(rule_id, false).unwrap();

        let dev = make_device(0x1234, 0x5678);
        assert_eq!(table.route(&dev), RoutingDecision::Unassigned);
        assert_eq!(table.active_rule_count(), 0);
        assert_eq!(table.total_rule_count(), 1);
    }

    #[test]
    fn routing_table_remove_rule() {
        let mut table = RoutingTable::new();
        let id = table.add_rule(1, DeviceMatcher::Any, "linux1".into());
        assert_eq!(table.total_rule_count(), 1);
        let removed = table.remove_rule(id);
        assert!(removed.is_some());
        assert_eq!(table.total_rule_count(), 0);
    }

    #[test]
    fn routing_state_assign_and_reassign() {
        let mut table = RoutingTable::new();
        table.add_rule(1, DeviceMatcher::Any, "linux1".into());
        let state = RoutingState::new(table);

        let dev = make_device(0x046d, 0xc077);
        let decision = state.assign_device(1, &dev);
        assert_eq!(decision, RoutingDecision::RouteToGuest("linux1".into()));
        assert_eq!(state.get_assignment(1), Some("linux1".into()));

        let old = state.reassign_device(1, "linux2".into()).unwrap();
        assert_eq!(old, "linux1");
        assert_eq!(state.get_assignment(1), Some("linux2".into()));
    }

    #[test]
    fn routing_state_unassign() {
        let mut table = RoutingTable::new();
        table.add_rule(1, DeviceMatcher::Any, "linux1".into());
        let state = RoutingState::new(table);

        let dev = make_device(0x046d, 0xc077);
        state.assign_device(1, &dev);
        let removed = state.unassign_device(1);
        assert_eq!(removed, Some("linux1".into()));
        assert_eq!(state.get_assignment(1), None);
    }

    #[test]
    fn routing_state_reassign_unassigned_fails() {
        let state = RoutingState::new(RoutingTable::new());
        let result = state.reassign_device(42, "linux1".into());
        assert!(result.is_err());
    }

    #[test]
    fn routing_state_thread_safe_handle() {
        let mut table = RoutingTable::new();
        table.add_rule(1, DeviceMatcher::Any, "linux1".into());
        let state = RoutingState::new(table);
        let handle = state.clone_handle();

        let dev = make_device(0x046d, 0xc077);
        state.assign_device(1, &dev);
        assert_eq!(handle.get_assignment(1), Some("linux1".into()));
    }

    #[test]
    fn parse_vid_pid_valid() {
        assert_eq!(parse_vid_pid("046d:c077"), Some((0x046d, 0xc077)));
        assert_eq!(parse_vid_pid("0000:0000"), Some((0, 0)));
        assert_eq!(parse_vid_pid("ffff:ffff"), Some((0xFFFF, 0xFFFF)));
    }

    #[test]
    fn parse_vid_pid_invalid() {
        assert_eq!(parse_vid_pid("not-valid"), None);
        assert_eq!(parse_vid_pid("046d"), None);
        assert_eq!(parse_vid_pid("046d:c077:extra"), None);
        assert_eq!(parse_vid_pid("zzzz:c077"), None);
    }

    #[test]
    fn matcher_display() {
        assert_eq!(
            DeviceMatcher::VidPid {
                vendor_id: 0x046d,
                product_id: 0xc077
            }
            .to_string(),
            "046d:c077"
        );
        assert_eq!(
            DeviceMatcher::VendorOnly { vendor_id: 0x046d }.to_string(),
            "046d:*"
        );
        assert_eq!(
            DeviceMatcher::PortPath("1-1".into()).to_string(),
            "port:1-1"
        );
        assert_eq!(DeviceMatcher::Any.to_string(), "*");
    }

    #[test]
    fn routing_table_with_table_access() {
        let state = RoutingState::new(RoutingTable::new());
        state.with_table(|table| {
            table.add_rule(1, DeviceMatcher::Any, "linux1".into());
        });
        let dev = make_device(0x1234, 0x5678);
        let decision = state.assign_device(1, &dev);
        assert_eq!(decision, RoutingDecision::RouteToGuest("linux1".into()));
    }
}
