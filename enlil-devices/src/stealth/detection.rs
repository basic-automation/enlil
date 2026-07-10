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
    /// `VMware` — `"VMwareVMware"`.
    Vmware,
    /// Oracle `VirtualBox` — `"VBoxVBoxVBox"`.
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
/// vendor signature at leaf `0x40000000`.
///
/// Enlil's synthesized CPUID must make this return `false` for a guest to
/// believe it is on bare metal.
#[must_use]
pub fn cpuid_reveals_hypervisor(leaf1_ecx: u32, ebx: u32, ecx: u32, edx: u32) -> bool {
    cpuid_hypervisor_present(leaf1_ecx) || hypervisor_vendor_from_signature(ebx, ecx, edx).is_some()
}

/// The 12-byte CPU vendor string from CPUID leaf 0, assembled from its `EBX`,
/// `EDX`, `ECX` registers.
///
/// Note the non-obvious **EBX, EDX, ECX** order the x86 architecture uses for
/// this leaf (e.g. `"Genu"`, `"ineI"`, `"ntel"`).
#[must_use]
pub fn cpu_vendor_string(ebx: u32, edx: u32, ecx: u32) -> [u8; 12] {
    let mut s = [0u8; 12];
    s[0..4].copy_from_slice(&ebx.to_le_bytes());
    s[4..8].copy_from_slice(&edx.to_le_bytes());
    s[8..12].copy_from_slice(&ecx.to_le_bytes());
    s
}

/// Whether the CPUID leaf-0 vendor string names a genuine x86 CPU vendor
/// (`GenuineIntel` / `AuthenticAMD` / `HygonGenuine`).
///
/// enlil presents the real host CPU's vendor, so a guest that reads a blank or
/// unrecognized vendor string would have a tell. A future in-guest agent uses
/// this to flag a suspicious vendor; the host uses it to check the identity
/// install presents a real vendor.
#[must_use]
pub fn is_genuine_cpu_vendor(ebx: u32, edx: u32, ecx: u32) -> bool {
    matches!(
        &cpu_vendor_string(ebx, edx, ecx),
        b"GenuineIntel" | b"AuthenticAMD" | b"HygonGenuine"
    )
}

/// A conservative bare-metal ceiling (in TSC cycles) for a single serializing
/// CPUID bracketed by two RDTSCs.
///
/// On real hardware a serializing CPUID costs roughly a few hundred cycles; a
/// hypervisor that intercepts CPUID pays a VM-exit → emulate → VM-entry round
/// trip that inflates it into the thousands. `1000` sits above realistic
/// bare-metal noise yet well below a trapped exit, so a sample median above it
/// is a strong hypervisor tell. Callers measuring a different serializing
/// operation should pass their own ceiling to [`timing_reveals_hypervisor`].
pub const BARE_METAL_CPUID_CYCLE_CEILING: u64 = 1000;

/// The median of a sample of cycle-count deltas, or `None` for an empty sample.
///
/// The median (not the mean) is the right central statistic here: a timing
/// sample is riddled with upward outliers from SMIs, interrupts, and preemption,
/// which drag the mean up but leave the median — the typical instruction cost —
/// intact. Used by [`timing_reveals_hypervisor`]; also useful for an in-guest
/// agent's timing report.
#[must_use]
pub fn median_cycles(deltas: &[u64]) -> Option<u64> {
    if deltas.is_empty() {
        return None;
    }
    let mut sorted = deltas.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Some(sorted[mid])
    } else {
        // Average the two central values without overflowing u64.
        let (lo, hi) = (sorted[mid - 1], sorted[mid]);
        Some(lo + (hi - lo) / 2)
    }
}

/// Whether a sample of cycle-count deltas measured across a serializing,
/// VM-exit-prone instruction (typically CPUID bracketed by two RDTSCs) reveals a
/// trapping hypervisor.
///
/// Compares the sample's [`median_cycles`] against `bare_metal_ceiling` (see
/// [`BARE_METAL_CPUID_CYCLE_CEILING`]): a median above the ceiling means the
/// operation is being intercepted and emulated across a VM boundary. An empty
/// sample reveals nothing (`false`).
///
/// This is the timing check a pafish/al-khaser-style guest agent runs; enlil's
/// timing stealth (item 5.4 — TSC offsetting + APERF/MPERF shadowing) exists to
/// keep these deltas in the bare-metal range, and this primitive is what the
/// in-guest agent uses to confirm it.
#[must_use]
pub fn timing_reveals_hypervisor(deltas: &[u64], bare_metal_ceiling: u64) -> bool {
    median_cycles(deltas).is_some_and(|m| m > bare_metal_ceiling)
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

    #[test]
    fn cpu_vendor_string_uses_ebx_edx_ecx_order() {
        // "GenuineIntel": EBX="Genu", EDX="ineI", ECX="ntel".
        let (ebx, edx, ecx) = (
            u32::from_le_bytes(*b"Genu"),
            u32::from_le_bytes(*b"ineI"),
            u32::from_le_bytes(*b"ntel"),
        );
        assert_eq!(&cpu_vendor_string(ebx, edx, ecx), b"GenuineIntel");
        assert!(is_genuine_cpu_vendor(ebx, edx, ecx));
    }

    #[test]
    fn amd_vendor_is_genuine_but_a_blank_or_hypervisor_vendor_is_not() {
        let amd = (
            u32::from_le_bytes(*b"Auth"),
            u32::from_le_bytes(*b"enti"),
            u32::from_le_bytes(*b"cAMD"),
        );
        assert!(is_genuine_cpu_vendor(amd.0, amd.1, amd.2));
        // A blank vendor (some emulators) is not genuine.
        assert!(!is_genuine_cpu_vendor(0, 0, 0));
        // A hypervisor signature in the leaf-0 slot is not a CPU vendor.
        let kvm = (
            u32::from_le_bytes(*b"KVMK"),
            u32::from_le_bytes(*b"VMKV"),
            u32::from_le_bytes(*b"M\0\0\0"),
        );
        assert!(!is_genuine_cpu_vendor(kvm.0, kvm.1, kvm.2));
    }

    #[test]
    fn median_handles_odd_even_and_empty_samples() {
        assert_eq!(median_cycles(&[]), None);
        assert_eq!(median_cycles(&[42]), Some(42));
        // Odd count: middle after sorting.
        assert_eq!(median_cycles(&[300, 100, 200]), Some(200));
        // Even count: average of the two central values (250 and 300 → 275).
        assert_eq!(median_cycles(&[100, 250, 300, 900]), Some(275));
        // No overflow near u64::MAX for an even sample.
        assert_eq!(median_cycles(&[u64::MAX, u64::MAX - 2]), Some(u64::MAX - 1));
    }

    #[test]
    fn timing_flags_trapped_cpuid_but_not_bare_metal() {
        // Bare-metal-ish CPUID deltas (hundreds of cycles) with a couple of
        // SMI/interrupt outliers: the median stays low, so no hypervisor tell.
        let bare_metal = [180, 200, 190, 8000, 210, 195, 205, 30000, 185];
        assert!(!timing_reveals_hypervisor(
            &bare_metal,
            BARE_METAL_CPUID_CYCLE_CEILING
        ));
        assert!(median_cycles(&bare_metal).unwrap() < BARE_METAL_CPUID_CYCLE_CEILING);

        // A trapping hypervisor: every CPUID pays a VM-exit round trip (thousands
        // of cycles). The median is far above the ceiling → detected.
        let trapped = [4200, 3900, 4500, 4100, 3800, 4300, 4000];
        assert!(timing_reveals_hypervisor(
            &trapped,
            BARE_METAL_CPUID_CYCLE_CEILING
        ));

        // An empty sample concludes nothing.
        assert!(!timing_reveals_hypervisor(
            &[],
            BARE_METAL_CPUID_CYCLE_CEILING
        ));
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
