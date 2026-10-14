//! GPU capability detection and sharing-tier recommendation (Phase 7.1).
//!
//! Enlil 1.0 must detect what GPUs the host has and recommend a sharing tier
//! for each instead of failing silently when a config asks for GPU sharing the
//! hardware cannot do. This module is the detection + decision layer:
//!
//! - [`detect_gpus`] turns a PCI function list (e.g. from an ECAM walk) into
//!   per-GPU [`GpuCapabilities`]: vendor, integrated/discrete heuristic,
//!   SR-IOV capability decode, and NVIDIA MIG capability.
//! - [`probe_sysfs_gpus`] does the same from a live Linux host's PCI sysfs
//!   tree (`/sys/bus/pci/devices`).
//! - [`recommend_tier`] picks the best tier a GPU's hardware supports.
//! - [`resolve_strategy`] validates an operator-pinned
//!   [`GpuStrategy`](enlil_config::GpuStrategy) (`[gpu] strategy`) against the
//!   detected capabilities, failing loudly on a mismatch instead of silently
//!   falling back.
//!
//! The full tier *implementations* beyond whole-GPU passthrough and
//! VirtIO-GPU are post-1.0 (T-7.2…T-7.5); each [`GpuRecommendation`] carries
//! whether its tier is implemented yet so later stages can fall back to
//! passthrough until the tier lands.

#[cfg(target_os = "linux")]
use crate::net::{pci_attr_hex, pci_config};
use crate::pci_discovery::{
    ExtendedCapability, PciFunction, SrIovCapability, extended_capabilities_in_config,
};
use enlil_config::GpuStrategy;
#[cfg(target_os = "linux")]
use std::path::Path;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Vendors and device identity
// ---------------------------------------------------------------------------

/// PCI vendor ID for Intel GPUs (0x8086).
pub const VENDOR_INTEL: u16 = 0x8086;
/// PCI vendor ID for AMD/ATI GPUs (0x1002 — distinct from AMD's 0x1022 CPU/
/// chipset ID).
pub const VENDOR_AMD: u16 = 0x1002;
/// PCI vendor ID for NVIDIA GPUs (0x10DE).
pub const VENDOR_NVIDIA: u16 = 0x10DE;

/// A GPU's vendor, from its PCI vendor ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    /// Intel (0x8086).
    Intel,
    /// AMD/ATI (0x1002).
    Amd,
    /// NVIDIA (0x10DE).
    Nvidia,
    /// Any other vendor ID (carried so diagnostics can name it).
    Other(u16),
}

impl GpuVendor {
    /// Map a PCI vendor ID to a [`GpuVendor`].
    #[must_use]
    pub const fn from_vendor_id(vendor_id: u16) -> Self {
        match vendor_id {
            VENDOR_INTEL => Self::Intel,
            VENDOR_AMD => Self::Amd,
            VENDOR_NVIDIA => Self::Nvidia,
            other => Self::Other(other),
        }
    }

    /// Short human-readable vendor name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Intel => "Intel",
            Self::Amd => "AMD",
            Self::Nvidia => "NVIDIA",
            Self::Other(_) => "unknown vendor",
        }
    }
}

/// Whether a GPU is integrated into the host SoC/chipset or a discrete card.
///
/// This is a heuristic (Intel/AMD display controllers on bus 0 are the
/// platform's integrated GPU; everything else is treated as discrete), used
/// only to attach warnings — e.g. passing through the host's boot GPU leaves
/// the hypervisor console without output. It never changes the recommended
/// tier by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    /// Integrated into the host SoC/chipset (often the boot display).
    Integrated,
    /// Discrete `PCIe` card.
    Discrete,
}

/// NVIDIA data-center device IDs known to support MIG (Multi-Instance GPU).
///
/// Representative, not exhaustive — absence from this table does not prove a
/// GPU lacks MIG; it only means the detector cannot *confirm* it. New
/// generations extend the table (T-7.3b owns NVML-based confirmation).
const NVIDIA_MIG_DEVICE_IDS: [u16; 6] = [
    0x20B0, // A100 SXM4 40GB
    0x20B2, // A100 PCIe 40GB
    0x20B5, // A100 PCIe 80GB
    0x20B7, // A30
    0x233A, // H100 SXM5 80GB
    0x26B5, // H200 SXM 141GB
];

/// Whether an NVIDIA device ID is in the known-MIG-capable table.
///
/// Returns `false` for non-NVIDIA GPUs (callers gate on the vendor first).
#[must_use]
pub fn is_mig_capable_device(device_id: u16) -> bool {
    NVIDIA_MIG_DEVICE_IDS.contains(&device_id)
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// What the detector learned about one physical GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuCapabilities {
    /// `(bus, device, function)` of the GPU's display-controller function.
    pub bdf: (u8, u8, u8),
    /// GPU vendor.
    pub vendor: GpuVendor,
    /// PCI device ID.
    pub device_id: u16,
    /// Integrated vs discrete (heuristic — see [`GpuKind`]).
    pub kind: GpuKind,
    /// Decoded SR-IOV extended capability, if the GPU advertises one.
    pub sr_iov: Option<SrIovCapability>,
    /// Whether the GPU is a known NVIDIA MIG-capable part.
    pub mig_capable: bool,
}

impl GpuCapabilities {
    /// Whether this GPU can serve SR-IOV-style sharing (hardware VFs or
    /// NVIDIA MIG instances).
    #[must_use]
    pub fn supports_sr_iov_tier(&self) -> bool {
        self.mig_capable || self.sr_iov.as_ref().is_some_and(|cap| cap.total_vfs > 0)
    }

    /// `bus:device.function` rendering of the BDF, e.g. `00:02.0`.
    #[must_use]
    pub fn bdf_string(&self) -> String {
        format_bdf(self.bdf.0, self.bdf.1, self.bdf.2)
    }
}

/// Render a BDF as `bb:dd.f`, e.g. `00:02.0`.
#[must_use]
pub fn format_bdf(bus: u8, device: u8, function: u8) -> String {
    format!("{bus:02x}:{device:02x}.{function}")
}

/// Classify a GPU as integrated or discrete from its vendor and bus.
///
/// Intel/AMD display controllers on bus 0 are the platform's integrated GPU;
/// anything else is treated as a discrete card. Heuristic only — used for
/// warnings, never to change the recommended tier.
#[must_use]
pub const fn classify_kind(vendor: GpuVendor, bus: u8) -> GpuKind {
    match vendor {
        GpuVendor::Intel | GpuVendor::Amd if bus == 0 => GpuKind::Integrated,
        _ => GpuKind::Discrete,
    }
}

/// Detect GPUs from a PCI function list (e.g. an ECAM walk's output).
///
/// Filters to display controllers (base class `0x03`) and decodes each one's
/// capabilities; `sr_iov_of` supplies the SR-IOV decode for a function (the
/// ECAM-image variant is
/// [`sr_iov_capability`](crate::pci_discovery::sr_iov_capability)). The result
/// is sorted by BDF for deterministic output.
pub fn detect_gpus(
    functions: &[PciFunction],
    sr_iov_of: impl Fn(&PciFunction) -> Option<SrIovCapability>,
) -> Vec<GpuCapabilities> {
    let mut gpus: Vec<GpuCapabilities> = functions
        .iter()
        .filter(|f| f.is_display_controller())
        .map(|f| {
            let vendor = GpuVendor::from_vendor_id(f.vendor_id);
            let mig_capable = vendor == GpuVendor::Nvidia && is_mig_capable_device(f.device_id);
            GpuCapabilities {
                bdf: f.bdf(),
                vendor,
                device_id: f.device_id,
                kind: classify_kind(vendor, f.bus),
                sr_iov: sr_iov_of(f),
                mig_capable,
            }
        })
        .collect();
    gpus.sort_by_key(|g| g.bdf);
    gpus
}

// ---------------------------------------------------------------------------
// Tiers and recommendations
// ---------------------------------------------------------------------------

/// The GPU sharing tiers (Phase 7).
///
/// Mirrors [`GpuStrategy`](enlil_config::GpuStrategy) minus the config-only
/// `Auto`/`None` selectors; [`GpuTier::as_strategy`] converts back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuTier {
    /// Tier 1: assign the whole GPU's IOMMU group to one guest via VFIO.
    Passthrough,
    /// Tier 2: SR-IOV VFs (2a/2c/2d) or NVIDIA MIG instances (2b), one per
    /// guest.
    SrIov,
    /// Tier 2.5: VirtIO-GPU paravirtual device (software-rendering fallback).
    VirtioGpu,
    /// Tier 3: mediated passthrough — trap command submission and multiplex
    /// (post-1.0, T-7.4).
    Mediated,
    /// Tier 4: time-sliced exclusive access with state save/restore
    /// (post-1.0, T-7.5).
    TimeSliced,
}

impl GpuTier {
    /// Short human-readable tier name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::SrIov => "SR-IOV",
            Self::VirtioGpu => "VirtIO-GPU",
            Self::Mediated => "mediated",
            Self::TimeSliced => "time-sliced",
        }
    }

    /// Whether the tier is fully implemented in Enlil 1.0.
    ///
    /// SR-IOV detection + the VF-enable decision layer landed (T-7.3d) but the
    /// live enable/assign path is T-7.3; mediated and time-sliced are T-7.4 /
    /// T-7.5. A recommendation for an unimplemented tier is still useful (it
    /// names the hardware's best fit), and callers use this flag to fall back
    /// to passthrough until the tier lands.
    #[must_use]
    pub const fn implemented_in_1_0(&self) -> bool {
        match self {
            Self::Passthrough | Self::VirtioGpu => true,
            Self::SrIov | Self::Mediated | Self::TimeSliced => false,
        }
    }

    /// Convert back to the config-facing strategy spelling.
    #[must_use]
    pub const fn as_strategy(&self) -> GpuStrategy {
        match self {
            Self::Passthrough => GpuStrategy::Passthrough,
            Self::SrIov => GpuStrategy::SrIov,
            Self::VirtioGpu => GpuStrategy::VirtioGpu,
            Self::Mediated => GpuStrategy::Mediated,
            Self::TimeSliced => GpuStrategy::TimeSliced,
        }
    }
}

/// The tier recommendation for one GPU (or for the host when no GPU exists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuRecommendation {
    /// The recommended tier.
    pub tier: GpuTier,
    /// Why this tier was chosen (mentions the capability that drove it).
    pub reason: String,
    /// Non-fatal caveats — e.g. AMD iGPU reset-bug warnings. Never empty for
    /// a tier that is not yet implemented in 1.0.
    pub warnings: Vec<String>,
}

impl GpuRecommendation {
    /// Whether the recommended tier is fully implemented in Enlil 1.0.
    #[must_use]
    pub const fn implemented(&self) -> bool {
        self.tier.implemented_in_1_0()
    }
}

/// Recommend the best sharing tier a GPU's hardware supports.
///
/// Preference order: MIG instances (NVIDIA data-center) → SR-IOV VFs →
/// whole-GPU passthrough. The recommendation names post-1.0 tiers when the
/// hardware's best fit is one (`implemented()` is then `false` and the reason
/// says which roadmap task owns the enablement), so a caller can fall back to
/// passthrough for 1.0 rather than failing silently.
#[must_use]
pub fn recommend_tier(gpu: &GpuCapabilities) -> GpuRecommendation {
    let bdf = gpu.bdf_string();
    let vendor_name = gpu.vendor.name();
    if gpu.mig_capable {
        return GpuRecommendation {
            tier: GpuTier::SrIov,
            reason: format!(
                "GPU {bdf} ({vendor_name} {device:#06x}) is MIG-capable: carve \
                 per-guest MIG instances (tier 2b)",
                device = gpu.device_id,
            ),
            warnings: vec![
                "MIG instance creation/assignment lands in T-7.3b; until then \
                 fall back to whole-GPU passthrough"
                    .to_string(),
            ],
        };
    }
    if let Some(cap) = &gpu.sr_iov
        && cap.total_vfs > 0
    {
        return GpuRecommendation {
            tier: GpuTier::SrIov,
            reason: format!(
                "GPU {bdf} ({vendor_name} {device:#06x}) advertises SR-IOV \
                 with {} VFs: assign one VF per guest",
                cap.total_vfs,
                device = gpu.device_id,
            ),
            warnings: vec![
                "VF enablement/assignment lands in T-7.3/T-7.3d; until \
                 then fall back to whole-GPU passthrough"
                    .to_string(),
            ],
        };
    }
    let mut warnings = Vec::new();
    if gpu.vendor == GpuVendor::Amd && gpu.kind == GpuKind::Integrated {
        warnings.push(
            "AMD integrated GPUs have known FLR/reset bugs: passthrough may \
             be unstable across guest reboots and the host may not recover the \
             GPU (see T-7.2)"
                .to_string(),
        );
    }
    if gpu.kind == GpuKind::Integrated {
        warnings.push(
            "this looks like the host's primary display GPU: passing it \
             through leaves the hypervisor console without output"
                .to_string(),
        );
    }
    GpuRecommendation {
        tier: GpuTier::Passthrough,
        reason: format!(
            "GPU {bdf} ({vendor_name} {device:#06x}) exposes no hardware \
             sharing features: assign the whole GPU via VFIO (tier 1)",
            device = gpu.device_id,
        ),
        warnings,
    }
}

/// Fallback recommendation when the host has no physical GPUs at all: guests
/// get VirtIO-GPU software rendering.
#[must_use]
pub fn no_gpu_fallback() -> GpuRecommendation {
    GpuRecommendation {
        tier: GpuTier::VirtioGpu,
        reason: "no physical GPUs detected: guests fall back to VirtIO-GPU \
                 software rendering"
            .to_string(),
        warnings: Vec::new(),
    }
}

/// Why an operator-pinned GPU strategy could not be honored.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StrategyError {
    /// The tier is valid but this GPU's hardware cannot do it.
    #[error("GPU {bdf} does not support the '{tier}' tier: {detail}")]
    TierUnsupported {
        /// BDF of the GPU that cannot honor the strategy.
        bdf: String,
        /// Requested tier name.
        tier: &'static str,
        /// What capability is missing.
        detail: String,
    },
    /// The tier names a post-1.0 implementation (also rejected at config
    /// validation; this is the programmatic path).
    #[error(
        "the '{tier}' tier is not implemented in Enlil 1.0 ({task}); use 'auto', 'passthrough', 'sr-iov', or 'virtio-gpu'"
    )]
    TierNotImplemented {
        /// Requested tier name.
        tier: &'static str,
        /// Roadmap task that owns the implementation.
        task: &'static str,
    },
}

/// Resolve the `[gpu] strategy` against the detected GPUs.
///
/// - `Auto` → per-GPU [`recommend_tier`]; with no GPUs, the
///   [`no_gpu_fallback`] VirtIO-GPU recommendation.
/// - `None` → empty (the operator explicitly assigned no GPU to any guest).
/// - A concrete tier → the same tier for every GPU, validated against each
///   GPU's capabilities. A mismatch is a [`StrategyError`], never a silent
///   fallback — e.g. requesting `sr-iov` for a GPU with no SR-IOV capability
///   names the GPU and the missing capability.
///
/// # Errors
///
/// Returns [`StrategyError`] when a pinned tier is post-1.0 or a GPU's
/// hardware cannot honor it.
pub fn resolve_strategy(
    gpus: &[GpuCapabilities],
    strategy: GpuStrategy,
) -> Result<Vec<GpuRecommendation>, StrategyError> {
    match strategy {
        GpuStrategy::Auto => {
            if gpus.is_empty() {
                Ok(vec![no_gpu_fallback()])
            } else {
                Ok(gpus.iter().map(recommend_tier).collect())
            }
        }
        GpuStrategy::None => Ok(Vec::new()),
        GpuStrategy::Mediated => Err(StrategyError::TierNotImplemented {
            tier: "mediated",
            task: "T-7.4",
        }),
        GpuStrategy::TimeSliced => Err(StrategyError::TierNotImplemented {
            tier: "time-sliced",
            task: "T-7.5",
        }),
        GpuStrategy::Passthrough => resolve_pinned(gpus, strategy, GpuTier::Passthrough),
        GpuStrategy::SrIov => resolve_pinned(gpus, strategy, GpuTier::SrIov),
        GpuStrategy::VirtioGpu => resolve_pinned(gpus, strategy, GpuTier::VirtioGpu),
    }
}

/// Resolve a concrete pinned tier against every detected GPU.
///
/// Shared by the concrete arms of [`resolve_strategy`]: a GPU that cannot
/// honor the tier yields [`StrategyError::TierUnsupported`]; otherwise the
/// recommendation records the pinned tier plus the hardware-driven warnings
/// (an AMD iGPU reset warning still applies to a pinned passthrough).
fn resolve_pinned(
    gpus: &[GpuCapabilities],
    strategy: GpuStrategy,
    wanted: GpuTier,
) -> Result<Vec<GpuRecommendation>, StrategyError> {
    gpus.iter()
        .map(|gpu| {
            let ok = match wanted {
                // Whole-GPU assignment and software rendering need no
                // hardware features.
                GpuTier::Passthrough | GpuTier::VirtioGpu => true,
                GpuTier::SrIov => gpu.supports_sr_iov_tier(),
                // Post-1.0 tiers never reach here (rejected by the caller).
                GpuTier::Mediated | GpuTier::TimeSliced => false,
            };
            if ok {
                let mut rec = GpuRecommendation {
                    tier: wanted,
                    reason: format!(
                        "operator pinned '{strat}' for GPU {bdf}",
                        strat = tier_name(strategy),
                        bdf = gpu.bdf_string(),
                    ),
                    warnings: Vec::new(),
                };
                rec.warnings.extend(recommend_tier(gpu).warnings);
                Ok(rec)
            } else {
                Err(StrategyError::TierUnsupported {
                    bdf: gpu.bdf_string(),
                    tier: wanted.name(),
                    detail: "GPU advertises no SR-IOV capability and is not a \
                             known MIG-capable part"
                        .to_string(),
                })
            }
        })
        .collect()
}

/// Config-spelling name of a strategy (kebab-case, matching the TOML).
const fn tier_name(strategy: GpuStrategy) -> &'static str {
    match strategy {
        GpuStrategy::Auto => "auto",
        GpuStrategy::Passthrough => "passthrough",
        GpuStrategy::SrIov => "sr-iov",
        GpuStrategy::VirtioGpu => "virtio-gpu",
        GpuStrategy::Mediated => "mediated",
        GpuStrategy::TimeSliced => "time-sliced",
        GpuStrategy::None => "none",
    }
}

// ---------------------------------------------------------------------------
// Live-host probing via PCI sysfs (Linux)
// ---------------------------------------------------------------------------

/// Decode a function's SR-IOV extended capability from a raw config-space blob
/// (e.g. `/sys/bus/pci/devices/<bdf>/config`).
///
/// Blob-relative variant of
/// [`sr_iov_capability`](crate::pci_discovery::sr_iov_capability): the blob
/// *is* the function's own config space, so the register block sits at the
/// capability offset directly. `None` when the blob has no SR-IOV capability
/// or the register block is not backed by the blob (sysfs may hand back only
/// the 256-byte legacy region).
#[must_use]
pub fn sr_iov_capability_in_config(config: &[u8]) -> Option<SrIovCapability> {
    let cap = extended_capabilities_in_config(config)
        .into_iter()
        .find(|c| c.id == ExtendedCapability::SR_IOV)?;
    let base = usize::from(cap.offset);
    let u16_at = |off: usize| {
        config
            .get(base + off..base + off + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |off: usize| {
        config
            .get(base + off..base + off + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    Some(SrIovCapability {
        vf_enabled: u16_at(0x08)? & 0x1 != 0,
        initial_vfs: u16_at(0x0C)?,
        total_vfs: u16_at(0x0E)?,
        num_vfs: u16_at(0x10)?,
        first_vf_offset: u16_at(0x14)?,
        vf_stride: u16_at(0x16)?,
        vf_device_id: u16_at(0x1A)?,
        supported_page_sizes: u32_at(0x1C)?,
        system_page_size: u32_at(0x20)?,
    })
}

/// Parse a PCI sysfs device directory name (`dddd:bb:dd.f`) into
/// `(bus, device, function)`. The segment is validated but not returned —
/// detection keys GPUs by bus/device/function.
fn parse_sysfs_bdf(name: &str) -> Option<(u8, u8, u8)> {
    let (bus_dev, func) = name.rsplit_once('.')?;
    let (_segment, bus, dev) = {
        let mut parts = bus_dev.split(':');
        (parts.next()?, parts.next()?, parts.next()?)
    };
    let bus = u8::from_str_radix(bus, 16).ok()?;
    let dev = u8::from_str_radix(dev, 16).ok()?;
    let func = u8::from_str_radix(func, 16).ok()?;
    (dev <= 0x1F && func <= 0x7).then_some((bus, dev, func))
}

/// Probe a Linux host's PCI sysfs tree for GPUs.
///
/// `devices_root` is `/sys/bus/pci/devices` on a live host; it is a parameter
/// so tests can point it at a fake tree. Each entry's `class`/`vendor`/`device`
/// attributes identify display controllers (base class `0x03`); the `config`
/// attribute feeds SR-IOV detection. Entries that cannot be read are skipped —
/// a partial inventory beats no inventory, and the caller sees exactly which
/// GPUs were found. The result is sorted by BDF.
#[cfg(target_os = "linux")]
#[must_use]
pub fn probe_sysfs_gpus(devices_root: &Path) -> Vec<GpuCapabilities> {
    let entries = std::fs::read_dir(devices_root).map(|rd| {
        rd.filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .collect::<Vec<_>>()
    });
    let Ok(entries) = entries else {
        return Vec::new();
    };
    let mut gpus = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some((bus, dev, func)) = parse_sysfs_bdf(&name) else {
            continue;
        };
        let Ok(class) = pci_attr_hex(devices_root, &name, "class") else {
            continue;
        };
        if (class >> 16) & 0xFF != 0x03 {
            continue;
        }
        let (Ok(vendor), Ok(device_id)) = (
            pci_attr_hex(devices_root, &name, "vendor"),
            pci_attr_hex(devices_root, &name, "device"),
        ) else {
            continue;
        };
        let (Ok(vendor_id), Ok(device_id)) = (u16::try_from(vendor), u16::try_from(device_id))
        else {
            continue;
        };
        let vendor_kind = GpuVendor::from_vendor_id(vendor_id);
        let sr_iov = pci_config(devices_root, &name)
            .ok()
            .and_then(|cfg| sr_iov_capability_in_config(&cfg));
        gpus.push(GpuCapabilities {
            bdf: (bus, dev, func),
            vendor: vendor_kind,
            device_id,
            kind: classify_kind(vendor_kind, bus),
            sr_iov,
            mig_capable: vendor_kind == GpuVendor::Nvidia && is_mig_capable_device(device_id),
        });
    }
    gpus.sort_by_key(|g| g.bdf);
    gpus
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pci_function(
        bus: u8,
        device: u8,
        function: u8,
        vendor_id: u16,
        device_id: u16,
        class_code: u8,
    ) -> PciFunction {
        PciFunction {
            segment: 0,
            bus,
            device,
            function,
            vendor_id,
            device_id,
            revision: 0,
            prog_if: 0,
            subclass: 0,
            class_code,
            header_type: 0,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
        }
    }

    fn no_sr_iov(_: &PciFunction) -> Option<SrIovCapability> {
        None
    }

    fn sr_iov_pf(total_vfs: u16) -> SrIovCapability {
        SrIovCapability {
            vf_enabled: false,
            initial_vfs: total_vfs,
            total_vfs,
            num_vfs: 0,
            first_vf_offset: 16,
            vf_stride: 1,
            vf_device_id: 0x1000,
            supported_page_sizes: 0x555,
            system_page_size: 0x1,
        }
    }

    #[test]
    fn vendor_ids_map_to_vendors() {
        assert_eq!(GpuVendor::from_vendor_id(0x8086), GpuVendor::Intel);
        assert_eq!(GpuVendor::from_vendor_id(0x1002), GpuVendor::Amd);
        assert_eq!(GpuVendor::from_vendor_id(0x10DE), GpuVendor::Nvidia);
        assert_eq!(GpuVendor::from_vendor_id(0x1234), GpuVendor::Other(0x1234));
        assert_eq!(GpuVendor::Intel.name(), "Intel");
    }

    #[test]
    fn mig_table_covers_known_datacenter_parts() {
        for id in [0x20B0, 0x20B2, 0x20B5, 0x20B7, 0x233A, 0x26B5] {
            assert!(is_mig_capable_device(id), "{id:#06x} should be MIG-capable");
        }
        // A consumer GeForce part is not in the table.
        assert!(!is_mig_capable_device(0x2684));
    }

    #[test]
    fn detect_gpus_filters_to_display_controllers() {
        let funcs = vec![
            pci_function(0, 2, 0, 0x8086, 0x9BC5, 0x03), // Intel iGPU
            pci_function(1, 0, 0, 0x10DE, 0x2684, 0x03), // NVIDIA dGPU
            pci_function(0, 31, 3, 0x8086, 0x1234, 0x0C), // USB, not a GPU
        ];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].bdf, (0, 2, 0));
        assert_eq!(gpus[0].vendor, GpuVendor::Intel);
        assert_eq!(gpus[0].kind, GpuKind::Integrated);
        assert_eq!(gpus[1].bdf, (1, 0, 0));
        assert_eq!(gpus[1].vendor, GpuVendor::Nvidia);
        assert_eq!(gpus[1].kind, GpuKind::Discrete);
        assert!(!gpus[1].mig_capable);
    }

    #[test]
    fn detect_gpus_marks_mig_parts() {
        let funcs = vec![pci_function(0x41, 0, 0, 0x10DE, 0x20B0, 0x03)];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].mig_capable);
        assert!(gpus[0].supports_sr_iov_tier());
    }

    #[test]
    fn recommend_prefers_sr_iov_for_virtualizable_gpus() {
        let funcs = vec![pci_function(1, 0, 0, 0x8086, 0x56A0, 0x03)];
        let gpus = detect_gpus(&funcs, |_| Some(sr_iov_pf(7)));
        let rec = recommend_tier(&gpus[0]);
        assert_eq!(rec.tier, GpuTier::SrIov);
        assert!(!rec.implemented());
        assert!(rec.reason.contains('7'));
    }

    #[test]
    fn recommend_mig_gpu_names_mig_tier() {
        let funcs = vec![pci_function(0x41, 0, 0, 0x10DE, 0x20B0, 0x03)];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        let rec = recommend_tier(&gpus[0]);
        assert_eq!(rec.tier, GpuTier::SrIov);
        assert!(rec.reason.contains("MIG"));
    }

    #[test]
    fn recommend_plain_gpu_falls_back_to_passthrough() {
        let funcs = vec![pci_function(1, 0, 0, 0x10DE, 0x2684, 0x03)];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        let rec = recommend_tier(&gpus[0]);
        assert_eq!(rec.tier, GpuTier::Passthrough);
        assert!(rec.implemented());
    }

    #[test]
    fn recommend_amd_igpu_warns_about_reset_bugs() {
        let funcs = vec![pci_function(0, 2, 0, 0x1002, 0x15E7, 0x03)];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        assert_eq!(gpus[0].kind, GpuKind::Integrated);
        let rec = recommend_tier(&gpus[0]);
        assert_eq!(rec.tier, GpuTier::Passthrough);
        assert!(
            rec.warnings.iter().any(|w| w.contains("FLR/reset")),
            "expected AMD iGPU reset-bug warning, got {:?}",
            rec.warnings
        );
    }

    #[test]
    fn resolve_auto_recommends_per_gpu() {
        let funcs = vec![
            pci_function(0, 2, 0, 0x8086, 0x9BC5, 0x03),
            pci_function(1, 0, 0, 0x10DE, 0x2684, 0x03),
        ];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        let recs = resolve_strategy(&gpus, GpuStrategy::Auto).unwrap();
        assert_eq!(recs.len(), 2);
        assert!(recs.iter().all(|r| r.tier == GpuTier::Passthrough));
    }

    #[test]
    fn resolve_auto_with_no_gpus_falls_back_to_virtio_gpu() {
        let recs = resolve_strategy(&[], GpuStrategy::Auto).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].tier, GpuTier::VirtioGpu);
        assert!(recs[0].implemented());
    }

    #[test]
    fn resolve_none_assigns_nothing() {
        let funcs = vec![pci_function(1, 0, 0, 0x10DE, 0x2684, 0x03)];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        let recs = resolve_strategy(&gpus, GpuStrategy::None).unwrap();
        assert!(recs.is_empty());
    }

    #[test]
    fn resolve_pinned_sr_iov_fails_loudly_without_capability() {
        let funcs = vec![pci_function(1, 0, 0, 0x10DE, 0x2684, 0x03)];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        let err = resolve_strategy(&gpus, GpuStrategy::SrIov).unwrap_err();
        assert!(matches!(err, StrategyError::TierUnsupported { .. }));
        let msg = err.to_string();
        assert!(msg.contains("01:00.0"), "error should name the GPU: {msg}");
        assert!(msg.contains("SR-IOV"), "error should name the tier: {msg}");
    }

    #[test]
    fn resolve_pinned_sr_iov_succeeds_with_capability() {
        let funcs = vec![pci_function(1, 0, 0, 0x8086, 0x56A0, 0x03)];
        let gpus = detect_gpus(&funcs, |_| Some(sr_iov_pf(7)));
        let recs = resolve_strategy(&gpus, GpuStrategy::SrIov).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].tier, GpuTier::SrIov);
    }

    #[test]
    fn resolve_post_1_0_tiers_are_rejected() {
        let funcs = vec![pci_function(1, 0, 0, 0x10DE, 0x2684, 0x03)];
        let gpus = detect_gpus(&funcs, no_sr_iov);
        for strategy in [GpuStrategy::Mediated, GpuStrategy::TimeSliced] {
            let err = resolve_strategy(&gpus, strategy).unwrap_err();
            assert!(
                matches!(err, StrategyError::TierNotImplemented { .. }),
                "expected TierNotImplemented for {strategy:?}"
            );
        }
    }

    #[test]
    fn tier_round_trips_to_config_strategy() {
        assert_eq!(GpuTier::Passthrough.as_strategy(), GpuStrategy::Passthrough);
        assert_eq!(GpuTier::SrIov.as_strategy(), GpuStrategy::SrIov);
        assert_eq!(GpuTier::VirtioGpu.as_strategy(), GpuStrategy::VirtioGpu);
        assert_eq!(tier_name(GpuStrategy::SrIov), "sr-iov");
        assert_eq!(tier_name(GpuStrategy::TimeSliced), "time-sliced");
    }

    // -- sysfs probing ------------------------------------------------------

    /// One fake PCI sysfs device for [`fake_sysfs`].
    #[cfg(target_os = "linux")]
    struct FakeDevice {
        /// Sysfs directory name (`dddd:bb:dd.f`).
        bdf: &'static str,
        /// `class` attribute value.
        class: u32,
        /// `vendor` attribute value.
        vendor: u16,
        /// `device` attribute value.
        device: u16,
        /// Optional `config` blob.
        config: Option<Vec<u8>>,
    }

    /// Build a fake `/sys/bus/pci/devices/<bdf>/` tree from [`FakeDevice`]s.
    #[cfg(target_os = "linux")]
    fn fake_sysfs(devices: &[FakeDevice]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for dev in devices {
            let dev_dir = dir.path().join(dev.bdf);
            std::fs::create_dir(&dev_dir).unwrap();
            std::fs::write(
                dev_dir.join("class"),
                format!("{class:#08x}\n", class = dev.class),
            )
            .unwrap();
            std::fs::write(
                dev_dir.join("vendor"),
                format!("{vendor:#06x}\n", vendor = dev.vendor),
            )
            .unwrap();
            std::fs::write(
                dev_dir.join("device"),
                format!("{device:#06x}\n", device = dev.device),
            )
            .unwrap();
            if let Some(bytes) = &dev.config {
                std::fs::write(dev_dir.join("config"), bytes).unwrap();
            }
        }
        dir
    }

    /// Config blob with an SR-IOV extended capability at 0x100 advertising
    /// `total_vfs` VFs.
    #[cfg(target_os = "linux")]
    fn config_with_sr_iov(total_vfs: u16) -> Vec<u8> {
        let mut cfg = vec![0u8; 0x140];
        // Class triple: display controller (0x03), subclass/VGA 0x00.
        cfg[0x0A] = 0x00;
        cfg[0x0B] = 0x03;
        // Extended cap header at 0x100: id 0x10 (SR-IOV), version 1, next 0.
        let header: u32 = 0x0010 | (1 << 16);
        cfg[0x100..0x104].copy_from_slice(&header.to_le_bytes());
        // SR-IOV register block.
        cfg[0x108..0x10A].copy_from_slice(&0u16.to_le_bytes()); // Control
        cfg[0x10C..0x10E].copy_from_slice(&total_vfs.to_le_bytes()); // InitialVFs
        cfg[0x10E..0x110].copy_from_slice(&total_vfs.to_le_bytes()); // TotalVFs
        cfg[0x110..0x112].copy_from_slice(&0u16.to_le_bytes()); // NumVFs
        cfg[0x114..0x116].copy_from_slice(&16u16.to_le_bytes()); // FirstVFOffset
        cfg[0x116..0x118].copy_from_slice(&1u16.to_le_bytes()); // VFStride
        cfg[0x11A..0x11C].copy_from_slice(&0x1000u16.to_le_bytes()); // VFDeviceID
        cfg[0x11C..0x120].copy_from_slice(&0x555u32.to_le_bytes()); // PageSizes
        cfg[0x120..0x124].copy_from_slice(&0x1u32.to_le_bytes()); // SysPageSize
        cfg
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn probe_sysfs_finds_gpus_and_decodes_sr_iov() {
        let dir = fake_sysfs(&[
            FakeDevice {
                bdf: "0000:00:02.0",
                class: 0x0003_0000,
                vendor: 0x8086,
                device: 0x9BC5,
                // Legacy-only config: no extended capabilities.
                config: Some(vec![0u8; 0x40]),
            },
            FakeDevice {
                bdf: "0000:01:00.0",
                class: 0x0003_0200,
                vendor: 0x8086,
                device: 0x56A0,
                config: Some(config_with_sr_iov(7)),
            },
            // Audio controller: not a GPU, skipped.
            FakeDevice {
                bdf: "0000:00:1f.3",
                class: 0x0004_0300,
                vendor: 0x8086,
                device: 0x1234,
                config: None,
            },
        ]);
        let gpus = probe_sysfs_gpus(dir.path());
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].bdf, (0, 2, 0));
        assert_eq!(gpus[0].kind, GpuKind::Integrated);
        assert!(gpus[0].sr_iov.is_none());
        assert_eq!(gpus[1].bdf, (1, 0, 0));
        let cap = gpus[1].sr_iov.as_ref().expect("SR-IOV should decode");
        assert_eq!(cap.total_vfs, 7);
        assert!(gpus[1].supports_sr_iov_tier());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn probe_sysfs_missing_tree_yields_no_gpus() {
        let gpus = probe_sysfs_gpus(Path::new("/nonexistent-pci-devices"));
        assert!(gpus.is_empty());
    }

    #[test]
    fn parse_sysfs_bdf_handles_names() {
        assert_eq!(parse_sysfs_bdf("0000:00:02.0"), Some((0, 2, 0)));
        assert_eq!(parse_sysfs_bdf("0000:41:00.0"), Some((0x41, 0, 0)));
        assert_eq!(parse_sysfs_bdf("junk"), None);
        assert_eq!(parse_sysfs_bdf("0000:00:02"), None);
    }
}

// ---------------------------------------------------------------------------
// Tier 1 GPU passthrough (Phase 7.2) — Linux-only
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod tier1_passthrough {
    use std::fs;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};

    use crate::net::{
        VfioContainer, VfioGroup, VfioPciDevice, bind_driver, iommu_group_of, pci_attr_hex,
        validate_bdf,
    };

    // ---------------------------------------------------------------------------
    // Constants
    // ---------------------------------------------------------------------------

    /// PCI base class for display controllers (GPUs).
    pub const PCI_CLASS_DISPLAY: u8 = 0x03;
    /// AMD vendor ID (`0x1002`).
    pub const AMD_VENDOR_ID: u16 = 0x1002;
    /// NVIDIA vendor ID (`0x10DE`).
    pub const NVIDIA_VENDOR_ID: u16 = 0x10DE;
    /// Intel vendor ID (`0x8086`).
    pub const INTEL_VENDOR_ID: u16 = 0x8086;
    /// The driver a GPU's IOMMU-group functions are bound to for passthrough.
    pub const VFIO_PCI_DRIVER: &str = "vfio-pci";
    /// PCI capability ID for PCI Express.
    const PCI_CAP_ID_EXP: u8 = 0x10;
    /// PCI Express Device Capabilities register offset (from the cap start).
    const PCIE_DEVCAP_OFFSET: usize = 0x04;
    /// PCI Express Device Control register offset (from the cap start).
    const PCIE_DEVCTL_OFFSET: usize = 0x08;
    /// Bit 28 of Device Capabilities: the function supports FLR.
    const PCIE_DEVCAP_FLR: u32 = 1 << 28;
    /// Bit 15 of Device Control: writing 1 initiates a Function-Level Reset.
    const PCIE_DEVCTL_INITIATE_FLR: u16 = 1 << 15;
    /// Expansion ROM signature (`0xAA55` little-endian).
    const ROM_SIGNATURE: u16 = 0xAA55;
    /// PCI Data Structure signature (`"PCIR"`).
    const ROM_PCID_SIG: &[u8; 4] = b"PCIR";

    // ---------------------------------------------------------------------------
    // Enumeration
    // ---------------------------------------------------------------------------

    /// A GPU found on the host: a PCI display controller plus its IOMMU group.
    #[derive(Debug, Clone)]
    pub struct HostGpu {
        /// Canonical BDF of the display function (e.g. `0000:01:00.0`).
        pub bdf: String,
        /// PCI vendor ID (`vendor` sysfs attribute).
        pub vendor_id: u16,
        /// PCI device ID (`device` sysfs attribute).
        pub device_id: u16,
        /// Subsystem vendor ID (`subsystem_vendor` sysfs attribute).
        pub subsystem_vendor_id: u16,
        /// Subsystem ID (`subsystem_device` sysfs attribute).
        pub subsystem_id: u16,
        /// IOMMU group number, when the kernel exposes one.
        pub iommu_group: Option<u32>,
        /// Every PCI function in the GPU's IOMMU group, including `bdf` itself,
        /// sorted for deterministic guest device order. Empty when the group is
        /// unknown — assignment refuses to proceed on an empty list.
        pub group_functions: Vec<String>,
    }

    impl HostGpu {
        /// Human-readable `vendor:device` label (e.g. `1002:731f`).
        #[must_use]
        pub fn label(&self) -> String {
            format!("{:04x}:{:04x}", self.vendor_id, self.device_id)
        }

        /// Whether the function at `bdf` is part of this GPU's assignment unit.
        #[must_use]
        pub fn owns_function(&self, bdf: &str) -> bool {
            self.group_functions.iter().any(|f| f == bdf)
        }
    }

    /// Enumerate every GPU (PCI base class `0x03`) visible under `pci_devices`.
    ///
    /// `pci_devices` is normally `/sys/bus/pci/devices` and `iommu_groups`
    /// normally `/sys/kernel/iommu_groups`; both are parameters so tests can use
    /// fake sysfs trees. Each GPU's IOMMU group and its member functions are
    /// resolved from `iommu_groups`.
    ///
    /// # Errors
    ///
    /// Fails when either root cannot be listed. Individual devices with
    /// unreadable attributes are skipped, not fatal — a half-populated sysfs
    /// snapshot still yields the GPUs it can describe.
    pub fn enumerate_gpus(pci_devices: &Path, iommu_groups: &Path) -> Result<Vec<HostGpu>> {
        let entries = fs::read_dir(pci_devices)
            .with_context(|| format!("listing {}", pci_devices.display()))?;
        let mut gpus = Vec::new();
        for entry in entries {
            let entry = entry?;
            let bdf = entry.file_name().to_string_lossy().into_owned();
            if validate_bdf(&bdf).is_err() {
                continue;
            }
            let Ok(class) = pci_attr_hex(pci_devices, &bdf, "class") else {
                continue;
            };
            if ((class >> 16) & 0xFF) as u8 != PCI_CLASS_DISPLAY {
                continue;
            }
            let Ok(vendor_id) = pci_attr_u16(pci_devices, &bdf, "vendor") else {
                continue;
            };
            let Ok(device_id) = pci_attr_u16(pci_devices, &bdf, "device") else {
                continue;
            };
            let subsystem_vendor_id =
                pci_attr_u16(pci_devices, &bdf, "subsystem_vendor").unwrap_or(0);
            let subsystem_id = pci_attr_u16(pci_devices, &bdf, "subsystem_device").unwrap_or(0);
            let iommu_group = iommu_group_of(pci_devices, &bdf).unwrap_or(None);
            let mut group_functions = if let Some(group) = iommu_group
                && let Ok(members) = group_functions_of(iommu_groups, group)
            {
                members
            } else {
                Vec::new()
            };
            if !group_functions.contains(&bdf) {
                group_functions.push(bdf.clone());
            }
            group_functions.sort();
            gpus.push(HostGpu {
                bdf,
                vendor_id,
                device_id,
                subsystem_vendor_id,
                subsystem_id,
                iommu_group,
                group_functions,
            });
        }
        gpus.sort_by(|a, b| a.bdf.cmp(&b.bdf));
        Ok(gpus)
    }

    /// Enumerate the GPUs on this host from the real sysfs.
    ///
    /// # Errors
    ///
    /// Fails when `/sys/bus/pci/devices` cannot be listed.
    pub fn enumerate_host_gpus() -> Result<Vec<HostGpu>> {
        enumerate_gpus(
            Path::new("/sys/bus/pci/devices"),
            Path::new("/sys/kernel/iommu_groups"),
        )
    }

    /// All PCI functions in IOMMU group `group`, sorted, as canonical BDFs.
    ///
    /// # Errors
    ///
    /// Fails when the group's `devices` directory cannot be listed.
    fn group_functions_of(iommu_groups: &Path, group: u32) -> Result<Vec<String>> {
        let dir = iommu_groups.join(group.to_string()).join("devices");
        let entries = fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))?;
        let mut functions = Vec::new();
        for entry in entries {
            let bdf = entry?.file_name().to_string_lossy().into_owned();
            if validate_bdf(&bdf).is_ok() {
                functions.push(bdf);
            }
        }
        functions.sort();
        Ok(functions)
    }

    // ---------------------------------------------------------------------------
    // Assignment — bind the whole IOMMU group to vfio-pci and open VFIO fds
    // ---------------------------------------------------------------------------

    /// A whole GPU (its entire IOMMU group) assigned to one guest via VFIO.
    ///
    /// Built by [`GpuPassthrough::assign`]; the VFIO container, group, and per
    /// function device fds stay open for the guest's lifetime. Drop order closes
    /// devices, then the group, then the container.
    pub struct GpuPassthrough {
        /// The enumerated GPU this assignment was built from.
        pub gpu: HostGpu,
        container: VfioContainer,
        group: VfioGroup,
        devices: Vec<VfioPciDevice>,
        pci_devices: PathBuf,
    }

    /// Read a 16-bit hex sysfs attribute (`vendor`, `device`, ...) for a PCI
    /// device, rejecting values that do not fit in 16 bits.
    fn pci_attr_u16(pci_devices: &Path, bdf: &str, attr: &str) -> Result<u16> {
        let raw = pci_attr_hex(pci_devices, bdf, attr)?;
        u16::try_from(raw).with_context(|| format!("{attr} of {bdf} out of 16-bit range: {raw:#x}"))
    }

    impl GpuPassthrough {
        /// Assign the GPU to a guest: bind every function in the GPU's IOMMU
        /// group to `vfio-pci`, then open the VFIO container, group, and a
        /// device fd per function.
        ///
        /// `pci_devices` is `/sys/bus/pci/devices`, `drivers` is
        /// `/sys/bus/pci/drivers` (both injectable for tests).
        ///
        /// # Errors
        ///
        /// Fails when the GPU has no known IOMMU group or no group members
        /// (there is nothing safe to assign), when a driver bind fails, or when
        /// `/dev/vfio/vfio` / the group device is unavailable.
        pub fn assign(pci_devices: &Path, drivers: &Path, gpu: &HostGpu) -> Result<Self> {
            let Some(group_id) = gpu.iommu_group else {
                bail!(
                    "GPU {} has no IOMMU group: cannot assign a group-less device",
                    gpu.bdf
                );
            };
            if gpu.group_functions.is_empty() {
                bail!("GPU {} has no IOMMU group members to assign", gpu.bdf);
            }
            for bdf in &gpu.group_functions {
                bind_driver(pci_devices, drivers, bdf, VFIO_PCI_DRIVER).with_context(|| {
                    format!("binding {bdf} (GPU {}) to {VFIO_PCI_DRIVER}", gpu.bdf)
                })?;
            }
            let container = VfioContainer::open().context("opening VFIO container")?;
            let group = VfioGroup::open(group_id)
                .with_context(|| format!("opening VFIO group {group_id} for GPU {}", gpu.bdf))?;
            group
                .set_container(&container)
                .context("attaching VFIO group to container")?;
            let mut devices = Vec::with_capacity(gpu.group_functions.len());
            for bdf in &gpu.group_functions {
                let dev = group
                    .device(bdf)
                    .with_context(|| format!("getting VFIO device fd for {bdf}"))?;
                devices.push(dev);
            }
            Ok(Self {
                gpu: gpu.clone(),
                container,
                group,
                devices,
                pci_devices: pci_devices.to_path_buf(),
            })
        }

        /// The BDFs handed to the guest, in assignment order.
        #[must_use]
        pub fn assigned_functions(&self) -> Vec<&str> {
            self.devices.iter().map(|d| d.bdf.as_str()).collect()
        }

        /// The VFIO container backing this assignment — guest memory is mapped
        /// for DMA through it.
        #[must_use]
        pub const fn container(&self) -> &VfioContainer {
            &self.container
        }

        /// The VFIO group backing this assignment.
        #[must_use]
        pub const fn group(&self) -> &VfioGroup {
            &self.group
        }

        /// Perform a Function-Level Reset on every assigned function — the guest
        /// reboot path. Tries `VFIO_DEVICE_RESET` first (the supported kernel
        /// path); when the kernel rejects it, falls back to setting the `PCIe`
        /// `InitiateFLR` bit directly in config space.
        ///
        /// Returns one outcome per function, in assignment order. Callers treat
        /// any `Err` as "the GPU may be wedged after reboot" and should surface
        /// [`amd_passthrough_warnings`] guidance (or `no_reset` config) rather
        /// than silently continuing.
        #[must_use]
        pub fn reset_gpu(&self) -> Vec<GpuFunctionReset> {
            self.devices
                .iter()
                .map(|dev| match dev.reset() {
                    Ok(()) => GpuFunctionReset {
                        bdf: dev.bdf.clone(),
                        method: GpuResetMethod::VfioDeviceReset,
                        result: Ok(()),
                    },
                    Err(vfio_err) => match initiate_flr(&self.pci_devices, &dev.bdf) {
                        Ok(()) => GpuFunctionReset {
                            bdf: dev.bdf.clone(),
                            method: GpuResetMethod::ConfigSpaceFlr,
                            result: Ok(()),
                        },
                        Err(flr_err) => GpuFunctionReset {
                            bdf: dev.bdf.clone(),
                            method: GpuResetMethod::ConfigSpaceFlr,
                            result: Err(flr_err
                                .context(format!("VFIO_DEVICE_RESET also failed: {vfio_err:#}"))),
                        },
                    },
                })
                .collect()
        }
    }

    /// Which reset path a [`GpuFunctionReset`] took.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GpuResetMethod {
        /// `VFIO_DEVICE_RESET` ioctl on the VFIO device fd.
        VfioDeviceReset,
        /// `PCIe` config-space `InitiateFLR` bit (fallback when the ioctl fails).
        ConfigSpaceFlr,
    }

    /// Outcome of resetting one GPU function at guest reboot.
    pub struct GpuFunctionReset {
        /// The function that was reset.
        pub bdf: String,
        /// Which reset path ran.
        pub method: GpuResetMethod,
        /// Whether the kernel accepted the reset.
        pub result: Result<()>,
    }

    // ---------------------------------------------------------------------------
    // Function-Level Reset via PCI config space (VFIO_DEVICE_RESET fallback)
    // ---------------------------------------------------------------------------

    /// Locate the PCI Express capability in a config-space image.
    ///
    /// Walks the legacy capability list from offset `0x34`, gated on the
    /// Capabilities List status bit, cycle-guarded like `pci_discovery`.
    const fn find_pcie_capability(config: &[u8]) -> Option<u16> {
        if config.len() < 0x40 {
            return None;
        }
        let status = u16::from_le_bytes([config[0x06], config[0x07]]);
        if status & 0x10 == 0 {
            return None;
        }
        let mut ptr = config[0x34] as u16;
        let mut guard = 0;
        while ptr != 0 && guard < 48 {
            let p = ptr as usize;
            if p + 2 > config.len() {
                return None;
            }
            if config[p] == PCI_CAP_ID_EXP {
                return Some(ptr);
            }
            ptr = config[p + 1] as u16;
            guard += 1;
        }
        None
    }

    /// Whether the `PCIe` function at `cap` advertises FLR (Device Capabilities
    /// bit 28).
    fn pcie_flr_capable(config: &[u8], cap: u16) -> bool {
        let start = cap as usize + PCIE_DEVCAP_OFFSET;
        if start + 4 > config.len() {
            return false;
        }
        let devcap = u32::from_le_bytes(
            config[start..start + 4]
                .try_into()
                .expect("bounds checked above"),
        );
        devcap & PCIE_DEVCAP_FLR != 0
    }

    /// Initiate a Function-Level Reset by setting the `PCIe` `InitiateFLR` bit
    /// (Device Control bit 15) through the sysfs `config` file.
    ///
    /// This is the fallback for when `VFIO_DEVICE_RESET` is rejected; it needs
    /// the same privileges as VFIO itself (root / the device's sysfs owner).
    ///
    /// # Errors
    ///
    /// Fails when the BDF is malformed, the config file cannot be opened, the
    /// device is not `PCIe`, or the function does not advertise FLR support.
    pub fn initiate_flr(pci_devices: &Path, bdf: &str) -> Result<()> {
        validate_bdf(bdf)?;
        let path = pci_devices.join(bdf).join("config");
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut config = vec![0u8; 4096];
        let n = f.read(&mut config).context("reading PCI config space")?;
        let config = &config[..n];
        let cap = find_pcie_capability(config)
            .context("no PCI Express capability: not a PCIe function")?;
        if !pcie_flr_capable(config, cap) {
            bail!("function {bdf} does not advertise FLR (DevCap bit 28 clear)");
        }
        let ctl_off = u64::from(cap) + PCIE_DEVCTL_OFFSET as u64;
        f.seek(SeekFrom::Start(ctl_off))
            .context("seeking to PCIe Device Control")?;
        let mut ctl = [0u8; 2];
        f.read_exact(&mut ctl)
            .context("reading PCIe Device Control")?;
        let updated = u16::from_le_bytes(ctl) | PCIE_DEVCTL_INITIATE_FLR;
        f.seek(SeekFrom::Start(ctl_off))
            .context("seeking to PCIe Device Control")?;
        f.write_all(&updated.to_le_bytes())
            .context("setting PCIe InitiateFLR")?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Option ROM / vBIOS
    // ---------------------------------------------------------------------------

    /// The code type of one expansion-ROM image (PCI Data Structure offset
    /// `0x14`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RomCodeType {
        /// `0x00` — IA-32 PC-AT (classic x86 vBIOS).
        PcAt,
        /// `0x02` — `OpenFirmware`.
        OpenFirmware,
        /// `0x03` — EFI (UEFI driver image).
        Efi,
        /// Any other code type, carried verbatim.
        Unknown(u8),
    }

    impl RomCodeType {
        const fn decode(code: u8) -> Self {
            match code {
                0x00 => Self::PcAt,
                0x02 => Self::OpenFirmware,
                0x03 => Self::Efi,
                other => Self::Unknown(other),
            }
        }
    }

    /// One image inside an expansion ROM (ROMs can chain several images; the
    /// indicator byte's bit 7 marks the last one).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RomImage {
        /// Byte offset of the image from the ROM start.
        pub offset: usize,
        /// Image length in bytes (the PCI Data Structure length field counts
        /// 512-byte blocks).
        pub len: usize,
        /// What machine code this image carries.
        pub code_type: RomCodeType,
        /// Whether this is the last image in the chain.
        pub last: bool,
    }

    /// Validate and decode an expansion-ROM image's structure.
    ///
    /// Checks the `0x55AA` signature of every image, the `PCIR` PCI Data
    /// Structure signature, and that image lengths stay inside `data` — without
    /// executing anything.
    ///
    /// # Errors
    ///
    /// Fails when the ROM is too small, a signature is wrong, the PCI Data
    /// Structure pointer runs out of bounds, or an image length overruns `data`.
    pub fn parse_option_rom(data: &[u8]) -> Result<Vec<RomImage>> {
        if data.len() < 512 {
            bail!(
                "option ROM too small to hold an image header: {} bytes",
                data.len()
            );
        }
        let mut images = Vec::new();
        let mut offset = 0usize;
        for _ in 0..16 {
            if offset + 0x1A > data.len() {
                bail!("ROM image at {offset:#x} is truncated");
            }
            let sig = u16::from_le_bytes([data[offset], data[offset + 1]]);
            if sig != ROM_SIGNATURE {
                bail!("bad expansion ROM signature {sig:#06x} at {offset:#x}");
            }
            let ds_ptr = u16::from_le_bytes([data[offset + 0x18], data[offset + 0x19]]) as usize;
            let ds = offset + ds_ptr;
            if ds + 0x16 > data.len() {
                bail!("PCI Data Structure pointer {ds_ptr:#x} at {offset:#x} runs out of bounds");
            }
            if &data[ds..ds + 4] != ROM_PCID_SIG {
                bail!("bad PCI Data Structure signature at {ds:#x}");
            }
            let blocks = u16::from_le_bytes([data[ds + 0x10], data[ds + 0x11]]) as usize;
            if blocks == 0 {
                bail!("zero-length ROM image at {offset:#x}");
            }
            let len = blocks * 512;
            if offset + len > data.len() {
                bail!(
                    "ROM image at {offset:#x} claims {len} bytes but only {} remain",
                    data.len() - offset
                );
            }
            let code_type = RomCodeType::decode(data[ds + 0x14]);
            let indicator = data[ds + 0x15];
            let last = indicator & 0x80 != 0;
            images.push(RomImage {
                offset,
                len,
                code_type,
                last,
            });
            if last {
                return Ok(images);
            }
            offset += len;
        }
        bail!("ROM image chain did not terminate after 16 images");
    }

    /// Load a GPU's option ROM (vBIOS) from sysfs and validate its structure.
    ///
    /// Reads `rom` under the device's sysfs directory (enabling it first, as
    /// the kernel requires). The returned image is ready to hand to the guest's
    /// firmware; [`parse_option_rom`] has already validated the signatures.
    ///
    /// # Errors
    ///
    /// Fails when the BDF is malformed, the `rom` attribute cannot be enabled
    /// or read (the device is usually unbound or its ROM is unreadable), or the
    /// image fails structural validation.
    pub fn read_option_rom(pci_devices: &Path, bdf: &str) -> Result<VbiosImage> {
        validate_bdf(bdf)?;
        let rom = pci_devices.join(bdf).join("rom");
        fs::write(&rom, b"1")
            .with_context(|| format!("enabling option ROM at {}", rom.display()))?;
        let data = fs::read(&rom).with_context(|| format!("reading {}", rom.display()));
        // Best effort: leave the ROM disabled even when the read failed.
        let _ = fs::write(&rom, b"0");
        let data = data?;
        let images = parse_option_rom(&data)?;
        Ok(VbiosImage { data, images })
    }

    /// A GPU's validated option ROM (vBIOS) image.
    pub struct VbiosImage {
        /// The raw ROM bytes, as read from sysfs.
        pub data: Vec<u8>,
        /// The decoded image chain (offsets into `data`).
        pub images: Vec<RomImage>,
    }

    impl VbiosImage {
        /// Whether any image in the ROM is a UEFI (EFI) driver image.
        #[must_use]
        pub fn has_efi_image(&self) -> bool {
            self.images
                .iter()
                .any(|img| img.code_type == RomCodeType::Efi)
        }
    }

    // ---------------------------------------------------------------------------
    // AMD GPU generation detection and passthrough warnings
    // ---------------------------------------------------------------------------

    /// The AMD GPU generation, from PCI vendor/device IDs. Best-effort: IDs the
    /// table does not know return [`AmdGpuFamily::Unknown`] rather than a guess.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AmdGpuFamily {
        /// Not an AMD GPU, or an AMD device ID this table does not recognize.
        Unknown,
        /// AMD APU integrated graphics (Raven Ridge, Renoir, Van Gogh,
        /// Rembrandt, Phoenix, Strix Point, ...).
        Integrated,
        /// Pre-Polaris GCN (Southern/Caribbean/Arctic Islands: HD 7000 through
        /// R9 Fury).
        LegacyGcn,
        /// GCN 4 "Polaris" (RX 400/500 series) — famous reset bug.
        Polaris,
        /// GCN 5 "Vega" (RX Vega, Radeon VII) — reset quirks.
        Vega,
        /// RDNA 1 "Navi 1x" (RX 5000 series) — reset bug.
        Navi1x,
        /// RDNA 2 "Navi 2x" (RX 6000 series).
        Navi2x,
        /// RDNA 3 "Navi 3x" (RX 7000 series).
        Navi3x,
        /// RDNA 4 "Navi 4x" (RX 9000 series).
        Navi4x,
        /// CDNA (Instinct MI100/MI200 compute cards).
        Cdna,
    }

    /// Whether `device_id` is an AMD APU integrated GPU.
    const fn is_amd_igpu(device_id: u16) -> bool {
        matches!(
            device_id,
            0x15D8 | 0x15DD | // Raven Ridge
            0x1636 | 0x1638 | // Renoir
            0x164C | // Van Gogh (Steam Deck APU)
            0x1681 | // Rembrandt
            0x15BF | 0x15C8 | // Phoenix
            0x13C0 // Strix Point
        )
    }

    /// Classify an AMD GPU (`vendor_id` should be [`AMD_VENDOR_ID`]) into its
    /// generation.
    ///
    /// # Example
    ///
    /// ```
    /// use enlil_devices::gpu::{AMD_VENDOR_ID, AmdGpuFamily, amd_gpu_family};
    ///
    /// assert_eq!(
    ///     amd_gpu_family(AMD_VENDOR_ID, 0x731F),
    ///     AmdGpuFamily::Navi1x
    /// );
    /// ```
    #[must_use]
    pub fn amd_gpu_family(vendor_id: u16, device_id: u16) -> AmdGpuFamily {
        if vendor_id != AMD_VENDOR_ID {
            return AmdGpuFamily::Unknown;
        }
        if is_amd_igpu(device_id) {
            return AmdGpuFamily::Integrated;
        }
        if (0x67C0..=0x67DF).contains(&device_id) || matches!(device_id, 0x67EF | 0x6FDF) {
            AmdGpuFamily::Polaris
        } else if (0x6860..=0x687F).contains(&device_id) || device_id == 0x66AF {
            AmdGpuFamily::Vega
        } else if (0x7310..=0x731F).contains(&device_id)
            || matches!(device_id, 0x7340 | 0x7341 | 0x7347)
        {
            AmdGpuFamily::Navi1x
        } else if (0x73A0..=0x73FF).contains(&device_id) {
            AmdGpuFamily::Navi2x
        } else if (0x7440..=0x745F).contains(&device_id) {
            AmdGpuFamily::Navi3x
        } else if (0x7590..=0x75BF).contains(&device_id) {
            AmdGpuFamily::Navi4x
        } else if matches!(device_id, 0x738C | 0x7408 | 0x740C | 0x740F) {
            AmdGpuFamily::Cdna
        } else if (0x6700..0x7400).contains(&device_id) {
            AmdGpuFamily::LegacyGcn
        } else {
            AmdGpuFamily::Unknown
        }
    }

    /// A known passthrough hazard for an AMD GPU.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AmdPassthroughWarning {
        /// AMD APU integrated graphics: unbinding the iGPU from the host is
        /// unstable on most platforms (shared memory controller / firmware with
        /// the host CPU) and can hang the host.
        IgpuInstability,
        /// The card's generation has the well-known AMD GPU reset bug: VFIO FLR
        /// may leave the card wedged after a guest reboot, needing the
        /// out-of-tree `vendor-reset` module or a host reboot.
        ResetBug,
    }

    impl AmdPassthroughWarning {
        /// The user-facing explanation of the hazard.
        #[must_use]
        pub const fn describe(self) -> &'static str {
            match self {
                Self::IgpuInstability => {
                    "AMD iGPU passthrough is unstable: the APU graphics share the \
                     memory controller and firmware with the host CPU, and unbinding \
                     them from the host can hang the machine. Keep the iGPU for the \
                     host console and pass through a discrete GPU instead."
                }
                Self::ResetBug => {
                    "This AMD GPU generation has a known reset bug: FLR may leave \
                     the card wedged after a guest reboot. The out-of-tree \
                     vendor-reset kernel module is the usual workaround; otherwise \
                     expect to reboot the host between guest runs."
                }
            }
        }
    }

    /// The passthrough hazards that apply to the GPU with these PCI IDs. Empty
    /// for non-AMD GPUs and for AMD GPUs with no known hazards.
    ///
    /// # Example
    ///
    /// ```
    /// use enlil_devices::gpu::{AMD_VENDOR_ID, AmdPassthroughWarning, amd_passthrough_warnings};
    ///
    /// let warnings = amd_passthrough_warnings(AMD_VENDOR_ID, 0x67DF); // RX 480
    /// assert_eq!(warnings, vec![AmdPassthroughWarning::ResetBug]);
    /// ```
    #[must_use]
    pub fn amd_passthrough_warnings(vendor_id: u16, device_id: u16) -> Vec<AmdPassthroughWarning> {
        match amd_gpu_family(vendor_id, device_id) {
            AmdGpuFamily::Integrated => vec![AmdPassthroughWarning::IgpuInstability],
            AmdGpuFamily::Polaris | AmdGpuFamily::Vega | AmdGpuFamily::Navi1x => {
                vec![AmdPassthroughWarning::ResetBug]
            }
            _ => Vec::new(),
        }
    }

    /// The passthrough warnings for an enumerated GPU, as display strings.
    #[must_use]
    pub fn gpu_passthrough_warnings(gpu: &HostGpu) -> Vec<String> {
        amd_passthrough_warnings(gpu.vendor_id, gpu.device_id)
            .into_iter()
            .map(|w| w.describe().to_string())
            .collect()
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::symlink;

        /// Synthesize an expansion ROM from `(code_type, last)` image specs; each
        /// image is one 512-byte block with a valid header and PCIR structure.
        fn synth_rom(images: &[(u8, bool)]) -> Vec<u8> {
            let mut data = Vec::new();
            for (code, last) in images {
                let mut img = vec![0u8; 512];
                img[0] = 0x55;
                img[1] = 0xAA;
                let ds = 0x20;
                img[0x18] = 0x20;
                img[0x19] = 0x00;
                img[ds..ds + 4].copy_from_slice(b"PCIR");
                img[ds + 0x10] = 1; // one 512-byte block
                img[ds + 0x11] = 0;
                img[ds + 0x14] = *code;
                img[ds + 0x15] = if *last { 0x80 } else { 0x00 };
                data.extend_from_slice(&img);
            }
            data
        }

        /// Synthesize a 256-byte config space with a `PCIe` capability at 0x40.
        fn synth_pcie_config(flr_capable: bool) -> Vec<u8> {
            let mut cfg = vec![0u8; 256];
            cfg[0x06] = 0x10; // STATUS: Capabilities List
            cfg[0x34] = 0x40; // first capability at 0x40
            cfg[0x40] = PCI_CAP_ID_EXP;
            cfg[0x41] = 0x00; // end of chain
            let devcap: u32 = if flr_capable { PCIE_DEVCAP_FLR } else { 0 };
            cfg[0x44..0x48].copy_from_slice(&devcap.to_le_bytes());
            cfg
        }

        // -- option ROM parsing ----------------------------------------------

        #[test]
        fn parses_single_x86_vbios_image() {
            let rom = synth_rom(&[(0x00, true)]);
            let images = parse_option_rom(&rom).unwrap();
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].offset, 0);
            assert_eq!(images[0].len, 512);
            assert_eq!(images[0].code_type, RomCodeType::PcAt);
            assert!(images[0].last);
        }

        #[test]
        fn parses_chained_x86_then_efi_images() {
            let rom = synth_rom(&[(0x00, false), (0x03, true)]);
            let images = parse_option_rom(&rom).unwrap();
            assert_eq!(images.len(), 2);
            assert_eq!(images[0].code_type, RomCodeType::PcAt);
            assert!(!images[0].last);
            assert_eq!(images[1].offset, 512);
            assert_eq!(images[1].code_type, RomCodeType::Efi);
            assert!(images[1].last);

            let vbios = VbiosImage { data: rom, images };
            assert!(vbios.has_efi_image());
        }

        #[test]
        fn rejects_bad_rom_signature() {
            let mut rom = synth_rom(&[(0x00, true)]);
            rom[0] = 0x00;
            assert!(parse_option_rom(&rom).is_err());
        }

        #[test]
        fn rejects_truncated_rom() {
            assert!(parse_option_rom(&[0x55, 0xAA]).is_err());
        }

        #[test]
        fn rejects_zero_length_image() {
            let mut rom = synth_rom(&[(0x00, true)]);
            rom[0x20 + 0x10] = 0; // image length blocks = 0
            assert!(parse_option_rom(&rom).is_err());
        }

        #[test]
        fn unknown_code_type_is_preserved() {
            let rom = synth_rom(&[(0x42, true)]);
            let images = parse_option_rom(&rom).unwrap();
            assert_eq!(images[0].code_type, RomCodeType::Unknown(0x42));
        }

        // -- PCIe capability walk / FLR ----------------------------------------

        #[test]
        fn finds_pcie_capability_in_chain() {
            let cfg = synth_pcie_config(true);
            assert_eq!(find_pcie_capability(&cfg), Some(0x40));
            assert!(pcie_flr_capable(&cfg, 0x40));
        }

        #[test]
        fn flr_capability_bit_matters() {
            let cfg = synth_pcie_config(false);
            assert_eq!(find_pcie_capability(&cfg), Some(0x40));
            assert!(!pcie_flr_capable(&cfg, 0x40));
        }

        #[test]
        fn no_capability_list_means_no_pcie_cap() {
            let mut cfg = synth_pcie_config(true);
            cfg[0x06] = 0x00; // clear Capabilities List status bit
            assert_eq!(find_pcie_capability(&cfg), None);
        }

        #[test]
        fn initiate_flr_sets_device_control_bit() {
            let dir = tempfile::tempdir().unwrap();
            let dev = dir.path().join("0000:01:00.0");
            fs::create_dir(&dev).unwrap();
            fs::write(dev.join("config"), synth_pcie_config(true)).unwrap();

            initiate_flr(dir.path(), "0000:01:00.0").unwrap();

            let cfg = fs::read(dev.join("config")).unwrap();
            let ctl = u16::from_le_bytes([cfg[0x48], cfg[0x49]]);
            assert_eq!(ctl & PCIE_DEVCTL_INITIATE_FLR, PCIE_DEVCTL_INITIATE_FLR);
        }

        #[test]
        fn initiate_flr_refuses_non_flr_function() {
            let dir = tempfile::tempdir().unwrap();
            let dev = dir.path().join("0000:01:00.0");
            fs::create_dir(&dev).unwrap();
            fs::write(dev.join("config"), synth_pcie_config(false)).unwrap();

            let err = initiate_flr(dir.path(), "0000:01:00.0").unwrap_err();
            assert!(format!("{err:#}").contains("does not advertise FLR"));
        }

        // -- AMD generation detection ------------------------------------------

        #[test]
        fn classifies_known_amd_generations() {
            let cases = [
                (0x67DF, AmdGpuFamily::Polaris),    // RX 480
                (0x687F, AmdGpuFamily::Vega),       // RX Vega 64
                (0x66AF, AmdGpuFamily::Vega),       // Radeon VII
                (0x731F, AmdGpuFamily::Navi1x),     // RX 5700 XT
                (0x73BF, AmdGpuFamily::Navi2x),     // RX 6800 XT
                (0x744C, AmdGpuFamily::Navi3x),     // RX 7900 XTX
                (0x7590, AmdGpuFamily::Navi4x),     // RX 9070
                (0x738C, AmdGpuFamily::Cdna),       // Instinct MI100
                (0x1636, AmdGpuFamily::Integrated), // Renoir iGPU
                (0x679A, AmdGpuFamily::LegacyGcn),  // Tahiti (HD 7970)
            ];
            for (device_id, family) in cases {
                assert_eq!(
                    amd_gpu_family(AMD_VENDOR_ID, device_id),
                    family,
                    "device {device_id:#06x}"
                );
            }
        }

        #[test]
        fn non_amd_and_unknown_ids_are_unknown() {
            assert_eq!(
                amd_gpu_family(NVIDIA_VENDOR_ID, 0x1E04),
                AmdGpuFamily::Unknown
            );
            assert_eq!(
                amd_gpu_family(INTEL_VENDOR_ID, 0x9BC5),
                AmdGpuFamily::Unknown
            );
            assert_eq!(amd_gpu_family(AMD_VENDOR_ID, 0xFFFF), AmdGpuFamily::Unknown);
        }

        #[test]
        fn reset_bug_and_igpu_warnings_fire() {
            assert_eq!(
                amd_passthrough_warnings(AMD_VENDOR_ID, 0x67DF),
                vec![AmdPassthroughWarning::ResetBug]
            );
            assert_eq!(
                amd_passthrough_warnings(AMD_VENDOR_ID, 0x1636),
                vec![AmdPassthroughWarning::IgpuInstability]
            );
            assert!(amd_passthrough_warnings(AMD_VENDOR_ID, 0x73BF).is_empty());
            assert!(amd_passthrough_warnings(NVIDIA_VENDOR_ID, 0x1E04).is_empty());
        }

        // -- enumeration -------------------------------------------------------

        /// Build a fake sysfs: `pci/devices/<bdf>/{class,vendor,device,...}` plus
        /// `groups/<n>/devices/<bdf>` trees. Returns `(pci_devices, iommu_groups)`.
        fn fake_sysfs() -> (tempfile::TempDir, PathBuf, PathBuf) {
            let tmp = tempfile::tempdir().unwrap();
            let pci = tmp.path().join("pci");
            let groups = tmp.path().join("groups");
            let mkdev = |bdf: &str, class: &str, vendor: &str, device: &str, group: u32| {
                let dev = pci.join(bdf);
                fs::create_dir_all(&dev).unwrap();
                fs::write(dev.join("class"), class).unwrap();
                fs::write(dev.join("vendor"), vendor).unwrap();
                fs::write(dev.join("device"), device).unwrap();
                fs::write(dev.join("subsystem_vendor"), "0x0000").unwrap();
                fs::write(dev.join("subsystem_device"), "0x0000").unwrap();
                let gdev = groups.join(group.to_string()).join("devices");
                fs::create_dir_all(&gdev).unwrap();
                fs::write(gdev.join(bdf), "").unwrap();
                symlink(format!("../../../groups/{group}"), dev.join("iommu_group")).unwrap();
            };
            // AMD dGPU + its audio function in group 5.
            mkdev("0000:01:00.0", "0x030000", "0x1002", "0x731f", 5);
            mkdev("0000:01:00.1", "0x040300", "0x1002", "0xab38", 5);
            // Intel iGPU in group 1.
            mkdev("0000:00:02.0", "0x030000", "0x8086", "0x9bc5", 1);
            // USB controller: not a GPU, must be ignored.
            mkdev("0000:00:14.0", "0x0c0330", "0x8086", "0x7ae0", 2);
            (tmp, pci, groups)
        }

        #[test]
        fn enumerates_gpus_with_iommu_groups() {
            let (_tmp, pci, groups) = fake_sysfs();
            let gpus = enumerate_gpus(&pci, &groups).unwrap();
            assert_eq!(gpus.len(), 2);

            let dgpu = gpus.iter().find(|g| g.bdf == "0000:01:00.0").unwrap();
            assert_eq!(dgpu.vendor_id, 0x1002);
            assert_eq!(dgpu.device_id, 0x731F);
            assert_eq!(dgpu.iommu_group, Some(5));
            assert_eq!(
                dgpu.group_functions,
                vec!["0000:01:00.0".to_string(), "0000:01:00.1".to_string()]
            );
            assert!(dgpu.owns_function("0000:01:00.1"));
            assert!(!dgpu.owns_function("0000:00:02.0"));
            assert_eq!(dgpu.label(), "1002:731f");

            let igpu = gpus.iter().find(|g| g.bdf == "0000:00:02.0").unwrap();
            assert_eq!(igpu.iommu_group, Some(1));
            assert_eq!(igpu.group_functions, vec!["0000:00:02.0".to_string()]);
        }

        #[test]
        fn enumerate_gpus_is_sorted_and_skips_garbage() {
            let (_tmp, pci, groups) = fake_sysfs();
            // A non-BDF directory entry must be skipped, not fatal.
            fs::create_dir(pci.join("not-a-device")).unwrap();
            let gpus = enumerate_gpus(&pci, &groups).unwrap();
            let bdfs: Vec<&str> = gpus.iter().map(|g| g.bdf.as_str()).collect();
            assert_eq!(bdfs, vec!["0000:00:02.0", "0000:01:00.0"]);
        }

        #[test]
        fn assign_fails_cleanly_without_vfio() {
            let (_tmp, pci, groups) = fake_sysfs();
            let gpus = enumerate_gpus(&pci, &groups).unwrap();
            let gpu = gpus.iter().find(|g| g.bdf == "0000:01:00.0").unwrap();
            // No vfio-pci driver in the fake drivers dir and no /dev/vfio/vfio:
            // assignment must fail with an error, never panic.
            let drivers = groups.join("drivers");
            fs::create_dir_all(&drivers).unwrap();
            let Err(err) = GpuPassthrough::assign(&pci, &drivers, gpu) else {
                panic!("assign should fail without VFIO");
            };
            assert!(!format!("{err:#}").is_empty());
        }

        #[test]
        fn assign_refuses_group_less_gpu() {
            let gpu = HostGpu {
                bdf: "0000:01:00.0".to_string(),
                vendor_id: AMD_VENDOR_ID,
                device_id: 0x731F,
                subsystem_vendor_id: 0,
                subsystem_id: 0,
                iommu_group: None,
                group_functions: Vec::new(),
            };
            let Err(err) =
                GpuPassthrough::assign(Path::new("/nonexistent"), Path::new("/nonexistent"), &gpu)
            else {
                panic!("assign should refuse a group-less GPU");
            };
            assert!(format!("{err:#}").contains("no IOMMU group"));
        }

        #[test]
        fn gpu_warnings_surface_as_strings() {
            let gpu = HostGpu {
                bdf: "0000:01:00.0".to_string(),
                vendor_id: AMD_VENDOR_ID,
                device_id: 0x67DF,
                subsystem_vendor_id: 0,
                subsystem_id: 0,
                iommu_group: Some(5),
                group_functions: vec!["0000:01:00.0".to_string()],
            };
            let warnings = gpu_passthrough_warnings(&gpu);
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("reset bug"));
        }
    }
}

#[cfg(target_os = "linux")]
pub use tier1_passthrough::*;
