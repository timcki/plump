// home screen: continue-reading card + recent list.
//
// the v1 mockup collapses the old launcher menu (5 buttons) into a
// single content surface: one big card for the most recently opened
// book, then the next 3 by bookmark generation. app navigation lives
// in the top bar (`ui::chrome::top_status`); only Reader is reachable
// from here, by selecting the card or one of the rows.
//
// all four draw a Card cover on the same left edge into the same text
// column, so the screen reads as four books with the top one opened
// rather than as one book and a list about some others.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};

use plump_kernel::util::FixedStr;

use crate::apps::recent::{self, RecentRecord};
use crate::apps::widgets::row::{self, RowFonts, RowGroup, RowLead, RowSpec, ValueChip};
use crate::apps::widgets::sheet::{ICON_LIST, ICON_PLAY};
use crate::apps::{
    App, AppContext, AppId, BgBudget, BgOutcome, MSG_TAG_OPEN_CONTENTS, RECENT_FILE, Transition,
};
use crate::kernel::QuickAction;
use crate::ui::StackFmt;
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::battery;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::kernel::bookmarks::{self, BmListEntry};
use crate::ui::{
    Alignment, CONTENT_TOP, FULL_CONTENT_W, LARGE_MARGIN, Region,
};

// continue-reading card.
const CARD_X: u16 = LARGE_MARGIN;
const CARD_W: u16 = FULL_CONTENT_W;
// the Card cover variant is 112x160, so the card is that plus its
// padding: at 180 with 14 of pad the inner height was 152 and the
// guard below threw every cover away, which is why the card has been
// drawing text on its own
const CARD_PAD: u16 = 12;
const CARD_H: u16 = crate::apps::cover_cache::CARD_THUMB_H + 2 * CARD_PAD;
const CARD_PROGRESS_H: u16 = row::BAR_H;

// recent list: one outlined group of cover rows, the same component
// the library and the reader's contents sheet draw.
//
// three, not four. the band under the card is 533 px, and four rows
// of it put a 64x96 thumb in the middle of 133 px with an empty
// right-hand column beside it -- a list that was mostly the paper
// between its contents. three rows take the Card cover the screen
// already loads for the card above them, which fills the row it is
// in, and the column that was empty carries the position.
const MAX_RECENT_ROWS: usize = 3;

/// Cover variant the recent rows draw. Same one the card uses, so a
/// row and the card show a book at the same size and the screen reads
/// as four books rather than one book and a list about three others.
const ROW_COVER: row::CoverBox = row::CoverBox::Card;

// 1 card + N rows.
const MAX_ITEMS: usize = 1 + MAX_RECENT_ROWS;

// quick menu rows for the most recent book. the manager lends them
// to every main-screen tab, so their ids sit above any app's own
pub(crate) const QA_CONTINUE: u8 = 0xE1;
pub(crate) const QA_CONTENTS: u8 = 0xE2;

pub(crate) const fn is_resume_action(id: u8) -> bool {
    matches!(id, QA_CONTINUE | QA_CONTENTS)
}

// layout regions are content-area relative; the manager paints the
// shared chrome (top status bar) over y < CONTENT_TOP, and the tab
// bar over y >= SCREEN_H - theme.bottom_bar_h.
const CARD_Y: u16 = CONTENT_TOP;
const CARD_REGION: Region = Region::new(CARD_X, CARD_Y, CARD_W, CARD_H);

// between the card and the list: a 24 px hairline, centred. the same
// rule the reader's loading plate sets between an author and a resume
// line, and 10 px cheaper than the caption it replaces -- which was
// carrying one word plus a count of rows you can see and count.
const RULE_W: u16 = 24;
const RULE_LEAD: u16 = 13;
const RULE_GAP: u16 = 13;
const RULE_Y: u16 = CARD_Y + CARD_H + RULE_LEAD;
const RULE_REGION: Region = Region::new(LARGE_MARGIN, RULE_Y, FULL_CONTENT_W, 1);
const FIRST_ROW_Y: u16 = RULE_Y + 1 + RULE_GAP;

const CONTENT_REGION: Region = Region::new(0, CONTENT_TOP, SCREEN_W, SCREEN_H - CONTENT_TOP);

/// Row height: the band left under the card, divided by the rows that
/// go in it. At three Card covers this comes out at 177, which is
/// 8 px of air above and below each one -- the same air the library
/// gives its own rows, and the cover reaches the full height of the
/// row instead of floating in it.
const ROW_H: u16 = (LIST_BOTTOM - FIRST_ROW_Y - 1) / MAX_RECENT_ROWS as u16;
const LIST_BOTTOM: u16 = plump_kernel::ui::Theme::default_v1().content_bottom();

/// The recent list as one group. Sized to the rows it holds, so a
/// card with two bookmarks behind it does not leave an empty frame.
fn recent_group(rows: usize) -> RowGroup {
    RowGroup::new(LARGE_MARGIN, FIRST_ROW_Y, FULL_CONTENT_W, rows, ROW_H)
}

fn row_region(i: usize) -> Region {
    recent_group(MAX_RECENT_ROWS).row_region(i)
}

// both of these were true by accident and then quietly stopped being:
// the card's inner height fell under the cover variant it asks for, so
// the guard meant for legacy 200x240 covers threw away every current
// one and the card drew text alone for weeks. checked at compile time
// now, because nothing at runtime complains.
const _: () = assert!(
    CARD_H - 2 * CARD_PAD >= crate::apps::cover_cache::CARD_THUMB_H,
    "card is shorter than the Card cover variant it loads"
);
const _: () = assert!(
    FIRST_ROW_Y + row::RowGroup::height(MAX_RECENT_ROWS, ROW_H) <= LIST_BOTTOM,
    "card plus recent list overflows the content band"
);
const _: () = assert!(
    ROW_H >= ROW_COVER.h() + 12,
    "recent rows crowd their covers"
);

// recent list entry. the author and the position come from the same
// bundle session that reads the row's cover, so a row costs one file
// open rather than two.
#[derive(Clone, Copy)]
struct RecentRow {
    filename: FixedStr<32>,
    title: FixedStr<64>,
    author: FixedStr<40>,
    progress_pct: u8,
    /// (chapter number, chapter count) from the book record
    position: Option<(u32, u32)>,
    valid: bool,
}

impl RecentRow {
    const EMPTY: Self = Self {
        filename: FixedStr::EMPTY,
        title: FixedStr::EMPTY,
        author: FixedStr::EMPTY,
        progress_pct: 0,
        position: None,
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
    /// the card's title, in the reader's family at heading scale
    book_title: &'static fonts::bitmap::BitmapFont,
    /// a recent row's title, matching the contents sheet
    book_row: &'static fonts::bitmap::BitmapFont,

    // active book (the card)
    recent_book: FixedStr<32>,
    recent_title: FixedStr<64>,
    recent_author: FixedStr<64>,
    recent_progress: u8,
    recent_stats_time: u32,
    /// chapter number as the device names it (1-based) and the count,
    /// from the bundle header the cover read already opens
    recent_chapter: u16,
    recent_chapter_count: u16,
    recent_cover: Option<crate::kernel::work_queue::DecodedImage>,

    // recent list (3 most-recent bookmarks after the card)
    recent_rows: [RecentRow; MAX_RECENT_ROWS],
    recent_row_count: usize,
    recent_row_covers: [Option<crate::kernel::work_queue::DecodedImage>; MAX_RECENT_ROWS],

    needs_load: bool,
    bat_pct: u8,
    /// books on the card, for the empty card's one line
    library_count: u16,

    qa_buf: [QuickAction; 2],
    qa_count: usize,
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
            book_title: fonts::heading_font(fonts::ReaderFont::Bookerly.family(), 0),
            book_row: fonts::body_font(fonts::ReaderFont::Bookerly.family(), 1),
            recent_book: FixedStr::EMPTY,
            recent_title: FixedStr::EMPTY,
            recent_author: FixedStr::EMPTY,
            recent_progress: 0,
            recent_stats_time: 0,
            recent_chapter: 0,
            recent_chapter_count: 0,
            recent_cover: None,
            recent_rows: [RecentRow::EMPTY; MAX_RECENT_ROWS],
            recent_row_count: 0,
            recent_row_covers: [const { None }; MAX_RECENT_ROWS],
            needs_load: false,
            bat_pct: 0,
            library_count: 0,
            qa_buf: [QuickAction::trigger(0, "", ""); 2],
            qa_count: 0,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
    }

    /// A book's title is set in the book's own face, the way the
    /// reader's contents sheet sets a chapter's. The device speaks in
    /// the UI face; only the book speaks in this one.
    pub fn set_reader_font(&mut self, font: fonts::ReaderFont) {
        self.book_title = fonts::heading_font(font.family(), 0);
        self.book_row = fonts::body_font(font.family(), 1);
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
        let t0 = embassy_time::Instant::now();
        self.load_card(k);
        self.load_library_count(k);
        let card_ms = t0.elapsed().as_millis();
        let t1 = embassy_time::Instant::now();
        self.load_recent_rows(k);
        log::info!(
            "home: load card {}ms rows {}ms",
            card_ms,
            t1.elapsed().as_millis()
        );
        self.rebuild_item_count();
    }

    fn load_card(&mut self, k: &mut KernelHandle<'_>) {
        let mut buf = [0u8; recent::BUF_LEN];
        match k
            .sd()
            .read_file_start_in_dir(k.sd().data_dir(), RECENT_FILE, &mut buf)
        {
            Ok((_, n)) if n > 0 => self.parse_recent(&buf[..n]),
            _ => self.recent_book = FixedStr::EMPTY,
        }

        if !self.recent_book.is_empty() {
            // the book record says where the reader is, in the same
            // numbers the reader's own screens use
            let rec = crate::apps::book_record::BookRecord::load(k, self.recent_book.as_str())
                .unwrap_or(crate::apps::book_record::BookRecord::EMPTY);
            self.recent_stats_time = rec.stats.time_secs;
            self.recent_chapter = rec.pos.map(|p| p.chapter_no).unwrap_or(0);
            self.recent_chapter_count = rec.pos.map(|p| p.chapter_count).unwrap_or(0);
            // RECENT's own percentage is written by the reader on every
            // turn, so it beats the record's; fall back to the record
            // for a book whose RECENT predates it
            if self.recent_progress == 0 {
                self.recent_progress = rec.pos.map(|p| p.progress_pct).unwrap_or(0);
            }
            let entry = crate::apps::cover_cache::load_book_entry(
                k,
                plump_kernel::util::hash::fnv1a(self.recent_book.as_bytes()),
                plump_kernel::kernel::bundle::CoverKind::Card,
            );
            // discard oversized covers from legacy single-variant bundles
            // (200x240) -- the reader re-decodes both variants at proper
            // size on next open. without this guard the old image would
            // overflow the card by ~88 px vertical.
            const CARD_INNER_H: u16 = CARD_H - 2 * CARD_PAD;
            self.recent_cover = entry.cover.filter(|img| img.height <= CARD_INNER_H);
        } else {
            self.recent_stats_time = 0;
            self.recent_cover = None;
            self.recent_chapter = 0;
            self.recent_chapter_count = 0;
        }
    }

    /// Books on the card. The directory cache is already loaded for
    /// the library, so this is a RAM read.
    fn load_library_count(&mut self, k: &mut KernelHandle<'_>) {
        self.library_count = if k.ensure_dir_cache_loaded().is_ok() {
            let mut page = [crate::drivers::storage::DirEntry::EMPTY; 1];
            k.dir_page(0, &mut page).map(|p| p.total as u16).unwrap_or(0)
        } else {
            0
        };
    }

    fn load_recent_rows(&mut self, k: &mut KernelHandle<'_>) {
        // pull the bookmark list sorted by generation (most recent
        // first), drop the one matching the card, take the next 3.
        let mut all = [BmListEntry::EMPTY; bookmarks::SLOTS];
        let n = k.bookmarks().load_all(&mut all);

        let _ = k.ensure_dir_cache_loaded();

        let mut out = 0usize;
        self.recent_rows = [RecentRow::EMPTY; MAX_RECENT_ROWS];
        // drop any previously-loaded covers before re-populating;
        // load_cover_variant_for needs a free heap window.
        self.recent_row_covers = [const { None }; MAX_RECENT_ROWS];

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
            // one bundle session per row for the cover and author; the
            // position is the book record's
            let book = crate::apps::cover_cache::load_book_entry(
                k,
                plump_kernel::util::hash::fnv1a(entry.filename.as_bytes()),
                plump_kernel::kernel::bundle::CoverKind::Card,
            );
            row.author = book.author;
            let pos = crate::apps::book_record::BookRecord::load(k, entry.filename.as_str())
                .and_then(|r| r.pos);
            row.progress_pct = pos.map(|p| p.progress_pct).unwrap_or(0);
            row.position = pos
                .filter(|p| p.chapter_count > 0)
                .map(|p| (p.chapter_no as u32, p.chapter_count as u32));
            row.valid = true;
            self.recent_rows[out] = row;
            self.recent_row_covers[out] = book.cover;
            out += 1;
        }
        self.recent_row_count = out;
    }

    fn parse_recent(&mut self, data: &[u8]) {
        let rec = RecentRecord::decode(data);
        self.recent_book.set(rec.filename);
        self.recent_title = FixedStr::from_bytes(rec.title);
        self.recent_author = FixedStr::from_bytes(rec.author);
        self.recent_progress = rec.progress;
    }

    fn rebuild_item_count(&mut self) {
        // card is always present (even when there's no recent book; it
        // shows an empty state). recent rows are appended.
        self.item_count = (1 + self.recent_row_count).min(MAX_ITEMS);
        if self.selected >= self.item_count {
            self.selected = 0;
        }
        self.rebuild_quick_actions();
    }

    // menu rows: resume the last book, or jump into its contents
    fn rebuild_quick_actions(&mut self) {
        self.qa_count = 0;
        if !self.has_recent() {
            return;
        }
        let mut pct = StackFmt::<8>::new();
        let _ = write!(pct, "{}%", self.recent_progress);
        self.qa_buf[0] = QuickAction::trigger(QA_CONTINUE, "Continue reading", "Open")
            .with_icon(ICON_PLAY)
            .with_value(pct.as_str());
        self.qa_buf[1] = QuickAction::trigger(QA_CONTENTS, "Contents", "Open").with_icon(ICON_LIST);
        self.qa_count = 2;
    }

    /// Push the reader on the most recent book; `tag` rides along
    /// (contents sheet on ready).
    fn open_recent(&self, ctx: &mut AppContext, tag: u8) -> Transition {
        if !self.has_recent() {
            return Transition::None;
        }
        ctx.set_message(self.recent_book.as_bytes());
        ctx.set_message_tag(tag);
        Transition::Push(AppId::Reader)
    }

    /// Filename of the book on the continue-reading card.
    pub fn recent_filename(&self) -> &[u8] {
        self.recent_book.as_bytes()
    }

    /// Title of the first recent row after the card's book, for the
    /// sleep card's "next" line once a book is finished.
    pub fn next_recent_title(&self) -> Option<&str> {
        self.recent_rows
            .iter()
            .take(self.recent_row_count)
            .find(|r| r.valid)
            .map(|r| r.display_name())
    }

    /// Describe the continue-reading book to the sleep card when the
    /// reader is not on the stack; false when there is no such book.
    pub fn fill_sleep_card(&self, card: &mut crate::apps::widgets::SleepCard) -> bool {
        if !self.has_recent() {
            return false;
        }
        card.set_book(
            self.recent_display_title(),
            self.recent_author_str(),
            self.recent_book.as_bytes(),
        );
        card.set_progress(self.recent_progress);
        if self.recent_chapter_count > 0 && self.recent_chapter > 0 {
            card.set_chapter_number(self.recent_chapter, self.recent_chapter_count);
        }
        true
    }

    pub(crate) fn has_recent(&self) -> bool {
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

    /// Time left in the book, extrapolated from what it has already
    /// cost at the fraction it has already covered.
    ///
    /// Needs a real fraction and a real history behind it: under 5% or
    /// five minutes the ratio is noise, so the card says nothing
    /// rather than guessing. Same guard the sleep card's pace uses.
    fn time_left_secs(&self) -> Option<u32> {
        let pct = self.recent_progress as u32;
        if !(5..100).contains(&pct) || self.recent_stats_time < 300 {
            return None;
        }
        Some(self.recent_stats_time.saturating_mul(100 - pct) / pct)
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
            // the empty card points at the library, so the press it
            // invites does something
            return if self.selected == 0 {
                Transition::Replace(AppId::Library)
            } else {
                Transition::None
            };
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
                "home: pre-sleep freed ~{}KB card cover",
                cover.data.capacity() / 1024
            );
        }
        let mut freed_bytes = 0usize;
        for slot in self.recent_row_covers.iter_mut() {
            if let Some(cover) = slot.take() {
                freed_bytes += cover.data.capacity();
            }
        }
        if freed_bytes > 0 {
            log::info!(
                "home: pre-sleep freed ~{}B mini-thumbs",
                freed_bytes,
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
        self.load_library_count(k);
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

    fn quick_actions(&self) -> &[QuickAction] {
        &self.qa_buf[..self.qa_count]
    }

    fn on_quick_trigger(&mut self, id: u8, ctx: &mut AppContext) -> Transition {
        match id {
            QA_CONTINUE => self.open_recent(ctx, 0),
            QA_CONTENTS => self.open_recent(ctx, MSG_TAG_OPEN_CONTENTS),
            _ => Transition::None,
        }
    }

    fn menu_title(&self) -> &str {
        if self.has_recent() {
            self.recent_display_title()
        } else {
            "Home"
        }
    }

    fn menu_meta(&self, out: &mut StackFmt<64>) {
        if !self.has_recent() {
            return;
        }
        let author = self.recent_author_str();
        if !author.is_empty() {
            let _ = write!(out, "{} \u{00B7} ", author);
        }
        let _ = write!(out, "{}% read", self.recent_progress);
    }

    fn draw(&self, strip: &mut StripBuffer) {
        const R_CARD: Size = Size::new(8, 8);

        let font = self.ui_fonts.body;
        let heading = self.ui_fonts.heading;

        // section caption
        let theme = plump_kernel::ui::Theme::default_v1();
        let mut painter = plump_kernel::ui::Painter::new(strip, &theme);
        // between the card and the list, the rule. no caption over
        // the card: it is the only thing up there and it says what it
        // is, and today's reading belongs to the Stats screen
        if self.recent_row_count > 0 {
            let x = RULE_REGION.x + (RULE_REGION.w.saturating_sub(RULE_W)) / 2;
            painter.hairline_h(RULE_REGION.y, x, x + RULE_W);
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
            // the card reserves the cover box rather than measuring
            // the image in it, and the box and the gap after it are
            // the row's own: the card's cover and the three below it
            // then share one left edge and one text column, which is
            // the whole reason the card is the same width as the
            // group under it. a book whose bundle only cached the
            // small variant used to shift this entire block left
            let (box_w, box_h) = (ROW_COVER.w(), ROW_COVER.h());
            let (text_x, text_w, text_align) = if let Some(ref img) = self.recent_cover {
                let img_x = inner_x + box_w.saturating_sub(img.width) / 2;
                let img_y = CARD_Y + CARD_PAD + box_h.saturating_sub(img.height) / 2;
                strip.blit_1bpp(
                    &img.data,
                    0,
                    img.width as usize,
                    img.height as usize,
                    img.stride,
                    img_x as i32,
                    img_y as i32,
                    !card_selected,
                );
                let tx = inner_x + box_w + row::CELL_GAP;
                let tw = (CARD_X + CARD_W - CARD_PAD).saturating_sub(tx);
                (tx, tw, Alignment::TopLeft)
            } else {
                (inner_x, inner_w, Alignment::Center)
            };

            // title
            let title_y = CARD_Y + CARD_PAD + 16;
            let title_region = Region::new(text_x, title_y, text_w, heading_h);
            let title = self.recent_display_title();
            draw_truncated(
                strip,
                self.book_title,
                title_region,
                title,
                text_w,
                text_align,
                card_fg,
            );

            // author
            let author = self.recent_author_str();
            if !author.is_empty() {
                let author_y = title_y + heading_h + 6;
                let author_region = Region::new(text_x, author_y, text_w, line_h);
                draw_truncated(strip, font, author_region, author, text_w, text_align, card_fg);
            }

            // the same tube the rows and the sleep card draw
            let bar_y = CARD_Y + CARD_H - CARD_PAD - CARD_PROGRESS_H - line_h - 6;
            row::draw_tube_bar(
                strip,
                Region::new(text_x, bar_y, text_w, CARD_PROGRESS_H),
                self.recent_progress as u32,
                100,
                card_fg,
            );

            // meta line: where you are on the left, what is left of the
            // book on the right, in the chrome font both sides
            let chrome = fonts::chrome_font();
            let meta_y = bar_y + CARD_PROGRESS_H + 4;
            let meta_r = Region::new(text_x, meta_y, text_w, line_h);

            let mut left = StackFmt::<40>::new();
            if self.recent_chapter_count > 0 {
                let _ = write!(
                    left,
                    "chapter {} of {}",
                    self.recent_chapter,
                    self.recent_chapter_count
                );
                if self.recent_progress > 0 {
                    let _ = write!(left, " \u{00B7} {}%", self.recent_progress);
                }
            } else if self.recent_progress > 0 {
                let _ = write!(left, "{}% read", self.recent_progress);
            }
            if !left.as_str().is_empty() {
                chrome.draw_aligned(strip, meta_r, left.as_str(), text_align, card_fg);
            }

            let mut right = StackFmt::<20>::new();
            match self.time_left_secs() {
                Some(secs) => {
                    write_duration(&mut right, secs);
                    let _ = right.write_str(" left");
                }
                None if self.recent_stats_time > 0 => {
                    write_duration(&mut right, self.recent_stats_time)
                }
                None => {}
            }
            // with no cover the text block is centred, and a
            // right-aligned second half would read as a stray figure
            if !right.as_str().is_empty() && matches!(text_align, Alignment::TopLeft) {
                chrome.draw_aligned(
                    strip,
                    meta_r,
                    right.as_str(),
                    Alignment::CenterRight,
                    card_fg,
                );
            }
        } else {
            // the card is still the press target, so it keeps its frame
            // and says where the press goes
            let chrome = fonts::chrome_font();
            let title_h = self.book_title.line_height;
            let top = CARD_Y + (CARD_H - title_h - line_h) / 2;
            self.book_title.draw_aligned(
                strip,
                Region::new(inner_x, top, inner_w, title_h),
                "Nothing open yet",
                Alignment::Center,
                card_fg,
            );
            let mut hint = StackFmt::<48>::new();
            match self.library_count {
                0 => {
                    let _ = hint.write_str("No books on the card");
                }
                1 => {
                    let _ = hint.write_str("One book is waiting in the library");
                }
                n => {
                    let _ = write!(hint, "{} books are waiting in the library", n);
                }
            }
            chrome.draw_aligned(
                strip,
                Region::new(inner_x, top + title_h + 2, inner_w, line_h),
                hint.as_str(),
                Alignment::Center,
                card_fg,
            );
        }

        // recent rows: one group, the same row the library and the
        // reader's contents sheet draw
        let group = recent_group(self.recent_row_count);
        group.draw_outline(strip);
        let row_fonts = RowFonts {
            text: self.book_row,
            small: fonts::chrome_font(),
            icon: fonts::icon_font(1),
        };
        let mut pos = StackFmt::<20>::new();
        for i in 0..self.recent_row_count {
            let row = &self.recent_rows[i];
            if !row.valid {
                continue;
            }
            // where you are, in the value cell every other list on the
            // device fills. the bar beside it is the same fact read at
            // a glance rather than counted, which is the pairing the
            // reader's contents sheet already draws
            pos.clear();
            if let Some((done, total)) = row.position {
                let _ = write!(pos, "{} / {}", done, total);
            }
            let spec = RowSpec {
                lead: RowLead::Cover(self.recent_row_covers[i].as_ref(), ROW_COVER),
                text: row.display_name(),
                text_font: self.book_row,
                value: pos.as_str(),
                selected: self.selected == i + 1,
                sub: row.author.as_str(),
                progress: row.position,
                chip: ValueChip::None,
            };
            group.draw_row(strip, i, &row_fonts, &spec);
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

/// `9h 32m`, or minutes alone under the hour. The card's two figures
/// and the sleep card's read the same way.
fn write_duration(out: &mut impl core::fmt::Write, secs: u32) {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        let _ = write!(out, "{}h {}m", hours, mins);
    } else {
        let _ = write!(out, "{}m", mins);
    }
}
