#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! Enlil first-run setup wizard.
//!
//! Runs as the first guest (tiny Linux initramfs) and walks the user through
//! hardware detection, resource allocation, and guest configuration.
//!
//! Phase 0: scaffold with stubs.

mod tui;

use std::collections::{HashSet, VecDeque};
use std::io::IsTerminal as _;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
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
    pub nics: Vec<String>,
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
        writeln!(f, "  NICs            : {}", format_list(&self.nics))?;
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
    hw.nics = detect_pci_devices(is_network_controller);
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

/// Whether a PCI `class` string is a network controller — base class `0x02`,
/// which is how NICs (Ethernet/Wi-Fi) enumerate. Guests need these for their
/// virtio-net uplink / SR-IOV NIC passthrough.
#[cfg(any(target_os = "linux", test))]
fn is_network_controller(class: &str) -> bool {
    class
        .trim()
        .strip_prefix("0x")
        .is_some_and(|c| c.starts_with("02"))
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
        nics: vec!["Intel I225-V 2.5GbE [8086:15f3]".into()],
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

/// How the wizard asks questions.
///
/// The same flow drives the fullscreen ratatui TUI ([`tui::TuiPrompter`]),
/// scripted `--dry-run` answers ([`ScriptedPrompter`]), and unit tests.
pub trait Prompter {
    /// Show an informational screen; returns when the user continues.
    ///
    /// # Errors
    ///
    /// Returns an error if the prompt cannot be displayed.
    fn info(&mut self, title: &str, lines: &[String]) -> Result<()>;

    /// Ask for free-form text; `validate` rejects bad input with a message.
    ///
    /// # Errors
    ///
    /// Returns an error if validation never succeeds, the user cancels, or the
    /// prompt fails.
    fn text(
        &mut self,
        prompt: &str,
        default: &str,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<String>;

    /// Ask for a whole number in `[min, max]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the user cancels or the prompt fails.
    fn number(&mut self, prompt: &str, default: u64, min: u64, max: u64) -> Result<u64>;

    /// Ask for any subset of `options`; returns the chosen indices.
    ///
    /// # Errors
    ///
    /// Returns an error if the user cancels or the prompt fails.
    fn multi_choice(&mut self, prompt: &str, options: &[String]) -> Result<Vec<usize>>;

    /// Ask a yes/no question.
    ///
    /// # Errors
    ///
    /// Returns an error if the user cancels or the prompt fails.
    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool>;
}

/// One scripted answer for a [`ScriptedPrompter`].
#[derive(Debug, Clone)]
pub enum ScriptedAnswer {
    /// Accept the prompt's default.
    Default,
    /// Answer a text prompt.
    Text(String),
    /// Answer a number prompt.
    Number(u64),
    /// Answer a multi-choice prompt with option indices.
    Multi(Vec<usize>),
    /// Answer a confirmation.
    Confirm(bool),
}

/// Non-interactive [`Prompter`] that answers from a script, or from defaults
/// once the script runs out.
///
/// Every question and the answer it resolved to is appended to
/// [`Self::transcript`], so `--dry-run` shows exactly which prompts ran and
/// what they produced. Also used by unit tests.
#[derive(Debug, Default)]
pub struct ScriptedPrompter {
    answers: VecDeque<ScriptedAnswer>,
    /// Human-readable log of every prompt and its resolved answer.
    pub transcript: Vec<String>,
}

impl ScriptedPrompter {
    /// Build a prompter that consumes `answers` in order, falling back to each
    /// prompt's default once the script runs out.
    #[must_use]
    pub fn new(answers: Vec<ScriptedAnswer>) -> Self {
        Self {
            answers: answers.into(),
            transcript: Vec::new(),
        }
    }

    fn next_answer(&mut self) -> ScriptedAnswer {
        self.answers.pop_front().unwrap_or(ScriptedAnswer::Default)
    }
}

impl Prompter for ScriptedPrompter {
    fn info(&mut self, title: &str, lines: &[String]) -> Result<()> {
        self.transcript
            .push(format!("info: {title} ({} lines)", lines.len()));
        Ok(())
    }

    fn text(
        &mut self,
        prompt: &str,
        default: &str,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<String> {
        let value = match self.next_answer() {
            ScriptedAnswer::Default => default.to_string(),
            ScriptedAnswer::Text(value) => value,
            other => {
                return Err(anyhow::anyhow!(
                    "scripted answer {other:?} cannot answer text prompt {prompt:?}"
                ));
            }
        };
        validate(&value).map_err(|message| {
            anyhow::anyhow!("invalid scripted answer for {prompt:?}: {message}")
        })?;
        self.transcript.push(format!("text: {prompt} -> {value:?}"));
        Ok(value)
    }

    fn number(&mut self, prompt: &str, default: u64, min: u64, max: u64) -> Result<u64> {
        let value = match self.next_answer() {
            ScriptedAnswer::Default => default,
            ScriptedAnswer::Number(value) => value,
            other => {
                return Err(anyhow::anyhow!(
                    "scripted answer {other:?} cannot answer number prompt {prompt:?}"
                ));
            }
        };
        let parsed = parse_bounded_u64(&value.to_string(), min, max).map_err(|message| {
            anyhow::anyhow!("invalid scripted answer for {prompt:?}: {message}")
        })?;
        self.transcript
            .push(format!("number: {prompt} -> {parsed}"));
        Ok(parsed)
    }

    fn multi_choice(&mut self, prompt: &str, options: &[String]) -> Result<Vec<usize>> {
        let chosen = match self.next_answer() {
            ScriptedAnswer::Default => Vec::new(),
            ScriptedAnswer::Multi(chosen) => chosen,
            other => {
                return Err(anyhow::anyhow!(
                    "scripted answer {other:?} cannot answer multi-choice prompt {prompt:?}"
                ));
            }
        };
        for &index in &chosen {
            if index >= options.len() {
                return Err(anyhow::anyhow!(
                    "scripted multi-choice index {index} out of range for {prompt:?}"
                ));
            }
        }
        let picked: Vec<&str> = chosen.iter().map(|&i| options[i].as_str()).collect();
        self.transcript.push(format!(
            "multi-choice: {prompt} ({} options) -> {picked:?}",
            options.len()
        ));
        Ok(chosen)
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        let value = match self.next_answer() {
            ScriptedAnswer::Default => default,
            ScriptedAnswer::Confirm(value) => value,
            other => {
                return Err(anyhow::anyhow!(
                    "scripted answer {other:?} cannot answer confirm prompt {prompt:?}"
                ));
            }
        };
        self.transcript
            .push(format!("confirm: {prompt} -> {value}"));
        Ok(value)
    }
}

/// Parse `input` as a `u64` within `[min, max]`; the `Err` message is shown to
/// the user, so it reads like guidance, not an assertion.
///
/// # Errors
///
/// Returns a user-facing message when `input` is not a whole number in range.
pub fn parse_bounded_u64(input: &str, min: u64, max: u64) -> Result<u64, String> {
    let value: u64 = input
        .trim()
        .parse()
        .map_err(|_| format!("{input:?} is not a whole number"))?;
    if value < min || value > max {
        return Err(format!("enter a number between {min} and {max}"));
    }
    Ok(value)
}

/// Convert a validated resource count to `usize` for the config structs.
///
/// # Errors
///
/// Returns an error on platforms where the value does not fit in `usize`
/// (the prompt ranges keep this from happening on 64-bit targets).
fn usize_from_u64(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| anyhow::anyhow!("{value} does not fit in usize"))
}

/// Validate a guest name: non-empty, short, filesystem/hostname-safe, unique.
fn validate_guest_name(name: &str, taken: &[String]) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.len() > 64 {
        return Err("name must be at most 64 characters".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("use only letters, digits, '-' and '_'".to_string());
    }
    if taken.iter().any(|other| other == name) {
        return Err(format!("a guest named {name:?} already exists"));
    }
    Ok(())
}

/// Offer the not-yet-assigned `devices` as a multi-choice passthrough prompt.
///
/// Devices picked here are recorded in `assigned`, so later guests are only
/// offered what's still free — a passthrough device belongs to exactly one
/// guest. Skips the prompt entirely when nothing is free.
///
/// # Errors
///
/// Returns an error if the prompt fails.
fn assign_devices(
    prompter: &mut dyn Prompter,
    prompt: &str,
    devices: &[String],
    assigned: &mut HashSet<String>,
) -> Result<Vec<String>> {
    let options: Vec<String> = devices
        .iter()
        .filter(|device| !assigned.contains(*device))
        .cloned()
        .collect();
    if options.is_empty() {
        return Ok(Vec::new());
    }
    let mut picked = Vec::new();
    for index in prompter.multi_choice(prompt, &options)? {
        let device = options
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("choice index {index} out of range"))?
            .clone();
        assigned.insert(device.clone());
        picked.push(device);
    }
    Ok(picked)
}

/// Prompt for one guest's resources, validated against what's still free.
///
/// Defaults split the remainder evenly across the guests still to come, and
/// maxima always leave room for them, so the totals can never exceed the
/// hardware.
///
/// # Errors
///
/// Returns an error if the user cancels or a prompt fails.
#[allow(clippy::too_many_arguments)]
fn prompt_guest(
    prompter: &mut dyn Prompter,
    hw: &HardwareInfo,
    index: u64,
    guest_count: u64,
    names: &mut Vec<String>,
    avail_cpus: &mut u64,
    avail_ram_mb: &mut u64,
    assigned_gpus: &mut HashSet<String>,
    assigned_storage: &mut HashSet<String>,
    assigned_usb: &mut HashSet<String>,
) -> Result<GuestDef> {
    let guests_left = guest_count - index;
    let who = format!("Guest {}/{}", index + 1, guest_count);

    let default_name = format!("guest-{}", index + 1);
    let taken = names.clone();
    let name = prompter.text(&format!("{who}: name"), &default_name, &|input| {
        validate_guest_name(input, &taken)
    })?;
    names.push(name.clone());

    let cpus = prompter.number(
        &format!("{who} ({name}): vCPUs"),
        (*avail_cpus / guests_left).max(1),
        1,
        *avail_cpus - (guests_left - 1),
    )?;
    let ram_mb = prompter.number(
        &format!("{who} ({name}): RAM (MB)"),
        (*avail_ram_mb / guests_left).max(512),
        512,
        *avail_ram_mb - 512 * (guests_left - 1),
    )?;
    let gpus = assign_devices(
        prompter,
        &format!("{who} ({name}): GPU passthrough"),
        &hw.gpus,
        assigned_gpus,
    )?;
    let storage = assign_devices(
        prompter,
        &format!("{who} ({name}): NVMe storage passthrough"),
        &hw.nvme_drives,
        assigned_storage,
    )?;
    let usb = assign_devices(
        prompter,
        &format!("{who} ({name}): USB controller passthrough"),
        &hw.usb_controllers,
        assigned_usb,
    )?;

    *avail_cpus -= cpus;
    *avail_ram_mb -= ram_mb;
    Ok(GuestDef {
        name,
        cpus: usize_from_u64(cpus)?,
        ram_mb,
        gpus,
        storage,
        usb,
    })
}

/// Interactive wizard that walks the user through resource allocation.
///
/// Every question goes through `prompter`, so the same flow drives the
/// fullscreen TUI, scripted `--dry-run` answers, and unit tests. Host
/// reservations and per-guest allocations are validated against the detected
/// hardware as they are entered, each passthrough device can only be assigned
/// to one guest, and the run ends with a review screen the user must accept.
///
/// # Errors
///
/// Returns an error if the hardware is too small to host a guest, the user
/// cancels or declines the review, or a prompt fails.
pub fn run_wizard(hw: &HardwareInfo, prompter: &mut dyn Prompter) -> Result<SetupConfig> {
    if hw.cpus < 2 {
        anyhow::bail!(
            "need at least 2 CPUs (one for the host, one for a guest); found {}",
            hw.cpus
        );
    }
    if hw.ram_mb < 2048 {
        anyhow::bail!("need at least 2048 MB RAM; found {} MB", hw.ram_mb);
    }

    let summary: Vec<String> = format!("{hw}").lines().map(str::to_string).collect();
    prompter.info(
        "Detected hardware \u{2014} continue to allocate resources",
        &summary,
    )?;

    let host_cpus = prompter.number(
        "CPUs to reserve for the host / hypervisor",
        2.min(hw.cpus as u64 - 1),
        1,
        hw.cpus as u64 - 1,
    )?;
    let host_ram_mb = prompter.number(
        "RAM to reserve for the host / hypervisor (MB)",
        4096.min(hw.ram_mb - 1024),
        512,
        hw.ram_mb - 1024,
    )?;

    let mut avail_cpus = hw.cpus as u64 - host_cpus;
    let mut avail_ram_mb = hw.ram_mb - host_ram_mb;

    // Every guest needs at least 1 vCPU and 512 MB RAM.
    let max_guests = avail_cpus.min(8).min(avail_ram_mb / 512).max(1);
    let guest_count = prompter.number("Guest VMs to define", max_guests.min(2), 1, max_guests)?;

    let mut guests = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut assigned_gpus = HashSet::new();
    let mut assigned_storage = HashSet::new();
    let mut assigned_usb = HashSet::new();
    for index in 0..guest_count {
        guests.push(prompt_guest(
            prompter,
            hw,
            index,
            guest_count,
            &mut names,
            &mut avail_cpus,
            &mut avail_ram_mb,
            &mut assigned_gpus,
            &mut assigned_storage,
            &mut assigned_usb,
        )?);
    }

    let config = SetupConfig {
        guests,
        host_cpus: usize_from_u64(host_cpus)?,
        host_ram_mb,
    };

    let review: Vec<String> = generate_config(&config)?
        .lines()
        .map(str::to_string)
        .collect();
    prompter.info("Review the generated configuration", &review)?;
    if !prompter.confirm("Accept this configuration?", true)? {
        anyhow::bail!("wizard cancelled: configuration not accepted");
    }
    Ok(config)
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

/// Command-line arguments for the setup wizard.
#[derive(Debug, Parser)]
#[command(name = "enlil-setup", version, about = "Enlil first-run setup wizard")]
struct Args {
    /// Answer every prompt with defaults and print the resulting config
    /// instead of writing it.
    #[arg(long)]
    dry_run: bool,
    /// Where to write the generated config (interactive mode only).
    #[arg(short, long, default_value = "enlil.toml")]
    output: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    println!("{BANNER}");

    println!("[1/4] Detecting hardware...\n");
    let hw = detect_hardware();
    println!("{hw}\n");

    println!("[2/4] Running setup wizard...\n");
    if args.dry_run {
        let mut prompter = ScriptedPrompter::default();
        let config = run_wizard(&hw, &mut prompter)?;
        println!("[3/4] Dry run — prompts answered with defaults:\n");
        for line in &prompter.transcript {
            println!("  {line}");
        }
        println!("\n[4/4] Generated configuration (not written):\n");
        println!("{}", generate_config(&config)?);
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        anyhow::bail!("stdin is not a terminal; re-run with --dry-run for non-interactive use");
    }
    // Scope the TUI so the terminal is restored before the summary prints.
    let config = {
        let mut prompter = tui::TuiPrompter::new()?;
        run_wizard(&hw, &mut prompter)?
    };

    println!("[3/4] Generating configuration...\n");
    let toml_output = generate_config(&config)?;
    println!("{toml_output}");

    println!("[4/4] Writing configuration...\n");
    write_config(&config, &args.output)?;
    println!("Done. Wrote {}", args.output.display());

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

    /// Deterministic 16-CPU / 64 GB inventory so wizard tests don't depend on
    /// the live host.
    fn example_hardware() -> HardwareInfo {
        HardwareInfo {
            cpus: 16,
            ram_mb: 65536,
            gpus: vec!["gpu-a".into(), "gpu-b".into()],
            nvme_drives: vec![],
            usb_controllers: vec![],
            nics: vec![],
            iommu_groups: vec![],
        }
    }

    fn run_with(hw: &HardwareInfo, answers: Vec<ScriptedAnswer>) -> Result<SetupConfig> {
        let mut prompter = ScriptedPrompter::new(answers);
        run_wizard(hw, &mut prompter)
    }

    #[test]
    fn wizard_produces_guests() {
        let hw = detect_hardware();
        let config = run_with(&hw, vec![]).expect("wizard");
        assert!(!config.guests.is_empty());
    }

    #[test]
    fn wizard_allocations_fit_hardware() {
        // Scripted answers accept every default, so the totals must fit the
        // fixed example inventory exactly.
        let hw = example_hardware();
        let config = run_with(&hw, vec![]).expect("wizard");
        let guest_cpus: usize = config.guests.iter().map(|g| g.cpus).sum();
        let guest_ram: u64 = config.guests.iter().map(|g| g.ram_mb).sum();
        assert!(config.host_cpus + guest_cpus <= hw.cpus);
        assert!(config.host_ram_mb + guest_ram <= hw.ram_mb);
    }

    #[test]
    fn dry_run_defaults_fit_and_are_logged() {
        let hw = example_hardware();
        let mut prompter = ScriptedPrompter::default();
        let config = run_wizard(&hw, &mut prompter).expect("wizard");
        assert_eq!(config.host_cpus, 2);
        assert_eq!(config.host_ram_mb, 4096);
        assert_eq!(config.guests.len(), 2);
        assert_eq!(config.guests[0].name, "guest-1");
        assert_eq!(config.guests[1].name, "guest-2");
        let guest_cpus: usize = config.guests.iter().map(|g| g.cpus).sum();
        let guest_ram: u64 = config.guests.iter().map(|g| g.ram_mb).sum();
        assert_eq!(config.host_cpus + guest_cpus, hw.cpus);
        assert_eq!(config.host_ram_mb + guest_ram, hw.ram_mb);
        // The transcript proves every prompt ran non-interactively.
        assert!(prompter.transcript.len() > 10);
        assert!(
            prompter
                .transcript
                .iter()
                .any(|line| line.contains("CPUs to reserve"))
        );
    }

    #[test]
    fn passthrough_devices_are_offered_only_once() {
        // Guest 1 takes gpu-a; guest 2 must then only be offered gpu-b.
        // Scripted answers are positional: cover every prompt in order.
        let config = run_with(
            &example_hardware(),
            vec![
                ScriptedAnswer::Default,        // host CPUs
                ScriptedAnswer::Default,        // host RAM
                ScriptedAnswer::Number(2),      // 2 guests
                ScriptedAnswer::Default,        // guest-1 name
                ScriptedAnswer::Default,        // guest-1 vCPUs
                ScriptedAnswer::Default,        // guest-1 RAM
                ScriptedAnswer::Multi(vec![0]), // guest-1 takes gpu-a
                ScriptedAnswer::Default,        // guest-2 name
                ScriptedAnswer::Default,        // guest-2 vCPUs
                ScriptedAnswer::Default,        // guest-2 RAM
                ScriptedAnswer::Multi(vec![0]), // guest-2 takes the remaining gpu-b
            ],
        )
        .expect("wizard");
        assert_eq!(config.guests.len(), 2);
        assert_eq!(config.guests[0].gpus, vec!["gpu-a".to_string()]);
        assert_eq!(config.guests[1].gpus, vec!["gpu-b".to_string()]);
    }

    #[test]
    fn declining_the_review_cancels_the_wizard() {
        // Scripted answers are positional: cover every prompt in order.
        let err = run_with(
            &example_hardware(),
            vec![
                ScriptedAnswer::Default,        // host CPUs
                ScriptedAnswer::Default,        // host RAM
                ScriptedAnswer::Number(1),      // single guest: fewer prompts
                ScriptedAnswer::Default,        // guest name
                ScriptedAnswer::Default,        // guest vCPUs
                ScriptedAnswer::Default,        // guest RAM
                ScriptedAnswer::Default,        // GPU passthrough: none
                ScriptedAnswer::Confirm(false), // decline the review
            ],
        )
        .expect_err("declining the review must fail");
        assert!(err.to_string().contains("cancelled"));
    }

    #[test]
    fn scripted_number_outside_range_is_rejected() {
        // First prompt is host CPUs on a 16-CPU box: max 15, so 99 must fail.
        let err = run_with(&example_hardware(), vec![ScriptedAnswer::Number(99)])
            .expect_err("out-of-range answer must fail");
        assert!(err.to_string().contains("between 1 and 15"));
    }

    #[test]
    fn wizard_rejects_undersized_hardware() {
        let mut tiny = example_hardware();
        tiny.cpus = 1;
        assert!(run_with(&tiny, vec![]).is_err());
        tiny.cpus = 16;
        tiny.ram_mb = 512;
        assert!(run_with(&tiny, vec![]).is_err());
    }

    #[test]
    fn parse_bounded_u64_accepts_only_the_range() {
        assert_eq!(parse_bounded_u64("5", 1, 10), Ok(5));
        assert_eq!(parse_bounded_u64("  10  ", 1, 10), Ok(10));
        assert!(parse_bounded_u64("0", 1, 10).is_err());
        assert!(parse_bounded_u64("11", 1, 10).is_err());
        assert!(parse_bounded_u64("abc", 1, 10).is_err());
        assert!(parse_bounded_u64("", 1, 10).is_err());
    }

    #[test]
    fn validate_guest_name_rejects_bad_names() {
        let taken = vec!["guest-1".to_string()];
        assert!(validate_guest_name("web", &taken).is_ok());
        assert!(validate_guest_name("web-2_x", &taken).is_ok());
        assert!(validate_guest_name("", &taken).is_err());
        assert!(validate_guest_name("   ", &taken).is_err());
        assert!(validate_guest_name("has space", &taken).is_err());
        assert!(validate_guest_name("semi;colon", &taken).is_err());
        assert!(validate_guest_name("guest-1", &taken).is_err());
        assert!(validate_guest_name(&"a".repeat(65), &taken).is_err());
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
    fn classifies_pci_network_controllers() {
        assert!(is_network_controller("0x020000")); // Ethernet
        assert!(is_network_controller("0x028000\n")); // other network, trailing newline
        assert!(!is_network_controller("0x030000")); // display, not network
        assert!(!is_network_controller("0x0c0330")); // USB, not network
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
        let config = run_with(&example_hardware(), vec![]).expect("wizard");
        let output = generate_config(&config).expect("serialization failed");
        assert!(output.contains("guest-1"));
        assert!(output.contains("guest-2"));

        // Round-trip: parse back
        let parsed: SetupConfig = toml::from_str(&output).expect("deserialization failed");
        assert_eq!(parsed.guests.len(), config.guests.len());
        assert_eq!(parsed.host_cpus, config.host_cpus);
    }

    #[test]
    fn write_config_round_trips_through_a_file() {
        let config = run_with(&example_hardware(), vec![]).expect("wizard");
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
