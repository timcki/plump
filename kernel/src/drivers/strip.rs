// strip-based rendering buffer for e-paper
// 4 KB strip instead of 48 KB framebuffer; display split into horizontal bands
// widgets draw to logical coords, clipped here

use embedded_graphics_core::{
    Pixel,
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Size},
    pixelcolor::BinaryColor,
    primitives::Rectangle,
};

use super::ssd1677::{HEIGHT, WIDTH};
use crate::ui::Region;

/// Controls how 2bpp font coverage values map to 1-bit strip pixels.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum GrayMode {
    /// Normal BW: any non-zero coverage → black pixel (bit cleared).
    /// Buffer starts 0xFF (white).
    #[default]
    Bw,
    /// Dual-plane: writes LSB to `buf` and MSB to `gray_buf` in one pass.
    /// Both buffers start 0x00.
    GrayDual,
}

pub const STRIP_ROWS: u16 = 40;
pub const PHYS_BYTES_PER_ROW: usize = (WIDTH as usize) / 8;

pub const STRIP_BUF_SIZE: usize = PHYS_BYTES_PER_ROW * STRIP_ROWS as usize;

/// byte offset and bit mask for a pixel at the given x position in a strip row
#[inline(always)]
fn bit_pos(buf_x: usize) -> (usize, u8) {
    (buf_x / 8, 1u8 << (7 - (buf_x & 7)))
}

/// pre-computed clipping bounds for 270° rotation blit
struct Clip270 {
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    base_buf_y: usize,
    rb: usize,
    wx: i32,
}

/// clip a glyph at (gx, gy) of size (w, h) against the physical window
/// for 270° rotation. returns None if fully clipped.
fn clip_270(win: &Region, row_bytes: u16, gx: i32, gy: i32, w: usize, h: usize) -> Option<Clip270> {
    let wx = win.x as i32;
    let wy = win.y as i32;
    let wx2 = wx + win.w as i32;
    let wy2 = wy + win.h as i32;
    let rb = row_bytes as usize;

    // clip glyph rows (y axis) against physical-x window
    let y0 = (wx - gy).clamp(0, h as i32) as usize;
    let y1 = (wx2 - gy).clamp(0, h as i32) as usize;
    if y0 >= y1 {
        return None;
    }

    // clip glyph cols (x axis) against physical-y window
    let x0 = (HEIGHT as i32 - gx - wy2).clamp(0, w as i32) as usize;
    let x1 = (HEIGHT as i32 - gx - wy).clamp(0, w as i32) as usize;
    if x0 >= x1 {
        return None;
    }

    let base_buf_y = (HEIGHT as i32 - 1 - gx - wy) as usize;
    Some(Clip270 { x0, x1, y0, y1, base_buf_y, rb, wx })
}

pub struct StripBuffer {
    buf: [u8; STRIP_BUF_SIZE],
    // secondary buffer for GrayDual mode (MSB plane)
    gray_buf: [u8; STRIP_BUF_SIZE],
    gray_mode: GrayMode,
    win: Region,
    row_bytes: u16,
}

impl StripBuffer {
    pub const fn new() -> Self {
        Self {
            buf: [0xFF; STRIP_BUF_SIZE],
            gray_buf: [0u8; STRIP_BUF_SIZE],
            gray_mode: GrayMode::Bw,
            win: Region::new(0, 0, WIDTH, STRIP_ROWS),
            row_bytes: (WIDTH / 8),
        }
    }

    pub fn gray_mode(&self) -> GrayMode {
        self.gray_mode
    }

    /// Run `f` with the buffer in [`GrayMode::GrayDual`], restoring
    /// [`GrayMode::Bw`] afterwards.
    ///
    /// The pairing is the bracket's, not the caller's: a forgotten
    /// restore leaves every later BW frame drawing into the gray
    /// planes, which the panel then reads as drive-gray states.
    pub fn with_gray_dual<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.gray_mode = GrayMode::GrayDual;
        let r = f(self);
        self.gray_mode = GrayMode::Bw;
        r
    }

    pub fn begin_window(&mut self, x: u16, y: u16, w: u16, mut h: u16) {
        let rb = (w / 8) as usize;
        if rb == 0 {
            self.win = Region::new(x, y, 0, 0);
            self.row_bytes = 0;
            return;
        }
        let max_h = (STRIP_BUF_SIZE / rb) as u16;
        if h > max_h {
            log::warn!(
                "begin_window: {}x{} exceeds strip buf, clamping h -> {}",
                w,
                h,
                max_h
            );
            h = max_h;
        }
        let total = rb * h as usize;

        self.win = Region::new(x, y, w, h);
        self.row_bytes = rb as u16;

        // white content in every plane in use. GrayDual keeps both
        // planes equal to the content except at AA edge pixels, where
        // `buf` (BW RAM) is cleared to mark the edge; see blit_2bpp
        self.buf[..total].fill(0xFF);
        if self.gray_mode == GrayMode::GrayDual {
            self.gray_buf[..total].fill(0xFF);
        }
    }

    pub fn data(&self) -> &[u8] {
        let total = self.row_bytes as usize * self.win.h as usize;
        &self.buf[..total]
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        let total = self.row_bytes as usize * self.win.h as usize;
        &mut self.buf[..total]
    }

    /// The content plane in GrayDual mode (goes to RED RAM).
    pub fn gray_data(&self) -> &[u8] {
        let total = self.row_bytes as usize * self.win.h as usize;
        &self.gray_buf[..total]
    }

    pub fn logical_window(&self) -> Region {
        let w = self.win;
        Region::new(HEIGHT - w.y - w.h, w.x, w.h, w.w)
    }

    pub fn max_rows_for_width(width: u16) -> u16 {
        let rb = (width / 8) as usize;
        if rb == 0 {
            return 0;
        }
        (STRIP_BUF_SIZE / rb) as u16
    }

    fn to_physical(&self, lx: u16, ly: u16) -> (u16, u16) {
        (ly, HEIGHT - 1 - lx)
    }

    fn set_pixel_physical(&mut self, px: u16, py: u16, black: bool) {
        if px < self.win.x || px >= self.win.x + self.win.w {
            return;
        }
        if py < self.win.y || py >= self.win.y + self.win.h {
            return;
        }

        let local_x = (px - self.win.x) as usize;
        let local_y = (py - self.win.y) as usize;
        let idx = (local_x / 8) + (local_y * self.row_bytes as usize);
        let bit = 7 - (local_x as u16 % 8);

        if black {
            self.buf[idx] &= !(1 << bit);
        } else {
            self.buf[idx] |= 1 << bit;
        }
        // a gray pass keeps the content in both planes: every 1-bit
        // primitive writes RED too, so after the revert RED holds
        // exactly what the panel shows and the next refresh is a delta
        if self.gray_mode == GrayMode::GrayDual {
            if black {
                self.gray_buf[idx] &= !(1 << bit);
            } else {
                self.gray_buf[idx] |= 1 << bit;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn blit_1bpp(
        &mut self,
        bitmaps: &[u8],
        offset: usize,
        w: usize,
        h: usize,
        stride: usize,
        gx: i32,
        gy: i32,
        black: bool,
    ) {
        if w == 0 || h == 0 || offset + stride * h > bitmaps.len() {
            return;
        }
        self.blit_1bpp_270(bitmaps, offset, w, h, stride, gx, gy, black)
    }

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn blit_1bpp_270(
        &mut self,
        bitmaps: &[u8],
        offset: usize,
        w: usize,
        h: usize,
        stride: usize,
        gx: i32,
        gy: i32,
        black: bool,
    ) {
        let c = match clip_270(&self.win, self.row_bytes, gx, gy, w, h) {
            Some(c) => c,
            None => return,
        };
        let dual = self.gray_mode == GrayMode::GrayDual;

        // hottest loop in the firmware. per destination byte we
        // accumulate up to 8 source pixels into one mask and apply it
        // with a single read-modify-write; the source index walks by
        // += stride (the bounds check blocks the compiler from doing
        // this strength reduction itself), and the destination row is
        // borrowed once per column instead of bounds-checked per pixel
        for x in c.x0..c.x1 {
            let src_bit = 1u8 << (7 - (x & 7));
            let mut src_idx = offset + c.y0 * stride + x / 8;
            let dst_row_base = (c.base_buf_y - x) * c.rb;
            let row = &mut self.buf[dst_row_base..dst_row_base + c.rb];
            let gray_row = &mut self.gray_buf[dst_row_base..dst_row_base + c.rb];

            let mut buf_x = (gy + c.y0 as i32 - c.wx) as usize;
            let mut y = c.y0;
            while y < c.y1 {
                let bit0 = buf_x & 7;
                let byte_col = buf_x >> 3;
                // consecutive glyph rows land in consecutive bits of
                // the same destination byte
                let group = (8 - bit0).min(c.y1 - y);
                let mut acc: u8 = 0;
                for k in 0..group {
                    if bitmaps[src_idx] & src_bit != 0 {
                        acc |= 0x80 >> (bit0 + k);
                    }
                    src_idx += stride;
                }
                if acc != 0 {
                    if black {
                        row[byte_col] &= !acc;
                        if dual {
                            gray_row[byte_col] &= !acc;
                        }
                    } else {
                        row[byte_col] |= acc;
                        if dual {
                            gray_row[byte_col] |= acc;
                        }
                    }
                }
                buf_x += group;
                y += group;
            }
        }
    }

    /// Blit a 2bpp glyph bitmap to the strip buffer.
    ///
    /// Pixel values: 0=white, 1=light gray, 2=dark gray, 3=black.
    /// `black` controls polarity (true=black text, false=white text).
    ///
    /// Bw: any non-zero value is ink, set/cleared per `black`.
    ///
    /// GrayDual: RED takes the pixel as the panel will hold it after
    /// the revert and BW the same bit, except at the one AA state (see
    /// the driver's `LUT_GRAYSCALE`): val 1 is the lifted edge, white
    /// in RED and 0 in BW; val 2 and 3 are black in both. White-on-
    /// dark text gets no gray waveform (the gray state only lifts from
    /// black) and is written as white in both.
    #[allow(clippy::too_many_arguments)]
    pub fn blit_2bpp(
        &mut self,
        bitmaps: &[u8],
        offset: usize,
        w: usize,
        h: usize,
        stride: usize,
        gx: i32,
        gy: i32,
        black: bool,
    ) {
        if w == 0 || h == 0 || offset + stride * h > bitmaps.len() {
            return;
        }
        self.blit_2bpp_270(bitmaps, offset, w, h, stride, gx, gy, black)
    }

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn blit_2bpp_270(
        &mut self,
        bitmaps: &[u8],
        offset: usize,
        w: usize,
        h: usize,
        stride: usize,
        gx: i32,
        gy: i32,
        black: bool,
    ) {
        let c = match clip_270(&self.win, self.row_bytes, gx, gy, w, h) {
            Some(c) => c,
            None => return,
        };

        let data = &bitmaps[offset..];

        // shared clip walk with the mode match hoisted out of the
        // column loop: each arm gets a specialized loop pair. the
        // source index walks by += stride (strength reduction the
        // bounds check otherwise blocks); val == 0 never draws in any
        // mode, so the skip lives in the shared shell
        macro_rules! walk {
            (|$val:ident, $idx:ident, $mask:ident| $body:block) => {
                for x in c.x0..c.x1 {
                    let src_byte_col = x / 4;
                    let src_shift = 6 - (x & 3) * 2;
                    let dst_row_base = (c.base_buf_y - x) * c.rb;
                    let mut src_idx = c.y0 * stride + src_byte_col;
                    let mut buf_x = (gy + c.y0 as i32 - c.wx) as usize;
                    for _y in c.y0..c.y1 {
                        let $val = (data[src_idx] >> src_shift) & 0x03;
                        if $val != 0 {
                            let (col, $mask) = bit_pos(buf_x);
                            let $idx = dst_row_base + col;
                            $body
                        }
                        src_idx += stride;
                        buf_x += 1;
                    }
                }
            };
        }

        match self.gray_mode {
            GrayMode::Bw => {
                if black {
                    walk!(|val, idx, mask| {
                        self.buf[idx] &= !mask;
                    });
                } else {
                    walk!(|val, idx, mask| {
                        self.buf[idx] |= mask;
                    });
                }
            }
            GrayMode::GrayDual => {
                if black {
                    walk!(|val, idx, mask| {
                        // val 1 is the lifted edge: white in RED, 0 in
                        // BW. val 2 and 3 are ink in both planes and
                        // take no waveform (one would lighten the body
                        // of the glyph on every pass)
                        if val == 1 {
                            self.gray_buf[idx] |= mask;
                            self.buf[idx] &= !mask;
                        } else {
                            self.gray_buf[idx] &= !mask;
                            self.buf[idx] &= !mask;
                        }
                    });
                } else {
                    walk!(|_val, idx, mask| {
                        self.gray_buf[idx] |= mask;
                        self.buf[idx] |= mask;
                    });
                }
            }
        }
    }

    /// Flatten a logical region in whatever mode the strip is in: a
    /// plain fill, written to both planes in GrayDual, so nothing
    /// blitted underneath earlier in the same pass keeps a gray
    /// waveform. overlays painted over gray content (the sleep card
    /// over the wallpaper) call this before drawing.
    pub fn fill_flat(&mut self, r: Region, black: bool) {
        let size = self.size();
        let sw = size.width as u16;
        let sh = size.height as u16;
        let lx0 = r.x.min(sw);
        let ly0 = r.y.min(sh);
        let lx1 = r.x.saturating_add(r.w).min(sw);
        let ly1 = r.y.saturating_add(r.h).min(sh);
        if lx0 >= lx1 || ly0 >= ly1 {
            return;
        }
        let (px0, py0, px1, py1) = (ly0, HEIGHT - lx1, ly1, HEIGHT - lx0);
        self.fill_physical_rect(px0, py0, px1, py1, black);
    }

    // local (window-relative) pixel bounds of a physical rect, None
    // when it misses the current strip window
    fn clip_physical(
        &self,
        px0: u16,
        py0: u16,
        px1: u16,
        py1: u16,
    ) -> Option<(usize, usize, usize, usize)> {
        let c = Region::new(px0, py0, px1 - px0, py1 - py0).clip(self.win);
        if c.w == 0 || c.h == 0 {
            return None;
        }
        Some((
            (c.x - self.win.x) as usize,
            (c.x + c.w - self.win.x) as usize,
            (c.y - self.win.y) as usize,
            (c.y + c.h - self.win.y) as usize,
        ))
    }

    fn fill_physical_rect(&mut self, px0: u16, py0: u16, px1: u16, py1: u16, black: bool) {
        let Some((lx0, lx1, ly0, ly1)) = self.clip_physical(px0, py0, px1, py1) else {
            return;
        };
        let rb = self.row_bytes as usize;
        fill_bits(&mut self.buf, rb, lx0, lx1, ly0, ly1, !black);
        if self.gray_mode == GrayMode::GrayDual {
            fill_bits(&mut self.gray_buf, rb, lx0, lx1, ly0, ly1, !black);
        }
    }
}

// set or clear every bit of a local pixel rect in a row-major 1bpp
// buffer. duplicated per polarity so the edge ops inline (a shared fn
// pointer defeated devirtualization) and the interior uses a
// word-wise slice fill instead of per-byte checked stores
fn fill_bits(buf: &mut [u8], rb: usize, lx0: usize, lx1: usize, ly0: usize, ly1: usize, set: bool) {
    let first_byte = lx0 / 8;
    let last_byte = (lx1 - 1) / 8;
    let first_mask: u8 = 0xFF >> (lx0 & 7);
    let last_mask: u8 = 0xFF << (7 - ((lx1 - 1) & 7));

    if set {
        for ly in ly0..ly1 {
            let row = ly * rb;
            if first_byte == last_byte {
                buf[row + first_byte] |= first_mask & last_mask;
            } else {
                buf[row + first_byte] |= first_mask;
                buf[row + first_byte + 1..row + last_byte].fill(0xFF);
                buf[row + last_byte] |= last_mask;
            }
        }
    } else {
        for ly in ly0..ly1 {
            let row = ly * rb;
            if first_byte == last_byte {
                buf[row + first_byte] &= !(first_mask & last_mask);
            } else {
                buf[row + first_byte] &= !first_mask;
                buf[row + first_byte + 1..row + last_byte].fill(0x00);
                buf[row + last_byte] &= !last_mask;
            }
        }
    }
}

impl Default for StripBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl OriginDimensions for StripBuffer {
    fn size(&self) -> Size {
        Size::new(HEIGHT as u32, WIDTH as u32)
    }
}

impl DrawTarget for StripBuffer {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let size = self.size();
        let log_w = size.width as i32;
        let log_h = size.height as i32;

        for Pixel(coord, color) in pixels {
            if coord.x < 0 || coord.x >= log_w || coord.y < 0 || coord.y >= log_h {
                continue;
            }

            let (px, py) = self.to_physical(coord.x as u16, coord.y as u16);
            self.set_pixel_physical(px, py, color == BinaryColor::On);
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let size = self.size();
        let sw = size.width as u16;
        let sh = size.height as u16;

        let lx0 = (area.top_left.x.max(0) as u16).min(sw);
        let ly0 = (area.top_left.y.max(0) as u16).min(sh);
        let lx1 = ((area.top_left.x.saturating_add(area.size.width as i32)).max(0) as u16).min(sw);
        let ly1 = ((area.top_left.y.saturating_add(area.size.height as i32)).max(0) as u16).min(sh);
        if lx0 >= lx1 || ly0 >= ly1 {
            return Ok(());
        }

        let black = color == BinaryColor::On;

        self.fill_physical_rect(ly0, HEIGHT - lx1, ly1, HEIGHT - lx0, black);
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        let w = area.size.width as i32;
        if w == 0 {
            return Ok(());
        }
        let mut x = area.top_left.x;
        let mut y = area.top_left.y;
        let x_end = x + w;
        let size = self.size();
        let log_w = size.width as i32;
        let log_h = size.height as i32;

        for color in colors {
            if x >= 0 && x < log_w && y >= 0 && y < log_h {
                let (px, py) = self.to_physical(x as u16, y as u16);
                self.set_pixel_physical(px, py, color == BinaryColor::On);
            }
            x += 1;
            if x >= x_end {
                x = area.top_left.x;
                y += 1;
            }
        }
        Ok(())
    }
}
