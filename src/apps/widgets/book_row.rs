// book list row: rounded background with inverted selection, 64x96
// mini-thumb cover on the left, title next to it, optional trailing
// text on the right ("42%" on home, "312 pages" in the library).
// shared by the home RECENT list and the library list so both
// screens render identically.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};

use crate::drivers::strip::StripBuffer;
use crate::fonts::bitmap::BitmapFont;
use crate::kernel::work_queue::DecodedImage;
use crate::ui::{Alignment, LARGE_MARGIN, Region};

pub const BOOK_ROW_H: u16 = 104;

// mini-thumb cover at the left of the row. matches
// `cover_cache::MINI_THUMB_*`; padding sits inside the row rect so the
// cover blits cleanly against the selection background.
pub const BOOK_ROW_COVER_W: u16 = 64;
pub const BOOK_ROW_COVER_H: u16 = 96;
const COVER_PAD: u16 = 4;
const TEXT_INDENT: u16 = BOOK_ROW_COVER_W + 12;

// gap between the title and the trailing text
const TRAILING_GAP: u16 = 8;
const CORNER: Size = Size::new(4, 4);

pub struct BookRow<'a> {
    region: Region,
    title: &'a str,
    cover: Option<&'a DecodedImage>,
    trailing: Option<&'a str>,
    selected: bool,
}

impl<'a> BookRow<'a> {
    pub fn new(region: Region, title: &'a str) -> Self {
        Self {
            region,
            title,
            cover: None,
            trailing: None,
            selected: false,
        }
    }

    pub fn cover(mut self, cover: Option<&'a DecodedImage>) -> Self {
        self.cover = cover;
        self
    }

    /// Right-aligned text after the title (progress percent, page
    /// count). Empty strings are treated as absent.
    pub fn trailing(mut self, text: &'a str) -> Self {
        if !text.is_empty() {
            self.trailing = Some(text);
        }
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn draw(&self, strip: &mut StripBuffer, font: &'static BitmapFont) {
        let region = self.region;
        let (bg, fg) = if self.selected {
            (BinaryColor::On, BinaryColor::Off)
        } else {
            (BinaryColor::Off, BinaryColor::On)
        };

        let rect = Rectangle::new(
            Point::new(region.x as i32, region.y as i32),
            Size::new(region.w as u32, region.h as u32),
        );
        RoundedRectangle::with_equal_corners(rect, CORNER)
            .into_styled(PrimitiveStyle::with_fill(bg))
            .draw(strip)
            .ok();

        // covers stored by `cover_cache::save_cover_variants` are
        // 1-bit packed; invert the bit when the row is selected so the
        // dark cover renders against the inverted row background.
        let cover_x = region.x + COVER_PAD;
        let cover_y = region.y + COVER_PAD;
        if let Some(img) = self.cover {
            strip.blit_1bpp(
                &img.data,
                0,
                img.width as usize,
                img.height as usize,
                img.stride,
                cover_x as i32,
                cover_y as i32,
                !self.selected,
            );
        } else {
            // empty slot: stroke the box so the row layout reads as
            // intentional and not "missing cover".
            Rectangle::new(
                Point::new(cover_x as i32, cover_y as i32),
                Size::new(BOOK_ROW_COVER_W as u32, BOOK_ROW_COVER_H as u32),
            )
            .into_styled(PrimitiveStyle::with_stroke(fg, 1))
            .draw(strip)
            .ok();
        }

        // title shifts right past the cover; the trailing text (when
        // shown) reserves its measured width on the right so the two
        // never collide.
        let trailing_w = self.trailing.map(|t| font.measure_str(t)).unwrap_or(0);
        let reserve = if trailing_w > 0 {
            trailing_w + LARGE_MARGIN + TRAILING_GAP
        } else {
            LARGE_MARGIN
        };
        let text_left = region.x + COVER_PAD + TEXT_INDENT;
        let title_region = Region::new(
            text_left,
            region.y,
            region.w.saturating_sub(COVER_PAD + TEXT_INDENT + reserve),
            region.h,
        );
        draw_truncated_title(strip, font, title_region, self.title, fg);

        if let Some(text) = self.trailing {
            let trailing_region = Region::new(
                region.x + region.w.saturating_sub(trailing_w + LARGE_MARGIN),
                region.y,
                trailing_w,
                region.h,
            );
            font.draw_aligned(strip, trailing_region, text, Alignment::CenterRight, fg);
        }
    }
}

fn draw_truncated_title(
    strip: &mut StripBuffer,
    font: &BitmapFont,
    region: Region,
    title: &str,
    fg: BinaryColor,
) {
    let cut = font.truncate_len(title, region.w);
    if cut >= title.len() {
        font.draw_aligned(strip, region, title, Alignment::CenterLeft, fg);
        return;
    }

    let mut buf = [0u8; 68];
    let mut n = cut.min(buf.len() - 3);
    while n > 0 && !title.is_char_boundary(n) {
        n -= 1;
    }
    buf[..n].copy_from_slice(&title.as_bytes()[..n]);
    buf[n..n + 3].copy_from_slice("…".as_bytes());
    let truncated = core::str::from_utf8(&buf[..n + 3]).unwrap_or(title);
    font.draw_aligned(strip, region, truncated, Alignment::CenterLeft, fg);
}
