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

use std::collections::VecDeque;

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
            status: 0,
            tx_queue: Virtqueue::new(format!("{}-tx", config.name), queue_size),
            rx_queue: Virtqueue::new(format!("{}-rx", config.name), queue_size),
            queue_size,
            backend,
            rx_pending: VecDeque::new(),
            control: NetControlState::default(),
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
        self.status = DeviceStatus::DriverOk as u8;
        self.tx_queue.enable();
        self.rx_queue.enable();
    }

    /// Reset the device to initial state.
    pub fn reset(&mut self) {
        self.status = 0;
        self.features = NetFeatures::from_bits(NetFeatures::DEFAULT);
        self.merge_rxbuf = false;
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

            let frame = &desc.data[hdr_size..];
            match self.backend.send(frame) {
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
    fn rx_header(&self) -> VirtioNetHeader {
        VirtioNetHeader {
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
