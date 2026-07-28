// region geometry, alignment helpers, progress bar, loading indicator

use embedded_graphics::{
    mono_font::MonoTextStyle, mono_font::ascii::FONT_9X18, pixelcolor::BinaryColor, prelude::*,
    primitives::PrimitiveStyle, primitives::Rectangle, text::Text,
};

use crate::drivers::strip::StripBuffer;
use crate::ui::stack_fmt::BorrowedFmt;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Region {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Region {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn to_rect(self) -> Rectangle {
        Rectangle::new(
            Point::new(self.x as i32, self.y as i32),
            Size::new(self.w as u32, self.h as u32),
        )
    }

    pub fn top_left(self) -> Point {
        Point::new(self.x as i32, self.y as i32)
    }

    pub fn align8(self) -> Self {
        let aligned_x = (self.x / 8) * 8;
        let extra = self.x - aligned_x;
        Self {
            x: aligned_x,
            y: self.y,
            w: (self.w + extra).div_ceil(8) * 8,
            h: self.h,
        }
    }

    /// Round both axes outward to multiples of 8. The panel windows on
    /// byte boundaries in physical coordinates, and rotation swaps the
    /// axes, so snapping both is what guarantees an unmasked window
    /// whatever the rotation.
    pub fn align8_xy(self) -> Self {
        let ax = (self.x / 8) * 8;
        let ay = (self.y / 8) * 8;
        Self {
            x: ax,
            y: ay,
            w: (self.w + (self.x - ax)).div_ceil(8) * 8,
            h: (self.h + (self.y - ay)).div_ceil(8) * 8,
        }
    }

    pub fn union(self, other: Region) -> Self {
        let x1 = self.x.min(other.x);
        let y1 = self.y.min(other.y);
        let x2 = (self.x + self.w).max(other.x + other.w);
        let y2 = (self.y + self.h).max(other.y + other.h);
        Self {
            x: x1,
            y: y1,
            w: x2 - x1,
            h: y2 - y1,
        }
    }

    /// Overlap of two regions, None when they do not intersect.
    pub fn intersection(self, other: Region) -> Option<Region> {
        if !self.intersects(other) {
            return None;
        }
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = (self.x + self.w).min(other.x + other.w);
        let y2 = (self.y + self.h).min(other.y + other.h);
        Some(Self {
            x: x1,
            y: y1,
            w: x2 - x1,
            h: y2 - y1,
        })
    }

    pub fn intersects(self, other: Region) -> bool {
        self.x < other.x + other.w
            && self.x + self.w > other.x
            && self.y < other.y + other.h
            && self.y + self.h > other.y
    }

    /// True when `other` lies entirely inside `self`; an empty `other`
    /// is contained by anything.
    pub fn contains(self, other: Region) -> bool {
        other.w == 0
            || other.h == 0
            || (other.x >= self.x
                && other.y >= self.y
                && other.x + other.w <= self.x + self.w
                && other.y + other.h <= self.y + self.h)
    }
}

/// A [`Region`] whose x, y, w and h are all multiples of 8.
///
/// The panel windows on byte boundaries in physical coordinates and
/// rotation swaps the axes, so a region snapped on both logical axes
/// maps to an unmasked physical window under every rotation. Making
/// the snap part of the type removes the per-call-site choice between
/// [`Region::align8`] and [`Region::align8_xy`] that previously let
/// partial windows carry edge masks (and let the mask slop park fake
/// plane state over neighbouring rows).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlignedRegion(Region);

impl AlignedRegion {
    /// Snap `r` outward to the nearest byte boundaries.
    pub fn snap(r: Region) -> Self {
        Self(r.align8_xy())
    }

    /// Wrap a region that is already aligned; asserts the invariant,
    /// for compile-time constants like the full screen.
    pub const fn from_aligned(r: Region) -> Self {
        assert!(
            r.x.is_multiple_of(8)
                && r.y.is_multiple_of(8)
                && r.w.is_multiple_of(8)
                && r.h.is_multiple_of(8)
        );
        Self(r)
    }

    #[inline]
    pub fn get(self) -> Region {
        self.0
    }

    /// Overlap of two aligned regions; max/min of multiples of 8 stay
    /// multiples of 8, so the result needs no re-snap.
    pub fn intersection(self, other: Self) -> Option<Self> {
        self.0.intersection(other.0).map(Self)
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0.union(other.0))
    }

    #[inline]
    pub fn intersects(self, other: Self) -> bool {
        self.0.intersects(other.0)
    }

    #[inline]
    pub fn contains(self, other: Self) -> bool {
        self.0.contains(other.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Alignment {
    #[default]
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl Alignment {
    pub fn position(self, region: Region, content_size: Size) -> Point {
        let cw = content_size.width as i32;
        let ch = content_size.height as i32;
        let rx = region.x as i32;
        let ry = region.y as i32;
        let rw = region.w as i32;
        let rh = region.h as i32;

        match self {
            Alignment::TopLeft => Point::new(rx, ry),
            Alignment::TopCenter => Point::new(rx + (rw - cw) / 2, ry),
            Alignment::TopRight => Point::new(rx + rw - cw, ry),
            Alignment::CenterLeft => Point::new(rx, ry + (rh - ch) / 2),
            Alignment::Center => Point::new(rx + (rw - cw) / 2, ry + (rh - ch) / 2),
            Alignment::CenterRight => Point::new(rx + rw - cw, ry + (rh - ch) / 2),
            Alignment::BottomLeft => Point::new(rx, ry + rh - ch),
            Alignment::BottomCenter => Point::new(rx + (rw - cw) / 2, ry + rh - ch),
            Alignment::BottomRight => Point::new(rx + rw - cw, ry + rh - ch),
        }
    }
}

#[inline]
pub fn wrap_next(current: usize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    if current + 1 >= count { 0 } else { current + 1 }
}

#[inline]
pub fn wrap_prev(current: usize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    if current == 0 { count - 1 } else { current - 1 }
}

/// Horizontal progress bar for 1-bit e-paper.
///
/// Draws a 1px black border around the full track and fills
/// proportionally from the left; `pct` is clamped to 0..=100.
/// Region should be at least 4px wide and 4px tall.
pub struct ProgressBar {
    pub region: Region,
    pub pct: u8,
}

impl ProgressBar {
    pub const fn new(region: Region, pct: u8) -> Self {
        Self { region, pct }
    }

    pub fn draw(&self, strip: &mut StripBuffer) {
        let pct = self.pct.min(100) as u32;

        // clear region
        self.region
            .to_rect()
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
            .draw(strip)
            .unwrap();

        // 1px border shows full extent even at 0%
        self.region
            .to_rect()
            .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
            .draw(strip)
            .unwrap();

        // filled portion inside the border
        if pct > 0 && self.region.w > 2 && self.region.h > 2 {
            let inner_w = (self.region.w - 2) as u32;
            let fill_w = (inner_w * pct / 100).max(1);
            Rectangle::new(
                Point::new((self.region.x + 1) as i32, (self.region.y + 1) as i32),
                Size::new(fill_w, (self.region.h - 2) as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(strip)
            .unwrap();
        }
    }
}

/// Loading indicator for 1-bit e-paper.
///
/// Draws `"msg...pct%"` centered vertically in the region using the
/// built-in FONT_9X18 mono font; works without any custom bitmap
/// fonts loaded, usable from any app or the kernel itself.
///
/// ```text
/// LoadingIndicator::new(region, "Loading", 25)       => "Loading...25%"
/// LoadingIndicator::new(region, "Caching 3/15", 20)  => "Caching 3/15...20%"
/// ```
pub struct LoadingIndicator<'a> {
    pub region: Region,
    pub msg: &'a str,
    pub pct: u8,
}

impl<'a> LoadingIndicator<'a> {
    pub const fn new(region: Region, msg: &'a str, pct: u8) -> Self {
        Self { region, msg, pct }
    }

    pub fn draw(&self, strip: &mut StripBuffer) {
        use core::fmt::Write;

        // clear region
        self.region
            .to_rect()
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
            .draw(strip)
            .unwrap();

        // format "msg...pct%"
        let mut buf = [0u8; 48];
        let mut fmt = BorrowedFmt::new(&mut buf);
        let _ = write!(fmt, "{}...{}%", self.msg, self.pct.min(100));
        let text = fmt.as_str();

        // FONT_9X18: 9px wide, 18px tall, ~14px ascent
        // center vertically; baseline = region.y + (h + 9) / 2
        let style = MonoTextStyle::new(&FONT_9X18, BinaryColor::On);
        let baseline_y = self.region.y as i32 + (self.region.h as i32 + 9) / 2;
        Text::new(text, Point::new(self.region.x as i32 + 2, baseline_y), style)
            .draw(strip)
            .unwrap();
    }
}
