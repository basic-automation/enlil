//! `VirtIO` network device feature flags.
//!
//! Defined per the `VirtIO` 1.2 specification, Section 5.1.3.

use std::fmt;

/// `VirtIO` network feature bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetFeatures(u64);

impl NetFeatures {
    // ── Device-specific feature bits (bits 0–23) ──

    /// Device has checksum offload support.
    pub const CSUM: u64 = 1 << 0;
    /// Driver handles packets with partial checksum.
    pub const GUEST_CSUM: u64 = 1 << 1;
    /// Control channel offloads reconfiguration.
    pub const CTRL_GUEST_OFFLOADS: u64 = 1 << 2;
    /// Device maximum MTU reporting.
    pub const MTU: u64 = 1 << 3;
    /// Device has given MAC address.
    pub const MAC: u64 = 1 << 5;
    /// Driver can receive `TSOv4`.
    pub const GUEST_TSO4: u64 = 1 << 7;
    /// Driver can receive `TSOv6`.
    pub const GUEST_TSO6: u64 = 1 << 8;
    /// Driver can receive TSO with ECN.
    pub const GUEST_ECN: u64 = 1 << 9;
    /// Driver can receive UFO.
    pub const GUEST_UFO: u64 = 1 << 10;
    /// Device can receive `TSOv4`.
    pub const HOST_TSO4: u64 = 1 << 11;
    /// Device can receive `TSOv6`.
    pub const HOST_TSO6: u64 = 1 << 12;
    /// Device can receive TSO with ECN.
    pub const HOST_ECN: u64 = 1 << 13;
    /// Device can receive UFO.
    pub const HOST_UFO: u64 = 1 << 14;
    /// Driver can merge receive buffers.
    pub const MRG_RXBUF: u64 = 1 << 15;
    /// Configuration status field available.
    pub const STATUS: u64 = 1 << 16;
    /// Control channel available.
    pub const CTRL_VQ: u64 = 1 << 17;
    /// Control channel RX mode support.
    pub const CTRL_RX: u64 = 1 << 18;
    /// Control channel VLAN filtering.
    pub const CTRL_VLAN: u64 = 1 << 19;
    /// Guest can send gratuitous packets.
    pub const GUEST_ANNOUNCE: u64 = 1 << 21;
    /// Device supports multiqueue.
    pub const MQ: u64 = 1 << 22;

    // ── VirtIO generic feature bits (bits 24–37) ──

    /// Indicates compliance with `VirtIO` 1.0+.
    pub const VERSION_1: u64 = 1 << 32;

    /// Default features offered by our device.
    pub const DEFAULT: u64 =
        Self::MAC | Self::STATUS | Self::MRG_RXBUF | Self::CSUM | Self::GUEST_CSUM;

    /// Create from raw bits.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Get raw bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Check if a feature is set.
    #[must_use]
    pub const fn contains(self, feature: u64) -> bool {
        (self.0 & feature) == feature
    }

    /// Set a feature bit.
    #[must_use]
    pub const fn with(self, feature: u64) -> Self {
        Self(self.0 | feature)
    }

    /// Clear a feature bit.
    #[must_use]
    pub const fn without(self, feature: u64) -> Self {
        Self(self.0 & !feature)
    }

    /// Negotiate features: returns the intersection of offered and requested.
    #[must_use]
    pub const fn negotiate(offered: Self, requested: Self) -> Self {
        Self(offered.0 & requested.0)
    }

    /// Empty feature set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }
}

impl Default for NetFeatures {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

impl fmt::Display for NetFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut features = Vec::new();
        if self.contains(Self::CSUM) {
            features.push("CSUM");
        }
        if self.contains(Self::GUEST_CSUM) {
            features.push("GUEST_CSUM");
        }
        if self.contains(Self::MAC) {
            features.push("MAC");
        }
        if self.contains(Self::MRG_RXBUF) {
            features.push("MRG_RXBUF");
        }
        if self.contains(Self::STATUS) {
            features.push("STATUS");
        }
        if self.contains(Self::VERSION_1) {
            features.push("VERSION_1");
        }
        write!(f, "[{}]", features.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_features() {
        let f = NetFeatures::default();
        assert!(f.contains(NetFeatures::MAC));
        assert!(f.contains(NetFeatures::STATUS));
        assert!(f.contains(NetFeatures::MRG_RXBUF));
        assert!(f.contains(NetFeatures::CSUM));
        assert!(f.contains(NetFeatures::GUEST_CSUM));
        assert!(!f.contains(NetFeatures::HOST_TSO4));
    }

    #[test]
    fn negotiate_features() {
        let offered = NetFeatures::default();
        let requested = NetFeatures::from_bits(NetFeatures::MAC | NetFeatures::CSUM);
        let negotiated = NetFeatures::negotiate(offered, requested);
        assert!(negotiated.contains(NetFeatures::MAC));
        assert!(negotiated.contains(NetFeatures::CSUM));
        assert!(!negotiated.contains(NetFeatures::MRG_RXBUF));
    }

    #[test]
    fn with_and_without() {
        let f = NetFeatures::empty()
            .with(NetFeatures::MAC)
            .with(NetFeatures::STATUS);
        assert!(f.contains(NetFeatures::MAC));
        assert!(f.contains(NetFeatures::STATUS));

        let f2 = f.without(NetFeatures::MAC);
        assert!(!f2.contains(NetFeatures::MAC));
        assert!(f2.contains(NetFeatures::STATUS));
    }

    #[test]
    fn display() {
        let f = NetFeatures::from_bits(NetFeatures::MAC | NetFeatures::CSUM);
        let s = format!("{f}");
        assert!(s.contains("MAC"));
        assert!(s.contains("CSUM"));
    }
}
