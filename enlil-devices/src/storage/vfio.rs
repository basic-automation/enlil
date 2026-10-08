//! VFIO device binding and IOMMU DMA mapping.
//!
//! Host-side plumbing for passing a physical PCI device through to the
//! hypervisor: VFIO container / IOMMU-group / device setup, Type-1 IOMMU DMA
//! mapping, BAR access, and the sysfs-based IOMMU gating that decides whether
//! passthrough is possible at all.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

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
/// `vfio_region_info.flags` bit: the region supports `mmap`.
pub const VFIO_REGION_INFO_FLAG_MMAP: u32 = 1 << 2;
/// PCI BAR0 region index. `NVMe` controller registers live in BAR0.
pub const VFIO_PCI_BAR0_REGION_INDEX: u32 = 0;
/// PCI config-space region index.
pub const VFIO_PCI_CONFIG_REGION_INDEX: u32 = 7;
/// PCI class code for `NVMe` controllers: base class 0x01 (mass storage),
/// subclass 0x08 (NVM), programming interface 0x02.
pub const PCI_CLASS_NVME: u32 = 0x01_08_02;

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

// ---------------------------------------------------------------------------
// Raw ioctl helpers
// ---------------------------------------------------------------------------

const fn void_ptr_of<T>(r: &mut T) -> *mut libc::c_void {
    std::ptr::from_mut(r).cast::<libc::c_void>()
}

const fn void_ptr_of_const<T>(r: &T) -> *mut libc::c_void {
    std::ptr::from_ref(r).cast::<libc::c_void>().cast_mut()
}

/// Issue an ioctl whose return value is only a success/failure indicator.
fn ioctl_ok(fd: &OwnedFd, req: u64, arg: *mut libc::c_void) -> io::Result<()> {
    // SAFETY: the caller passes a request number and argument valid for this fd.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as libc::c_ulong, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Issue an ioctl whose return value carries data (e.g. a file descriptor or
/// an extension probe result).
fn ioctl_ret(fd: &OwnedFd, req: u64, arg: *mut libc::c_void) -> io::Result<i32> {
    // SAFETY: the caller passes a request number and argument valid for this fd.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as libc::c_ulong, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

// ---------------------------------------------------------------------------
// VFIO container: /dev/vfio/vfio + the Type-1 IOMMU
// ---------------------------------------------------------------------------

struct ContainerInner {
    fd: OwnedFd,
}

/// An open VFIO container bound to the Type-1 IOMMU backend.
///
/// Cloning shares the underlying file description; DMA mappings hold their own
/// reference so they are always unmapped before the container closes.
#[derive(Clone)]
pub struct VfioContainer {
    inner: Arc<ContainerInner>,
}

impl VfioContainer {
    /// Open `/dev/vfio/vfio`, verify the API version, and bind the Type-1
    /// IOMMU backend.
    ///
    /// # Errors
    ///
    /// Returns an error if `/dev/vfio/vfio` cannot be opened (VFIO not
    /// enabled), the API version is unexpected, or the Type-1 IOMMU backend
    /// is unavailable.
    pub fn open() -> Result<Self> {
        let path = Path::new("/dev/vfio/vfio");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let fd = OwnedFd::from(file);

        let api = ioctl_ret(&fd, VFIO_GET_API_VERSION, std::ptr::null_mut())
            .context("VFIO_GET_API_VERSION")?;
        if u32::try_from(api).map_err(|_| anyhow::anyhow!("bad api"))? != VFIO_API_VERSION {
            bail!("unexpected VFIO API version {api}, expected {VFIO_API_VERSION}");
        }
        let ext = ioctl_ret(
            &fd,
            VFIO_CHECK_EXTENSION,
            VFIO_TYPE1_IOMMU as *mut libc::c_void,
        )
        .context("VFIO_CHECK_EXTENSION")?;
        if ext != 1 {
            bail!("VFIO Type-1 IOMMU backend not supported by this kernel");
        }
        ioctl_ok(&fd, VFIO_SET_IOMMU, VFIO_TYPE1_IOMMU as *mut libc::c_void)
            .context("VFIO_SET_IOMMU")?;

        Ok(Self {
            inner: Arc::new(ContainerInner { fd }),
        })
    }

    /// Map `[ptr, ptr + size)` at IOVA `iova` in the container's IOMMU.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `ptr` is page-aligned, `size` is a
    /// non-zero multiple of the page size, the memory is valid for the whole
    /// lifetime of the returned mapping, and it is not unmapped or freed
    /// early. [`DmaMapping`]'s `Drop` unmaps the range; dropping the mapping
    /// before the memory is the caller's responsibility to avoid (the
    /// `DmaBuf` type in the `NVMe` driver upholds this by declaration order).
    ///
    /// # Errors
    ///
    /// Returns an error if the `VFIO_IOMMU_MAP_DMA` ioctl fails (e.g. the
    /// range overlaps an existing mapping or IOVA is reserved).
    pub unsafe fn map_dma(
        &self,
        iova: u64,
        ptr: *const u8,
        size: usize,
        writable: bool,
    ) -> Result<DmaMapping> {
        if size == 0 || !size.is_multiple_of(4096) {
            bail!("DMA mapping size must be a non-zero page multiple, got {size}");
        }
        if !iova.is_multiple_of(4096) {
            bail!("DMA mapping IOVA must be page-aligned, got {iova:#x}");
        }
        if !ptr.addr().is_multiple_of(4096) {
            bail!("DMA mapping host address must be page-aligned");
        }
        let mut req = VfioDmaMap {
            argsz: 32,
            flags: VFIO_DMA_MAP_FLAG_READ | if writable { VFIO_DMA_MAP_FLAG_WRITE } else { 0 },
            vaddr: ptr.addr() as u64,
            iova,
            size: size as u64,
        };
        ioctl_ok(&self.inner.fd, VFIO_IOMMU_MAP_DMA, void_ptr_of(&mut req))
            .context("VFIO_IOMMU_MAP_DMA")?;
        Ok(DmaMapping {
            container: Arc::clone(&self.inner),
            iova,
            size,
        })
    }
}

/// A live IOMMU DMA mapping. Dropping it unmaps the range (best-effort: a
/// failed unmap ioctl is logged and ignored since `Drop` cannot fail).
pub struct DmaMapping {
    container: Arc<ContainerInner>,
    iova: u64,
    size: usize,
}

impl Drop for DmaMapping {
    fn drop(&mut self) {
        let req = VfioDmaUnmap {
            argsz: 24,
            flags: 0,
            iova: self.iova,
            size: self.size as u64,
        };
        if ioctl_ok(
            &self.container.fd,
            VFIO_IOMMU_UNMAP_DMA,
            void_ptr_of_const(&req),
        )
        .is_err()
        {
            log::warn!(
                "VFIO_IOMMU_UNMAP_DMA failed for IOVA {:#x} (size {:#x}); leaking the mapping",
                self.iova,
                self.size
            );
        }
    }
}

// ---------------------------------------------------------------------------
// VFIO group: /dev/vfio/<group>
// ---------------------------------------------------------------------------

/// An open VFIO IOMMU group, checked viable and attached to a container.
pub struct VfioGroup {
    fd: OwnedFd,
}

impl VfioGroup {
    /// Open `/dev/vfio/<id>` and verify the group is viable (every device in
    /// it is bound to a VFIO-compatible driver or unbound).
    ///
    /// # Errors
    ///
    /// Returns an error if the group device cannot be opened or the group is
    /// not viable.
    pub fn open(id: u32) -> Result<Self> {
        let path = format!("/dev/vfio/{id}");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("opening {path}"))?;
        let fd = OwnedFd::from(file);
        let mut status = VfioGroupStatus { argsz: 8, flags: 0 };
        ioctl_ok(&fd, VFIO_GROUP_GET_STATUS, void_ptr_of(&mut status))
            .context("VFIO_GROUP_GET_STATUS")?;
        if status.flags & VFIO_GROUP_FLAGS_VIABLE == 0 {
            bail!(
                "IOMMU group {id} is not viable: bind every device in the group to vfio-pci \
                 (or unbind it) before passthrough"
            );
        }
        Ok(Self { fd })
    }

    /// Attach the group to a VFIO container.
    ///
    /// # Errors
    ///
    /// Returns an error if `VFIO_GROUP_SET_CONTAINER` fails.
    pub fn set_container(&self, container: &VfioContainer) -> Result<()> {
        let raw = container.inner.fd.as_raw_fd();
        ioctl_ok(&self.fd, VFIO_GROUP_SET_CONTAINER, void_ptr_of_const(&raw))
            .context("VFIO_GROUP_SET_CONTAINER")?;
        Ok(())
    }

    /// Open the PCI device `bdf` (`dddd:bb:dd.f`) in this group.
    ///
    /// # Errors
    ///
    /// Returns an error if the device is not in this group or the
    /// `VFIO_GROUP_GET_DEVICE_FD` ioctl fails.
    pub fn device(&self, bdf: &str) -> Result<VfioPciDevice> {
        let name = CString::new(bdf).context("BDF contains an interior NUL")?;
        let bytes = name.as_bytes_with_nul();
        let req = vfio_group_get_device_fd(bytes.len());
        let ret = ioctl_ret(
            &self.fd,
            req,
            name.as_ptr().cast::<libc::c_void>().cast_mut(),
        )
        .with_context(|| format!("VFIO_GROUP_GET_DEVICE_FD for {bdf}"))?;
        // SAFETY: the kernel returns a fresh file descriptor on success.
        let fd = unsafe { OwnedFd::from_raw_fd(ret) };
        Ok(VfioPciDevice { fd })
    }
}

// ---------------------------------------------------------------------------
// VFIO PCI device
// ---------------------------------------------------------------------------

/// A VFIO region descriptor (a PCI BAR, ROM, or config space).
#[derive(Debug, Clone, Copy)]
pub struct PciRegion {
    /// VFIO region index (0-5 = BAR0-BAR5, 7 = config space).
    pub index: u32,
    /// Region size in bytes.
    pub size: u64,
    /// Offset for `mmap` on the device fd.
    pub mmap_offset: u64,
    /// `vfio_region_info.flags`.
    pub flags: u32,
}

impl PciRegion {
    /// Whether the region can be mapped into the process address space.
    #[must_use]
    pub const fn mmapable(&self) -> bool {
        self.flags & VFIO_REGION_INFO_FLAG_MMAP != 0
    }
}

/// An open VFIO PCI device.
pub struct VfioPciDevice {
    fd: OwnedFd,
}

impl VfioPciDevice {
    /// Issue a function-level or bus reset to the device.
    ///
    /// # Errors
    ///
    /// Returns an error if `VFIO_DEVICE_RESET` fails.
    pub fn reset(&self) -> Result<()> {
        ioctl_ok(&self.fd, VFIO_DEVICE_RESET, std::ptr::null_mut()).context("VFIO_DEVICE_RESET")?;
        Ok(())
    }

    /// Describe region `index` (BARs are 0-5, config space is 7).
    ///
    /// # Errors
    ///
    /// Returns an error if `VFIO_DEVICE_GET_REGION_INFO` fails.
    pub fn region(&self, index: u32) -> Result<PciRegion> {
        let mut info = VfioRegionInfo {
            argsz: 32,
            flags: 0,
            index,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        ioctl_ok(
            &self.fd,
            VFIO_DEVICE_GET_REGION_INFO,
            void_ptr_of(&mut info),
        )
        .with_context(|| format!("VFIO_DEVICE_GET_REGION_INFO for region {index}"))?;
        Ok(PciRegion {
            index,
            size: info.size,
            mmap_offset: info.offset,
            flags: info.flags,
        })
    }

    /// Memory-map a region into the process address space (shared,
    /// read/write).
    ///
    /// # Errors
    ///
    /// Returns an error if the region is not mmapable or `mmap` fails.
    pub fn mmap_region(&self, region: &PciRegion) -> Result<MmapRegion> {
        if !region.mmapable() {
            bail!("VFIO region {} is not mmapable", region.index);
        }
        let len = usize::try_from(region.size).context("region size too large")?;
        let offset = i64::try_from(region.mmap_offset).context("region offset too large")?;
        // SAFETY: offset/len describe the VFIO region on this device fd; the
        // mapping is private to the returned owner, which unmaps on drop.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd.as_raw_fd(),
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error()).context("mmap of VFIO region");
        }
        let ptr = NonNull::new(ptr.cast::<u8>()).context("mmap returned null")?;
        Ok(MmapRegion { ptr, len })
    }

    /// Read 16 bits of PCI config space at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an error if the config region cannot be described or the read
    /// fails.
    pub fn config_read16(&self, offset: u64) -> Result<u16> {
        let cfg = self.region(VFIO_PCI_CONFIG_REGION_INDEX)?;
        let file_offset =
            i64::try_from(cfg.mmap_offset + offset).context("config offset too large")?;
        let mut buf = [0u8; 2];
        // SAFETY: buf is a valid 2-byte out-pointer; offset is within config space.
        let n = unsafe {
            libc::pread(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                file_offset,
            )
        };
        if n != 2 {
            bail!("short pread of PCI config space at offset {offset:#x}");
        }
        Ok(u16::from_le_bytes(buf))
    }

    /// Write 16 bits of PCI config space at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an error if the config region cannot be described or the write
    /// fails.
    pub fn config_write16(&self, offset: u64, value: u16) -> Result<()> {
        let cfg = self.region(VFIO_PCI_CONFIG_REGION_INDEX)?;
        let file_offset =
            i64::try_from(cfg.mmap_offset + offset).context("config offset too large")?;
        let buf = value.to_le_bytes();
        // SAFETY: buf is a valid 2-byte in-pointer; offset is within config space.
        let n = unsafe {
            libc::pwrite(
                self.fd.as_raw_fd(),
                buf.as_ptr().cast::<libc::c_void>(),
                buf.len(),
                file_offset,
            )
        };
        if n != 2 {
            bail!("short pwrite of PCI config space at offset {offset:#x}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MMIO access
// ---------------------------------------------------------------------------

/// A memory-mapped device BAR region.
pub struct MmapRegion {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is exclusively owned; volatile access from multiple
// threads is how MMIO works.
unsafe impl Send for MmapRegion {}
unsafe impl Sync for MmapRegion {}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // SAFETY: the region was created by `mmap_region` and not yet unmapped.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast::<libc::c_void>(), self.len);
        }
    }
}

/// Volatile MMIO register access over a mapped BAR.
///
/// All offsets are in bytes from the start of the BAR.
pub trait Mmio {
    /// Read a 32-bit register.
    ///
    /// # Panics
    ///
    /// Panics if `offset + 4` is outside the mapped region.
    fn read32(&self, offset: u64) -> u32;
    /// Write a 32-bit register.
    ///
    /// # Panics
    ///
    /// Panics if `offset + 4` is outside the mapped region.
    fn write32(&self, offset: u64, value: u32);
    /// Read a 64-bit register.
    ///
    /// # Panics
    ///
    /// Panics if `offset + 8` is outside the mapped region.
    fn read64(&self, offset: u64) -> u64;
    /// Write a 64-bit register.
    ///
    /// # Panics
    ///
    /// Panics if `offset + 8` is outside the mapped region.
    fn write64(&self, offset: u64, value: u64);
}

impl MmapRegion {
    fn check(&self, offset: u64, width: u64) {
        assert!(
            offset + width <= self.len as u64,
            "MMIO access out of bounds: offset {offset:#x} + {width} > {:#x}",
            self.len
        );
    }
}

impl Mmio for MmapRegion {
    fn read32(&self, offset: u64) -> u32 {
        self.check(offset, 4);
        // SAFETY: bounds-checked above; the pointer is 4-byte aligned because
        // the BAR mapping is page-aligned and register offsets are.
        unsafe {
            self.ptr
                .cast::<u32>()
                .as_ptr()
                .add((offset / 4) as usize)
                .read_volatile()
        }
    }

    fn write32(&self, offset: u64, value: u32) {
        self.check(offset, 4);
        // SAFETY: as for `read32`.
        unsafe {
            self.ptr
                .cast::<u32>()
                .as_ptr()
                .add((offset / 4) as usize)
                .write_volatile(value);
        }
    }

    fn read64(&self, offset: u64) -> u64 {
        self.check(offset, 8);
        // SAFETY: bounds-checked above; page-aligned base + 8-byte-aligned offset.
        unsafe {
            self.ptr
                .cast::<u64>()
                .as_ptr()
                .add((offset / 8) as usize)
                .read_volatile()
        }
    }

    fn write64(&self, offset: u64, value: u64) {
        self.check(offset, 8);
        // SAFETY: as for `read64`.
        unsafe {
            self.ptr
                .cast::<u64>()
                .as_ptr()
                .add((offset / 8) as usize)
                .write_volatile(value);
        }
    }
}

// ---------------------------------------------------------------------------
// IOMMU gating via sysfs
// ---------------------------------------------------------------------------

/// Validate a PCI bus-device-function string: `dddd:bb:dd.f` (all hex).
///
/// # Errors
///
/// Returns an error if the string is not a canonical PCI BDF.
pub fn validate_bdf(bdf: &str) -> Result<()> {
    fn hex2(s: &[u8]) -> bool {
        s.iter().all(u8::is_ascii_hexdigit)
    }
    let b = bdf.as_bytes();
    let ok = b.len() == 12
        && hex2(&b[0..4])
        && b[4] == b':'
        && hex2(&b[5..7])
        && b[7] == b':'
        && hex2(&b[8..10])
        && b[10] == b'.'
        && hex2(&b[11..12]);
    if ok {
        Ok(())
    } else {
        bail!("invalid PCI BDF {bdf:?}: expected dddd:bb:dd.f, e.g. 0000:01:00.0");
    }
}

/// Count the IOMMU groups visible under a sysfs root (normally `/sys`).
fn iommu_group_count(sys: &Path) -> usize {
    let dir = sys.join("kernel/iommu_groups");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .count()
}

/// Whether the host IOMMU is available for VFIO passthrough.
///
/// True when `/sys/kernel/iommu_groups` exists and is non-empty — i.e. the
/// kernel booted with `intel_iommu=on` / `amd_iommu=on` (or equivalent) and
/// the IOMMU driver enumerated groups.
#[must_use]
pub fn iommu_available() -> bool {
    iommu_group_count(Path::new("/sys")) > 0
}

/// Resolve the IOMMU group id of a PCI device through sysfs.
///
/// # Errors
///
/// Returns an error if the device has no `iommu_group` symlink (IOMMU
/// disabled for it) or the group id cannot be parsed.
pub fn device_iommu_group(bdf: &str) -> Result<u32> {
    device_iommu_group_under(Path::new("/sys"), bdf)
}

fn device_iommu_group_under(sys: &Path, bdf: &str) -> Result<u32> {
    let link = sys.join(format!("bus/pci/devices/{bdf}/iommu_group"));
    let target =
        std::fs::read_link(&link).with_context(|| format!("reading {}", link.display()))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("bad iommu_group symlink target for {bdf}"))?;
    name.parse::<u32>()
        .with_context(|| format!("unparsable IOMMU group id {name:?} for {bdf}"))
}

/// Read a PCI device's class code through sysfs.
///
/// # Errors
///
/// Returns an error if the class file cannot be read or parsed.
pub fn device_class(bdf: &str) -> Result<u32> {
    device_class_under(Path::new("/sys"), bdf)
}

fn device_class_under(sys: &Path, bdf: &str) -> Result<u32> {
    let path = sys.join(format!("bus/pci/devices/{bdf}/class"));
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    u32::from_str_radix(text.trim().trim_start_matches("0x"), 16)
        .with_context(|| format!("unparsable PCI class {text:?} for {bdf}"))
}

/// Name of the driver bound to a PCI device, or `None` if unbound.
///
/// # Errors
///
/// Returns an error if the driver symlink exists but cannot be read.
pub fn device_driver(bdf: &str) -> Result<Option<String>> {
    device_driver_under(Path::new("/sys"), bdf)
}

fn device_driver_under(sys: &Path, bdf: &str) -> Result<Option<String>> {
    let link = sys.join(format!("bus/pci/devices/{bdf}/driver"));
    match std::fs::read_link(&link) {
        Ok(target) => Ok(target
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", link.display())),
    }
}

/// An `NVMe` controller visible on the PCI bus and its passthrough readiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeCandidate {
    /// PCI address (`dddd:bb:dd.f`).
    pub bdf: String,
    /// IOMMU group id, if the device is in one.
    pub iommu_group: Option<u32>,
    /// Bound driver name, if any.
    pub driver: Option<String>,
    /// Whether the device can be passed through right now: IOMMU group
    /// present and bound to `vfio-pci`.
    pub passthrough_ready: bool,
}

/// Enumerate `NVMe` controllers on the PCI bus with their passthrough
/// readiness. Sorted by BDF.
#[must_use]
pub fn discover_nvme_controllers() -> Vec<NvmeCandidate> {
    discover_under(Path::new("/sys"))
}

fn discover_under(sys: &Path) -> Vec<NvmeCandidate> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(sys.join("bus/pci/devices")) else {
        return out;
    };
    for entry in rd.filter_map(Result::ok) {
        let Some(bdf) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if device_class_under(sys, &bdf).is_ok_and(|c| c == PCI_CLASS_NVME) {
            let iommu_group = device_iommu_group_under(sys, &bdf).ok();
            let driver = device_driver_under(sys, &bdf).ok().flatten();
            out.push(NvmeCandidate {
                passthrough_ready: iommu_group.is_some() && driver.as_deref() == Some("vfio-pci"),
                bdf,
                iommu_group,
                driver,
            });
        }
    }
    out.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_linux_vfio_h() {
        // Spot-check the const-fn ioctl encoding against the well-known
        // linux/vfio.h request numbers.
        assert_eq!(VFIO_GET_API_VERSION, 0x3b64);
        assert_eq!(VFIO_CHECK_EXTENSION, 0x3b65);
        assert_eq!(VFIO_SET_IOMMU, 0x3b66);
        assert_eq!(VFIO_GROUP_GET_STATUS, 0x8008_3b67);
        assert_eq!(VFIO_DEVICE_GET_REGION_INFO, 0xc020_3b6c);
        assert_eq!(VFIO_IOMMU_MAP_DMA, 0x4020_3b71);
        assert_eq!(VFIO_IOMMU_UNMAP_DMA, 0x4018_3b72);
        // GET_DEVICE_FD encodes the name length: "0000:01:00.0\0" is 13 bytes.
        assert_eq!(vfio_group_get_device_fd(13), 0x400d_3b6a);
    }

    #[test]
    fn vfio_struct_layouts_match_the_abi() {
        assert_eq!(std::mem::size_of::<VfioGroupStatus>(), 8);
        assert_eq!(std::mem::size_of::<VfioRegionInfo>(), 32);
        assert_eq!(std::mem::size_of::<VfioDmaMap>(), 32);
        assert_eq!(std::mem::size_of::<VfioDmaUnmap>(), 24);
    }

    #[test]
    fn validate_bdf_accepts_canonical_forms() {
        assert!(validate_bdf("0000:01:00.0").is_ok());
        assert!(validate_bdf("0000:ff:1f.7").is_ok());
        assert!(validate_bdf("abcd:00:1a.3").is_ok());
    }

    #[test]
    fn validate_bdf_rejects_malformed() {
        for bad in [
            "",
            "01:00.0",
            "0000:01:00",
            "0000:1:00.0",
            "0000:01:00.00",
            "0000:01:00.0 ",
            "gggg:01:00.0",
            "0000-01-00.0",
        ] {
            assert!(validate_bdf(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    /// Build a fake sysfs tree: `<root>/bus/pci/devices/<bdf>/{class,driver,iommu_group?}`.
    fn fake_sysfs() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (bdf, class, driver, group) in [
            ("0000:01:00.0", "0x010802\n", Some("vfio-pci"), Some("12")),
            ("0000:02:00.0", "0x010802\n", Some("nvme"), Some("13")),
            ("0000:03:00.0", "0x030000\n", Some("nvidia"), Some("14")),
            ("0000:04:00.0", "0x010802\n", None, None),
        ] {
            let dev = root.path().join(format!("bus/pci/devices/{bdf}"));
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::write(dev.join("class"), class).unwrap();
            if let Some(driver) = driver {
                std::os::unix::fs::symlink(
                    format!("../../../../bus/pci/drivers/{driver}"),
                    dev.join("driver"),
                )
                .unwrap();
            }
            if let Some(group) = group {
                let groups = root.path().join("kernel/iommu_groups");
                std::fs::create_dir_all(groups.join(group)).unwrap();
                std::os::unix::fs::symlink(
                    format!("../../../kernel/iommu_groups/{group}"),
                    dev.join("iommu_group"),
                )
                .unwrap();
            }
        }
        root
    }

    #[test]
    fn iommu_gating_counts_groups() {
        let root = fake_sysfs();
        assert!(iommu_group_count(root.path()) > 0);

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(iommu_group_count(empty.path()), 0);
        std::fs::create_dir_all(empty.path().join("kernel/iommu_groups")).unwrap();
        assert_eq!(iommu_group_count(empty.path()), 0);
        std::fs::create_dir_all(empty.path().join("kernel/iommu_groups/0")).unwrap();
        assert_eq!(iommu_group_count(empty.path()), 1);
    }

    #[test]
    fn device_iommu_group_parses_symlink() {
        let root = fake_sysfs();
        assert_eq!(
            device_iommu_group_under(root.path(), "0000:01:00.0").unwrap(),
            12
        );
        assert!(device_iommu_group_under(root.path(), "0000:04:00.0").is_err());
    }

    #[test]
    fn device_class_and_driver_helpers() {
        let root = fake_sysfs();
        assert_eq!(
            device_class_under(root.path(), "0000:01:00.0").unwrap(),
            PCI_CLASS_NVME
        );
        assert_eq!(
            device_driver_under(root.path(), "0000:01:00.0")
                .unwrap()
                .as_deref(),
            Some("vfio-pci")
        );
        assert_eq!(
            device_driver_under(root.path(), "0000:04:00.0").unwrap(),
            None
        );
    }

    #[test]
    fn discover_finds_only_nvme_with_readiness() {
        let root = fake_sysfs();
        let found = discover_under(root.path());
        let bdfs: Vec<&str> = found.iter().map(|c| c.bdf.as_str()).collect();
        assert_eq!(bdfs, ["0000:01:00.0", "0000:02:00.0", "0000:04:00.0"]);

        let ready: Vec<&str> = found
            .iter()
            .filter(|c| c.passthrough_ready)
            .map(|c| c.bdf.as_str())
            .collect();
        // Only the vfio-pci-bound device in an IOMMU group is ready.
        assert_eq!(ready, ["0000:01:00.0"]);
        assert_eq!(found[0].iommu_group, Some(12));
        assert_eq!(found[1].driver.as_deref(), Some("nvme"));
        assert_eq!(found[2].iommu_group, None);
    }
}
