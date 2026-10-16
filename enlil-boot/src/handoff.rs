//! Firmware → kernel handoff block.
//!
//! Before calling `ExitBootServices()` the UEFI stage collects the platform
//! resources the enlil kernel needs to bring the machine up on its own:
//! the UEFI memory map, the ACPI RSDP, the GOP framebuffer, and the PCI root
//! bridges inventoried from the firmware's PCI protocol handles. Those are
//! packed into a [`BootHandoff`] and passed to the kernel entry.
//!
//! The types here are `no_std`-clean and host-agnostic (no `alloc`, no
//! firmware calls) so the collection/validation logic is unit-tested on the
//! dev toolchain, and the real UEFI collection code (Phase 6.1) fills them
//! in with values read from firmware protocols.

/// The pixel layout of a GOP framebuffer, mirroring UEFI's `PixelFormat`.
///
/// Kept as a plain enum here (rather than re-exporting the uefi-rs type) so
/// the bytes-per-pixel logic is host-testable without a UEFI toolchain; the
/// firmware code maps the real `uefi::proto::console::gop::PixelFormat` onto
/// this before building a [`Framebuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit, 8:8:8 RGB with a reserved 4th byte.
    Rgb,
    /// 32-bit, 8:8:8 BGR with a reserved 4th byte.
    Bgr,
    /// Custom channel layout described by a bitmask (still 32-bit/pixel).
    Bitmask,
    /// No linear framebuffer — the mode only supports blt operations.
    BltOnly,
}

impl PixelFormat {
    /// Bytes per pixel for a directly-addressable framebuffer, or `None` for
    /// a blt-only mode that exposes no linear framebuffer.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> Option<u32> {
        match self {
            Self::Rgb | Self::Bgr | Self::Bitmask => Some(4),
            Self::BltOnly => None,
        }
    }
}

/// A linear framebuffer as reported by the UEFI Graphics Output Protocol.
///
/// `stride` is the number of pixels per scanline (which may exceed `width`
/// when the mode is padded), so the byte offset of a pixel is
/// `(y * stride + x) * bytes_per_pixel`. `pixel_format` records the GOP
/// channel order so the kernel's console writes color correctly (RGB vs BGR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framebuffer {
    /// Physical base address of the framebuffer.
    pub base: u64,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Pixels per scanline (>= `width`).
    pub stride: u32,
    /// Bytes per pixel.
    pub bytes_per_pixel: u32,
    /// The GOP pixel format (channel order) of this framebuffer.
    pub pixel_format: PixelFormat,
}

impl Framebuffer {
    /// Build a framebuffer from the GOP mode parameters, or `None` if the
    /// mode is blt-only (no linear framebuffer to hand the kernel).
    #[must_use]
    pub const fn from_gop(
        base: u64,
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
    ) -> Option<Self> {
        match format.bytes_per_pixel() {
            Some(bytes_per_pixel) => Some(Self {
                base,
                width,
                height,
                stride,
                bytes_per_pixel,
                pixel_format: format,
            }),
            None => None,
        }
    }

    /// Total size of the framebuffer in bytes (`stride * height * bpp`).
    #[must_use]
    pub const fn size_bytes(&self) -> u64 {
        self.stride as u64 * self.height as u64 * self.bytes_per_pixel as u64
    }

    /// Byte offset of the pixel at `(x, y)`, or `None` if it is outside the
    /// visible area.
    #[must_use]
    pub const fn pixel_offset(&self, x: u32, y: u32) -> Option<u64> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some((y as u64 * self.stride as u64 + x as u64) * self.bytes_per_pixel as u64)
    }
}

/// Maximum guests whose ESP firmware the boot app stages into the handoff.
pub const MAX_BOOT_GUESTS: usize = 16;

/// One guest's OVMF images as staged from the ESP into boot-services memory.
///
/// Both extents live in `LOADER_DATA` pages allocated before
/// `ExitBootServices()`; the kernel must treat them as reserved, never as
/// free conventional memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestFirmware {
    /// Guest id as NUL-terminated ASCII (`firmware::MAX_GUEST_ID_LEN` + NUL).
    pub id: [u8; 33],
    /// Physical base of the staged `OVMF_CODE.fd` (`0` when not staged).
    pub code_base: u64,
    /// Valid bytes at `code_base`.
    pub code_len: usize,
    /// Physical base of the staged `OVMF_VARS.fd` template (`0` when not
    /// staged).
    pub vars_base: u64,
    /// Valid bytes at `vars_base`.
    pub vars_len: usize,
    /// The guest's virtual-disk assignment (`disk.txt`), if the ESP has one.
    pub disk: StagedFile,
}

impl GuestFirmware {
    /// An empty (unstaged) entry.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            id: [0; 33],
            code_base: 0,
            code_len: 0,
            vars_base: 0,
            vars_len: 0,
            disk: StagedFile::EMPTY,
        }
    }

    /// Whether both firmware images were staged.
    #[must_use]
    pub const fn has_firmware(&self) -> bool {
        self.code_base != 0 && self.code_len != 0 && self.vars_base != 0 && self.vars_len != 0
    }

    /// The guest id as a string (bytes up to the first NUL).
    #[must_use]
    pub fn id_str(&self) -> &str {
        let len = self
            .id
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.id.len());
        core::str::from_utf8(&self.id[..len]).unwrap_or("?")
    }
}

/// A file staged from the ESP into boot-services (`LOADER_DATA`) memory.
///
/// The pages were allocated before `ExitBootServices()` and must be treated as
/// reserved by the kernel, never as free conventional memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StagedFile {
    /// Physical base of the staged bytes (`0` when not staged).
    pub base: u64,
    /// Valid bytes at `base`.
    pub len: usize,
}

impl StagedFile {
    /// An empty (unstaged) file.
    pub const EMPTY: Self = Self { base: 0, len: 0 };

    /// Whether any bytes were staged.
    #[must_use]
    pub const fn is_staged(&self) -> bool {
        self.base != 0 && self.len != 0
    }
}

/// A Linux guest staged from the ESP: its kernel image, optional initrd and
/// optional command line (the files under `EFI/enlil/linux/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinuxPayload {
    /// The `bzImage` (`vmlinuz`).
    pub kernel: StagedFile,
    /// The initial ramdisk (`initrd.img`), if any.
    pub initrd: StagedFile,
    /// The kernel command line (`cmdline.txt`, no trailing NUL), if any.
    pub cmdline: StagedFile,
    /// The virtual-disk assignment (`disk.txt`: which host disk, which sector
    /// ranges), if any. Without it the guest gets no disk.
    pub disk: StagedFile,
}

impl LinuxPayload {
    /// No Linux guest staged.
    pub const EMPTY: Self = Self {
        kernel: StagedFile::EMPTY,
        initrd: StagedFile::EMPTY,
        cmdline: StagedFile::EMPTY,
        disk: StagedFile::EMPTY,
    };

    /// Whether a bootable kernel image was staged.
    #[must_use]
    pub const fn has_kernel(&self) -> bool {
        self.kernel.is_staged()
    }
}

/// Maximum PCI root bridges recorded in a [`BootHandoff`].
///
/// One bridge per firmware-enumerated PCI segment is the norm (usually one);
/// the bound keeps the handoff `Copy` and alloc-free. Bridges past the bound
/// are still logged by the firmware collector, just not recorded.
pub const MAX_PCI_ROOT_BRIDGES: usize = 8;

/// One PCI root bridge as inventoried from the firmware's PCI Root Bridge
/// I/O protocol handles, collected before `ExitBootServices()`.
///
/// After exit the protocol is gone, so this is the durable record the
/// kernel's boot device tree (roadmap 6.3) consumes: which segment each
/// firmware-enumerated root bridge lives on and which bus range it decodes.
/// The range is the firmware's own report (`Configuration()`); firmware
/// that publishes no bus aperture yields a degenerate `0-0`, so consumers
/// treat it as advisory next to the MCFG/ECAM discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PciRootBridge {
    /// PCI segment group number (0 on single-segment machines).
    pub segment: u16,
    /// First bus number this root bridge decodes.
    pub bus_start: u8,
    /// Last bus number this root bridge decodes (inclusive).
    pub bus_end: u8,
    /// Firmware-reported attributes (`EFI_PCI_ATTRIBUTE_*`), truncated to 32
    /// bits — every attribute the spec defines fits there. Advisory: the
    /// kernel re-derives what it needs from config space.
    pub attributes: u32,
}

impl PciRootBridge {
    /// Whether this bridge decodes `bus`.
    #[must_use]
    pub const fn covers_bus(self, bus: u8) -> bool {
        bus >= self.bus_start && bus <= self.bus_end
    }
}

/// Everything the UEFI stage collects before `ExitBootServices()` and hands
/// to the enlil kernel.
///
/// The memory map is described by the firmware's own descriptor layout so the
/// kernel can re-parse it with [`enlil-platform`](../../enlil_platform)'s
/// `MemoryMap::from_uefi` without the UEFI stage needing to allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootHandoff {
    /// Physical base of the UEFI memory descriptor array.
    pub memory_map_base: u64,
    /// Total length of the memory descriptor array in bytes.
    pub memory_map_len: usize,
    /// Size of one memory descriptor in bytes (firmware-reported; may exceed
    /// `size_of::<UefiMemoryDescriptor>()` on future firmware).
    pub memory_descriptor_size: usize,
    /// Physical address of the ACPI RSDP, or `0` if the firmware exposed none.
    pub acpi_rsdp: u64,
    /// The GOP framebuffer, if the firmware provided a graphics console.
    pub framebuffer: Option<Framebuffer>,
    /// Physical base of the staged `EFI/enlil/config.toml` (`0` when the ESP
    /// had none); `LOADER_DATA` pages, valid for `config_len` bytes.
    pub config_base: u64,
    /// Valid bytes at `config_base`.
    pub config_len: usize,
    /// Per-guest OVMF extents staged from the ESP; only the first
    /// `guest_firmware_count` entries are valid.
    pub guest_firmwares: [GuestFirmware; MAX_BOOT_GUESTS],
    /// Number of valid entries in `guest_firmwares` (`<= MAX_BOOT_GUESTS`).
    pub guest_firmware_count: usize,
    /// PCI root bridges inventoried from the firmware's PCI Root Bridge I/O
    /// protocol handles; only the first `pci_root_bridge_count` entries are
    /// valid. Advisory: a PCI-less boot is still a valid handoff.
    pub pci_root_bridges: [PciRootBridge; MAX_PCI_ROOT_BRIDGES],
    /// Number of valid entries in `pci_root_bridges`.
    pub pci_root_bridge_count: u8,
    /// A Linux guest staged from `EFI/enlil/linux/` (empty when the ESP has
    /// none): the kernel runs it as a real guest OS when a kernel is present.
    pub linux_guest: LinuxPayload,
    /// The real machine's SMBIOS structure table, copied into memory the kernel
    /// keeps (empty when the firmware published none): firmware guests see the
    /// real board's identity, not an invented one.
    pub smbios_tables: StagedFile,
    /// The matching SMBIOS entry point, rebuilt to describe the copy.
    pub smbios_anchor: StagedFile,
}

impl BootHandoff {
    /// Number of memory descriptors in the map (`0` if the descriptor size
    /// was not reported).
    #[must_use]
    pub const fn descriptor_count(&self) -> usize {
        match self.memory_map_len.checked_div(self.memory_descriptor_size) {
            Some(n) => n,
            None => 0,
        }
    }

    /// Whether the firmware handed us an ACPI RSDP.
    #[must_use]
    pub const fn has_acpi(&self) -> bool {
        self.acpi_rsdp != 0
    }

    /// Whether the boot app staged a config from the ESP.
    #[must_use]
    pub const fn has_config(&self) -> bool {
        self.config_base != 0 && self.config_len != 0
    }

    /// The guest the config asks to boot from its own firmware, if one with
    /// both OVMF images staged says `boot = "firmware"` in its `[guest.<id>]`
    /// table. Staged firmware alone never starts a guest: the proof runs stage
    /// OVMF too, and must stay proofs.
    #[must_use]
    pub fn firmware_boot_guest(&self) -> Option<GuestFirmware> {
        let config = self.config_bytes()?;
        self.guest_firmwares
            .iter()
            .take(self.guest_firmware_count.min(MAX_BOOT_GUESTS))
            .find(|g| g.has_firmware() && crate::esp::guest_boots_firmware(config, g.id_str()))
            .copied()
    }

    /// `run-seconds` from guest `id`'s `[guest.<id>]` table, if the config sets it
    /// (`Some(0)` is "no limit").
    #[must_use]
    pub fn guest_run_seconds(&self, id: &str) -> Option<u64> {
        crate::esp::guest_run_seconds(self.config_bytes()?, id)
    }

    /// The staged `config.toml`, if there is one.
    fn config_bytes(&self) -> Option<&[u8]> {
        if !self.has_config() {
            return None;
        }
        let base = usize::try_from(self.config_base).ok()?;
        // SAFETY: the UEFI stage staged `config_len` bytes of `LOADER_DATA` at
        // `config_base` (never freed); the kernel runs identity-mapped.
        Some(unsafe {
            core::slice::from_raw_parts(
                core::ptr::with_exposed_provenance::<u8>(base),
                self.config_len,
            )
        })
    }

    /// Whether a usable graphics framebuffer was handed over.
    #[must_use]
    pub const fn has_framebuffer(&self) -> bool {
        self.framebuffer.is_some()
    }

    /// The PCI root bridges the firmware reported (empty when none).
    ///
    /// The count is clamped to [`MAX_PCI_ROOT_BRIDGES`] so a hand-built
    /// handoff with a wild count can never read past the array.
    #[must_use]
    pub fn pci_root_bridges(&self) -> &[PciRootBridge] {
        let n = (self.pci_root_bridge_count as usize).min(MAX_PCI_ROOT_BRIDGES);
        &self.pci_root_bridges[..n]
    }

    /// Whether the firmware handed us any PCI root bridge inventory.
    #[must_use]
    pub const fn has_pci_root_bridges(&self) -> bool {
        self.pci_root_bridge_count != 0
    }

    /// The recorded root bridge for `segment`, if the firmware reported one.
    #[must_use]
    pub fn root_bridge_for_segment(&self, segment: u16) -> Option<&PciRootBridge> {
        self.pci_root_bridges()
            .iter()
            .find(|bridge| bridge.segment == segment)
    }

    /// Whether this handoff carries the minimum the kernel needs to boot: a
    /// non-empty memory map with a sane descriptor size and an ACPI RSDP.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.memory_map_base != 0
            && self.memory_descriptor_size != 0
            && self.memory_map_len >= self.memory_descriptor_size
            && self.acpi_rsdp != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fb() -> Framebuffer {
        Framebuffer {
            base: 0x8000_0000,
            width: 1920,
            height: 1080,
            stride: 2048,
            bytes_per_pixel: 4,
            pixel_format: PixelFormat::Bgr,
        }
    }

    #[test]
    fn pixel_format_bytes_per_pixel() {
        assert_eq!(PixelFormat::Rgb.bytes_per_pixel(), Some(4));
        assert_eq!(PixelFormat::Bgr.bytes_per_pixel(), Some(4));
        assert_eq!(PixelFormat::Bitmask.bytes_per_pixel(), Some(4));
        // A blt-only mode has no linear framebuffer to describe.
        assert_eq!(PixelFormat::BltOnly.bytes_per_pixel(), None);
    }

    #[test]
    fn from_gop_builds_addressable_modes_and_rejects_blt_only() {
        let fb = Framebuffer::from_gop(0x8000_0000, 1280, 720, 1280, PixelFormat::Bgr)
            .expect("BGR is addressable");
        assert_eq!(fb.bytes_per_pixel, 4);
        assert_eq!(fb.pixel_format, PixelFormat::Bgr);
        assert_eq!(fb.size_bytes(), 1280 * 720 * 4);
        // Blt-only yields no framebuffer.
        assert_eq!(
            Framebuffer::from_gop(0, 1280, 720, 1280, PixelFormat::BltOnly),
            None
        );
    }

    #[test]
    fn framebuffer_size_uses_stride_not_width() {
        // Padded mode: stride (2048) > width (1920), so size must be driven
        // by the stride to cover the whole scanline padding.
        assert_eq!(sample_fb().size_bytes(), 2048 * 1080 * 4);
    }

    #[test]
    fn pixel_offset_honors_stride_and_bounds() {
        let fb = sample_fb();
        assert_eq!(fb.pixel_offset(0, 0), Some(0));
        assert_eq!(fb.pixel_offset(1, 0), Some(4));
        // Row 1 starts one full stride in, not one width in.
        assert_eq!(fb.pixel_offset(0, 1), Some(2048 * 4));
        // Out of the visible area.
        assert_eq!(fb.pixel_offset(1920, 0), None);
        assert_eq!(fb.pixel_offset(0, 1080), None);
    }

    #[test]
    fn descriptor_count_divides_len_by_size() {
        let h = BootHandoff {
            memory_map_base: 0x1000,
            memory_map_len: 48 * 10,
            memory_descriptor_size: 48,
            acpi_rsdp: 0xE_0000,
            framebuffer: None,
            config_base: 0,
            config_len: 0,
            guest_firmwares: [GuestFirmware::empty(); MAX_BOOT_GUESTS],
            guest_firmware_count: 0,
            pci_root_bridges: [PciRootBridge::default(); MAX_PCI_ROOT_BRIDGES],
            pci_root_bridge_count: 0,
            linux_guest: LinuxPayload::EMPTY,
            smbios_tables: StagedFile::EMPTY,
            smbios_anchor: StagedFile::EMPTY,
        };
        assert_eq!(h.descriptor_count(), 10);
    }

    #[test]
    fn descriptor_count_is_zero_when_size_zero() {
        let h = BootHandoff {
            memory_map_base: 0x1000,
            memory_map_len: 480,
            memory_descriptor_size: 0,
            acpi_rsdp: 0xE_0000,
            framebuffer: None,
            config_base: 0,
            config_len: 0,
            guest_firmwares: [GuestFirmware::empty(); MAX_BOOT_GUESTS],
            guest_firmware_count: 0,
            pci_root_bridges: [PciRootBridge::default(); MAX_PCI_ROOT_BRIDGES],
            pci_root_bridge_count: 0,
            linux_guest: LinuxPayload::EMPTY,
            smbios_tables: StagedFile::EMPTY,
            smbios_anchor: StagedFile::EMPTY,
        };
        assert_eq!(h.descriptor_count(), 0);
    }

    #[test]
    fn is_valid_requires_map_and_rsdp() {
        let good = BootHandoff {
            memory_map_base: 0x1000,
            memory_map_len: 48,
            memory_descriptor_size: 48,
            acpi_rsdp: 0xE_0000,
            framebuffer: Some(sample_fb()),
            config_base: 0x9_0000,
            config_len: 128,
            guest_firmwares: [GuestFirmware::empty(); MAX_BOOT_GUESTS],
            guest_firmware_count: 0,
            pci_root_bridges: [PciRootBridge::default(); MAX_PCI_ROOT_BRIDGES],
            pci_root_bridge_count: 0,
            linux_guest: LinuxPayload::EMPTY,
            smbios_tables: StagedFile::EMPTY,
            smbios_anchor: StagedFile::EMPTY,
        };
        assert!(good.is_valid());
        assert!(good.has_acpi());
        assert!(good.has_framebuffer());

        // Missing RSDP.
        assert!(
            !BootHandoff {
                acpi_rsdp: 0,
                ..good
            }
            .is_valid()
        );
        // Zero descriptor size.
        assert!(
            !BootHandoff {
                memory_descriptor_size: 0,
                ..good
            }
            .is_valid()
        );
        // Map shorter than one descriptor.
        assert!(
            !BootHandoff {
                memory_map_len: 0,
                ..good
            }
            .is_valid()
        );
        // Null map base.
        assert!(
            !BootHandoff {
                memory_map_base: 0,
                ..good
            }
            .is_valid()
        );
    }

    fn empty_handoff() -> BootHandoff {
        BootHandoff {
            memory_map_base: 0x1000,
            memory_map_len: 48,
            memory_descriptor_size: 48,
            acpi_rsdp: 0xE_0000,
            framebuffer: None,
            config_base: 0,
            config_len: 0,
            guest_firmwares: [GuestFirmware::empty(); MAX_BOOT_GUESTS],
            guest_firmware_count: 0,
            pci_root_bridges: [PciRootBridge::default(); MAX_PCI_ROOT_BRIDGES],
            pci_root_bridge_count: 0,
            linux_guest: LinuxPayload::EMPTY,
            smbios_tables: StagedFile::EMPTY,
            smbios_anchor: StagedFile::EMPTY,
        }
    }

    #[test]
    fn a_guests_run_limit_comes_from_the_staged_config() {
        let config = b"[guest.win]\nboot = \"firmware\"\nrun-seconds = 1800\n[guest.x]\ncpus = 1\n";
        let mut h = empty_handoff();
        h.config_base = config.as_ptr() as u64;
        h.config_len = config.len();
        assert_eq!(h.guest_run_seconds("win"), Some(1800));
        assert_eq!(h.guest_run_seconds("x"), None);
        h.config_base = 0;
        assert_eq!(h.guest_run_seconds("win"), None, "no config");
    }

    #[test]
    fn firmware_boot_guest_needs_config_opt_in_and_both_images() {
        let config = b"[guest.proof]\ncpus = 1\n[guest.win]\nboot = \"firmware\"\n";
        let mut h = empty_handoff();
        h.config_base = config.as_ptr() as u64;
        h.config_len = config.len();
        let staged = |id: &[u8], both: bool| {
            let mut g = GuestFirmware::empty();
            g.id[..id.len()].copy_from_slice(id);
            g.code_base = 0x10_0000;
            g.code_len = 4096;
            if both {
                g.vars_base = 0x20_0000;
                g.vars_len = 4096;
            }
            g
        };
        h.guest_firmwares[0] = staged(b"proof", true);
        h.guest_firmwares[1] = staged(b"win", false);
        h.guest_firmware_count = 2;
        // `proof` has both images but no opt-in; `win` opts in but lacks VARS.
        assert_eq!(h.firmware_boot_guest(), None);
        h.guest_firmwares[1] = staged(b"win", true);
        assert_eq!(h.firmware_boot_guest().unwrap().id_str(), "win");
        // No config: nothing boots.
        h.config_base = 0;
        assert_eq!(h.firmware_boot_guest(), None);
    }

    fn handoff_with_bridges() -> BootHandoff {
        let mut bridges = [PciRootBridge::default(); MAX_PCI_ROOT_BRIDGES];
        bridges[0] = PciRootBridge {
            segment: 0,
            bus_start: 0,
            bus_end: 0xFF,
            attributes: 0x1,
        };
        bridges[1] = PciRootBridge {
            segment: 1,
            bus_start: 0,
            bus_end: 0x0F,
            attributes: 0x1,
        };
        BootHandoff {
            memory_map_base: 0x1000,
            memory_map_len: 48,
            memory_descriptor_size: 48,
            acpi_rsdp: 0xE_0000,
            framebuffer: None,
            config_base: 0,
            config_len: 0,
            guest_firmwares: [GuestFirmware::empty(); MAX_BOOT_GUESTS],
            guest_firmware_count: 0,
            pci_root_bridges: bridges,
            pci_root_bridge_count: 2,
            linux_guest: LinuxPayload::EMPTY,
            smbios_tables: StagedFile::EMPTY,
            smbios_anchor: StagedFile::EMPTY,
        }
    }

    #[test]
    fn guest_firmware_id_str_reads_nul_terminated_id() {
        let mut g = GuestFirmware::empty();
        g.id[..6].copy_from_slice(b"linux1");
        assert_eq!(g.id_str(), "linux1");
        assert_eq!(GuestFirmware::empty().id_str(), "");
    }

    #[test]
    fn has_config_requires_base_and_len() {
        assert!(!empty_handoff().has_config());
        assert!(
            BootHandoff {
                config_base: 0x9_0000,
                config_len: 128,
                ..empty_handoff()
            }
            .has_config()
        );
        assert!(
            !BootHandoff {
                config_base: 0x9_0000,
                ..empty_handoff()
            }
            .has_config()
        );
    }
    #[test]
    fn pci_bridges_slice_honors_count_and_clamps_wild_counts() {
        let h = handoff_with_bridges();
        assert!(h.has_pci_root_bridges());
        let list = h.pci_root_bridges();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].segment, 0);
        assert!(list[0].covers_bus(0x00));
        assert!(list[0].covers_bus(0xFF));
        assert_eq!(list[1].segment, 1);
        assert_eq!(list[1].bus_end, 0x0F);
        assert!(list[1].covers_bus(0x0F));
        assert!(!list[1].covers_bus(0x10));

        // A wild count can never read past the array.
        let wild = BootHandoff {
            pci_root_bridge_count: u8::MAX,
            ..h
        };
        assert_eq!(wild.pci_root_bridges().len(), MAX_PCI_ROOT_BRIDGES);
    }

    #[test]
    fn empty_pci_inventory_is_still_a_valid_handoff() {
        let h = BootHandoff {
            pci_root_bridge_count: 0,
            ..handoff_with_bridges()
        };
        assert!(!h.has_pci_root_bridges());
        assert_eq!(h.pci_root_bridges(), []);
        assert_eq!(h.root_bridge_for_segment(0), None);
        // PCI inventory is advisory: a PCI-less boot still boots.
        assert!(h.is_valid());
    }

    #[test]
    fn root_bridge_for_segment_finds_each_segment() {
        let h = handoff_with_bridges();
        assert_eq!(h.root_bridge_for_segment(0).unwrap().bus_end, 0xFF);
        assert_eq!(h.root_bridge_for_segment(1).unwrap().bus_start, 0);
        assert_eq!(h.root_bridge_for_segment(2), None);
    }

    #[test]
    fn default_bridge_is_zeroed() {
        let b = PciRootBridge::default();
        assert_eq!(
            (b.segment, b.bus_start, b.bus_end, b.attributes),
            (0, 0, 0, 0)
        );
        // Degenerate zero range: covers only bus 0, nothing else.
        assert!(b.covers_bus(0));
        assert!(!b.covers_bus(1));
    }
}
