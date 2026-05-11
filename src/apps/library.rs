// Library tab: 2x3 grid of book covers with filter chips and a page
// footer. successor to the old Files list browser.
//
// scope for chunk J:
//   - All filter shows every entry in the dir cache, paginated 6 at
//     a time across a 2-column / 3-row grid.
//   - Reading filter shows only books that have a bookmark entry.
//   - Done filter is reserved (empty) until bundle.progress_pct is
//     plumbed here as a per-row read.
//   - cover thumbnails are deterministic placeholder shapes derived
//     from the filename hash (cover_placeholder module). real cover
//     thumbnails land in a later chunk that extends bundle.Covers
//     with a thumb-sized variant.
//
// navigation:
//   Up / Down: row change within the current screen (chips <-> rows).
//   Left / Right: column change within the current row, with edge
//     overflow that paginates the grid and finally yields AtEdge so
//     the manager switches tabs (chunk E).
//   Select on chip: switch filter.
//   Select on cell: push Reader for the highlighted book.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Painter, Region, Theme};

use crate::apps::{
    App, AppContext, AppId, BgBudget, BgOutcome, HDir, HResult, Transition,
};
use crate::apps::cover_placeholder;
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::storage::DirEntry;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::ui::{
    Alignment, BitmapDynLabel, CONTENT_TOP, FilterChip, LARGE_MARGIN,
};
use plump_kernel::util::hash;

const GRID_COLS: usize = 2;
const GRID_ROWS: usize = 3;
const GRID_CELLS: usize = GRID_COLS * GRID_ROWS;

const N_CHIPS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Reading,
    Done,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Reading => "Reading",
            Filter::Done => "Done",
        }
    }

    fn from_index(i: usize) -> Self {
        match i {
            1 => Filter::Reading,
            2 => Filter::Done,
            _ => Filter::All,
        }
    }

}

// cursor lives on either a filter chip or a grid cell. grid cells are
// indexed (row, col); only cells that map to a valid entry are
// considered selectable but the cursor positions are predictable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cursor {
    Chip(u8),       // 0..N_CHIPS
    Cell(u8, u8),   // (row 0..GRID_ROWS, col 0..GRID_COLS)
}

const CHIP_AREA_Y: u16 = CONTENT_TOP;
const CHIP_H: u16 = 30;
const CHIP_GAP: u16 = 10;
const CHIP_W: u16 = 92;
const GRID_TOP: u16 = CHIP_AREA_Y + CHIP_H + 14;
const FOOTER_H: u16 = 22;
const FOOTER_Y: u16 = SCREEN_H - Theme::default_v1().bottom_bar_h - FOOTER_H;
const GRID_BOTTOM: u16 = FOOTER_Y - 4;
const GRID_H: u16 = GRID_BOTTOM - GRID_TOP;
const ROW_H: u16 = GRID_H / GRID_ROWS as u16;
const COL_W: u16 = SCREEN_W / GRID_COLS as u16;
const COVER_PAD: u16 = 18;
const TITLE_BAND_H: u16 = 20;

const CONTENT_REGION: Region = Region::new(0, CONTENT_TOP, SCREEN_W, SCREEN_H - CONTENT_TOP);

pub struct LibraryApp {
    filter: Filter,
    cursor: Cursor,
    page: usize, // 0-indexed page within the filtered list
    page_entries: [Option<DirEntry>; GRID_CELLS],
    page_count: usize,
    filtered_total: usize,
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
            filter: Filter::All,
            cursor: Cursor::Cell(0, 0),
            page: 0,
            page_entries: [const { None }; GRID_CELLS],
            page_count: 0,
            filtered_total: 0,
            ui_fonts: fonts::UiFonts::for_size(0),
            needs_load: true,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
    }

    /// Total pages under the current filter. At least 1 so the footer
    /// always reads "page 1 of 1" even on an empty library.
    fn total_pages(&self) -> usize {
        self.filtered_total.div_ceil(GRID_CELLS).max(1)
    }

    fn load_filtered_page(&mut self, k: &mut KernelHandle<'_>) {
        self.page_entries = [const { None }; GRID_CELLS];
        self.page_count = 0;
        self.filtered_total = 0;

        let _ = k.ensure_dir_cache_loaded();

        match self.filter {
            Filter::All => {
                let mut scratch = [DirEntry::EMPTY; GRID_CELLS];
                let dirpage = match k.dir_page(self.page * GRID_CELLS, &mut scratch) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                self.filtered_total = dirpage.total;
                for (slot, entry) in self
                    .page_entries
                    .iter_mut()
                    .zip(scratch.iter().take(dirpage.count))
                {
                    *slot = Some(*entry);
                }
                self.page_count = dirpage.count;
            }
            Filter::Reading => {
                // walk the bookmark cache; resolve to dir entries by
                // filename. paginate by slicing the load_all output.
                let mut bms = [crate::kernel::bookmarks::BmListEntry::EMPTY;
                    crate::kernel::bookmarks::SLOTS];
                let n = k.bookmark_cache().load_all(&mut bms);
                self.filtered_total = n;
                let start = self.page * GRID_CELLS;
                let end = (start + GRID_CELLS).min(n);
                let mut produced = 0usize;
                for entry in bms.iter().take(end).skip(start) {
                    let mut de = DirEntry::EMPTY;
                    // bookmark filenames are FixedStr<32>; DirEntry uses
                    // FixedStr<13> (FAT 8.3). truncate to fit.
                    let raw = entry.filename.as_bytes();
                    let n = raw.len().min(13);
                    de.name.set(&raw[..n]);
                    if let Some(title) = k.dir_cache_mut().find_title(raw) {
                        de.set_title(title);
                    }
                    self.page_entries[produced] = Some(de);
                    produced += 1;
                }
                self.page_count = produced;
            }
            Filter::Done => {
                // no source yet; classification waits for bundle
                // header reads (deferred follow-up).
            }
        }
    }

    fn cell_region(row: u8, col: u8) -> Region {
        let x = col as u16 * COL_W;
        let y = GRID_TOP + row as u16 * ROW_H;
        Region::new(x, y, COL_W, ROW_H)
    }

    fn cover_region(row: u8, col: u8) -> Region {
        let cell = Self::cell_region(row, col);
        Region::new(
            cell.x + COVER_PAD,
            cell.y + 4,
            cell.w - 2 * COVER_PAD,
            cell.h - TITLE_BAND_H - 8,
        )
    }

    fn title_region(row: u8, col: u8) -> Region {
        let cell = Self::cell_region(row, col);
        Region::new(cell.x + 6, cell.y + cell.h - TITLE_BAND_H, cell.w - 12, TITLE_BAND_H)
    }

    fn chip_region(idx: usize) -> Region {
        let total_w = (CHIP_W * N_CHIPS as u16) + (CHIP_GAP * (N_CHIPS as u16 - 1));
        let x0 = (SCREEN_W - total_w) / 2;
        Region::new(
            x0 + idx as u16 * (CHIP_W + CHIP_GAP),
            CHIP_AREA_Y,
            CHIP_W,
            CHIP_H,
        )
    }

    fn move_cursor_up(&mut self, ctx: &mut AppContext) {
        match self.cursor {
            Cursor::Chip(_) => {}
            Cursor::Cell(0, col) => {
                // jump up to the chip aligned roughly with this column
                let chip_idx = ((col as usize) * (N_CHIPS - 1) / GRID_COLS.saturating_sub(1).max(1)).min(N_CHIPS - 1);
                self.cursor = Cursor::Chip(chip_idx as u8);
                ctx.mark_dirty(CONTENT_REGION);
            }
            Cursor::Cell(r, c) => {
                self.cursor = Cursor::Cell(r - 1, c);
                ctx.mark_dirty(CONTENT_REGION);
            }
        }
    }

    fn move_cursor_down(&mut self, ctx: &mut AppContext) {
        match self.cursor {
            Cursor::Chip(_) => {
                self.cursor = Cursor::Cell(0, 0);
                ctx.mark_dirty(CONTENT_REGION);
            }
            Cursor::Cell(r, c) if (r as usize) + 1 < GRID_ROWS => {
                self.cursor = Cursor::Cell(r + 1, c);
                ctx.mark_dirty(CONTENT_REGION);
            }
            _ => {}
        }
    }

    /// `dir` left = -1, right = +1. Returns Consumed when the cursor
    /// moved within the current screen (or page changed), AtEdge when
    /// the manager should switch tabs.
    fn move_cursor_h(&mut self, dir: HDir, ctx: &mut AppContext) -> HResult {
        let delta: i8 = match dir {
            HDir::Left => -1,
            HDir::Right => 1,
        };
        match self.cursor {
            Cursor::Chip(idx) => {
                let n = N_CHIPS as i8;
                let new = idx as i8 + delta;
                if new < 0 || new >= n {
                    return HResult::AtEdge;
                }
                self.cursor = Cursor::Chip(new as u8);
                ctx.mark_dirty(CONTENT_REGION);
                HResult::Consumed
            }
            Cursor::Cell(row, col) => {
                let new_col = col as i8 + delta;
                if new_col >= 0 && new_col < GRID_COLS as i8 {
                    self.cursor = Cursor::Cell(row, new_col as u8);
                    ctx.mark_dirty(CONTENT_REGION);
                    return HResult::Consumed;
                }
                // edge of grid: page or yield to tab switch.
                match dir {
                    HDir::Right => {
                        if self.page + 1 < self.total_pages() {
                            self.page += 1;
                            self.cursor = Cursor::Cell(row, 0);
                            self.needs_load = true;
                            ctx.mark_dirty(CONTENT_REGION);
                            HResult::Consumed
                        } else {
                            HResult::AtEdge
                        }
                    }
                    HDir::Left => {
                        if self.page > 0 {
                            self.page -= 1;
                            self.cursor = Cursor::Cell(row, GRID_COLS as u8 - 1);
                            self.needs_load = true;
                            ctx.mark_dirty(CONTENT_REGION);
                            HResult::Consumed
                        } else {
                            HResult::AtEdge
                        }
                    }
                }
            }
        }
    }

    fn select(&mut self, ctx: &mut AppContext) -> Transition {
        match self.cursor {
            Cursor::Chip(idx) => {
                self.filter = Filter::from_index(idx as usize);
                self.page = 0;
                self.cursor = Cursor::Cell(0, 0);
                self.needs_load = true;
                ctx.mark_dirty(CONTENT_REGION);
                Transition::None
            }
            Cursor::Cell(row, col) => {
                let cell_idx = row as usize * GRID_COLS + col as usize;
                if let Some(entry) = &self.page_entries[cell_idx] {
                    ctx.set_message(entry.name.as_bytes());
                    Transition::Push(AppId::Reader)
                } else {
                    Transition::None
                }
            }
        }
    }
}

impl App<AppId> for LibraryApp {
    fn on_enter(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        self.cursor = Cursor::Cell(0, 0);
        self.page = 0;
        self.needs_load = true;
        ctx.mark_dirty(CONTENT_REGION);
    }

    fn on_resume(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        self.needs_load = true;
        ctx.mark_dirty(CONTENT_REGION);
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
        self.load_filtered_page(k);
        self.needs_load = false;
        ctx.mark_dirty(CONTENT_REGION);
        BgOutcome::Progress { more: false }
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        match event {
            ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                self.move_cursor_up(ctx);
                Transition::None
            }
            ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                self.move_cursor_down(ctx);
                Transition::None
            }
            ActionEvent::Press(Action::Select) => self.select(ctx),
            _ => Transition::None,
        }
    }

    fn on_horizontal(&mut self, dir: HDir, ctx: &mut AppContext) -> HResult {
        self.move_cursor_h(dir, ctx)
    }

    fn draw(&self, strip: &mut StripBuffer) {
        let theme = Theme::default_v1();
        let mut p = Painter::new(strip, &theme);
        let font = self.ui_fonts.body;

        // filter chips
        for i in 0..N_CHIPS {
            let f = Filter::from_index(i);
            let region = Self::chip_region(i);
            let active = f == self.filter;
            let count = if active {
                self.filtered_total as u16
            } else {
                0
            };
            FilterChip::new(region, f.label(), count, active).draw(&mut p, font);
            // cursor-on-chip indicator: thin ring around the chip
            if let Cursor::Chip(idx) = self.cursor
                && idx as usize == i
                && !active
            {
                p.rect_stroke(region, 1);
            }
        }

        // grid cells
        for r in 0..GRID_ROWS {
            for c in 0..GRID_COLS {
                let cell_idx = r * GRID_COLS + c;
                let entry = self.page_entries[cell_idx].as_ref();
                let cell = Self::cell_region(r as u8, c as u8);

                if let Some(e) = entry {
                    let cover = Self::cover_region(r as u8, c as u8);
                    let id = hash::fnv1a(e.name.as_bytes());
                    cover_placeholder::draw_cover(&mut p, cover, id);

                    // title under the cover
                    let title_region = Self::title_region(r as u8, c as u8);
                    let name = e.display_name();
                    font.draw_aligned(
                        p.strip_mut(),
                        title_region,
                        name,
                        Alignment::TopCenter,
                        BinaryColor::On,
                    );
                }

                // selection indicator (thin border around the cell when
                // the cursor is on it). drawn after the cover so the
                // border isn't overwritten by the placeholder fill.
                if let Cursor::Cell(cr, cc) = self.cursor
                    && cr as usize == r
                    && cc as usize == c
                {
                    p.rect_stroke(cell, 2);
                }
            }
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
