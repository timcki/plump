// Library tab: paginated list of every book on the card, one BookRow
// per entry (mini cover on the left, title next to it) so it matches
// the home RECENT list exactly. successor to the 2x3 cover grid,
// whose filter chips are gone for now (broken rendering + confusing
// navigation).
//
// navigation:
//   Up / Down: move the selection; crossing the top/bottom edge flips
//     to the previous/next page.
//   Left / Right: previous/next page directly; on the first/last page
//     yields AtEdge so the manager switches tabs (chunk E).
//   Select: push Reader for the highlighted book.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Region, Theme};

use crate::apps::widgets::{BOOK_ROW_H, BookRow};
use crate::apps::{App, AppContext, AppId, BgBudget, BgOutcome, HDir, HResult, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::storage::DirEntry;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::kernel::work_queue::DecodedImage;
use crate::ui::{Alignment, BitmapDynLabel, CONTENT_TOP, FULL_CONTENT_W, LARGE_MARGIN};

const ROWS_PER_PAGE: usize = 6;
const ROW_GAP: u16 = 4;
const ROW_STRIDE: u16 = BOOK_ROW_H + ROW_GAP;
const LIST_TOP: u16 = CONTENT_TOP;

const FOOTER_H: u16 = 22;
const FOOTER_Y: u16 = SCREEN_H - Theme::default_v1().bottom_bar_h - FOOTER_H;

const CONTENT_REGION: Region = Region::new(0, CONTENT_TOP, SCREEN_W, SCREEN_H - CONTENT_TOP);

fn row_region(i: usize) -> Region {
    Region::new(
        LARGE_MARGIN,
        LIST_TOP + i as u16 * ROW_STRIDE,
        FULL_CONTENT_W,
        BOOK_ROW_H,
    )
}

pub struct LibraryApp {
    selected: usize, // row index within the current page
    page: usize,
    entries: [Option<DirEntry>; ROWS_PER_PAGE],
    covers: [Option<DecodedImage>; ROWS_PER_PAGE],
    // cached page count per row from the book's layout index; 0 =
    // unknown (never opened, or not indexed yet)
    pages: [u32; ROWS_PER_PAGE],
    page_count: usize,
    total: usize,
    ui_fonts: fonts::UiFonts,
    needs_load: bool,
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
            page: 0,
            entries: [const { None }; ROWS_PER_PAGE],
            covers: [const { None }; ROWS_PER_PAGE],
            pages: [0; ROWS_PER_PAGE],
            page_count: 0,
            total: 0,
            ui_fonts: fonts::UiFonts::for_size(0),
            needs_load: true,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
    }

    /// Total pages. At least 1 so the footer always reads
    /// "page 1 of 1" even on an empty library.
    fn total_pages(&self) -> usize {
        self.total.div_ceil(ROWS_PER_PAGE).max(1)
    }

    fn load_page(&mut self, k: &mut KernelHandle<'_>) {
        self.entries = [const { None }; ROWS_PER_PAGE];
        // drop old covers before loading new ones so
        // load_cover_variant_for has a free heap window
        self.covers = [const { None }; ROWS_PER_PAGE];
        self.pages = [0; ROWS_PER_PAGE];
        self.page_count = 0;
        self.total = 0;

        let _ = k.ensure_dir_cache_loaded();

        let mut scratch = [DirEntry::EMPTY; ROWS_PER_PAGE];
        let dirpage = match k.dir_page(self.page * ROWS_PER_PAGE, &mut scratch) {
            Ok(p) => p,
            Err(_) => return,
        };
        self.total = dirpage.total;
        self.page_count = dirpage.count;
        for (i, entry) in scratch.iter().take(dirpage.count).enumerate() {
            self.covers[i] = crate::apps::cover_cache::load_cover_variant_for(
                k,
                entry.name.as_bytes(),
                plump_kernel::kernel::bundle::CoverKind::Mini,
            );
            let name_hash = plump_kernel::util::hash::fnv1a(entry.name.as_bytes());
            self.pages[i] =
                plump_kernel::kernel::bundle::cached_total_pages(k.sd(), name_hash)
                    .unwrap_or(0);
            self.entries[i] = Some(*entry);
        }
        if self.selected >= self.page_count {
            self.selected = self.page_count.saturating_sub(1);
        }
    }

    fn move_up(&mut self, ctx: &mut AppContext) {
        if self.selected > 0 {
            ctx.mark_dirty(row_region(self.selected));
            self.selected -= 1;
            ctx.mark_dirty(row_region(self.selected));
        } else if self.page > 0 {
            // every page before the last is full, so the bottom row
            // always exists on the previous page. no dirty mark here:
            // background_step marks after the reload, so the panel
            // never paints the old page's rows under the new state
            self.page -= 1;
            self.selected = ROWS_PER_PAGE - 1;
            self.needs_load = true;
        }
    }

    fn move_down(&mut self, ctx: &mut AppContext) {
        if self.selected + 1 < self.page_count {
            ctx.mark_dirty(row_region(self.selected));
            self.selected += 1;
            ctx.mark_dirty(row_region(self.selected));
        } else if self.page + 1 < self.total_pages() {
            // dirty mark deferred to background_step, after the reload
            self.page += 1;
            self.selected = 0;
            self.needs_load = true;
        }
    }

    /// Left = previous page, right = next page. AtEdge on the
    /// first/last page hands the gesture back for a tab switch.
    fn move_page(&mut self, dir: HDir, _ctx: &mut AppContext) -> HResult {
        match dir {
            HDir::Right if self.page + 1 < self.total_pages() => self.page += 1,
            HDir::Left if self.page > 0 => self.page -= 1,
            _ => return HResult::AtEdge,
        }
        // keep the row index; load_page clamps it on short last pages.
        // the dirty mark waits for background_step so the render never
        // races the reload and paints the previous page's entries
        self.needs_load = true;
        HResult::Consumed
    }

    fn select(&mut self, ctx: &mut AppContext) -> Transition {
        // between a page change and its reload the entries still hold
        // the previous page; opening one would launch the wrong book
        if self.needs_load {
            return Transition::None;
        }
        if let Some(entry) = self.entries.get(self.selected).and_then(|e| e.as_ref()) {
            ctx.set_message(entry.name.as_bytes());
            Transition::Push(AppId::Reader)
        } else {
            Transition::None
        }
    }
}

impl App<AppId> for LibraryApp {
    fn on_enter(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        self.selected = 0;
        self.page = 0;
        self.needs_load = true;
        ctx.mark_dirty(CONTENT_REGION);
    }

    fn on_resume(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        // keep page + selection so backing out of a book returns to
        // the same spot; reload in case files or covers changed
        self.needs_load = true;
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
        self.needs_load = true;
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
        self.load_page(k);
        self.needs_load = false;
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

    fn on_horizontal(&mut self, dir: HDir, ctx: &mut AppContext) -> HResult {
        self.move_page(dir, ctx)
    }

    fn draw(&self, strip: &mut StripBuffer) {
        let font = self.ui_fonts.body;

        for i in 0..self.page_count {
            let Some(entry) = self.entries[i].as_ref() else {
                continue;
            };
            // page count from the cached layout index; hidden until
            // the book has been opened and indexed at least once
            let mut pages = crate::ui::stack_fmt::StackFmt::<16>::new();
            match self.pages[i] {
                0 => {}
                1 => {
                    let _ = write!(pages, "1 page");
                }
                n => {
                    let _ = write!(pages, "{} pages", n);
                }
            }
            BookRow::new(row_region(i), entry.display_name())
                .cover(self.covers[i].as_ref())
                .trailing(pages.as_str())
                .selected(self.selected == i)
                .draw(strip, font);
        }

        if self.page_count == 0 {
            let center_y = LIST_TOP + (FOOTER_Y - LIST_TOP - font.line_height) / 2;
            let center = Region::new(LARGE_MARGIN, center_y, FULL_CONTENT_W, font.line_height);
            font.draw_aligned(strip, center, "No books on the card", Alignment::Center, BinaryColor::On);
        }

        // pagination footer: "page X of N"
        let mut footer = BitmapDynLabel::<24>::new(
            Region::new(LARGE_MARGIN, FOOTER_Y, SCREEN_W - 2 * LARGE_MARGIN, FOOTER_H),
            font,
        )
        .alignment(Alignment::Center);
        let _ = write!(footer, "page {} of {}", self.page + 1, self.total_pages());
        footer.draw(strip).ok();
    }
}
