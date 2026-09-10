// sleep card: the continue-reading card parked in the bottom-left
// corner of the sleep screen, over the wallpaper or on plain paper
// (design: Pulp Sleep Screen, corner card). cover, title, author and
// chapter in the reader's own face, a position bar with the page
// counter, then one footer line: time left in the chapter and the
// book from this book's own pace, or the chapter count when there is
// no pace yet, or the next book once this one is finished.
//
// the card is filled by the app manager right before the active app
// drops its heap for the wallpaper allocator, so everything it draws
// from lives in this fixed struct (the cover included). it is
// painted inside both sleep passes: `fill_flat` clears the wallpaper's
// gray codes under it in the grayscale pass, so its glyph edges get
// anti-aliased while the paper under them stays flat.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};

use plump_kernel::drivers::strip::GrayMode;
use plump_kernel::util::{FixedStr, hash};

use crate::apps::cover_placeholder;
use crate::apps::widgets::sheet::{SHEET_MARGIN, draw_ellipsized};
use crate::board::SCREEN_H;
use crate::drivers::strip::StripBuffer;
use crate::fonts::{self, FontSet, ReaderFont, Style, bitmap::BitmapFont};
use crate::kernel::work_queue::DecodedImage;
use crate::ui::{Alignment, Painter, Region, StackFmt, Theme};

const CARD_X: u16 = SHEET_MARGIN;
const CARD_W: u16 = 320;
const CARD_BOTTOM: u16 = SCREEN_H - SHEET_MARGIN;
const PAD: u16 = 14;
const R_CARD: u32 = 10;
const BORDER_W: u32 = 2;

// cover slot: the bundle's Card variant (112 x 160) box-filtered
// down to fit, the Mini variant (64 x 96) centred as-is
const COVER_W: u16 = 84;
const COVER_H: u16 = 126;
const COVER_STRIDE: usize = (COVER_W as usize).div_ceil(8);
const COVER_BYTES: usize = COVER_STRIDE * COVER_H as usize;
const COVER_GAP: u16 = 14;

const BAR_H: u16 = 4;
const BAR_GAP: u16 = 6;
const RULE_GAP: u16 = 10;

// a pace needs some history before its estimate means anything
const PACE_MIN_PAGES: u32 = 10;
const PACE_MIN_SECS: u32 = 300;

/// Faces the card draws with: the reader's family for the title and
/// chapter, the chrome font for everything numeric.
#[derive(Clone, Copy)]
struct CardFonts {
    title: &'static BitmapFont,
    chapter: &'static BitmapFont,
    small: &'static BitmapFont,
}

impl CardFonts {
    fn for_reader(font: ReaderFont) -> Self {
        Self {
            title: FontSet::for_reader(font, 1).font(Style::Regular),
            chapter: FontSet::for_reader(font, 0).font(Style::Italic),
            small: fonts::chrome_font(),
        }
    }
}

pub struct SleepCard {
    set: bool,
    fonts: Option<CardFonts>,

    title: FixedStr<64>,
    author: FixedStr<64>,
    chapter: FixedStr<64>,
    next_title: FixedStr<64>,
    finished: bool,

    percent: u8,
    // page / total across the book; total 0 = unknown
    book_page: u32,
    book_total: u32,
    // page / total inside the chapter; total 0 = unknown
    ch_page: u32,
    ch_total: u32,
    // 1-based chapter number of count; count 0 = unknown
    chapter_no: u16,
    chapter_count: u16,

    // this book's reading stats, the source of its pace
    stats_pages: u32,
    stats_secs: u32,
    bat_pct: u8,

    cover_id: u32,
    // mini cover as stored: scaled to fit the 64 x 96 slot, so its
    // size varies with the source aspect ratio; w 0 = none
    cover_w: u16,
    cover_h: u16,
    cover_stride: usize,
    cover: [u8; COVER_BYTES],
}

impl SleepCard {
    pub const fn new() -> Self {
        Self {
            set: false,
            fonts: None,
            title: FixedStr::EMPTY,
            author: FixedStr::EMPTY,
            chapter: FixedStr::EMPTY,
            next_title: FixedStr::EMPTY,
            finished: false,
            percent: 0,
            book_page: 0,
            book_total: 0,
            ch_page: 0,
            ch_total: 0,
            chapter_no: 0,
            chapter_count: 0,
            stats_pages: 0,
            stats_secs: 0,
            bat_pct: 0,
            cover_id: 0,
            cover_w: 0,
            cover_h: 0,
            cover_stride: 0,
            cover: [0u8; COVER_BYTES],
        }
    }

    /// Forget the previous fill; the card draws nothing until
    /// `set_book` is called again.
    pub fn clear(&mut self) {
        *self = Self {
            cover: [0u8; COVER_BYTES],
            ..Self::new()
        };
    }

    #[inline]
    pub fn is_set(&self) -> bool {
        self.set
    }

    pub fn set_fonts(&mut self, reader_font: ReaderFont) {
        self.fonts = Some(CardFonts::for_reader(reader_font));
    }

    pub fn set_book(&mut self, title: &str, author: &str, filename: &[u8]) {
        self.set = true;
        self.title.set(title.as_bytes());
        self.author.set(author.as_bytes());
        self.cover_id = hash::fnv1a(filename);
    }

    pub fn set_progress(&mut self, percent: u8) {
        self.percent = percent.min(100);
        self.finished = self.percent >= 100;
    }

    pub fn set_chapter_title(&mut self, title: &str) {
        self.chapter.set(title.as_bytes());
    }

    pub fn set_chapter_number(&mut self, no: u16, count: u16) {
        self.chapter_no = no;
        self.chapter_count = count;
    }

    pub fn set_chapter_pages(&mut self, page: u32, total: u32) {
        self.ch_page = page;
        self.ch_total = total;
    }

    pub fn set_book_pages(&mut self, page: u32, total: u32) {
        self.book_page = page;
        self.book_total = total;
    }

    pub fn set_next(&mut self, title: &str) {
        self.next_title.set(title.as_bytes());
    }

    pub fn has_stats(&self) -> bool {
        self.stats_pages > 0 || self.stats_secs > 0
    }

    pub fn set_stats(&mut self, pages: u32, secs: u32) {
        self.stats_pages = pages;
        self.stats_secs = secs;
    }

    pub fn set_battery(&mut self, pct: u8) {
        self.bat_pct = pct.min(100);
    }

    /// Take a 1bpp cover into the card: copied when it already fits
    /// the slot, box-filtered down when it is larger. False for an
    /// image the card cannot use (the placeholder shape draws then).
    pub fn set_cover(&mut self, img: &DecodedImage) -> bool {
        if img.width == 0
            || img.height == 0
            || img.stride != (img.width as usize).div_ceil(8)
            || img.data.len() < img.stride * img.height as usize
        {
            return false;
        }
        if img.width <= COVER_W && img.height <= COVER_H {
            let n = img.stride * img.height as usize;
            self.cover[..n].copy_from_slice(&img.data[..n]);
            self.cover_w = img.width;
            self.cover_h = img.height;
            self.cover_stride = img.stride;
            return true;
        }
        // fit inside the slot, keeping the aspect ratio (16.16 fixed)
        let sx = ((COVER_W as u32) << 16) / img.width as u32;
        let sy = ((COVER_H as u32) << 16) / img.height as u32;
        let scale = sx.min(sy);
        let dw = ((img.width as u32 * scale) >> 16).clamp(1, COVER_W as u32) as u16;
        let dh = ((img.height as u32 * scale) >> 16).clamp(1, COVER_H as u32) as u16;
        let stride = (dw as usize).div_ceil(8);
        self.cover[..stride * dh as usize].fill(0);
        scale_1bpp(img, &mut self.cover, dw, dh, stride);
        self.cover_w = dw;
        self.cover_h = dh;
        self.cover_stride = stride;
        true
    }

    // seconds per page from this book's own history, None until there
    // is enough of it
    fn pace(&self) -> Option<u32> {
        if self.stats_pages >= PACE_MIN_PAGES && self.stats_secs >= PACE_MIN_SECS {
            Some(self.stats_secs / self.stats_pages)
        } else {
            None
        }
    }

    fn chapter_left_secs(&self) -> Option<u32> {
        if self.finished || self.ch_total == 0 {
            return None;
        }
        let left = self.ch_total.saturating_sub(self.ch_page);
        self.pace().map(|p| p.saturating_mul(left))
    }

    fn book_left_secs(&self) -> Option<u32> {
        if self.finished {
            return None;
        }
        if self.book_total > 0 {
            let left = self.book_total.saturating_sub(self.book_page);
            return self.pace().map(|p| p.saturating_mul(left));
        }
        // no page directory yet: scale the time spent by what is left
        if self.percent >= 5 && self.stats_secs >= PACE_MIN_SECS {
            let p = self.percent as u64;
            return Some((self.stats_secs as u64 * (100 - p) / p) as u32);
        }
        None
    }

    fn geometry(&self, f: &CardFonts, has_footer: bool) -> Geom {
        let text_h = f.title.line_height
            + f.small.line_height
            + f.chapter.line_height
            + BAR_GAP
            + BAR_H
            + BAR_GAP
            + f.small.line_height;
        let top_h = text_h.max(COVER_H);
        let footer_h = if has_footer {
            RULE_GAP + 1 + RULE_GAP + f.small.line_height
        } else {
            0
        };
        let h = PAD + top_h + footer_h + PAD;
        let card = Region::new(CARD_X, CARD_BOTTOM - h, CARD_W, h);
        let cover = Region::new(
            card.x + PAD,
            card.y + PAD + (top_h - COVER_H) / 2,
            COVER_W,
            COVER_H,
        );
        let text_x = cover.x + COVER_W + COVER_GAP;
        let text = Region::new(
            text_x,
            card.y + PAD + (top_h - text_h) / 2,
            card.x + card.w - PAD - text_x,
            text_h,
        );
        let rule_y = card.y + PAD + top_h + RULE_GAP;
        Geom {
            card,
            cover,
            text,
            rule_y,
            line_y: rule_y + 1 + RULE_GAP,
        }
    }

    pub fn draw(&self, strip: &mut StripBuffer) {
        let (true, Some(f)) = (self.set, self.fonts) else {
            return;
        };
        let inner_w = CARD_W - 2 * PAD;
        let mut footer = StackFmt::<96>::new();
        self.footer_line(f.small, inner_w, &mut footer);
        let g = self.geometry(&f, !footer.as_str().is_empty());
        if !g.card.intersects(strip.logical_window()) {
            return;
        }

        // the rounded fill is the paper in the BW pass; the flat clear
        // runs only in the gray pass, where the wallpaper's codes under
        // the card must go to no-change. clearing the rectangle in BW
        // too would leave white corners outside the radius
        let bw = strip.gray_mode() == GrayMode::Bw;
        if !bw {
            strip.fill_flat(g.card, false);
        }
        if bw {
            let rect = g.card.to_rect();
            let radii = Size::new(R_CARD, R_CARD);
            RoundedRectangle::with_equal_corners(rect, radii)
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
                .draw(strip)
                .ok();
            RoundedRectangle::with_equal_corners(rect, radii)
                .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, BORDER_W))
                .draw(strip)
                .ok();
            self.draw_cover(strip, g.cover);
        }

        let ink = BinaryColor::On;
        let inner_x = g.card.x + PAD;

        // title, author with the battery at its right end, chapter
        let mut y = g.text.y;
        draw_ellipsized(
            strip,
            f.title,
            Region::new(g.text.x, y, g.text.w, f.title.line_height),
            self.title.as_str(),
            ink,
        );
        y += f.title.line_height;

        let mut bat = StackFmt::<8>::new();
        let _ = write!(bat, "{}%", self.bat_pct);
        let bat_w = f.small.measure_str(bat.as_str());
        let bat_icon_w = BAT_W + BAT_NUB_W + BAT_GAP;
        let author_w = g.text.w.saturating_sub(bat_w + bat_icon_w + COVER_GAP);
        draw_ellipsized(
            strip,
            f.small,
            Region::new(g.text.x, y, author_w, f.small.line_height),
            self.author.as_str(),
            ink,
        );
        let bat_region = Region::new(g.text.x, y, g.text.w, f.small.line_height);
        f.small
            .draw_aligned(strip, bat_region, bat.as_str(), Alignment::CenterRight, ink);
        if bw {
            let icon_x = g.text.x + g.text.w - bat_w - bat_icon_w;
            let icon_y = y + (f.small.line_height - BAT_H) / 2;
            draw_battery(strip, icon_x, icon_y, self.bat_pct);
        }
        y += f.small.line_height;

        let chapter: &str = if self.finished {
            "Finished"
        } else {
            self.chapter.as_str()
        };
        draw_ellipsized(
            strip,
            f.chapter,
            Region::new(g.text.x, y, g.text.w, f.chapter.line_height),
            chapter,
            ink,
        );
        y += f.chapter.line_height + BAR_GAP;

        // position bar and the counter row under it
        if bw {
            let bar = Region::new(g.text.x, y, g.text.w, BAR_H);
            let radii = Size::new(2, 2);
            RoundedRectangle::with_equal_corners(bar.to_rect(), radii)
                .into_styled(PrimitiveStyle::with_stroke(ink, 1))
                .draw(strip)
                .ok();
            let filled = (bar.w as u32 * self.percent as u32 / 100) as u16;
            if filled > 0 {
                let fill = Region::new(bar.x, bar.y, filled.max(BAR_H), bar.h);
                RoundedRectangle::with_equal_corners(fill.to_rect(), radii)
                    .into_styled(PrimitiveStyle::with_fill(ink))
                    .draw(strip)
                    .ok();
            }
        }
        y += BAR_H + BAR_GAP;

        let counter_region = Region::new(g.text.x, y, g.text.w, f.small.line_height);
        let mut left = StackFmt::<24>::new();
        let mut right = StackFmt::<8>::new();
        if self.book_total > 0 {
            let _ = write!(left, "{} / {}", self.book_page, self.book_total);
            let _ = write!(right, "{}%", self.percent);
        } else {
            let _ = write!(left, "{}% read", self.percent);
        }
        f.small.draw_aligned(
            strip,
            counter_region,
            left.as_str(),
            Alignment::CenterLeft,
            ink,
        );
        f.small.draw_aligned(
            strip,
            counter_region,
            right.as_str(),
            Alignment::CenterRight,
            ink,
        );

        // hairline, then the one footer line
        if footer.as_str().is_empty() {
            return;
        }
        if bw {
            Rectangle::new(
                Point::new(inner_x as i32, g.rule_y as i32),
                Size::new(inner_w as u32, 1),
            )
            .into_styled(PrimitiveStyle::with_fill(ink))
            .draw(strip)
            .ok();
        }
        draw_ellipsized(
            strip,
            f.small,
            Region::new(inner_x, g.line_y, inner_w, f.small.line_height),
            footer.as_str(),
            ink,
        );
    }

    fn draw_cover(&self, strip: &mut StripBuffer, r: Region) {
        if self.cover_w > 0 {
            // centred in the slot; a cover narrower or shorter than
            // the slot leaves paper around it, like the home card
            let x = r.x + (r.w - self.cover_w) / 2;
            let y = r.y + (r.h - self.cover_h) / 2;
            strip.blit_1bpp(
                &self.cover,
                0,
                self.cover_w as usize,
                self.cover_h as usize,
                self.cover_stride,
                x as i32,
                y as i32,
                true,
            );
        } else {
            let theme = Theme::default_v1();
            let mut painter = Painter::new(strip, &theme);
            cover_placeholder::draw_cover(&mut painter, r, self.cover_id);
        }
    }

    // the footer line: what is left in the chapter and the book, or
    // the next book once finished, or where in the book without a
    // pace. the long form drops its "left" when it would not fit
    fn footer_line<const N: usize>(&self, font: &BitmapFont, max_w: u16, out: &mut StackFmt<N>) {
        if self.finished {
            if !self.next_title.is_empty() {
                let _ = write!(out, "Next: {}", self.next_title.as_str());
            }
            return;
        }
        let (ch, bk) = (self.chapter_left_secs(), self.book_left_secs());
        if ch.is_some() || bk.is_some() {
            for long in [true, false] {
                out.clear();
                let mut first = true;
                for (secs, what) in [(ch, "chapter"), (bk, "book")] {
                    let Some(secs) = secs else { continue };
                    if !first {
                        let _ = out.write_str(" \u{00B7} ");
                    }
                    fmt_left(out, secs);
                    let _ = write!(
                        out,
                        "{} in {}",
                        if long && first { " left" } else { "" },
                        what
                    );
                    first = false;
                }
                if font.measure_str(out.as_str()) <= max_w {
                    return;
                }
            }
            return;
        }
        if self.chapter_count > 0 {
            let _ = write!(out, "Chapter {} of {}", self.chapter_no, self.chapter_count);
        }
    }
}

impl Default for SleepCard {
    fn default() -> Self {
        Self::new()
    }
}

struct Geom {
    card: Region,
    cover: Region,
    text: Region,
    rule_y: u16,
    line_y: u16,
}

// time left: to the minute under an hour, to five minutes above it,
// so the figure never looks more precise than the pace behind it
fn fmt_left<const N: usize>(out: &mut StackFmt<N>, secs: u32) {
    let mins = (secs + 30) / 60;
    if mins < 60 {
        let _ = write!(out, "{}m", mins.max(1));
        return;
    }
    let mins = (mins + 2) / 5 * 5;
    let (h, m) = (mins / 60, mins % 60);
    if m == 0 {
        let _ = write!(out, "{}h", h);
    } else {
        let _ = write!(out, "{}h {}m", h, m);
    }
}

// shrink a 1bpp image into `dst` (row-major, `stride` bytes per row)
// with a box filter: every destination pixel takes the ink density of
// its source window and a 4x4 Bayer threshold turns that back into
// one bit. nearest-neighbour would beat against the source dither
fn scale_1bpp(src: &DecodedImage, dst: &mut [u8], dw: u16, dh: u16, stride: usize) {
    const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    let (sw, sh) = (src.width as u32, src.height as u32);
    for y in 0..dh as u32 {
        let sy0 = y * sh / dh as u32;
        let sy1 = ((y + 1) * sh / dh as u32).max(sy0 + 1).min(sh);
        for x in 0..dw as u32 {
            let sx0 = x * sw / dw as u32;
            let sx1 = ((x + 1) * sw / dw as u32).max(sx0 + 1).min(sw);
            let mut ink = 0u32;
            for sy in sy0..sy1 {
                let row = &src.data[sy as usize * src.stride..];
                for sx in sx0..sx1 {
                    ink += (row[(sx / 8) as usize] >> (7 - (sx & 7)) & 1) as u32;
                }
            }
            let total = (sy1 - sy0) * (sx1 - sx0);
            let d16 = ink * 16 / total;
            if d16 > BAYER[(y & 3) as usize][(x & 3) as usize] as u32 {
                dst[y as usize * stride + (x / 8) as usize] |= 0x80 >> (x & 7);
            }
        }
    }
}

// battery glyph: outlined body with a nub, filled to the percentage
const BAT_W: u16 = 16;
const BAT_H: u16 = 9;
const BAT_NUB_W: u16 = 2;
const BAT_GAP: u16 = 5;

fn draw_battery(strip: &mut StripBuffer, x: u16, y: u16, pct: u8) {
    let ink = BinaryColor::On;
    Rectangle::new(
        Point::new(x as i32, y as i32),
        Size::new(BAT_W as u32, BAT_H as u32),
    )
    .into_styled(PrimitiveStyle::with_stroke(ink, 1))
    .draw(strip)
    .ok();
    Rectangle::new(
        Point::new((x + BAT_W) as i32, (y + 2) as i32),
        Size::new(BAT_NUB_W as u32, (BAT_H - 4) as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(ink))
    .draw(strip)
    .ok();
    let inner_w = BAT_W - 4;
    let filled = (inner_w as u32 * pct as u32 / 100) as u16;
    if filled > 0 {
        Rectangle::new(
            Point::new((x + 2) as i32, (y + 2) as i32),
            Size::new(filled as u32, (BAT_H - 4) as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(ink))
        .draw(strip)
        .ok();
    }
}
