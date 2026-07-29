// deterministic geometric placeholder shapes for book covers.
//
// derived from the book id (FNV-1a of the filename), so the same book
// always gets the same placeholder across boots. six shapes x two
// tones gives 12 distinct combinations - enough to feel non-uniform
// across a library of ~100 books.
//
// pure render function: zero heap, zero SD I/O, no caching. cheap
// enough to redraw every cover on every page change.
//
// later: real cover thumbnails (chunk N or beyond) extend the bundle
// `Covers` section with a thumb-sized variant; Library renders those
// in preference to the placeholder when available.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle, Triangle};

use plump_kernel::ui::{Painter, Region};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeKind {
    Disc,
    Bar,
    Line,
    Ring,
    Diamond,
    Dash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// dark cover, light shape (mockup "dark" tile)
    Dark,
    /// light cover, dark shape (mockup "tan" tile)
    Tan,
}

#[inline]
pub fn placeholder(id: u32) -> (ShapeKind, Tone) {
    let kind = match id % 6 {
        0 => ShapeKind::Disc,
        1 => ShapeKind::Bar,
        2 => ShapeKind::Line,
        3 => ShapeKind::Ring,
        4 => ShapeKind::Diamond,
        _ => ShapeKind::Dash,
    };
    // mix a second bit of the hash into the tone so two adjacent books
    // (different shapes, same tone) don't always feel the same.
    let tone = if (id >> 7) & 1 == 0 {
        Tone::Dark
    } else {
        Tone::Tan
    };
    (kind, tone)
}

/// Draw a placeholder cover at `region`. The strip buffer is borrowed
/// from `painter`; no rounded corners (covers are flat tiles).
pub fn draw_cover(p: &mut Painter<'_>, region: Region, id: u32) {
    let (kind, tone) = placeholder(id);
    let (bg, fg) = match tone {
        Tone::Dark => (BinaryColor::On, BinaryColor::Off),
        Tone::Tan => (BinaryColor::Off, BinaryColor::On),
    };

    let strip = p.strip_mut();
    let rect = Rectangle::new(
        Point::new(region.x as i32, region.y as i32),
        Size::new(region.w as u32, region.h as u32),
    );
    rect.into_styled(PrimitiveStyle::with_fill(bg)).draw(strip).ok();

    // Tan tiles get an outline so they're visible on a cream background.
    if tone == Tone::Tan {
        rect.into_styled(PrimitiveStyle::with_stroke(fg, 1)).draw(strip).ok();
    }

    let cx = region.x + region.w / 2;
    let cy = region.y + region.h / 2;
    let style_fill = PrimitiveStyle::with_fill(fg);
    let style_stroke_thick = PrimitiveStyle::with_stroke(fg, 3);

    match kind {
        ShapeKind::Disc => {
            let r = (region.w.min(region.h) / 4).max(4);
            Circle::new(
                Point::new((cx - r) as i32, (cy - r) as i32),
                (r * 2) as u32,
            )
            .into_styled(style_fill)
            .draw(strip)
            .ok();
        }
        ShapeKind::Bar => {
            let bw = (region.w / 7).max(4);
            let bh = (region.h * 6 / 10).max(8);
            Rectangle::new(
                Point::new((cx - bw / 2) as i32, (cy - bh / 2) as i32),
                Size::new(bw as u32, bh as u32),
            )
            .into_styled(style_fill)
            .draw(strip)
            .ok();
        }
        ShapeKind::Line => {
            let lw = (region.w * 5 / 10).max(8);
            let lh = (region.h / 14).max(3);
            Rectangle::new(
                Point::new((cx - lw / 2) as i32, (cy - lh / 2) as i32),
                Size::new(lw as u32, lh as u32),
            )
            .into_styled(style_fill)
            .draw(strip)
            .ok();
        }
        ShapeKind::Ring => {
            let r = (region.w.min(region.h) / 4).max(4);
            Circle::new(
                Point::new((cx - r) as i32, (cy - r) as i32),
                (r * 2) as u32,
            )
            .into_styled(style_stroke_thick)
            .draw(strip)
            .ok();
        }
        ShapeKind::Diamond => {
            let r = (region.w.min(region.h) / 5).max(4) as i32;
            Triangle::new(
                Point::new(cx as i32, cy as i32 - r),
                Point::new(cx as i32 + r, cy as i32),
                Point::new(cx as i32 - r, cy as i32),
            )
            .into_styled(style_fill)
            .draw(strip)
            .ok();
            Triangle::new(
                Point::new(cx as i32, cy as i32 + r),
                Point::new(cx as i32 + r, cy as i32),
                Point::new(cx as i32 - r, cy as i32),
            )
            .into_styled(style_fill)
            .draw(strip)
            .ok();
        }
        ShapeKind::Dash => {
            let lw = (region.w * 6 / 10).max(8);
            let lh = (region.h / 10).max(4);
            Rectangle::new(
                Point::new((cx - lw / 2) as i32, (cy - lh / 2) as i32),
                Size::new(lw as u32, lh as u32),
            )
            .into_styled(style_fill)
            .draw(strip)
            .ok();
        }
    }
}
