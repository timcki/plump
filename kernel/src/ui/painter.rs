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
    primitives::{PrimitiveStyle, Rectangle},
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

    /// Re-borrow with a different foreground color (same clip).
    pub fn with_fg(&mut self, fg: BinaryColor) -> Painter<'_> {
        Painter {
            strip: &mut *self.strip,
            region: self.region,
            fg,
            theme: self.theme,
        }
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
        let r = clamp(self.region, r);
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

    /// Rounded rectangle, stroked or filled in the foreground color.
    ///
    /// On 1-bit e-paper at small radii the corners are barely visible;
    /// `radius == 0` falls through to a sharp rect for free.
    pub fn rounded_rect(&mut self, r: Region, radius: u16, fill: bool) {
        use embedded_graphics::primitives::RoundedRectangle;
        if r.w == 0 || r.h == 0 {
            return;
        }
        let rect = Rectangle::new(r.top_left(), Size::new(r.w as u32, r.h as u32));
        let style = if fill {
            PrimitiveStyle::with_fill(self.fg)
        } else {
            PrimitiveStyle::with_stroke(self.fg, self.theme.panel_stroke as u32)
        };
        let _ = RoundedRectangle::with_equal_corners(rect, Size::new(radius as u32, radius as u32))
            .into_styled(style)
            .draw(self.strip);
    }

    /// True if `r` overlaps the current clip. Cheap pre-check for
    /// widgets that want to skip work when fully clipped out.
    #[inline]
    pub fn intersects(&self, r: Region) -> bool {
        self.region.intersects(r)
    }
}

#[inline]
fn clamp(parent: Region, child: Region) -> Region {
    let x0 = parent.x.max(child.x);
    let y0 = parent.y.max(child.y);
    let x1 = (parent.x + parent.w).min(child.x + child.w);
    let y1 = (parent.y + parent.h).min(child.y + child.h);
    if x1 <= x0 || y1 <= y0 {
        return Region::new(x0, y0, 0, 0);
    }
    Region::new(x0, y0, x1 - x0, y1 - y0)
}
