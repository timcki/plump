// painter: font-agnostic drawing wrapper around StripBuffer.
//
// bundles the strip buffer, an active region (clip rect), a foreground
// color, and a reference to the theme. widgets call painter methods
// to draw primitives; child painters reborrow with narrower clip
// regions for nested layouts.
//
// text drawing lives in the distro because fonts are distro-owned
// (BitmapFont is in src/fonts/). painter exposes only raw geometry
// and blit primitives; widgets that need glyphs combine a Painter
// with a font.
//
// chrome widgets take a single Painter and never touch StripBuffer
// directly.

use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::Rectangle,
};

use crate::drivers::strip::StripBuffer;
use crate::ui::theme::Theme;
use crate::ui::Region;

pub struct Painter<'a> {
    strip: &'a mut StripBuffer,
    region: Region,
    fg: BinaryColor,
    theme: &'a Theme,
}

impl<'a> Painter<'a> {
    /// Construct a painter with the full screen region as its clip.
    ///
    /// Callers (scheduler) typically pass `BinaryColor::On` and the
    /// kernel-owned theme.
    pub fn new(strip: &'a mut StripBuffer, theme: &'a Theme) -> Self {
        Self {
            strip,
            region: Region::new(0, 0, crate::board::SCREEN_W, crate::board::SCREEN_H),
            fg: BinaryColor::On,
            theme,
        }
    }

    #[inline]
    pub fn region(&self) -> Region {
        self.region
    }

    #[inline]
    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// Direct mutable access to the underlying strip buffer. Use
    /// sparingly; widgets that need glyph blits should call this and
    /// then go through `StripBuffer`'s blit_* methods with their own
    /// font tables.
    #[inline]
    pub fn strip_mut(&mut self) -> &mut StripBuffer {
        self.strip
    }

    // raw drawing primitives

    /// Fill an arbitrary sub-region with a specific color. Region is
    /// clamped to the current clip before drawing.
    pub fn fill_in(&mut self, r: Region, color: BinaryColor) {
        let r = self.region.clip(r);
        if r.w == 0 || r.h == 0 {
            return;
        }
        let _ = self
            .strip
            .fill_solid(&Rectangle::new(r.top_left(), Size::new(r.w as u32, r.h as u32)), color);
    }

    /// 1 px horizontal line at row `y`, from `x0` (inclusive) to `x1`
    /// (exclusive), in the foreground color.
    pub fn hairline_h(&mut self, y: u16, x0: u16, x1: u16) {
        if x1 <= x0 {
            return;
        }
        let r = Region::new(x0, y, x1 - x0, 1);
        self.fill_in(r, self.fg);
    }

    /// True if `r` overlaps the current clip. Cheap pre-check for
    /// widgets that want to skip work when fully clipped out.
    #[inline]
    pub fn intersects(&self, r: Region) -> bool {
        self.region.intersects(r)
    }
}
