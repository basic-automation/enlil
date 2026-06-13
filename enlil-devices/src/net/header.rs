//! `VirtIO` network header.
//!
//! Every packet transmitted or received through a VirtIO-net device is
//! prefixed with this header. Defined in `VirtIO` 1.2, Section 5.1.6.

use std::fmt;

/// GSO (Generic Segmentation Offload) types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GsoType {
    None = 0,
    TcpV4 = 1,
    Udp = 3,
    TcpV6 = 4,
    TcpEcn = 0x80,
}

impl GsoType {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::TcpV4),
            3 => Some(Self::Udp),
            4 => Some(Self::TcpV6),
            0x80 => Some(Self::TcpEcn),
            _ => None,
        }
    }
}

/// Header flags.
pub mod flags {
    /// The device needs a checksum computed.
    pub const NEEDS_CSUM: u8 = 1;
    /// The device has validated the received data checksum.
    pub const DATA_VALID: u8 = 2;
}

/// `VirtIO` network header (12 bytes, or 10 without mergeable rx buffers).
///
/// This header precedes every Ethernet frame in the virtqueue.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct VirtioNetHeader {
    /// Flags (see `flags` module).
    pub flags: u8,
    /// GSO type (see `GsoType`).
    pub gso_type: u8,
    /// Ethernet + IP + TCP/UDP header length for GSO.
    pub hdr_len: u16,
    /// Maximum segment size for GSO.
    pub gso_size: u16,
    /// Checksum start offset from the beginning of the packet.
    pub csum_start: u16,
    /// Checksum offset from `csum_start` to place the checksum.
    pub csum_offset: u16,
    /// Number of merged buffers (only with `VIRTIO_NET_F_MRG_RXBUF`).
    pub num_buffers: u16,
}

/// Size of the header without the `num_buffers` field.
pub const VIRTIO_NET_HDR_SIZE: usize = 10;
/// Size of the header with the `num_buffers` field (mergeable rx buffers).
pub const VIRTIO_NET_HDR_SIZE_MRG: usize = 12;

impl VirtioNetHeader {
    /// A zeroed header (no offloads, no GSO).
    pub const EMPTY: Self = Self {
        flags: 0,
        gso_type: 0,
        hdr_len: 0,
        gso_size: 0,
        csum_start: 0,
        csum_offset: 0,
        num_buffers: 0,
    };

    /// Parse a header from a byte slice.
    ///
    /// If `merge_rxbuf` is true, expects 12 bytes; otherwise 10.
    #[must_use]
    pub const fn from_bytes(data: &[u8], merge_rxbuf: bool) -> Option<Self> {
        let min_len = if merge_rxbuf {
            VIRTIO_NET_HDR_SIZE_MRG
        } else {
            VIRTIO_NET_HDR_SIZE
        };

        if data.len() < min_len {
            return None;
        }

        Some(Self {
            flags: data[0],
            gso_type: data[1],
            hdr_len: u16::from_le_bytes([data[2], data[3]]),
            gso_size: u16::from_le_bytes([data[4], data[5]]),
            csum_start: u16::from_le_bytes([data[6], data[7]]),
            csum_offset: u16::from_le_bytes([data[8], data[9]]),
            num_buffers: if merge_rxbuf {
                u16::from_le_bytes([data[10], data[11]])
            } else {
                0
            },
        })
    }

    /// Serialize the header to bytes.
    #[must_use]
    pub fn to_bytes(&self, merge_rxbuf: bool) -> Vec<u8> {
        let mut buf = Vec::with_capacity(if merge_rxbuf {
            VIRTIO_NET_HDR_SIZE_MRG
        } else {
            VIRTIO_NET_HDR_SIZE
        });
        buf.push(self.flags);
        buf.push(self.gso_type);
        buf.extend_from_slice(&{ self.hdr_len }.to_le_bytes());
        buf.extend_from_slice(&{ self.gso_size }.to_le_bytes());
        buf.extend_from_slice(&{ self.csum_start }.to_le_bytes());
        buf.extend_from_slice(&{ self.csum_offset }.to_le_bytes());
        if merge_rxbuf {
            buf.extend_from_slice(&{ self.num_buffers }.to_le_bytes());
        }
        buf
    }

    /// Size of this header in bytes.
    #[must_use]
    pub const fn wire_size(merge_rxbuf: bool) -> usize {
        if merge_rxbuf {
            VIRTIO_NET_HDR_SIZE_MRG
        } else {
            VIRTIO_NET_HDR_SIZE
        }
    }

    /// Check if checksum offload is requested.
    #[must_use]
    pub const fn needs_csum(&self) -> bool {
        (self.flags & flags::NEEDS_CSUM) != 0
    }

    /// Check if received data checksum has been validated.
    #[must_use]
    pub const fn data_valid(&self) -> bool {
        (self.flags & flags::DATA_VALID) != 0
    }

    /// Get the GSO type.
    #[must_use]
    pub const fn gso(&self) -> Option<GsoType> {
        GsoType::from_u8(self.gso_type)
    }
}

impl Default for VirtioNetHeader {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl fmt::Debug for VirtioNetHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtioNetHeader")
            .field("flags", &self.flags)
            .field("gso_type", &self.gso_type)
            .field("hdr_len", &{ self.hdr_len })
            .field("gso_size", &{ self.gso_size })
            .field("csum_start", &{ self.csum_start })
            .field("csum_offset", &{ self.csum_offset })
            .field("num_buffers", &{ self.num_buffers })
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_header_roundtrip() {
        let hdr = VirtioNetHeader::EMPTY;
        let bytes = hdr.to_bytes(true);
        assert_eq!(bytes.len(), VIRTIO_NET_HDR_SIZE_MRG);

        let parsed = VirtioNetHeader::from_bytes(&bytes, true).unwrap();
        assert_eq!(parsed, hdr);
    }

    #[test]
    fn header_without_merge() {
        let hdr = VirtioNetHeader::EMPTY;
        let bytes = hdr.to_bytes(false);
        assert_eq!(bytes.len(), VIRTIO_NET_HDR_SIZE);

        let parsed = VirtioNetHeader::from_bytes(&bytes, false).unwrap();
        assert_eq!(parsed.flags, 0);
        let num_buffers = { parsed.num_buffers };
        assert_eq!(num_buffers, 0);
    }

    #[test]
    fn header_with_csum() {
        let hdr = VirtioNetHeader {
            flags: flags::NEEDS_CSUM,
            gso_type: GsoType::None as u8,
            hdr_len: 0,
            gso_size: 0,
            csum_start: 14, // after Ethernet header
            csum_offset: 16,
            num_buffers: 1,
        };
        assert!(hdr.needs_csum());
        assert!(!hdr.data_valid());
        assert_eq!(hdr.gso(), Some(GsoType::None));

        let bytes = hdr.to_bytes(true);
        let parsed = VirtioNetHeader::from_bytes(&bytes, true).unwrap();
        let csum_start = { parsed.csum_start };
        let csum_offset = { parsed.csum_offset };
        let num_buffers = { parsed.num_buffers };
        assert_eq!(csum_start, 14);
        assert_eq!(csum_offset, 16);
        assert_eq!(num_buffers, 1);
    }

    #[test]
    fn parse_too_short() {
        let bytes = [0u8; 5];
        assert!(VirtioNetHeader::from_bytes(&bytes, false).is_none());
        assert!(VirtioNetHeader::from_bytes(&bytes, true).is_none());
    }

    #[test]
    fn gso_type_roundtrip() {
        assert_eq!(GsoType::from_u8(0), Some(GsoType::None));
        assert_eq!(GsoType::from_u8(1), Some(GsoType::TcpV4));
        assert_eq!(GsoType::from_u8(3), Some(GsoType::Udp));
        assert_eq!(GsoType::from_u8(4), Some(GsoType::TcpV6));
        assert_eq!(GsoType::from_u8(0x80), Some(GsoType::TcpEcn));
        assert_eq!(GsoType::from_u8(0xFF), None);
    }

    #[test]
    fn flag_constants_are_defined() {
        assert_eq!(flags::NEEDS_CSUM, 1);
        assert_eq!(flags::DATA_VALID, 2);
    }
}
