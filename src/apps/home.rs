// home screen: continue-reading card + recent list.
//
// the v1 mockup collapses the old launcher menu (5 buttons) into a
// single content surface: one big card for the most recently opened
// book, then a short list of the next 3 by bookmark generation. all
// app navigation now happens through the bottom tab bar; only Reader
// is reachable from here (by selecting the card or a recent row).

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};

use plump_kernel::util::FixedStr;

use crate::apps::{App, AppContext, AppId, BgBudget, BgOutcome, RECENT_FILE, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::battery;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::kernel::bookmarks::{self, BmListEntry};
use crate::ui::{
    Alignment, BitmapDynLabel, CONTENT_TOP, FULL_CONTENT_W, LARGE_MARGIN, Region, SectionLabel,
};

// section caption strip height.
const CAPTION_H: u16 = 16;
const CAPTION_GAP: u16 = 8;

// continue-reading card.
const CARD_X: u16 = LARGE_MARGIN;
const CARD_W: u16 = FULL_CONTENT_W;
const CARD_H: u16 = 180;
const CARD_PAD: u16 = 14;
const CARD_PROGRESS_H: u16 = 5;

// recent list.
const MAX_RECENT_ROWS: usize = 3;
const ROW_H: u16 = 56;
const ROW_GAP: u16 = 4;
const ROW_STRIDE: u16 = ROW_H + ROW_GAP;
const ROW_X: u16 = LARGE_MARGIN;
const ROW_W: u16 = FULL_CONTENT_W;

// 1 card + N rows.
const MAX_ITEMS: usize = 1 + MAX_RECENT_ROWS;

// layout regions are content-area relative; the manager paints the
// shared chrome (top status bar) over y < CONTENT_TOP, and the tab
// bar over y >= SCREEN_H - theme.bottom_bar_h.
const CONTINUE_CAPTION_REGION: Region =
    Region::new(LARGE_MARGIN, CONTENT_TOP, FULL_CONTENT_W, CAPTION_H);
const CARD_Y: u16 = CONTENT_TOP + CAPTION_H + CAPTION_GAP;
const CARD_REGION: Region = Region::new(CARD_X, CARD_Y, CARD_W, CARD_H);
const RECENT_CAPTION_Y: u16 = CARD_Y + CARD_H + CAPTION_GAP * 2;
const RECENT_CAPTION_REGION: Region =
    Region::new(LARGE_MARGIN, RECENT_CAPTION_Y, FULL_CONTENT_W, CAPTION_H);
const FIRST_ROW_Y: u16 = RECENT_CAPTION_Y + CAPTION_H + CAPTION_GAP;

const CONTENT_REGION: Region = Region::new(0, CONTENT_TOP, SCREEN_W, SCREEN_H - CONTENT_TOP);

fn row_region(i: usize) -> Region {
    Region::new(ROW_X, FIRST_ROW_Y + i as u16 * ROW_STRIDE, ROW_W, ROW_H)
}

// recent list entry (filename + display title + progress placeholder).
// progress comes from bundle headers in a follow-up; today rows show
// only the title so the structural rewrite stays bounded.
#[derive(Clone, Copy)]
struct RecentRow {
    filename: FixedStr<32>,
    title: FixedStr<64>,
    progress_pct: u8,
    valid: bool,
}

impl RecentRow {
    const EMPTY: Self = Self {
        filename: FixedStr::EMPTY,
        title: FixedStr::EMPTY,
        progress_pct: 0,
        valid: false,
    };

    fn display_name(&self) -> &str {
        if !self.title.is_empty() {
            self.title.as_str()
        } else {
            self.filename.as_str()
        }
    }
}

pub struct HomeApp {
    selected: usize,
    item_count: usize,
    ui_fonts: fonts::UiFonts,

    // active book (the card)
    recent_book: FixedStr<32>,
    recent_title: FixedStr<64>,
    recent_author: FixedStr<64>,
    recent_progress: u8,
    recent_stats_time: u32,
    recent_cover: Option<crate::kernel::work_queue::DecodedImage>,

    // recent list (3 most-recent bookmarks after the card)
    recent_rows: [RecentRow; MAX_RECENT_ROWS],
    recent_row_count: usize,

    needs_load: bool,
    bat_pct: u8,
}

impl Default for HomeApp {
    fn default() -> Self {
        Self::new()
    }
}

impl HomeApp {
    pub fn new() -> Self {
        Self {
            selected: 0,
            item_count: 1,
            ui_fonts: fonts::UiFonts::for_size(0),
            recent_book: FixedStr::EMPTY,
            recent_title: FixedStr::EMPTY,
            recent_author: FixedStr::EMPTY,
            recent_progress: 0,
            recent_stats_time: 0,
            recent_cover: None,
            recent_rows: [RecentRow::EMPTY; MAX_RECENT_ROWS],
            recent_row_count: 0,
            needs_load: false,
            bat_pct: 0,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
    }

    // RTC session accessors (no inner state machine any more; bookmark
    // cursor is unused but kept for binary compat until chunk N).
    #[inline]
    pub fn state_id(&self) -> u8 {
        0
    }

    #[inline]
    pub fn selected(&self) -> usize {
        self.selected
    }

    #[inline]
    pub fn bm_selected(&self) -> usize {
        0
    }

    #[inline]
    pub fn bm_scroll(&self) -> usize {
        0
    }

    pub fn set_battery(&mut self, battery_mv: u16) {
        self.bat_pct = battery::battery_percentage(battery_mv);
    }

    pub fn restore_state(
        &mut self,
        _state_id: u8,
        selected: usize,
        _bm_selected: usize,
        _bm_scroll: usize,
    ) {
        self.selected = selected.min(MAX_ITEMS - 1);
        self.needs_load = true;
    }

    pub fn load_recent(&mut self, k: &mut KernelHandle<'_>) {
        self.load_card(k);
        self.load_recent_rows(k);
        self.rebuild_item_count();
    }

    fn load_card(&mut self, k: &mut KernelHandle<'_>) {
        let mut buf = [0u8; 196];
        match k
            .sd()
            .read_file_start_in_dir(k.sd().data_dir(), RECENT_FILE, &mut buf)
        {
            Ok((_, n)) if n > 0 => self.parse_recent(&buf[..n]),
            _ => self.recent_book = FixedStr::EMPTY,
        }

        if !self.recent_book.is_empty() {
            if let Some(s) = crate::apps::stats::ReadingStats::load(k, self.recent_book.as_str()) {
                self.recent_stats_time = s.time_secs;
            } else {
                self.recent_stats_time = 0;
            }
            self.recent_cover =
                crate::apps::cover_cache::load_cover_for(k, self.recent_book.as_bytes());
        } else {
            self.recent_stats_time = 0;
            self.recent_cover = None;
        }
    }

    fn load_recent_rows(&mut self, k: &mut KernelHandle<'_>) {
        // pull the bookmark list sorted by generation (most recent
        // first), drop the one matching the card, take the next 3.
        let mut all = [BmListEntry::EMPTY; bookmarks::SLOTS];
        let n = k.bookmark_cache().load_all(&mut all);

        let _ = k.ensure_dir_cache_loaded();

        let mut out = 0usize;
        self.recent_rows = [RecentRow::EMPTY; MAX_RECENT_ROWS];

        for entry in all.iter().take(n) {
            if out >= MAX_RECENT_ROWS {
                break;
            }
            if entry.filename.as_str() == self.recent_book.as_str() {
                continue;
            }
            let mut row = RecentRow::EMPTY;
            row.filename = entry.filename;
            if let Some(title) = k.dir_cache_mut().find_title(entry.filename.as_bytes()) {
                row.title.set(title);
            }
            row.progress_pct = 0; // chunk I follow-up: read from bundle header
            row.valid = true;
            self.recent_rows[out] = row;
            out += 1;
        }
        self.recent_row_count = out;
    }

    fn parse_recent(&mut self, data: &[u8]) {
        // format: filename\0title\0author\0progress_byte
        let mut fields = data.splitn(4, |&b| b == 0);

        if let Some(fname) = fields.next() {
            self.recent_book.set(fname);
        } else {
            self.recent_book = FixedStr::EMPTY;
            return;
        }
        self.recent_title = fields.next().map(FixedStr::from_bytes).unwrap_or_default();
        self.recent_author = fields.next().map(FixedStr::from_bytes).unwrap_or_default();
        self.recent_progress = fields
            .next()
            .and_then(|r| r.first().copied())
            .unwrap_or(0);
    }

    fn rebuild_item_count(&mut self) {
        // card is always present (even when there's no recent book; it
        // shows an empty state). recent rows are appended.
        self.item_count = (1 + self.recent_row_count).min(MAX_ITEMS);
        if self.selected >= self.item_count {
            self.selected = 0;
        }
    }

    fn has_recent(&self) -> bool {
        !self.recent_book.is_empty()
    }

    fn recent_display_title(&self) -> &str {
        if !self.recent_title.is_empty() {
            self.recent_title.as_str()
        } else {
            self.recent_book.as_str()
        }
    }

    fn recent_author_str(&self) -> &str {
        self.recent_author.as_str()
    }

    fn selection_region(&self, idx: usize) -> Region {
        if idx == 0 {
            CARD_REGION
        } else {
            row_region(idx - 1)
        }
    }

    fn move_selection(&mut self, delta: isize, ctx: &mut AppContext) {
        if self.item_count == 0 {
            return;
        }
        let new = (self.selected as isize + delta).rem_euclid(self.item_count as isize) as usize;
        if new != self.selected {
            ctx.mark_dirty(self.selection_region(self.selected));
            self.selected = new;
            ctx.mark_dirty(self.selection_region(self.selected));
        }
    }

    /// Set the filename to open on the next Reader push.
    fn open_book(&self, ctx: &mut AppContext) -> Transition {
        let name = if self.selected == 0 {
            self.recent_book.as_str()
        } else {
            let row = &self.recent_rows[self.selected - 1];
            if !row.valid {
                return Transition::None;
            }
            row.filename.as_str()
        };
        if name.is_empty() {
            return Transition::None;
        }
        ctx.set_message(name.as_bytes());
        Transition::Push(AppId::Reader)
    }
}

impl App<AppId> for HomeApp {
    fn on_enter(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        ctx.clear_message();
        self.selected = 0;
        self.bat_pct = battery::battery_percentage(k.battery_mv());
        self.needs_load = true;
        ctx.mark_dirty(CONTENT_REGION);
    }

    fn on_resume(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        self.selected = 0;
        self.bat_pct = battery::battery_percentage(k.battery_mv());
        self.needs_load = true;
        ctx.mark_dirty(CONTENT_REGION);
    }

    fn on_pre_sleep(&mut self, _k: &mut KernelHandle<'_>) {
        if let Some(cover) = self.recent_cover.take() {
            log::info!(
                "home: pre-sleep freed ~{}KB cover thumb",
                cover.data.capacity() / 1024
            );
        }
    }

    fn background_step(
        &mut self,
        ctx: &mut AppContext,
        k: &mut KernelHandle<'_>,
        _budget: BgBudget,
    ) -> BgOutcome {
        if !self.needs_load {
            return BgOutcome::Idle;
        }
        let old_count = self.item_count;
        self.load_card(k);
        self.load_recent_rows(k);
        self.rebuild_item_count();
        self.needs_load = false;
        if self.item_count != old_count {
            ctx.request_full_redraw();
        } else {
            ctx.mark_dirty(CONTENT_REGION);
        }
        BgOutcome::Progress { more: false }
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        match event {
            ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                self.move_selection(1, ctx);
                Transition::None
            }
            ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                self.move_selection(-1, ctx);
                Transition::None
            }
            ActionEvent::Press(Action::Select) => self.open_book(ctx),
            _ => Transition::None,
        }
    }

    fn draw(&self, strip: &mut StripBuffer) {
        const R_CARD: Size = Size::new(8, 8);
        const R_ROW: Size = Size::new(4, 4);

        let font = self.ui_fonts.body;
        let heading = self.ui_fonts.heading;

        // section captions
        let theme = plump_kernel::ui::Theme::default_v1();
        let mut painter = plump_kernel::ui::Painter::new(strip, &theme);
        SectionLabel::new(CONTINUE_CAPTION_REGION, "CONTINUE READING")
            .draw(&mut painter, font);
        if self.recent_row_count > 0 {
            SectionLabel::new(RECENT_CAPTION_REGION, "RECENT").draw(&mut painter, font);
        }

        // card
        let card_selected = self.selected == 0;
        let (card_bg, card_fg) = if card_selected {
            (BinaryColor::On, BinaryColor::Off)
        } else {
            (BinaryColor::Off, BinaryColor::On)
        };
        let card_rect = Rectangle::new(
            Point::new(CARD_X as i32, CARD_Y as i32),
            Size::new(CARD_W as u32, CARD_H as u32),
        );
        RoundedRectangle::with_equal_corners(card_rect, R_CARD)
            .into_styled(PrimitiveStyle::with_fill(card_bg))
            .draw(strip)
            .ok();
        RoundedRectangle::with_equal_corners(card_rect, R_CARD)
            .into_styled(PrimitiveStyle::with_stroke(card_fg, 2))
            .draw(strip)
            .ok();

        let inner_x = CARD_X + CARD_PAD;
        let inner_w = CARD_W - 2 * CARD_PAD;
        let line_h = font.line_height;
        let heading_h = heading.line_height;

        if self.has_recent() {
            const COVER_GAP: u16 = 16;
            let (text_x, text_w, text_align) = if let Some(ref img) = self.recent_cover {
                let img_x = inner_x as i32;
                let img_y = CARD_Y as i32
                    + CARD_PAD as i32
                    + ((CARD_H - 2 * CARD_PAD) as i32 - img.height as i32) / 2;
                strip.blit_1bpp(
                    &img.data,
                    0,
                    img.width as usize,
                    img.height as usize,
                    img.stride,
                    img_x,
                    img_y.max(CARD_Y as i32 + CARD_PAD as i32),
                    !card_selected,
                );
                let tx = inner_x + img.width + COVER_GAP;
                let tw = (CARD_X + CARD_W - CARD_PAD).saturating_sub(tx);
                (tx, tw, Alignment::TopLeft)
            } else {
                (inner_x, inner_w, Alignment::Center)
            };

            // title
            let title_y = CARD_Y + CARD_PAD + 16;
            let title_region = Region::new(text_x, title_y, text_w, heading_h);
            let title = self.recent_display_title();
            draw_truncated(strip, heading, title_region, title, text_w, text_align, card_fg);

            // author
            let author = self.recent_author_str();
            if !author.is_empty() {
                let author_y = title_y + heading_h + 6;
                let author_region = Region::new(text_x, author_y, text_w, line_h);
                draw_truncated(strip, font, author_region, author, text_w, text_align, card_fg);
            }

            // progress bar
            let bar_y = CARD_Y + CARD_H - CARD_PAD - CARD_PROGRESS_H - line_h - 6;
            let bar_w = text_w as u32;
            let filled = (bar_w * self.recent_progress as u32) / 100;
            let bar_rect = Rectangle::new(
                Point::new(text_x as i32, bar_y as i32),
                Size::new(bar_w, CARD_PROGRESS_H as u32),
            );
            RoundedRectangle::with_equal_corners(bar_rect, Size::new(3, 3))
                .into_styled(PrimitiveStyle::with_stroke(card_fg, 1))
                .draw(strip)
                .ok();
            if filled > 0 {
                RoundedRectangle::with_equal_corners(
                    Rectangle::new(
                        Point::new(text_x as i32, bar_y as i32),
                        Size::new(filled, CARD_PROGRESS_H as u32),
                    ),
                    Size::new(3, 3),
                )
                .into_styled(PrimitiveStyle::with_fill(card_fg))
                .draw(strip)
                .ok();
            }

            // stats text
            let pct_y = bar_y + CARD_PROGRESS_H + 4;
            let pct_region = Region::new(text_x, pct_y, text_w, line_h);
            let mut pct_buf = BitmapDynLabel::<32>::new(pct_region, font)
                .alignment(text_align)
                .inverted(card_selected);
            if self.recent_stats_time > 0 {
                let hours = self.recent_stats_time / 3600;
                let mins = (self.recent_stats_time % 3600) / 60;
                if hours > 0 {
                    let _ = write!(pct_buf, "{}h {}m", hours, mins);
                } else {
                    let _ = write!(pct_buf, "{}m", mins);
                }
            } else {
                let _ = write!(pct_buf, "{}% read", self.recent_progress);
            }
            pct_buf.draw(strip).ok();
        } else {
            let center_y = CARD_Y + (CARD_H - line_h) / 2;
            let center = Region::new(inner_x, center_y, inner_w, line_h);
            font.draw_aligned(strip, center, "No book opened yet", Alignment::Center, card_fg);
        }

        // recent rows
        for i in 0..self.recent_row_count {
            let row = &self.recent_rows[i];
            if !row.valid {
                continue;
            }
            let region = row_region(i);
            let selected = self.selected == i + 1;
            let (bg, fg) = if selected {
                (BinaryColor::On, BinaryColor::Off)
            } else {
                (BinaryColor::Off, BinaryColor::On)
            };

            let rect = Rectangle::new(
                Point::new(region.x as i32, region.y as i32),
                Size::new(region.w as u32, region.h as u32),
            );
            RoundedRectangle::with_equal_corners(rect, R_ROW)
                .into_styled(PrimitiveStyle::with_fill(bg))
                .draw(strip)
                .ok();

            // title (left), percent (right)
            let title_region = Region::new(
                region.x + LARGE_MARGIN,
                region.y,
                region.w.saturating_sub(LARGE_MARGIN + 64),
                region.h,
            );
            font.draw_aligned(
                strip,
                title_region,
                row.display_name(),
                Alignment::CenterLeft,
                fg,
            );

            let pct_region = Region::new(
                region.x + region.w - 64 - LARGE_MARGIN,
                region.y,
                64,
                region.h,
            );
            let mut pct_buf = BitmapDynLabel::<8>::new(pct_region, font)
                .alignment(Alignment::CenterRight)
                .inverted(selected);
            let _ = write!(pct_buf, "{}%", row.progress_pct);
            pct_buf.draw(strip).ok();
        }
    }
}

fn draw_truncated(
    strip: &mut StripBuffer,
    font: &fonts::bitmap::BitmapFont,
    region: Region,
    text: &str,
    text_w: u16,
    align: Alignment,
    fg: BinaryColor,
) {
    let cut = font.truncate_len(text, text_w);
    if cut >= text.len() {
        font.draw_aligned(strip, region, text, align, fg);
        return;
    }
    let mut buf = [0u8; 68];
    let n = cut.min(64);
    buf[..n].copy_from_slice(&text.as_bytes()[..n]);
    buf[n] = 0xE2;
    buf[n + 1] = 0x80;
    buf[n + 2] = 0xA6;
    let truncated = core::str::from_utf8(&buf[..n + 3]).unwrap_or(text);
    font.draw_aligned(strip, region, truncated, align, fg);
}
