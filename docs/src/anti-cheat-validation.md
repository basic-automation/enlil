# Anti-Cheat Validation (BattlEye / EAC / Vanguard)

Roadmap item 5.8, T-5.4. This document records what the three major kernel
anti-cheats check with respect to virtualization, enlil's posture on each
vector, what is validated automatically, and what still requires a manual
Windows-guest run. It is the written half of the acceptance criterion
"anti-cheat validation documented"; the executable half is the
`enlil_devices::stealth::suites` inventory plus the in-guest detection agent.

## Scope and honesty notes

- Anti-cheat vendors treat *undetectability* as an arms race and some ban VMs
  **by policy**, not by detection. Enlil's 1.0 claim is narrower and
  verifiable: **an enlil guest exposes no hypervisor tell** — the CPUID,
  timing, firmware, device, and artifact checks these products (and
  pafish/al-khaser) run come back clean. That is what this document validates.
  It is not a guarantee that any specific game will let you play on a VM.
- BattlEye, Easy Anti-Cheat (EAC), and Vanguard are closed-source and update
  continuously. The vectors below are the publicly documented, long-stable
  ones. New vectors are handled by the same mechanism as everything else:
  they become entries in the `stealth::suites` inventory with a disposition.

## What the anti-cheats check (virtualization-relevant vectors)

| # | Vector | Products known to use it | Enlil posture | Validated by |
|---|--------|--------------------------|---------------|--------------|
| 1 | CPUID hypervisor-present bit (leaf 1 ECX[31]) | BE, EAC, Vanguard | Cleared by `CpuidStealthTable` (LOCKED PRINCIPLE 1) | In-guest agent `cpuid/hypervisor-present-bit`; KVM-gated `guest_suite_cpu_checks_report_clean` |
| 2 | Hypervisor vendor signature (leaf 0x40000000) | BE, EAC, Vanguard | Whole range hidden as out-of-range; no signature, no KVM/Hyper-V magic | In-guest agent `cpuid/vendor-signature`; KVM-gated suite test incl. magic-value assert |
| 3 | RDTSC / timing analysis (VMEXIT inflation) | BE, EAC, Vanguard | TSC offsetting — guest RDTSC needs no exit; APERF/MPERF shadowed | `stealth::timing`; bare-metal SVM path; KVM test asserts sane median (KVM exit cost documented as the limit) |
| 4 | Firmware string scans (SMBIOS/DSDT for VBOX/VMware/QEMU) | BE, EAC | Enlil synthesizes its own SMBIOS (ASUS/G.Skill-class defaults) and DSDT (AMI OEM IDs); no VM strings exist | By construction (`stealth::suites` firmware checks) |
| 5 | PCI device-ID scans (VMware SVGA, VBox, Xen, Hyper-V, VirtIO) | BE, EAC | Guest PCI topology carries no hypervisor IDs (VirtIO wildcard included) | In-guest agent `devices/pci-ids` |
| 6 | NIC MAC OUI scans | BE, EAC | Guest MACs are synthesized locally-administered, never a VM-vendor OUI; config rejects VM OUIs | In-guest agent `devices/nic-oui`; `MacAddress::is_hypervisor_oui` |
| 7 | Disk model/serial scans (VBOX HARDDISK etc.) | BE | Disks are VFIO/NVMe passthrough — the guest reads the physical disk's genuine IDENTIFY strings | By construction (passthrough-only storage) |
| 8 | Registry artifact scans (VBox/VMware keys) | BE, EAC | Enlil installs no guest additions/tools; keys cannot exist | In-guest agent `registry/hypervisor-keys` (Windows) |
| 9 | Driver blocklists (known cheat/VM drivers) | BE, EAC, Vanguard | Enlil installs no guest drivers at all | Windows guest runbook (§4) |
| 10 | Unsigned-driver / DSE enforcement | Vanguard, EAC | Guest kernel policy is the guest OS's own; enlil neither signs nor injects drivers | N/A — guest-OS behavior, not a hypervisor tell |
| 11 | TPM 2.0 attestation (Vanguard on Win11) | Vanguard | Per-guest seeded vTPM 2.0 (LOCKED PRINCIPLE 1), persisted via the on-disk TPM state store | vTPM implementation (item 5.5); guest-OS attestation flow is a Windows runbook step |
| 12 | Secure Boot state | Vanguard | Boot chain is signed (sbsign tooling); guest Secure Boot enrollment is operator-controlled | `scripts/make-signed-iso.sh`; runbook §4 |
| 13 | DMA protection / IOMMU | Vanguard | VFIO passthrough is IOMMU-gated; the IOMMU is real hardware | Platform requirement, documented in runbook |
| 14 | Memory integrity / HVCI | Vanguard | HVCI is a guest-OS feature running on the real CPU; enlil does not interfere with it | Windows guest runbook (§4) |

Vectors 10–14 are guest-OS or hardware properties, not hypervisor tells: enlil's
obligation is to not *break* them (no unsigned driver injection, real IOMMU,
real CPU for HVCI), which it meets by doing nothing.

## pafish / al-khaser runbook (Windows guest)

The real `pafish.exe` / `al-khaser.exe` binaries only run on Windows, so the
final validation is manual, on a KVM-capable host:

1. Provision the Windows guest with **≥ 2 vCPUs** and **≥ 2 GiB RAM**
   (both suites flag 1-vCPU / tiny-RAM guests as sandboxes by design —
   `stealth::suites` marks these `WindowsGuestRun`).
2. Attach a realistically sized passthrough disk; use ordinary user/host names.
3. Boot the guest under enlil, let it age past the uptime checks, and keep the
   session interactive (mouse-movement checks need a live cursor).
4. Run `bridge-agent stealth-check` in the guest — expect no tells.
5. Run `pafish.exe` and `al-khaser.exe` — expect no VM detections. Record the
   per-check output next to the `stealth::suites` inventory; any new check
   either suite gains becomes a new inventory entry with a disposition.
6. For anti-cheat specifically: install the game + its anti-cheat in the same
   guest and confirm the client launches without a virtualization warning.
   (Whether the vendor *allows* VM play is their policy; enlil's bar is no
   *detection*.)

## What remains

- The Windows-guest runs above are manual today: they need a Windows image,
  a KVM-capable host, and (for anti-cheat clients) game accounts. Automating
  them is a post-1.0 hardening item, not a 1.0 blocker — the automatable
  surface (CPUID, timing path, PCI, NIC, registry, firmware, disk) is covered
  by the in-guest agent, the KVM-gated tests, and the by-construction
  inventories.
- If an anti-cheat vendor ships a genuinely new VM-detection vector, the
  process is: add it to `stealth::suites` as a check with an honest
  disposition, defeat it in `stealth::*`, and extend the in-guest agent.
