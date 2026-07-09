//! Host-side CPUID hypervisor-detection primitives (item 5.8).
//!
//! These are the CPUID checks a guest-side agent (pafish / al-khaser style)
//! runs to decide it is running under a hypervisor — implemented and tested
//! host-side for two reasons: they are the shared logic the future in-guest
//! agent will use, and they let enlil verify **its own** synthesized CPUID does
//! not trip them (LOCKED PRINCIPLE 1 — transparency is the product). The
//! self-verification test in this module asserts the [`CpuidStealthTable`] is
//! clean: the hypervisor-present bit is cleared and leaf `0x40000000` exposes no
//! vendor signature.
//!
//! [`CpuidStealthTable`]: super::cpuid::CpuidStealthTable

/// A hypervisor a guest could identify from the CPUID `0x40000000` vendor leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HypervisorVendor {
    /// KVM — `"KVMKVMKVM\0\0\0"`.
    Kvm,
    /// Microsoft Hyper-V — `"Microsoft Hv"`.
    HyperV,
    /// VMware — `"VMwareVMware"`.
    Vmware,
    /// Oracle VirtualBox — `"VBoxVBoxVBox"`.
    VirtualBox,
    /// Xen — `"XenVMMXenVMM"`.
    Xen,
    /// QEMU TCG (software emulation) — `"TCGTCGTCGTCG"`.
    QemuTcg,
    /// FreeBSD bhyve — `"bhyve bhyve "`.
    Bhyve,
}

impl HypervisorVendor {
    /// The human-readable name of the hypervisor.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kvm => "KVM",
            Self::HyperV => "Microsoft Hyper-V",
            Self::Vmware => "VMware",
            Self::VirtualBox => "VirtualBox",
            Self::Xen => "Xen",
            Self::QemuTcg => "QEMU TCG",
            Self::Bhyve => "bhyve",
        }
    }
}

/// Whether CPUID.1:ECX bit 31 (the "hypervisor present" bit) is set — the
/// single most common VM tell, and the one enlil's stealth clears first.
#[must_use]
pub const fn cpuid_hypervisor_present(leaf1_ecx: u32) -> bool {
    leaf1_ecx & (1 << 31) != 0
}

/// Decode the 12-byte hypervisor vendor signature at CPUID leaf `0x40000000`
/// from its `EBX`, `ECX`, `EDX` registers (each a little-endian dword of the
/// ASCII string, in that order).
///
/// Returns the matched [`HypervisorVendor`], or `None` if the bytes are not a
/// known hypervisor signature — which is what real hardware returns (leaf
/// `0x40000000` is out of range, reading as leftover/zero), and what enlil's
/// stealth must present.
#[must_use]
pub fn hypervisor_vendor_from_signature(ebx: u32, ecx: u32, edx: u32) -> Option<HypervisorVendor> {
    let mut sig = [0u8; 12];
    sig[0..4].copy_from_slice(&ebx.to_le_bytes());
    sig[4..8].copy_from_slice(&ecx.to_le_bytes());
    sig[8..12].copy_from_slice(&edx.to_le_bytes());
    Some(match &sig {
        b"KVMKVMKVM\0\0\0" => HypervisorVendor::Kvm,
        b"Microsoft Hv" => HypervisorVendor::HyperV,
        b"VMwareVMware" => HypervisorVendor::Vmware,
        b"VBoxVBoxVBox" => HypervisorVendor::VirtualBox,
        b"XenVMMXenVMM" => HypervisorVendor::Xen,
        b"TCGTCGTCGTCG" => HypervisorVendor::QemuTcg,
        b"bhyve bhyve " => HypervisorVendor::Bhyve,
        _ => return None,
    })
}

/// Whether CPUID exposes any hypervisor tell: the present bit set, or a known
/// vendor signature at leaf `0x40000000`. Enlil's synthesized CPUID must make
/// this return `false` for a guest to believe it is on bare metal.
#[must_use]
pub fn cpuid_reveals_hypervisor(leaf1_ecx: u32, sig_ebx: u32, sig_ecx: u32, sig_edx: u32) -> bool {
    cpuid_hypervisor_present(leaf1_ecx)
        || hypervisor_vendor_from_signature(sig_ebx, sig_ecx, sig_edx).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack an ASCII signature into the `(ebx, ecx, edx)` a guest reads from
    /// CPUID leaf 0x40000000.
    fn sig(s: &[u8; 12]) -> (u32, u32, u32) {
        (
            u32::from_le_bytes(s[0..4].try_into().unwrap()),
            u32::from_le_bytes(s[4..8].try_into().unwrap()),
            u32::from_le_bytes(s[8..12].try_into().unwrap()),
        )
    }

    #[test]
    fn hypervisor_present_bit_is_bit_31() {
        assert!(cpuid_hypervisor_present(1 << 31));
        assert!(cpuid_hypervisor_present(0xFFFF_FFFF));
        assert!(!cpuid_hypervisor_present(0));
        assert!(!cpuid_hypervisor_present(0x7FFF_FFFF)); // all but bit 31
    }

    #[test]
    fn decodes_known_vendor_signatures() {
        let cases = [
            (b"KVMKVMKVM\0\0\0", HypervisorVendor::Kvm),
            (b"Microsoft Hv", HypervisorVendor::HyperV),
            (b"VMwareVMware", HypervisorVendor::Vmware),
            (b"VBoxVBoxVBox", HypervisorVendor::VirtualBox),
            (b"XenVMMXenVMM", HypervisorVendor::Xen),
            (b"TCGTCGTCGTCG", HypervisorVendor::QemuTcg),
            (b"bhyve bhyve ", HypervisorVendor::Bhyve),
        ];
        for (raw, want) in cases {
            let (ebx, ecx, edx) = sig(raw);
            assert_eq!(
                hypervisor_vendor_from_signature(ebx, ecx, edx),
                Some(want),
                "signature {:?}",
                core::str::from_utf8(raw).unwrap_or("<non-utf8>")
            );
            assert!(cpuid_reveals_hypervisor(0, ebx, ecx, edx));
        }
    }

    #[test]
    fn a_clean_leaf_reveals_nothing() {
        // Real hardware: bit 31 clear, leaf 0x40000000 reads as zero.
        assert_eq!(hypervisor_vendor_from_signature(0, 0, 0), None);
        assert!(!cpuid_reveals_hypervisor(0, 0, 0, 0));
        // A non-signature string (e.g. an Intel brand fragment) is not a vendor.
        let (ebx, ecx, edx) = sig(b"GenuineIntel");
        assert_eq!(hypervisor_vendor_from_signature(ebx, ecx, edx), None);
    }

    /// Enlil's own synthesized CPUID must be clean (LOCKED PRINCIPLE 1): the
    /// hypervisor-present bit cleared and no vendor signature at 0x40000000.
    #[test]
    fn enlils_stealth_cpuid_is_not_detectable() {
        use super::super::cpuid::{CpuidStealthConfig, CpuidStealthTable};

        let table = CpuidStealthTable::build(&CpuidStealthConfig::from_host(1, 1));

        let leaf1 = table.lookup(1, 0);
        assert!(
            !cpuid_hypervisor_present(leaf1.ecx),
            "CPUID.1:ECX[31] must be cleared by stealth"
        );

        let hv = table.lookup(0x4000_0000, 0);
        assert_eq!(
            hypervisor_vendor_from_signature(hv.ebx, hv.ecx, hv.edx),
            None,
            "leaf 0x40000000 must expose no hypervisor vendor signature"
        );
        assert!(
            !cpuid_reveals_hypervisor(leaf1.ecx, hv.ebx, hv.ecx, hv.edx),
            "enlil's synthesized CPUID trips a hypervisor detector"
        );
    }
}
