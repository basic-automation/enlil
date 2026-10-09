//! Curated pafish + al-khaser check inventories (roadmap item 5.8, T-5.4).
//!
//! The in-guest detection agent (T-5.2, `bridge_agent::stealth`) covers
//! enlil's *own* checklist, but the industry-standard suites —
//! [pafish](https://github.com/a0rtega/pafish) (Paranoid Fish, by Alberto
//! Ortega) and [al-khaser](https://github.com/LordNoteworthy/al-khaser) (by
//! Hasherezade) — are what the outside world runs to decide a machine is a
//! VM. Enlil's 1.0 transparency claim needs those suites to run clean, so
//! this module curates their documented check families and maps **every**
//! check to its disposition against enlil's stealth:
//!
//! - [`Coverage::StealthPrimitive`] — defeated by an enlil stealth primitive
//!   (the named path is the mechanism);
//! - [`Coverage::AgentCheck`] — covered by an existing in-guest agent check
//!   (the named `bridge_agent::stealth` check);
//! - [`Coverage::ByConstruction`] — the tell cannot exist because enlil
//!   synthesizes the artifact itself (own ACPI/SMBIOS, passthrough disks);
//! - [`Coverage::WindowsGuestRun`] — not automatable without a full Windows
//!   guest: validated by running the real suite binary there (see the
//!   anti-cheat validation doc's runbook).
//!
//! [`evaluate_both_suites`] joins a live agent run onto both inventories and
//! produces a [`SuiteRunReport`]: the machine-readable form of "pafish and
//! al-khaser run clean in an enlil guest".
//!

/// Which anti-VM suite a check belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite {
    /// pafish (Paranoid Fish) by Alberto Ortega.
    Pafish,
    /// al-khaser by Hasherezade.
    AlKhaser,
}

impl Suite {
    /// The suite's short name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pafish => "pafish",
            Self::AlKhaser => "al-khaser",
        }
    }

    /// The suite's upstream project URL.
    #[must_use]
    pub const fn upstream(self) -> &'static str {
        match self {
            Self::Pafish => "https://github.com/a0rtega/pafish",
            Self::AlKhaser => "https://github.com/LordNoteworthy/al-khaser",
        }
    }
}

/// The family a suite check belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckCategory {
    /// CPUID / CPU-identity checks.
    Cpu,
    /// RDTSC / tick-count timing checks.
    Timing,
    /// ACPI / SMBIOS / BIOS firmware-string checks.
    Firmware,
    /// Windows registry artifact checks.
    Registry,
    /// PCI / NIC / disk device-enumeration checks.
    Devices,
    /// Guest driver-presence checks.
    Drivers,
    /// Guest process-enumeration checks.
    Processes,
    /// RAM / disk-size / uptime environment checks.
    Environment,
    /// Sandbox-product artifact checks (Sandboxie, Cuckoo, ...).
    Sandbox,
    /// Anti-debugger checks.
    Debugger,
    /// Human-interaction checks (mouse, recent documents, ...).
    HumanInteraction,
}

impl CheckCategory {
    /// The category's short name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Timing => "timing",
            Self::Firmware => "firmware",
            Self::Registry => "registry",
            Self::Devices => "devices",
            Self::Drivers => "drivers",
            Self::Processes => "processes",
            Self::Environment => "environment",
            Self::Sandbox => "sandbox",
            Self::Debugger => "debugger",
            Self::HumanInteraction => "human-interaction",
        }
    }
}

/// How enlil defeats (or still must validate) one suite check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Defeated by an enlil stealth primitive; the `&str` names it, e.g.
    /// `"enlil_devices::stealth::cpuid::CpuidStealthTable"`.
    StealthPrimitive(&'static str),
    /// Covered by an existing in-guest agent check; the `&str` is the
    /// `bridge_agent::stealth` check name, e.g. `"cpuid/hypervisor-present-bit"`.
    AgentCheck(&'static str),
    /// The tell cannot exist: enlil synthesizes the artifact (its own ACPI
    /// tables, SMBIOS, DSDT; passthrough disks) instead of inheriting a
    /// hypervisor's. The `&str` explains why.
    ByConstruction(&'static str),
    /// Not automatable without a full Windows guest: validated by running the
    /// real suite binary there. The `&str` is the runbook note (provisioning
    /// requirement or validation step).
    WindowsGuestRun(&'static str),
}

/// One check from the pafish or al-khaser suite and its disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuiteCheck {
    /// Which suite this check comes from.
    pub suite: Suite,
    /// Stable dotted id, e.g. `"cpu/hypervisor-present-bit"`.
    pub id: &'static str,
    /// The check family.
    pub category: CheckCategory,
    /// Short human-readable name.
    pub name: &'static str,
    /// What the check does, in enlil's own words.
    pub description: &'static str,
    /// How enlil defeats it, or what still needs a Windows guest run.
    pub coverage: Coverage,
}

/// The pafish check inventory and each check's disposition against enlil.
#[must_use]
pub const fn pafish_checks() -> &'static [SuiteCheck] {
    &PAFISH_CHECKS
}

/// The al-khaser check inventory and each check's disposition against enlil.
#[must_use]
pub const fn alkhaser_checks() -> &'static [SuiteCheck] {
    &ALKHASER_CHECKS
}

/// The inventory for one suite.
#[must_use]
pub const fn suite_checks(suite: Suite) -> &'static [SuiteCheck] {
    match suite {
        Suite::Pafish => pafish_checks(),
        Suite::AlKhaser => alkhaser_checks(),
    }
}

const PAFISH: Suite = Suite::Pafish;
const ALKHASER: Suite = Suite::AlKhaser;

/// pafish's documented check families, mapped onto enlil.
const PAFISH_CHECKS: [SuiteCheck; 26] = [
    SuiteCheck {
        suite: PAFISH,
        id: "cpu/hypervisor-present-bit",
        category: CheckCategory::Cpu,
        name: "CPUID hypervisor-present bit",
        description: "CPUID leaf 1 ECX bit 31 set means a hypervisor is present; \
            pafish's first and most basic check.",
        coverage: Coverage::AgentCheck("cpuid/hypervisor-present-bit"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "cpu/hypervisor-vendor-signature",
        category: CheckCategory::Cpu,
        name: "CPUID 0x40000000 vendor signature",
        description: "Compares the leaf-0x40000000 signature against KVMKVMKVM, \
            Microsoft Hv, VMwareVMware, VBoxVBoxVBox, XenVMMXenVMM.",
        coverage: Coverage::AgentCheck("cpuid/vendor-signature"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "cpu/genuine-vendor-string",
        category: CheckCategory::Cpu,
        name: "Genuine CPU vendor string",
        description: "pafish flags a leaf-0 vendor that is not GenuineIntel; enlil \
            passes the host's real vendor through.",
        coverage: Coverage::AgentCheck("cpuid/cpu-vendor"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "cpu/kvm-magic-value",
        category: CheckCategory::Cpu,
        name: "KVM paravirt magic value",
        description: "KVM's 0x4B4D564B magic in CPUID 0x40000000 EAX. Enlil hides \
            the whole 0x40000000 range as out-of-range, so no magic is visible.",
        coverage: Coverage::StealthPrimitive(
            "enlil_devices::stealth::cpuid::CpuidStealthTable (0x40000000 region hidden)",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "cpu/vmware-backdoor-port",
        category: CheckCategory::Cpu,
        name: "VMware backdoor I/O port",
        description: "The VMware backdoor (port 0x5658, magic VMXh). Enlil has no \
            VMware backdoor; the port answers as bare hardware.",
        coverage: Coverage::ByConstruction("no VMware backdoor exists in enlil's device model"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "cpu/virtualpc-invalid-opcode",
        category: CheckCategory::Cpu,
        name: "VirtualPC invalid opcode",
        description: "VirtualPC answered a specific invalid opcode instead of \
            faulting; enlil's CPU is the real host CPU and faults normally.",
        coverage: Coverage::ByConstruction("guest executes on the real host CPU"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "timing/rdtsc-vmexit",
        category: CheckCategory::Timing,
        name: "RDTSC CPUID-bracket timing",
        description: "RDTSC, CPUID, RDTSC: a VMEXIT round trip inflates the delta \
            into the thousands of cycles on a trapping hypervisor.",
        coverage: Coverage::AgentCheck("timing/rdtsc-cpuid"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "timing/rdtsc-diff",
        category: CheckCategory::Timing,
        name: "Back-to-back RDTSC delta",
        description: "Two consecutive RDTSCs should differ by tens of cycles; \
            emulated TSCs drift. Defeated by TSC offsetting (no exit on read).",
        coverage: Coverage::StealthPrimitive(
            "enlil_devices::stealth::timing (TSC offsetting, no RDTSC exit)",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "timing/sleep-tickcount",
        category: CheckCategory::Timing,
        name: "Sleep vs GetTickCount discrepancy",
        description: "Sleep(500) then compare GetTickCount before/after; a large \
            discrepancy implies time dilation. Enlil never dilates the guest \
            clock: the guest TSC runs at the real host rate (offset, not \
            scaled) and timer ticks are delivered by the real LAPIC.",
        coverage: Coverage::StealthPrimitive(
            "enlil_devices::stealth::timing (TSC offsetting; guest clock not dilated)",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "firmware/acpi-dsdt-strings",
        category: CheckCategory::Firmware,
        name: "ACPI DSDT OEM strings",
        description: "Reads the DSDT via GetSystemFirmwareTable and flags VBOX__ \
            or BOCHS signatures. Enlil synthesizes its own DSDT with AMI OEM IDs.",
        coverage: Coverage::ByConstruction(
            "enlil_devices::acpi generates the DSDT with OemInfo::ami(); no VBOX/BOCHS strings exist",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "firmware/smbios-strings",
        category: CheckCategory::Firmware,
        name: "SMBIOS manufacturer/product strings",
        description: "Flags innotek/VirtualBox/VMware/QEMU in the DMI tables. \
            Enlil synthesizes SMBIOS with physical-hardware-like defaults.",
        coverage: Coverage::ByConstruction(
            "enlil_devices::smbios synthesizes the tables (ASUS/G.Skill-class defaults); no VM vendor strings",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "firmware/bios-version-strings",
        category: CheckCategory::Firmware,
        name: "System/Video BIOS version strings",
        description: "Registry SystemBiosVersion containing VBOX, \
            VideoBiosVersion containing VIRTUALBOX. Enlil's firmware strings are \
            synthesized, never VBOX-derived.",
        coverage: Coverage::ByConstruction(
            "synthesized SMBIOS/ACPI firmware strings contain no VBOX markers",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "registry/vbox-guest-keys",
        category: CheckCategory::Registry,
        name: "VirtualBox guest keys",
        description: "HKLM VBoxGuestAdditions / VBOX service keys. Enlil installs \
            no guest additions; the keys cannot exist unless the operator adds them.",
        coverage: Coverage::AgentCheck("registry/hypervisor-keys"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "registry/vmware-tools-keys",
        category: CheckCategory::Registry,
        name: "VMware Tools keys",
        description: "HKLM vmtools / vmhgfs service keys. Same posture as the \
            VirtualBox keys: enlil ships no guest tools.",
        coverage: Coverage::AgentCheck("registry/hypervisor-keys"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "registry/system-manufacturer",
        category: CheckCategory::Registry,
        name: "System manufacturer/product registry",
        description: "HKLM\\HARDWARE\\DESCRIPTION\\System manufacturer/product \
            naming a VM vendor (innotek, QEMU, KVM). Backed by enlil's \
            synthesized SMBIOS, so the values are physical-hardware-like.",
        coverage: Coverage::AgentCheck("registry/hypervisor-keys"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "devices/pci-ids",
        category: CheckCategory::Devices,
        name: "PCI device-ID enumeration",
        description: "Flags VMware SVGA/VMXNET3, VirtualBox, Xen, Hyper-V VMBus, \
            QXL, and any VirtIO (0x1AF4) device IDs on the PCI bus.",
        coverage: Coverage::AgentCheck("devices/pci-ids"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "devices/nic-oui",
        category: CheckCategory::Devices,
        name: "NIC MAC vendor OUIs",
        description: "Flags VirtualBox (08:00:27), QEMU (52:54:00), VMware \
            (00:05:69/00:0C:29/00:50:56), Parallels (00:1C:42) MAC OUIs.",
        coverage: Coverage::AgentCheck("devices/nic-oui"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "devices/disk-model",
        category: CheckCategory::Devices,
        name: "Disk model strings",
        description: "WMI disk-model checks flag VBOX HARDDISK / QEMU HARDDISK. \
            Enlil never emulates a disk: storage is VFIO/NVMe passthrough, so \
            the guest reads the physical disk's genuine IDENTIFY strings.",
        coverage: Coverage::ByConstruction(
            "enlil_devices::storage is passthrough-only; no emulated disk model strings exist",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "drivers/virtual-drivers-absent",
        category: CheckCategory::Drivers,
        name: "Virtualization driver files",
        description: "Looks for vbox*.sys / vmmouse.sys-style driver files in \
            System32\\drivers. Enlil installs no guest drivers.",
        coverage: Coverage::WindowsGuestRun(
            "run pafish.exe in the Windows guest; enlil ships no guest drivers",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "processes/vm-agent-processes",
        category: CheckCategory::Processes,
        name: "VM agent processes",
        description: "Enumerates vboxservice.exe, vboxtray.exe, vmtoolsd.exe, \
            vmacthlp.exe. Enlil runs no in-guest agents of its own.",
        coverage: Coverage::WindowsGuestRun(
            "run pafish.exe in the Windows guest; no enlil processes exist in-guest",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "environment/processor-count",
        category: CheckCategory::Environment,
        name: "Processor count",
        description: "Flags fewer than 2 processors as a sandbox. Enlil passes \
            the real topology through, so this is a provisioning requirement: \
            guests must be configured with >= 2 vCPUs.",
        coverage: Coverage::WindowsGuestRun(
            "provision the Windows guest with >= 2 vCPUs; topology is passed through",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "environment/disk-size",
        category: CheckCategory::Environment,
        name: "Disk size",
        description: "Flags a suspiciously small disk as a sandbox. The disk is \
            the real passed-through device; size is whatever the operator attached.",
        coverage: Coverage::WindowsGuestRun(
            "attach a realistically sized disk to the Windows guest",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "environment/memory-size",
        category: CheckCategory::Environment,
        name: "Memory size",
        description: "Flags tiny RAM as a sandbox. Guest RAM is operator-sized \
            real memory; provision >= 2 GiB.",
        coverage: Coverage::WindowsGuestRun("provision the Windows guest with >= 2 GiB RAM"),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "human-interaction/mouse-movement",
        category: CheckCategory::HumanInteraction,
        name: "Mouse movement",
        description: "GetCursorPos, Sleep(10s), GetCursorPos: a stationary cursor \
            implies automation. Needs a real interactive Windows session.",
        coverage: Coverage::WindowsGuestRun(
            "run pafish.exe in an interactive Windows guest session with a live cursor",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "debugger/is-debugger-present",
        category: CheckCategory::Debugger,
        name: "IsDebuggerPresent",
        description: "Flags an attached debugger. Enlil attaches no debugger to \
            the guest; validated in the Windows guest run.",
        coverage: Coverage::WindowsGuestRun(
            "run pafish.exe in the Windows guest with no debugger attached",
        ),
    },
    SuiteCheck {
        suite: PAFISH,
        id: "debugger/nt-global-flag",
        category: CheckCategory::Debugger,
        name: "PEB NtGlobalFlag / heap flags",
        description: "Flags debugger-induced PEB/heap flags. No enlil mechanism \
            sets them; validated in the Windows guest run.",
        coverage: Coverage::WindowsGuestRun("run pafish.exe in the Windows guest"),
    },
];

/// al-khaser's documented check families, mapped onto enlil.
const ALKHASER_CHECKS: [SuiteCheck; 30] = [
    SuiteCheck {
        suite: ALKHASER,
        id: "cpu/hypervisor-present-bit",
        category: CheckCategory::Cpu,
        name: "CPUID hypervisor-present bit",
        description: "CPUID leaf 1 ECX bit 31. Same tell as pafish's first check.",
        coverage: Coverage::AgentCheck("cpuid/hypervisor-present-bit"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "cpu/hypervisor-vendor-signature",
        category: CheckCategory::Cpu,
        name: "CPUID 0x40000000 vendor signature",
        description: "Decodes the hypervisor vendor from the 0x40000000 signature \
            (KVM, Hyper-V, VMware, VirtualBox, Xen).",
        coverage: Coverage::AgentCheck("cpuid/vendor-signature"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "cpu/hypervisor-magic-values",
        category: CheckCategory::Cpu,
        name: "Hypervisor magic values",
        description: "KVM (0x4B4D564B) / Hyper-V interface magic values probed via \
            hypervisor CPUID leaves. Enlil hides the whole hypervisor leaf range.",
        coverage: Coverage::StealthPrimitive(
            "enlil_devices::stealth::cpuid::CpuidStealthTable (0x40000000 region hidden)",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "cpu/genuine-vendor-string",
        category: CheckCategory::Cpu,
        name: "Genuine CPU vendor string",
        description: "Leaf-0 vendor must be a genuine CPU vendor; enlil passes the \
            host's real vendor through.",
        coverage: Coverage::AgentCheck("cpuid/cpu-vendor"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "cpu/thread-count",
        category: CheckCategory::Cpu,
        name: "CPU thread count",
        description: "Flags a 1-2 thread machine as analysis hardware. The guest \
            sees exactly its provisioned vCPU topology (never the host's), so \
            provisioning >= 2 vCPUs passes this check.",
        coverage: Coverage::StealthPrimitive(
            "enlil_devices::stealth::cpuid::CpuidStealthTable (guest sees its provisioned vCPU topology)",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "timing/rdtsc-vmexit",
        category: CheckCategory::Timing,
        name: "RDTSC VMEXIT timing",
        description: "RDTSC deltas across a serializing instruction reveal a \
            trapping hypervisor.",
        coverage: Coverage::AgentCheck("timing/rdtsc-cpuid"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "timing/rdtsc-diff",
        category: CheckCategory::Timing,
        name: "RDTSC consecutive-read delta",
        description: "Two back-to-back RDTSCs must stay in the tens-of-cycles \
            range; defeated by TSC offsetting with no RDTSC exit.",
        coverage: Coverage::StealthPrimitive(
            "enlil_devices::stealth::timing (TSC offsetting, no RDTSC exit)",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "timing/qpc-vs-tickcount",
        category: CheckCategory::Timing,
        name: "QPC vs GetTickCount ratio",
        description: "QueryPerformanceCounter and GetTickCount must agree; \
            divergence implies clock virtualization. Both trace to the \
            undilated guest TSC / real LAPIC tick under enlil.",
        coverage: Coverage::StealthPrimitive(
            "enlil_devices::stealth::timing (TSC offsetting; guest clock not dilated)",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "firmware/smbios-strings",
        category: CheckCategory::Firmware,
        name: "SMBIOS firmware-table strings",
        description: "Reads the raw SMBIOS table and flags VBOX/VMWARE/QEMU \
            strings. Enlil synthesizes its own SMBIOS.",
        coverage: Coverage::ByConstruction(
            "enlil_devices::smbios synthesizes the tables; no VM vendor strings",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "firmware/disk-serial",
        category: CheckCategory::Firmware,
        name: "Disk serial number",
        description: "Flags VBOX-style disk serials. Passthrough disks expose the \
            physical disk's genuine serial.",
        coverage: Coverage::ByConstruction(
            "enlil_devices::storage is passthrough-only; the guest sees the real disk serial",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "registry/vbox-guest-keys",
        category: CheckCategory::Registry,
        name: "VirtualBox Guest Additions keys",
        description: "SOFTWARE\\Oracle\\VirtualBox Guest Additions. Enlil \
            installs no guest additions.",
        coverage: Coverage::AgentCheck("registry/hypervisor-keys"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "registry/vmware-tools-keys",
        category: CheckCategory::Registry,
        name: "VMware Tools keys",
        description: "SOFTWARE\\VMware, Inc.\\VMware Tools. Enlil installs no \
            guest tools.",
        coverage: Coverage::AgentCheck("registry/hypervisor-keys"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "registry/wine-keys",
        category: CheckCategory::Registry,
        name: "Wine registry keys",
        description: "Wine leaves SOFTWARE\\Wine keys; irrelevant on a real \
            Windows guest under enlil.",
        coverage: Coverage::ByConstruction("guest is real Windows, not Wine"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "devices/pci-ids",
        category: CheckCategory::Devices,
        name: "PCI device-ID enumeration",
        description: "Same hypervisor PCI-ID table as pafish's device check.",
        coverage: Coverage::AgentCheck("devices/pci-ids"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "devices/nic-oui",
        category: CheckCategory::Devices,
        name: "NIC MAC vendor OUIs",
        description: "Same virtualization-vendor MAC OUI table as pafish.",
        coverage: Coverage::AgentCheck("devices/nic-oui"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "devices/vbox-device-names",
        category: CheckCategory::Devices,
        name: "VirtualBox/VMware device names",
        description: "Probes \\\\.\\VBoxMiniRdrDN, \\\\.\\HGFS, \\\\.\\vmci, \
            \\\\.\\VBoxGuest device paths. Enlil creates no such devices.",
        coverage: Coverage::ByConstruction(
            "enlil's device model exposes no VBox/VMware device paths",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "devices/wmi-queries",
        category: CheckCategory::Devices,
        name: "WMI hardware queries",
        description: "Win32_ComputerSystem / Win32_VideoController / \
            Win32_DiskDrive manufacturer+model queries. Backed by enlil's \
            synthesized SMBIOS and passthrough disks.",
        coverage: Coverage::WindowsGuestRun(
            "run al-khaser.exe in the Windows guest; SMBIOS is synthesized, disks are passthrough",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "drivers/virtual-drivers-enumeration",
        category: CheckCategory::Drivers,
        name: "Driver enumeration",
        description: "Enumerates loaded drivers for vmmouse/vmhgfs/vbox*.sys. \
            Enlil installs no guest drivers.",
        coverage: Coverage::WindowsGuestRun(
            "run al-khaser.exe in the Windows guest; enlil ships no guest drivers",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "processes/vm-processes",
        category: CheckCategory::Processes,
        name: "VM vendor processes",
        description: "Looks for vbox*/vmware* processes. Enlil runs nothing \
            in-guest.",
        coverage: Coverage::WindowsGuestRun(
            "run al-khaser.exe in the Windows guest; no enlil processes exist in-guest",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "processes/analysis-tool-processes",
        category: CheckCategory::Processes,
        name: "Analysis-tool processes",
        description: "Flags Wireshark/ProcMon/debuggers as analysis tooling. A \
            clean validation guest simply does not run them.",
        coverage: Coverage::WindowsGuestRun(
            "run al-khaser.exe in a clean Windows guest with no analysis tools installed",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "sandbox/sandboxie-dll",
        category: CheckCategory::Sandbox,
        name: "Sandboxie artifacts",
        description: "SbieDll.dll / SbieDrv.sys presence. No sandbox product is \
            involved in an enlil guest.",
        coverage: Coverage::WindowsGuestRun(
            "run al-khaser.exe in the Windows guest; no sandbox product present",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "sandbox/cuckoo-artifacts",
        category: CheckCategory::Sandbox,
        name: "Cuckoo/Comodo/Qihoo artifacts",
        description: "Cuckoo agent / Comodo / Qihoo 360 sandbox markers. Not \
            present in an enlil guest.",
        coverage: Coverage::WindowsGuestRun(
            "run al-khaser.exe in the Windows guest; no sandbox product present",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "sandbox/known-dlls",
        category: CheckCategory::Sandbox,
        name: "Known analysis DLLs",
        description: "api_log.dll, dir_watch.dll, pstorec.dll, vmcheck.dll \
            loaded-module checks. Not present in a clean guest.",
        coverage: Coverage::WindowsGuestRun("run al-khaser.exe in a clean Windows guest"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "environment/uptime",
        category: CheckCategory::Environment,
        name: "System uptime",
        description: "Flags a suspiciously short uptime as a fresh sandbox. A \
            validation run simply lets the guest age before running the suite.",
        coverage: Coverage::WindowsGuestRun(
            "let the Windows guest run before executing al-khaser.exe",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "environment/username-hostname",
        category: CheckCategory::Environment,
        name: "Username / hostname",
        description: "Flags sandbox-default names. Provision the validation guest \
            with ordinary names.",
        coverage: Coverage::WindowsGuestRun(
            "provision the Windows guest with ordinary user/host names",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "human-interaction/mouse-movement",
        category: CheckCategory::HumanInteraction,
        name: "Mouse movement",
        description: "Cursor must move between samples; a static cursor implies \
            automation. Needs an interactive session.",
        coverage: Coverage::WindowsGuestRun(
            "run al-khaser.exe in an interactive Windows guest session",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "human-interaction/recent-documents",
        category: CheckCategory::HumanInteraction,
        name: "Recent-documents history",
        description: "An empty recent-documents list implies a fresh sandbox \
            image. Seed the validation guest with normal usage.",
        coverage: Coverage::WindowsGuestRun(
            "seed the Windows guest with ordinary usage before the suite run",
        ),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "debugger/is-debugger-present",
        category: CheckCategory::Debugger,
        name: "IsDebuggerPresent / CheckRemoteDebuggerPresent",
        description: "Flags an attached debugger. Enlil attaches none.",
        coverage: Coverage::WindowsGuestRun("run al-khaser.exe with no debugger attached"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "debugger/hardware-breakpoints",
        category: CheckCategory::Debugger,
        name: "Hardware breakpoints",
        description: "GetThreadContext DR0-DR3 must be clear. Enlil sets no guest \
            breakpoints.",
        coverage: Coverage::WindowsGuestRun("run al-khaser.exe in the Windows guest"),
    },
    SuiteCheck {
        suite: ALKHASER,
        id: "debugger/api-hooks",
        category: CheckCategory::Debugger,
        name: "API hook detection",
        description: "Checks ntdll export prologues for inline hooks. Nothing in \
            enlil hooks guest APIs.",
        coverage: Coverage::WindowsGuestRun("run al-khaser.exe in the Windows guest"),
    },
];

/// A verdict from an in-guest agent check, in suite-evaluator terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentVerdict {
    /// The agent check ran and found no tell.
    Pass,
    /// The agent check ran and found a tell.
    Tell,
    /// The agent check could not run here.
    Skipped,
}

/// The outcome of one suite check after evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuiteCheckStatus {
    /// Ran (via an agent check) and is clean.
    Clean,
    /// Ran (via an agent check) and a tell was found.
    Tell,
    /// The backing agent check could not run here; not a verdict.
    Skipped,
    /// Defeated by an enlil stealth primitive or by construction; no in-guest
    /// run needed.
    Mitigated,
    /// Requires the real suite binary in a full Windows guest.
    NeedsWindowsGuest,
}

impl SuiteCheckStatus {
    /// One-word status tag for reports.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Clean => "CLEAN",
            Self::Tell => "TELL",
            Self::Skipped => "SKIP",
            Self::Mitigated => "MITIGATED",
            Self::NeedsWindowsGuest => "WIN-GUEST",
        }
    }
}

/// One suite check joined with its evaluation outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteCheckOutcome<'a> {
    /// The inventory entry.
    pub check: &'a SuiteCheck,
    /// The evaluated outcome.
    pub status: SuiteCheckStatus,
    /// Human-readable evidence / disposition note.
    pub detail: String,
}

/// Evaluate one suite inventory against live agent verdicts.
///
/// `agent` maps agent check names (e.g. `"cpuid/hypervisor-present-bit"`) to
/// their verdicts. Checks with [`Coverage::AgentCheck`] coverage are resolved
/// through it; [`Coverage::StealthPrimitive`] / [`Coverage::ByConstruction`]
/// become [`SuiteCheckStatus::Mitigated`]; [`Coverage::WindowsGuestRun`]
/// becomes [`SuiteCheckStatus::NeedsWindowsGuest`]. An agent-covered check
/// with no matching verdict is [`SuiteCheckStatus::Skipped`].
#[must_use]
pub fn evaluate_suite<'a>(
    checks: &'a [SuiteCheck],
    agent: &[(&str, AgentVerdict)],
) -> Vec<SuiteCheckOutcome<'a>> {
    checks
        .iter()
        .map(|check| {
            let (status, detail) = match check.coverage {
                Coverage::StealthPrimitive(primitive) => (
                    SuiteCheckStatus::Mitigated,
                    format!("defeated by stealth primitive {primitive}"),
                ),
                Coverage::ByConstruction(why) => (
                    SuiteCheckStatus::Mitigated,
                    format!("by construction: {why}"),
                ),
                Coverage::WindowsGuestRun(note) => (
                    SuiteCheckStatus::NeedsWindowsGuest,
                    format!("requires Windows guest run: {note}"),
                ),
                Coverage::AgentCheck(name) => {
                    match agent.iter().find(|&&(n, _)| n == name).map(|&(_, v)| v) {
                        Some(AgentVerdict::Pass) => (
                            SuiteCheckStatus::Clean,
                            format!("in-guest agent check {name} passed"),
                        ),
                        Some(AgentVerdict::Tell) => (
                            SuiteCheckStatus::Tell,
                            format!("in-guest agent check {name} FOUND A TELL"),
                        ),
                        Some(AgentVerdict::Skipped) | None => (
                            SuiteCheckStatus::Skipped,
                            format!("in-guest agent check {name} did not run here"),
                        ),
                    }
                }
            };
            SuiteCheckOutcome {
                check,
                status,
                detail,
            }
        })
        .collect()
}

/// The evaluated report over both suite inventories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteRunReport<'a> {
    /// One outcome per inventory entry, pafish first then al-khaser.
    pub outcomes: Vec<SuiteCheckOutcome<'a>>,
}

impl SuiteRunReport<'_> {
    /// `(clean, tell, skipped, mitigated, needs_windows_guest)` counts.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize, usize, usize) {
        let (mut clean, mut tell, mut skipped, mut mitigated, mut windows) = (0, 0, 0, 0, 0);
        for o in &self.outcomes {
            match o.status {
                SuiteCheckStatus::Clean => clean += 1,
                SuiteCheckStatus::Tell => tell += 1,
                SuiteCheckStatus::Skipped => skipped += 1,
                SuiteCheckStatus::Mitigated => mitigated += 1,
                SuiteCheckStatus::NeedsWindowsGuest => windows += 1,
            }
        }
        (clean, tell, skipped, mitigated, windows)
    }

    /// Whether any evaluated check found a hypervisor tell.
    #[must_use]
    pub fn any_tells(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| o.status == SuiteCheckStatus::Tell)
    }
}

impl std::fmt::Display for SuiteRunReport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "pafish + al-khaser suite coverage report")?;
        for o in &self.outcomes {
            writeln!(
                f,
                "[{}] {}:{} — {}",
                o.status.tag(),
                o.check.suite.name(),
                o.check.id,
                o.detail
            )?;
        }
        let (clean, tell, skipped, mitigated, windows) = self.counts();
        write!(
            f,
            "verdict: {clean} clean, {tell} tell, {skipped} skipped, \
             {mitigated} mitigated, {windows} need Windows guest — "
        )?;
        if self.any_tells() {
            write!(f, "HYPERVISOR TELL FOUND")
        } else {
            write!(f, "no hypervisor tells in automated coverage")
        }
    }
}

/// Evaluate **both** suite inventories against live agent verdicts.
///
/// This is the machine-readable form of "pafish and al-khaser run clean in an
/// enlil guest": every check the in-guest agent can automate must come back
/// [`SuiteCheckStatus::Clean`], the rest are [`SuiteCheckStatus::Mitigated`]
/// (stealth primitive / by construction) or
/// [`SuiteCheckStatus::NeedsWindowsGuest`] (real suite binary in a Windows
/// guest, per the validation runbook).
#[must_use]
pub fn evaluate_both_suites(agent: &[(&str, AgentVerdict)]) -> SuiteRunReport<'static> {
    let mut outcomes = evaluate_suite(pafish_checks(), agent);
    outcomes.extend(evaluate_suite(alkhaser_checks(), agent));
    SuiteRunReport { outcomes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The agent check names [`Coverage::AgentCheck`] may reference — the
    /// stable names from `bridge_agent::stealth`.
    const KNOWN_AGENT_CHECKS: [&str; 7] = [
        "cpuid/hypervisor-present-bit",
        "cpuid/vendor-signature",
        "cpuid/cpu-vendor",
        "timing/rdtsc-cpuid",
        "devices/pci-ids",
        "devices/nic-oui",
        "registry/hypervisor-keys",
    ];

    fn all_checks() -> Vec<&'static SuiteCheck> {
        pafish_checks()
            .iter()
            .chain(alkhaser_checks().iter())
            .collect()
    }

    #[test]
    fn check_ids_are_unique() {
        let mut seen = HashSet::new();
        for c in all_checks() {
            let key = (c.suite.name(), c.id);
            assert!(seen.insert(key), "duplicate suite check id: {key:?}");
        }
    }

    #[test]
    fn every_check_is_documented() {
        for c in all_checks() {
            assert!(!c.name.is_empty(), "empty name for {}", c.id);
            assert!(!c.description.is_empty(), "empty description for {}", c.id);
            assert!(
                !c.description.contains("TODO") && !c.description.contains("FIXME"),
                "placeholder text in {}",
                c.id
            );
        }
    }

    #[test]
    fn agent_check_coverage_references_known_agent_checks() {
        for c in all_checks() {
            if let Coverage::AgentCheck(name) = c.coverage {
                assert!(
                    KNOWN_AGENT_CHECKS.contains(&name),
                    "suite check {} references unknown agent check {name}",
                    c.id
                );
            }
        }
    }

    #[test]
    fn stealth_primitive_coverage_references_stealth_paths() {
        for c in all_checks() {
            if let Coverage::StealthPrimitive(path) = c.coverage {
                assert!(
                    path.starts_with("enlil_devices::stealth::"),
                    "suite check {} references non-stealth path {path}",
                    c.id
                );
            }
        }
    }

    #[test]
    fn windows_guest_coverage_always_carries_a_runbook_note() {
        for c in all_checks() {
            if let Coverage::WindowsGuestRun(note) = c.coverage {
                assert!(!note.is_empty(), "empty runbook note for {}", c.id);
            }
        }
    }

    #[test]
    fn evaluate_maps_agent_verdicts() {
        let agent = [
            ("cpuid/hypervisor-present-bit", AgentVerdict::Pass),
            ("cpuid/vendor-signature", AgentVerdict::Tell),
            ("timing/rdtsc-cpuid", AgentVerdict::Skipped),
        ];
        let outcomes = evaluate_suite(pafish_checks(), &agent);
        let status_of = |id: &str| {
            outcomes
                .iter()
                .find(|o| o.check.id == id)
                .unwrap_or_else(|| panic!("missing check {id}"))
                .status
        };
        assert_eq!(
            status_of("cpu/hypervisor-present-bit"),
            SuiteCheckStatus::Clean
        );
        assert_eq!(
            status_of("cpu/hypervisor-vendor-signature"),
            SuiteCheckStatus::Tell
        );
        assert_eq!(status_of("timing/rdtsc-vmexit"), SuiteCheckStatus::Skipped);
        // An agent-covered check with no verdict at all is Skipped, not Clean.
        assert_eq!(status_of("devices/pci-ids"), SuiteCheckStatus::Skipped);
        // Stealth-primitive and by-construction checks are Mitigated.
        assert_eq!(
            status_of("cpu/kvm-magic-value"),
            SuiteCheckStatus::Mitigated
        );
        assert_eq!(
            status_of("firmware/acpi-dsdt-strings"),
            SuiteCheckStatus::Mitigated
        );
        // Windows-only checks need the Windows guest run.
        assert_eq!(
            status_of("human-interaction/mouse-movement"),
            SuiteCheckStatus::NeedsWindowsGuest
        );
    }

    #[test]
    fn both_suites_report_counts_add_up() {
        let report = evaluate_both_suites(&[]);
        let total = pafish_checks().len() + alkhaser_checks().len();
        assert_eq!(report.outcomes.len(), total);
        let (clean, tell, skipped, mitigated, windows) = report.counts();
        assert_eq!(clean + tell + skipped + mitigated + windows, total);
        // With no agent verdicts, nothing automated can be clean or a tell.
        assert_eq!(clean, 0);
        assert_eq!(tell, 0);
        assert!(!report.any_tells());
    }

    #[test]
    fn a_tell_anywhere_marks_the_report() {
        let agent = [("devices/pci-ids", AgentVerdict::Tell)];
        let report = evaluate_both_suites(&agent);
        assert!(report.any_tells());
        let display = format!("{report}");
        assert!(display.contains("HYPERVISOR TELL FOUND"));
    }

    #[test]
    fn all_clean_report_has_no_tells() {
        let agent: Vec<(&str, AgentVerdict)> = KNOWN_AGENT_CHECKS
            .iter()
            .map(|&n| (n, AgentVerdict::Pass))
            .collect();
        let report = evaluate_both_suites(&agent);
        assert!(!report.any_tells());
        let (clean, tell, skipped, mitigated, windows) = report.counts();
        assert_eq!(tell, 0);
        assert_eq!(skipped, 0);
        assert!(clean > 0 && mitigated > 0 && windows > 0);
        assert!(format!("{report}").contains("no hypervisor tells in automated coverage"));
    }

    #[test]
    fn cpu_and_timing_checks_are_automatable() {
        // The checks a KVM-gated in-guest probe can actually exercise must not
        // be parked behind WindowsGuestRun: they are the ones CI proves clean.
        for c in all_checks() {
            if matches!(c.category, CheckCategory::Cpu | CheckCategory::Timing) {
                assert!(
                    !matches!(c.coverage, Coverage::WindowsGuestRun(_)),
                    "cpu/timing check {} must be automatable, not WindowsGuestRun",
                    c.id
                );
            }
        }
    }
}
