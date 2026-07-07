//! Guest NIC MAC-address parsing and transparency validation.
//!
//! A guest must not be able to tell it is virtualized by inspecting its own NIC
//! (Phase 5.8 NIC-OUI check). The runtime authority for the OUI classification
//! lives in `enlil-devices` (`net::config::MacAddress::is_hypervisor_oui`); this
//! module carries a self-contained copy so `enlil-config` — a foundational crate
//! that must not depend on the higher-level device layer — can reject an unsafe
//! `mac` in a guest config *before* the hypervisor ever starts. Keep the two OUI
//! lists in sync.

/// OUIs registered to virtualization vendors. A guest NIC presenting one of
/// these is an immediate hypervisor tell to any in-guest detector (pafish /
/// al-khaser, or a plain `ip link`). Mirror of
/// `enlil-devices::net::config::MacAddress::HYPERVISOR_OUIS`.
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

/// Parse a MAC address written as six colon- or hyphen-separated hex octets
/// (e.g. `de:ad:be:ef:00:01` or `DE-AD-BE-EF-00-01`). Returns the raw six bytes.
///
/// # Errors
///
/// Returns a human-readable message if the string is not exactly six two-digit
/// hex octets separated by a single, consistent `:` or `-`.
pub fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let sep = if s.contains(':') {
        ':'
    } else if s.contains('-') {
        '-'
    } else {
        return Err(format!(
            "MAC '{s}' must be six hex octets separated by ':' or '-'"
        ));
    };
    let parts: Vec<&str> = s.split(sep).collect();
    if parts.len() != 6 {
        return Err(format!("MAC '{s}' has {} octets, expected 6", parts.len()));
    }
    let mut bytes = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        if part.len() != 2 {
            return Err(format!(
                "MAC '{s}': octet '{part}' must be exactly two hex digits"
            ));
        }
        bytes[i] = u8::from_str_radix(part, 16)
            .map_err(|_| format!("MAC '{s}': octet '{part}' is not valid hex"))?;
    }
    Ok(bytes)
}

/// Whether a 6-byte MAC's OUI (first three octets) is registered to a
/// virtualization vendor and would betray the hypervisor to a guest.
#[must_use]
pub fn is_hypervisor_oui(mac: [u8; 6]) -> bool {
    let oui = [mac[0], mac[1], mac[2]];
    HYPERVISOR_OUIS.contains(&oui)
}

/// Validate a guest NIC MAC string for use in a guest config.
///
/// Returns a rejection reason, or `None` if it is a safe, well-formed unicast
/// address that reveals no hypervisor. Rejects a malformed address, a
/// multicast/broadcast address (invalid as a NIC's own source address), and any
/// virtualization-vendor OUI (Phase 5.8 transparency).
#[must_use]
pub fn mac_rejection_reason(s: &str) -> Option<String> {
    let mac = match parse_mac(s) {
        Ok(m) => m,
        Err(e) => return Some(e),
    };
    // Bit 0 of the first octet marks a group (multicast/broadcast) address,
    // which a station may never use as its own source MAC.
    if mac[0] & 0x01 != 0 {
        return Some(format!(
            "MAC '{s}' is a multicast/broadcast address; a NIC MAC must be unicast"
        ));
    }
    if is_hypervisor_oui(mac) {
        return Some(format!(
            "MAC '{s}' uses a virtualization-vendor OUI ({:02x}:{:02x}:{:02x}), which \
             reveals the hypervisor to the guest; use a locally-administered MAC (set \
             bit 1 of the first octet) or a real physical-vendor OUI",
            mac[0], mac[1], mac[2]
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colon_and_hyphen_forms() {
        assert_eq!(
            parse_mac("de:ad:be:ef:00:01").unwrap(),
            [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]
        );
        assert_eq!(
            parse_mac("DE-AD-BE-EF-00-01").unwrap(),
            [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_mac("de:ad:be:ef:00").is_err()); // 5 octets
        assert!(parse_mac("de:ad:be:ef:00:01:02").is_err()); // 7 octets
        assert!(parse_mac("deadbeef0001").is_err()); // no separators
        assert!(parse_mac("de:ad:be:ef:0:01").is_err()); // single digit
        assert!(parse_mac("de:ad:be:ef:zz:01").is_err()); // non-hex
    }

    #[test]
    fn flags_hypervisor_ouis() {
        assert!(is_hypervisor_oui([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])); // KVM
        assert!(is_hypervisor_oui([0x00, 0x0C, 0x29, 0, 0, 1])); // VMware
        assert!(!is_hypervisor_oui([0xDE, 0xAD, 0xBE, 0xEF, 0, 1]));
    }

    #[test]
    fn rejection_reason_covers_each_case() {
        // Hypervisor OUI (KVM) rejected.
        assert!(mac_rejection_reason("52:54:00:12:34:56").is_some());
        // Multicast rejected (bit 0 of first octet set).
        assert!(mac_rejection_reason("01:00:5e:00:00:01").is_some());
        // Malformed rejected.
        assert!(mac_rejection_reason("not-a-mac").is_some());
        // A locally-administered, non-vendor unicast MAC is accepted.
        assert_eq!(mac_rejection_reason("de:ad:be:ef:00:01"), None);
        // A real physical-vendor unicast OUI (Intel 00:1b:21) is accepted.
        assert_eq!(mac_rejection_reason("00:1b:21:aa:bb:cc"), None);
    }
}
