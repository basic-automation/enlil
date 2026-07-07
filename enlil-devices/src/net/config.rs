//! Network device configuration types.

/// A 6-byte MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddress(pub [u8; 6]);

impl MacAddress {
    /// Check if this is a multicast address (bit 0 of first octet set).
    #[must_use]
    pub const fn is_multicast(self) -> bool {
        self.0[0] & 0x01 != 0
    }

    /// Check if this is a broadcast address (all `0xFF`).
    #[must_use]
    pub fn is_broadcast(self) -> bool {
        self.0 == [0xFF; 6]
    }

    /// Check if this is a unicast address.
    #[must_use]
    pub const fn is_unicast(self) -> bool {
        !self.is_multicast()
    }

    /// Get the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }

    /// The 24-bit Organizationally Unique Identifier (the first three octets).
    #[must_use]
    pub const fn oui(self) -> [u8; 3] {
        [self.0[0], self.0[1], self.0[2]]
    }

    /// Whether the address is locally administered (bit 1 of the first octet).
    /// A locally-administered address carries no registered vendor identity, so
    /// it reveals nothing about the underlying hardware — the safe default for a
    /// synthesized guest NIC.
    #[must_use]
    pub const fn is_locally_administered(self) -> bool {
        self.0[0] & 0x02 != 0
    }

    /// OUIs registered to virtualization vendors, which a guest-side detector
    /// (pafish/al-khaser, and plain `ip link`) treats as a hypervisor tell.
    /// A synthesized guest NIC must never present one of these.
    const HYPERVISOR_OUIS: [[u8; 3]; 10] = [
        [0x52, 0x54, 0x00], // QEMU / KVM virtio-net
        [0x00, 0x16, 0x3E], // Xen
        [0x00, 0x15, 0x5D], // Microsoft Hyper-V
        [0x00, 0x1C, 0x42], // Parallels
        [0x08, 0x00, 0x27], // Oracle VirtualBox
        [0x0A, 0x00, 0x27], // VirtualBox host-only
        [0x00, 0x05, 0x69], // VMware
        [0x00, 0x0C, 0x29], // VMware
        [0x00, 0x1C, 0x14], // VMware
        [0x00, 0x50, 0x56], // VMware
    ];

    /// Whether this address's OUI is registered to a virtualization vendor and
    /// so would betray the hypervisor to a guest inspecting its own NIC (Phase
    /// 5.8 NIC-OUI check). Config validation should reject a guest MAC for which
    /// this is `true`; prefer a locally-administered address
    /// ([`is_locally_administered`](Self::is_locally_administered)) or a real
    /// physical-vendor OUI instead.
    #[must_use]
    pub fn is_hypervisor_oui(self) -> bool {
        let oui = self.oui();
        Self::HYPERVISOR_OUIS.contains(&oui)
    }

    /// Synthesize a stable, transparent guest NIC MAC from the guest name — the
    /// safe default when a guest config leaves its NIC MAC unset (the complement
    /// to `enlil-config`'s hypervisor-OUI rejection, item 5.8).
    ///
    /// Deterministic: a domain-separated SHA-256 of the name (the same scheme
    /// [`crate::tpm::VirtualTpm::seed_for_guest`] uses), so a guest keeps the same
    /// MAC across reboots. Guaranteed transparent — the address is unicast (bit 0
    /// clear), locally administered (bit 1 set, so it carries no registered
    /// vendor identity), and never a virtualization-vendor OUI (a QEMU/Xen/VMware/…
    /// prefix would be an immediate hypervisor tell to a guest inspecting its own
    /// NIC).
    #[must_use]
    pub fn synthesize_for_guest(name: &str) -> Self {
        let mut input = b"enlil-guest-mac:".to_vec();
        input.extend_from_slice(name.as_bytes());
        let digest = crate::crypto::sha256(&input);
        let mut octets = [0u8; 6];
        octets.copy_from_slice(&digest[..6]);
        // Force unicast (clear bit 0) + locally administered (set bit 1).
        octets[0] = (octets[0] & 0xFC) | 0x02;
        let mut mac = Self(octets);
        // A locally-administered first octet can still land on a hypervisor OUI
        // that is itself locally administered (QEMU 52:54:00, VirtualBox host-only
        // 0A:00:27). If it does, bump the first octet by 0x04 — which preserves
        // the unicast (bit 0) and locally-administered (bit 1) bits and never
        // carries into them — until the OUI is transparent. The hypervisor OUI
        // set is finite, so this terminates.
        while mac.is_hypervisor_oui() {
            mac.0[0] = mac.0[0].wrapping_add(0x04);
        }
        mac
    }
}

impl std::fmt::Display for MacAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// Configuration for a virtual network device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetDeviceConfig {
    /// Device name (e.g. "vnet0").
    pub name: String,
    /// MAC address for this device.
    pub mac: MacAddress,
    /// Optional bridge to connect to.
    pub bridge: Option<String>,
    /// MTU in bytes.
    pub mtu: u16,
    /// Whether to enable checksum offload.
    pub checksum_offload: bool,
    /// Whether to enable TSO.
    pub tso: bool,
    /// Number of TX queues.
    pub tx_queues: u8,
    /// Number of RX queues.
    pub rx_queues: u8,
}

impl Default for NetDeviceConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            mac: MacAddress([0; 6]),
            bridge: None,
            mtu: 1500,
            checksum_offload: true,
            tso: false,
            tx_queues: 1,
            rx_queues: 1,
        }
    }
}

impl NetDeviceConfig {
    /// Create a new network device configuration.
    #[must_use]
    pub fn new(name: &str, mac: MacAddress) -> Self {
        Self {
            name: name.to_string(),
            mac,
            ..Self::default()
        }
    }

    /// Create a bridged network device configuration.
    #[must_use]
    pub fn bridge(name: &str, mac: MacAddress, bridge: &str) -> Self {
        Self {
            name: name.to_string(),
            mac,
            bridge: Some(bridge.to_string()),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mac_address() {
        let mac = MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert!(mac.is_unicast());
        assert!(!mac.is_multicast());
        assert!(!mac.is_broadcast());

        let broadcast = MacAddress([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(broadcast.is_broadcast());
        assert!(broadcast.is_multicast());

        let multicast = MacAddress([0x01, 0x00, 0x5E, 0x00, 0x00, 0x01]);
        assert!(multicast.is_multicast());
        assert!(!multicast.is_broadcast());
    }

    #[test]
    fn test_mac_display() {
        let mac = MacAddress([0x02, 0xAB, 0xCD, 0xEF, 0x01, 0x23]);
        assert_eq!(format!("{mac}"), "02:ab:cd:ef:01:23");
    }

    #[test]
    fn flags_hypervisor_ouis_and_clears_transparent_ones() {
        // QEMU/KVM's virtio-net OUI is the classic tell.
        assert!(MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]).is_hypervisor_oui());
        // A few more vendors a detector checks.
        assert!(MacAddress([0x00, 0x16, 0x3E, 0, 0, 1]).is_hypervisor_oui()); // Xen
        assert!(MacAddress([0x00, 0x15, 0x5D, 0, 0, 1]).is_hypervisor_oui()); // Hyper-V
        assert!(MacAddress([0x08, 0x00, 0x27, 0, 0, 1]).is_hypervisor_oui()); // VirtualBox
        assert!(MacAddress([0x00, 0x0C, 0x29, 0, 0, 1]).is_hypervisor_oui()); // VMware

        // A locally-administered address betrays no vendor: transparent.
        let la = MacAddress([0x02, 0xAB, 0xCD, 0x01, 0x02, 0x03]);
        assert!(la.is_locally_administered());
        assert!(!la.is_hypervisor_oui());
        assert_eq!(la.oui(), [0x02, 0xAB, 0xCD]);

        // A real physical-vendor OUI (Dell) is not flagged, and is globally
        // administered (bit 1 clear).
        let dell = MacAddress([0x00, 0x14, 0x22, 0x11, 0x22, 0x33]);
        assert!(!dell.is_hypervisor_oui());
        assert!(!dell.is_locally_administered());
    }

    #[test]
    fn synthesized_guest_mac_is_deterministic_and_transparent() {
        let a = MacAddress::synthesize_for_guest("windows-11");
        // Deterministic: same name → same MAC (stable across reboots).
        assert_eq!(a, MacAddress::synthesize_for_guest("windows-11"));
        // Distinct names → distinct MACs (no collision for a couple of names).
        assert_ne!(a, MacAddress::synthesize_for_guest("ubuntu"));
        // Transparent: unicast, locally administered, never a hypervisor OUI.
        for name in ["windows-11", "ubuntu", "", "guest-Ω", "52-54-00"] {
            let mac = MacAddress::synthesize_for_guest(name);
            assert!(mac.is_unicast(), "{name}: must be unicast");
            assert!(
                mac.is_locally_administered(),
                "{name}: must be locally administered"
            );
            assert!(
                !mac.is_hypervisor_oui(),
                "{name}: must not present a hypervisor OUI, got {mac}"
            );
        }
    }

    #[test]
    fn synthesize_escapes_a_locally_administered_hypervisor_oui() {
        // The escape loop preserves the unicast + locally-administered bits while
        // bumping off any hypervisor OUI. Exercise it directly on the two
        // locally-administered hypervisor prefixes.
        for start in [
            MacAddress([0x52, 0x54, 0x00, 1, 2, 3]), // QEMU (locally administered)
            MacAddress([0x0A, 0x00, 0x27, 1, 2, 3]), // VirtualBox host-only
        ] {
            assert!(start.is_hypervisor_oui() && start.is_locally_administered());
            let mut mac = start;
            while mac.is_hypervisor_oui() {
                mac.0[0] = mac.0[0].wrapping_add(0x04);
            }
            assert!(mac.is_unicast());
            assert!(mac.is_locally_administered());
            assert!(!mac.is_hypervisor_oui());
        }
    }

    #[test]
    fn test_config_default() {
        let config = NetDeviceConfig::new("vnet0", MacAddress([0x02, 0, 0, 0, 0, 1]));
        assert_eq!(config.name, "vnet0");
        assert_eq!(config.mtu, 1500);
        assert!(config.bridge.is_none());
    }

    #[test]
    fn test_config_bridge() {
        let config = NetDeviceConfig::bridge("vnet0", MacAddress([0x02, 0, 0, 0, 0, 1]), "br0");
        assert_eq!(config.bridge.as_deref(), Some("br0"));
    }
}
