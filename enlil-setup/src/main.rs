#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! Enlil first-run setup wizard.
//!
//! Runs as the first guest (tiny Linux initramfs) and walks the user through
//! hardware detection, resource allocation, and guest configuration.
//!
//! Phase 0: scaffold with stubs.

use anyhow::Result;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Hardware detection
// ---------------------------------------------------------------------------

/// Detected hardware inventory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareInfo {
    pub cpus: usize,
    pub ram_mb: u64,
    pub gpus: Vec<String>,
    pub nvme_drives: Vec<String>,
    pub usb_controllers: Vec<String>,
    pub iommu_groups: Vec<String>,
}

impl std::fmt::Display for HardwareInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "  CPUs            : {}", self.cpus)?;
        writeln!(f, "  RAM             : {} MB", self.ram_mb)?;
        writeln!(f, "  GPUs            : {}", format_list(&self.gpus))?;
        writeln!(f, "  NVMe drives     : {}", format_list(&self.nvme_drives))?;
        writeln!(
            f,
            "  USB controllers : {}",
            format_list(&self.usb_controllers)
        )?;
        write!(f, "  IOMMU groups    : {}", format_list(&self.iommu_groups))
    }
}

fn format_list(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".into()
    } else {
        items.join(", ")
    }
}

/// Detect hardware on the running system.
///
/// On Linux this will eventually read from `/sys` and `/proc`.
/// Everywhere else it returns dummy data for development.
#[must_use]
pub fn detect_hardware() -> HardwareInfo {
    #[cfg(target_os = "linux")]
    {
        detect_hardware_linux()
    }
    #[cfg(not(target_os = "linux"))]
    {
        detect_hardware_stub()
    }
}

#[cfg(target_os = "linux")]
fn detect_hardware_linux() -> HardwareInfo {
    // Start from the stub and overlay whatever we can read for real. CPU count
    // and RAM come from procfs; NVMe drives, IOMMU groups, and the GPU/USB
    // controllers (by PCI class) from sysfs — everything but the stub's own
    // fallbacks is now live.
    let mut hw = detect_hardware_stub();
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo")
        && let Some(n) = count_cpus_from_cpuinfo(&cpuinfo)
    {
        hw.cpus = n;
    }
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo")
        && let Some(mb) = parse_meminfo_total_mb(&meminfo)
    {
        hw.ram_mb = mb;
    }
    // Overlay the real device lists — honest (possibly empty) beats the stub's
    // fabricated hardware. A host with the IOMMU disabled truthfully shows none.
    hw.nvme_drives = detect_nvme_drives();
    hw.iommu_groups = detect_iommu_groups();
    hw.gpus = detect_pci_devices(is_display_controller);
    hw.usb_controllers = detect_pci_devices(is_usb_controller);
    hw
}

/// Enumerate PCI devices under `/sys/bus/pci/devices` whose class matches
/// `is_match`, labelling each `vendor:device [BDF]`. Empty if the directory is
/// absent. Shared by the GPU and USB-controller scans.
#[cfg(target_os = "linux")]
fn detect_pci_devices(is_match: fn(&str) -> bool) -> Vec<String> {
    let root = "/sys/bus/pci/devices";
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut bdfs: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    bdfs.sort();
    bdfs.into_iter()
        .filter_map(|bdf| {
            let class = std::fs::read_to_string(format!("{root}/{bdf}/class")).ok()?;
            if !is_match(&class) {
                return None;
            }
            let vendor =
                std::fs::read_to_string(format!("{root}/{bdf}/vendor")).unwrap_or_default();
            let device =
                std::fs::read_to_string(format!("{root}/{bdf}/device")).unwrap_or_default();
            Some(pci_device_label(&vendor, &device, &bdf))
        })
        .collect()
}

/// Whether a PCI `class` string (sysfs form, e.g. `0x030000`) is a display
/// controller — base class `0x03`, which is how GPUs enumerate.
#[cfg(any(target_os = "linux", test))]
fn is_display_controller(class: &str) -> bool {
    class
        .trim()
        .strip_prefix("0x")
        .is_some_and(|c| c.starts_with("03"))
}

/// Whether a PCI `class` string is a USB controller — base class `0x0c`,
/// subclass `0x03`.
#[cfg(any(target_os = "linux", test))]
fn is_usb_controller(class: &str) -> bool {
    class
        .trim()
        .strip_prefix("0x")
        .is_some_and(|c| c.starts_with("0c03"))
}

/// Label a PCI device `vendor:device [BDF]` from its sysfs `vendor`/`device`
/// hex IDs (each of the form `0x10de`) and its bus address.
#[cfg(any(target_os = "linux", test))]
fn pci_device_label(vendor: &str, device: &str, bdf: &str) -> String {
    let id = |raw: &str| {
        let raw = raw.trim();
        raw.strip_prefix("0x").unwrap_or(raw).to_string()
    };
    format!("{}:{} [{bdf}]", id(vendor), id(device))
}

/// Enumerate `NVMe` drives from `/sys/class/nvme`, labelling each with its model.
/// Empty if the directory is absent (no `NVMe` / not exposed).
#[cfg(target_os = "linux")]
fn detect_nvme_drives() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/nvme") else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let model = std::fs::read_to_string(format!("/sys/class/nvme/{name}/model"))
                .unwrap_or_default();
            nvme_label(&model, &name)
        })
        .collect()
}

/// Enumerate IOMMU groups from `/sys/kernel/iommu_groups`, each with its member
/// devices. Empty if the directory is absent (IOMMU off / not exposed) — which
/// is itself a meaningful signal, since passthrough isolation needs it.
#[cfg(target_os = "linux")]
fn detect_iommu_groups() -> Vec<String> {
    let root = "/sys/kernel/iommu_groups";
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut ids: Vec<u64> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u64>().ok())
        .collect();
    ids.sort_unstable();
    ids.into_iter()
        .map(|id| {
            let devices: Vec<String> = std::fs::read_dir(format!("{root}/{id}/devices"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .collect();
            format_iommu_group(id, &devices)
        })
        .collect()
}

/// Label an `NVMe` drive from its sysfs `model` and device `name` (e.g.
/// `Samsung 990 Pro [nvme0]`). Falls back to a generic label when the model is
/// blank.
#[cfg(any(target_os = "linux", test))]
fn nvme_label(model: &str, name: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        format!("NVMe [{name}]")
    } else {
        format!("{model} [{name}]")
    }
}

/// Format one IOMMU group and its (sorted) member devices for the inventory.
#[cfg(any(target_os = "linux", test))]
fn format_iommu_group(id: u64, devices: &[String]) -> String {
    let mut devices = devices.to_vec();
    devices.sort();
    if devices.is_empty() {
        format!("Group {id}: (no devices)")
    } else {
        format!("Group {id}: {}", devices.join(", "))
    }
}

/// Total RAM in MB parsed from the contents of `/proc/meminfo`. The `MemTotal`
/// line reports kibibytes; returns `None` if the line is absent or unparsable.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo_total_mb(meminfo: &str) -> Option<u64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// Count logical CPUs from the contents of `/proc/cpuinfo` — one `processor`
/// line per logical CPU. Returns `None` when no such line is present.
#[cfg(any(target_os = "linux", test))]
fn count_cpus_from_cpuinfo(cpuinfo: &str) -> Option<usize> {
    let n = cpuinfo
        .lines()
        .filter(|l| l.starts_with("processor") && l.contains(':'))
        .count();
    (n > 0).then_some(n)
}

fn detect_hardware_stub() -> HardwareInfo {
    HardwareInfo {
        cpus: 16,
        ram_mb: 65536,
        gpus: vec![
            "NVIDIA RTX 4090 [10de:2684]".into(),
            "AMD Radeon RX 7900 XTX [1002:744c]".into(),
        ],
        nvme_drives: vec![
            "Samsung 990 Pro 2TB [nvme0]".into(),
            "Samsung 990 Pro 2TB [nvme1]".into(),
        ],
        usb_controllers: vec!["xHCI Host Controller [8086:a36d]".into()],
        iommu_groups: vec![
            "Group 0: Host bridge".into(),
            "Group 1: GPU 10de:2684".into(),
            "Group 2: GPU 1002:744c".into(),
            "Group 3: NVMe nvme0".into(),
            "Group 4: NVMe nvme1".into(),
            "Group 5: USB xHCI".into(),
        ],
    }
}

// ---------------------------------------------------------------------------
// Setup configuration (wizard output)
// ---------------------------------------------------------------------------

/// A single guest VM definition produced by the wizard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestDef {
    pub name: String,
    pub cpus: usize,
    pub ram_mb: u64,
    pub gpus: Vec<String>,
    pub storage: Vec<String>,
    pub usb: Vec<String>,
}

/// Full setup configuration — the wizard's output, serialised to TOML and
/// consumed by the rest of the Enlil stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupConfig {
    pub guests: Vec<GuestDef>,
    /// CPUs reserved for the hypervisor / host.
    pub host_cpus: usize,
    /// RAM (MB) reserved for the hypervisor / host.
    pub host_ram_mb: u64,
}

/// Interactive wizard that walks the user through resource allocation.
///
/// Phase 0: returns a hard-coded two-guest example.
#[must_use]
pub fn run_wizard(hw: &HardwareInfo) -> SetupConfig {
    // TODO: actual TUI prompts
    let _ = hw;

    SetupConfig {
        host_cpus: 2,
        host_ram_mb: 4096,
        guests: vec![
            GuestDef {
                name: "windows-gaming".into(),
                cpus: 8,
                ram_mb: 32768,
                gpus: vec!["10de:2684".into()],
                storage: vec!["nvme0".into()],
                usb: vec!["8086:a36d".into()],
            },
            GuestDef {
                name: "linux-workstation".into(),
                cpus: 6,
                ram_mb: 28672,
                gpus: vec!["1002:744c".into()],
                storage: vec!["nvme1".into()],
                usb: vec![],
            },
        ],
    }
}

/// Serialize a [`SetupConfig`] to TOML.
///
/// # Errors
///
/// Returns an error if TOML serialization fails.
pub fn generate_config(config: &SetupConfig) -> Result<String> {
    Ok(toml::to_string_pretty(config)?)
}

/// Write a [`SetupConfig`] to `path` as TOML — the wizard's output the rest of
/// the Enlil stack loads.
///
/// # Errors
///
/// Returns an error if serialization ([`generate_config`]) fails or the file
/// cannot be written.
pub fn write_config(config: &SetupConfig, path: &std::path::Path) -> Result<()> {
    let toml = generate_config(config)?;
    std::fs::write(path, toml)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Banner
// ---------------------------------------------------------------------------

const BANNER: &str = r"
  ╔═══════════════════════════════════════════╗
  ║         E N L I L   S E T U P             ║
  ║   First-run hardware & guest wizard       ║
  ╚═══════════════════════════════════════════╝
";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    println!("{BANNER}");

    println!("[1/4] Detecting hardware...\n");
    let hw = detect_hardware();
    println!("{hw}\n");

    println!("[2/4] Running setup wizard...\n");
    let config = run_wizard(&hw);

    println!("[3/4] Generating configuration...\n");
    let toml_output = generate_config(&config)?;
    println!("{toml_output}");

    println!("[4/4] Writing configuration...\n");
    let out_path = std::path::Path::new("enlil.toml");
    write_config(&config, out_path)?;
    println!("Done. Wrote {}", out_path.display());

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_hardware_returns_valid_info() {
        let hw = detect_hardware();
        assert!(hw.cpus > 0);
        assert!(hw.ram_mb > 0);
    }

    #[test]
    fn wizard_produces_guests() {
        let hw = detect_hardware();
        let config = run_wizard(&hw);
        assert!(!config.guests.is_empty());
    }

    #[test]
    fn wizard_allocations_fit_hardware() {
        // The Phase-0 wizard returns a fixed example sized for a 16-CPU / 64 GB
        // box, so check its allocations against that inventory rather than the
        // live host — detect_hardware now returns the real CPU/RAM on Linux,
        // which need not be that large.
        let hw = HardwareInfo {
            cpus: 16,
            ram_mb: 65536,
            gpus: vec![],
            nvme_drives: vec![],
            usb_controllers: vec![],
            iommu_groups: vec![],
        };
        let config = run_wizard(&hw);
        let guest_cpus: usize = config.guests.iter().map(|g| g.cpus).sum();
        let guest_ram: u64 = config.guests.iter().map(|g| g.ram_mb).sum();
        assert!(config.host_cpus + guest_cpus <= hw.cpus);
        assert!(config.host_ram_mb + guest_ram <= hw.ram_mb);
    }

    #[test]
    fn parses_memtotal_from_meminfo() {
        let sample = "MemTotal:       65536000 kB\nMemFree:         1000 kB\n";
        assert_eq!(parse_meminfo_total_mb(sample), Some(64000));
        // No MemTotal line → None.
        assert_eq!(parse_meminfo_total_mb("MemFree: 10 kB"), None);
    }

    #[test]
    fn counts_logical_cpus_from_cpuinfo() {
        let sample = "processor\t: 0\nvendor_id\t: X\n\nprocessor\t: 1\nvendor_id\t: X\n";
        assert_eq!(count_cpus_from_cpuinfo(sample), Some(2));
        assert_eq!(count_cpus_from_cpuinfo("no cpus here"), None);
    }

    #[test]
    fn nvme_label_uses_the_model_or_a_fallback() {
        assert_eq!(
            nvme_label("Samsung 990 Pro 2TB\n", "nvme0"),
            "Samsung 990 Pro 2TB [nvme0]",
            "trims the sysfs model and appends the device name"
        );
        assert_eq!(
            nvme_label("   ", "nvme1"),
            "NVMe [nvme1]",
            "blank model falls back"
        );
    }

    #[test]
    fn classifies_pci_display_and_usb_controllers() {
        assert!(is_display_controller("0x030000")); // VGA display controller
        assert!(is_display_controller("0x038000\n")); // other display, trailing newline
        assert!(!is_display_controller("0x0c0330")); // USB, not display
        assert!(is_usb_controller("0x0c0330")); // xHCI
        assert!(!is_usb_controller("0x030000")); // display, not USB
        assert!(!is_usb_controller("0x0c0500")); // SMBus (0c05), not USB
    }

    #[test]
    fn pci_device_label_formats_vendor_device_and_bdf() {
        assert_eq!(
            pci_device_label("0x10de\n", "0x2684\n", "0000:01:00.0"),
            "10de:2684 [0000:01:00.0]"
        );
    }

    #[test]
    fn format_iommu_group_sorts_devices_and_handles_empty() {
        assert_eq!(
            format_iommu_group(5, &["0000:01:00.1".into(), "0000:01:00.0".into()]),
            "Group 5: 0000:01:00.0, 0000:01:00.1",
            "devices are sorted"
        );
        assert_eq!(format_iommu_group(3, &[]), "Group 3: (no devices)");
    }

    #[test]
    fn generate_config_produces_valid_toml() {
        let hw = detect_hardware();
        let config = run_wizard(&hw);
        let output = generate_config(&config).expect("serialization failed");
        assert!(output.contains("windows-gaming"));
        assert!(output.contains("linux-workstation"));

        // Round-trip: parse back
        let parsed: SetupConfig = toml::from_str(&output).expect("deserialization failed");
        assert_eq!(parsed.guests.len(), config.guests.len());
        assert_eq!(parsed.host_cpus, config.host_cpus);
    }

    #[test]
    fn write_config_round_trips_through_a_file() {
        let config = run_wizard(&detect_hardware());
        // Unique temp path (no tempfile dep); cleaned up at the end.
        let path = std::env::temp_dir().join(format!(
            "enlil-setup-write-{}-{}.toml",
            std::process::id(),
            config.guests.len()
        ));
        write_config(&config, &path).expect("write config");
        let text = std::fs::read_to_string(&path).expect("read back config");
        let parsed: SetupConfig = toml::from_str(&text).expect("parse written config");
        assert_eq!(parsed.guests.len(), config.guests.len());
        assert_eq!(parsed.host_cpus, config.host_cpus);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn hardware_info_display() {
        let hw = detect_hardware();
        let text = format!("{hw}");
        assert!(text.contains("CPUs"));
        assert!(text.contains("RAM"));
    }

    #[test]
    fn format_list_empty() {
        assert_eq!(format_list(&[]), "(none)");
    }

    #[test]
    fn format_list_items() {
        let items = vec!["a".into(), "b".into()];
        assert_eq!(format_list(&items), "a, b");
    }
}
