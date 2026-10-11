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
