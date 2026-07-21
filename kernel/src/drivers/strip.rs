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

use super::ssd1677::{HEIGHT, Rotation, WIDTH};
use crate::ui::Region;

/// Controls how 2bpp font coverage values map to 1-bit strip pixels.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum GrayMode {
    /// Normal BW: any non-zero coverage → black pixel (bit cleared).
    /// Buffer starts 0xFF (white).
    #[default]
    Bw,
    /// LSB plane (→ BW RAM): coverage values 2 and 3 set bit.
    /// Buffer starts 0x00.
    GrayLsb,
    /// MSB plane (→ RED RAM): coverage values 1 and 2 set bit.
    /// Buffer starts 0x00.
    GrayMsb,
    /// Dual-plane: writes LSB to `buf` and MSB to `gray_buf` in one pass.
    /// Both buffers start 0x00.
    GrayDual,
}

pub const STRIP_ROWS: u16 = 40;
pub const PHYS_BYTES_PER_ROW: usize = (WIDTH as usize) / 8;

pub const STRIP_BUF_SIZE: usize = PHYS_BYTES_PER_ROW * STRIP_ROWS as usize;
pub const STRIP_COUNT: u16 = HEIGHT / STRIP_ROWS;

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
    rotation: Rotation,
    gray_mode: GrayMode,
    win: Region,
    row_bytes: u16,
}

impl StripBuffer {
    pub const fn new() -> Self {
        Self {
            buf: [0xFF; STRIP_BUF_SIZE],
            gray_buf: [0u8; STRIP_BUF_SIZE],
            rotation: Rotation::Deg270,
            gray_mode: GrayMode::Bw,
            win: Region::new(0, 0, WIDTH, STRIP_ROWS),
            row_bytes: (WIDTH / 8),
        }
    }

    pub fn gray_mode(&self) -> GrayMode {
        self.gray_mode
    }

    pub fn set_gray_mode(&mut self, mode: GrayMode) {
        self.gray_mode = mode;
    }

    pub fn begin_strip(&mut self, rotation: Rotation, strip_idx: u16) {
        self.rotation = rotation;
        self.win = Region::new(0, strip_idx * STRIP_ROWS, WIDTH, STRIP_ROWS);
        self.row_bytes = PHYS_BYTES_PER_ROW as u16;

        let fill = if self.gray_mode == GrayMode::Bw {
            0xFF
        } else {
            0x00
        };
        self.buf[..STRIP_BUF_SIZE].fill(fill);
        if self.gray_mode == GrayMode::GrayDual {
            self.gray_buf[..STRIP_BUF_SIZE].fill(0x00);
        }
    }

    pub fn begin_window(&mut self, rotation: Rotation, x: u16, y: u16, w: u16, mut h: u16) {
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

        self.rotation = rotation;
        self.win = Region::new(x, y, w, h);
        self.row_bytes = rb as u16;

        let fill = if self.gray_mode == GrayMode::Bw {
            0xFF
        } else {
            0x00
        };
        self.buf[..total].fill(fill);
        if self.gray_mode == GrayMode::GrayDual {
            self.gray_buf[..total].fill(0x00);
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

    /// Secondary buffer data (MSB plane) for GrayDual mode.
    pub fn gray_data(&self) -> &[u8] {
        let total = self.row_bytes as usize * self.win.h as usize;
        &self.gray_buf[..total]
    }

    pub fn gray_data_mut(&mut self) -> &mut [u8] {
        let total = self.row_bytes as usize * self.win.h as usize;
        &mut self.gray_buf[..total]
    }

    pub fn window(&self) -> Region {
        self.win
    }

    pub fn logical_window(&self) -> Region {
        let w = self.win;
        match self.rotation {
            Rotation::Deg0 => w,
            Rotation::Deg90 => Region::new(w.y, WIDTH - w.x - w.w, w.h, w.w),
            Rotation::Deg180 => Region::new(
                WIDTH - w.x - w.w,
                HEIGHT - w.y - w.h,
                w.w,
                w.h,
            ),
            Rotation::Deg270 => Region::new(HEIGHT - w.y - w.h, w.x, w.h, w.w),
        }
    }

    pub const fn strip_count() -> u16 {
        STRIP_COUNT
    }

    pub fn max_rows_for_width(width: u16) -> u16 {
        let rb = (width / 8) as usize;
        if rb == 0 {
            return 0;
        }
        (STRIP_BUF_SIZE / rb) as u16
    }

    fn to_physical(&self, lx: u16, ly: u16) -> (u16, u16) {
        match self.rotation {
            Rotation::Deg0 => (lx, ly),
            Rotation::Deg90 => (WIDTH - 1 - ly, lx),
            Rotation::Deg180 => (WIDTH - 1 - lx, HEIGHT - 1 - ly),
            Rotation::Deg270 => (ly, HEIGHT - 1 - lx),
        }
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
    }

    /// Set a bit in the secondary gray buffer (for GrayDual generic fallback).
    fn set_gray_pixel_physical(&mut self, px: u16, py: u16) {
        if px < self.win.x || px >= self.win.x + self.win.w {
            return;
        }
        if py < self.win.y || py >= self.win.y + self.win.h {
            return;
        }

        let local_x = (px - self.win.x) as usize;
        let local_y = (py - self.win.y) as usize;
        let idx = (local_x / 8) + (local_y * self.row_bytes as usize);
        self.gray_buf[idx] |= 1 << (7 - (local_x % 8));
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
        // gray passes refresh only AA glyph pixels (blit_2bpp). 1bpp
        // content writes BW polarity, which the gray LUT would read as
        // a drive-gray state; skipping leaves both planes at {0,0} =
        // no change, so the panel keeps the BW-refresh image.
        if self.gray_mode != GrayMode::Bw {
            return;
        }
        match self.rotation {
            Rotation::Deg270 => self.blit_1bpp_270(bitmaps, offset, w, h, stride, gx, gy, black),
            _ => self.blit_1bpp_generic(bitmaps, offset, w, h, stride, gx, gy, black),
        }
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

        for x in c.x0..c.x1 {
            let src_byte_idx = x / 8;
            let src_bit = 1u8 << (7 - (x & 7));
            let dst_row_base = (c.base_buf_y - x) * c.rb;

            if black {
                for y in c.y0..c.y1 {
                    if bitmaps[offset + y * stride + src_byte_idx] & src_bit != 0 {
                        let buf_x = (gy + y as i32 - c.wx) as usize;
                        let byte_col = buf_x / 8;
                        let inv_mask = !(1u8 << (7 - (buf_x & 7)));
                        self.buf[dst_row_base + byte_col] &= inv_mask;
                    }
                }
            } else {
                for y in c.y0..c.y1 {
                    if bitmaps[offset + y * stride + src_byte_idx] & src_bit != 0 {
                        let buf_x = (gy + y as i32 - c.wx) as usize;
                        let byte_col = buf_x / 8;
                        let mask = 1u8 << (7 - (buf_x & 7));
                        self.buf[dst_row_base + byte_col] |= mask;
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn blit_1bpp_generic(
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
        let (lw, lh) = match self.rotation {
            Rotation::Deg0 | Rotation::Deg180 => (WIDTH as i32, HEIGHT as i32),
            Rotation::Deg90 | Rotation::Deg270 => (HEIGHT as i32, WIDTH as i32),
        };

        for y in 0..h {
            let ly = gy + y as i32;
            if ly < 0 || ly >= lh {
                continue;
            }
            let row = offset + y * stride;
            for x in 0..w {
                let lx = gx + x as i32;
                if lx < 0 || lx >= lw {
                    continue;
                }
                if bitmaps[row + x / 8] & (1 << (7 - (x & 7))) != 0 {
                    let (px, py) = self.to_physical(lx as u16, ly as u16);
                    self.set_pixel_physical(px, py, black);
                }
            }
        }
    }

    /// Blit a 2bpp glyph bitmap to the strip buffer.
    ///
    /// Pixel values: 0=white, 1=light gray, 2=dark gray, 3=black.
    /// `black` controls polarity in Bw mode (true=black text, false=white text).
    /// In gray modes, white text (`black == false`) is skipped entirely:
    /// the gray LUT states drive pixels darkward, which would erase
    /// white-on-dark glyphs. Skipping leaves them at {0,0} = no change.
    ///
    /// Behaviour depends on `self.gray_mode`:
    ///   Bw:       any non-zero → set/clear bit per `black` (buffer starts 0xFF)
    ///   GrayLsb:  val >= 2     → set bit in buf   (buffer starts 0x00)
    ///   GrayMsb:  val 1 or 2   → set bit in buf   (buffer starts 0x00)
    ///   GrayDual: val >= 2     → set bit in buf (LSB plane)
    ///             val 1 or 2   → set bit in gray_buf (MSB plane)
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
        if self.gray_mode != GrayMode::Bw && !black {
            return;
        }
        match self.rotation {
            Rotation::Deg270 => self.blit_2bpp_270(bitmaps, offset, w, h, stride, gx, gy, black),
            _ => self.blit_2bpp_generic(bitmaps, offset, w, h, stride, gx, gy, black),
        }
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
        let gray_mode = self.gray_mode;

        for x in c.x0..c.x1 {
            let src_byte_col = x / 4;
            let src_shift = 6 - (x & 3) * 2;
            let dst_row_base = (c.base_buf_y - x) * c.rb;

            match gray_mode {
                GrayMode::Bw => {
                    if black {
                        for y in c.y0..c.y1 {
                            let val = (data[y * stride + src_byte_col] >> src_shift) & 0x03;
                            if val != 0 {
                                let (col, mask) = bit_pos((gy + y as i32 - c.wx) as usize);
                                self.buf[dst_row_base + col] &= !mask;
                            }
                        }
                    } else {
                        for y in c.y0..c.y1 {
                            let val = (data[y * stride + src_byte_col] >> src_shift) & 0x03;
                            if val != 0 {
                                let (col, mask) = bit_pos((gy + y as i32 - c.wx) as usize);
                                self.buf[dst_row_base + col] |= mask;
                            }
                        }
                    }
                }
                GrayMode::GrayLsb => {
                    for y in c.y0..c.y1 {
                        let val = (data[y * stride + src_byte_col] >> src_shift) & 0x03;
                        if val >= 2 {
                            let (col, mask) = bit_pos((gy + y as i32 - c.wx) as usize);
                            self.buf[dst_row_base + col] |= mask;
                        }
                    }
                }
                GrayMode::GrayMsb => {
                    for y in c.y0..c.y1 {
                        let val = (data[y * stride + src_byte_col] >> src_shift) & 0x03;
                        if val == 1 || val == 2 {
                            let (col, mask) = bit_pos((gy + y as i32 - c.wx) as usize);
                            self.buf[dst_row_base + col] |= mask;
                        }
                    }
                }
                GrayMode::GrayDual => {
                    for y in c.y0..c.y1 {
                        let val = (data[y * stride + src_byte_col] >> src_shift) & 0x03;
                        if val == 0 {
                            continue;
                        }
                        let (col, mask) = bit_pos((gy + y as i32 - c.wx) as usize);
                        let idx = dst_row_base + col;
                        if val >= 2 {
                            self.buf[idx] |= mask;
                        }
                        if val <= 2 {
                            self.gray_buf[idx] |= mask;
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn blit_2bpp_generic(
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
        let (lw, lh) = match self.rotation {
            Rotation::Deg0 | Rotation::Deg180 => (WIDTH as i32, HEIGHT as i32),
            Rotation::Deg90 | Rotation::Deg270 => (HEIGHT as i32, WIDTH as i32),
        };

        for y in 0..h {
            let ly = gy + y as i32;
            if ly < 0 || ly >= lh {
                continue;
            }
            let row = offset + y * stride;
            for x in 0..w {
                let lx = gx + x as i32;
                if lx < 0 || lx >= lw {
                    continue;
                }
                let val = (bitmaps[row + x / 4] >> (6 - (x & 3) * 2)) & 0x03;
                if self.gray_mode == GrayMode::GrayDual {
                    if val == 0 {
                        continue;
                    }
                    let (px, py) = self.to_physical(lx as u16, ly as u16);
                    if val >= 2 {
                        self.set_pixel_physical(px, py, false);
                    }
                    if val <= 2 {
                        self.set_gray_pixel_physical(px, py);
                    }
                } else {
                    let should_draw = match self.gray_mode {
                        GrayMode::Bw => val != 0,
                        GrayMode::GrayLsb => val >= 2,
                        GrayMode::GrayMsb => val == 1 || val == 2,
                        GrayMode::GrayDual => unreachable!(),
                    };
                    if should_draw {
                        let (px, py) = self.to_physical(lx as u16, ly as u16);
                        let set_black = black && self.gray_mode == GrayMode::Bw;
                        self.set_pixel_physical(px, py, set_black);
                    }
                }
            }
        }
    }

    fn fill_physical_rect(&mut self, px0: u16, py0: u16, px1: u16, py1: u16, black: bool) {
        let cx0 = px0.max(self.win.x);
        let cx1 = px1.min(self.win.x + self.win.w);
        let cy0 = py0.max(self.win.y);
        let cy1 = py1.min(self.win.y + self.win.h);
        if cx0 >= cx1 || cy0 >= cy1 {
            return;
        }

        let lx0 = (cx0 - self.win.x) as usize;
        let lx1 = (cx1 - self.win.x) as usize;
        let ly0 = (cy0 - self.win.y) as usize;
        let ly1 = (cy1 - self.win.y) as usize;
        let rb = self.row_bytes as usize;

        let first_byte = lx0 / 8;
        let last_byte = (lx1 - 1) / 8;
        let first_mask: u8 = 0xFF >> (lx0 & 7);
        let last_mask: u8 = 0xFF << (7 - ((lx1 - 1) & 7));

        // duplicated per polarity so the edge ops inline (a shared fn
        // pointer defeated devirtualization) and the interior uses a
        // word-wise slice fill instead of per-byte checked stores
        if black {
            for ly in ly0..ly1 {
                let row = ly * rb;
                if first_byte == last_byte {
                    self.buf[row + first_byte] &= !(first_mask & last_mask);
                } else {
                    self.buf[row + first_byte] &= !first_mask;
                    self.buf[row + first_byte + 1..row + last_byte].fill(0x00);
                    self.buf[row + last_byte] &= !last_mask;
                }
            }
        } else {
            for ly in ly0..ly1 {
                let row = ly * rb;
                if first_byte == last_byte {
                    self.buf[row + first_byte] |= first_mask & last_mask;
                } else {
                    self.buf[row + first_byte] |= first_mask;
                    self.buf[row + first_byte + 1..row + last_byte].fill(0xFF);
                    self.buf[row + last_byte] |= last_mask;
                }
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
        match self.rotation {
            Rotation::Deg0 | Rotation::Deg180 => Size::new(WIDTH as u32, HEIGHT as u32),
            Rotation::Deg90 | Rotation::Deg270 => Size::new(HEIGHT as u32, WIDTH as u32),
        }
    }
}

impl DrawTarget for StripBuffer {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        // BW-polarity path; a no-op during gray passes (see blit_1bpp)
        if self.gray_mode != GrayMode::Bw {
            return Ok(());
        }
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
        // BW-polarity path; a no-op during gray passes (see blit_1bpp)
        if self.gray_mode != GrayMode::Bw {
            return Ok(());
        }
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

        match self.rotation {
            Rotation::Deg0 => {
                self.fill_physical_rect(lx0, ly0, lx1, ly1, black);
            }
            Rotation::Deg90 => {
                self.fill_physical_rect(WIDTH - ly1, lx0, WIDTH - ly0, lx1, black);
            }
            Rotation::Deg180 => {
                self.fill_physical_rect(
                    WIDTH - lx1,
                    HEIGHT - ly1,
                    WIDTH - lx0,
                    HEIGHT - ly0,
                    black,
                );
            }
            Rotation::Deg270 => {
                self.fill_physical_rect(ly0, HEIGHT - lx1, ly1, HEIGHT - lx0, black);
            }
        }
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        // BW-polarity path; a no-op during gray passes (see blit_1bpp)
        if self.gray_mode != GrayMode::Bw {
            return Ok(());
        }
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
