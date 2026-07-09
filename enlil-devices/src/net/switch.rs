//! Virtual network switch for inter-guest communication.
//!
//! The switch learns MAC addresses from incoming frames and forwards
//! unicast frames to the correct port. Broadcast/multicast frames
//! are flooded to all ports except the source.
//!
//! # Architecture
//!
//! Each guest NIC connects to the switch via a `PortId`. When a frame
//! arrives on a port, the switch inspects the source MAC to update its
//! forwarding table, then uses the destination MAC to decide where to
//! send the frame.

use crate::truncate::u32_of;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::config::MacAddress;

/// Identifies a port on the virtual switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortId(pub u32);

impl std::fmt::Display for PortId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "port-{}", self.0)
    }
}

/// Entry in the MAC forwarding table.
#[derive(Debug, Clone)]
struct FdbEntry {
    port: PortId,
    last_seen: Instant,
}

/// A virtual Ethernet switch that forwards frames between ports.
///
/// Supports:
/// - MAC address learning
/// - Unicast forwarding
/// - Broadcast/multicast flooding
/// - Aging of MAC table entries
pub struct VirtualSwitch {
    /// MAC forwarding database.
    fdb: HashMap<MacAddress, FdbEntry>,
    /// Per-port output queues: frames waiting to be delivered.
    port_queues: HashMap<PortId, VecDeque<Vec<u8>>>,
    /// Set of registered port IDs.
    ports: Vec<PortId>,
    /// How long a MAC entry stays valid before aging out.
    aging_time: Duration,
    /// Maximum entries in the forwarding database.
    max_fdb_entries: usize,
    /// Statistics.
    stats: SwitchStats,
}

/// Switch traffic statistics.
#[derive(Debug, Clone, Default)]
pub struct SwitchStats {
    /// Total frames received by the switch.
    pub received: u64,
    /// Total frames forwarded (unicast hit).
    pub forwarded: u64,
    /// Total frames flooded (broadcast/unknown unicast).
    pub flooded: u64,
    /// Total frames dropped (e.g., to the source port).
    pub dropped: u64,
}

impl VirtualSwitch {
    /// Create a new virtual switch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fdb: HashMap::new(),
            port_queues: HashMap::new(),
            ports: Vec::new(),
            aging_time: Duration::from_mins(5),
            max_fdb_entries: 4096,
            stats: SwitchStats::default(),
        }
    }

    /// Create a switch with a custom aging time.
    #[must_use]
    pub fn with_aging_time(aging_time: Duration) -> Self {
        Self {
            aging_time,
            ..Self::new()
        }
    }

    /// Register a new port on the switch. Returns the port ID.
    pub fn add_port(&mut self) -> PortId {
        let id = PortId(u32_of(self.ports.len()));
        self.ports.push(id);
        self.port_queues.insert(id, VecDeque::new());
        id
    }

    /// Remove a port from the switch.
    pub fn remove_port(&mut self, port: PortId) {
        self.ports.retain(|p| *p != port);
        self.port_queues.remove(&port);
        // Remove all FDB entries pointing to this port.
        self.fdb.retain(|_, entry| entry.port != port);
    }

    /// Number of registered ports.
    #[must_use]
    pub const fn port_count(&self) -> usize {
        self.ports.len()
    }

    /// Number of entries in the forwarding database.
    #[must_use]
    pub fn fdb_size(&self) -> usize {
        self.fdb.len()
    }

    /// Get a reference to the switch statistics.
    #[must_use]
    pub const fn stats(&self) -> &SwitchStats {
        &self.stats
    }

    /// Process an incoming Ethernet frame from the given source port.
    ///
    /// The frame must be a raw Ethernet frame (destination MAC at offset 0,
    /// source MAC at offset 6). The switch will learn the source MAC and
    /// enqueue the frame for delivery to the appropriate port(s).
    pub fn process_frame(&mut self, src_port: PortId, frame: &[u8]) {
        // Minimum Ethernet frame: 14-byte header.
        if frame.len() < 14 {
            self.stats.dropped += 1;
            return;
        }

        self.stats.received += 1;

        let dst_mac = MacAddress([frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]]);
        let src_mac = MacAddress([frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]]);

        // Learn the source MAC.
        self.learn(src_mac, src_port);

        // Age out old entries periodically (cheap check).
        if self.stats.received.is_multiple_of(1000) {
            self.age_entries();
        }

        // Forward or flood.
        if dst_mac.is_broadcast() || dst_mac.is_multicast() {
            self.flood(src_port, frame);
        } else if let Some(dst_port) = self.lookup(dst_mac) {
            if dst_port == src_port {
                // Don't send back to source.
                self.stats.dropped += 1;
            } else {
                self.enqueue(dst_port, frame);
                self.stats.forwarded += 1;
            }
        } else {
            // Unknown unicast — flood.
            self.flood(src_port, frame);
        }
    }

    /// Dequeue the next frame for the given port.
    ///
    /// Returns `None` if there are no pending frames.
    pub fn dequeue(&mut self, port: PortId) -> Option<Vec<u8>> {
        self.port_queues
            .get_mut(&port)
            .and_then(VecDeque::pop_front)
    }

    /// Check if a port has pending frames.
    #[must_use]
    pub fn has_pending(&self, port: PortId) -> bool {
        self.port_queues.get(&port).is_some_and(|q| !q.is_empty())
    }

    /// Number of pending frames for a port.
    #[must_use]
    pub fn pending_count(&self, port: PortId) -> usize {
        self.port_queues
            .get(&port)
            .map_or(0, std::collections::VecDeque::len)
    }

    /// Manually flush all FDB entries.
    pub fn flush_fdb(&mut self) {
        self.fdb.clear();
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn learn(&mut self, mac: MacAddress, port: PortId) {
        if mac.is_broadcast() || mac.is_multicast() {
            return;
        }

        // Enforce FDB size limit.
        if self.fdb.len() >= self.max_fdb_entries && !self.fdb.contains_key(&mac) {
            self.age_entries();
            if self.fdb.len() >= self.max_fdb_entries {
                return; // Still full after aging, drop.
            }
        }

        self.fdb.insert(
            mac,
            FdbEntry {
                port,
                last_seen: Instant::now(),
            },
        );
    }

    fn lookup(&self, mac: MacAddress) -> Option<PortId> {
        // Treat an entry aged past the aging time as a miss so a departed or
        // moved station is flooded to (re-learning its new port) rather than
        // silently unicast to its stale port until the next bulk age-out.
        self.fdb
            .get(&mac)
            .filter(|entry| entry.last_seen.elapsed() < self.aging_time)
            .map(|entry| entry.port)
    }

    fn flood(&mut self, src_port: PortId, frame: &[u8]) {
        let targets: Vec<PortId> = self
            .ports
            .iter()
            .copied()
            .filter(|p| *p != src_port)
            .collect();

        for port in targets {
            self.enqueue(port, frame);
        }
        self.stats.flooded += 1;
    }

    fn enqueue(&mut self, port: PortId, frame: &[u8]) {
        if let Some(queue) = self.port_queues.get_mut(&port) {
            queue.push_back(frame.to_vec());
        }
    }

    fn age_entries(&mut self) {
        let deadline = self.aging_time;
        self.fdb
            .retain(|_, entry| entry.last_seen.elapsed() < deadline);
    }
}

impl Default for VirtualSwitch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(dst: [u8; 6], src: [u8; 6], payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.extend_from_slice(&dst);
        frame.extend_from_slice(&src);
        frame.extend_from_slice(&[0x08, 0x00]); // EtherType: IPv4
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn add_and_remove_ports() {
        let mut sw = VirtualSwitch::new();
        let p0 = sw.add_port();
        let p1 = sw.add_port();
        assert_eq!(sw.port_count(), 2);

        sw.remove_port(p0);
        assert_eq!(sw.port_count(), 1);
        assert!(!sw.has_pending(p0));
        assert!(!sw.has_pending(p1));
    }

    #[test]
    fn broadcast_floods_all_ports() {
        let mut sw = VirtualSwitch::new();
        let p0 = sw.add_port();
        let p1 = sw.add_port();
        let p2 = sw.add_port();

        let frame = make_frame(
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            b"hello",
        );

        sw.process_frame(p0, &frame);

        // p1 and p2 should get the frame, but not p0.
        assert!(!sw.has_pending(p0));
        assert!(sw.has_pending(p1));
        assert!(sw.has_pending(p2));
    }

    #[test]
    fn unicast_learning_and_forwarding() {
        let mut sw = VirtualSwitch::new();
        let p0 = sw.add_port();
        let p1 = sw.add_port();
        let _p2 = sw.add_port();

        let mac_a = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let mac_b = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];

        // A sends to B (unknown) — flooded.
        let frame1 = make_frame(mac_b, mac_a, b"first");
        sw.process_frame(p0, &frame1);

        // B replies to A — should be forwarded only to p0.
        let frame2 = make_frame(mac_a, mac_b, b"reply");
        sw.process_frame(p1, &frame2);

        // p0 should have the reply.
        assert!(sw.has_pending(p0));
        let delivered = sw.dequeue(p0).unwrap();
        assert_eq!(&delivered[14..], b"reply");
    }

    #[test]
    fn short_frame_dropped() {
        let mut sw = VirtualSwitch::new();
        let p0 = sw.add_port();
        let _p1 = sw.add_port();

        sw.process_frame(p0, &[0u8; 5]);
        assert_eq!(sw.stats().dropped, 1);
        assert_eq!(sw.stats().received, 0);
    }

    #[test]
    fn a_stale_fdb_entry_floods_instead_of_forwarding_to_a_dead_port() {
        let mut sw = VirtualSwitch::with_aging_time(Duration::from_millis(10));
        let a = sw.add_port();
        let b = sw.add_port();
        let c = sw.add_port();

        let m = [0x02, 0x00, 0x00, 0x00, 0x00, 0x0A];
        let other = [0x02, 0x00, 0x00, 0x00, 0x00, 0x0B];

        // Learn M on port A (a broadcast frame from A with src = M).
        sw.process_frame(a, &make_frame([0xFF; 6], m, b"hello"));
        // Drain the flood the learning frame produced on B and C.
        while sw.dequeue(a).is_some() {}
        while sw.dequeue(b).is_some() {}
        while sw.dequeue(c).is_some() {}

        // Fresh entry: a unicast to M forwards only to A, not flooded to C.
        let fwd = sw.stats().forwarded;
        sw.process_frame(b, &make_frame(m, other, b"fresh"));
        assert_eq!(sw.stats().forwarded, fwd + 1, "fresh entry forwards");
        assert!(sw.has_pending(a));
        assert!(!sw.has_pending(c), "a fresh unicast is not flooded to C");
        while sw.dequeue(a).is_some() {}

        // Let the entry age out, then the same unicast must flood (reaching A
        // and C), not silently unicast to the stale port.
        std::thread::sleep(Duration::from_millis(25));
        let flooded = sw.stats().flooded;
        sw.process_frame(b, &make_frame(m, other, b"stale"));
        assert_eq!(sw.stats().flooded, flooded + 1, "a stale entry floods");
        assert!(
            sw.has_pending(a) && sw.has_pending(c),
            "the flood reaches both non-source ports"
        );
    }

    #[test]
    fn flush_fdb() {
        let mut sw = VirtualSwitch::new();
        let p0 = sw.add_port();
        let _p1 = sw.add_port();

        let frame = make_frame(
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            b"data",
        );
        sw.process_frame(p0, &frame);
        assert!(sw.fdb_size() > 0);

        sw.flush_fdb();
        assert_eq!(sw.fdb_size(), 0);
    }
}
