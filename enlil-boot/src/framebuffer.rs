//! GOP framebuffer drawing backend for the enlil kernel (Phase 6.2).
//!
//! The UEFI stage collects the GOP linear framebuffer into the handoff
//! ([`Framebuffer`](crate::handoff::Framebuffer)); once boot services are gone
//! the kernel owns that memory directly. This module draws into it: a
//! [`Canvas`] is a borrowed view over the raw framebuffer bytes with the
//! geometry needed to place pixels, and the pixel-offset / fill logic is pure
//! and host-tested over an ordinary byte buffer. The firmware
//! [`FramebufferConsole`] wraps the live framebuffer and self-tests by writing
//! a pixel and reading it back — proof the GOP backend is wired and writable
//! with no firmware help.
//!
//! Color is written as **grayscale** (equal R/G/B), which is independent of
//! the RGB-vs-BGR channel order — the handoff does not yet record the pixel
//! format, so a color console waits until it does (see the ROADMAP note).

use crate::handoff::Framebuffer;

/// A grayscale intensity (0 = black, 255 = white), written to every color
/// channel so the result is correct for both RGB and BGR modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gray(pub u8);

impl Gray {
    /// Black.
    pub const BLACK: Self = Self(0);
    /// White.
    pub const WHITE: Self = Self(255);

    /// The little-endian pixel bytes for this gray at `bytes_per_pixel`.
    ///
    /// The low `bytes_per_pixel` bytes are returned; the color channels all
    /// get the intensity and any 4th (reserved/alpha) byte is 0.
    #[must_use]
    pub const fn to_pixel_bytes(self, bytes_per_pixel: u32) -> [u8; 4] {
        match bytes_per_pixel {
            // 32-bit (3 color channels + reserved) and 24-bit (3 channels):
            // low three bytes carry the intensity, the 4th is a zero
            // reserved/alpha byte.
            3 | 4 => [self.0, self.0, self.0, 0],
            // Fallback for unusual depths: fill what we can.
            _ => [self.0, self.0, self.0, self.0],
        }
    }
}

/// A borrowed drawing view over framebuffer bytes plus its geometry.
///
/// Split from the firmware code so the placement logic is host-testable over
/// a plain `&mut [u8]`. All coordinates are clamped/bounds-checked; an
/// off-screen pixel is a no-op rather than a panic or a wild write.
pub struct Canvas<'a> {
    bytes: &'a mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    bytes_per_pixel: u32,
}

impl<'a> Canvas<'a> {
    /// Wrap `bytes` as a framebuffer of the given geometry.
    ///
    /// Returns `None` if the geometry is degenerate (zero `bytes_per_pixel`)
    /// or `bytes` is too short to hold `stride * height` pixels.
    #[must_use]
    pub fn new(
        bytes: &'a mut [u8],
        width: u32,
        height: u32,
        stride: u32,
        bytes_per_pixel: u32,
    ) -> Option<Self> {
        if bytes_per_pixel == 0 || width == 0 || height == 0 {
            return None;
        }
        let needed = (stride as usize)
            .checked_mul(height as usize)?
            .checked_mul(bytes_per_pixel as usize)?;
        if bytes.len() < needed {
            return None;
        }
        Some(Self {
            bytes,
            width,
            height,
            stride,
            bytes_per_pixel,
        })
    }

    /// Wrap the handoff [`Framebuffer`] over its live memory.
    ///
    /// # Safety
    ///
    /// `fb.base` must point to `fb.size_bytes()` of valid, writable
    /// framebuffer memory owned by the caller (the GOP linear framebuffer
    /// handed across `ExitBootServices`).
    #[must_use]
    pub unsafe fn from_framebuffer(fb: &Framebuffer) -> Option<Self> {
        let len = usize::try_from(fb.size_bytes()).ok()?;
        // SAFETY: contract delegated to the caller (see doc).
        let bytes = unsafe { core::slice::from_raw_parts_mut(fb.base as *mut u8, len) };
        Self::new(bytes, fb.width, fb.height, fb.stride, fb.bytes_per_pixel)
    }

    /// Byte offset of the pixel at `(x, y)`, or `None` if off-screen.
    #[must_use]
    pub fn pixel_offset(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = (y as usize)
            .checked_mul(self.stride as usize)?
            .checked_add(x as usize)?
            .checked_mul(self.bytes_per_pixel as usize)?;
        Some(idx)
    }

    /// Set the pixel at `(x, y)`; a no-op if off-screen.
    pub fn put_pixel(&mut self, x: u32, y: u32, gray: Gray) {
        let Some(offset) = self.pixel_offset(x, y) else {
            return;
        };
        let pixel = gray.to_pixel_bytes(self.bytes_per_pixel);
        let bpp = self.bytes_per_pixel as usize;
        if let Some(dst) = self.bytes.get_mut(offset..offset + bpp) {
            dst.copy_from_slice(&pixel[..bpp]);
        }
    }

    /// Read the pixel at `(x, y)` back as its raw channel bytes, or `None`
    /// off-screen. Used by the self-test.
    #[must_use]
    pub fn read_pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        let offset = self.pixel_offset(x, y)?;
        let bpp = self.bytes_per_pixel as usize;
        let src = self.bytes.get(offset..offset + bpp)?;
        let mut out = [0u8; 4];
        out[..bpp].copy_from_slice(src);
        Some(out)
    }

    /// Fill the rectangle `[x, x+w) x [y, y+h)` (clipped to the canvas) with
    /// `gray`.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, gray: Gray) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        let mut py = y;
        while py < y_end {
            let mut px = x;
            while px < x_end {
                self.put_pixel(px, py, gray);
                px += 1;
            }
            py += 1;
        }
    }

    /// Fill the whole canvas with `gray`.
    pub fn clear(&mut self, gray: Gray) {
        self.fill_rect(0, 0, self.width, self.height, gray);
    }

    /// The canvas width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The canvas height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Blit an 8x8 `glyph` at `(x, y)`: set pixels for `1` bits to `fg`, `0`
    /// bits to `bg`. Each glyph byte is a row, bit 7 the leftmost pixel.
    pub fn draw_glyph(&mut self, x: u32, y: u32, glyph: &[u8; GLYPH_HEIGHT], fg: Gray, bg: Gray) {
        for (&bits, row) in glyph.iter().zip(0u32..) {
            for col in 0..GLYPH_WIDTH {
                let color = if glyph_row_bit(bits, col) { fg } else { bg };
                self.put_pixel(x + col, y + row, color);
            }
        }
    }

    /// Draw `text` (ASCII) starting at `(x, y)`, one 8x8 glyph per character
    /// advancing by [`GLYPH_WIDTH`]. Unknown characters render blank.
    pub fn draw_text(&mut self, x: u32, y: u32, text: &[u8], fg: Gray, bg: Gray) {
        let mut gx = x;
        for &c in text {
            self.draw_glyph(gx, y, &font_glyph(c), fg, bg);
            gx += GLYPH_WIDTH;
        }
    }
}

/// Glyph cell width in pixels.
pub const GLYPH_WIDTH: u32 = 8;
/// Glyph cell height in pixels (rows in the bitmap).
pub const GLYPH_HEIGHT: usize = 8;

/// Whether column `col` (0 = leftmost) of an 8-pixel glyph row `bits` is set.
#[must_use]
pub const fn glyph_row_bit(bits: u8, col: u32) -> bool {
    (bits >> (7 - col)) & 1 != 0
}

/// The 8x8 bitmap for an ASCII character, or a blank cell for unsupported ones.
///
/// A minimal font — enough to render the kernel's on-screen banner; the full
/// ASCII set waits until a font resource replaces this hand-rolled table.
#[must_use]
pub const fn font_glyph(c: u8) -> [u8; GLYPH_HEIGHT] {
    match c {
        b'E' => [0xFE, 0xC0, 0xC0, 0xFC, 0xC0, 0xC0, 0xFE, 0x00],
        b'N' => [0xC6, 0xE6, 0xF6, 0xDE, 0xCE, 0xC6, 0xC6, 0x00],
        b'L' => [0xC0, 0xC0, 0xC0, 0xC0, 0xC0, 0xC0, 0xFE, 0x00],
        b'I' => [0xFE, 0x30, 0x30, 0x30, 0x30, 0x30, 0xFE, 0x00],
        b'K' => [0xC6, 0xCC, 0xD8, 0xF0, 0xD8, 0xCC, 0xC6, 0x00],
        b'R' => [0xFC, 0xC6, 0xC6, 0xFC, 0xD8, 0xCC, 0xC6, 0x00],
        _ => [0; GLYPH_HEIGHT], // space + unsupported → blank
    }
}

#[cfg(target_os = "uefi")]
pub use hw::{draw_and_selftest, draw_text_banner};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{Canvas, GLYPH_WIDTH, Gray, font_glyph, glyph_row_bit};
    use crate::handoff::Framebuffer;

    /// Draw the kernel banner text on the framebuffer and self-test it by
    /// reading back a pixel the glyph bitmap says must be lit.
    ///
    /// Returns whether the read-back matched — proof the 8x8 text console blits
    /// correctly into the live framebuffer. Draws "ENLIL" in black on the white
    /// top band `draw_and_selftest` laid down.
    #[must_use]
    pub fn draw_text_banner(fb: &Framebuffer) -> bool {
        const BANNER: &[u8] = b"ENLIL";
        // SAFETY: `fb` describes the live GOP framebuffer (see draw_and_selftest).
        let Some(mut canvas) = (unsafe { Canvas::from_framebuffer(fb) }) else {
            return false;
        };
        let (tx, ty) = (8, 4);
        canvas.draw_text(tx, ty, BANNER, Gray::BLACK, Gray::WHITE);

        // Verify a pixel the 'E' glyph (first char) sets: its top row is 0xFE,
        // so column 0 of row 0 is lit → black foreground.
        let lit = glyph_row_bit(font_glyph(b'E')[0], 0);
        let px = canvas.read_pixel(tx, ty);
        // And a definitely-blank cell to the right of the banner stays white.
        let blank_x = tx + u32::try_from(BANNER.len()).unwrap_or(0) * GLYPH_WIDTH;
        let blank = canvas.read_pixel(blank_x, ty);
        lit && matches!(px, Some([0, 0, 0, _])) && matches!(blank, Some([255, 255, 255, _]))
    }

    /// Draw a boot indicator on the GOP framebuffer and self-test the backend
    /// by writing a known pixel and reading it back.
    ///
    /// Returns whether the read-back matched — proof the kernel can drive the
    /// framebuffer with the firmware gone. Draws a dark background with a
    /// white band across the top so a physical boot shows visible life.
    #[must_use]
    pub fn draw_and_selftest(fb: &Framebuffer) -> bool {
        // SAFETY: `fb` came from the UEFI GOP collection and describes live,
        // writable linear framebuffer memory handed across ExitBootServices.
        let Some(mut canvas) = (unsafe { Canvas::from_framebuffer(fb) }) else {
            return false;
        };

        // Dark background, a white status band across the top eighth.
        canvas.clear(Gray(0x10));
        let band_h = (canvas.height() / 8).max(1);
        canvas.fill_rect(0, 0, canvas.width(), band_h, Gray::WHITE);

        // Self-test: write a mid-gray probe pixel and read it back.
        let (px, py) = (canvas.width() / 2, canvas.height() / 2);
        canvas.put_pixel(px, py, Gray(0x7F));
        matches!(canvas.read_pixel(px, py), Some([0x7F, 0x7F, 0x7F, _]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(_width: u32, height: u32, stride: u32, bpp: u32) -> Vec<u8> {
        vec![0u8; (stride * height * bpp) as usize]
    }

    #[test]
    fn pixel_offset_honors_stride_and_bounds() {
        let mut buf = geometry(4, 4, 8, 4); // padded stride 8 > width 4
        let canvas = Canvas::new(&mut buf, 4, 4, 8, 4).expect("valid geometry");
        assert_eq!(canvas.pixel_offset(0, 0), Some(0));
        assert_eq!(canvas.pixel_offset(1, 0), Some(4));
        // Row 1 begins one full stride in (8 pixels * 4 bpp), not one width.
        assert_eq!(canvas.pixel_offset(0, 1), Some(32));
        assert_eq!(canvas.pixel_offset(4, 0), None); // off the right edge
        assert_eq!(canvas.pixel_offset(0, 4), None); // off the bottom
    }

    #[test]
    fn put_and_read_pixel_round_trips() {
        let mut buf = geometry(4, 4, 4, 4);
        let mut canvas = Canvas::new(&mut buf, 4, 4, 4, 4).expect("valid");
        canvas.put_pixel(2, 1, Gray(0x7F));
        assert_eq!(canvas.read_pixel(2, 1), Some([0x7F, 0x7F, 0x7F, 0]));
        // Untouched neighbor stays black.
        assert_eq!(canvas.read_pixel(1, 1), Some([0, 0, 0, 0]));
    }

    #[test]
    fn off_screen_writes_are_noops() {
        let mut buf = geometry(2, 2, 2, 4);
        let before = buf.clone();
        let mut canvas = Canvas::new(&mut buf, 2, 2, 2, 4).expect("valid");
        canvas.put_pixel(99, 99, Gray::WHITE); // ignored
        assert_eq!(buf, before);
    }

    #[test]
    fn fill_rect_clips_to_the_canvas() {
        let mut buf = geometry(4, 4, 4, 4);
        let mut canvas = Canvas::new(&mut buf, 4, 4, 4, 4).expect("valid");
        // A rect starting near the edge and running past it fills only the
        // in-bounds part.
        canvas.fill_rect(3, 3, 10, 10, Gray::WHITE);
        assert_eq!(canvas.read_pixel(3, 3), Some([255, 255, 255, 0]));
        assert_eq!(canvas.read_pixel(2, 2), Some([0, 0, 0, 0]));
    }

    #[test]
    fn glyph_row_bit_reads_left_to_right() {
        // 0x80 = only the leftmost pixel; 0x01 = only the rightmost.
        assert!(glyph_row_bit(0x80, 0));
        assert!(!glyph_row_bit(0x80, 1));
        assert!(glyph_row_bit(0x01, 7));
        assert!(!glyph_row_bit(0x01, 0));
        // 0xFE (E's top row) is lit across columns 0..=6, blank at 7.
        assert!(glyph_row_bit(0xFE, 0));
        assert!(!glyph_row_bit(0xFE, 7));
    }

    #[test]
    fn font_glyph_blank_for_unsupported() {
        assert_eq!(font_glyph(b' '), [0; GLYPH_HEIGHT]);
        assert_eq!(font_glyph(b'?'), [0; GLYPH_HEIGHT]);
        assert_ne!(font_glyph(b'E'), [0; GLYPH_HEIGHT]);
    }

    #[test]
    fn draw_text_blits_glyph_pixels() {
        // A canvas big enough for one glyph.
        let mut buf = geometry(8, 8, 8, 4);
        let mut canvas = Canvas::new(&mut buf, 8, 8, 8, 4).expect("valid");
        canvas.draw_text(0, 0, b"E", Gray::WHITE, Gray::BLACK);
        // E's top row (0xFE): column 0 lit (white), column 7 blank (black).
        assert_eq!(canvas.read_pixel(0, 0), Some([255, 255, 255, 0]));
        assert_eq!(canvas.read_pixel(7, 0), Some([0, 0, 0, 0]));
        // Row 1 (0xC0): column 0 lit, column 2 blank.
        assert_eq!(canvas.read_pixel(0, 1), Some([255, 255, 255, 0]));
        assert_eq!(canvas.read_pixel(2, 1), Some([0, 0, 0, 0]));
    }

    #[test]
    fn clear_fills_every_pixel() {
        let mut buf = geometry(3, 3, 3, 4);
        let mut canvas = Canvas::new(&mut buf, 3, 3, 3, 4).expect("valid");
        canvas.clear(Gray(0x22));
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(canvas.read_pixel(x, y), Some([0x22, 0x22, 0x22, 0]));
            }
        }
    }

    #[test]
    fn rejects_degenerate_geometry_and_short_buffers() {
        let mut buf = geometry(4, 4, 4, 4);
        assert!(Canvas::new(&mut buf, 4, 4, 4, 0).is_none()); // zero bpp
        assert!(Canvas::new(&mut buf, 0, 4, 4, 4).is_none()); // zero width
        let mut small = vec![0u8; 16];
        assert!(Canvas::new(&mut small, 4, 4, 4, 4).is_none()); // too short
    }

    #[test]
    fn gray_pixel_bytes_are_channel_order_independent() {
        // Equal channels → identical whether the mode is RGB or BGR.
        assert_eq!(Gray(0x7F).to_pixel_bytes(4), [0x7F, 0x7F, 0x7F, 0]);
        assert_eq!(Gray::WHITE.to_pixel_bytes(3), [255, 255, 255, 0]);
    }
}
