//! The privileged service VM (ROADMAP 6.6) — Xen Dom0-style.
//!
//! On bare metal the service VM is the privileged domain that owns the
//! hardware drivers, runs the management console, and provides the device
//! backends (virtio-blk / virtio-net / virtio-console) the unprivileged guests
//! consume. This module is the domain model:
//!
//! - [`ServiceVmSpec`] describes the domain's envelope: its vCPUs (the AP
//!   inventory), its carved RAM, the ECAM + GOP framebuffer windows it keeps,
//!   the device-ownership table, and the backend registry.
//! - [`assemble`] validates the spec and produces a [`ServiceVmPlan`]: every
//!   discovered device starts owned by the service VM, each gets the driver
//!   6.5 will load ([`driver_for_class`]), and all MMIO windows are merged
//!   (containment-deduped) and checked against the service VM's RAM.
//! - [`ServiceVmPlan::transfer_device`] moves one function's ownership to a
//!   guest — the passthrough move primitive. The NPT still maps the window
//!   until a future revoke primitive lands; the policy half is here.
//! - [`ServiceVmPlan::build_npt`] lays the service VM's nested page tables
//!   through `enlil-hal`'s [`NptMapper`](enlil_hal::npt::NptMapper`): its RAM
//!   plus every MMIO window identity-mapped, so drivers touch real hardware.
//!
//! The pure planner is host-testable (see `tests`); the firmware half lives in
//! the kernel's `bring_up_service_vm`, which gathers the real discovery
//! results and boots the domain. The QEMU+OVMF harness asserts the boot lines.

use enlil_hal::npt::{NptError, NptMapper};

/// The service VM's domain id (Xen Dom0-style: id 0 is privileged).
pub const SERVICE_VM_ID: u32 = 0;
/// How much RAM the kernel carves for the service VM, in MiB.
pub const SERVICE_VM_RAM_MIB: u64 = 8;
/// How much RAM the kernel carves for the service VM, in bytes.
pub const SERVICE_VM_RAM_BYTES: u64 = SERVICE_VM_RAM_MIB * 1024 * 1024;
/// Minimum RAM a service-VM envelope must carry (a driver domain plus the
/// management console cannot run in less).
pub const MIN_SERVICE_VM_RAM_BYTES: u64 = 4 * 1024 * 1024;
/// 2 MiB — the service VM's RAM must be huge-page aligned (its NPT maps RAM
/// as 2 MiB leaves).
pub const SERVICE_VM_RAM_ALIGN: u64 = 2 * 1024 * 1024;
/// Upper bound on devices in one plan (bounded, allocation-free).
pub const MAX_SERVICE_VM_DEVICES: usize = 32;
/// Upper bound on merged MMIO windows in one plan.
pub const MAX_MMIO_WINDOWS: usize = 32;
/// Upper bound on device backends the service VM registers.
pub const MAX_BACKENDS: usize = 4;
/// Upper bound on MMIO windows recorded per device.
pub const MAX_WINDOWS_PER_DEVICE: usize = 4;
/// 4 KiB page size (NPT granularity floor).
const PAGE_4KIB: u64 = 4096;

/// One MMIO window the service VM owns: a BAR window, the ECAM window, or the
/// GOP framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MmioWindow {
    /// Base physical address.
    pub base: u64,
    /// Size in bytes (0 = unsized — never mapped).
    pub size: u64,
}

impl MmioWindow {
    /// Align the window to 4 KiB (base down, end up) so an NPT can map it;
    /// returns `None` for a zero-sized window.
    #[must_use]
    pub const fn page_aligned(self) -> Option<Self> {
        if self.size == 0 {
            return None;
        }
        let base = self.base & !(PAGE_4KIB - 1);
        let end = self.base.saturating_add(self.size);
        let end = end.saturating_add(PAGE_4KIB - 1) & !(PAGE_4KIB - 1);
        Some(Self {
            base,
            size: end.saturating_sub(base),
        })
    }

    /// Whether this window overlaps `other`.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.base < other.base.saturating_add(other.size)
            && other.base < self.base.saturating_add(self.size)
    }

    /// Whether this window fully contains `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        let self_end = self.base.saturating_add(self.size);
        let other_end = other.base.saturating_add(other.size);
        self.base <= other.base && other_end <= self_end
    }
}

/// Who owns a device.
///
/// The privileged service VM owns every discovered device by default; a
/// numbered guest owns one after passthrough; the hypervisor kernel itself
/// owns nothing yet (bridges stay with the service VM for config-space
/// routing — the variant exists for the future IOMMU-group owner).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Owner {
    /// The privileged service VM (Xen Dom0-style).
    #[default]
    ServiceVm,
    /// An unprivileged guest, by id — assigned via passthrough.
    Guest(u32),
    /// The hypervisor kernel itself.
    Hypervisor,
}

/// Which physical driver a device gets. The mapping is class-driven now; the
/// drivers themselves land in 6.5 (the assignment is the policy, loadable
/// later).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DriverKind {
    /// Mass storage (`AHCI` / `NVMe`).
    Storage,
    /// Network controller.
    Network,
    /// Display controller (the GOP framebuffer console).
    Display,
    /// USB controller (xHCI).
    Usb,
    /// Anything else (bridges, misc).
    #[default]
    Other,
}

/// The driver 6.5 will load for a PCI class code.
///
/// Pure policy, host-testable: storage → [`DriverKind::Storage`], network →
/// [`DriverKind::Network`], display → [`DriverKind::Display`], serial-bus USB
/// (0x0C) → [`DriverKind::Usb`], everything else → [`DriverKind::Other`].
#[must_use]
pub const fn driver_for_class(class: u8) -> DriverKind {
    match class {
        0x01 => DriverKind::Storage,
        0x02 => DriverKind::Network,
        0x03 => DriverKind::Display,
        0x0C => DriverKind::Usb,
        _ => DriverKind::Other,
    }
}

/// One discovered PCI function and the driver + owner the service VM assigns.
#[derive(Debug, Clone, Copy, Default)]
pub struct OwnedDevice {
    /// PCI device number.
    pub device: u8,
    /// PCI function number.
    pub function: u8,
    /// PCI class code.
    pub class: u8,
    /// The driver 6.5 will load for it.
    pub driver: DriverKind,
    /// Current owner (starts at [`Owner::ServiceVm`]).
    pub owner: Owner,
    /// Sized MMIO windows captured in `windows`.
    pub window_count: u8,
    /// The sized memory BAR windows.
    pub windows: [MmioWindow; MAX_WINDOWS_PER_DEVICE],
}

impl OwnedDevice {
    /// Build a service-VM-owned device from a bus-0 MMIO inventory entry.
    #[must_use]
    pub const fn from_inventory(
        device: u8,
        function: u8,
        class: u8,
        bars: &[(u64, u64)],
    ) -> Option<Self> {
        if bars.len() > MAX_WINDOWS_PER_DEVICE {
            return None;
        }
        let mut windows = [MmioWindow { base: 0, size: 0 }; MAX_WINDOWS_PER_DEVICE];
        let mut count: u8 = 0;
        let mut i = 0;
        while i < bars.len() {
            windows[count as usize] = MmioWindow {
                base: bars[i].0,
                size: bars[i].1,
            };
            count += 1;
            i += 1;
        }
        Some(Self {
            device,
            function,
            class,
            driver: driver_for_class(class),
            owner: Owner::ServiceVm,
            window_count: count,
            windows,
        })
    }
}

/// A device backend the service VM provides to unprivileged guests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Block storage (virtio-blk, backed by the NVMe/AHCI driver).
    Block,
    /// Network (virtio-net, backed by the NIC driver).
    Net,
    /// Management console (virtio-console, backed by the service VM's own
    /// serial + GOP console).
    Console,
}

/// One registered backend: its kind and its well-known name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backend {
    /// What the backend serves.
    pub kind: BackendKind,
    /// Well-known name guests attach to.
    pub name: &'static str,
}

/// The backends the service VM registers at boot — the device backends
/// guests' frontends attach to (the drivers behind them land in 6.5).
pub const DEFAULT_BACKENDS: [Backend; 3] = [
    Backend {
        kind: BackendKind::Block,
        name: "blk0",
    },
    Backend {
        kind: BackendKind::Net,
        name: "net0",
    },
    Backend {
        kind: BackendKind::Console,
        name: "con0",
    },
];

/// Everything the kernel gathered about the service VM's envelope.
pub struct ServiceVmSpec<'a> {
    /// vCPUs for the service VM (the MADT AP inventory count).
    pub vcpus: u32,
    /// System-physical base of the carved RAM.
    pub ram_base: u64,
    /// Size of the carved RAM in bytes.
    pub ram_size: u64,
    /// The discovered PCI functions, all service-VM-owned.
    pub devices: &'a [OwnedDevice],
    /// The backends to register.
    pub backends: &'a [Backend],
    /// The ECAM window the service VM uses for config-space access.
    pub ecam: Option<MmioWindow>,
    /// The GOP framebuffer window (owned by the management console).
    pub framebuffer: Option<MmioWindow>,
    /// Whether the service VM owns the 16550 serial console (COM1).
    pub console_serial: bool,
}

/// Why a [`ServiceVmSpec`] could not become a [`ServiceVmPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceVmError {
    /// No vCPUs in the spec (no AP inventory).
    NoVcpus,
    /// The RAM carve is smaller than [`MIN_SERVICE_VM_RAM_BYTES`].
    RamTooSmall {
        /// The offered size in bytes.
        size: u64,
    },
    /// The RAM base is not [`SERVICE_VM_RAM_ALIGN`]-aligned.
    RamUnaligned {
        /// The offered base.
        base: u64,
    },
    /// The RAM carve's end overflowed the address space.
    RamOverflow,
    /// A merged MMIO window overlaps the service VM's RAM — a driver touching
    /// it would corrupt the domain's own memory.
    RamOverlapsMmio {
        /// The offending window.
        window: MmioWindow,
    },
    /// More devices than [`MAX_SERVICE_VM_DEVICES`].
    TooManyDevices {
        /// The offered count.
        count: usize,
    },
    /// More MMIO windows than [`MAX_MMIO_WINDOWS`].
    TooManyMmioWindows {
        /// The offered count.
        count: usize,
    },
    /// More backends than [`MAX_BACKENDS`].
    TooManyBackends {
        /// The offered count.
        count: usize,
    },
    /// A device in the spec is not service-VM-owned at assembly time.
    DeviceNotServiceVmOwned {
        /// PCI device number.
        device: u8,
        /// PCI function number.
        function: u8,
    },
    /// The NPT builder refused the plan.
    Npt(NptError),
}

impl From<NptError> for ServiceVmError {
    fn from(err: NptError) -> Self {
        Self::Npt(err)
    }
}

/// The validated service-VM envelope: its identity, its owned devices, the
/// backends it registers, and the merged MMIO windows its NPT maps.
#[derive(Debug)]
pub struct ServiceVmPlan {
    /// vCPUs for the service VM.
    pub vcpus: u32,
    /// The carved RAM (base + size).
    pub ram: MmioWindow,
    /// Whether the service VM owns the serial console.
    pub console_serial: bool,
    /// Whether the service VM owns the GOP framebuffer console.
    pub console_framebuffer: bool,
    /// Devices owned, in `devices`.
    pub device_count: usize,
    /// The device-ownership table (all [`Owner::ServiceVm`] at assembly).
    pub devices: [OwnedDevice; MAX_SERVICE_VM_DEVICES],
    /// Backends registered, in `backends`.
    pub backend_count: usize,
    /// The registered backends.
    pub backends: [Backend; MAX_BACKENDS],
    /// Merged, page-aligned MMIO windows the NPT maps, in `mmio_windows`.
    pub mmio_window_count: usize,
    /// Every MMIO window (device BARs + ECAM + framebuffer), merged.
    pub mmio_windows: [MmioWindow; MAX_MMIO_WINDOWS],
    /// Total MMIO bytes across the merged windows.
    pub total_mmio_bytes: u64,
}

/// Why [`ServiceVmPlan::transfer_device`] refused the move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferError {
    /// No such device in the plan.
    DeviceNotFound {
        /// PCI device number.
        device: u8,
        /// PCI function number.
        function: u8,
    },
    /// The device is not service-VM-owned (already passed through or
    /// hypervisor-owned) — ownership is exclusive, never shared.
    NotOwnedByServiceVm {
        /// The current owner.
        owner: Owner,
    },
}

/// Collect every MMIO window in the spec — device BARs, then ECAM, then the
/// framebuffer — each page-aligned (NPT granularity); zero-sized windows are
/// skipped.
///
/// Returns the candidates plus their count.
///
/// # Errors
///
/// [`ServiceVmError::DeviceNotServiceVmOwned`] when a device is not
/// service-VM-owned at assembly time, or
/// [`ServiceVmError::TooManyMmioWindows`] past [`MAX_MMIO_WINDOWS`].
fn collect_mmio_windows(
    spec: &ServiceVmSpec<'_>,
) -> Result<([MmioWindow; MAX_MMIO_WINDOWS], usize), ServiceVmError> {
    let mut candidates = [MmioWindow { base: 0, size: 0 }; MAX_MMIO_WINDOWS];
    let mut candidate_count = 0usize;
    let mut push = |window: MmioWindow| -> Result<(), ServiceVmError> {
        let Some(aligned) = window.page_aligned() else {
            return Ok(());
        };
        if candidate_count >= MAX_MMIO_WINDOWS {
            return Err(ServiceVmError::TooManyMmioWindows {
                count: candidate_count + 1,
            });
        }
        candidates[candidate_count] = aligned;
        candidate_count += 1;
        Ok(())
    };
    for device in spec.devices {
        if device.owner != Owner::ServiceVm {
            return Err(ServiceVmError::DeviceNotServiceVmOwned {
                device: device.device,
                function: device.function,
            });
        }
        for window in device.windows.iter().take(usize::from(device.window_count)) {
            push(*window)?;
        }
    }
    if let Some(ecam) = spec.ecam {
        push(ecam)?;
    }
    if let Some(framebuffer) = spec.framebuffer {
        push(framebuffer)?;
    }
    Ok((candidates, candidate_count))
}

/// Merge overlapping/adjacent windows (sort by base, sweep) into `merged`.
/// This dedupes containment — e.g. the GOP framebuffer inside the display
/// BAR. Returns the merged count.
fn merge_windows(candidates: &[MmioWindow], merged: &mut [MmioWindow; MAX_MMIO_WINDOWS]) -> usize {
    let mut sorted = [MmioWindow { base: 0, size: 0 }; MAX_MMIO_WINDOWS];
    let count = candidates.len().min(MAX_MMIO_WINDOWS);
    sorted[..count].copy_from_slice(&candidates[..count]);
    sort_windows_by_base(&mut sorted[..count]);
    let mut merged_count = 0usize;
    for candidate in sorted.iter().take(count) {
        if merged_count == 0 {
            merged[0] = *candidate;
            merged_count = 1;
            continue;
        }
        let last = &mut merged[merged_count - 1];
        let last_end = last.base.saturating_add(last.size);
        if candidate.base <= last_end {
            // Overlaps or touches: extend.
            let end = candidate.base.saturating_add(candidate.size).max(last_end);
            last.size = end.saturating_sub(last.base);
        } else {
            merged[merged_count] = *candidate;
            merged_count += 1;
        }
    }
    merged_count
}

/// Validate a [`ServiceVmSpec`] into a [`ServiceVmPlan`].
///
/// Merges the device BAR windows with the ECAM + framebuffer windows
/// (containment-deduped — the GOP framebuffer usually sits inside the
/// display BAR), page-aligns them, and rejects any overlap with the service
/// VM's RAM. Pure and host-testable.
///
/// # Errors
///
/// [`ServiceVmError`] — see its variants.
pub fn assemble(spec: &ServiceVmSpec<'_>) -> Result<ServiceVmPlan, ServiceVmError> {
    if spec.vcpus == 0 {
        return Err(ServiceVmError::NoVcpus);
    }
    if spec.ram_size < MIN_SERVICE_VM_RAM_BYTES {
        return Err(ServiceVmError::RamTooSmall {
            size: spec.ram_size,
        });
    }
    if !spec.ram_base.is_multiple_of(SERVICE_VM_RAM_ALIGN) {
        return Err(ServiceVmError::RamUnaligned {
            base: spec.ram_base,
        });
    }
    // The end must be representable; `ram` carries it for the overlap test.
    spec.ram_base
        .checked_add(spec.ram_size)
        .ok_or(ServiceVmError::RamOverflow)?;
    let ram = MmioWindow {
        base: spec.ram_base,
        size: spec.ram_size,
    };
    if spec.devices.len() > MAX_SERVICE_VM_DEVICES {
        return Err(ServiceVmError::TooManyDevices {
            count: spec.devices.len(),
        });
    }
    if spec.backends.len() > MAX_BACKENDS {
        return Err(ServiceVmError::TooManyBackends {
            count: spec.backends.len(),
        });
    }

    let (candidates, candidate_count) = collect_mmio_windows(spec)?;
    let mut merged = [MmioWindow { base: 0, size: 0 }; MAX_MMIO_WINDOWS];
    let merged_count = merge_windows(&candidates[..candidate_count], &mut merged);

    // No MMIO window may cover the service VM's own RAM.
    for window in merged.iter().take(merged_count) {
        if window.overlaps(ram) {
            return Err(ServiceVmError::RamOverlapsMmio { window: *window });
        }
    }

    let mut devices = [OwnedDevice {
        device: 0,
        function: 0,
        class: 0,
        driver: DriverKind::Other,
        owner: Owner::ServiceVm,
        window_count: 0,
        windows: [MmioWindow { base: 0, size: 0 }; MAX_WINDOWS_PER_DEVICE],
    }; MAX_SERVICE_VM_DEVICES];
    devices[..spec.devices.len()].copy_from_slice(spec.devices);

    let mut backends = [Backend {
        kind: BackendKind::Block,
        name: "",
    }; MAX_BACKENDS];
    backends[..spec.backends.len()].copy_from_slice(spec.backends);

    let mut mmio_windows = [MmioWindow { base: 0, size: 0 }; MAX_MMIO_WINDOWS];
    mmio_windows[..merged_count].copy_from_slice(&merged[..merged_count]);
    let total_mmio_bytes = merged
        .iter()
        .take(merged_count)
        .fold(0u64, |acc, w| acc.saturating_add(w.size));

    Ok(ServiceVmPlan {
        vcpus: spec.vcpus,
        ram,
        console_serial: spec.console_serial,
        console_framebuffer: spec.framebuffer.is_some_and(|fb| fb.size != 0),
        device_count: spec.devices.len(),
        devices,
        backend_count: spec.backends.len(),
        backends,
        mmio_window_count: merged_count,
        mmio_windows,
        total_mmio_bytes,
    })
}

/// Insertion sort of windows by base (bounded, allocation-free).
fn sort_windows_by_base(windows: &mut [MmioWindow]) {
    for i in 1..windows.len() {
        let mut j = i;
        while j > 0 && windows[j].base < windows[j - 1].base {
            windows.swap(j, j - 1);
            j -= 1;
        }
    }
}

impl ServiceVmPlan {
    /// Move one PCI function's ownership from the service VM to a guest —
    /// the passthrough move primitive.
    ///
    /// Ownership is exclusive: the service VM stops owning the function the
    /// moment a guest claims it, so a device can never be double-assigned.
    /// (The live NPT still maps the window until a revoke primitive lands;
    /// this is the policy half.)
    ///
    /// # Errors
    ///
    /// [`TransferError::DeviceNotFound`] or
    /// [`TransferError::NotOwnedByServiceVm`].
    pub fn transfer_device(
        &mut self,
        device: u8,
        function: u8,
        guest: u32,
    ) -> Result<(), TransferError> {
        let slot = self.devices[..self.device_count]
            .iter_mut()
            .find(|d| d.device == device && d.function == function)
            .ok_or(TransferError::DeviceNotFound { device, function })?;
        if slot.owner != Owner::ServiceVm {
            return Err(TransferError::NotOwnedByServiceVm { owner: slot.owner });
        }
        slot.owner = Owner::Guest(guest);
        Ok(())
    }

    /// How many on-demand NPT tables the plan needs at most: PML4 + PDPT + one
    /// PD per GiB-straddling window plus one PT per window, over-allocated.
    #[must_use]
    pub const fn npt_table_pages(&self) -> usize {
        4 + 3 * MAX_MMIO_WINDOWS
    }

    /// Lay the service VM's nested page tables into `buf`: its RAM plus every
    /// merged MMIO window, identity-mapped (guest-physical == system-physical)
    /// so drivers touch real hardware.
    ///
    /// `phys_base` is the system-physical address of `buf[0]` (4 KiB-aligned);
    /// returns the value to program into the VMCB's `NESTED_CR3`.
    ///
    /// # Errors
    ///
    /// [`ServiceVmError::Npt`] if the mapper refuses a range (overlap, too
    /// many tables for `buf`, unaligned base).
    pub fn build_npt(&self, buf: &mut [u8], phys_base: u64) -> Result<u64, ServiceVmError> {
        let mut mapper = NptMapper::new(buf, phys_base)?;
        mapper.map_range(self.ram.base, self.ram.base, self.ram.size, &[])?;
        for window in self.mmio_windows.iter().take(self.mmio_window_count) {
            mapper.map_range(window.base, window.base, window.size, &[])?;
        }
        Ok(mapper.ncr3())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#[repr(align(4096))]` pages so a host-side NPT buffer has a usable
    /// physical base (the mapper needs 4 KiB alignment).
    #[repr(align(4096))]
    struct Page([u8; 4096]);

    fn device(device: u8, function: u8, class: u8, bars: &[(u64, u64)]) -> OwnedDevice {
        OwnedDevice::from_inventory(device, function, class, bars).unwrap()
    }

    fn spec<'a>(
        devices: &'a [OwnedDevice],
        backends: &'a [Backend],
        framebuffer: Option<MmioWindow>,
    ) -> ServiceVmSpec<'a> {
        ServiceVmSpec {
            vcpus: 2,
            ram_base: 0x20_0000,
            ram_size: SERVICE_VM_RAM_BYTES,
            devices,
            backends,
            ecam: Some(MmioWindow {
                base: 0xE000_0000,
                size: 0x1000_0000,
            }),
            framebuffer,
            console_serial: true,
        }
    }

    #[test]
    fn assembles_a_privileged_domain() {
        let devices = [
            device(2, 0, 0x03, &[(0x8000_0000, 0x100_0000)]), // display: VGA BAR
            device(3, 0, 0x01, &[(0x8200_0000, 0x1000)]),     // storage
        ];
        // The GOP framebuffer sits inside the display BAR: merged away.
        let fb = Some(MmioWindow {
            base: 0x8000_0000,
            size: 0x300_000,
        });
        let plan = assemble(&spec(&devices, &DEFAULT_BACKENDS, fb)).unwrap();
        assert_eq!(plan.vcpus, 2);
        assert_eq!(plan.ram.base, 0x20_0000);
        assert_eq!(plan.device_count, 2);
        assert_eq!(plan.devices[0].driver, DriverKind::Display);
        assert_eq!(plan.devices[1].driver, DriverKind::Storage);
        assert!(
            plan.devices
                .iter()
                .take(2)
                .all(|d| d.owner == Owner::ServiceVm)
        );
        assert_eq!(plan.backend_count, 3);
        assert!(plan.console_serial);
        assert!(plan.console_framebuffer);
        // 3 merged windows: VGA BAR (framebuffer inside it), storage BAR, ECAM.
        assert_eq!(plan.mmio_window_count, 3);
        assert_eq!(plan.total_mmio_bytes, 0x100_0000 + 0x1000 + 0x1000_0000);
    }

    #[test]
    fn merges_adjacent_windows() {
        let devices = [device(
            4,
            0,
            0x02,
            &[(0x9000_0000, 0x1000), (0x9000_1000, 0x1000)],
        )];
        let plan = assemble(&spec(&devices, &[], None)).unwrap();
        // The two adjacent 4 KiB windows merge with nothing else nearby: one
        // window of 8 KiB, plus ECAM.
        assert_eq!(plan.mmio_window_count, 2);
        let merged = plan.mmio_windows[0];
        assert_eq!(merged.base, 0x9000_0000);
        assert_eq!(merged.size, 0x2000);
    }

    #[test]
    fn rejects_a_missing_ap_inventory() {
        let mut s = spec(&[], &[], None);
        s.vcpus = 0;
        assert_eq!(assemble(&s).unwrap_err(), ServiceVmError::NoVcpus);
    }

    #[test]
    fn rejects_too_little_ram() {
        let mut s = spec(&[], &[], None);
        s.ram_size = MIN_SERVICE_VM_RAM_BYTES - 1;
        assert_eq!(
            assemble(&s).unwrap_err(),
            ServiceVmError::RamTooSmall { size: s.ram_size }
        );
    }

    #[test]
    fn rejects_unaligned_ram() {
        let mut s = spec(&[], &[], None);
        s.ram_base = 0x20_1000;
        assert_eq!(
            assemble(&s).unwrap_err(),
            ServiceVmError::RamUnaligned { base: 0x20_1000 }
        );
    }

    #[test]
    fn rejects_mmio_overlapping_ram() {
        // A BAR decoding over the service VM's own RAM must never be mapped.
        let devices = [device(5, 0, 0x01, &[(0x20_0000, 0x1000)])];
        let err = assemble(&spec(&devices, &[], None)).unwrap_err();
        assert_eq!(
            err,
            ServiceVmError::RamOverlapsMmio {
                window: MmioWindow {
                    base: 0x20_0000,
                    size: 0x1000
                }
            }
        );
    }

    #[test]
    fn rejects_a_device_not_owned_by_the_service_vm() {
        let mut rogue = device(6, 0, 0x02, &[(0x9100_0000, 0x1000)]);
        rogue.owner = Owner::Guest(1);
        let devices = [rogue];
        let err = assemble(&spec(&devices, &[], None)).unwrap_err();
        assert_eq!(
            err,
            ServiceVmError::DeviceNotServiceVmOwned {
                device: 6,
                function: 0
            }
        );
    }

    #[test]
    fn transfer_moves_ownership_to_a_guest_exactly_once() {
        let devices = [device(7, 0, 0x02, &[(0x9200_0000, 0x1000)])];
        let mut plan = assemble(&spec(&devices, &[], None)).unwrap();
        plan.transfer_device(7, 0, 1).unwrap();
        assert_eq!(plan.devices[0].owner, Owner::Guest(1));
        // Double-assignment is refused: ownership is exclusive.
        assert_eq!(
            plan.transfer_device(7, 0, 2),
            Err(TransferError::NotOwnedByServiceVm {
                owner: Owner::Guest(1)
            })
        );
        assert_eq!(
            plan.transfer_device(9, 0, 1),
            Err(TransferError::DeviceNotFound {
                device: 9,
                function: 0
            })
        );
    }

    #[test]
    fn driver_policy_maps_classes() {
        assert_eq!(driver_for_class(0x01), DriverKind::Storage);
        assert_eq!(driver_for_class(0x02), DriverKind::Network);
        assert_eq!(driver_for_class(0x03), DriverKind::Display);
        assert_eq!(driver_for_class(0x0C), DriverKind::Usb);
        assert_eq!(driver_for_class(0x06), DriverKind::Other);
        assert_eq!(driver_for_class(0xFF), DriverKind::Other);
    }

    #[test]
    fn npt_identity_maps_ram_and_mmio() {
        let devices = [device(2, 0, 0x03, &[(0x8000_0000, 0x100_0000)])];
        let plan = assemble(&spec(&devices, &[], None)).unwrap();
        // 128 pages (512 KiB) — heap-allocated, not on the test thread's stack.
        let mut pages: Vec<Page> = (0..128).map(|_| Page([0u8; 4096])).collect();
        let buf = unsafe {
            core::slice::from_raw_parts_mut(pages.as_mut_ptr().cast::<u8>(), pages.len() * 4096)
        };
        let phys_base = buf.as_ptr() as u64;
        let ncr3 = plan.build_npt(buf, phys_base).unwrap();
        assert_eq!(ncr3, phys_base);
        // The PML4's first entry is a present table pointer (low RAM mapped).
        let pml4e0 = u64::from_le_bytes(pages[0].0[0..8].try_into().unwrap());
        assert_ne!(pml4e0 & 1, 0, "PML4[0] must be present");
        // RAM and the BAR window both translate identity.
        assert_eq!(
            enlil_hal::npt::translate_npt(buf, phys_base, 0x20_0000),
            Some(0x20_0000)
        );
        assert_eq!(
            enlil_hal::npt::translate_npt(buf, phys_base, 0x8000_1234),
            Some(0x8000_1234)
        );
        // An unmapped address translates to nothing.
        assert_eq!(
            enlil_hal::npt::translate_npt(buf, phys_base, 0x7000_0000),
            None
        );
    }

    #[test]
    fn window_helpers_behave() {
        let w = MmioWindow {
            base: 0x1234,
            size: 0x100,
        };
        assert_eq!(
            w.page_aligned(),
            Some(MmioWindow {
                base: 0x1000,
                size: 0x1000
            })
        );
        assert_eq!(MmioWindow { base: 1, size: 0 }.page_aligned(), None);
        let a = MmioWindow {
            base: 0x1000,
            size: 0x1000,
        };
        let b = MmioWindow {
            base: 0x1800,
            size: 0x1000,
        };
        assert!(a.overlaps(b));
        assert!(!a.contains(b));
        let big = MmioWindow {
            base: 0x1000,
            size: 0x3000,
        };
        assert!(big.contains(b));
    }
}
