//! virtio-net control virtqueue (`VIRTIO_NET_CTRL`).
//!
//! A guest that negotiates `VIRTIO_NET_F_CTRL_VQ` configures RX filtering,
//! the MAC filter tables, VLAN filtering, link announcement, and multiqueue
//! through a dedicated control virtqueue rather than the config space. Each
//! command is a buffer `{ class: u8, command: u8, command-specific data… }`
//! and the device replies with a single ack byte ([`VIRTIO_NET_OK`] /
//! [`VIRTIO_NET_ERR`]). This module holds the parsed constants and the device
//! state those commands mutate; [`super::VirtioNetDevice::process_control`]
//! drives it.

use super::config::MacAddress;
use std::collections::BTreeSet;

/// Control command classes.
pub const VIRTIO_NET_CTRL_RX: u8 = 0;
pub const VIRTIO_NET_CTRL_MAC: u8 = 1;
pub const VIRTIO_NET_CTRL_VLAN: u8 = 2;
pub const VIRTIO_NET_CTRL_ANNOUNCE: u8 = 3;
pub const VIRTIO_NET_CTRL_MQ: u8 = 4;

/// `VIRTIO_NET_CTRL_RX` commands (each takes a 1-byte on/off payload).
pub const VIRTIO_NET_CTRL_RX_PROMISC: u8 = 0;
pub const VIRTIO_NET_CTRL_RX_ALLMULTI: u8 = 1;
pub const VIRTIO_NET_CTRL_RX_ALLUNI: u8 = 2;
pub const VIRTIO_NET_CTRL_RX_NOMULTI: u8 = 3;
pub const VIRTIO_NET_CTRL_RX_NOUNI: u8 = 4;
pub const VIRTIO_NET_CTRL_RX_NOBCAST: u8 = 5;

/// `VIRTIO_NET_CTRL_MAC` commands.
pub const VIRTIO_NET_CTRL_MAC_TABLE_SET: u8 = 0;
pub const VIRTIO_NET_CTRL_MAC_ADDR_SET: u8 = 1;

/// `VIRTIO_NET_CTRL_VLAN` commands (each takes a 2-byte VLAN id payload).
pub const VIRTIO_NET_CTRL_VLAN_ADD: u8 = 0;
pub const VIRTIO_NET_CTRL_VLAN_DEL: u8 = 1;

/// `VIRTIO_NET_CTRL_ANNOUNCE` command.
pub const VIRTIO_NET_CTRL_ANNOUNCE_ACK: u8 = 0;

/// `VIRTIO_NET_CTRL_MQ` command (takes a 2-byte queue-pair count).
pub const VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET: u8 = 0;

/// Acks returned in the command's status byte.
pub const VIRTIO_NET_OK: u8 = 0;
pub const VIRTIO_NET_ERR: u8 = 1;

/// VLAN ids are 12-bit.
pub const VLAN_VID_MAX: u16 = 4096;

bitflags::bitflags! {
    /// Receive-filter modes toggled via `VIRTIO_NET_CTRL_RX`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct RxFilterMode: u8 {
        /// Receive all packets regardless of destination.
        const PROMISC = 1 << 0;
        /// Receive all multicast packets.
        const ALLMULTI = 1 << 1;
        /// Receive all unicast packets.
        const ALLUNI = 1 << 2;
        /// Drop all multicast packets.
        const NOMULTI = 1 << 3;
        /// Drop all unicast packets.
        const NOUNI = 1 << 4;
        /// Drop broadcast packets.
        const NOBCAST = 1 << 5;
    }
}

/// State mutated by control-virtqueue commands.
#[derive(Debug, Default, Clone)]
pub struct NetControlState {
    /// Active receive-filter modes.
    pub rx_mode: RxFilterMode,
    /// Accepted VLAN ids (empty = no VLAN filtering applied).
    pub vlan_filter: BTreeSet<u16>,
    /// Programmed unicast MAC filter table.
    pub unicast_table: Vec<MacAddress>,
    /// Programmed multicast MAC filter table.
    pub multicast_table: Vec<MacAddress>,
    /// A link-change announcement is pending guest acknowledgement.
    pub announce_needed: bool,
    /// Negotiated number of active queue pairs (multiqueue).
    pub vq_pairs: u16,
}

/// Parse one `virtio_net_ctrl_mac` sub-table (`le32 entries` followed by that
/// many 6-byte MACs) from the front of `data`, returning the table and the
/// number of bytes consumed, or `None` if the buffer is malformed.
pub(super) fn parse_mac_table(data: &[u8]) -> Option<(Vec<MacAddress>, usize)> {
    if data.len() < 4 {
        return None;
    }
    let entries = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    let needed = 4usize.checked_add(entries.checked_mul(6)?)?;
    if data.len() < needed {
        return None;
    }
    let mut table = Vec::with_capacity(entries);
    for i in 0..entries {
        let off = 4 + i * 6;
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&data[off..off + 6]);
        table.push(MacAddress(mac));
    }
    Some((table, needed))
}
