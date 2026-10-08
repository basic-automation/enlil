//! SR-IOV NIC passthrough: Virtual Function provisioning and guest assignment.
//!
//! When the host has an SR-IOV-capable NIC, an IOMMU, and VFIO, a guest can be
//! handed a Virtual Function directly instead of going through the emulated
//! virtio-net path — the high-performance networking tier for Phase 3.2.
//! Everything here is capability-gated: [`SriovGating::probe`] reports which
//! prerequisites are present, discovery returns an empty list when there is no
//! SR-IOV NIC, and every host-mutating step fails cleanly (permission or
//! missing hardware) instead of panicking.
//!
//! # Typical flow
//!
//! ```text
//! let paths = HostPaths::live();
//! let gating = SriovGating::probe(&paths);
//! if !gating.ready() { /* fall back to virtio-net; see gating.missing() */ }
//!
//! let pfs = discover_sriov_nic_pfs(&paths)?;
//! let vfs = enable_vfs(&paths, &pfs[0].bdf, 2)?;          // provision 2 VFs
//! let container = Arc::new(VfioContainer::open()?);       // one per host
//! let mut table = VfAssignmentTable::new();
//! assign_vf_to_guest(&paths, &mut table, &container, "guest-0", &vfs[0])?;
//! // ... later ...
//! table.unassign(&vfs[0].bdf);                            // hand the VF back
//! disable_vfs(&paths, &pfs[0].bdf)?;
//! ```
//!
//! The guest-facing side is [`SriovVfAssignment`]: config-space and BAR-region
//! accessors the platform layer uses to expose the VF in the guest's PCI
//! topology, plus DMA mapping through the shared VFIO container.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::vfio_pci::{
    DmaMapping, PciRegion, VfioContainer, VfioGroup, VfioPciDevice, bind_driver, bound_driver,
    device_mac, iommu_group_of, pci_attr_dec, pci_attr_hex, pci_config, validate_bdf,
};
use crate::pci_discovery::{ExtendedCapability, extended_capabilities_in_config};

/// PCI base class for network controllers.
const PCI_BASE_CLASS_NETWORK: u32 = 0x02;
/// VFIO driver name VFs are bound to for passthrough.
const VFIO_PCI_DRIVER: &str = "vfio-pci";

// ---------------------------------------------------------------------------
// Host paths (injectable for tests)
// ---------------------------------------------------------------------------

/// The host filesystem locations the SR-IOV layer reads/writes.
#[derive(Debug, Clone)]
pub struct HostPaths {
    /// `/sys/bus/pci/devices`.
    pub pci_devices: PathBuf,
    /// `/sys/kernel/iommu_groups`.
    pub iommu_groups: PathBuf,
    /// `/sys/bus/pci/drivers`.
    pub drivers: PathBuf,
    /// `/dev/vfio/vfio`.
    pub vfio_dev: PathBuf,
}

impl HostPaths {
    /// The real host paths.
    #[must_use]
    pub fn live() -> Self {
        Self {
            pci_devices: PathBuf::from("/sys/bus/pci/devices"),
            iommu_groups: PathBuf::from("/sys/kernel/iommu_groups"),
            drivers: PathBuf::from("/sys/bus/pci/drivers"),
            vfio_dev: PathBuf::from("/dev/vfio/vfio"),
        }
    }
}

// ---------------------------------------------------------------------------
// Capability gating
// ---------------------------------------------------------------------------

/// Which SR-IOV passthrough prerequisites the host satisfies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SriovGating {
    /// `/sys/bus/pci/devices` exists (PCI sysfs is exposed).
    pub sysfs_pci: bool,
    /// At least one IOMMU group exists (the IOMMU is enabled).
    pub iommu: bool,
    /// `/dev/vfio/vfio` exists (VFIO is available).
    pub vfio: bool,
}

impl SriovGating {
    /// Probe the host for the passthrough prerequisites.
    #[must_use]
    pub fn probe(paths: &HostPaths) -> Self {
        let iommu = std::fs::read_dir(&paths.iommu_groups).is_ok_and(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.path().is_dir())
        });
        Self {
            sysfs_pci: paths.pci_devices.is_dir(),
            iommu,
            vfio: paths.vfio_dev.exists(),
        }
    }

    /// Whether all prerequisites hold and passthrough may be attempted.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.sysfs_pci && self.iommu && self.vfio
    }

    /// Human-readable names of the missing prerequisites (empty when ready).
    #[must_use]
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.sysfs_pci {
            out.push("PCI sysfs (/sys/bus/pci/devices)");
        }
        if !self.iommu {
            out.push("IOMMU (/sys/kernel/iommu_groups is empty — enable intel_iommu/amd_iommu)");
        }
        if !self.vfio {
            out.push("VFIO (/dev/vfio/vfio)");
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Physical Function discovery
// ---------------------------------------------------------------------------

/// An SR-IOV-capable NIC Physical Function found on the host.
#[derive(Debug, Clone)]
pub struct SriovNicPf {
    /// Canonical BDF (`"dddd:bb:dd.f"`).
    pub bdf: String,
    /// PCI vendor ID.
    pub vendor_id: u16,
    /// PCI device ID.
    pub device_id: u16,
    /// `sriov_totalvfs`: maximum VFs the PF can provide.
    pub total_vfs: u16,
    /// `sriov_numvfs`: VFs currently enabled.
    pub num_vfs: u16,
    /// Driver currently bound to the PF, if any.
    pub driver: Option<String>,
    /// IOMMU group of the PF, if the IOMMU exposes one.
    pub iommu_group: Option<u32>,
    /// MAC of the PF's netdev, if the kernel created one.
    pub mac: Option<[u8; 6]>,
}

/// A Virtual Function provisioned from an SR-IOV PF.
#[derive(Debug, Clone)]
pub struct SriovVf {
    /// Canonical BDF of the VF.
    pub bdf: String,
    /// BDF of the PF this VF was provisioned from.
    pub pf_bdf: String,
    /// VF index (`virtfn<index>`).
    pub index: u32,
    /// PCI vendor ID (inherited from the PF's VF device ID).
    pub vendor_id: u16,
    /// PCI device ID.
    pub device_id: u16,
    /// Driver currently bound to the VF, if any.
    pub driver: Option<String>,
    /// IOMMU group of the VF — required for VFIO binding.
    pub iommu_group: Option<u32>,
    /// MAC of the VF's netdev, if the kernel created one.
    pub mac: Option<[u8; 6]>,
}

/// Format a MAC address as `aa:bb:cc:dd:ee:ff`.
#[must_use]
pub fn fmt_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn read_vendor_id(paths: &HostPaths, bdf: &str) -> Result<u16> {
    let raw = pci_attr_hex(&paths.pci_devices, bdf, "vendor")?;
    u16::try_from(raw).context("vendor ID out of range")
}

fn read_device_id(paths: &HostPaths, bdf: &str) -> Result<u16> {
    let raw = pci_attr_hex(&paths.pci_devices, bdf, "device")?;
    u16::try_from(raw).context("device ID out of range")
}

fn read_vf_count(paths: &HostPaths, bdf: &str, attr: &str) -> u16 {
    pci_attr_dec(&paths.pci_devices, bdf, attr)
        .ok()
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(0)
}

fn read_pf(paths: &HostPaths, bdf: &str) -> Result<Option<SriovNicPf>> {
    let class = pci_attr_hex(&paths.pci_devices, bdf, "class")?;
    if (class >> 16) & 0xFF != PCI_BASE_CLASS_NETWORK {
        return Ok(None);
    }
    let config = pci_config(&paths.pci_devices, bdf)?;
    if !extended_capabilities_in_config(&config)
        .iter()
        .any(|cap| cap.id == ExtendedCapability::SR_IOV)
    {
        return Ok(None);
    }
    Ok(Some(SriovNicPf {
        bdf: bdf.to_string(),
        vendor_id: read_vendor_id(paths, bdf)?,
        device_id: read_device_id(paths, bdf)?,
        total_vfs: read_vf_count(paths, bdf, "sriov_totalvfs"),
        num_vfs: read_vf_count(paths, bdf, "sriov_numvfs"),
        driver: bound_driver(&paths.pci_devices, bdf)?,
        iommu_group: iommu_group_of(&paths.pci_devices, bdf)?,
        mac: device_mac(&paths.pci_devices, bdf)?,
    }))
}

/// Discover SR-IOV-capable NIC Physical Functions on the host.
///
/// Walks the PCI sysfs tree, keeps network-class (0x02) functions whose
/// config space advertises the SR-IOV extended capability, and reports their
/// VF capacity.
///
/// # Errors
///
/// Fails on I/O errors while walking the PCI sysfs tree (a missing tree
/// yields an empty list instead — "no SR-IOV NIC" is a normal outcome).
pub fn discover_sriov_nic_pfs(paths: &HostPaths) -> Result<Vec<SriovNicPf>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&paths.pci_devices) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", paths.pci_devices.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(bdf) = name.to_str() else { continue };
        if validate_bdf(bdf).is_err() || !entry.path().is_dir() {
            continue;
        }
        if let Some(pf) = read_pf(paths, bdf)? {
            out.push(pf);
        }
    }
    out.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Virtual Function provisioning
// ---------------------------------------------------------------------------

fn read_vf(paths: &HostPaths, pf_bdf: &str, index: u32, bdf: &str) -> Result<SriovVf> {
    Ok(SriovVf {
        bdf: bdf.to_string(),
        pf_bdf: pf_bdf.to_string(),
        index,
        vendor_id: read_vendor_id(paths, bdf)?,
        device_id: read_device_id(paths, bdf)?,
        driver: bound_driver(&paths.pci_devices, bdf)?,
        iommu_group: iommu_group_of(&paths.pci_devices, bdf)?,
        mac: device_mac(&paths.pci_devices, bdf)?,
    })
}

/// List the VFs currently provisioned from the PF `pf_bdf`, via its
/// `virtfn*` symlinks, sorted by VF index.
///
/// # Errors
///
/// Fails when `pf_bdf` is malformed or a VF's attributes cannot be read.
pub fn vfs_of(paths: &HostPaths, pf_bdf: &str) -> Result<Vec<SriovVf>> {
    validate_bdf(pf_bdf)?;
    let pf_dir = paths.pci_devices.join(pf_bdf);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&pf_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", pf_dir.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(index_str) = name.strip_prefix("virtfn") else {
            continue;
        };
        let Ok(index) = index_str.parse::<u32>() else {
            continue;
        };
        let target = std::fs::read_link(entry.path())
            .with_context(|| format!("reading {}", entry.path().display()))?;
        let vf_bdf = target
            .file_name()
            .and_then(|n| n.to_str())
            .context("non-UTF8 virtfn link target")?;
        validate_bdf(vf_bdf)
            .with_context(|| format!("PF {pf_bdf} has malformed virtfn{index} target"))?;
        out.push(read_vf(paths, pf_bdf, index, vf_bdf)?);
    }
    out.sort_by_key(|vf| vf.index);
    Ok(out)
}

/// Enable `count` VFs on the PF `pf_bdf` (via `sriov_numvfs`) and wait for
/// the VF devices to appear, retrying `retries` times with `delay` between
/// attempts.
///
/// # Errors
///
/// Fails when `count` is zero, exceeds the PF's `sriov_totalvfs`, the
/// `sriov_numvfs` write fails (needs privilege), or the VFs do not appear in
/// time.
pub fn enable_vfs_with_retry(
    paths: &HostPaths,
    pf_bdf: &str,
    count: u16,
    retries: u32,
    delay: Duration,
) -> Result<Vec<SriovVf>> {
    validate_bdf(pf_bdf)?;
    if count == 0 {
        bail!("cannot enable 0 VFs on {pf_bdf}");
    }
    let total = read_vf_count(paths, pf_bdf, "sriov_totalvfs");
    if count > total {
        bail!("PF {pf_bdf} supports {total} VFs, cannot enable {count}");
    }
    std::fs::write(
        paths.pci_devices.join(pf_bdf).join("sriov_numvfs"),
        count.to_string(),
    )
    .with_context(|| format!("enabling {count} VFs on {pf_bdf} (need CAP_SYS_ADMIN)"))?;
    for _ in 0..retries {
        let vfs = vfs_of(paths, pf_bdf)?;
        if vfs.len() == usize::from(count) {
            return Ok(vfs);
        }
        std::thread::sleep(delay);
    }
    let found = vfs_of(paths, pf_bdf)?.len();
    bail!("VFs of {pf_bdf} did not appear after enabling {count} (found {found})");
}

/// Enable `count` VFs on the PF `pf_bdf`, waiting up to ~2s for them to appear.
///
/// # Errors
///
/// See [`enable_vfs_with_retry`].
pub fn enable_vfs(paths: &HostPaths, pf_bdf: &str, count: u16) -> Result<Vec<SriovVf>> {
    enable_vfs_with_retry(paths, pf_bdf, count, 50, Duration::from_millis(40))
}

/// Disable all VFs on the PF `pf_bdf` (writes `0` to `sriov_numvfs`) and wait
/// for the VF devices to go away.
///
/// # Errors
///
/// Fails when the `sriov_numvfs` write fails or the VFs do not go away in time.
pub fn disable_vfs(paths: &HostPaths, pf_bdf: &str) -> Result<()> {
    validate_bdf(pf_bdf)?;
    std::fs::write(paths.pci_devices.join(pf_bdf).join("sriov_numvfs"), "0")
        .with_context(|| format!("disabling VFs on {pf_bdf}"))?;
    for _ in 0..25 {
        if vfs_of(paths, pf_bdf)?.is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    bail!("VFs of {pf_bdf} did not go away after disabling");
}

// ---------------------------------------------------------------------------
// VFIO-bound VF and guest assignment
// ---------------------------------------------------------------------------

/// A VF bound to `vfio-pci` and attached to the shared VFIO container: the
/// live handle the hypervisor uses for passthrough.
pub struct BoundVf {
    container: Arc<VfioContainer>,
    // Kept open for the binding's lifetime: the device fd was derived from
    // this group, and closing it early would be a use-after-close hazard if
    // the kernel ever ties device-fd validity to the group.
    #[allow(dead_code)]
    group: VfioGroup,
    device: VfioPciDevice,
}

/// A VF assigned to a guest: the guest-facing passthrough handle.
///
/// Owns the VFIO binding for the assignment's lifetime; dropping the table
/// entry (via [`VfAssignmentTable::unassign`]) releases the device fd, the
/// group, and one container reference. The platform layer uses the config and
/// BAR accessors to expose the VF in the guest's PCI topology.
pub struct SriovVfAssignment {
    guest: String,
    vf: SriovVf,
    bound: BoundVf,
}

impl SriovVfAssignment {
    /// Guest identifier this VF is assigned to.
    #[must_use]
    pub fn guest(&self) -> &str {
        &self.guest
    }

    /// The assigned VF's descriptor.
    #[must_use]
    pub const fn vf(&self) -> &SriovVf {
        &self.vf
    }

    /// Read 16 bits of the VF's PCI config space (via the VFIO device fd).
    ///
    /// # Errors
    ///
    /// Fails when the config region cannot be queried or read.
    pub fn config_read16(&self, offset: u64) -> Result<u16> {
        self.bound.device.config_read16(offset)
    }

    /// Write 16 bits of the VF's PCI config space.
    ///
    /// # Errors
    ///
    /// Fails when the config region cannot be queried or written.
    pub fn config_write16(&self, offset: u64, value: u16) -> Result<()> {
        self.bound.device.config_write16(offset, value)
    }

    /// Query all six BAR regions of the VF.
    ///
    /// # Errors
    ///
    /// Fails when the kernel rejects a region query.
    pub fn bars(&self) -> Result<Vec<PciRegion>> {
        (0..6).map(|i| self.bound.device.region(i)).collect()
    }

    /// Issue a Function-Level Reset on the VF.
    ///
    /// # Errors
    ///
    /// Fails when the kernel rejects the reset.
    pub fn reset(&self) -> Result<()> {
        self.bound.device.reset()
    }

    /// Map a host virtual range for VF DMA at `iova` (Type-1 IOMMU).
    ///
    /// # Errors
    ///
    /// Fails when the kernel rejects the mapping.
    pub fn dma_map(&self, vaddr: u64, iova: u64, size: u64, writable: bool) -> Result<DmaMapping> {
        self.bound.container.dma_map(vaddr, iova, size, writable)
    }
}

/// Tracks which VF is assigned to which guest. A VF may be assigned to at
/// most one guest at a time; assigning an already-assigned VF is an error.
#[derive(Default)]
pub struct VfAssignmentTable {
    assignments: HashMap<String, SriovVfAssignment>,
}

impl VfAssignmentTable {
    /// An empty assignment table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an assignment.
    ///
    /// # Errors
    ///
    /// Fails when the VF is already assigned to a guest.
    pub fn assign(&mut self, assignment: SriovVfAssignment) -> Result<()> {
        let bdf = assignment.vf.bdf.clone();
        if let Some(existing) = self.assignments.get(&bdf) {
            bail!("VF {bdf} is already assigned to guest {:?}", existing.guest);
        }
        self.assignments.insert(bdf, assignment);
        Ok(())
    }

    /// Release a VF back to the host pool, returning its assignment (whose
    /// drop closes the VFIO fds). Returns `None` if the VF was not assigned.
    #[must_use]
    pub fn unassign(&mut self, vf_bdf: &str) -> Option<SriovVfAssignment> {
        self.assignments.remove(vf_bdf)
    }

    /// Look up the assignment for a VF BDF.
    #[must_use]
    pub fn get(&self, vf_bdf: &str) -> Option<&SriovVfAssignment> {
        self.assignments.get(vf_bdf)
    }

    /// Whether the VF is currently assigned to any guest.
    #[must_use]
    pub fn is_assigned(&self, vf_bdf: &str) -> bool {
        self.assignments.contains_key(vf_bdf)
    }

    /// All assignments belonging to `guest`.
    #[must_use]
    pub fn assigned_to(&self, guest: &str) -> Vec<&SriovVfAssignment> {
        self.assignments
            .values()
            .filter(|a| a.guest == guest)
            .collect()
    }

    /// Number of live assignments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.assignments.len()
    }

    /// Whether no VF is currently assigned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.assignments.is_empty()
    }
}

/// Bind `vf` to `vfio-pci`, attach its IOMMU group to `container` (which
/// verifies group viability), and record the assignment to `guest` in `table`.
///
/// This is the full "VF assignment to a guest" step.
///
/// # Errors
///
/// Fails when the VF is already assigned, the driver bind fails (usually
/// missing privilege), the VF has no IOMMU group (IOMMU disabled), the group
/// is not viable, or the VFIO device fd cannot be opened.
pub fn assign_vf_to_guest(
    paths: &HostPaths,
    table: &mut VfAssignmentTable,
    container: &Arc<VfioContainer>,
    guest: &str,
    vf: &SriovVf,
) -> Result<()> {
    validate_bdf(&vf.bdf)?;
    if table.is_assigned(&vf.bdf) {
        bail!("VF {} is already assigned", vf.bdf);
    }
    bind_driver(&paths.pci_devices, &paths.drivers, &vf.bdf, VFIO_PCI_DRIVER)?;
    let group_id = iommu_group_of(&paths.pci_devices, &vf.bdf)?
        .with_context(|| format!("VF {} has no IOMMU group — is the IOMMU enabled?", vf.bdf))?;
    let group = VfioGroup::open(group_id)?;
    group.set_container(container)?;
    let device = group.device(&vf.bdf)?;
    if let Err(e) = device.reset() {
        log::warn!("VF {} FLR failed (continuing): {e:#}", vf.bdf);
    }
    table.assign(SriovVfAssignment {
        guest: guest.to_string(),
        vf: vf.clone(),
        bound: BoundVf {
            container: Arc::clone(container),
            group,
            device,
        },
    })
}

/// The first VF of `vfs` not present in `table`, if any.
#[must_use]
pub fn first_free_vf<'a>(vfs: &'a [SriovVf], table: &VfAssignmentTable) -> Option<&'a SriovVf> {
    vfs.iter().find(|vf| !table.is_assigned(&vf.bdf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs as unix_fs;

    /// Fake host layout: `$root/{devices,iommu_groups,drivers}` plus a fake
    /// `/dev/vfio/vfio` stand-in path.
    struct FakeHost {
        _dir: tempfile::TempDir,
        paths: HostPaths,
    }

    impl FakeHost {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            for d in ["devices", "drivers", "iommu_groups", "dev-vfio"] {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            let paths = HostPaths {
                pci_devices: root.join("devices"),
                iommu_groups: root.join("iommu_groups"),
                drivers: root.join("drivers"),
                vfio_dev: root.join("dev-vfio/vfio"),
            };
            Self { _dir: dir, paths }
        }

        fn add_device(&self, bdf: &str) -> PathBuf {
            let dev = self.paths.pci_devices.join(bdf);
            std::fs::create_dir_all(&dev).unwrap();
            dev
        }

        fn write_attr(&self, bdf: &str, attr: &str, value: &str) {
            std::fs::write(self.paths.pci_devices.join(bdf).join(attr), value).unwrap();
        }

        /// Config blob with an SR-IOV extended capability at 0x100.
        fn sriov_config() -> Vec<u8> {
            let mut config = vec![0u8; 4096];
            let header = u32::from(ExtendedCapability::SR_IOV) | (1 << 16); // ver 1, next 0
            config[0x100..0x104].copy_from_slice(&header.to_le_bytes());
            config
        }

        fn add_sriov_pf(&self, bdf: &str, total_vfs: u16) {
            self.add_device(bdf);
            self.write_attr(bdf, "class", "0x020000\n");
            self.write_attr(bdf, "vendor", "0x8086\n");
            self.write_attr(bdf, "device", "0x1528\n");
            self.write_attr(bdf, "sriov_totalvfs", &format!("{total_vfs}\n"));
            self.write_attr(bdf, "sriov_numvfs", "0\n");
            std::fs::write(
                self.paths.pci_devices.join(bdf).join("config"),
                Self::sriov_config(),
            )
            .unwrap();
        }

        fn add_vf(&self, pf_bdf: &str, index: u32, vf_bdf: &str) {
            self.add_device(vf_bdf);
            self.write_attr(vf_bdf, "class", "0x020000\n");
            self.write_attr(vf_bdf, "vendor", "0x8086\n");
            self.write_attr(vf_bdf, "device", "0x1889\n");
            unix_fs::symlink(
                format!("../{vf_bdf}"),
                self.paths
                    .pci_devices
                    .join(pf_bdf)
                    .join(format!("virtfn{index}")),
            )
            .unwrap();
        }

        fn set_iommu_group(&self, bdf: &str, group: u32) {
            unix_fs::symlink(
                format!("../../iommu_groups/{group}"),
                self.paths.pci_devices.join(bdf).join("iommu_group"),
            )
            .unwrap();
        }
    }

    #[test]
    fn gating_probe_reports_each_missing_piece() {
        let fake = FakeHost::new();
        let gating = SriovGating::probe(&fake.paths);
        // devices/ exists, iommu_groups/ is empty, fake vfio node absent.
        assert!(gating.sysfs_pci);
        assert!(!gating.iommu);
        assert!(!gating.vfio);
        assert!(!gating.ready());
        assert_eq!(gating.missing().len(), 2);

        std::fs::create_dir_all(fake.paths.iommu_groups.join("7")).unwrap();
        std::fs::write(&fake.paths.vfio_dev, "").unwrap();
        let gating = SriovGating::probe(&fake.paths);
        assert!(gating.ready());
        assert!(gating.missing().is_empty());
    }

    #[test]
    fn discover_finds_only_sriov_nics() {
        let fake = FakeHost::new();
        fake.add_sriov_pf("0000:03:00.0", 4);
        // Storage controller: wrong class.
        fake.add_device("0000:04:00.0");
        fake.write_attr("0000:04:00.0", "class", "0x010802\n");
        fake.write_attr("0000:04:00.0", "vendor", "0x144d\n");
        fake.write_attr("0000:04:00.0", "device", "0xa808\n");
        // Plain NIC without the SR-IOV extended cap.
        fake.add_device("0000:05:00.0");
        fake.write_attr("0000:05:00.0", "class", "0x020000\n");
        fake.write_attr("0000:05:00.0", "vendor", "0x10ec\n");
        fake.write_attr("0000:05:00.0", "device", "0x8168\n");
        std::fs::write(
            fake.paths.pci_devices.join("0000:05:00.0/config"),
            vec![0u8; 4096],
        )
        .unwrap();
        // Non-BDF entry is skipped.
        std::fs::create_dir_all(fake.paths.pci_devices.join("not-a-device")).unwrap();

        // PF netdev with a MAC.
        let net = fake.paths.pci_devices.join("0000:03:00.0/net/eth0");
        std::fs::create_dir_all(&net).unwrap();
        std::fs::write(net.join("address"), "52:54:00:aa:bb:cc\n").unwrap();

        let pfs = discover_sriov_nic_pfs(&fake.paths).unwrap();
        assert_eq!(pfs.len(), 1);
        let pf = &pfs[0];
        assert_eq!(pf.bdf, "0000:03:00.0");
        assert_eq!(pf.vendor_id, 0x8086);
        assert_eq!(pf.device_id, 0x1528);
        assert_eq!(pf.total_vfs, 4);
        assert_eq!(pf.num_vfs, 0);
        assert_eq!(pf.mac, Some([0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc]));
        assert_eq!(fmt_mac(&pf.mac.unwrap()), "52:54:00:aa:bb:cc");
    }

    #[test]
    fn discover_on_missing_sysfs_is_empty_not_an_error() {
        let fake = FakeHost::new();
        let mut paths = fake.paths.clone();
        paths.pci_devices = fake.paths.pci_devices.join("does-not-exist");
        assert!(discover_sriov_nic_pfs(&paths).unwrap().is_empty());
    }

    #[test]
    fn vfs_of_resolves_virtfn_symlinks_sorted() {
        let fake = FakeHost::new();
        fake.add_sriov_pf("0000:03:00.0", 4);
        fake.add_vf("0000:03:00.0", 1, "0000:03:10.1");
        fake.add_vf("0000:03:00.0", 0, "0000:03:10.0");
        fake.set_iommu_group("0000:03:10.0", 12);

        let vfs = vfs_of(&fake.paths, "0000:03:00.0").unwrap();
        assert_eq!(vfs.len(), 2);
        assert_eq!(vfs[0].index, 0);
        assert_eq!(vfs[0].bdf, "0000:03:10.0");
        assert_eq!(vfs[0].pf_bdf, "0000:03:00.0");
        assert_eq!(vfs[0].device_id, 0x1889);
        assert_eq!(vfs[0].iommu_group, Some(12));
        assert_eq!(vfs[1].index, 1);
        assert_eq!(vfs[1].bdf, "0000:03:10.1");
    }

    #[test]
    fn enable_vfs_validates_and_waits_for_devices() {
        let fake = FakeHost::new();
        fake.add_sriov_pf("0000:03:00.0", 4);

        assert!(
            enable_vfs_with_retry(&fake.paths, "0000:03:00.0", 0, 3, Duration::from_millis(1))
                .is_err()
        );
        assert!(
            enable_vfs_with_retry(&fake.paths, "0000:03:00.0", 5, 3, Duration::from_millis(1))
                .is_err()
        );

        // Pre-created VFs satisfy the post-write poll immediately.
        fake.add_vf("0000:03:00.0", 0, "0000:03:10.0");
        fake.add_vf("0000:03:00.0", 1, "0000:03:10.1");
        let vfs =
            enable_vfs_with_retry(&fake.paths, "0000:03:00.0", 2, 3, Duration::from_millis(1))
                .unwrap();
        assert_eq!(vfs.len(), 2);
        assert_eq!(
            std::fs::read_to_string(fake.paths.pci_devices.join("0000:03:00.0/sriov_numvfs"))
                .unwrap(),
            "2"
        );
    }

    #[test]
    fn enable_vfs_times_out_when_vfs_never_appear() {
        let fake = FakeHost::new();
        fake.add_sriov_pf("0000:03:00.0", 4);
        let err =
            enable_vfs_with_retry(&fake.paths, "0000:03:00.0", 2, 3, Duration::from_millis(1))
                .unwrap_err();
        assert!(format!("{err:#}").contains("did not appear"));
    }

    #[test]
    fn disable_vfs_writes_zero() {
        let fake = FakeHost::new();
        fake.add_sriov_pf("0000:03:00.0", 4);
        fake.write_attr("0000:03:00.0", "sriov_numvfs", "2\n");
        // No virtfn symlinks: already effectively disabled.
        disable_vfs(&fake.paths, "0000:03:00.0").unwrap();
        assert_eq!(
            std::fs::read_to_string(fake.paths.pci_devices.join("0000:03:00.0/sriov_numvfs"))
                .unwrap(),
            "0"
        );
    }

    fn stub_assignment(guest: &str, vf_bdf: &str) -> SriovVfAssignment {
        SriovVfAssignment {
            guest: guest.to_string(),
            vf: SriovVf {
                bdf: vf_bdf.to_string(),
                pf_bdf: "0000:03:00.0".to_string(),
                index: 0,
                vendor_id: 0x8086,
                device_id: 0x1889,
                driver: None,
                iommu_group: Some(12),
                mac: None,
            },
            bound: BoundVf {
                container: Arc::new(VfioContainer::stub()),
                group: VfioGroup::stub(12),
                device: VfioPciDevice::stub(vf_bdf),
            },
        }
    }

    #[test]
    fn assignment_table_tracks_guests_and_rejects_double_assign() {
        let mut table = VfAssignmentTable::new();
        assert!(table.is_empty());
        table
            .assign(stub_assignment("guest-0", "0000:03:10.0"))
            .unwrap();
        table
            .assign(stub_assignment("guest-1", "0000:03:10.1"))
            .unwrap();
        assert_eq!(table.len(), 2);
        assert!(table.is_assigned("0000:03:10.0"));
        assert!(!table.is_assigned("0000:03:10.2"));

        // Double-assigning the same VF fails and keeps the original owner.
        let err = table
            .assign(stub_assignment("guest-1", "0000:03:10.0"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("already assigned"));
        assert_eq!(table.get("0000:03:10.0").unwrap().guest(), "guest-0");

        assert_eq!(table.assigned_to("guest-0").len(), 1);
        assert_eq!(table.assigned_to("guest-1").len(), 1);
        assert!(table.assigned_to("guest-9").is_empty());

        let released = table.unassign("0000:03:10.0").unwrap();
        assert_eq!(released.guest(), "guest-0");
        assert_eq!(released.vf().bdf, "0000:03:10.0");
        assert_eq!(released.vf().iommu_group, Some(12));
        assert!(!table.is_assigned("0000:03:10.0"));
        assert!(table.unassign("0000:03:10.0").is_none());
    }

    #[test]
    fn first_free_vf_skips_assigned_ones() {
        let vfs = vec![
            stub_assignment("guest-0", "0000:03:10.0").vf,
            stub_assignment("guest-0", "0000:03:10.1").vf,
        ];
        let mut table = VfAssignmentTable::new();
        assert_eq!(first_free_vf(&vfs, &table).unwrap().bdf, "0000:03:10.0");
        table
            .assign(stub_assignment("guest-0", "0000:03:10.0"))
            .unwrap();
        assert_eq!(first_free_vf(&vfs, &table).unwrap().bdf, "0000:03:10.1");
        table
            .assign(stub_assignment("guest-0", "0000:03:10.1"))
            .unwrap();
        assert!(first_free_vf(&vfs, &table).is_none());
    }

    #[test]
    fn assign_vf_to_guest_rejects_already_assigned_without_touching_sysfs() {
        let fake = FakeHost::new();
        let mut table = VfAssignmentTable::new();
        table
            .assign(stub_assignment("guest-0", "0000:03:10.0"))
            .unwrap();
        let container = Arc::new(VfioContainer::stub());
        let vf = stub_assignment("guest-1", "0000:03:10.0").vf;
        let err =
            assign_vf_to_guest(&fake.paths, &mut table, &container, "guest-1", &vf).unwrap_err();
        assert!(format!("{err:#}").contains("already assigned"));
        // Ownership unchanged.
        assert_eq!(table.get("0000:03:10.0").unwrap().guest(), "guest-0");
    }
}
