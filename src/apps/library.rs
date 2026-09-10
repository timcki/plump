// Library tab: scrollable list of every book on the card, drawn with
// the shared list row (widgets/row.rs) inside one outlined group, so
// it is the same object as home's recent list, the reader's contents
// sheet and the settings screen. successor to the paged list, which
// in turn replaced the 2x3 cover grid (filter chips are gone for now:
// broken rendering + confusing navigation).
//
// a row shows the mini cover, the title in the book's own face, the
// author, the book's length, and a position bar once you have started
// it. a caption above the list shows the book count on the left and,
// once the list overflows the screen, the visible range on the right.
//
// navigation:
//   Up / Down: move the selection one row; the window scrolls by one
//     row when the selection crosses its top/bottom edge.
//   Left / Right: not handled here; the manager's default AtEdge
//     switches to the neighbouring tab (chunk E).
//   Select: push Reader for the highlighted book.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Painter, Region, StackFmt, Theme};

use plump_kernel::util::FixedStr;

use crate::apps::widgets::row::{self, RowFonts, RowLead, RowSpec, ValueChip};
use crate::apps::{App, AppContext, AppId, BgBudget, BgOutcome, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::storage::DirEntry;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::kernel::work_queue::DecodedImage;
use crate::ui::{Alignment, CONTENT_TOP, FULL_CONTENT_W, LARGE_MARGIN, SectionLabel};

const VISIBLE_ROWS: usize = 6;

const CAPTION_H: u16 = 16;
const CAPTION_GAP: u16 = 8;
const CAPTION_REGION: Region = Region::new(LARGE_MARGIN, CONTENT_TOP, FULL_CONTENT_W, CAPTION_H);

const LIST_TOP: u16 = CONTENT_TOP + CAPTION_H + CAPTION_GAP;
const LIST_BOTTOM: u16 = Theme::default_v1().content_bottom();

/// Rows divide the band they are given rather than carrying a fixed
/// height: six of them come out at 112, which leaves the 96 px cover
/// 8 px above and below. Two less than home's list, which is what
/// buys the sixth book on a browse screen.
const ROW_H: u16 = (LIST_BOTTOM - LIST_TOP - 1) / VISIBLE_ROWS as u16;
const _: () = assert!(ROW_H >= row::COVER_H + 12, "library rows crowd their covers");
const _: () = assert!(
    LIST_TOP + row::RowGroup::height(VISIBLE_ROWS, ROW_H) <= LIST_BOTTOM,
    "library list overflows the content band"
);

const CONTENT_REGION: Region = Region::new(0, CONTENT_TOP, SCREEN_W, SCREEN_H - CONTENT_TOP);

/// The visible window as one outlined group, sized to the rows it
/// actually holds so a short shelf does not leave an empty frame.
fn list_group(rows: usize) -> row::RowGroup {
    row::RowGroup::new(LARGE_MARGIN, LIST_TOP, FULL_CONTENT_W, rows, ROW_H)
}

fn row_region(slot: usize) -> Region {
    list_group(VISIBLE_ROWS).row_region(slot)
}

// pending SD work for the visible window, run from background_step
#[derive(Clone, Copy, PartialEq, Eq)]
enum Load {
    Idle,
    // the window moved; rows still in view are shifted, the rest read
    Window,
    // reread every row (entering the tab, resuming, waking)
    Full,
}

pub struct LibraryApp {
    selected: usize, // absolute index into the directory listing
    scroll: usize,   // absolute index of the first visible row
    // scroll offset the window below was loaded at; differs from
    // `scroll` between a move and its background reload
    window_start: usize,
    window_count: usize,
    entries: [Option<DirEntry>; VISIBLE_ROWS],
    covers: [Option<DecodedImage>; VISIBLE_ROWS],
    // cached page count per row from the book's layout index; 0 =
    // unknown (never opened, or not indexed yet)
    pages: [u32; VISIBLE_ROWS],
    // author and reading position, from the same bundle session that
    // reads the row's cover
    authors: [FixedStr<32>; VISIBLE_ROWS],
    positions: [Option<(u32, u32)>; VISIBLE_ROWS],
    total: usize,
    ui_fonts: fonts::UiFonts,
    /// a book's title is set in the book's own face
    book_font: &'static fonts::bitmap::BitmapFont,
    load: Load,
}

impl Default for LibraryApp {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryApp {
    pub fn new() -> Self {
        Self {
            selected: 0,
            scroll: 0,
            window_start: 0,
            window_count: 0,
            entries: [const { None }; VISIBLE_ROWS],
            covers: [const { None }; VISIBLE_ROWS],
            pages: [0; VISIBLE_ROWS],
            authors: [FixedStr::EMPTY; VISIBLE_ROWS],
            positions: [None; VISIBLE_ROWS],
            total: 0,
            ui_fonts: fonts::UiFonts::for_size(0),
            book_font: fonts::body_font(fonts::ReaderFont::Bookerly.family(), 1),
            load: Load::Full,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
    }

    /// A book's title is set in the book's own face, the way the
    /// reader's contents sheet sets a chapter's.
    pub fn set_reader_font(&mut self, font: fonts::ReaderFont) {
        self.book_font = fonts::body_font(font.family(), 1);
    }

    fn clear_slot(&mut self, slot: usize) {
        self.entries[slot] = None;
        self.covers[slot] = None;
        self.pages[slot] = 0;
        self.authors[slot] = FixedStr::EMPTY;
        self.positions[slot] = None;
    }

    /// Read directory entry `index` plus its mini cover and cached
    /// page count into `slot`. Returns false past the end of the list.
    fn load_slot(&mut self, k: &mut KernelHandle<'_>, slot: usize, index: usize) -> bool {
        // drop the old cover first so the bundle read has a free heap
        // window
        self.clear_slot(slot);
        let mut scratch = [DirEntry::EMPTY; 1];
        let Ok(page) = k.dir_page(index, &mut scratch) else {
            return false;
        };
        self.total = page.total;
        if page.count == 0 {
            return false;
        }
        let entry = scratch[0];
        let name_hash = plump_kernel::util::hash::fnv1a(entry.name.as_bytes());
        let book = crate::apps::cover_cache::load_book_entry(
            k,
            name_hash,
            plump_kernel::kernel::bundle::CoverKind::Mini,
        );
        self.pages[slot] = book.total_pages.unwrap_or(0);
        self.authors[slot] = FixedStr::from_bytes(book.author.as_bytes());
        self.positions[slot] = book.position();
        self.covers[slot] = book.cover;
        self.entries[slot] = Some(entry);
        true
    }

    fn load_window(&mut self, k: &mut KernelHandle<'_>) {
        let t0 = embassy_time::Instant::now();
        let _ = k.ensure_dir_cache_loaded();
        let dir_ms = t0.elapsed().as_millis();

        // files may have come or gone since the last load (upload tab,
        // card swap): refresh the total and clamp before reading rows
        let mut none: [DirEntry; 0] = [];
        if let Ok(page) = k.dir_page(0, &mut none) {
            self.total = page.total;
        }
        self.selected = self.selected.min(self.total.saturating_sub(1));
        self.scroll = self.scroll.min(self.total.saturating_sub(VISIBLE_ROWS));
        self.scroll_into_view();

        let delta = self.scroll as isize - self.window_start as isize;
        let (first, last) = match (self.load, delta) {
            // one row down: keep rows 1.. and read the new bottom row
            (Load::Window, 1) => {
                self.entries.rotate_left(1);
                self.covers.rotate_left(1);
                self.pages.rotate_left(1);
                (VISIBLE_ROWS - 1, VISIBLE_ROWS)
            }
            // one row up: keep rows ..N-1 and read the new top row
            (Load::Window, -1) => {
                self.entries.rotate_right(1);
                self.covers.rotate_right(1);
                self.pages.rotate_right(1);
                (0, 1)
            }
            (Load::Window, 0) => (VISIBLE_ROWS, VISIBLE_ROWS),
            _ => (0, VISIBLE_ROWS),
        };
        // a rotated slot may hold a stale cover from before the shift;
        // load_slot drops it before allocating the replacement
        let t1 = embassy_time::Instant::now();
        for slot in first..last {
            self.load_slot(k, slot, self.scroll + slot);
        }
        log::info!(
            "library: window {}..{} of {} loaded in {}ms (dir cache {}ms, {} rows {}ms)",
            self.scroll,
            self.scroll + VISIBLE_ROWS.min(self.total.saturating_sub(self.scroll)),
            self.total,
            t0.elapsed().as_millis(),
            dir_ms,
            last - first,
            t1.elapsed().as_millis(),
        );
        self.window_start = self.scroll;
        self.window_count = self.total.saturating_sub(self.scroll).min(VISIBLE_ROWS);
        for slot in self.window_count..VISIBLE_ROWS {
            self.clear_slot(slot);
        }
        self.load = Load::Idle;
    }

    fn scroll_into_view(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + VISIBLE_ROWS {
            self.scroll = self.selected + 1 - VISIBLE_ROWS;
        }
    }

    /// Apply a selection change: scroll the window if the new row is
    /// off screen (the redraw waits for the background reload so the
    /// panel never paints the old rows under the new state), else mark
    /// just the two rows that swap highlight.
    fn moved_from(&mut self, old: usize, ctx: &mut AppContext) {
        if old == self.selected {
            return;
        }
        let old_scroll = self.scroll;
        self.scroll_into_view();
        if self.scroll != old_scroll {
            self.load = Load::Window;
        } else if self.load == Load::Idle {
            ctx.mark_dirty(row_region(old - self.scroll));
            ctx.mark_dirty(row_region(self.selected - self.scroll));
        }
    }

    fn move_up(&mut self, ctx: &mut AppContext) {
        let old = self.selected;
        self.selected = self.selected.saturating_sub(1);
        self.moved_from(old, ctx);
    }

    fn move_down(&mut self, ctx: &mut AppContext) {
        let old = self.selected;
        if self.selected + 1 < self.total {
            self.selected += 1;
        }
        self.moved_from(old, ctx);
    }

    fn select(&mut self, ctx: &mut AppContext) -> Transition {
        // between a move and its reload the window still holds the
        // previous rows; opening one would launch the wrong book
        if self.load != Load::Idle {
            return Transition::None;
        }
        let slot = self.selected - self.window_start;
        if let Some(entry) = self.entries.get(slot).and_then(|e| e.as_ref()) {
            ctx.set_message(entry.name.as_bytes());
            Transition::Push(AppId::Reader)
        } else {
            Transition::None
        }
    }

    fn draw_caption(&self, strip: &mut StripBuffer, font: &'static fonts::bitmap::BitmapFont) {
        let theme = Theme::default_v1();
        let mut painter = Painter::new(strip, &theme);

        let mut count = StackFmt::<16>::new();
        let _ = match self.total {
            1 => write!(count, "1 book"),
            n => write!(count, "{} books", n),
        };
        SectionLabel::new(CAPTION_REGION, count.as_str()).draw(&mut painter, font);

        if self.total > VISIBLE_ROWS {
            let mut range = StackFmt::<24>::new();
            let first = self.scroll + 1;
            let last = (self.scroll + VISIBLE_ROWS).min(self.total);
            let _ = write!(range, "{}\u{2013}{} of {}", first, last, self.total);
            SectionLabel::new(CAPTION_REGION, range.as_str())
                .right_aligned()
                .draw(&mut painter, font);
        }
    }
}

impl App<AppId> for LibraryApp {
    fn on_enter(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        self.selected = 0;
        self.scroll = 0;
        self.load = Load::Full;
        ctx.mark_dirty(CONTENT_REGION);
    }

    fn on_resume(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        // keep scroll + selection so backing out of a book returns to
        // the same spot; reload in case files or covers changed
        self.load = Load::Full;
        ctx.mark_dirty(CONTENT_REGION);
    }

    fn on_pre_sleep(&mut self, _k: &mut KernelHandle<'_>) {
        let mut freed_bytes = 0usize;
        for slot in self.covers.iter_mut() {
            if let Some(cover) = slot.take() {
                freed_bytes += cover.data.capacity();
            }
        }
        if freed_bytes > 0 {
            log::info!("library: pre-sleep freed ~{}B mini-thumbs", freed_bytes);
        }
        // reload covers on wake
        self.load = Load::Full;
    }

    fn background_step(
        &mut self,
        ctx: &mut AppContext,
        k: &mut KernelHandle<'_>,
        _budget: BgBudget,
    ) -> BgOutcome {
        if self.load == Load::Idle {
            return BgOutcome::Idle;
        }
        self.load_window(k);
        ctx.mark_dirty(CONTENT_REGION);
        BgOutcome::Progress { more: false }
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        match event {
            ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                self.move_up(ctx);
                Transition::None
            }
            ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                self.move_down(ctx);
                Transition::None
            }
            ActionEvent::Press(Action::Select) => self.select(ctx),
            _ => Transition::None,
        }
    }

    fn draw(&self, strip: &mut StripBuffer) {
        let font = self.ui_fonts.body;

        self.draw_caption(strip, font);

        let group = list_group(self.window_count);
        group.draw_outline(strip);
        let row_fonts = RowFonts {
            text: self.book_font,
            small: fonts::chrome_font(),
            icon: fonts::icon_font(1),
        };

        for slot in 0..self.window_count {
            let Some(entry) = self.entries[slot].as_ref() else {
                continue;
            };
            // the page count is the book's length, which the position
            // bar does not say: one is how long, the other how far
            let mut pages = StackFmt::<16>::new();
            match self.pages[slot] {
                0 => {}
                1 => {
                    let _ = write!(pages, "1 page");
                }
                n => {
                    let _ = write!(pages, "{} pages", n);
                }
            }
            group.draw_row(
                strip,
                slot,
                &row_fonts,
                &RowSpec {
                    lead: RowLead::Cover(self.covers[slot].as_ref()),
                    text: entry.display_name(),
                    text_font: self.book_font,
                    value: pages.as_str(),
                    selected: self.selected == self.window_start + slot,
                    sub: self.authors[slot].as_str(),
                    // only a book you have started gets a bar; an
                    // untouched one would show an empty tube on every
                    // row and say nothing
                    progress: self.positions[slot],
                    chip: ValueChip::None,
                },
            );
        }

        if self.window_count == 0 {
            let center_y = LIST_TOP + (LIST_BOTTOM - LIST_TOP - font.line_height) / 2;
            let center = Region::new(LARGE_MARGIN, center_y, FULL_CONTENT_W, font.line_height);
            font.draw_aligned(
                strip,
                center,
                "No books on the card",
                Alignment::Center,
                BinaryColor::On,
            );
        }
    }
}
