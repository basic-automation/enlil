//! In-guest automated hypervisor-detection agent (roadmap item 5.8, T-5.2).
//!
//! The host-side detection primitives live in
//! [`enlil_devices::stealth::detection`]; this module is the agent that runs
//! them **from inside a booted guest**, pafish/al-khaser style: it reads the
//! guest-visible CPUID leaves, times a serializing CPUID bracketed by RDTSC,
//! enumerates the guest's PCI devices and NIC MACs, and (on Windows) inspects
//! the registry for hypervisor artifacts, then reports every hypervisor tell
//! it finds.
//!
//! Each check is split into three layers so the logic stays testable on any
//! host:
//! 1. *collection* (`collect_*`) — best-effort OS/architecture-specific reads,
//!    returning `None` when the platform cannot provide the data;
//! 2. *parsing* (`parse_*`) — pure functions turning raw OS output into typed
//!    values, unit-tested with synthetic inputs;
//! 3. *evaluation* (`evaluate_*`) — pure functions turning typed values into
//!    [`CheckResult`]s through the shared
//!    [`enlil_devices::stealth::detection`] primitives.
//!
//! [`run_detection`] wires the three layers together. Checks the platform
//! cannot support are reported as [`CheckStatus::Skipped`], never as
//! failures, so the agent also runs (partially) on non-x86 or
//! non-Linux/Windows guests.

use enlil_devices::{
    net::MacAddress,
    stealth::detection::{
        BARE_METAL_CPUID_CYCLE_CEILING, cpuid_hypervisor_present, hypervisor_vendor_from_signature,
        is_genuine_cpu_vendor, median_cycles, pci_device_reveals_hypervisor,
        timing_reveals_hypervisor,
    },
};

/// Outcome of a single detection check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// The check ran and found no hypervisor tell.
    Pass,
    /// The check ran and found a hypervisor tell.
    Tell,
    /// The check could not run on this platform; not a verdict.
    Skipped,
}

/// One named hypervisor-detection check and its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Stable dotted name, e.g. `"cpuid/hypervisor-present-bit"`.
    pub check: &'static str,
    /// The outcome.
    pub status: CheckStatus,
    /// Human-readable evidence, including the measured values.
    pub detail: String,
}

/// Raw CPUID readings taken inside the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuidReadings {
    /// CPUID leaf 1 `ECX` (bit 31 is the hypervisor-present bit).
    pub leaf1_ecx: u32,
    /// CPUID leaf `0x40000000` `EBX` (hypervisor vendor signature word 0).
    pub hv_ebx: u32,
    /// CPUID leaf `0x40000000` `ECX` (hypervisor vendor signature word 1).
    pub hv_ecx: u32,
    /// CPUID leaf `0x40000000` `EDX` (hypervisor vendor signature word 2).
    pub hv_edx: u32,
    /// CPUID leaf 0 `EBX` (CPU vendor string word 0).
    pub vendor_ebx: u32,
    /// CPUID leaf 0 `EDX` (CPU vendor string word 1).
    pub vendor_edx: u32,
    /// CPUID leaf 0 `ECX` (CPU vendor string word 2).
    pub vendor_ecx: u32,
}

/// A sample of RDTSC cycle-count deltas measured across a serializing CPUID.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TimingSample {
    /// One delta per probe iteration.
    pub deltas: Vec<u64>,
}

/// One PCI device visible to the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciDevice {
    /// PCI vendor ID.
    pub vendor_id: u16,
    /// PCI device ID.
    pub device_id: u16,
    /// Where the device was found (e.g. the Linux sysfs BDF `0000:00:02.0`).
    pub location: String,
}

/// One network interface visible to the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NicInfo {
    /// Interface name (e.g. `eth0`).
    pub name: String,
    /// Interface MAC address.
    pub mac: [u8; 6],
}

/// One hypervisor artifact found in the guest's registry (Windows only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryFinding {
    /// Registry key holding the artifact.
    pub key: String,
    /// What was found there.
    pub evidence: String,
}

/// The full report of one in-guest detection run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectionReport {
    /// One entry per check, in run order.
    pub results: Vec<CheckResult>,
}

impl DetectionReport {
    /// Whether any check reported a hypervisor tell.
    #[must_use]
    pub fn has_hypervisor_tell(&self) -> bool {
        self.results.iter().any(|r| r.status == CheckStatus::Tell)
    }

    /// `(pass, tell, skipped)` counts over the report's checks.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut tell = 0;
        let mut skipped = 0;
        for r in &self.results {
            match r.status {
                CheckStatus::Pass => pass += 1,
                CheckStatus::Tell => tell += 1,
                CheckStatus::Skipped => skipped += 1,
            }
        }
        (pass, tell, skipped)
    }
}

impl std::fmt::Display for DetectionReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "in-guest hypervisor detection report")?;
        for r in &self.results {
            let tag = match r.status {
                CheckStatus::Pass => "PASS",
                CheckStatus::Tell => "TELL",
                CheckStatus::Skipped => "SKIP",
            };
            writeln!(f, "[{tag}] {} — {}", r.check, r.detail)?;
        }
        let (pass, tell, skipped) = self.counts();
        write!(f, "verdict: {pass} pass, {tell} tell, {skipped} skipped — ")?;
        if self.has_hypervisor_tell() {
            write!(f, "HYPERVISOR DETECTED")
        } else {
            write!(f, "no hypervisor tells")
        }
    }
}

fn skipped(check: &'static str, reason: &str) -> CheckResult {
    CheckResult {
        check,
        status: CheckStatus::Skipped,
        detail: reason.to_string(),
    }
}

/// The 12-byte hypervisor vendor signature from the `0x40000000` registers.
fn hv_signature_bytes(r: &CpuidReadings) -> [u8; 12] {
    let mut sig = [0u8; 12];
    sig[0..4].copy_from_slice(&r.hv_ebx.to_le_bytes());
    sig[4..8].copy_from_slice(&r.hv_ecx.to_le_bytes());
    sig[8..12].copy_from_slice(&r.hv_edx.to_le_bytes());
    sig
}

/// Evaluate guest CPUID readings: the hypervisor-present bit, the
/// `0x40000000` vendor signature, and the leaf-0 CPU vendor genuineness.
#[must_use]
pub fn evaluate_cpuid(readings: &CpuidReadings) -> [CheckResult; 3] {
    let present = cpuid_hypervisor_present(readings.leaf1_ecx);
    let vendor =
        hypervisor_vendor_from_signature(readings.hv_ebx, readings.hv_ecx, readings.hv_edx);
    let genuine = is_genuine_cpu_vendor(
        readings.vendor_ebx,
        readings.vendor_edx,
        readings.vendor_ecx,
    );

    let signature_detail = vendor.map_or_else(
        || {
            format!(
                "CPUID 0x40000000 signature {:02x?} is not a known hypervisor",
                hv_signature_bytes(readings)
            )
        },
        |v| format!("CPUID 0x40000000 exposes hypervisor vendor {:?}", v.name()),
    );
    let mut vendor_string = [0u8; 12];
    vendor_string[0..4].copy_from_slice(&readings.vendor_ebx.to_le_bytes());
    vendor_string[4..8].copy_from_slice(&readings.vendor_edx.to_le_bytes());
    vendor_string[8..12].copy_from_slice(&readings.vendor_ecx.to_le_bytes());

    [
        CheckResult {
            check: "cpuid/hypervisor-present-bit",
            status: if present {
                CheckStatus::Tell
            } else {
                CheckStatus::Pass
            },
            detail: format!(
                "CPUID.1:ECX = {:#010x} (hypervisor-present bit 31 {})",
                readings.leaf1_ecx,
                if present { "SET" } else { "clear" }
            ),
        },
        CheckResult {
            check: "cpuid/vendor-signature",
            status: if vendor.is_some() {
                CheckStatus::Tell
            } else {
                CheckStatus::Pass
            },
            detail: signature_detail,
        },
        CheckResult {
            check: "cpuid/cpu-vendor",
            status: if genuine {
                CheckStatus::Pass
            } else {
                CheckStatus::Tell
            },
            detail: format!(
                "CPUID leaf-0 vendor {:?} {}",
                String::from_utf8_lossy(&vendor_string),
                if genuine {
                    "is a genuine CPU vendor"
                } else {
                    "is NOT a genuine CPU vendor"
                }
            ),
        },
    ]
}

/// Evaluate an RDTSC timing sample: a median above the bare-metal ceiling
/// means the serializing CPUID is being trapped and emulated across a VM
/// boundary.
#[must_use]
pub fn evaluate_timing(sample: &TimingSample) -> CheckResult {
    let median = median_cycles(&sample.deltas);
    CheckResult {
        check: "timing/rdtsc-cpuid",
        status: if timing_reveals_hypervisor(&sample.deltas, BARE_METAL_CPUID_CYCLE_CEILING) {
            CheckStatus::Tell
        } else {
            CheckStatus::Pass
        },
        detail: median.map_or_else(
            || "no timing samples collected".to_string(),
            |m| {
                format!(
                    "median of {} RDTSC-bracketed CPUID samples = {m} cycles (bare-metal ceiling {BARE_METAL_CPUID_CYCLE_CEILING})",
                    sample.deltas.len()
                )
            },
        ),
    }
}

/// Evaluate the guest's PCI device list against known hypervisor device IDs.
#[must_use]
pub fn evaluate_pci(devices: &[PciDevice]) -> CheckResult {
    let mut tells = Vec::new();
    for dev in devices {
        if let Some(desc) = pci_device_reveals_hypervisor(dev.vendor_id, dev.device_id) {
            tells.push(format!(
                "{:04x}:{:04x} at {} ({desc})",
                dev.vendor_id, dev.device_id, dev.location
            ));
        }
    }
    CheckResult {
        check: "devices/pci-ids",
        status: if tells.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Tell
        },
        detail: if tells.is_empty() {
            format!(
                "{} PCI device(s) enumerated, none match known hypervisor device IDs",
                devices.len()
            )
        } else {
            format!("hypervisor PCI device(s): {}", tells.join("; "))
        },
    }
}

/// Evaluate the guest's NIC MACs against virtualization-vendor OUIs.
#[must_use]
pub fn evaluate_nics(nics: &[NicInfo]) -> CheckResult {
    let mut tells = Vec::new();
    for nic in nics {
        let mac = MacAddress(nic.mac);
        if mac.is_hypervisor_oui() {
            tells.push(format!("{} has hypervisor-vendor OUI ({mac})", nic.name));
        }
    }
    CheckResult {
        check: "devices/nic-oui",
        status: if tells.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Tell
        },
        detail: if tells.is_empty() {
            format!(
                "{} NIC(s) checked, none carry a hypervisor-vendor OUI",
                nics.len()
            )
        } else {
            format!("hypervisor NIC OUI(s): {}", tells.join("; "))
        },
    }
}

/// Evaluate Windows registry findings (collected by the registry collector).
#[must_use]
pub fn evaluate_registry(findings: &[RegistryFinding]) -> CheckResult {
    CheckResult {
        check: "registry/hypervisor-keys",
        status: if findings.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Tell
        },
        detail: if findings.is_empty() {
            "no hypervisor artifacts in BIOS strings, guest-addition keys, or services".to_string()
        } else {
            format!(
                "hypervisor registry artifact(s): {}",
                findings
                    .iter()
                    .map(|f| format!("{} ({})", f.key, f.evidence))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        },
    }
}

/// Run the full in-guest detection suite and return the report.
///
/// Every layer is best-effort: a check the platform cannot support is
/// reported as [`CheckStatus::Skipped`], and a collection failure never
/// aborts the run.
#[must_use]
pub fn run_detection() -> DetectionReport {
    let mut results = Vec::new();

    match collect_cpuid() {
        Some(readings) => results.extend(evaluate_cpuid(&readings)),
        None => {
            for check in [
                "cpuid/hypervisor-present-bit",
                "cpuid/vendor-signature",
                "cpuid/cpu-vendor",
            ] {
                results.push(skipped(check, "CPUID is only readable on x86_64"));
            }
        }
    }

    match collect_timing() {
        Some(sample) => results.push(evaluate_timing(&sample)),
        None => results.push(skipped(
            "timing/rdtsc-cpuid",
            "RDTSC is only readable on x86_64",
        )),
    }

    match collect_pci_devices() {
        Some(devices) => results.push(evaluate_pci(&devices)),
        None => results.push(skipped(
            "devices/pci-ids",
            "PCI enumeration is only implemented on Linux and Windows",
        )),
    }

    match collect_nics() {
        Some(nics) => results.push(evaluate_nics(&nics)),
        None => results.push(skipped(
            "devices/nic-oui",
            "NIC enumeration is only implemented on Linux and Windows",
        )),
    }

    match collect_registry() {
        Some(findings) => results.push(evaluate_registry(&findings)),
        None => results.push(skipped(
            "registry/hypervisor-keys",
            "registry checks are only implemented on Windows",
        )),
    }

    DetectionReport { results }
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// Read the guest-visible CPUID leaves. Only meaningful on `x86_64`.
// On non-x86_64 targets this always returns `None`, which the lint cannot see
// through the `cfg`; the `Option` is load-bearing for the cross-platform API.
#[allow(clippy::unnecessary_wraps)]
fn collect_cpuid() -> Option<CpuidReadings> {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::__cpuid;
        let leaf1 = __cpuid(1);
        let hv = __cpuid(0x4000_0000);
        let leaf0 = __cpuid(0);
        Some(CpuidReadings {
            leaf1_ecx: leaf1.ecx,
            hv_ebx: hv.ebx,
            hv_ecx: hv.ecx,
            hv_edx: hv.edx,
            vendor_ebx: leaf0.ebx,
            vendor_edx: leaf0.edx,
            vendor_ecx: leaf0.ecx,
        })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// Number of RDTSC-bracketed CPUID probes per timing run.
const TIMING_SAMPLES: usize = 32;

/// Time a serializing CPUID bracketed by two RDTSCs, the classic VM-exit
/// timing probe. Only meaningful on `x86_64`.
// As above: the `Option` is load-bearing on non-x86_64 targets.
#[allow(clippy::unnecessary_wraps)]
fn collect_timing() -> Option<TimingSample> {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{__cpuid, _rdtsc};
        let mut deltas = Vec::with_capacity(TIMING_SAMPLES);
        for _ in 0..TIMING_SAMPLES {
            // SAFETY: RDTSC is unprivileged and always available on x86_64.
            let start = unsafe { _rdtsc() };
            // CPUID serializes the pipeline, so the delta brackets exactly one
            // serializing instruction.
            __cpuid(0);
            // SAFETY: as above.
            let end = unsafe { _rdtsc() };
            deltas.push(end.wrapping_sub(start));
        }
        Some(TimingSample { deltas })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// Enumerate the guest's PCI devices (Linux sysfs, Windows `PnP`).
fn collect_pci_devices() -> Option<Vec<PciDevice>> {
    #[cfg(target_os = "linux")]
    {
        let mut devices = Vec::new();
        let entries = std::fs::read_dir("/sys/bus/pci/devices").ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let vendor = std::fs::read_to_string(path.join("vendor"))
                .ok()
                .and_then(|s| parse_sysfs_hex(&s));
            let device = std::fs::read_to_string(path.join("device"))
                .ok()
                .and_then(|s| parse_sysfs_hex(&s));
            if let (Some(vendor_id), Some(device_id)) = (vendor, device) {
                devices.push(PciDevice {
                    vendor_id,
                    device_id,
                    location: entry.file_name().to_string_lossy().into_owned(),
                });
            }
        }
        Some(devices)
    }
    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-PnpDevice | Where-Object { $_.DeviceID -like 'PCI*' } | Select-Object -ExpandProperty DeviceID",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Some(
            parse_pnp_device_ids(&text)
                .into_iter()
                .map(|(vendor_id, device_id, location)| PciDevice {
                    vendor_id,
                    device_id,
                    location,
                })
                .collect(),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Enumerate the guest's NICs and MACs (Linux sysfs, Windows `getmac`).
fn collect_nics() -> Option<Vec<NicInfo>> {
    #[cfg(target_os = "linux")]
    {
        let mut nics = Vec::new();
        let entries = std::fs::read_dir("/sys/class/net").ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "lo" {
                continue;
            }
            let address = std::fs::read_to_string(entry.path().join("address")).ok()?;
            if let Some(mac) = parse_mac_str(address.trim()) {
                nics.push(NicInfo { name, mac });
            }
        }
        Some(nics)
    }
    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("getmac")
            .args(["/fo", "csv", "/nh"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Some(parse_getmac_csv(&text))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Inspect the Windows registry for hypervisor artifacts (BIOS strings,
/// guest-addition install keys, hypervisor service/driver names).
/// Returns `None` on non-Windows platforms, or when `reg.exe` is unavailable.
// The Windows body is not `const`-compatible; the lint only sees the
// non-Windows `None` branch through the `cfg`.
#[allow(clippy::missing_const_for_fn)]
fn collect_registry() -> Option<Vec<RegistryFinding>> {
    #[cfg(target_os = "windows")]
    {
        let mut findings = Vec::new();
        let mut any_query_ok = false;

        // 1. BIOS / video-BIOS version strings (pafish's first stop).
        for (key, value) in [
            (r"HKLM\HARDWARE\DESCRIPTION\System", "SystemBiosVersion"),
            (r"HKLM\HARDWARE\DESCRIPTION\System", "VideoBiosVersion"),
        ] {
            if let Some(data) = reg_query_value(key, value) {
                any_query_ok = true;
                if let Some(keyword) = hypervisor_keyword_in(&data) {
                    findings.push(RegistryFinding {
                        key: format!("{key} [{value}]"),
                        evidence: format!("contains {keyword:?}: {data}"),
                    });
                }
            }
        }

        // 2. Guest-addition install keys.
        for key in [
            r"HKLM\SOFTWARE\Oracle\VirtualBox Guest Additions",
            r"HKLM\SOFTWARE\VMware, Inc.\VMware Tools",
            r"HKLM\SOFTWARE\Xen",
        ] {
            if reg_key_exists(key) {
                any_query_ok = true;
                findings.push(RegistryFinding {
                    key: key.to_string(),
                    evidence: "guest-addition install key exists".to_string(),
                });
            }
        }

        // 3. Hypervisor service/driver names.
        if let Some(services) = reg_query_subkeys(r"HKLM\SYSTEM\CurrentControlSet\Services") {
            any_query_ok = true;
            for service in services {
                let lower = service.to_lowercase();
                if HYPERVISOR_SERVICE_PATTERNS
                    .iter()
                    .any(|p| lower.contains(p))
                {
                    findings.push(RegistryFinding {
                        key: format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{service}"),
                        evidence: "hypervisor service/driver installed".to_string(),
                    });
                }
            }
        }

        if !any_query_ok && findings.is_empty() {
            // reg.exe is unavailable — skip the check rather than misreport.
            return None;
        }
        Some(findings)
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// Parsing (pure; unit-tested with synthetic inputs)
// ---------------------------------------------------------------------------

/// Parse a Linux sysfs hex word like `"0x1af4\n"`.
#[cfg(target_os = "linux")]
fn parse_sysfs_hex(s: &str) -> Option<u16> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u16::from_str_radix(s, 16).ok()
}

/// Parse a MAC string with `:` or `-` separators, e.g. `"52:54:00:12:34:56"`.
fn parse_mac_str(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.split([':', '-']);
    for byte in &mut mac {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(mac)
}

/// Parse Windows `PnP` `PCI\VEN_xxxx&DEV_yyyy&...` device-ID lines into
/// `(vendor_id, device_id, raw_line)` triples.
#[cfg(target_os = "windows")]
fn parse_pnp_device_ids(text: &str) -> Vec<(u16, u16, String)> {
    fn hex4(s: &str) -> Option<u16> {
        if s.len() < 4 || !s[..4].chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u16::from_str_radix(&s[..4], 16).ok()
    }

    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let (Some(ven), Some(dev)) = (line.find("VEN_"), line.find("DEV_")) else {
            continue;
        };
        if dev < ven {
            continue;
        }
        if let (Some(vendor_id), Some(device_id)) = (hex4(&line[ven + 4..]), hex4(&line[dev + 4..]))
        {
            out.push((vendor_id, device_id, line.to_string()));
        }
    }
    out
}

/// Parse `getmac /fo csv /nh` output (`"Name","52-54-00-12-34-56","..."`).
#[cfg(target_os = "windows")]
fn parse_getmac_csv(text: &str) -> Vec<NicInfo> {
    let mut nics = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('"') {
            continue;
        }
        let fields: Vec<&str> = line.split("\",\"").collect();
        if fields.len() < 2 {
            continue;
        }
        let name = fields[0].trim_start_matches('"').to_string();
        let mac_text = fields[1].trim_end_matches('"');
        if let Some(mac) = parse_mac_str(mac_text) {
            nics.push(NicInfo { name, mac });
        }
    }
    nics
}

/// Parse `reg query <key> /v <value>` output, returning the value's data.
///
/// Output looks like:
/// ```text
/// HKEY_LOCAL_MACHINE\HARDWARE\DESCRIPTION\System
///     SystemBiosVersion    REG_MULTI_SZ    VBOX  - 1
/// ```
#[cfg(target_os = "windows")]
fn parse_reg_query_value_output(text: &str) -> Option<String> {
    for line in text.lines().skip(1) {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() >= 3 {
            return Some(tokens[2..].join(" "));
        }
    }
    None
}

/// Parse `reg query <key>` output into its subkey names (last path component
/// of each listed full key path, skipping the queried key itself on line 0).
#[cfg(target_os = "windows")]
fn parse_reg_query_subkeys(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.rsplit('\\').next().unwrap_or(l).to_string())
        .collect()
}

/// Run `reg query` with `args`; `None` when `reg.exe` is missing or fails.
#[cfg(target_os = "windows")]
fn reg_query(args: &[&str]) -> Option<std::process::Output> {
    let output = std::process::Command::new("reg")
        .arg("query")
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output)
}

/// Query one registry value's data via `reg query <key> /v <value>`.
#[cfg(target_os = "windows")]
fn reg_query_value(key: &str, value: &str) -> Option<String> {
    let output = reg_query(&[key, "/v", value])?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_reg_query_value_output(&text)
}

/// Whether a registry key exists (`reg query <key>` succeeds).
#[cfg(target_os = "windows")]
fn reg_key_exists(key: &str) -> bool {
    reg_query(&[key]).is_some()
}

/// List a registry key's subkey names via `reg query <key>`.
#[cfg(target_os = "windows")]
fn reg_query_subkeys(key: &str) -> Option<Vec<String>> {
    let output = reg_query(&[key])?;
    let text = String::from_utf8_lossy(&output.stdout);
    Some(parse_reg_query_subkeys(&text))
}

/// Substrings (matched case-insensitively) that betray a hypervisor in a
/// registry string.
#[cfg(target_os = "windows")]
const HYPERVISOR_KEYWORDS: [&str; 10] = [
    "vbox",
    "virtualbox",
    "vmware",
    "xen",
    "qemu",
    "kvm",
    "parallels",
    "bhyve",
    "hyper-v",
    "virtual machine",
];

/// Substrings (matched case-insensitively) identifying hypervisor
/// services/drivers under `HKLM\SYSTEM\CurrentControlSet\Services`.
#[cfg(target_os = "windows")]
const HYPERVISOR_SERVICE_PATTERNS: [&str; 11] = [
    "vbox",
    "vmhgfs",
    "vmxnet",
    "vmci",
    "vmicheartbeat",
    "vmicvss",
    "vmicshutdown",
    "vmicexchange",
    "xenservice",
    "xenbus",
    "xennet",
];

/// The first hypervisor keyword found (case-insensitively) in `data`.
#[cfg(target_os = "windows")]
fn hypervisor_keyword_in(data: &str) -> Option<&'static str> {
    let lower = data.to_lowercase();
    HYPERVISOR_KEYWORDS
        .iter()
        .find(|kw| lower.contains(**kw))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_cpuid_readings() -> CpuidReadings {
        // What an enlil guest sees: present bit clear, no vendor signature,
        // genuine Intel vendor string.
        CpuidReadings {
            leaf1_ecx: 0,
            hv_ebx: 0,
            hv_ecx: 0,
            hv_edx: 0,
            vendor_ebx: u32::from_le_bytes(*b"Genu"),
            vendor_edx: u32::from_le_bytes(*b"ineI"),
            vendor_ecx: u32::from_le_bytes(*b"ntel"),
        }
    }

    fn kvm_signature_readings() -> CpuidReadings {
        let mut r = clean_cpuid_readings();
        r.leaf1_ecx = 1 << 31;
        r.hv_ebx = u32::from_le_bytes(*b"KVMK");
        r.hv_ecx = u32::from_le_bytes(*b"VMKV");
        r.hv_edx = u32::from_le_bytes(*b"M\0\0\0");
        r
    }

    #[test]
    fn clean_cpuid_readings_produce_no_tells() {
        let results = evaluate_cpuid(&clean_cpuid_readings());
        assert!(results.iter().all(|r| r.status == CheckStatus::Pass));
    }

    #[test]
    fn kvm_cpuid_readings_produce_tells() {
        let results = evaluate_cpuid(&kvm_signature_readings());
        assert_eq!(results[0].check, "cpuid/hypervisor-present-bit");
        assert_eq!(results[0].status, CheckStatus::Tell);
        assert_eq!(results[1].check, "cpuid/vendor-signature");
        assert_eq!(results[1].status, CheckStatus::Tell);
        assert!(results[1].detail.contains("KVM"));
        // The CPU vendor itself is still genuine — only two tells.
        assert_eq!(results[2].status, CheckStatus::Pass);
    }

    #[test]
    fn blank_cpu_vendor_is_a_tell() {
        let mut r = clean_cpuid_readings();
        r.vendor_ebx = 0;
        r.vendor_edx = 0;
        r.vendor_ecx = 0;
        let results = evaluate_cpuid(&r);
        assert_eq!(results[2].status, CheckStatus::Tell);
    }

    #[test]
    fn timing_flags_trapped_cpuid() {
        let bare_metal = TimingSample {
            deltas: vec![180, 200, 190, 8000, 210, 195, 205, 30000, 185],
        };
        assert_eq!(evaluate_timing(&bare_metal).status, CheckStatus::Pass);

        let trapped = TimingSample {
            deltas: vec![4200, 3900, 4500, 4100, 3800, 4300, 4000],
        };
        let result = evaluate_timing(&trapped);
        assert_eq!(result.status, CheckStatus::Tell);
        assert!(result.detail.contains("cycles"));
    }

    #[test]
    fn pci_evaluation_flags_hypervisor_devices() {
        let devices = vec![
            PciDevice {
                vendor_id: 0x8086,
                device_id: 0x1237,
                location: "0000:00:00.0".to_string(),
            },
            PciDevice {
                vendor_id: 0x1AF4,
                device_id: 0x1041,
                location: "0000:00:03.0".to_string(),
            },
        ];
        let result = evaluate_pci(&devices);
        assert_eq!(result.status, CheckStatus::Tell);
        assert!(result.detail.contains("1af4:1041"));

        let clean = vec![PciDevice {
            vendor_id: 0x8086,
            device_id: 0x100E,
            location: "0000:00:03.0".to_string(),
        }];
        assert_eq!(evaluate_pci(&clean).status, CheckStatus::Pass);
        assert_eq!(evaluate_pci(&[]).status, CheckStatus::Pass);
    }

    #[test]
    fn nic_evaluation_flags_hypervisor_ouis() {
        let nics = vec![
            NicInfo {
                name: "eth0".to_string(),
                mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56], // QEMU/KVM
            },
            NicInfo {
                name: "eth1".to_string(),
                mac: [0x02, 0x11, 0x22, 0x33, 0x44, 0x55], // locally administered
            },
        ];
        let result = evaluate_nics(&nics);
        assert_eq!(result.status, CheckStatus::Tell);
        assert!(result.detail.contains("eth0"));

        let clean = vec![NicInfo {
            name: "eth1".to_string(),
            mac: [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
        }];
        assert_eq!(evaluate_nics(&clean).status, CheckStatus::Pass);
    }

    #[test]
    fn registry_evaluation_flags_findings() {
        assert_eq!(
            evaluate_registry(&[]).status,
            CheckStatus::Pass,
            "a clean registry passes"
        );
        let findings = vec![RegistryFinding {
            key: r"HKLM\SOFTWARE\Oracle\VirtualBox Guest Additions".to_string(),
            evidence: "guest-addition install key exists".to_string(),
        }];
        let result = evaluate_registry(&findings);
        assert_eq!(result.status, CheckStatus::Tell);
        assert!(result.detail.contains("VirtualBox"));
    }

    #[test]
    fn a_clean_enlil_guest_report_has_no_tells() {
        // The full evaluator chain over synthetic "enlil guest" inputs: this
        // is what the report must look like inside a booted enlil guest.
        let mut results = Vec::new();
        results.extend(evaluate_cpuid(&clean_cpuid_readings()));
        results.push(evaluate_timing(&TimingSample {
            deltas: vec![200; 32],
        }));
        results.push(evaluate_pci(&[]));
        results.push(evaluate_nics(&[NicInfo {
            name: "eth0".to_string(),
            mac: [0x02, 0x9a, 0x3c, 0x11, 0x22, 0x33],
        }]));
        results.push(evaluate_registry(&[]));
        let report = DetectionReport { results };
        assert!(!report.has_hypervisor_tell());
        assert_eq!(report.counts(), (7, 0, 0));
        let text = report.to_string();
        assert!(text.contains("no hypervisor tells"));
    }

    #[test]
    fn report_counts_and_verdict() {
        let report = DetectionReport {
            results: vec![
                CheckResult {
                    check: "a",
                    status: CheckStatus::Pass,
                    detail: String::new(),
                },
                CheckResult {
                    check: "b",
                    status: CheckStatus::Tell,
                    detail: String::new(),
                },
                CheckResult {
                    check: "c",
                    status: CheckStatus::Skipped,
                    detail: String::new(),
                },
            ],
        };
        assert!(report.has_hypervisor_tell());
        assert_eq!(report.counts(), (1, 1, 1));
        let text = report.to_string();
        assert!(text.contains("HYPERVISOR DETECTED"));
        assert!(text.contains("[TELL] b"));
    }

    #[test]
    fn detection_run_completes_and_covers_every_check() {
        // Smoke test on the live machine: the agent must run to completion and
        // report every check exactly once. The *verdict* is not asserted —
        // this host may itself be virtualized.
        let report = run_detection();
        let names: Vec<&str> = report.results.iter().map(|r| r.check).collect();
        assert_eq!(
            names,
            [
                "cpuid/hypervisor-present-bit",
                "cpuid/vendor-signature",
                "cpuid/cpu-vendor",
                "timing/rdtsc-cpuid",
                "devices/pci-ids",
                "devices/nic-oui",
                "registry/hypervisor-keys",
            ]
        );
    }

    #[test]
    fn mac_parsing_accepts_colon_and_dash_forms() {
        assert_eq!(
            parse_mac_str("52:54:00:12:34:56"),
            Some([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
        );
        assert_eq!(
            parse_mac_str("52-54-00-12-34-56"),
            Some([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
        );
        assert_eq!(parse_mac_str("52:54:00:12:34"), None);
        assert_eq!(parse_mac_str("52:54:00:12:34:56:78"), None);
        assert_eq!(parse_mac_str("zz:54:00:12:34:56"), None);
        assert_eq!(parse_mac_str("525:54:00:12:34:56"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sysfs_hex_parsing() {
        assert_eq!(parse_sysfs_hex("0x1af4\n"), Some(0x1AF4));
        assert_eq!(parse_sysfs_hex("0X8086"), Some(0x8086));
        assert_eq!(parse_sysfs_hex("1af4"), None);
        assert_eq!(parse_sysfs_hex("0xzz"), None);
        assert_eq!(parse_sysfs_hex(""), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pnp_device_id_parsing() {
        let text = "PCI\\VEN_1AF4&DEV_1041&SUBSYS_10411AF4&REV_01\\4&1234\nPCI\\VEN_8086&DEV_100E&SUBSYS_00008086\njunk line\n";
        let parsed = parse_pnp_device_ids(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, 0x1AF4);
        assert_eq!(parsed[0].1, 0x1041);
        assert_eq!(parsed[1].0, 0x8086);
        assert_eq!(parsed[1].1, 0x100E);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn getmac_csv_parsing() {
        let text = "\"Ethernet\",\"52-54-00-12-34-56\",\"\\Device\\Tcpip_1\"\r\n\"Wi-Fi\",\"N/A\",\"Media disconnected\"\r\n";
        let nics = parse_getmac_csv(text);
        assert_eq!(nics.len(), 1);
        assert_eq!(nics[0].name, "Ethernet");
        assert_eq!(nics[0].mac, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reg_query_value_output_parsing() {
        let text = "HKEY_LOCAL_MACHINE\\HARDWARE\\DESCRIPTION\\System\r\n    SystemBiosVersion    REG_MULTI_SZ    VBOX  - 1\r\n";
        assert_eq!(
            parse_reg_query_value_output(text),
            Some("VBOX - 1".to_string())
        );
        assert_eq!(
            parse_reg_query_value_output("HKEY_LOCAL_MACHINE\\X\r\n"),
            None
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reg_query_subkey_parsing_and_keyword_matching() {
        let text = "HKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\VBoxService\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\Tcpip\r\n";
        let subkeys = parse_reg_query_subkeys(text);
        assert_eq!(
            subkeys,
            vec!["VBoxService".to_string(), "Tcpip".to_string()]
        );
        assert_eq!(hypervisor_keyword_in("VBOX - 1"), Some("vbox"));
        assert_eq!(hypervisor_keyword_in("American Megatrends Inc."), None);
    }
}
