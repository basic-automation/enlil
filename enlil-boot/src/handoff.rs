//! Firmware → kernel handoff block.
//!
//! Before calling `ExitBootServices()` the UEFI stage collects the platform
//! resources the enlil kernel needs to bring the machine up on its own:
//! the UEFI memory map, the ACPI RSDP, and the GOP framebuffer. Those are
//! packed into a [`BootHandoff`] and passed to the kernel entry.
//!
//! The types here are `no_std`-clean and host-agnostic (no `alloc`, no
//! firmware calls) so the collection/validation logic is unit-tested on the
//! dev toolchain, and the real UEFI collection code (Phase 6.1) fills them
//! in with values read from firmware protocols.

/// A linear framebuffer as reported by the UEFI Graphics Output Protocol.
///
/// `stride` is the number of pixels per scanline (which may exceed `width`
/// when the mode is padded), so the byte offset of a pixel is
/// `(y * stride + x) * bytes_per_pixel`.
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
}

impl Framebuffer {
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

    /// Whether a usable graphics framebuffer was handed over.
    #[must_use]
    pub const fn has_framebuffer(&self) -> bool {
        self.framebuffer.is_some()
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
        }
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
}
