//! `VirtIO` network device emulation.
//!
//! Implements a VirtIO-net device with TX/RX virtqueues, a pluggable
//! network backend, and basic statistics.

use super::backend::NetBackend;
use super::config::{MacAddress, NetDeviceConfig};
use super::control::{self, NetControlState, RxFilterMode};
use super::features::NetFeatures;
use super::header::VirtioNetHeader;
use super::virtqueue::Virtqueue;
use crate::truncate::usize_of;

use std::collections::VecDeque;

/// The guest receive offloads that `VIRTIO_NET_CTRL_GUEST_OFFLOADS` can toggle.
/// A negotiated guest offload starts active and the control command may later
/// disable it; the active subset is tracked in [`NetControlState::active_offloads`].
const GUEST_OFFLOAD_MASK: u64 = NetFeatures::GUEST_CSUM
    | NetFeatures::GUEST_TSO4
    | NetFeatures::GUEST_TSO6
    | NetFeatures::GUEST_ECN
    | NetFeatures::GUEST_UFO;

/// Device status bits (`VirtIO` 1.2, Section 2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    Reset = 0,
    Acknowledge = 1,
    Driver = 2,
    DriverOk = 4,
    FeaturesOk = 8,
    Failed = 128,
}

/// The 16-bit one's-complement internet checksum (RFC 1071) over `data`.
/// Used to complete TX checksum-offload requests.
fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let (chunks, remainder) = data.as_chunks::<2>();
    for c in chunks {
        sum += u32::from(u16::from_be_bytes([c[0], c[1]]));
    }
    if let [last] = remainder {
        sum += u32::from(u16::from_be_bytes([*last, 0]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !u16::try_from(sum & 0xFFFF).unwrap_or(0)
}

/// `virtio_net_config.status` bit: the link is up.
pub const NET_S_LINK_UP: u16 = 1;
/// `virtio_net_config.status` bit: the device wants the guest to re-announce
/// its presence (gratuitous ARP) — set after a migration/link change.
pub const NET_S_ANNOUNCE: u16 = 2;

/// Network device statistics.
#[derive(Debug, Clone, Default)]
pub struct NetDeviceStats {
    pub tx_packets: u64,
    pub tx_bytes: u64,
    pub tx_errors: u64,
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub rx_drops: u64,
    /// Control-virtqueue commands acknowledged OK.
    pub ctrl_commands: u64,
    /// Control-virtqueue commands rejected (malformed or unsupported).
    pub ctrl_errors: u64,
    /// TX frames whose checksum the device completed (CSUM offload).
    pub tx_csum_offloads: u64,
}

/// A `VirtIO` network device.
///
/// Contains TX and RX virtqueues, a pluggable backend, MAC address,
/// feature negotiation, and packet statistics.
pub struct VirtioNetDevice {
    /// Device name.
    name: String,
    /// Device MAC address.
    mac: MacAddress,
    /// Negotiated features.
    features: NetFeatures,
    /// Whether mergeable RX buffers are enabled.
    merge_rxbuf: bool,
    /// Whether the virtual link is up (reported in the config-space status).
    link_up: bool,
    /// Link MTU reported in config space (`VIRTIO_NET_F_MTU`).
    mtu: u16,
    /// Device status.
    status: u8,
    /// TX virtqueue (guest → host).
    tx_queue: Virtqueue,
    /// RX virtqueue (host → guest).
    rx_queue: Virtqueue,
    /// Queue size.
    queue_size: u16,
    /// Network backend.
    backend: Box<dyn NetBackend>,
    /// Pending RX frames waiting for guest buffers.
    rx_pending: VecDeque<Vec<u8>>,
    /// Control-virtqueue state (RX mode, MAC/VLAN filters, multiqueue).
    control: NetControlState,
    /// Statistics.
    stats: NetDeviceStats,
}

impl VirtioNetDevice {
    /// Configured virtqueue size.
    #[must_use]
    pub const fn queue_size(&self) -> u16 {
        self.queue_size
    }

    /// Create a new `VirtIO` network device.
    #[must_use]
    pub fn new(config: &NetDeviceConfig, backend: Box<dyn NetBackend>) -> Self {
        let queue_size: u16 = 256; // default virtqueue size
        Self {
            name: config.name.clone(),
            mac: config.mac,
            features: NetFeatures::from_bits(NetFeatures::DEFAULT),
            merge_rxbuf: false,
            link_up: true,
            mtu: config.mtu,
            status: 0,
            tx_queue: Virtqueue::new(format!("{}-tx", config.name), queue_size),
            rx_queue: Virtqueue::new(format!("{}-rx", config.name), queue_size),
            queue_size,
            backend,
            rx_pending: VecDeque::new(),
            control: NetControlState {
                // Negotiated guest offloads start active (the control command
                // may disable them); DEFAULT offers GUEST_CSUM.
                active_offloads: NetFeatures::DEFAULT & GUEST_OFFLOAD_MASK,
                ..NetControlState::default()
            },
            stats: NetDeviceStats::default(),
        }
    }

    /// Get the device name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the MAC address.
    #[must_use]
    pub const fn mac(&self) -> MacAddress {
        self.mac
    }

    /// Get negotiated features.
    #[must_use]
    pub const fn features(&self) -> NetFeatures {
        self.features
    }

    /// Get device statistics.
    #[must_use]
    pub const fn stats(&self) -> &NetDeviceStats {
        &self.stats
    }

    /// Get device status.
    #[must_use]
    pub const fn status(&self) -> u8 {
        self.status
    }

    /// Activate the device with negotiated features.
    pub const fn activate(&mut self, features: NetFeatures) {
        self.features = features;
        self.merge_rxbuf = features.contains(NetFeatures::MRG_RXBUF);
        // The negotiated guest offloads start active.
        self.control.active_offloads = features.bits() & GUEST_OFFLOAD_MASK;
        self.status = DeviceStatus::DriverOk as u8;
        self.tx_queue.enable();
        self.rx_queue.enable();
    }

    /// Reset the device to initial state.
    pub fn reset(&mut self) {
        self.status = 0;
        self.features = NetFeatures::from_bits(NetFeatures::DEFAULT);
        self.merge_rxbuf = false;
        self.link_up = true;
        self.tx_queue.reset();
        self.rx_queue.reset();
        self.rx_pending.clear();
        self.control = NetControlState::default();
        self.stats = NetDeviceStats::default();
    }

    /// Read-only view of the control-virtqueue state (RX mode, MAC/VLAN
    /// filters, multiqueue) as last programmed by the guest.
    #[must_use]
    pub const fn control_state(&self) -> &NetControlState {
        &self.control
    }

    /// Set the virtual link state. Dropping the link clears `LINK_UP` in the
    /// config-space status the guest polls (when `VIRTIO_NET_F_STATUS` is on).
    pub const fn set_link_up(&mut self, up: bool) {
        self.link_up = up;
    }

    /// Whether the virtual link is currently up.
    #[must_use]
    pub const fn is_link_up(&self) -> bool {
        self.link_up
    }

    /// The link MTU reported to the guest.
    #[must_use]
    pub const fn mtu(&self) -> u16 {
        self.mtu
    }

    /// Ask the guest to re-announce itself (gratuitous ARP) — sets the
    /// `ANNOUNCE` status bit; the guest clears it via `VIRTIO_NET_CTRL_ANNOUNCE`.
    pub const fn request_announce(&mut self) {
        self.control.announce_needed = true;
    }

    /// The current `virtio_net_config.status` field (`LINK_UP` / `ANNOUNCE`).
    #[must_use]
    pub const fn config_status(&self) -> u16 {
        let mut s: u16 = 0;
        if self.link_up {
            s |= NET_S_LINK_UP;
        }
        if self.control.announce_needed {
            s |= NET_S_ANNOUNCE;
        }
        s
    }

    /// Serialize the `virtio_net_config` the guest reads: `mac[6]`, `status`,
    /// `max_virtqueue_pairs`, `mtu`. We offer `VIRTIO_NET_F_MTU`, so the `mtu`
    /// field carries the configured value (default 1500), not 0.
    fn config_as_bytes(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0..6].copy_from_slice(self.mac.as_bytes());
        b[6..8].copy_from_slice(&self.config_status().to_le_bytes());
        let pairs = self.control.vq_pairs.max(1);
        b[8..10].copy_from_slice(&pairs.to_le_bytes());
        b[10..12].copy_from_slice(&self.mtu.to_le_bytes());
        b
    }

    /// Read the device config space at `offset` for `size` bytes (1/2/4), as the
    /// virtio transport does on a guest config read. Out-of-range reads as 0.
    #[must_use]
    pub fn read_config(&self, offset: u64, size: u8) -> u64 {
        let bytes = self.config_as_bytes();
        let offset = usize_of(offset);
        let mut val = 0u64;
        for i in 0..usize::from(size) {
            if let Some(&byte) = bytes.get(offset + i) {
                val |= u64::from(byte) << (i * 8);
            }
        }
        val
    }

    /// Process one `VIRTIO_NET_CTRL` command from the control virtqueue.
    ///
    /// `buf` is the command buffer `{ class, command, command-specific data… }`;
    /// the returned byte is the ack the device writes back
    /// ([`VIRTIO_NET_OK`](control::VIRTIO_NET_OK) /
    /// [`VIRTIO_NET_ERR`](control::VIRTIO_NET_ERR)). A command is rejected if
    /// the control virtqueue was not negotiated, the buffer is too short, or the
    /// payload is malformed.
    pub fn process_control(&mut self, buf: &[u8]) -> u8 {
        if !self.features.contains(NetFeatures::CTRL_VQ) || buf.len() < 2 {
            self.stats.ctrl_errors += 1;
            return control::VIRTIO_NET_ERR;
        }
        let (class, command, data) = (buf[0], buf[1], &buf[2..]);
        let ack = match class {
            control::VIRTIO_NET_CTRL_RX => self.ctrl_rx(command, data),
            control::VIRTIO_NET_CTRL_MAC => self.ctrl_mac(command, data),
            control::VIRTIO_NET_CTRL_VLAN => self.ctrl_vlan(command, data),
            control::VIRTIO_NET_CTRL_ANNOUNCE => {
                if command == control::VIRTIO_NET_CTRL_ANNOUNCE_ACK {
                    self.control.announce_needed = false;
                    control::VIRTIO_NET_OK
                } else {
                    control::VIRTIO_NET_ERR
                }
            }
            control::VIRTIO_NET_CTRL_MQ => self.ctrl_mq(command, data),
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS => self.ctrl_guest_offloads(command, data),
            _ => control::VIRTIO_NET_ERR,
        };
        if ack == control::VIRTIO_NET_OK {
            self.stats.ctrl_commands += 1;
        } else {
            self.stats.ctrl_errors += 1;
        }
        ack
    }

    /// `VIRTIO_NET_CTRL_RX`: toggle a receive-filter mode (1-byte on/off).
    fn ctrl_rx(&mut self, command: u8, data: &[u8]) -> u8 {
        let Some(&on) = data.first() else {
            return control::VIRTIO_NET_ERR;
        };
        let flag = match command {
            control::VIRTIO_NET_CTRL_RX_PROMISC => RxFilterMode::PROMISC,
            control::VIRTIO_NET_CTRL_RX_ALLMULTI => RxFilterMode::ALLMULTI,
            control::VIRTIO_NET_CTRL_RX_ALLUNI => RxFilterMode::ALLUNI,
            control::VIRTIO_NET_CTRL_RX_NOMULTI => RxFilterMode::NOMULTI,
            control::VIRTIO_NET_CTRL_RX_NOUNI => RxFilterMode::NOUNI,
            control::VIRTIO_NET_CTRL_RX_NOBCAST => RxFilterMode::NOBCAST,
            _ => return control::VIRTIO_NET_ERR,
        };
        self.control.rx_mode.set(flag, on != 0);
        control::VIRTIO_NET_OK
    }

    /// `VIRTIO_NET_CTRL_MAC`: set the primary MAC, or program the filter tables.
    fn ctrl_mac(&mut self, command: u8, data: &[u8]) -> u8 {
        match command {
            control::VIRTIO_NET_CTRL_MAC_ADDR_SET => {
                if data.len() < 6 {
                    return control::VIRTIO_NET_ERR;
                }
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&data[..6]);
                self.mac = MacAddress(mac);
                control::VIRTIO_NET_OK
            }
            control::VIRTIO_NET_CTRL_MAC_TABLE_SET => {
                // Two consecutive virtio_net_ctrl_mac sub-tables: unicast, then
                // multicast. Parse both before committing so a malformed second
                // table does not leave a half-applied filter.
                let Some((unicast, used)) = control::parse_mac_table(data) else {
                    return control::VIRTIO_NET_ERR;
                };
                let Some((multicast, _)) = control::parse_mac_table(&data[used..]) else {
                    return control::VIRTIO_NET_ERR;
                };
                self.control.unicast_table = unicast;
                self.control.multicast_table = multicast;
                control::VIRTIO_NET_OK
            }
            _ => control::VIRTIO_NET_ERR,
        }
    }

    /// `VIRTIO_NET_CTRL_VLAN`: add or remove a VLAN id from the filter.
    fn ctrl_vlan(&mut self, command: u8, data: &[u8]) -> u8 {
        if data.len() < 2 {
            return control::VIRTIO_NET_ERR;
        }
        let vid = u16::from_le_bytes([data[0], data[1]]);
        if vid >= control::VLAN_VID_MAX {
            return control::VIRTIO_NET_ERR;
        }
        match command {
            control::VIRTIO_NET_CTRL_VLAN_ADD => {
                self.control.vlan_filter.insert(vid);
                control::VIRTIO_NET_OK
            }
            control::VIRTIO_NET_CTRL_VLAN_DEL => {
                self.control.vlan_filter.remove(&vid);
                control::VIRTIO_NET_OK
            }
            _ => control::VIRTIO_NET_ERR,
        }
    }

    /// `VIRTIO_NET_CTRL_MQ`: set the number of active queue pairs.
    const fn ctrl_mq(&mut self, command: u8, data: &[u8]) -> u8 {
        if command != control::VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET || data.len() < 2 {
            return control::VIRTIO_NET_ERR;
        }
        self.control.vq_pairs = u16::from_le_bytes([data[0], data[1]]);
        control::VIRTIO_NET_OK
    }

    /// `VIRTIO_NET_CTRL_GUEST_OFFLOADS`: enable/disable the guest receive
    /// offloads at runtime. The 8-byte `le64` payload is a bitmap of
    /// `VIRTIO_NET_F_*` positions; only offloads that were actually negotiated
    /// (a subset of the controllable `GUEST_*` features) may be requested —
    /// anything else is rejected, as a real device would NAK an unsupported
    /// offload rather than silently accept it.
    const fn ctrl_guest_offloads(&mut self, command: u8, data: &[u8]) -> u8 {
        if command != control::VIRTIO_NET_CTRL_GUEST_OFFLOADS_SET || data.len() < 8 {
            return control::VIRTIO_NET_ERR;
        }
        let requested = u64::from_le_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);
        // The offloads this command can toggle, gated by what was negotiated.
        let supported = self.features.bits() & GUEST_OFFLOAD_MASK;
        if requested & !supported != 0 {
            return control::VIRTIO_NET_ERR;
        }
        self.control.active_offloads = requested;
        control::VIRTIO_NET_OK
    }

    /// Decide whether an inbound Ethernet `frame` passes the receive filter the
    /// guest programmed through the control virtqueue. Mirrors a real NIC:
    /// promiscuous accepts everything; otherwise broadcast, multicast, and
    /// unicast each have their own accept rule (all-modes, filter tables, the
    /// device's own MAC) and explicit drop modes. A frame too short to carry a
    /// destination MAC is dropped.
    fn accepts_frame(&self, frame: &[u8]) -> bool {
        let Some(dest) = frame.get(0..6) else {
            return false;
        };
        let c = &self.control;
        if c.rx_mode.contains(RxFilterMode::PROMISC) {
            return true;
        }
        let is_broadcast = dest == [0xFF; 6];
        let is_multicast = !is_broadcast && (dest[0] & 0x01) != 0;
        if is_broadcast {
            !c.rx_mode.contains(RxFilterMode::NOBCAST)
        } else if is_multicast {
            !c.rx_mode.contains(RxFilterMode::NOMULTI)
                && (c.rx_mode.contains(RxFilterMode::ALLMULTI)
                    || c.multicast_table.iter().any(|m| m.as_bytes() == dest))
        } else {
            // Unicast: our own MAC, an entry in the unicast filter table, or any
            // unicast when alluni is set; nouni drops all unicast.
            !c.rx_mode.contains(RxFilterMode::NOUNI)
                && (c.rx_mode.contains(RxFilterMode::ALLUNI)
                    || self.mac.as_bytes() == dest
                    || c.unicast_table.iter().any(|m| m.as_bytes() == dest))
        }
    }

    /// Process pending TX descriptors.
    ///
    /// Reads frames from the TX virtqueue and sends them through the backend.
    pub fn process_tx(&mut self) {
        if !self.tx_queue.is_enabled() {
            return;
        }

        while self.tx_queue.has_available() {
            let Ok(desc) = self.tx_queue.pop_available() else {
                break;
            };

            // The descriptor data contains: [VirtioNetHeader][Ethernet frame]
            let hdr_size = VirtioNetHeader::wire_size(self.merge_rxbuf);
            if desc.data.len() < hdr_size {
                self.stats.tx_errors += 1;
                self.tx_queue.push_used(desc);
                continue;
            }

            // Honour a checksum-offload request: when the guest sets NEEDS_CSUM
            // it has only seeded the pseudo-header sum, so the device must finish
            // the checksum before the frame goes on the wire.
            let header = VirtioNetHeader::from_bytes(&desc.data, self.merge_rxbuf);
            let result = match header {
                Some(h) if h.needs_csum() => {
                    let (start, offset) = ({ h.csum_start }, { h.csum_offset });
                    let mut frame = desc.data[hdr_size..].to_vec();
                    if Self::complete_checksum(&mut frame, start, offset).is_err() {
                        self.stats.tx_errors += 1;
                        self.tx_queue.push_used(desc);
                        continue;
                    }
                    self.stats.tx_csum_offloads += 1;
                    self.backend.send(&frame)
                }
                _ => self.backend.send(&desc.data[hdr_size..]),
            };
            match result {
                Ok(n) => {
                    self.stats.tx_packets += 1;
                    self.stats.tx_bytes += n as u64;
                }
                Err(_) => {
                    self.stats.tx_errors += 1;
                }
            }

            self.tx_queue.push_used(desc);
        }
    }

    /// Complete a checksum-offload request: write the internet checksum over
    /// `frame[csum_start..]` into the two bytes at `csum_start + csum_offset`,
    /// where the guest has seeded the pseudo-header partial sum. Errors if the
    /// offsets fall outside the frame.
    fn complete_checksum(frame: &mut [u8], csum_start: u16, csum_offset: u16) -> Result<(), ()> {
        let start = usize::from(csum_start);
        let pos = start + usize::from(csum_offset);
        if start > frame.len() || pos + 2 > frame.len() {
            return Err(());
        }
        let csum = internet_checksum(&frame[start..]);
        frame[pos..pos + 2].copy_from_slice(&csum.to_be_bytes());
        Ok(())
    }

    /// Process pending RX: read from backend and queue frames.
    ///
    /// Returns the number of frames received.
    pub fn process_rx(&mut self) -> usize {
        if !self.rx_queue.is_enabled() {
            return 0;
        }

        // Read new frames from the backend.
        let mut received = 0;
        let mut buf = vec![0u8; 65536];
        loop {
            if !self.backend.has_pending_rx() {
                break;
            }
            match self.backend.recv(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Apply the guest's receive filter to frames arriving off the
                    // wire; a rejected frame is counted as a drop, not delivered.
                    if !self.accepts_frame(&buf[..n]) {
                        self.stats.rx_drops += 1;
                        continue;
                    }
                    // Prepend the VirtIO net header (num_buffers set for merge mode).
                    let mut frame = self.rx_header().to_bytes(self.merge_rxbuf);
                    frame.extend_from_slice(&buf[..n]);
                    self.rx_pending.push_back(frame);
                    received += 1;
                }
            }
        }

        // Deliver pending frames to guest RX buffers.
        while let Some(frame) = self.rx_pending.front() {
            if !self.rx_queue.has_available() {
                break;
            }
            let Ok(mut desc) = self.rx_queue.pop_available() else {
                break;
            };

            if desc.data.len() < frame.len() {
                self.stats.rx_drops += 1;
                self.rx_queue.push_used(desc);
                self.rx_pending.pop_front();
                continue;
            }

            desc.data[..frame.len()].copy_from_slice(frame);
            desc.data.truncate(frame.len());
            self.stats.rx_packets += 1;
            self.stats.rx_bytes += frame.len() as u64;
            self.rx_queue.push_used(desc);
            self.rx_pending.pop_front();
        }

        received
    }

    /// The virtio-net header prepended to a received frame. When mergeable RX
    /// buffers are negotiated (`VIRTIO_NET_F_MRG_RXBUF`), `num_buffers` must be
    /// the count of descriptors the frame spans and is **≥1** — a guest reads it
    /// to know how many buffers to consume, and 0 is invalid. This model places
    /// each frame in a single RX buffer, so `num_buffers` is 1 in merge mode
    /// (the field is not serialized at all without merge).
    ///
    /// When the `GUEST_CSUM` receive offload is active, the header advertises
    /// `VIRTIO_NET_HDR_F_DATA_VALID`: the frames this model delivers carry valid
    /// checksums, so the guest may skip re-verifying them (the point of the
    /// offload). The flag is dropped once the guest disables `GUEST_CSUM` via
    /// `VIRTIO_NET_CTRL_GUEST_OFFLOADS`.
    fn rx_header(&self) -> VirtioNetHeader {
        let mut flags = 0u8;
        if self.control.active_offloads & NetFeatures::GUEST_CSUM != 0 {
            flags |= super::header::flags::DATA_VALID;
        }
        VirtioNetHeader {
            flags,
            num_buffers: u16::from(self.merge_rxbuf),
            ..VirtioNetHeader::EMPTY
        }
    }

    /// Inject a frame directly into the RX path (for testing or switch delivery).
    pub fn inject_rx(&mut self, frame: &[u8]) {
        let mut data = self.rx_header().to_bytes(self.merge_rxbuf);
        data.extend_from_slice(frame);
        self.rx_pending.push_back(data);
    }

    /// Get number of pending RX frames.
    #[must_use]
    pub fn pending_rx_count(&self) -> usize {
        self.rx_pending.len()
    }

    /// Check if there are used TX descriptors ready for the guest.
    #[must_use]
    pub fn has_tx_completions(&self) -> bool {
        self.tx_queue.has_used()
    }

    /// Check if there are used RX descriptors ready for the guest.
    #[must_use]
    pub fn has_rx_completions(&self) -> bool {
        self.rx_queue.has_used()
    }

    /// Get backend name.
    #[must_use]
    pub fn backend_name(&self) -> &str {
        self.backend.backend_name()
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::NullBackend;
    use super::*;

    fn make_device() -> VirtioNetDevice {
        let config =
            NetDeviceConfig::new("test0", MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]));
        let backend = Box::new(NullBackend::new());
        VirtioNetDevice::new(&config, backend)
    }

    #[test]
    fn test_create_device() {
        let dev = make_device();
        assert_eq!(dev.name(), "test0");
        assert_eq!(dev.status(), 0);
    }

    #[test]
    fn test_activate_and_reset() {
        let mut dev = make_device();
        dev.activate(NetFeatures::from_bits(
            NetFeatures::MAC | NetFeatures::STATUS,
        ));
        assert_eq!(dev.status(), DeviceStatus::DriverOk as u8);

        dev.reset();
        assert_eq!(dev.status(), 0);
    }

    #[test]
    fn test_tx_processing() {
        let mut dev = make_device();
        dev.activate(NetFeatures::from_bits(
            NetFeatures::MAC | NetFeatures::STATUS,
        ));

        // Build a TX descriptor: [header][frame]
        let hdr = VirtioNetHeader::EMPTY;
        let mut data = hdr.to_bytes(false);
        data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        dev.tx_queue.push_available(data, false).unwrap();

        dev.process_tx();
        assert_eq!(dev.stats().tx_packets, 1);
        assert!(dev.has_tx_completions());
    }

    #[test]
    fn test_inject_rx() {
        let mut dev = make_device();
        dev.activate(NetFeatures::from_bits(
            NetFeatures::MAC | NetFeatures::STATUS,
        ));

        dev.inject_rx(&[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(dev.pending_rx_count(), 1);

        // Push an RX buffer for the guest
        let buf = vec![0u8; 256];
        dev.rx_queue.push_available(buf, true).unwrap();

        dev.process_rx();
        assert_eq!(dev.stats().rx_packets, 1);
        assert!(dev.has_rx_completions());
    }

    #[test]
    fn rx_header_num_buffers_is_one_in_merge_mode() {
        let mut dev = make_device();
        // Negotiate mergeable RX buffers.
        dev.activate(NetFeatures::from_bits(
            NetFeatures::MAC | NetFeatures::MRG_RXBUF,
        ));
        dev.inject_rx(&[0xAA, 0xBB, 0xCC]);
        let frame = dev.rx_pending.front().expect("one pending frame");
        // The 12-byte (merge) header parses with num_buffers == 1, not the
        // invalid 0 — a guest needs ≥1 to know how many buffers to consume.
        let hdr = VirtioNetHeader::from_bytes(frame, true).expect("parse merge header");
        let num_buffers = hdr.num_buffers; // copy out of the packed struct
        assert_eq!(num_buffers, 1);
    }

    #[test]
    fn rx_header_sets_data_valid_when_guest_csum_active() {
        let mut dev = make_device();
        // Negotiating GUEST_CSUM makes the offload active by default.
        dev.activate(NetFeatures::from_bits(
            NetFeatures::MAC | NetFeatures::GUEST_CSUM,
        ));
        dev.inject_rx(&[0xAA, 0xBB, 0xCC]);
        let frame = dev.rx_pending.front().expect("one pending frame");
        let hdr = VirtioNetHeader::from_bytes(frame, false).expect("parse header");
        assert!(hdr.data_valid(), "DATA_VALID set while GUEST_CSUM active");
    }

    #[test]
    fn rx_header_omits_data_valid_without_guest_csum() {
        let mut dev = make_device();
        dev.activate(NetFeatures::from_bits(NetFeatures::MAC)); // no GUEST_CSUM
        dev.inject_rx(&[0x01, 0x02]);
        let frame = dev.rx_pending.front().expect("one pending frame");
        let hdr = VirtioNetHeader::from_bytes(frame, false).expect("parse header");
        assert!(!hdr.data_valid());
    }

    #[test]
    fn rx_header_drops_data_valid_when_guest_disables_csum() {
        let mut dev = make_device();
        dev.activate(NetFeatures::from_bits(
            NetFeatures::MAC
                | NetFeatures::GUEST_CSUM
                | NetFeatures::CTRL_VQ
                | NetFeatures::CTRL_GUEST_OFFLOADS,
        ));
        // Guest turns off all guest offloads.
        let mut cmd = vec![
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS,
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS_SET,
        ];
        cmd.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(dev.process_control(&cmd), control::VIRTIO_NET_OK);

        dev.inject_rx(&[0xDD, 0xEE]);
        let frame = dev.rx_pending.front().expect("one pending frame");
        let hdr = VirtioNetHeader::from_bytes(frame, false).expect("parse header");
        assert!(
            !hdr.data_valid(),
            "DATA_VALID cleared after disabling GUEST_CSUM"
        );
    }

    #[test]
    fn rx_header_omits_num_buffers_without_merge() {
        let mut dev = make_device();
        dev.activate(NetFeatures::from_bits(NetFeatures::MAC));
        dev.inject_rx(&[0xAA, 0xBB, 0xCC]);
        let frame = dev.rx_pending.front().expect("one pending frame");
        // Without merge the header is the 10-byte form: data starts right after.
        assert_eq!(frame.len(), VirtioNetHeader::wire_size(false) + 3);
    }

    #[test]
    fn test_tx_too_short() {
        let mut dev = make_device();
        dev.activate(NetFeatures::from_bits(
            NetFeatures::MAC | NetFeatures::STATUS,
        ));

        // Push a descriptor that's too short (no header)
        dev.tx_queue.push_available(vec![0x01], false).unwrap();
        dev.process_tx();
        assert_eq!(dev.stats().tx_errors, 1);
    }

    #[test]
    fn test_backend_name() {
        let dev = make_device();
        assert_eq!(dev.backend_name(), "null");
    }

    #[test]
    fn device_status_values() {
        assert_eq!(DeviceStatus::Reset as u8, 0);
        assert_eq!(DeviceStatus::Acknowledge as u8, 1);
        assert_eq!(DeviceStatus::Driver as u8, 2);
        assert_eq!(DeviceStatus::DriverOk as u8, 4);
        assert_eq!(DeviceStatus::FeaturesOk as u8, 8);
        assert_eq!(DeviceStatus::Failed as u8, 128);
    }

    #[test]
    fn ctrl_vq_is_offered_by_default() {
        let dev = make_device();
        assert!(dev.features().contains(NetFeatures::CTRL_VQ));
        assert!(dev.features().contains(NetFeatures::CTRL_RX));
        assert!(dev.features().contains(NetFeatures::CTRL_VLAN));
    }

    #[test]
    fn ctrl_guest_offloads_set_accepts_negotiated_and_rejects_unsupported() {
        let mut dev = make_device();
        assert!(dev.features().contains(NetFeatures::CTRL_GUEST_OFFLOADS));

        // Enabling the negotiated GUEST_CSUM offload is accepted and stored.
        let mut ok = vec![
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS,
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS_SET,
        ];
        ok.extend_from_slice(&NetFeatures::GUEST_CSUM.to_le_bytes());
        assert_eq!(dev.process_control(&ok), control::VIRTIO_NET_OK);
        assert_eq!(dev.control_state().active_offloads, NetFeatures::GUEST_CSUM);

        // Requesting an un-negotiated offload (GUEST_TSO4) is rejected and the
        // stored set is left unchanged.
        let mut bad = vec![
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS,
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS_SET,
        ];
        bad.extend_from_slice(&NetFeatures::GUEST_TSO4.to_le_bytes());
        assert_eq!(dev.process_control(&bad), control::VIRTIO_NET_ERR);
        assert_eq!(dev.control_state().active_offloads, NetFeatures::GUEST_CSUM);

        // A truncated (<8-byte) payload is rejected.
        assert_eq!(
            dev.process_control(&[
                control::VIRTIO_NET_CTRL_GUEST_OFFLOADS,
                control::VIRTIO_NET_CTRL_GUEST_OFFLOADS_SET,
                0,
                0,
                0,
            ]),
            control::VIRTIO_NET_ERR
        );

        // Disabling all offloads (empty bitmap) is accepted.
        let mut off = vec![
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS,
            control::VIRTIO_NET_CTRL_GUEST_OFFLOADS_SET,
        ];
        off.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(dev.process_control(&off), control::VIRTIO_NET_OK);
        assert_eq!(dev.control_state().active_offloads, 0);
    }

    #[test]
    fn ctrl_rx_toggles_promiscuous_mode() {
        let mut dev = make_device();
        // class=RX cmd=PROMISC data=on.
        let ack = dev.process_control(&[
            control::VIRTIO_NET_CTRL_RX,
            control::VIRTIO_NET_CTRL_RX_PROMISC,
            1,
        ]);
        assert_eq!(ack, control::VIRTIO_NET_OK);
        assert!(dev.control_state().rx_mode.contains(RxFilterMode::PROMISC));
        // Turn it back off.
        dev.process_control(&[
            control::VIRTIO_NET_CTRL_RX,
            control::VIRTIO_NET_CTRL_RX_PROMISC,
            0,
        ]);
        assert!(!dev.control_state().rx_mode.contains(RxFilterMode::PROMISC));
        assert_eq!(dev.stats().ctrl_commands, 2);
    }

    #[test]
    fn ctrl_mac_addr_set_changes_the_mac() {
        let mut dev = make_device();
        let new = [0x52, 0x54, 0x00, 0xAB, 0xCD, 0xEF];
        let mut cmd = vec![
            control::VIRTIO_NET_CTRL_MAC,
            control::VIRTIO_NET_CTRL_MAC_ADDR_SET,
        ];
        cmd.extend_from_slice(&new);
        assert_eq!(dev.process_control(&cmd), control::VIRTIO_NET_OK);
        assert_eq!(dev.mac(), MacAddress(new));
    }

    #[test]
    fn ctrl_mac_table_set_programs_both_filters() {
        let mut dev = make_device();
        let mut cmd = vec![
            control::VIRTIO_NET_CTRL_MAC,
            control::VIRTIO_NET_CTRL_MAC_TABLE_SET,
        ];
        // Unicast table: 1 entry.
        cmd.extend_from_slice(&1u32.to_le_bytes());
        cmd.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x10]);
        // Multicast table: 2 entries.
        cmd.extend_from_slice(&2u32.to_le_bytes());
        cmd.extend_from_slice(&[0x33, 0x33, 0, 0, 0, 0x01]);
        cmd.extend_from_slice(&[0x33, 0x33, 0, 0, 0, 0x02]);
        assert_eq!(dev.process_control(&cmd), control::VIRTIO_NET_OK);
        assert_eq!(dev.control_state().unicast_table.len(), 1);
        assert_eq!(dev.control_state().multicast_table.len(), 2);
    }

    #[test]
    fn ctrl_mac_table_set_rejects_a_truncated_table() {
        let mut dev = make_device();
        let mut cmd = vec![
            control::VIRTIO_NET_CTRL_MAC,
            control::VIRTIO_NET_CTRL_MAC_TABLE_SET,
        ];
        cmd.extend_from_slice(&5u32.to_le_bytes()); // claims 5 entries…
        cmd.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x10]); // …but supplies one
        assert_eq!(dev.process_control(&cmd), control::VIRTIO_NET_ERR);
        assert!(dev.control_state().unicast_table.is_empty(), "not applied");
    }

    #[test]
    fn ctrl_vlan_add_and_remove() {
        let mut dev = make_device();
        let add = |vid: u16| {
            let mut c = vec![
                control::VIRTIO_NET_CTRL_VLAN,
                control::VIRTIO_NET_CTRL_VLAN_ADD,
            ];
            c.extend_from_slice(&vid.to_le_bytes());
            c
        };
        assert_eq!(dev.process_control(&add(42)), control::VIRTIO_NET_OK);
        assert!(dev.control_state().vlan_filter.contains(&42));
        // Out-of-range VLAN id is rejected.
        assert_eq!(dev.process_control(&add(5000)), control::VIRTIO_NET_ERR);
        // Remove it.
        let mut del = vec![
            control::VIRTIO_NET_CTRL_VLAN,
            control::VIRTIO_NET_CTRL_VLAN_DEL,
        ];
        del.extend_from_slice(&42u16.to_le_bytes());
        assert_eq!(dev.process_control(&del), control::VIRTIO_NET_OK);
        assert!(!dev.control_state().vlan_filter.contains(&42));
    }

    #[test]
    fn ctrl_mq_sets_queue_pairs() {
        let mut dev = make_device();
        let mut cmd = vec![
            control::VIRTIO_NET_CTRL_MQ,
            control::VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET,
        ];
        cmd.extend_from_slice(&4u16.to_le_bytes());
        assert_eq!(dev.process_control(&cmd), control::VIRTIO_NET_OK);
        assert_eq!(dev.control_state().vq_pairs, 4);
    }

    #[test]
    fn ctrl_rejects_short_buffer_unknown_class_and_without_feature() {
        let mut dev = make_device();
        assert_eq!(
            dev.process_control(&[control::VIRTIO_NET_CTRL_RX]),
            control::VIRTIO_NET_ERR
        );
        assert_eq!(dev.process_control(&[0xFE, 0x00]), control::VIRTIO_NET_ERR);
        assert!(dev.stats().ctrl_errors >= 2);

        // Activating without CTRL_VQ disables the control path entirely.
        dev.activate(NetFeatures::from_bits(NetFeatures::MAC));
        let ack = dev.process_control(&[
            control::VIRTIO_NET_CTRL_RX,
            control::VIRTIO_NET_CTRL_RX_PROMISC,
            1,
        ]);
        assert_eq!(ack, control::VIRTIO_NET_ERR);
    }

    #[test]
    fn config_space_reports_mac_and_link_status() {
        let mut dev = make_device();
        // MAC at offset 0 (6 bytes).
        for (i, &b) in [0x02u8, 0x00, 0x00, 0x00, 0x00, 0x01].iter().enumerate() {
            assert_eq!(dev.read_config(i as u64, 1), u64::from(b), "mac byte {i}");
        }
        // status at offset 6: link up by default.
        assert_eq!(dev.read_config(6, 2), u64::from(NET_S_LINK_UP));
        // max_virtqueue_pairs at offset 8 defaults to 1.
        assert_eq!(dev.read_config(8, 2), 1);

        // Drop the link → status clears LINK_UP.
        dev.set_link_up(false);
        assert!(!dev.is_link_up());
        assert_eq!(dev.read_config(6, 2), 0);

        // Requesting an announce sets the ANNOUNCE bit alongside LINK_UP.
        dev.set_link_up(true);
        dev.request_announce();
        assert_eq!(
            dev.read_config(6, 2),
            u64::from(NET_S_LINK_UP | NET_S_ANNOUNCE)
        );
        // The guest acks it through the control vq, clearing the bit.
        dev.process_control(&[
            control::VIRTIO_NET_CTRL_ANNOUNCE,
            control::VIRTIO_NET_CTRL_ANNOUNCE_ACK,
        ]);
        assert_eq!(dev.read_config(6, 2), u64::from(NET_S_LINK_UP));
    }

    #[test]
    fn config_space_out_of_range_reads_zero() {
        let dev = make_device();
        assert_eq!(dev.read_config(100, 4), 0);
    }

    #[test]
    fn config_space_reports_mtu() {
        let dev = make_device(); // default config MTU = 1500
        assert!(dev.features().contains(NetFeatures::MTU));
        assert_eq!(dev.mtu(), 1500);
        // mtu is at config offset 10 (after mac[6], status[2], pairs[2]).
        assert_eq!(dev.read_config(10, 2), 1500);

        // A jumbo-frame MTU is reported faithfully.
        let mut config = NetDeviceConfig::new("j0", MacAddress([0x02, 0, 0, 0, 0, 0x01]));
        config.mtu = 9000;
        let jumbo = VirtioNetDevice::new(&config, Box::new(NullBackend::new()));
        assert_eq!(jumbo.read_config(10, 2), 9000);
    }

    // A classic IPv4 header (checksum field zeroed) whose internet checksum is
    // 0xB861 — the worked example from the IPv4 checksum literature.
    fn ipv4_header_zeroed_csum() -> Vec<u8> {
        vec![
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ]
    }

    #[test]
    fn internet_checksum_matches_known_vector() {
        assert_eq!(internet_checksum(&ipv4_header_zeroed_csum()), 0xB861);
        // A fully-formed (valid) header checksums to zero.
        let mut hdr = ipv4_header_zeroed_csum();
        hdr[10..12].copy_from_slice(&0xB861u16.to_be_bytes());
        assert_eq!(internet_checksum(&hdr), 0);
    }

    #[test]
    fn complete_checksum_fills_the_field() {
        let mut frame = ipv4_header_zeroed_csum();
        // csum_start = 0 (whole header), csum_offset = 10 (the checksum field).
        VirtioNetDevice::complete_checksum(&mut frame, 0, 10).unwrap();
        assert_eq!(&frame[10..12], &0xB861u16.to_be_bytes());
        // Out-of-range offsets are rejected.
        assert!(VirtioNetDevice::complete_checksum(&mut frame, 0, 100).is_err());
    }

    #[test]
    fn process_tx_completes_a_needs_csum_frame() {
        use std::sync::{Arc, Mutex};
        // A backend that captures the last frame it was asked to send.
        #[derive(Clone)]
        struct CaptureBackend(Arc<Mutex<Vec<u8>>>);
        impl super::super::backend::NetBackend for CaptureBackend {
            fn send(&mut self, frame: &[u8]) -> std::io::Result<usize> {
                *self.0.lock().unwrap() = frame.to_vec();
                Ok(frame.len())
            }
            fn recv(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
            fn has_pending_rx(&self) -> bool {
                false
            }
            fn backend_name(&self) -> &'static str {
                "capture"
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let config = NetDeviceConfig::new("tx0", MacAddress([0x02, 0, 0, 0, 0, 0x01]));
        let mut dev = VirtioNetDevice::new(&config, Box::new(CaptureBackend(captured.clone())));
        dev.activate(NetFeatures::from_bits(NetFeatures::MAC)); // non-merge header

        // Build [virtio header (NEEDS_CSUM)][IP header with zeroed checksum].
        let hdr = VirtioNetHeader {
            flags: super::super::header::flags::NEEDS_CSUM,
            csum_start: 0,
            csum_offset: 10,
            ..VirtioNetHeader::EMPTY
        };
        let mut data = hdr.to_bytes(false);
        data.extend_from_slice(&ipv4_header_zeroed_csum());
        dev.tx_queue.push_available(data, false).unwrap();

        dev.process_tx();
        assert_eq!(dev.stats().tx_csum_offloads, 1);
        assert_eq!(dev.stats().tx_packets, 1);
        let sent = captured.lock().unwrap().clone();
        assert_eq!(
            &sent[10..12],
            &0xB861u16.to_be_bytes(),
            "checksum completed on the wire"
        );
    }

    fn eth_frame(dest: [u8; 6]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&dest);
        f.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x99]); // src
        f.extend_from_slice(&[0x08, 0x00]); // ethertype IPv4
        f.resize(64, 0);
        f
    }

    #[test]
    fn rx_filter_accepts_own_mac_and_broadcast_drops_others() {
        let dev = make_device(); // MAC 02:00:00:00:00:01, default filter
        let own = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        assert!(dev.accepts_frame(&eth_frame(own)), "own unicast");
        assert!(dev.accepts_frame(&eth_frame([0xFF; 6])), "broadcast");
        assert!(
            !dev.accepts_frame(&eth_frame([0x02, 0, 0, 0, 0, 0x02])),
            "other unicast dropped by default"
        );
        assert!(
            !dev.accepts_frame(&eth_frame([0x01, 0, 0x5E, 0, 0, 0x01])),
            "multicast dropped by default"
        );
        assert!(!dev.accepts_frame(&[0x01, 0x02]), "runt frame dropped");
    }

    #[test]
    fn rx_filter_promisc_accepts_everything() {
        let mut dev = make_device();
        dev.process_control(&[
            control::VIRTIO_NET_CTRL_RX,
            control::VIRTIO_NET_CTRL_RX_PROMISC,
            1,
        ]);
        assert!(dev.accepts_frame(&eth_frame([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01])));
        assert!(dev.accepts_frame(&eth_frame([0x01, 0, 0x5E, 0, 0, 0x01])));
    }

    #[test]
    fn rx_filter_honours_tables_and_drop_modes() {
        let mut dev = make_device();
        // Program a unicast and a multicast filter entry.
        let mut cmd = vec![
            control::VIRTIO_NET_CTRL_MAC,
            control::VIRTIO_NET_CTRL_MAC_TABLE_SET,
        ];
        cmd.extend_from_slice(&1u32.to_le_bytes());
        cmd.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x42]); // extra unicast
        cmd.extend_from_slice(&1u32.to_le_bytes());
        cmd.extend_from_slice(&[0x01, 0, 0x5E, 0, 0, 0x07]); // joined multicast
        dev.process_control(&cmd);
        assert!(
            dev.accepts_frame(&eth_frame([0x02, 0, 0, 0, 0, 0x42])),
            "table unicast"
        );
        assert!(
            dev.accepts_frame(&eth_frame([0x01, 0, 0x5E, 0, 0, 0x07])),
            "joined mcast"
        );
        assert!(
            !dev.accepts_frame(&eth_frame([0x01, 0, 0x5E, 0, 0, 0x08])),
            "unjoined mcast"
        );

        // allmulti accepts any multicast; nobcast drops broadcast.
        dev.process_control(&[
            control::VIRTIO_NET_CTRL_RX,
            control::VIRTIO_NET_CTRL_RX_ALLMULTI,
            1,
        ]);
        assert!(
            dev.accepts_frame(&eth_frame([0x01, 0, 0x5E, 0, 0, 0x08])),
            "allmulti"
        );
        dev.process_control(&[
            control::VIRTIO_NET_CTRL_RX,
            control::VIRTIO_NET_CTRL_RX_NOBCAST,
            1,
        ]);
        assert!(
            !dev.accepts_frame(&eth_frame([0xFF; 6])),
            "nobcast drops broadcast"
        );
    }

    #[test]
    fn process_rx_drops_frames_the_filter_rejects() {
        // A backend that hands out two frames: one to our MAC, one to a stranger.
        struct FeedBackend(VecDeque<Vec<u8>>);
        impl super::super::backend::NetBackend for FeedBackend {
            fn send(&mut self, frame: &[u8]) -> std::io::Result<usize> {
                Ok(frame.len())
            }
            fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.pop_front().map_or(Ok(0), |f| {
                    buf[..f.len()].copy_from_slice(&f);
                    Ok(f.len())
                })
            }
            fn has_pending_rx(&self) -> bool {
                !self.0.is_empty()
            }
            fn backend_name(&self) -> &'static str {
                "feed"
            }
        }

        let own = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let frames = VecDeque::from(vec![eth_frame(own), eth_frame([0x02, 0, 0, 0, 0, 0xFE])]);
        let config = NetDeviceConfig::new("test0", MacAddress(own));
        let mut dev = VirtioNetDevice::new(&config, Box::new(FeedBackend(frames)));
        dev.activate(NetFeatures::from_bits(NetFeatures::MAC));
        for _ in 0..2 {
            dev.rx_queue.push_available(vec![0u8; 256], true).unwrap();
        }
        let received = dev.process_rx();
        assert_eq!(received, 1, "only the frame to our MAC is accepted");
        assert_eq!(dev.stats().rx_drops, 1, "the stranger frame was dropped");
        assert_eq!(dev.stats().rx_packets, 1);
    }

    #[test]
    fn ctrl_announce_ack_clears_pending() {
        let mut dev = make_device();
        // Simulate a pending announcement, then ack it.
        dev.process_control(&[
            control::VIRTIO_NET_CTRL_RX,
            control::VIRTIO_NET_CTRL_RX_PROMISC,
            1,
        ]);
        let ack = dev.process_control(&[
            control::VIRTIO_NET_CTRL_ANNOUNCE,
            control::VIRTIO_NET_CTRL_ANNOUNCE_ACK,
        ]);
        assert_eq!(ack, control::VIRTIO_NET_OK);
        assert!(!dev.control_state().announce_needed);
    }
}
