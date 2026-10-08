//! VFIO whole-controller passthrough fallback for USB (Phase 4.4).
//!
//! The virtual-xHCI + per-device forwarding stack cannot handle every
//! device: isochronous endpoints have no sync-libusb path, a forwarder open
//! can fail (permission, device busy, vanished hardware), and some devices
//! simply misbehave behind forwarded TDs. The escape hatch is whole-
//! controller VFIO passthrough: hand the guest the physical xHCI controller
//! itself via the IOMMU, and let its native driver own every port.
//!
//! This module is the honest version of that hatch. xHCI controllers are
//! notorious for unstable Function-Level Resets under VFIO — a controller
//! whose FLR hangs takes the whole USB bus (and every guest device on it)
//! down with it. So the flow is:
//!
//! 1. [`VfioFallback::assess`] gates the controller (xHCI class, IOMMU group
//!    present, VFIO available, group viable) and *assesses* FLR stability:
//!    whether the `PCIe` Device Capabilities register advertises FLR, and
//!    whether the VID:PID is on the known-unstable quirk list.
//! 2. Every concern becomes a [`VfioWarning`] surfaced to the operator.
//! 3. [`VfioFallback::attempt`] refuses an unstable-FLR controller unless the
//!    operator explicitly opted in (`allow_unstable_flr`), then binds the
//!    controller to `vfio-pci`, opens the VFIO container/group/device, and
//!    issues the reset — logging the warnings loudly first.
//!
//! [`VfioFallbackPolicy`] is the config-driven half: when per-device routing
//! fails ([`HotplugOutcome::AttachFailed`](super::hotplug::HotplugOutcome)),
//! the hot-plug dispatcher recommends the configured fallback instead of
//! silently stranding the device.

//! VFIO whole-controller passthrough fallback for USB (Phase 4.4).
pub const XHCI_CLASS_CODE: u32 = 0x0C_0330;

/// Offset of the PCI Status register; bit 4 marks a capabilities list.
const PCI_STATUS: usize = 0x06;
/// Status bit: capabilities list present.
const PCI_STATUS_CAP_LIST: u16 = 1 << 4;
/// Offset of the first capability pointer.
const PCI_CAPABILITY_LIST: usize = 0x34;
/// PCI capability ID for PCI Express.
const PCI_CAP_ID_EXP: u8 = 0x10;
/// Offset of the 32-bit Device Capabilities register inside the PCI Express
/// capability structure.
const PCI_EXP_DEVCAP_OFFSET: usize = 0x02;
/// Bit 28 of Device Capabilities: the function supports Function-Level Reset.
const PCI_EXP_DEVCAP_FLR: u32 = 1 << 28;

/// How Function-Level Reset looks on this controller — the stability
/// assessment the warnings (and the opt-in gate) are built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlrAssessment {
    /// The `PCIe` Device Capabilities register advertises FLR.
    Advertised,
    /// The controller's config space carries no FLR advertisement (no `PCIe`
    /// capability, or the bit is clear).
    NotAdvertised,
    /// The controller advertises FLR but its VID:PID is on the known-
    /// unstable list — community-reported cases of FLR hanging or the
    /// function failing to re-enumerate.
    KnownUnstable {
        /// PCI vendor ID.
        vendor_id: u16,
        /// PCI device ID.
        device_id: u16,
    },
}

impl FlrAssessment {
    /// Whether passthrough may proceed without an explicit unstable-FLR
    /// opt-in.
    #[must_use]
    pub const fn is_stable(self) -> bool {
        matches!(self, Self::Advertised)
    }
}

/// A controller whose FLR is community-reported as unreliable under VFIO,
/// with the honest note that goes to the operator.
struct FlrQuirk {
    vendor_id: u16,
    device_id: u16,
    note: &'static str,
}

/// xHCI controllers with community-reported unstable FLR. These are *not*
/// blocklisted — the operator can still opt in — but the warning names the
/// device so the risk is explicit.
const FLR_QUIRKS: &[FlrQuirk] = &[
    FlrQuirk {
        vendor_id: 0x1912,
        device_id: 0x0014,
        note: "Renesas uPD720201: FLR reported unreliable under VFIO; the \
               function may not re-enumerate after reset",
    },
    FlrQuirk {
        vendor_id: 0x1912,
        device_id: 0x0015,
        note: "Renesas uPD720202: FLR reported unreliable under VFIO; the \
               function may not re-enumerate after reset",
    },
    FlrQuirk {
        vendor_id: 0x1B21,
        device_id: 0x1142,
        note: "ASMedia ASM1042: FLR reported unreliable; some boards need a \
               secondary bus reset before the controller comes back",
    },
];

/// The quirk note for a VID:PID, if it is on the known-unstable list.
#[must_use]
pub fn flr_quirk_note(vendor_id: u16, device_id: u16) -> Option<&'static str> {
    FLR_QUIRKS
        .iter()
        .find(|q| q.vendor_id == vendor_id && q.device_id == device_id)
        .map(|q| q.note)
}

/// Assess FLR support from a PCI config-space image (the sysfs `config`
/// attribute): walk the capability list for the PCI Express capability and
/// read bit 28 of Device Capabilities.
///
/// Returns `None` when the image is too short or carries no `PCIe` capability
/// (FLR support then cannot be determined); otherwise `Some(true)` when the
/// FLR bit is set.
#[must_use]
pub fn flr_advertised(config: &[u8]) -> Option<bool> {
    let status = u16::from_le_bytes(config.get(PCI_STATUS..PCI_STATUS + 2)?.try_into().ok()?);
    if status & PCI_STATUS_CAP_LIST == 0 {
        return Some(false);
    }
    let mut ptr = usize::from(*config.get(PCI_CAPABILITY_LIST)?);
    // Standard capability structures live at >= 0x40; bail on garbage.
    let mut steps = 0;
    while ptr >= 0x40 && steps < 48 {
        steps += 1;
        let id = *config.get(ptr)?;
        let next = *config.get(ptr + 1)?;
        if id == PCI_CAP_ID_EXP {
            let devcap = u32::from_le_bytes(
                config
                    .get(ptr + PCI_EXP_DEVCAP_OFFSET..ptr + PCI_EXP_DEVCAP_OFFSET + 4)?
                    .try_into()
                    .ok()?,
            );
            return Some(devcap & PCI_EXP_DEVCAP_FLR != 0);
        }
        if next == 0 {
            return Some(false);
        }
        ptr = usize::from(next);
    }
    None
}

/// A warning the operator sees before (and during) whole-controller
/// passthrough — never silent, never downgraded to debug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfioWarning {
    /// The controller does not advertise FLR in its `PCIe` Device
    /// Capabilities. Reset behavior is then undefined: it may do nothing,
    /// or hang the function.
    FlrNotAdvertised,
    /// The controller is on the known-unstable FLR list.
    FlrKnownUnstable {
        /// PCI vendor ID.
        vendor_id: u16,
        /// PCI device ID.
        device_id: u16,
        /// The quirk note.
        note: &'static str,
    },
    /// The controller is still bound to a host driver; passthrough unbinds
    /// it (the host loses the controller and every device on it).
    ControllerBoundToDriver {
        /// Driver the controller is currently bound to.
        driver: String,
    },
    /// Other PCI functions share the controller's IOMMU group — they move
    /// into the guest's IOMMU domain with it.
    SiblingFunctionsInGroup {
        /// BDFs of the other functions in the group.
        siblings: Vec<String>,
    },
}

impl std::fmt::Display for VfioWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FlrNotAdvertised => write!(
                f,
                "controller does not advertise Function-Level Reset; \
                 reset behavior is undefined and may hang the function"
            ),
            Self::FlrKnownUnstable {
                vendor_id,
                device_id,
                note,
            } => write!(
                f,
                "FLR on {vendor_id:04x}:{device_id:04x} is reported unstable: {note}"
            ),
            Self::ControllerBoundToDriver { driver } => write!(
                f,
                "controller is bound to host driver '{driver}'; passthrough \
                 unbinds it and the host loses every device on this controller"
            ),
            Self::SiblingFunctionsInGroup { siblings } => write!(
                f,
                "IOMMU group contains sibling functions [{}] that move into \
                 the guest's IOMMU domain with the controller",
                siblings.join(", ")
            ),
        }
    }
}

/// Why per-device forwarding gave up on a device — the trigger for the
/// fallback recommendation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackReason {
    /// Routing claimed the device but the registry could not attach it.
    AttachFailed {
        /// The registry's error, as displayed.
        error: String,
    },
}

impl std::fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AttachFailed { error } => {
                write!(f, "per-device attach failed: {error}")
            }
        }
    }
}

/// The config-driven fallback plan: which host controller to pass through,
/// to which guest, and whether the operator accepted unstable-FLR risk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfioFallbackPolicy {
    /// Host xHCI controller BDF (`"dddd:bb:dd.f"`).
    pub bdf: String,
    /// Guest that receives the whole controller.
    pub target_guest: String,
    /// The operator explicitly accepted unstable-FLR risk
    /// (`allow_unstable_flr`).
    pub allow_unstable_flr: bool,
}

impl VfioFallbackPolicy {
    /// The operator-facing recommendation emitted when `reason` fires: what
    /// failed, what the fallback does, and the FLR-stability caveat.
    #[must_use]
    pub fn recommend(&self, reason: &FallbackReason) -> String {
        let flr_note = if self.allow_unstable_flr {
            "the operator accepted unstable-FLR risk (allow_unstable_flr)"
        } else {
            "passthrough will REFUSE controllers without stable FLR \
             (set allow_unstable_flr to override)"
        };
        format!(
            "USB fallback: {reason}; whole-controller VFIO passthrough of host \
             xHCI {} is configured for guest '{}' — {}",
            self.bdf, self.target_guest, flr_note
        )
    }
}

// ---------------------------------------------------------------------------
// Linux: assessment gates and the passthrough itself
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod imp {
    use std::path::{Path, PathBuf};

    use super::{FlrAssessment, VfioWarning, XHCI_CLASS_CODE, flr_advertised, flr_quirk_note};
    use crate::net::{
        VfioContainer, VfioGroup, VfioPciDevice, bind_driver, bound_driver, iommu_group_of,
        pci_attr_hex, pci_config, validate_bdf,
    };

    /// Failures assessing or attempting whole-controller VFIO passthrough.
    #[derive(Debug, thiserror::Error)]
    pub enum VfioFallbackError {
        /// The BDF is malformed.
        #[error("malformed controller BDF {0:?}")]
        BdfInvalid(String),
        /// The PCI function is not an xHCI controller.
        #[error("PCI device {bdf} is not an xHCI controller (class {class:#08x})")]
        NotXhci {
            /// The BDF that was checked.
            bdf: String,
            /// The class code that was read.
            class: u32,
        },
        /// The controller is in no IOMMU group — passthrough is impossible.
        #[error("PCI device {0} is in no IOMMU group (IOMMU disabled or not isolating it)")]
        NoIommuGroup(String),
        /// VFIO itself is unavailable on this host.
        #[error("VFIO unavailable: {0}")]
        VfioUnavailable(String),
        /// The IOMMU group is not viable (a sibling is bound to a
        /// non-VFIO driver).
        #[error("IOMMU group {0} is not viable for VFIO")]
        GroupNotViable(u32),
        /// The controller's FLR is unstable and the operator did not opt in.
        #[error(
            "refusing passthrough: FLR is {assessment:?}; set allow_unstable_flr to proceed anyway"
        )]
        UnstableFlr {
            /// The assessment that blocked the attempt.
            assessment: FlrAssessment,
        },
        /// Binding the controller to vfio-pci failed.
        #[error("could not bind {bdf} to vfio-pci: {reason}")]
        DriverBind {
            /// The controller BDF.
            bdf: String,
            /// Why the bind failed.
            reason: String,
        },
        /// The VFIO device fd or reset failed.
        #[error("VFIO device setup failed for {bdf}: {reason}")]
        DeviceSetup {
            /// The controller BDF.
            bdf: String,
            /// Why setup failed.
            reason: String,
        },
        /// A sysfs read the assessment needs failed.
        #[error("sysfs read failed: {0}")]
        Sysfs(String),
    }

    /// An assessed host xHCI controller, ready for a gated passthrough
    /// attempt: the gates passed, the warnings are collected, and
    /// [`attempt`](Self::attempt) performs the bind + reset.
    pub struct VfioFallback {
        /// Controller BDF.
        pub bdf: String,
        /// FLR stability assessment.
        pub assessment: FlrAssessment,
        /// Warnings the operator must see before proceeding.
        pub warnings: Vec<VfioWarning>,
        /// IOMMU group number.
        pub group_id: u32,
    }

    impl VfioFallback {
        /// Assess the host xHCI controller at `bdf` under `pci_devices`
        /// (`/sys/bus/pci/devices`): verify it is an xHCI controller in an
        /// IOMMU group with VFIO available, assess FLR stability, and
        /// collect every warning.
        ///
        /// # Errors
        ///
        /// [`VfioFallbackError`] when a gate fails: malformed BDF, not an
        /// xHCI controller, no IOMMU group, or VFIO unavailable.
        pub fn assess(pci_devices: &Path, bdf: &str) -> Result<Self, VfioFallbackError> {
            validate_bdf(bdf).map_err(|_| VfioFallbackError::BdfInvalid(bdf.to_string()))?;
            let class = pci_attr_hex(pci_devices, bdf, "class")
                .map_err(|e| VfioFallbackError::Sysfs(e.to_string()))?;
            if class != XHCI_CLASS_CODE {
                return Err(VfioFallbackError::NotXhci {
                    bdf: bdf.to_string(),
                    class,
                });
            }
            let vendor = u16::try_from(
                pci_attr_hex(pci_devices, bdf, "vendor")
                    .map_err(|e| VfioFallbackError::Sysfs(e.to_string()))?,
            )
            .unwrap_or(0);
            let device = u16::try_from(
                pci_attr_hex(pci_devices, bdf, "device")
                    .map_err(|e| VfioFallbackError::Sysfs(e.to_string()))?,
            )
            .unwrap_or(0);
            let config = pci_config(pci_devices, bdf)
                .map_err(|e| VfioFallbackError::Sysfs(e.to_string()))?;

            let mut warnings = Vec::new();
            let assessment = match flr_quirk_note(vendor, device) {
                Some(note) => {
                    warnings.push(VfioWarning::FlrKnownUnstable {
                        vendor_id: vendor,
                        device_id: device,
                        note,
                    });
                    FlrAssessment::KnownUnstable {
                        vendor_id: vendor,
                        device_id: device,
                    }
                }
                None => {
                    if flr_advertised(&config) == Some(true) {
                        FlrAssessment::Advertised
                    } else {
                        warnings.push(VfioWarning::FlrNotAdvertised);
                        FlrAssessment::NotAdvertised
                    }
                }
            };

            let group_id = iommu_group_of(pci_devices, bdf)
                .map_err(|e| VfioFallbackError::Sysfs(e.to_string()))?
                .ok_or_else(|| VfioFallbackError::NoIommuGroup(bdf.to_string()))?;

            if let Some(driver) = bound_driver(pci_devices, bdf)
                .map_err(|e| VfioFallbackError::Sysfs(e.to_string()))?
                && driver != "vfio-pci"
            {
                warnings.push(VfioWarning::ControllerBoundToDriver { driver });
            }

            let siblings = sibling_functions(pci_devices, bdf, group_id);
            if !siblings.is_empty() {
                warnings.push(VfioWarning::SiblingFunctionsInGroup { siblings });
            }

            // The VFIO container must open — the last gate before any
            // destructive step (bind/unbind) happens in `attempt`.
            VfioContainer::open().map_err(|e| VfioFallbackError::VfioUnavailable(e.to_string()))?;

            Ok(Self {
                bdf: bdf.to_string(),
                assessment,
                warnings,
                group_id,
            })
        }

        /// Attempt the passthrough: log every warning loudly, refuse an
        /// unstable-FLR controller unless `allow_unstable_flr`, bind the
        /// controller to `vfio-pci`, open the VFIO container/group/device,
        /// and issue the Function-Level Reset.
        ///
        /// `pci_devices` is `/sys/bus/pci/devices`, `drivers_dir` is
        /// `/sys/bus/pci/drivers` (both injectable for tests).
        ///
        /// # Errors
        ///
        /// [`VfioFallbackError::UnstableFlr`] when the assessment is not
        /// stable and the operator did not opt in; bind/device/reset
        /// failures surface with the controller named.
        pub fn attempt(
            &self,
            allow_unstable_flr: bool,
            pci_devices: &Path,
            drivers_dir: &Path,
        ) -> Result<VfioPassthrough, VfioFallbackError> {
            for warning in &self.warnings {
                log::warn!("USB VFIO fallback ({}): {warning}", self.bdf);
            }
            if !self.assessment.is_stable() && !allow_unstable_flr {
                return Err(VfioFallbackError::UnstableFlr {
                    assessment: self.assessment,
                });
            }
            if !self.assessment.is_stable() {
                log::warn!(
                    "USB VFIO fallback ({}): proceeding despite unstable FLR \
                     ({:?}) at the operator's explicit request",
                    self.bdf,
                    self.assessment
                );
            }

            bind_driver(pci_devices, drivers_dir, &self.bdf, "vfio-pci").map_err(|e| {
                VfioFallbackError::DriverBind {
                    bdf: self.bdf.clone(),
                    reason: e.to_string(),
                }
            })?;

            let container = VfioContainer::open()
                .map_err(|e| VfioFallbackError::VfioUnavailable(e.to_string()))?;
            let group = VfioGroup::open(self.group_id)
                .map_err(|e| VfioFallbackError::VfioUnavailable(e.to_string()))?;
            group
                .set_container(&container)
                .map_err(|_| VfioFallbackError::GroupNotViable(self.group_id))?;
            let device = group
                .device(&self.bdf)
                .map_err(|e| VfioFallbackError::DeviceSetup {
                    bdf: self.bdf.clone(),
                    reason: e.to_string(),
                })?;

            log::warn!(
                "USB VFIO fallback ({}): issuing Function-Level Reset — the \
                 controller and every device on it will drop off the host bus",
                self.bdf
            );
            device.reset().map_err(|e| VfioFallbackError::DeviceSetup {
                bdf: self.bdf.clone(),
                reason: format!("FLR reset failed: {e}"),
            })?;

            Ok(VfioPassthrough {
                container,
                group,
                device,
                warnings: self.warnings.clone(),
                bdf: self.bdf.clone(),
            })
        }

        /// The warnings `attempt` will log before touching the controller.
        #[must_use]
        pub const fn warnings(&self) -> &Vec<VfioWarning> {
            &self.warnings
        }
    }

    /// A live whole-controller passthrough.
    ///
    /// The VFIO container, group, and device fd stay open (dropping them
    /// releases the controller). The guest platform layer maps the device's
    /// BARs and wires its interrupts from here.
    pub struct VfioPassthrough {
        /// The VFIO container (Type-1 IOMMU context).
        pub container: VfioContainer,
        /// The IOMMU group the controller was opened from.
        pub group: VfioGroup,
        /// The bound, reset xHCI controller.
        pub device: VfioPciDevice,
        /// Warnings that were logged before the reset.
        pub warnings: Vec<VfioWarning>,
        /// Controller BDF.
        pub bdf: String,
    }

    /// Other PCI functions in the controller's IOMMU group (resolved through
    /// the `iommu_group` symlink so `pci_devices` stays injectable).
    /// `group_id` is the assessed group, kept in the signature for call-site
    /// clarity; the directory walk resolves the group through the symlink.
    pub fn sibling_functions(pci_devices: &Path, bdf: &str, group_id: u32) -> Vec<String> {
        let _ = group_id;
        let link = pci_devices.join(bdf).join("iommu_group");
        let Ok(target) = std::fs::read_link(&link) else {
            return Vec::new();
        };
        let group_dir: PathBuf = link.parent().unwrap_or(pci_devices).join(target);
        let devices_dir = group_dir.join("devices");
        let Ok(entries) = std::fs::read_dir(&devices_dir) else {
            return Vec::new();
        };
        let mut siblings: Vec<String> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name != bdf)
            .collect();
        siblings.sort();
        siblings
    }
}

#[cfg(target_os = "linux")]
pub use imp::{VfioFallback, VfioFallbackError, VfioPassthrough};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 256-byte config space with a capability list containing one
    /// PCI Express capability whose Device Capabilities has `flr` set/clear.
    fn config_with_pcie(flr: bool) -> Vec<u8> {
        let mut config = vec![0u8; 256];
        // Status: capabilities list present.
        config[PCI_STATUS] = 0x10;
        config[PCI_STATUS + 1] = 0;
        // Capability pointer -> 0x40.
        config[PCI_CAPABILITY_LIST] = 0x40;
        // PCIe cap: id 0x10, no next.
        config[0x40] = PCI_CAP_ID_EXP;
        config[0x41] = 0x00;
        let mut devcap = 0u32;
        if flr {
            devcap |= PCI_EXP_DEVCAP_FLR;
        }
        config[PCI_EXP_DEVCAP_OFFSET + 0x40..PCI_EXP_DEVCAP_OFFSET + 0x44]
            .copy_from_slice(&devcap.to_le_bytes());
        config
    }

    #[test]
    fn flr_bit_detected_in_pcie_device_capabilities() {
        assert_eq!(flr_advertised(&config_with_pcie(true)), Some(true));
        assert_eq!(flr_advertised(&config_with_pcie(false)), Some(false));
    }

    #[test]
    fn missing_capability_list_is_not_an_flr_advertisement() {
        let mut config = vec![0u8; 256];
        // Status bit 4 clear: no capability list at all.
        config[PCI_STATUS] = 0;
        assert_eq!(flr_advertised(&config), Some(false));
    }

    #[test]
    fn capability_walk_skips_unrelated_caps_and_stops_cleanly() {
        let mut config = vec![0u8; 256];
        config[PCI_STATUS] = 0x10;
        config[PCI_CAPABILITY_LIST] = 0x40;
        // MSI cap (id 0x05) at 0x40 -> PCIe cap at 0x60 -> end.
        config[0x40] = 0x05;
        config[0x41] = 0x60;
        config[0x60] = PCI_CAP_ID_EXP;
        config[0x61] = 0x00;
        let devcap = PCI_EXP_DEVCAP_FLR;
        config[PCI_EXP_DEVCAP_OFFSET + 0x60..PCI_EXP_DEVCAP_OFFSET + 0x64]
            .copy_from_slice(&devcap.to_le_bytes());
        assert_eq!(flr_advertised(&config), Some(true));
    }

    #[test]
    fn truncated_config_space_is_indeterminate_not_advertised() {
        // No capability list at all: definitively no FLR.
        assert_eq!(flr_advertised(&[0u8; 64]), Some(false));
        // A capability list pointing past the end of a truncated image:
        // indeterminate, not "no FLR".
        let mut truncated = vec![0u8; 64];
        truncated[PCI_STATUS] = 0x10;
        truncated[PCI_CAPABILITY_LIST] = 0x40;
        assert_eq!(flr_advertised(&truncated), None);
        assert_eq!(flr_advertised(&[]), None);
    }

    #[test]
    fn quirk_table_names_the_reported_unstable_controllers() {
        let note = flr_quirk_note(0x1912, 0x0015).expect("uPD720202 quirk");
        assert!(note.contains("uPD720202"), "note names the part: {note}");
        assert!(flr_quirk_note(0x1912, 0x0014).is_some());
        assert!(flr_quirk_note(0x1B21, 0x1142).is_some());
        assert_eq!(flr_quirk_note(0x8086, 0xA36D), None);
    }

    #[test]
    fn stable_assessment_is_the_only_one_not_needing_opt_in() {
        assert!(FlrAssessment::Advertised.is_stable());
        assert!(!FlrAssessment::NotAdvertised.is_stable());
        assert!(
            !FlrAssessment::KnownUnstable {
                vendor_id: 0x1912,
                device_id: 0x0015
            }
            .is_stable()
        );
    }

    #[test]
    fn warnings_render_as_operator_readable_sentences() {
        let w = VfioWarning::FlrNotAdvertised;
        assert!(w.to_string().contains("Function-Level Reset"));
        let w = VfioWarning::ControllerBoundToDriver {
            driver: "xhci_hcd".into(),
        };
        assert!(w.to_string().contains("xhci_hcd"));
        let w = VfioWarning::SiblingFunctionsInGroup {
            siblings: vec!["0000:03:00.1".into()],
        };
        assert!(w.to_string().contains("0000:03:00.1"));
    }

    #[test]
    fn policy_recommendation_names_device_guest_and_flr_stance() {
        let policy = VfioFallbackPolicy {
            bdf: "0000:03:00.0".into(),
            target_guest: "linux1".into(),
            allow_unstable_flr: false,
        };
        let reason = FallbackReason::AttachFailed {
            error: "no free port".into(),
        };
        let text = policy.recommend(&reason);
        assert!(text.contains("0000:03:00.0"), "{text}");
        assert!(text.contains("linux1"), "{text}");
        assert!(text.contains("no free port"), "{text}");
        assert!(text.contains("REFUSE"), "{text}");

        let opted_in = VfioFallbackPolicy {
            allow_unstable_flr: true,
            ..policy
        };
        assert!(
            opted_in
                .recommend(&reason)
                .contains("accepted unstable-FLR")
        );
    }

    #[cfg(target_os = "linux")]
    mod linux_tests {
        use super::super::imp::{VfioFallback, VfioFallbackError};
        use super::super::{VfioWarning, XHCI_CLASS_CODE};
        use super::*;
        use std::path::Path;

        /// Fake `/sys/bus/pci/devices/<bdf>` layout with class/vendor/device,
        /// a config-space file, an `iommu_group` symlink, and a driver link.
        struct FakePci {
            dir: tempfile::TempDir,
            devices: std::path::PathBuf,
        }

        impl FakePci {
            fn xhci(bdf: &str, vendor: u16, device: u16, flr: bool, driver: &str) -> Self {
                let dir = tempfile::tempdir().expect("tempdir");
                let devices = dir.path().join("devices");
                let dev = devices.join(bdf);
                std::fs::create_dir_all(&dev).unwrap();
                std::fs::write(dev.join("class"), format!("0x{XHCI_CLASS_CODE:06x}\n")).unwrap();
                std::fs::write(dev.join("vendor"), format!("0x{vendor:04x}\n")).unwrap();
                std::fs::write(dev.join("device"), format!("0x{device:04x}\n")).unwrap();
                std::fs::write(dev.join("config"), config_with_pcie(flr)).unwrap();
                // iommu_group symlink -> a sibling dir with a devices/ listing.
                let group = dir.path().join("iommu_groups").join("7");
                std::fs::create_dir_all(group.join("devices")).unwrap();
                std::os::unix::fs::symlink(&group, dev.join("iommu_group")).unwrap();
                std::os::unix::fs::symlink(
                    dir.path().join("drivers").join(driver),
                    dev.join("driver"),
                )
                .unwrap();
                std::fs::create_dir_all(dir.path().join("drivers").join(driver)).unwrap();
                Self { dir, devices }
            }

            fn path(&self) -> &Path {
                &self.devices
            }
        }

        #[test]
        fn assess_collects_warnings_for_bound_unstable_controller() {
            // uPD720202 bound to xhci_hcd with FLR advertised: the quirk
            // wins over the advertisement, and the bound driver warns.
            let fake = FakePci::xhci("0000:03:00.0", 0x1912, 0x0015, true, "xhci_hcd");
            // No /dev/vfio/vfio on this runner: the last gate fails, which
            // is the honest outcome here — but the failure must be the VFIO
            // gate, not an earlier one, proving the checks ran in order.
            match VfioFallback::assess(fake.path(), "0000:03:00.0") {
                Err(VfioFallbackError::VfioUnavailable(_)) => {}
                Err(other) => panic!("expected the VFIO gate, got: {other}"),
                Ok(_) => panic!("assess succeeded without /dev/vfio/vfio"),
            }
        }

        #[test]
        fn assess_rejects_non_xhci_and_malformed_bdf() {
            let fake = FakePci::xhci("0000:03:00.0", 0x8086, 0xA36D, true, "xhci_hcd");
            // Overwrite the class with a network controller's.
            std::fs::write(fake.path().join("0000:03:00.0").join("class"), "0x020000\n").unwrap();
            assert!(matches!(
                VfioFallback::assess(fake.path(), "0000:03:00.0"),
                Err(VfioFallbackError::NotXhci { .. })
            ));
            assert!(matches!(
                VfioFallback::assess(fake.path(), "not-a-bdf"),
                Err(VfioFallbackError::BdfInvalid(_))
            ));
        }

        #[test]
        fn assess_requires_an_iommu_group() {
            let fake = FakePci::xhci("0000:03:00.0", 0x8086, 0xA36D, true, "xhci_hcd");
            std::fs::remove_file(fake.path().join("0000:03:00.0").join("iommu_group")).unwrap();
            assert!(matches!(
                VfioFallback::assess(fake.path(), "0000:03:00.0"),
                Err(VfioFallbackError::NoIommuGroup(_))
            ));
        }

        #[test]
        fn sibling_functions_in_the_group_become_a_warning() {
            let fake = FakePci::xhci("0000:03:00.0", 0x8086, 0xA36D, true, "vfio-pci");
            // The group's devices dir gains a sibling function.
            let group_devices = fake
                .dir
                .path()
                .join("iommu_groups")
                .join("7")
                .join("devices");
            std::fs::create_dir_all(group_devices.join("0000:03:00.1")).unwrap();
            let siblings = super::super::imp::sibling_functions(fake.path(), "0000:03:00.0", 7);
            assert_eq!(siblings, vec!["0000:03:00.1".to_string()]);
            // And the warning renders with the sibling named.
            let w = VfioWarning::SiblingFunctionsInGroup { siblings };
            assert!(w.to_string().contains("0000:03:00.1"));
        }
    }
}
