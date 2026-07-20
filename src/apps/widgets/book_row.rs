// book list row: rounded background with inverted selection, 64x96
// mini-thumb cover on the left, title next to it, optional progress
// percent on the right. shared by the home RECENT list and the
// library list so both screens render identically.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};

use crate::apps::widgets::BitmapDynLabel;
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

// width reserved on the right for the trailing percent label
const TRAILING_W: u16 = 64;
const CORNER: Size = Size::new(4, 4);

pub struct BookRow<'a> {
    region: Region,
    title: &'a str,
    cover: Option<&'a DecodedImage>,
    progress_pct: Option<u8>,
    selected: bool,
}

impl<'a> BookRow<'a> {
    pub fn new(region: Region, title: &'a str) -> Self {
        Self {
            region,
            title,
            cover: None,
            progress_pct: None,
            selected: false,
        }
    }

    pub fn cover(mut self, cover: Option<&'a DecodedImage>) -> Self {
        self.cover = cover;
        self
    }

    pub fn progress_pct(mut self, pct: u8) -> Self {
        self.progress_pct = Some(pct);
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

        // title shifts right past the cover; the trailing percent
        // (when shown) reserves width on the right so text never
        // collides with either neighbour.
        let trailing = if self.progress_pct.is_some() {
            TRAILING_W + LARGE_MARGIN
        } else {
            LARGE_MARGIN
        };
        let text_left = region.x + COVER_PAD + TEXT_INDENT;
        let title_region = Region::new(
            text_left,
            region.y,
            region.w.saturating_sub(COVER_PAD + TEXT_INDENT + trailing),
            region.h,
        );
        font.draw_aligned(strip, title_region, self.title, Alignment::CenterLeft, fg);

        if let Some(pct) = self.progress_pct {
            let pct_region = Region::new(
                region.x + region.w - TRAILING_W - LARGE_MARGIN,
                region.y,
                TRAILING_W,
                region.h,
            );
            let mut pct_buf = BitmapDynLabel::<8>::new(pct_region, font)
                .alignment(Alignment::CenterRight)
                .inverted(self.selected);
            let _ = write!(pct_buf, "{}%", pct);
            pct_buf.draw(strip).ok();
        }
    }
}
