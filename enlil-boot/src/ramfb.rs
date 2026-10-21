//! The `ramfb` display: a framebuffer in guest RAM that the guest firmware
//! describes through `fw_cfg`.
//!
//! OVMF draws its menus and boot logo, and Windows' basic display driver keeps
//! drawing after boot, on a plain block of the guest's own memory. The guest
//! tells the machine where it is by writing a 28-byte description to the
//! `etc/ramfb` file: address, pixel format, size and stride (all big-endian).
//! There is no device to model beyond that: the hypervisor reads the description
//! back, and copies the picture to a real display whenever it wants one.
//!
//! Pure and host-tested.

/// The `fw_cfg` file the guest writes its framebuffer description to.
pub const FILE_NAME: &str = "etc/ramfb";
/// Size of that file.
pub const CONFIG_BYTES: usize = 28;
/// `DRM_FORMAT_XRGB8888`: 32 bits per pixel, blue in the low byte.
pub const FOURCC_XRGB8888: u32 = 0x3432_5258;
/// Bytes per pixel of the only format offered.
const BYTES_PER_PIXEL: usize = 4;
/// A sanity bound on either dimension.
const MAX_DIMENSION: u32 = 8192;

/// A framebuffer the guest has configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Guest-physical address of the first pixel.
    pub address: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes from the start of one row to the next.
    pub stride: u32,
}

impl Config {
    /// Bytes the framebuffer occupies.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.stride as usize * self.height as usize
    }
}

/// Parse the description in `bytes`; `None` until the guest has configured a
/// usable `XRGB8888` framebuffer (an all-zero file means "no display yet").
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<Config> {
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let address = u64::from_be_bytes(bytes.get(0..8)?.try_into().ok()?);
    let (fourcc, width, height, stride) = (be32(8)?, be32(16)?, be32(20)?, be32(24)?);
    let min_stride = u64::from(width) * BYTES_PER_PIXEL as u64;
    (address != 0
        && fourcc == FOURCC_XRGB8888
        && (1..=MAX_DIMENSION).contains(&width)
        && (1..=MAX_DIMENSION).contains(&height)
        && u64::from(stride) >= min_stride)
        .then_some(Config {
            address,
            width,
            height,
            stride,
        })
}

/// Copy the guest's picture to a host framebuffer, top left, clipped to the smaller.
///
/// `src` is laid out as `cfg` says; `dst` is `dst_width` x `dst_height` pixels
/// with `dst_stride` pixels per row. The guest's pixels are blue-low (`B G R x` in memory); a host framebuffer
/// with red in the low byte needs `swap_red_blue`. Does nothing if `src` or
/// `dst` is too short for the geometry it is given.
pub fn blit(
    src: &[u8],
    cfg: &Config,
    dst: &mut [u8],
    dst_width: u32,
    dst_height: u32,
    dst_stride: u32,
    swap_red_blue: bool,
) {
    let (dst_row, dst_stride) = (dst_stride as usize * BYTES_PER_PIXEL, dst_stride as usize);
    if src.len() < cfg.bytes() || dst.len() < dst_stride * dst_height as usize * BYTES_PER_PIXEL {
        return;
    }
    let width = cfg.width.min(dst_width) as usize * BYTES_PER_PIXEL;
    let rows = cfg.height.min(dst_height) as usize;
    for y in 0..rows {
        let from = &src[y * cfg.stride as usize..][..width];
        let to = &mut dst[y * dst_row..][..width];
        if swap_red_blue {
            for (d, s) in to
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(from.as_chunks::<4>().0)
            {
                *d = [s[2], s[1], s[0], s[3]];
            }
        } else {
            to.copy_from_slice(from);
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // small, bounded test values
mod tests {
    use super::*;

    /// What OVMF writes for 800x600: big-endian fields.
    fn description(address: u64, width: u32, height: u32, stride: u32) -> [u8; CONFIG_BYTES] {
        let mut d = [0u8; CONFIG_BYTES];
        d[0..8].copy_from_slice(&address.to_be_bytes());
        d[8..12].copy_from_slice(&FOURCC_XRGB8888.to_be_bytes());
        d[16..20].copy_from_slice(&width.to_be_bytes());
        d[20..24].copy_from_slice(&height.to_be_bytes());
        d[24..28].copy_from_slice(&stride.to_be_bytes());
        d
    }

    #[test]
    fn a_configured_framebuffer_is_parsed() {
        let c = parse(&description(0xC00_0000, 800, 600, 3200)).unwrap();
        assert_eq!(c.address, 0xC00_0000);
        assert_eq!((c.width, c.height, c.stride), (800, 600, 3200));
        assert_eq!(c.bytes(), 3200 * 600);
    }

    #[test]
    fn an_unconfigured_or_unusable_description_is_not_a_display() {
        assert_eq!(parse(&[0; CONFIG_BYTES]), None, "the file starts zeroed");
        assert_eq!(parse(&description(0, 800, 600, 3200)), None, "no address");
        assert_eq!(parse(&description(0x1000, 0, 600, 3200)), None, "no width");
        assert_eq!(parse(&description(0x1000, 800, 0, 3200)), None, "no height");
        assert_eq!(
            parse(&description(0x1000, 800, 600, 3199)),
            None,
            "a stride shorter than a row"
        );
        assert_eq!(parse(&description(0x1000, 99_999, 600, 400_000)), None);
        let mut other = description(0x1000, 800, 600, 3200);
        other[8..12].copy_from_slice(&0x3231_3652u32.to_be_bytes()); // RGB565
        assert_eq!(parse(&other), None, "only XRGB8888 is offered");
        assert_eq!(parse(&[0; 10]), None, "truncated");
    }

    /// A `w` x `h` picture whose pixel (x, y) is `[x, y, 0xAB, 0xFF]`.
    fn picture(w: usize, h: usize, stride: usize) -> Vec<u8> {
        let mut v = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                v[y * stride + x * 4..][..4].copy_from_slice(&[x as u8, y as u8, 0xAB, 0xFF]);
            }
        }
        v
    }

    fn cfg(w: u32, h: u32, stride: u32) -> Config {
        Config {
            address: 0x1000,
            width: w,
            height: h,
            stride,
        }
    }

    #[test]
    fn the_picture_lands_at_the_top_left_of_a_larger_screen() {
        let src = picture(4, 3, 16);
        let mut dst = vec![0x55u8; 8 * 5 * 4];
        blit(&src, &cfg(4, 3, 16), &mut dst, 8, 5, 8, false);
        assert_eq!(&dst[0..4], &[0, 0, 0xAB, 0xFF]);
        assert_eq!(&dst[3 * 4..][..4], &[3, 0, 0xAB, 0xFF]);
        assert_eq!(&dst[(2 * 8 + 1) * 4..][..4], &[1, 2, 0xAB, 0xFF]);
        assert_eq!(
            &dst[4 * 4..][..4],
            &[0x55; 4],
            "right of the picture is untouched"
        );
        assert_eq!(
            &dst[3 * 8 * 4..][..4],
            &[0x55; 4],
            "below the picture is untouched"
        );
    }

    #[test]
    fn a_larger_picture_is_clipped_to_the_screen() {
        let src = picture(6, 6, 24);
        let mut dst = vec![0u8; 3 * 2 * 4];
        blit(&src, &cfg(6, 6, 24), &mut dst, 3, 2, 3, false);
        assert_eq!(
            &dst[(3 + 2) * 4..][..4],
            &[2, 1, 0xAB, 0xFF],
            "last visible pixel"
        );
    }

    #[test]
    fn a_padded_stride_on_either_side_is_honoured() {
        // The guest pads its rows to 32 bytes, the host to 12 pixels.
        let src = picture(4, 2, 32);
        let mut dst = vec![0u8; 12 * 2 * 4];
        blit(&src, &cfg(4, 2, 32), &mut dst, 10, 2, 12, false);
        assert_eq!(
            &dst[12 * 4..][..4],
            &[0, 1, 0xAB, 0xFF],
            "second row starts a stride on"
        );
        assert_eq!(&dst[12 * 4 + 3 * 4..][..4], &[3, 1, 0xAB, 0xFF]);
    }

    #[test]
    fn a_red_low_host_gets_red_and_blue_swapped() {
        let src = [0x10, 0x20, 0x30, 0xFF, 0x40, 0x50, 0x60, 0x00];
        let mut dst = [0u8; 8];
        blit(&src, &cfg(2, 1, 8), &mut dst, 2, 1, 2, true);
        assert_eq!(dst, [0x30, 0x20, 0x10, 0xFF, 0x60, 0x50, 0x40, 0x00]);
    }

    #[test]
    fn buffers_too_short_for_their_geometry_are_left_alone() {
        let src = picture(4, 3, 16);
        let mut dst = vec![0x55u8; 8 * 5 * 4];
        blit(&src[..20], &cfg(4, 3, 16), &mut dst, 8, 5, 8, false);
        assert!(dst.iter().all(|&b| b == 0x55), "short source");
        let mut small = vec![0x55u8; 10];
        blit(&src, &cfg(4, 3, 16), &mut small, 8, 5, 8, false);
        assert!(small.iter().all(|&b| b == 0x55), "short destination");
    }
}
