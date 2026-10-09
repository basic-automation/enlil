//! GOP framebuffer color console for the enlil kernel (Phase 6.2/6.5).
//!
//! The UEFI stage collects the GOP linear framebuffer into the handoff
//! ([`Framebuffer`](crate::handoff::Framebuffer)) — including the GOP
//! [`PixelFormat`](crate::handoff::PixelFormat) so the kernel writes color
//! channels in the right order — and once boot services are gone the kernel
//! owns that memory directly. This module draws into it:
//!
//! * [`Canvas`]: a borrowed view over the raw framebuffer bytes with the
//!   geometry needed to place pixels; the placement logic is pure and
//!   host-tested over an ordinary byte buffer.
//! * [`Color`]: an RGB color whose pixel bytes honor the mode's channel
//!   order (RGB vs BGR).
//! * [`TextConsole`]: a real text console over a [`Canvas`] — full-ASCII 8x8
//!   font, line wrap, scrolling, backspace, and a block cursor. It implements
//!   [`core::fmt::Write`] so the kernel can `writeln!` into it.
//!
//! The firmware self-tests ([`hw`]) wrap the live framebuffer and verify by
//! writing pixels and reading them back — proof the GOP backend is wired and
//! writable with no firmware help.

use crate::handoff::{Framebuffer, PixelFormat};

/// An RGB color for the framebuffer console.
///
/// The [`PixelFormat`] carried by the [`Canvas`] decides the on-wire channel
/// order; see [`Color::to_pixel_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Red channel intensity (0-255).
    pub r: u8,
    /// Green channel intensity (0-255).
    pub g: u8,
    /// Blue channel intensity (0-255).
    pub b: u8,
}

impl Color {
    /// Black.
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    /// White.
    pub const WHITE: Self = Self::rgb(0xFF, 0xFF, 0xFF);
    /// Pure red.
    pub const RED: Self = Self::rgb(0xFF, 0, 0);
    /// Pure green.
    pub const GREEN: Self = Self::rgb(0, 0xFF, 0);
    /// Pure blue.
    pub const BLUE: Self = Self::rgb(0, 0, 0xFF);
    /// Yellow.
    pub const YELLOW: Self = Self::rgb(0xFF, 0xFF, 0);
    /// Cyan.
    pub const CYAN: Self = Self::rgb(0, 0xFF, 0xFF);

    /// Build a color from its channels.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// A grayscale intensity (0 = black, 255 = white).
    #[must_use]
    pub const fn gray(v: u8) -> Self {
        Self::rgb(v, v, v)
    }

    /// The little-endian pixel bytes for this color at `bytes_per_pixel`,
    /// honoring the GOP `format`'s channel order.
    ///
    /// RGB modes store `[r, g, b, 0]`, BGR modes `[b, g, r, 0]`; the 4th
    /// (reserved) byte is 0. A [`PixelFormat::Bitmask`] mode carries no mask
    /// info in the handoff, so it is written RGB-order as a documented
    /// best-effort.
    #[must_use]
    pub const fn to_pixel_bytes(self, bytes_per_pixel: u32, format: PixelFormat) -> [u8; 4] {
        let ordered = match format {
            PixelFormat::Bgr => [self.b, self.g, self.r, 0],
            PixelFormat::Rgb | PixelFormat::Bitmask | PixelFormat::BltOnly => {
                [self.r, self.g, self.b, 0]
            }
        };
        match bytes_per_pixel {
            // 32-bit (3 color channels + reserved) and 24-bit (3 channels).
            3 | 4 => ordered,
            // Fallback for unusual depths: fill what we can.
            _ => [self.r, self.g, self.b, self.r],
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
    format: PixelFormat,
}

impl<'a> Canvas<'a> {
    /// Wrap `bytes` as a framebuffer of the given geometry, writing pixels in
    /// the `format` channel order.
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
        format: PixelFormat,
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
            format,
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
        Self::new(
            bytes,
            fb.width,
            fb.height,
            fb.stride,
            fb.bytes_per_pixel,
            fb.pixel_format,
        )
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
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        let Some(offset) = self.pixel_offset(x, y) else {
            return;
        };
        let pixel = color.to_pixel_bytes(self.bytes_per_pixel, self.format);
        let bpp = self.bytes_per_pixel as usize;
        if let Some(dst) = self.bytes.get_mut(offset..offset + bpp) {
            dst.copy_from_slice(&pixel[..bpp]);
        }
    }

    /// Write raw channel bytes for the pixel at `(x, y)`; a no-op if
    /// off-screen. Used to restore pixels saved from under the cursor.
    fn put_pixel_raw(&mut self, x: u32, y: u32, bytes: [u8; 4]) {
        let Some(offset) = self.pixel_offset(x, y) else {
            return;
        };
        let bpp = self.bytes_per_pixel as usize;
        if let Some(dst) = self.bytes.get_mut(offset..offset + bpp) {
            dst.copy_from_slice(&bytes[..bpp]);
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
    /// `color`.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Color) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        let mut py = y;
        while py < y_end {
            let mut px = x;
            while px < x_end {
                self.put_pixel(px, py, color);
                px += 1;
            }
            py += 1;
        }
    }

    /// Fill the whole canvas with `color`.
    pub fn clear(&mut self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Scroll the pixel content up by `rows` scanlines: rows `[rows..height)`
    /// move to `[0..height-rows)`. The freed bottom band keeps its old bytes;
    /// the caller clears it. Safe on overlapping ranges (`copy_within`
    /// semantics).
    pub fn scroll_up(&mut self, rows: u32) {
        let rows = rows.min(self.height);
        if rows == 0 {
            return;
        }
        let row_bytes = (self.stride as usize).saturating_mul(self.bytes_per_pixel as usize);
        let total = row_bytes
            .saturating_mul(self.height as usize)
            .min(self.bytes.len());
        let src = row_bytes.saturating_mul(rows as usize).min(total);
        self.bytes.copy_within(src..total, 0);
    }

    /// Blit an 8x8 `glyph` at `(x, y)`: set pixels for `1` bits to `fg`, `0`
    /// bits to `bg`. Each glyph byte is a row, bit 7 the leftmost pixel.
    pub fn draw_glyph(&mut self, x: u32, y: u32, glyph: &[u8; GLYPH_HEIGHT], fg: Color, bg: Color) {
        for (&bits, row) in glyph.iter().zip(0u32..) {
            for col in 0..GLYPH_WIDTH {
                let color = if glyph_row_bit(bits, col) { fg } else { bg };
                self.put_pixel(x + col, y + row, color);
            }
        }
    }

    /// Draw `text` (ASCII) starting at `(x, y)`, one 8x8 glyph per character
    /// advancing by [`GLYPH_WIDTH`]. Unsupported characters render blank.
    pub fn draw_text(&mut self, x: u32, y: u32, text: &[u8], fg: Color, bg: Color) {
        let mut gx = x;
        for &c in text {
            self.draw_glyph(gx, y, &font_glyph(c), fg, bg);
            gx += GLYPH_WIDTH;
        }
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

    /// The GOP pixel format: the channel order pixels are written in.
    #[must_use]
    pub const fn format(&self) -> PixelFormat {
        self.format
    }
}

/// Glyph cell width in pixels.
pub const GLYPH_WIDTH: u32 = 8;
/// Glyph cell height in pixels (rows in the bitmap).
pub const GLYPH_HEIGHT: usize = 8;

/// 8x8 bitmap font for printable ASCII (`0x20..=0x7E`).
///
/// Authored as ASCII art and generated by `font8x8.py` (kept in the
/// workflow workdir); each entry is 8 rows, bit 7 the leftmost pixel.
/// `0x7F` and control bytes render as the blank cell via [`font_glyph`].
const FONT: [[u8; GLYPH_HEIGHT]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x20 ' '
    [0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x18, 0x00], // 0x21 '!'
    [0x6C, 0x6C, 0x6C, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x22 '"'
    [0x24, 0x24, 0x7E, 0x24, 0x24, 0x7E, 0x24, 0x00], // 0x23 '#'
    [0x10, 0x3C, 0x50, 0x38, 0x1C, 0x0A, 0x3C, 0x08], // 0x24 '$'
    [0x62, 0x62, 0x04, 0x08, 0x10, 0x20, 0x46, 0x46], // 0x25 '%'
    [0x38, 0x44, 0x44, 0x30, 0x4C, 0x44, 0x46, 0x00], // 0x26 '&'
    [0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x27 "'"
    [0x0C, 0x18, 0x30, 0x30, 0x30, 0x18, 0x0C, 0x00], // 0x28 '('
    [0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x18, 0x30, 0x00], // 0x29 ')'
    [0x00, 0x44, 0x28, 0x7C, 0x28, 0x44, 0x00, 0x00], // 0x2A '*'
    [0x00, 0x10, 0x10, 0x7C, 0x10, 0x10, 0x00, 0x00], // 0x2B '+'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30], // 0x2C ','
    [0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x00], // 0x2D '-'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00], // 0x2E '.'
    [0x02, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x40], // 0x2F '/'
    [0x38, 0x44, 0x4C, 0x54, 0x64, 0x44, 0x38, 0x00], // 0x30 '0'
    [0x10, 0x30, 0x10, 0x10, 0x10, 0x10, 0x7C, 0x00], // 0x31 '1'
    [0x38, 0x44, 0x04, 0x0C, 0x18, 0x20, 0x7E, 0x00], // 0x32 '2'
    [0x7C, 0x08, 0x08, 0x3C, 0x08, 0x08, 0x7C, 0x00], // 0x33 '3'
    [0x0C, 0x1C, 0x2C, 0x44, 0x7E, 0x04, 0x04, 0x00], // 0x34 '4'
    [0x7E, 0x40, 0x7C, 0x02, 0x02, 0x42, 0x3C, 0x00], // 0x35 '5'
    [0x1C, 0x30, 0x40, 0x7C, 0x42, 0x42, 0x3C, 0x00], // 0x36 '6'
    [0x7E, 0x04, 0x08, 0x10, 0x10, 0x10, 0x10, 0x00], // 0x37 '7'
    [0x3C, 0x42, 0x42, 0x3C, 0x42, 0x42, 0x3C, 0x00], // 0x38 '8'
    [0x3C, 0x42, 0x42, 0x3E, 0x02, 0x0C, 0x38, 0x00], // 0x39 '9'
    [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x00], // 0x3A ':'
    [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x30], // 0x3B ';'
    [0x04, 0x08, 0x10, 0x20, 0x10, 0x08, 0x04, 0x00], // 0x3C '<'
    [0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00, 0x00], // 0x3D '='
    [0x20, 0x10, 0x08, 0x04, 0x08, 0x10, 0x20, 0x00], // 0x3E '>'
    [0x38, 0x44, 0x04, 0x08, 0x10, 0x00, 0x10, 0x00], // 0x3F '?'
    [0x38, 0x44, 0x5C, 0x54, 0x5C, 0x40, 0x3C, 0x00], // 0x40 '@'
    [0x18, 0x3C, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x00], // 0x41 'A'
    [0x7C, 0x42, 0x42, 0x7C, 0x42, 0x42, 0x7C, 0x00], // 0x42 'B'
    [0x3C, 0x42, 0x40, 0x40, 0x40, 0x42, 0x3C, 0x00], // 0x43 'C'
    [0x78, 0x44, 0x42, 0x42, 0x42, 0x44, 0x78, 0x00], // 0x44 'D'
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x7E, 0x00], // 0x45 'E'
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x40, 0x00], // 0x46 'F'
    [0x3C, 0x42, 0x40, 0x4E, 0x42, 0x42, 0x3C, 0x00], // 0x47 'G'
    [0x42, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x42, 0x00], // 0x48 'H'
    [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00], // 0x49 'I'
    [0x1E, 0x08, 0x08, 0x08, 0x08, 0x48, 0x30, 0x00], // 0x4A 'J'
    [0x44, 0x48, 0x50, 0x60, 0x50, 0x48, 0x44, 0x00], // 0x4B 'K'
    [0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7E, 0x00], // 0x4C 'L'
    [0x42, 0x66, 0x5A, 0x5A, 0x42, 0x42, 0x42, 0x00], // 0x4D 'M'
    [0x42, 0x62, 0x62, 0x52, 0x4A, 0x46, 0x46, 0x00], // 0x4E 'N'
    [0x3C, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00], // 0x4F 'O'
    [0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40, 0x40, 0x00], // 0x50 'P'
    [0x3C, 0x42, 0x42, 0x42, 0x52, 0x4A, 0x3C, 0x0A], // 0x51 'Q'
    [0x7C, 0x42, 0x42, 0x7C, 0x48, 0x44, 0x42, 0x00], // 0x52 'R'
    [0x3C, 0x42, 0x40, 0x3C, 0x02, 0x02, 0x42, 0x3C], // 0x53 'S'
    [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00], // 0x54 'T'
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00], // 0x55 'U'
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00], // 0x56 'V'
    [0x42, 0x42, 0x42, 0x42, 0x5A, 0x5A, 0x66, 0x00], // 0x57 'W'
    [0x42, 0x42, 0x24, 0x18, 0x18, 0x24, 0x42, 0x00], // 0x58 'X'
    [0x42, 0x42, 0x24, 0x18, 0x18, 0x18, 0x18, 0x00], // 0x59 'Y'
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x20, 0x7E, 0x00], // 0x5A 'Z'
    [0x1C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x1C, 0x00], // 0x5B '['
    [0x40, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x02], // 0x5C '\\'
    [0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x38, 0x00], // 0x5D ']'
    [0x18, 0x3C, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x5E '^'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7E, 0x00], // 0x5F '_'
    [0x18, 0x18, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x60 '`'
    [0x00, 0x00, 0x3C, 0x02, 0x3E, 0x42, 0x3E, 0x00], // 0x61 'a'
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x7C, 0x00], // 0x62 'b'
    [0x00, 0x00, 0x3C, 0x40, 0x40, 0x40, 0x3C, 0x00], // 0x63 'c'
    [0x02, 0x02, 0x3E, 0x42, 0x42, 0x42, 0x3E, 0x00], // 0x64 'd'
    [0x00, 0x00, 0x3C, 0x42, 0x7E, 0x40, 0x3C, 0x00], // 0x65 'e'
    [0x0C, 0x18, 0x18, 0x3C, 0x18, 0x18, 0x18, 0x00], // 0x66 'f'
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x3C], // 0x67 'g'
    [0x40, 0x40, 0x7C, 0x44, 0x44, 0x44, 0x44, 0x00], // 0x68 'h'
    [0x18, 0x00, 0x38, 0x18, 0x18, 0x18, 0x3C, 0x00], // 0x69 'i'
    [0x0C, 0x00, 0x1C, 0x0C, 0x0C, 0x0C, 0x4C, 0x18], // 0x6A 'j'
    [0x40, 0x40, 0x44, 0x48, 0x70, 0x48, 0x44, 0x00], // 0x6B 'k'
    [0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00], // 0x6C 'l'
    [0x00, 0x00, 0x6C, 0x54, 0x54, 0x44, 0x44, 0x00], // 0x6D 'm'
    [0x00, 0x00, 0x7C, 0x44, 0x44, 0x44, 0x44, 0x00], // 0x6E 'n'
    [0x00, 0x00, 0x3C, 0x42, 0x42, 0x42, 0x3C, 0x00], // 0x6F 'o'
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40], // 0x70 'p'
    [0x00, 0x00, 0x3C, 0x42, 0x42, 0x3E, 0x02, 0x02], // 0x71 'q'
    [0x00, 0x00, 0x5C, 0x64, 0x40, 0x40, 0x40, 0x00], // 0x72 'r'
    [0x00, 0x00, 0x3C, 0x40, 0x1C, 0x02, 0x3C, 0x00], // 0x73 's'
    [0x10, 0x10, 0x7C, 0x10, 0x10, 0x10, 0x18, 0x00], // 0x74 't'
    [0x00, 0x00, 0x44, 0x44, 0x44, 0x44, 0x3C, 0x00], // 0x75 'u'
    [0x00, 0x00, 0x44, 0x44, 0x44, 0x28, 0x10, 0x00], // 0x76 'v'
    [0x00, 0x00, 0x44, 0x44, 0x54, 0x54, 0x6C, 0x00], // 0x77 'w'
    [0x00, 0x00, 0x44, 0x28, 0x10, 0x28, 0x44, 0x00], // 0x78 'x'
    [0x00, 0x00, 0x44, 0x44, 0x44, 0x3C, 0x08, 0x70], // 0x79 'y'
    [0x00, 0x00, 0x7E, 0x04, 0x08, 0x10, 0x7E, 0x00], // 0x7A 'z'
    [0x0E, 0x18, 0x18, 0x38, 0x18, 0x18, 0x0E, 0x00], // 0x7B '{'
    [0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00], // 0x7C '|'
    [0x70, 0x18, 0x18, 0x1C, 0x18, 0x18, 0x70, 0x00], // 0x7D '}'
    [0x00, 0x00, 0x2C, 0x48, 0x00, 0x00, 0x00, 0x00], // 0x7E '~'
];

/// Whether column `col` (0 = leftmost) of an 8-pixel glyph row `bits` is set.
#[must_use]
pub const fn glyph_row_bit(bits: u8, col: u32) -> bool {
    (bits >> (7 - col)) & 1 != 0
}

/// The 8x8 bitmap for an ASCII character, from the full printable-ASCII
/// [`FONT`] table; anything outside `0x20..=0x7E` renders as a blank cell.
#[must_use]
pub const fn font_glyph(c: u8) -> [u8; GLYPH_HEIGHT] {
    if c >= 0x20 && c <= 0x7E {
        FONT[(c - 0x20) as usize]
    } else {
        [0; GLYPH_HEIGHT]
    }
}

/// Height of the block cursor in pixels (the bottom rows of the cell).
const CURSOR_HEIGHT: u32 = 2;
/// Raw bytes saved from under the cursor: 8 x 2 pixels, 4 bytes each.
const CURSOR_SAVE_BYTES: usize = 8 * 2 * 4;

/// A scrolling text console over a [`Canvas`]: full-ASCII 8x8 text with line
/// wrap, scrolling, backspace, and a block cursor.
///
/// The console owns its [`Canvas`] (built from the handoff [`Framebuffer`]);
/// all drawing is bounds-checked, so writing past the edges wraps or scrolls
/// instead of faulting. Implements [`core::fmt::Write`], so the kernel can
/// `writeln!` colored status lines into it.
///
/// The block cursor saves the pixels underneath it before painting and
/// restores them when it moves, so it never damages glyphs (e.g. the
/// descenders of `g`/`y`) it sits on.
pub struct TextConsole<'a> {
    canvas: Canvas<'a>,
    fg: Color,
    bg: Color,
    cell_h: u32,
    cols: u32,
    rows: u32,
    cx: u32,
    cy: u32,
    cursor_visible: bool,
    cursor_drawn: bool,
    under_cursor: [u8; CURSOR_SAVE_BYTES],
}

impl<'a> TextConsole<'a> {
    /// Wrap `canvas` as a text console with the given default colors and a
    /// visible block cursor at the top-left cell.
    ///
    /// Returns `None` when the canvas is too small for even one 8x8 cell.
    /// The screen is *not* cleared; call [`clear`](Self::clear) first for a
    /// fresh console.
    #[must_use]
    pub fn new(canvas: Canvas<'a>, fg: Color, bg: Color) -> Option<Self> {
        let cell_h = u32::try_from(GLYPH_HEIGHT).ok()?;
        let cols = canvas.width() / GLYPH_WIDTH;
        let rows = canvas.height() / cell_h;
        if cols == 0 || rows == 0 {
            return None;
        }
        let mut this = Self {
            canvas,
            fg,
            bg,
            cell_h,
            cols,
            rows,
            cx: 0,
            cy: 0,
            cursor_visible: true,
            cursor_drawn: false,
            under_cursor: [0; CURSOR_SAVE_BYTES],
        };
        this.draw_cursor();
        Some(this)
    }

    /// Fill the screen with the background color and home the cursor.
    pub fn clear(&mut self) {
        self.cursor_drawn = false;
        self.canvas.clear(self.bg);
        self.cx = 0;
        self.cy = 0;
        self.draw_cursor();
    }

    /// Change the default foreground/background colors for subsequently
    /// written cells (already-drawn cells keep their colors).
    pub const fn set_colors(&mut self, fg: Color, bg: Color) {
        self.fg = fg;
        self.bg = bg;
    }

    /// Show or hide the block cursor, erasing/redrawing as needed.
    pub fn set_cursor_visible(&mut self, visible: bool) {
        if visible == self.cursor_visible {
            return;
        }
        self.erase_cursor();
        self.cursor_visible = visible;
        self.draw_cursor();
    }

    /// Write one byte: printable ASCII draws a glyph and advances; `\n`
    /// moves to the next line (scrolling at the bottom), `\r` returns to
    /// column 0, `\t` advances to the next 8-column tab stop, backspace
    /// (`0x08`) erases the previous cell; other control bytes are ignored.
    pub fn write_byte(&mut self, b: u8) {
        self.erase_cursor();
        match b {
            b'\n' => self.advance_line(),
            b'\r' => self.cx = 0,
            b'\t' => {
                let stop = (self.cx + 8) & !7;
                while self.cx < stop.min(self.cols) {
                    self.put_cell(b' ');
                }
            }
            0x08 => self.backspace(),
            0x00..=0x1F | 0x7F => {}
            _ => self.put_cell(b),
        }
        self.draw_cursor();
    }

    /// Write raw bytes, interpreting `\n`, `\r`, `\t` and backspace.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_byte(b);
        }
    }

    /// Move to column 0 of the next row, scrolling the screen up when the
    /// cursor is already on the last row. Keeps the cursor consistent.
    pub fn newline(&mut self) {
        self.erase_cursor();
        self.advance_line();
        self.draw_cursor();
    }

    /// The console width in 8-pixel columns.
    #[must_use]
    pub const fn cols(&self) -> u32 {
        self.cols
    }

    /// The console height in 8-pixel rows.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.rows
    }

    /// The glyph cell height in pixels.
    #[must_use]
    pub const fn cell_height(&self) -> u32 {
        self.cell_h
    }

    /// The cursor position in `(column, row)` cells.
    #[must_use]
    pub const fn cursor(&self) -> (u32, u32) {
        (self.cx, self.cy)
    }

    /// Whether the block cursor is currently shown.
    #[must_use]
    pub const fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Read the pixel at `(x, y)` back as its raw channel bytes, or `None`
    /// off-screen. Used by the self-test.
    #[must_use]
    pub fn read_pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        self.canvas.read_pixel(x, y)
    }

    /// Draw `b`'s glyph at the cursor cell and advance, wrapping to the next
    /// line (scrolling at the bottom) when the row is full.
    fn put_cell(&mut self, b: u8) {
        if self.cx >= self.cols {
            self.advance_line();
        }
        let x = self.cx * GLYPH_WIDTH;
        let y = self.cy * self.cell_h;
        self.canvas
            .draw_glyph(x, y, &font_glyph(b), self.fg, self.bg);
        self.cx += 1;
    }

    /// Move to column 0 of the next row, scrolling when already on the last
    /// row. The cursor must be erased before calling this.
    fn advance_line(&mut self) {
        self.cx = 0;
        if self.cy + 1 >= self.rows {
            self.scroll();
        } else {
            self.cy += 1;
        }
    }

    /// Scroll the screen up by one text row and clear the freed bottom row.
    /// The cursor must be erased before calling this.
    fn scroll(&mut self) {
        self.canvas.scroll_up(self.cell_h);
        let y = (self.rows - 1) * self.cell_h;
        self.canvas
            .fill_rect(0, y, self.canvas.width(), self.cell_h, self.bg);
    }

    /// Erase the cell before the cursor and move back onto it; a no-op at
    /// the top-left corner.
    fn backspace(&mut self) {
        if self.cx > 0 {
            self.cx -= 1;
        } else if self.cy > 0 {
            self.cy -= 1;
            self.cx = self.cols - 1;
        } else {
            return;
        }
        let x = self.cx * GLYPH_WIDTH;
        let y = self.cy * self.cell_h;
        self.canvas
            .draw_glyph(x, y, &[0; GLYPH_HEIGHT], self.fg, self.bg);
    }

    /// Top-left pixel of the block cursor.
    const fn cursor_origin(&self) -> (u32, u32) {
        (
            self.cx * GLYPH_WIDTH,
            self.cy * self.cell_h + self.cell_h - CURSOR_HEIGHT,
        )
    }

    /// Draw the block cursor over the current cell, saving the pixels
    /// underneath first. A no-op when hidden or already drawn.
    fn draw_cursor(&mut self) {
        if !self.cursor_visible || self.cursor_drawn {
            return;
        }
        let (x0, y0) = self.cursor_origin();
        for dy in 0..CURSOR_HEIGHT {
            for dx in 0..GLYPH_WIDTH {
                let i = ((dy * GLYPH_WIDTH + dx) * 4) as usize;
                let saved = self.canvas.read_pixel(x0 + dx, y0 + dy).unwrap_or([0; 4]);
                self.under_cursor[i..i + 4].copy_from_slice(&saved);
                self.canvas.put_pixel(x0 + dx, y0 + dy, self.fg);
            }
        }
        self.cursor_drawn = true;
    }

    /// Erase the block cursor, restoring the saved pixels underneath. A
    /// no-op when not drawn.
    fn erase_cursor(&mut self) {
        if !self.cursor_drawn {
            return;
        }
        let (x0, y0) = self.cursor_origin();
        for dy in 0..CURSOR_HEIGHT {
            for dx in 0..GLYPH_WIDTH {
                let i = ((dy * GLYPH_WIDTH + dx) * 4) as usize;
                let mut saved = [0u8; 4];
                saved.copy_from_slice(&self.under_cursor[i..i + 4]);
                self.canvas.put_pixel_raw(x0 + dx, y0 + dy, saved);
            }
        }
        self.cursor_drawn = false;
    }
}

impl core::fmt::Write for TextConsole<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

/// Write the color management-console demo into `console`: a header, a
/// status line, then more numbered lines than the screen holds (forcing at
/// least one scroll).
///
/// Shared by the firmware self-test ([`draw_boot_console`](hw::draw_boot_console))
/// and host tests, so the exact on-screen accounting the live verification
/// asserts is covered without a VM.
pub fn write_console_demo(console: &mut TextConsole) {
    use core::fmt::Write as _;
    console.clear();
    let _ = writeln!(console, "enlil management console");
    console.set_colors(Color::GREEN, Color::BLACK);
    let _ = writeln!(console, "gop: color console online");
    console.set_colors(Color::CYAN, Color::BLACK);
    // Force scrolling: one more numbered line than the screen holds, on top
    // of the two header lines, so the top visible row is "boot line 01 ok".
    for n in 0..=console.rows() {
        let _ = writeln!(console, "boot line {n:02} ok");
    }
}

/// Verify the [`write_console_demo`] output by pixel read-back: the
/// scrolled-to-top row really moved up, and the block cursor sits on the
/// last row.
///
/// `bytes_per_pixel`/`format` describe the GOP mode the console was built
/// from, so the expected bytes honor its channel order.
#[must_use]
pub fn verify_console_demo(
    console: &TextConsole,
    bytes_per_pixel: u32,
    format: PixelFormat,
) -> bool {
    let fg = Color::CYAN;
    let bg = Color::BLACK;
    // The scrolled-to-top 'b' (row 0 = 0x40: column 1 lit, column 0 blank)
    // proves real content moved up, not just a clear.
    let brow0 = font_glyph(b'b')[0];
    let glyph_ok = glyph_row_bit(brow0, 1) && !glyph_row_bit(brow0, 0);
    let scroll_ok = console.read_pixel(1, 0) == Some(fg.to_pixel_bytes(bytes_per_pixel, format))
        && console.read_pixel(0, 0) == Some(bg.to_pixel_bytes(bytes_per_pixel, format));
    // The block cursor sits at the home column of the last row.
    let (cx, cy) = console.cursor();
    let cursor_y = cy * console.cell_height() + console.cell_height() - 1;
    let cursor_ok = cx == 0
        && cy == console.rows() - 1
        && console.read_pixel(cx * GLYPH_WIDTH, cursor_y)
            == Some(fg.to_pixel_bytes(bytes_per_pixel, format));
    glyph_ok && scroll_ok && cursor_ok
}

#[cfg(target_os = "uefi")]
pub use hw::{draw_and_selftest, draw_boot_console, draw_text_banner};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{Canvas, Color, TextConsole, font_glyph, glyph_row_bit};
    use crate::handoff::Framebuffer;

    /// Draw a color boot indicator on the GOP framebuffer and self-test the
    /// backend by writing a known pixel and reading it back.
    ///
    /// Returns whether the read-back matched — proof the kernel can drive the
    /// framebuffer with the firmware gone. Draws a dark-blue field with a
    /// yellow band across the top so a physical boot shows visible life, and
    /// probes a pure-red pixel: the read-back must match the mode's channel
    /// order (RGB vs BGR), proving the
    /// [`PixelFormat`](crate::handoff::PixelFormat) is threaded through the
    /// handoff correctly.
    #[must_use]
    pub fn draw_and_selftest(fb: &Framebuffer) -> bool {
        // SAFETY: `fb` came from the UEFI GOP collection and describes live,
        // writable linear framebuffer memory handed across ExitBootServices.
        let Some(mut canvas) = (unsafe { Canvas::from_framebuffer(fb) }) else {
            return false;
        };

        // Dark-blue field, a yellow status band across the top eighth.
        canvas.clear(Color::rgb(0, 0, 0x40));
        let band_h = (canvas.height() / 8).max(1);
        canvas.fill_rect(0, 0, canvas.width(), band_h, Color::YELLOW);

        // Self-test: a pure-red probe pixel must read back in the mode's
        // channel order — this fails if the pixel format was misthreaded.
        let (px, py) = (canvas.width() / 2, canvas.height() / 2);
        canvas.put_pixel(px, py, Color::RED);
        let expected = Color::RED.to_pixel_bytes(fb.bytes_per_pixel, fb.pixel_format);
        canvas.read_pixel(px, py) == Some(expected)
    }

    /// Draw the kernel banner text on the framebuffer and self-test it by
    /// reading back pixels the glyph bitmap says must be lit or blank.
    ///
    /// Returns whether the read-back matched — proof the color text console
    /// blits correctly into the live framebuffer. Draws "ENLIL" in white on
    /// the dark-blue field `draw_and_selftest` laid down.
    #[must_use]
    pub fn draw_text_banner(fb: &Framebuffer) -> bool {
        const BANNER: &[u8] = b"ENLIL";
        // SAFETY: `fb` describes the live GOP framebuffer (see draw_and_selftest).
        let Some(mut canvas) = (unsafe { Canvas::from_framebuffer(fb) }) else {
            return false;
        };
        let (tx, ty) = (8, 4);
        let fg = Color::WHITE;
        let bg = Color::rgb(0, 0, 0x40);
        canvas.draw_text(tx, ty, BANNER, fg, bg);

        // The 'E' glyph's top row is 0x7E: column 1 lit, column 0 blank.
        let row0 = font_glyph(b'E')[0];
        glyph_row_bit(row0, 1)
            && !glyph_row_bit(row0, 0)
            && canvas.read_pixel(tx + 1, ty)
                == Some(fg.to_pixel_bytes(fb.bytes_per_pixel, fb.pixel_format))
            && canvas.read_pixel(tx, ty)
                == Some(bg.to_pixel_bytes(fb.bytes_per_pixel, fb.pixel_format))
    }

    /// Bring up the color management console: clear to black, print a colored
    /// boot summary through [`TextConsole`] (forcing at least one scroll),
    /// and verify the blit, the scroll, and the cursor by reading pixels back.
    ///
    /// Returns whether every read-back matched.
    #[must_use]
    pub fn draw_boot_console(fb: &Framebuffer) -> bool {
        // SAFETY: `fb` describes the live GOP framebuffer (see draw_and_selftest).
        let Some(canvas) = (unsafe { Canvas::from_framebuffer(fb) }) else {
            return false;
        };
        let Some(mut console) = TextConsole::new(canvas, Color::WHITE, Color::BLACK) else {
            return false;
        };
        // Absurdly narrow modes would wrap the header and break the scroll
        // accounting below; a real management console is wider than this.
        if console.cols() < 40 {
            return false;
        }
        super::write_console_demo(&mut console);
        super::verify_console_demo(&console, fb.bytes_per_pixel, fb.pixel_format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(_width: u32, height: u32, stride: u32, bpp: u32) -> Vec<u8> {
        vec![0u8; (stride * height * bpp) as usize]
    }

    fn canvas_rgb(buf: &mut [u8], width: u32, height: u32) -> Canvas<'_> {
        Canvas::new(buf, width, height, width, 4, PixelFormat::Rgb).expect("valid geometry")
    }

    /// Expected read-back bytes for `color` on the test canvases (32-bit RGB).
    fn px(color: Color) -> [u8; 4] {
        color.to_pixel_bytes(4, PixelFormat::Rgb)
    }

    #[test]
    fn pixel_offset_honors_stride_and_bounds() {
        let mut buf = geometry(4, 4, 8, 4); // padded stride 8 > width 4
        let canvas = Canvas::new(&mut buf, 4, 4, 8, 4, PixelFormat::Rgb).expect("valid geometry");
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
        let mut canvas = canvas_rgb(&mut buf, 4, 4);
        canvas.put_pixel(2, 1, Color::gray(0x7F));
        assert_eq!(canvas.read_pixel(2, 1), Some([0x7F, 0x7F, 0x7F, 0]));
        // Untouched neighbor stays black.
        assert_eq!(canvas.read_pixel(1, 1), Some([0, 0, 0, 0]));
    }

    #[test]
    fn color_pixel_bytes_honor_channel_order() {
        let red = Color::RED;
        // RGB mode: [r, g, b, reserved].
        assert_eq!(red.to_pixel_bytes(4, PixelFormat::Rgb), [0xFF, 0, 0, 0]);
        // BGR mode: channels swapped.
        assert_eq!(red.to_pixel_bytes(4, PixelFormat::Bgr), [0, 0, 0xFF, 0]);
        // 24-bit modes drop the reserved byte but keep the order.
        assert_eq!(red.to_pixel_bytes(3, PixelFormat::Rgb), [0xFF, 0, 0, 0]);
        assert_eq!(red.to_pixel_bytes(3, PixelFormat::Bgr), [0, 0, 0xFF, 0]);
        // Bitmask has no mask info: documented RGB-order best-effort.
        assert_eq!(red.to_pixel_bytes(4, PixelFormat::Bitmask), [0xFF, 0, 0, 0]);
        // Grayscale stays channel-order independent (equal channels).
        assert_eq!(
            Color::gray(0x7F).to_pixel_bytes(4, PixelFormat::Bgr),
            [0x7F, 0x7F, 0x7F, 0]
        );
    }

    #[test]
    fn put_pixel_writes_channels_in_mode_order() {
        let mut buf = geometry(2, 1, 2, 4);
        let mut rgb = Canvas::new(&mut buf, 2, 1, 2, 4, PixelFormat::Rgb).expect("valid");
        rgb.put_pixel(0, 0, Color::RED);
        rgb.put_pixel(1, 0, Color::BLUE);
        assert_eq!(rgb.read_pixel(0, 0), Some([0xFF, 0, 0, 0]));
        assert_eq!(rgb.read_pixel(1, 0), Some([0, 0, 0xFF, 0]));

        let mut buf = geometry(2, 1, 2, 4);
        let mut bgr = Canvas::new(&mut buf, 2, 1, 2, 4, PixelFormat::Bgr).expect("valid");
        bgr.put_pixel(0, 0, Color::RED);
        bgr.put_pixel(1, 0, Color::BLUE);
        assert_eq!(bgr.read_pixel(0, 0), Some([0, 0, 0xFF, 0]));
        assert_eq!(bgr.read_pixel(1, 0), Some([0xFF, 0, 0, 0]));
    }

    #[test]
    fn off_screen_writes_are_noops() {
        let mut buf = geometry(2, 2, 2, 4);
        let before = buf.clone();
        let mut canvas = canvas_rgb(&mut buf, 2, 2);
        canvas.put_pixel(99, 99, Color::WHITE); // ignored
        assert_eq!(buf, before);
    }

    #[test]
    fn fill_rect_clips_to_the_canvas() {
        let mut buf = geometry(4, 4, 4, 4);
        let mut canvas = canvas_rgb(&mut buf, 4, 4);
        // A rect starting near the edge and running past it fills only the
        // in-bounds part.
        canvas.fill_rect(3, 3, 10, 10, Color::WHITE);
        assert_eq!(canvas.read_pixel(3, 3), Some([255, 255, 255, 0]));
        assert_eq!(canvas.read_pixel(2, 2), Some([0, 0, 0, 0]));
    }

    #[test]
    fn scroll_up_moves_scanlines() {
        let mut buf = geometry(8, 16, 8, 4);
        {
            let mut canvas = canvas_rgb(&mut buf, 8, 16);
            canvas.put_pixel(0, 0, Color::RED); // top row marker
            canvas.put_pixel(0, 8, Color::GREEN); // second text row marker
            canvas.scroll_up(8);
            // Row 1's content moved to row 0; the freed band keeps old bytes.
            assert_eq!(canvas.read_pixel(0, 0), Some([0, 0xFF, 0, 0]));
        }
        // A zero-row scroll is a no-op.
        let before = buf.clone();
        {
            let mut canvas = canvas_rgb(&mut buf, 8, 16);
            canvas.scroll_up(0);
        }
        assert_eq!(buf, before);
    }

    #[test]
    fn scroll_up_handles_padded_stride() {
        // Stride 16 > width 8: the copy must move whole scanlines, padding
        // included, not just the visible pixels.
        let mut buf = geometry(8, 16, 16, 4);
        let mut canvas = Canvas::new(&mut buf, 8, 16, 16, 4, PixelFormat::Rgb).expect("valid");
        canvas.fill_rect(0, 8, 8, 8, Color::BLUE);
        canvas.scroll_up(8);
        assert_eq!(canvas.read_pixel(0, 0), Some([0, 0, 0xFF, 0]));
        assert_eq!(canvas.read_pixel(7, 7), Some([0, 0, 0xFF, 0]));
    }

    #[test]
    fn glyph_row_bit_reads_left_to_right() {
        // 0x80 = only the leftmost pixel; 0x01 = only the rightmost.
        assert!(glyph_row_bit(0x80, 0));
        assert!(!glyph_row_bit(0x80, 1));
        assert!(glyph_row_bit(0x01, 7));
        assert!(!glyph_row_bit(0x01, 0));
        // 0x7E (E's top row) is lit across columns 1..=6, blank at 0 and 7.
        assert!(glyph_row_bit(0x7E, 1));
        assert!(!glyph_row_bit(0x7E, 0));
        assert!(!glyph_row_bit(0x7E, 7));
    }

    #[test]
    fn font_covers_all_printable_ascii() {
        for c in 0x20u8..=0x7E {
            let g = font_glyph(c);
            if c == b' ' {
                assert_eq!(g, [0; GLYPH_HEIGHT], "space must be blank");
            } else {
                assert_ne!(g, [0; GLYPH_HEIGHT], "glyph 0x{c:02X} must not be blank");
            }
        }
        // Controls, DEL, and high bytes render blank.
        for c in [0x00u8, 0x09, 0x0A, 0x1F, 0x7F, 0x80, 0xFF] {
            assert_eq!(font_glyph(c), [0; GLYPH_HEIGHT], "0x{c:02X} must be blank");
        }
    }

    #[test]
    fn cursor_save_buffer_matches_cursor_geometry() {
        assert_eq!(
            CURSOR_SAVE_BYTES,
            (GLYPH_WIDTH as usize) * (CURSOR_HEIGHT as usize) * 4
        );
    }

    #[test]
    fn draw_text_blits_glyph_pixels() {
        // A canvas big enough for one glyph.
        let mut buf = geometry(8, 8, 8, 4);
        let mut canvas = canvas_rgb(&mut buf, 8, 8);
        canvas.draw_text(0, 0, b"E", Color::WHITE, Color::BLACK);
        // E's top row (0x7E): column 1 lit (white), columns 0 and 7 blank.
        assert_eq!(canvas.read_pixel(0, 0), Some([0, 0, 0, 0]));
        assert_eq!(canvas.read_pixel(1, 0), Some([255, 255, 255, 0]));
        assert_eq!(canvas.read_pixel(7, 0), Some([0, 0, 0, 0]));
    }

    #[test]
    fn clear_fills_every_pixel() {
        let mut buf = geometry(3, 3, 3, 4);
        let mut canvas = canvas_rgb(&mut buf, 3, 3);
        canvas.clear(Color::rgb(0x22, 0x11, 0x08));
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(canvas.read_pixel(x, y), Some([0x22, 0x11, 0x08, 0]));
            }
        }
    }

    #[test]
    fn rejects_degenerate_geometry_and_short_buffers() {
        let mut buf = geometry(4, 4, 4, 4);
        assert!(Canvas::new(&mut buf, 4, 4, 4, 0, PixelFormat::Rgb).is_none()); // zero bpp
        assert!(Canvas::new(&mut buf, 0, 4, 4, 4, PixelFormat::Rgb).is_none()); // zero width
        let mut small = vec![0u8; 16];
        assert!(Canvas::new(&mut small, 4, 4, 4, 4, PixelFormat::Rgb).is_none()); // too short
    }

    fn console_2x2(buf: &mut [u8]) -> TextConsole<'_> {
        let canvas = Canvas::new(buf, 16, 16, 16, 4, PixelFormat::Rgb).expect("valid");
        let mut console = TextConsole::new(canvas, Color::WHITE, Color::BLACK).expect("2x2");
        console.clear();
        console
    }

    #[test]
    fn console_rejects_tiny_canvas() {
        let mut buf = geometry(4, 4, 4, 4);
        let canvas = Canvas::new(&mut buf, 4, 4, 4, 4, PixelFormat::Rgb).expect("valid");
        assert!(TextConsole::new(canvas, Color::WHITE, Color::BLACK).is_none());
    }

    #[test]
    fn console_wraps_at_end_of_row() {
        let mut buf = geometry(16, 16, 16, 4);
        let mut console = console_2x2(&mut buf);
        assert_eq!((console.cols(), console.rows()), (2, 2));
        console.write_bytes(b"abcd");
        // "ab" on row 0, "cd" wrapped to row 1.
        assert_eq!(console.cursor(), (2, 1));
        // 'a' row 2 is 0x3C: column 2 lit.
        assert_eq!(console.read_pixel(2, 2), Some(px(Color::WHITE)));
        // 'c' (row 1) row 2 is 0x3C: column 2 lit.
        assert_eq!(console.read_pixel(2, 8 + 2), Some(px(Color::WHITE)));
    }

    #[test]
    fn console_newline_scrolls_at_bottom() {
        // 1 column x 3 rows.
        let mut buf = geometry(8, 24, 8, 4);
        let canvas = Canvas::new(&mut buf, 8, 24, 8, 4, PixelFormat::Rgb).expect("valid");
        let mut console = TextConsole::new(canvas, Color::WHITE, Color::BLACK).expect("1x3");
        console.clear();
        console.write_bytes(b"a\nb\nc\nd\n");
        // Four lines on three rows: 'a' scrolled off, screen shows c, d.
        assert_eq!(console.cursor(), (0, 2));
        // 'c' row 2 is 0x3C: column 2 lit at the top row.
        assert_eq!(console.read_pixel(2, 2), Some(px(Color::WHITE)));
        // 'd' row 0 is 0x02: column 6 lit on the second row.
        assert_eq!(console.read_pixel(6, 8), Some(px(Color::WHITE)));
        // The freed bottom row is background (above the cursor block).
        assert_eq!(console.read_pixel(0, 16), Some(px(Color::BLACK)));
    }

    #[test]
    fn console_backspace_erases_previous_cell() {
        let mut buf = geometry(16, 16, 16, 4);
        let mut console = console_2x2(&mut buf);
        console.write_bytes(b"ab");
        console.write_byte(0x08);
        assert_eq!(console.cursor(), (1, 0));
        // Cell (1,0) blanked; cell (0,0) still 'a'.
        assert_eq!(console.read_pixel(8, 0), Some(px(Color::BLACK)));
        assert_eq!(console.read_pixel(2, 2), Some(px(Color::WHITE))); // 'a' row 2 col 2
        // Backspace at top-left is a no-op, not a wild write.
        console.write_byte(b'\r');
        console.write_byte(0x08);
        assert_eq!(console.cursor(), (0, 0));
        assert_eq!(console.read_pixel(2, 2), Some(px(Color::WHITE)));
    }

    #[test]
    fn console_tab_advances_to_tab_stop() {
        let mut buf = geometry(128, 8, 128, 4);
        let canvas = Canvas::new(&mut buf, 128, 8, 128, 4, PixelFormat::Rgb).expect("valid");
        let mut console = TextConsole::new(canvas, Color::WHITE, Color::BLACK).expect("16x1");
        console.clear();
        console.write_bytes(b"a\tb");
        // 'a' at col 0, tab to col 8, 'b' at col 8.
        assert_eq!(console.cursor(), (9, 0));
        assert_eq!(console.read_pixel(2, 2), Some(px(Color::WHITE))); // 'a'
        assert_eq!(console.read_pixel(8 * 8 + 1, 2), Some(px(Color::WHITE))); // 'b' row 2 col 1
    }

    #[test]
    fn console_ignores_other_control_bytes() {
        let mut buf = geometry(16, 16, 16, 4);
        let mut console = console_2x2(&mut buf);
        console.write_bytes(b"a\x00\x1f\x7fb");
        // Only 'a' and 'b' drew, adjacent.
        assert_eq!(console.cursor(), (2, 0));
        assert_eq!(console.read_pixel(2, 2), Some(px(Color::WHITE))); // 'a'
        assert_eq!(console.read_pixel(8 + 1, 2), Some(px(Color::WHITE))); // 'b' row 2 col 1
    }

    #[test]
    fn console_cursor_restores_glyphs_underneath() {
        let mut buf = geometry(16, 8, 16, 4);
        let canvas = Canvas::new(&mut buf, 16, 8, 16, 4, PixelFormat::Rgb).expect("valid");
        let mut console = TextConsole::new(canvas, Color::WHITE, Color::BLACK).expect("2x1");
        console.clear();
        // 'g' has lit pixels in the cursor's bottom rows (row 7 = 0x3C).
        console.write_bytes(b"g\r");
        // Cursor now sits on the 'g' cell; hiding it must restore the glyph.
        console.set_cursor_visible(false);
        assert!(!console.cursor_visible());
        assert_eq!(console.read_pixel(2, 7), Some(px(Color::WHITE))); // 'g' row 7 col 2 lit
        assert_eq!(console.read_pixel(0, 7), Some(px(Color::BLACK))); // 'g' row 7 col 0 blank
        // Showing it again paints the block; the pixel differs from the glyph.
        console.set_cursor_visible(true);
        assert_eq!(console.read_pixel(0, 7), Some(px(Color::WHITE))); // cursor block
    }

    #[test]
    fn console_implements_fmt_write() {
        use core::fmt::Write as _;
        let mut buf = geometry(16, 16, 16, 4);
        let mut console = console_2x2(&mut buf);
        write!(console, "n={}", 42).expect("fmt write");
        assert_eq!(console.cursor(), (2, 1)); // "n=42": 2 cells, wrap, 2 cells
        // 'n' row 2 is 0x7C: column 1 lit.
        assert_eq!(console.read_pixel(1, 2), Some(px(Color::WHITE)));
    }

    #[test]
    fn boot_console_demo_verifies_on_simulated_gop() {
        // QEMU/OVMF's typical mode: 1280x800, exercised in both channel
        // orders — this is the exact sequence the live firmware self-test
        // runs, minus the raw-pointer framebuffer wrap.
        for format in [PixelFormat::Rgb, PixelFormat::Bgr] {
            let mut buf = vec![0u8; 1280 * 800 * 4];
            let canvas = Canvas::new(&mut buf, 1280, 800, 1280, 4, format).expect("valid");
            let mut console =
                TextConsole::new(canvas, Color::WHITE, Color::BLACK).expect("console");
            assert!(console.cols() >= 40);
            write_console_demo(&mut console);
            assert!(
                verify_console_demo(&console, 4, format),
                "demo verifies for {format:?}"
            );
        }
    }

    #[test]
    fn console_set_colors_applies_to_new_cells() {
        let mut buf = geometry(16, 16, 16, 4);
        let mut console = console_2x2(&mut buf);
        console.write_byte(b'a');
        console.set_colors(Color::GREEN, Color::BLACK);
        console.write_byte(b'b');
        assert_eq!(console.read_pixel(8 + 1, 2), Some(px(Color::GREEN))); // 'b' green
        assert_eq!(console.read_pixel(2, 2), Some(px(Color::WHITE))); // 'a' kept white
    }
}
