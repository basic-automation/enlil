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
        write!(
            f,
            "  IOMMU groups    : {}",
            format_list(&self.iommu_groups)
        )
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
    // TODO: read /proc/cpuinfo, /proc/meminfo, /sys/class/drm, etc.
    detect_hardware_stub()
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
pub fn generate_config(config: &SetupConfig) -> Result<String> {
    Ok(toml::to_string_pretty(config)?)
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

    println!("[4/4] Done. (Phase 0 — config not yet written to disk)");

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
        let hw = detect_hardware();
        let config = run_wizard(&hw);
        let guest_cpus: usize = config.guests.iter().map(|g| g.cpus).sum();
        let guest_ram: u64 = config.guests.iter().map(|g| g.ram_mb).sum();
        assert!(config.host_cpus + guest_cpus <= hw.cpus);
        assert!(config.host_ram_mb + guest_ram <= hw.ram_mb);
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
