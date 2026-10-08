//! VFIO PCI device binding for passthrough NIC Virtual Functions.
//!
//! Host-side plumbing that hands a physical PCI function (an SR-IOV VF) to the
//! hypervisor: VFIO container / IOMMU-group / device setup, Type-1 IOMMU DMA
//! mapping, BAR region access, and the sysfs driver (un)binding that moves a
//! VF onto `vfio-pci`.
//!
//! Everything here is capability-gated by the caller: [`VfioContainer::open`]
//! fails when `/dev/vfio/vfio` is absent, and group setup fails when the
//! device's IOMMU group is not viable, so a host without VFIO/IOMMU gets a
//! clean error instead of a panic.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use anyhow::{Context, Result, bail};

// ---------------------------------------------------------------------------
// VFIO ioctl numbers (linux/vfio.h)
// ---------------------------------------------------------------------------

const VFIO_TYPE: u32 = 0x3b; // b';'

/// Encode a Linux ioctl request number: `(dir << 30) | (size << 16) |
/// (type << 8) | nr`.
const fn ioc(dir: u64, ty: u32, nr: u32, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | (nr as u64)
}

const fn vio(nr: u32) -> u64 {
    ioc(0, VFIO_TYPE, nr, 0)
}
const fn vior(nr: u32, size: usize) -> u64 {
    ioc(2, VFIO_TYPE, nr, size)
}
const fn viow(nr: u32, size: usize) -> u64 {
    ioc(1, VFIO_TYPE, nr, size)
}
const fn viowr(nr: u32, size: usize) -> u64 {
    ioc(3, VFIO_TYPE, nr, size)
}

const VFIO_GET_API_VERSION: u64 = vio(100);
const VFIO_CHECK_EXTENSION: u64 = vio(101);
const VFIO_SET_IOMMU: u64 = vio(102);
const VFIO_GROUP_GET_STATUS: u64 = vior(103, 8);
const VFIO_GROUP_SET_CONTAINER: u64 = viow(104, 4);
const VFIO_DEVICE_GET_REGION_INFO: u64 = viowr(108, 32);
const VFIO_DEVICE_RESET: u64 = vio(111);
const VFIO_IOMMU_MAP_DMA: u64 = viow(113, 32);
const VFIO_IOMMU_UNMAP_DMA: u64 = viow(114, 24);

/// `VFIO_GROUP_GET_DEVICE_FD` takes the device name as its argument, so the
/// request number depends on the name length (including the NUL terminator).
const fn vfio_group_get_device_fd(name_len_with_nul: usize) -> u64 {
    viow(106, name_len_with_nul)
}

// ---------------------------------------------------------------------------
// VFIO constants
// ---------------------------------------------------------------------------

/// Expected `VFIO_GET_API_VERSION` return value.
pub const VFIO_API_VERSION: u32 = 0;
/// Type-1 IOMMU backend: DMA addresses are translated by the host IOMMU.
pub const VFIO_TYPE1_IOMMU: u32 = 1;
/// `vfio_group_status.flags` bit: every device in the group is bound to a
/// VFIO-compatible driver or is unbound.
pub const VFIO_GROUP_FLAGS_VIABLE: u32 = 1 << 0;
/// DMA mapping is readable by the device.
pub const VFIO_DMA_MAP_FLAG_READ: u32 = 1 << 0;
/// DMA mapping is writable by the device.
pub const VFIO_DMA_MAP_FLAG_WRITE: u32 = 1 << 1;
/// PCI BAR0 region index.
pub const VFIO_PCI_BAR0_REGION_INDEX: u32 = 0;
/// PCI BAR5 region index.
pub const VFIO_PCI_BAR5_REGION_INDEX: u32 = 5;
/// PCI config-space region index.
pub const VFIO_PCI_CONFIG_REGION_INDEX: u32 = 7;
/// Number of regions a VFIO PCI device exposes (BAR0-5, ROM, config, VGA).
pub const VFIO_PCI_NUM_REGIONS: u32 = 9;

// ---------------------------------------------------------------------------
// VFIO structs (linux/vfio.h)
// ---------------------------------------------------------------------------

/// `struct vfio_group_status`.
#[repr(C)]
struct VfioGroupStatus {
    argsz: u32,
    flags: u32,
}

/// `struct vfio_region_info`.
#[repr(C)]
struct VfioRegionInfo {
    argsz: u32,
    flags: u32,
    index: u32,
    cap_offset: u32,
    size: u64,
    offset: u64,
}

/// `struct vfio_iommu_type1_dma_map`.
#[repr(C)]
struct VfioDmaMap {
    argsz: u32,
    flags: u32,
    vaddr: u64,
    iova: u64,
    size: u64,
}

/// `struct vfio_iommu_type1_dma_unmap`.
#[repr(C)]
struct VfioDmaUnmap {
    argsz: u32,
    flags: u32,
    iova: u64,
    size: u64,
}

fn ioctl_ptr(fd: &OwnedFd, req: u64, arg: *mut libc::c_void) -> Result<i32> {
    // SAFETY: `fd` is a live owned fd, `req` is a VFIO ioctl, and `arg`
    // points at a caller-owned struct of the size the request encodes.
    let ret = unsafe { libc::ioctl(fd.as_raw_fd(), req as libc::c_ulong, arg) };
    if ret < 0 {
        Err(io::Error::last_os_error()).with_context(|| format!("vfio ioctl {req:#x} failed"))
    } else {
        Ok(ret)
    }
}

fn ioctl_val(fd: &OwnedFd, req: u64, val: libc::c_ulong) -> Result<i32> {
    // SAFETY: `fd` is a live owned fd and the ioctl takes a scalar argument.
    let ret = unsafe { libc::ioctl(fd.as_raw_fd(), req as libc::c_ulong, val) };
    if ret < 0 {
        Err(io::Error::last_os_error()).with_context(|| format!("vfio ioctl {req:#x} failed"))
    } else {
        Ok(ret)
    }
}

// ---------------------------------------------------------------------------
// VFIO container
// ---------------------------------------------------------------------------

/// An open VFIO container (`/dev/vfio/vfio`) with the Type-1 IOMMU backend
/// selected — the DMA-mapping context every bound PCI device shares.
pub struct VfioContainer {
    fd: OwnedFd,
}

impl VfioContainer {
    /// Open the VFIO container, verify the API version, confirm the Type-1
    /// IOMMU extension, and select it.
    ///
    /// # Errors
    ///
    /// Fails (cleanly) when `/dev/vfio/vfio` does not exist, the kernel
    /// reports an unexpected API version, or the Type-1 IOMMU extension is
    /// unsupported — the first capability gate for passthrough.
    #[must_use = "the container owns the VFIO fd; dropping it closes passthrough"]
    pub fn open() -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/vfio/vfio")
            .context("opening /dev/vfio/vfio (VFIO not available on this host?)")?;
        let fd = OwnedFd::from(file);

        let version = ioctl_val(&fd, VFIO_GET_API_VERSION, 0)?;
        let version = u32::try_from(version).context("VFIO returned a negative API version")?;
        if version != VFIO_API_VERSION {
            bail!("unexpected VFIO API version {version}");
        }
        let type1 = u64::from(VFIO_TYPE1_IOMMU);
        let has_type1 = ioctl_val(&fd, VFIO_CHECK_EXTENSION, type1 as libc::c_ulong)?;
        if has_type1 == 0 {
            bail!("VFIO Type-1 IOMMU extension not supported");
        }
        ioctl_val(&fd, VFIO_SET_IOMMU, type1 as libc::c_ulong)?;
        Ok(Self { fd })
    }

    /// Map a host virtual range into the IOMMU address space at `iova`.
    ///
    /// The VF's DMA then targets `iova`; the host IOMMU translates it back
    /// to this mapping, so guest-physical pages pinned here are what the
    /// physical NIC reads/writes.
    ///
    /// # Errors
    ///
    /// Fails when the kernel rejects the mapping (e.g. overlapping or
    /// unaligned range).
    pub fn dma_map(&self, vaddr: u64, iova: u64, size: u64, writable: bool) -> Result<DmaMapping> {
        let mut flags = VFIO_DMA_MAP_FLAG_READ;
        if writable {
            flags |= VFIO_DMA_MAP_FLAG_WRITE;
        }
        let argsz = u32::try_from(size_of::<VfioDmaMap>()).context("VfioDmaMap too large")?;
        let mut map = VfioDmaMap {
            argsz,
            flags,
            vaddr,
            iova,
            size,
        };
        ioctl_ptr(
            &self.fd,
            VFIO_IOMMU_MAP_DMA,
            (&raw mut map).cast::<libc::c_void>(),
        )?;
        Ok(DmaMapping { iova, size })
    }

    /// Remove a DMA mapping previously installed with [`dma_map`](Self::dma_map).
    ///
    /// # Errors
    ///
    /// Fails when the kernel rejects the unmap (e.g. no such mapping).
    pub fn dma_unmap(&self, mapping: &DmaMapping) -> Result<()> {
        let argsz = u32::try_from(size_of::<VfioDmaUnmap>()).context("VfioDmaUnmap too large")?;
        let mut unmap = VfioDmaUnmap {
            argsz,
            flags: 0,
            iova: mapping.iova,
            size: mapping.size,
        };
        ioctl_ptr(
            &self.fd,
            VFIO_IOMMU_UNMAP_DMA,
            (&raw mut unmap).cast::<libc::c_void>(),
        )?;
        Ok(())
    }
}

/// A live Type-1 IOMMU DMA mapping; records the range so it can be torn down.
pub struct DmaMapping {
    /// I/O virtual address the device uses.
    pub iova: u64,
    /// Mapping length in bytes.
    pub size: u64,
}

#[cfg(test)]
impl VfioContainer {
    /// Test stub: a container wrapping an `eventfd`, with no kernel VFIO state.
    /// Only for exercising ownership/assignment logic without hardware.
    pub(crate) fn stub() -> Self {
        // SAFETY: eventfd(0, 0) returns a fresh fd on success.
        let fd = unsafe { libc::eventfd(0, 0) };
        assert!(fd >= 0, "eventfd failed");
        // SAFETY: `fd` is a fresh fd owned by us.
        Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        }
    }
}

// ---------------------------------------------------------------------------
// VFIO group
// ---------------------------------------------------------------------------

/// An open VFIO IOMMU group (`/dev/vfio/<id>`), bound to a container.
pub struct VfioGroup {
    fd: OwnedFd,
    /// IOMMU group number.
    pub id: u32,
}

impl VfioGroup {
    /// Open the group device node.
    ///
    /// # Errors
    ///
    /// Fails when `/dev/vfio/<id>` cannot be opened.
    pub fn open(id: u32) -> Result<Self> {
        let path = format!("/dev/vfio/{id}");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {path}"))?;
        Ok(Self {
            fd: OwnedFd::from(file),
            id,
        })
    }

    /// Attach the group to `container` and verify the group is viable (every
    /// device in it is VFIO-bound or unbound) — the IOMMU safety gate.
    ///
    /// # Errors
    ///
    /// Fails when the container cannot be set or the group is not viable.
    pub fn set_container(&self, container: &VfioContainer) -> Result<()> {
        let container_fd =
            u64::try_from(container.fd.as_raw_fd()).context("container fd invalid")?;
        ioctl_val(
            &self.fd,
            VFIO_GROUP_SET_CONTAINER,
            container_fd as libc::c_ulong,
        )?;
        let mut status = VfioGroupStatus { argsz: 8, flags: 0 };
        ioctl_ptr(
            &self.fd,
            VFIO_GROUP_GET_STATUS,
            (&raw mut status).cast::<libc::c_void>(),
        )?;
        if status.flags & VFIO_GROUP_FLAGS_VIABLE == 0 {
            bail!("IOMMU group {} is not viable for VFIO", self.id);
        }
        Ok(())
    }

    /// Open the VFIO device fd for the PCI function `bdf`
    /// (`"dddd:bb:dd.f"`).
    ///
    /// # Errors
    ///
    /// Fails when `bdf` is malformed or the kernel refuses the device fd.
    pub fn device(&self, bdf: &str) -> Result<VfioPciDevice> {
        validate_bdf(bdf)?;
        let name = CString::new(bdf).context("BDF contained an interior NUL")?;
        let bytes = name.as_bytes_with_nul();
        let req = vfio_group_get_device_fd(bytes.len());
        // VFIO_GROUP_GET_DEVICE_FD returns the new fd directly.
        // SAFETY: `bytes` lives for the call and the request encodes its length.
        let ret = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                req as libc::c_ulong,
                bytes.as_ptr().cast::<libc::c_void>(),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("VFIO_GROUP_GET_DEVICE_FD for {bdf}"));
        }
        // SAFETY: the kernel returned a fresh fd on success.
        let fd = unsafe { OwnedFd::from_raw_fd(ret) };
        Ok(VfioPciDevice {
            fd,
            bdf: bdf.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// VFIO PCI device
// ---------------------------------------------------------------------------

/// One VFIO-bound PCI region (BAR, ROM, config, …).
pub struct PciRegion {
    /// Region index (`0..=5` are BARs, `7` is config space).
    pub index: u32,
    /// Region size in bytes.
    pub size: u64,
    /// Device-fd offset for `mmap`.
    pub offset: u64,
}

impl PciRegion {
    /// Whether the region supports `mmap` (non-empty regions do).
    #[must_use]
    pub const fn mmapable(&self) -> bool {
        self.size > 0
    }
}

/// A VFIO-bound PCI function: config-space access, BAR region info, reset.
pub struct VfioPciDevice {
    fd: OwnedFd,
    /// BDF the device was opened as.
    pub bdf: String,
}

impl VfioPciDevice {
    /// Issue a Function-Level Reset.
    ///
    /// # Errors
    ///
    /// Fails when the kernel rejects the reset.
    pub fn reset(&self) -> Result<()> {
        ioctl_val(&self.fd, VFIO_DEVICE_RESET, 0)?;
        Ok(())
    }

    /// Query region `index` (BARs `0..=5`, config space `7`).
    ///
    /// # Errors
    ///
    /// Fails when the index is out of range or the kernel rejects the query.
    pub fn region(&self, index: u32) -> Result<PciRegion> {
        if index >= VFIO_PCI_NUM_REGIONS {
            bail!("VFIO PCI region index {index} out of range");
        }
        let argsz =
            u32::try_from(size_of::<VfioRegionInfo>()).context("VfioRegionInfo too large")?;
        let mut info = VfioRegionInfo {
            argsz,
            flags: 0,
            index,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        ioctl_ptr(
            &self.fd,
            VFIO_DEVICE_GET_REGION_INFO,
            (&raw mut info).cast::<libc::c_void>(),
        )?;
        Ok(PciRegion {
            index,
            size: info.size,
            offset: info.offset,
        })
    }

    /// Memory-map a region that supports it (BARs); returns the mapping.
    ///
    /// # Errors
    ///
    /// Fails when the region is empty or `mmap` is rejected.
    pub fn mmap_region(&self, region: &PciRegion) -> Result<MmapRegion> {
        let len = usize::try_from(region.size).context("VFIO region size out of range")?;
        if len == 0 {
            bail!("cannot mmap empty region {}", region.index);
        }
        let offset = i64::try_from(region.offset).context("VFIO region offset out of range")?;
        // SAFETY: offset/size come from the kernel's own region info; the
        // mapping is private to this process and unmapped on drop.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd.as_raw_fd(),
                offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error()).context("mmap VFIO PCI region");
        }
        Ok(MmapRegion {
            ptr: ptr.cast::<u8>(),
            len,
        })
    }

    fn config_pread(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        // The config region is exposed at its device-fd offset; pread it.
        let region = self.region(VFIO_PCI_CONFIG_REGION_INDEX)?;
        let at = i64::try_from(region.offset).context("config region offset out of range")?;
        let at = at
            .checked_add(i64::try_from(offset).context("config offset out of range")?)
            .context("config offset overflow")?;
        // SAFETY: `fd` is live, `buf` is a valid writable slice.
        let ret = unsafe {
            libc::pread(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                at as libc::off_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error()).context("reading VFIO PCI config space");
        }
        let n = usize::try_from(ret).context("pread returned an invalid count")?;
        if n != buf.len() {
            bail!(
                "short read of VFIO PCI config space ({n} of {} bytes)",
                buf.len()
            );
        }
        Ok(())
    }

    fn config_pwrite(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let region = self.region(VFIO_PCI_CONFIG_REGION_INDEX)?;
        let at = i64::try_from(region.offset).context("config region offset out of range")?;
        let at = at
            .checked_add(i64::try_from(offset).context("config offset out of range")?)
            .context("config offset overflow")?;
        // SAFETY: `fd` is live, `buf` is a valid readable slice.
        let ret = unsafe {
            libc::pwrite(
                self.fd.as_raw_fd(),
                buf.as_ptr().cast::<libc::c_void>(),
                buf.len(),
                at as libc::off_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error()).context("writing VFIO PCI config space");
        }
        let n = usize::try_from(ret).context("pwrite returned an invalid count")?;
        if n != buf.len() {
            bail!(
                "short write of VFIO PCI config space ({n} of {} bytes)",
                buf.len()
            );
        }
        Ok(())
    }

    /// Read 16 bits of PCI config space.
    ///
    /// # Errors
    ///
    /// Fails when the config region cannot be queried or read.
    pub fn config_read16(&self, offset: u64) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.config_pread(offset, &mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    /// Write 16 bits of PCI config space.
    ///
    /// # Errors
    ///
    /// Fails when the config region cannot be queried or written.
    pub fn config_write16(&self, offset: u64, value: u16) -> Result<()> {
        self.config_pwrite(offset, &value.to_le_bytes())
    }
}

#[cfg(test)]
mod test_stubs {
    use super::{OwnedFd, VfioGroup, VfioPciDevice};
    use std::os::fd::FromRawFd;

    impl VfioGroup {
        /// Test stub: a group wrapping an `eventfd`, with no kernel VFIO state.
        pub(crate) fn stub(id: u32) -> Self {
            // SAFETY: eventfd(0, 0) returns a fresh fd on success.
            let fd = unsafe { libc::eventfd(0, 0) };
            assert!(fd >= 0, "eventfd failed");
            Self {
                // SAFETY: `fd` is a fresh fd owned by us.
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                id,
            }
        }
    }

    impl VfioPciDevice {
        /// Test stub: a device wrapping an `eventfd`, with no kernel VFIO state.
        pub(crate) fn stub(bdf: &str) -> Self {
            // SAFETY: eventfd(0, 0) returns a fresh fd on success.
            let fd = unsafe { libc::eventfd(0, 0) };
            assert!(fd >= 0, "eventfd failed");
            Self {
                // SAFETY: `fd` is a fresh fd owned by us.
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                bdf: bdf.to_string(),
            }
        }
    }
}

/// A `mmap`'d VFIO PCI BAR region; unmapped on drop.
pub struct MmapRegion {
    ptr: *mut u8,
    len: usize,
}

impl MmapRegion {
    /// Raw access to the mapped BAR.
    #[must_use]
    pub const fn as_ptr(&self) -> *const u8 {
        self.ptr.cast_const()
    }

    /// Mapping length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// SAFETY: the mapping is a plain byte window; ownership moves with the value.
unsafe impl Send for MmapRegion {}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` came from a successful `mmap` and are unmapped
        // exactly once here.
        unsafe {
            libc::munmap(self.ptr.cast::<libc::c_void>(), self.len);
        }
    }
}

// ---------------------------------------------------------------------------
// BDF validation and sysfs driver binding
// ---------------------------------------------------------------------------

/// Validate a PCI BDF string (`"dddd:bb:dd.f"`, hex fields).
///
/// # Errors
///
/// Fails when the string is not of the form `dddd:bb:dd.f` with hex fields of
/// the right widths.
#[must_use = "validation result must be checked"]
pub fn validate_bdf(bdf: &str) -> Result<()> {
    let (domain, rest) = bdf
        .split_once(':')
        .context("BDF missing domain separator ':'")?;
    let (bus, rest) = rest
        .split_once(':')
        .context("BDF missing bus separator ':'")?;
    let (device, function) = rest
        .split_once('.')
        .context("BDF missing function separator '.'")?;
    let fields = [(domain, 4), (bus, 2), (device, 2), (function, 1)];
    for (field, width) in fields {
        if field.len() != width || !field.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("malformed BDF {bdf:?}");
        }
    }
    Ok(())
}

/// Read the first line of a sysfs attribute file, trimmed.
fn sysfs_read_trim(path: &Path) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text.lines().next().unwrap_or("").trim().to_string())
}

/// Write a value to a sysfs attribute file.
fn sysfs_write(path: &Path, value: &str) -> Result<()> {
    std::fs::write(path, value).with_context(|| format!("writing {}", path.display()))
}

/// Parse a sysfs hex attribute (`"0x8086\n"`).
fn parse_hex_attr(text: &str) -> Result<u32> {
    let text = text.trim();
    let digits = text.strip_prefix("0x").unwrap_or(text);
    u32::from_str_radix(digits, 16).with_context(|| format!("parsing hex sysfs value {text:?}"))
}

/// The driver a PCI device is currently bound to, from its `driver` symlink.
///
/// # Errors
///
/// Fails on I/O errors other than the symlink being absent (unbound device).
pub fn bound_driver(pci_devices: &Path, bdf: &str) -> Result<Option<String>> {
    let link = pci_devices.join(bdf).join("driver");
    match std::fs::read_link(&link) {
        Ok(target) => Ok(target
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", link.display())),
    }
}

/// Perform the sysfs writes that move `bdf` onto `driver` (unbind the current
/// driver if any, set `driver_override`, write the new `bind`), without the
/// final verification read — the kernel completes the bind asynchronously, so
/// the caller (or [`bind_driver`]) re-reads the `driver` symlink afterwards.
///
/// # Errors
///
/// Fails when `bdf` is malformed or unknown, or a sysfs write fails
/// (typically missing privilege).
pub fn bind_driver_writes(
    pci_devices: &Path,
    drivers_dir: &Path,
    bdf: &str,
    driver: &str,
) -> Result<()> {
    validate_bdf(bdf)?;
    let dev = pci_devices.join(bdf);
    if !dev.is_dir() {
        bail!(
            "PCI device {bdf} not present under {}",
            pci_devices.display()
        );
    }
    if let Some(current) = bound_driver(pci_devices, bdf)? {
        if current == driver {
            return Ok(());
        }
        sysfs_write(&dev.join("driver").join("unbind"), bdf)?;
    }
    sysfs_write(&dev.join("driver_override"), driver)?;
    sysfs_write(&drivers_dir.join(driver).join("bind"), bdf)?;
    Ok(())
}

/// Bind the PCI function `bdf` to `driver` (e.g. `"vfio-pci"`):
/// unbind the current driver if any, set `driver_override`, then bind.
///
/// `pci_devices` is `/sys/bus/pci/devices` (injectable for tests),
/// `drivers_dir` is `/sys/bus/pci/drivers`.
///
/// # Errors
///
/// Fails when the bind cannot be performed (see [`bind_driver_writes`]) or the
/// kernel does not report the new binding afterwards.
pub fn bind_driver(pci_devices: &Path, drivers_dir: &Path, bdf: &str, driver: &str) -> Result<()> {
    bind_driver_writes(pci_devices, drivers_dir, bdf, driver)?;
    match bound_driver(pci_devices, bdf)? {
        Some(bound) if bound == driver => Ok(()),
        other => bail!("bind of {bdf} to {driver} did not take (now: {other:?})"),
    }
}

/// Clear `driver_override` so the device binds normally again.
///
/// # Errors
///
/// Fails when `bdf` is malformed or the sysfs write fails.
pub fn clear_driver_override(pci_devices: &Path, bdf: &str) -> Result<()> {
    validate_bdf(bdf)?;
    sysfs_write(&pci_devices.join(bdf).join("driver_override"), "\n")
        .context("clearing driver_override")
}

// ---------------------------------------------------------------------------
// Sysfs attribute helpers shared with the SR-IOV layer
// ---------------------------------------------------------------------------

/// Read a hex sysfs attribute (`vendor`, `device`, `class`) for a PCI device.
///
/// # Errors
///
/// Fails when the attribute cannot be read or parsed.
#[must_use = "attribute value must be used"]
pub fn pci_attr_hex(pci_devices: &Path, bdf: &str, attr: &str) -> Result<u32> {
    let text = sysfs_read_trim(&pci_devices.join(bdf).join(attr))?;
    parse_hex_attr(&text)
}

/// Read a decimal sysfs attribute (`sriov_totalvfs`, `sriov_numvfs`) for a PCI
/// device.
///
/// # Errors
///
/// Fails when the attribute cannot be read or parsed.
#[must_use = "attribute value must be used"]
pub fn pci_attr_dec(pci_devices: &Path, bdf: &str, attr: &str) -> Result<u64> {
    let text = sysfs_read_trim(&pci_devices.join(bdf).join(attr))?;
    text.parse::<u64>()
        .with_context(|| format!("parsing decimal sysfs value {text:?}"))
}

/// Read a device's config space (up to 4 KiB) from sysfs.
///
/// # Errors
///
/// Fails when the `config` attribute cannot be read.
#[must_use = "config bytes must be used"]
pub fn pci_config(pci_devices: &Path, bdf: &str) -> Result<Vec<u8>> {
    std::fs::read(pci_devices.join(bdf).join("config"))
        .with_context(|| format!("reading config space for {bdf}"))
}

/// The IOMMU group number for a PCI device, from its `iommu_group` symlink.
///
/// # Errors
///
/// Fails on I/O errors other than the symlink being absent (no IOMMU group),
/// or when the link target is not a group number.
pub fn iommu_group_of(pci_devices: &Path, bdf: &str) -> Result<Option<u32>> {
    let link = pci_devices.join(bdf).join("iommu_group");
    match std::fs::read_link(&link) {
        Ok(target) => {
            let name = target
                .file_name()
                .and_then(|n| n.to_str())
                .context("non-UTF8 iommu_group link target")?;
            Ok(Some(name.parse::<u32>().with_context(|| {
                format!("parsing iommu group {name:?}")
            })?))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", link.display())),
    }
}

/// The MAC address of a PCI network device, from `net/<iface>/address`, if the
/// kernel created a netdev for it.
///
/// # Errors
///
/// Fails on I/O errors while listing the `net` directory (a missing `net`
/// directory simply yields `None`).
pub fn device_mac(pci_devices: &Path, bdf: &str) -> Result<Option<[u8; 6]>> {
    let net_dir = pci_devices.join(bdf).join("net");
    let mut entries = match std::fs::read_dir(&net_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", net_dir.display())),
    };
    while let Some(entry) = entries.next().transpose()? {
        let addr_file = entry.path().join("address");
        let Ok(text) = std::fs::read_to_string(&addr_file) else {
            continue;
        };
        let parts: Vec<&str> = text.trim().split(':').collect();
        if parts.len() != 6 {
            continue;
        }
        let mut mac = [0u8; 6];
        let mut ok = true;
        for (i, part) in parts.iter().enumerate() {
            if let Ok(b) = u8::from_str_radix(part, 16) {
                mac[i] = b;
            } else {
                ok = false;
                break;
            }
        }
        if ok {
            return Ok(Some(mac));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vfio_ioctl_numbers_match_linux_vfio_h() {
        // _IO(';', 100) .. spot-checks against the kernel header encoding.
        assert_eq!(VFIO_GET_API_VERSION, 0x3b64);
        assert_eq!(VFIO_CHECK_EXTENSION, 0x3b65);
        assert_eq!(VFIO_SET_IOMMU, 0x3b66);
        // _IOW(';', 0x6a, len) with a 12-byte name+NUL: dir=1, size=12.
        assert_eq!(
            vfio_group_get_device_fd(12),
            (1 << 30) | (0x0c << 16) | (0x3b << 8) | 0x6a
        );
        // _IOR(';', 0x67, 8): dir=2.
        assert_eq!(
            VFIO_GROUP_GET_STATUS,
            (2 << 30) | (0x08 << 16) | (0x3b << 8) | 0x67
        );
    }

    #[test]
    fn bdf_validation_accepts_canonical_forms() {
        assert!(validate_bdf("0000:03:00.0").is_ok());
        assert!(validate_bdf("ffff:ff:1f.7").is_ok());
        assert!(validate_bdf("0000:3:00.0").is_err());
        assert!(validate_bdf("0000:03:00").is_err());
        assert!(validate_bdf("0000:03:00.0 ").is_err());
        assert!(validate_bdf("0000:03:zz.0").is_err());
        assert!(validate_bdf("").is_err());
    }

    /// Fake sysfs layout: `$root/devices/<bdf>/...` and `$root/drivers/...`.
    struct FakeSysfs {
        _dir: tempfile::TempDir,
        devices: std::path::PathBuf,
        drivers: std::path::PathBuf,
    }

    impl FakeSysfs {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let devices = dir.path().join("devices");
            let drivers = dir.path().join("drivers");
            let dev = devices.join("0000:03:10.0");
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::create_dir_all(drivers.join("vfio-pci")).unwrap();
            // VF bound to a normal NIC driver initially: `driver` is a
            // symlink like on real sysfs, and `driver/unbind` resolves
            // through it to the driver's unbind attribute.
            std::fs::create_dir_all(drivers.join("e1000e")).unwrap();
            std::fs::write(drivers.join("e1000e/unbind"), "").unwrap();
            std::os::unix::fs::symlink("../../drivers/e1000e", dev.join("driver")).unwrap();
            std::fs::write(dev.join("driver_override"), "\n").unwrap();
            std::fs::write(drivers.join("vfio-pci/bind"), "").unwrap();
            Self {
                _dir: dir,
                devices,
                drivers,
            }
        }

        /// After `bind_driver_writes`, rewrite the `driver` symlink to the
        /// target so the verification step sees the new binding (the kernel
        /// would do this on a live system).
        fn simulate_kernel_rebind(&self, bdf: &str, driver: &str) {
            let link = self.devices.join(bdf).join("driver");
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(format!("../../drivers/{driver}"), link).unwrap();
        }
    }

    #[test]
    fn bind_driver_sequence_writes_sysfs_in_order() {
        let fake = FakeSysfs::new();
        // The VF starts on e1000e.
        assert_eq!(
            bound_driver(&fake.devices, "0000:03:10.0").unwrap(),
            Some("e1000e".to_string())
        );
        // Drive the real write sequence, then emulate what the kernel does on
        // a live system: complete the bind by re-pointing the `driver`
        // symlink, which is what `bind_driver`'s verification reads.
        bind_driver_writes(&fake.devices, &fake.drivers, "0000:03:10.0", "vfio-pci").unwrap();
        let dev = fake.devices.join("0000:03:10.0");
        assert_eq!(
            std::fs::read_to_string(dev.join("driver/unbind")).unwrap(),
            "0000:03:10.0",
            "current driver is unbound first"
        );
        assert_eq!(
            std::fs::read_to_string(dev.join("driver_override")).unwrap(),
            "vfio-pci",
            "driver_override pins the next bind"
        );
        assert_eq!(
            std::fs::read_to_string(fake.drivers.join("vfio-pci/bind")).unwrap(),
            "0000:03:10.0",
            "vfio-pci bind is triggered"
        );
        fake.simulate_kernel_rebind("0000:03:10.0", "vfio-pci");
        assert_eq!(
            bound_driver(&fake.devices, "0000:03:10.0").unwrap(),
            Some("vfio-pci".to_string())
        );
        // Binding again to the same driver is a no-op (already bound).
        bind_driver_writes(&fake.devices, &fake.drivers, "0000:03:10.0", "vfio-pci").unwrap();
        // `bind_driver` itself succeeds once the kernel side has completed.
        bind_driver(&fake.devices, &fake.drivers, "0000:03:10.0", "vfio-pci").unwrap();
    }

    #[test]
    fn bind_driver_rejects_unknown_device() {
        let fake = FakeSysfs::new();
        assert!(bind_driver(&fake.devices, &fake.drivers, "0000:99:99.9", "vfio-pci").is_err());
        assert!(bind_driver(&fake.devices, &fake.drivers, "not-a-bdf", "vfio-pci").is_err());
    }

    #[test]
    fn sysfs_attr_parsers_handle_kernel_formats() {
        assert_eq!(parse_hex_attr("0x8086\n").unwrap(), 0x8086);
        assert_eq!(parse_hex_attr("0x020000").unwrap(), 0x0002_0000);
        assert!(parse_hex_attr("xyz").is_err());
    }

    #[test]
    fn iommu_group_missing_is_not_an_error() {
        let fake = FakeSysfs::new();
        assert_eq!(iommu_group_of(&fake.devices, "0000:03:10.0").unwrap(), None);
        std::os::unix::fs::symlink(
            "../../../../../kernel/iommu_groups/12",
            fake.devices.join("0000:03:10.0/iommu_group"),
        )
        .unwrap();
        assert_eq!(
            iommu_group_of(&fake.devices, "0000:03:10.0").unwrap(),
            Some(12)
        );
    }

    #[test]
    fn device_mac_parses_kernel_address_file() {
        let fake = FakeSysfs::new();
        assert_eq!(device_mac(&fake.devices, "0000:03:10.0").unwrap(), None);
        let net = fake.devices.join("0000:03:10.0/net/eth0");
        std::fs::create_dir_all(&net).unwrap();
        std::fs::write(net.join("address"), "52:54:00:12:34:56\n").unwrap();
        assert_eq!(
            device_mac(&fake.devices, "0000:03:10.0").unwrap(),
            Some([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
        );
    }
}
