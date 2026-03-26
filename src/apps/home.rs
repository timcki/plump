// launcher screen: menu, bookmarks browser

use core::fmt::Write as _;

use crate::apps::{App, AppContext, AppId, RECENT_FILE, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::battery;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::kernel::bookmarks::{self, BmListEntry};
use crate::ui::{
    Alignment, BitmapDynLabel, BitmapLabel, CONTENT_TOP, FULL_CONTENT_W, HEADER_W, LARGE_MARGIN,
    Region, SECTION_GAP, TITLE_Y_OFFSET,
};

// status bar at top
const STATUS_Y: u16 = CONTENT_TOP + 4;
const STATUS_H: u16 = 20;
const STATUS_PAD: u16 = 12;
const STATUS_TITLE_REGION: Region = Region::new(STATUS_PAD, STATUS_Y, 160, STATUS_H);
const STATUS_BAT_REGION: Region = Region::new(SCREEN_W - STATUS_PAD - 80, STATUS_Y, 80, STATUS_H);

// book card (shown when a recent book exists)
const CARD_X: u16 = 40;
const CARD_W: u16 = SCREEN_W - 2 * CARD_X;
const CARD_H: u16 = 280;
const CARD_Y: u16 = STATUS_Y + STATUS_H + 16;
const CARD_PAD: u16 = 16;
const CARD_REGION: Region = Region::new(CARD_X, CARD_Y, CARD_W, CARD_H);
const CARD_PROGRESS_H: u16 = 8;

const ITEM_W: u16 = 280;
const ITEM_H: u16 = 52;
const ITEM_GAP: u16 = 14;
const ITEM_STRIDE: u16 = ITEM_H + ITEM_GAP;
const ITEM_X: u16 = (SCREEN_W - ITEM_W) / 2;
const MAX_ITEMS: usize = 6;

// bookmark list layout (matches Files app)
const BM_ROW_H: u16 = 52;
const BM_ROW_GAP: u16 = 4;
const BM_ROW_STRIDE: u16 = BM_ROW_H + BM_ROW_GAP;
const BM_TITLE_Y: u16 = CONTENT_TOP + TITLE_Y_OFFSET;
const BM_HEADER_LIST_GAP: u16 = SECTION_GAP;
const BM_STATUS_W: u16 = 144;
const BM_STATUS_X: u16 = SCREEN_W - LARGE_MARGIN - BM_STATUS_W;

const CONTENT_REGION: Region = Region::new(0, CONTENT_TOP, SCREEN_W, SCREEN_H - CONTENT_TOP);

fn compute_item_regions() -> [Region; MAX_ITEMS] {
    let item_y = CARD_Y + CARD_H + ITEM_GAP;
    [
        Region::new(ITEM_X, item_y, ITEM_W, ITEM_H),
        Region::new(ITEM_X, item_y + ITEM_STRIDE, ITEM_W, ITEM_H),
        Region::new(ITEM_X, item_y + ITEM_STRIDE * 2, ITEM_W, ITEM_H),
        Region::new(ITEM_X, item_y + ITEM_STRIDE * 3, ITEM_W, ITEM_H),
        Region::new(ITEM_X, item_y + ITEM_STRIDE * 4, ITEM_W, ITEM_H),
        Region::new(ITEM_X, item_y + ITEM_STRIDE * 5, ITEM_W, ITEM_H),
    ]
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum HomeState {
    Menu,
    ShowBookmarks,
}

enum MenuAction {
    Continue,
    Push(AppId),
    OpenBookmarks,
}

pub struct HomeApp {
    state: HomeState,
    selected: usize,
    ui_fonts: fonts::UiFonts,
    item_regions: [Region; MAX_ITEMS],
    item_count: usize,

    recent_book: [u8; 32],
    recent_book_len: usize,
    recent_title: [u8; 64],
    recent_title_len: u8,
    recent_author: [u8; 64],
    recent_author_len: u8,
    recent_progress: u8,
    recent_stats_pages: u32,
    recent_stats_time: u32,
    recent_cover: Option<crate::kernel::work_queue::DecodedImage>,
    needs_load_recent: bool,

    bm_entries: [BmListEntry; bookmarks::SLOTS],
    bm_count: usize,
    bm_selected: usize,
    bm_scroll: usize,
    needs_load_bookmarks: bool,

    bat_pct: u8,
}

impl Default for HomeApp {
    fn default() -> Self {
        Self::new()
    }
}

impl HomeApp {
    pub fn new() -> Self {
        let uf = fonts::UiFonts::for_size(0);
        Self {
            state: HomeState::Menu,
            selected: 0,
            ui_fonts: uf,
            item_regions: compute_item_regions(),
            item_count: 5, // card + 4 menu buttons
            recent_book: [0u8; 32],
            recent_book_len: 0,
            recent_title: [0u8; 64],
            recent_title_len: 0,
            recent_author: [0u8; 64],
            recent_author_len: 0,
            recent_progress: 0,
            recent_stats_pages: 0,
            recent_stats_time: 0,
            recent_cover: None,
            needs_load_recent: false,
            bm_entries: [BmListEntry::EMPTY; bookmarks::SLOTS],
            bm_count: 0,
            bm_selected: 0,
            bm_scroll: 0,
            needs_load_bookmarks: false,
            bat_pct: 0,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
        self.item_regions = compute_item_regions();
    }

    // Session state accessors for RTC persistence
    #[inline]
    pub fn state_id(&self) -> u8 {
        match self.state {
            HomeState::Menu => 0,
            HomeState::ShowBookmarks => 1,
        }
    }

    #[inline]
    pub fn selected(&self) -> usize {
        self.selected
    }

    #[inline]
    pub fn bm_selected(&self) -> usize {
        self.bm_selected
    }

    #[inline]
    pub fn bm_scroll(&self) -> usize {
        self.bm_scroll
    }

    // set battery percentage for status display (used by session restore
    // since on_enter is not called during restore)
    pub fn set_battery(&mut self, battery_mv: u16) {
        self.bat_pct = battery::battery_percentage(battery_mv);
    }

    // restore home state from RTC session data
    //
    // this sets up ALL state needed to resume without calling on_enter().
    // on_enter() would reset state=Menu and selected=0, clobbering
    // the restored values.
    pub fn restore_state(
        &mut self,
        state_id: u8,
        selected: usize,
        bm_selected: usize,
        bm_scroll: usize,
    ) {
        self.state = match state_id {
            1 => HomeState::ShowBookmarks,
            _ => HomeState::Menu,
        };
        self.selected = selected;
        self.bm_selected = bm_selected;
        self.bm_scroll = bm_scroll;

        // trigger background loads for data we need to display
        self.needs_load_recent = true;
        if self.state == HomeState::ShowBookmarks {
            self.needs_load_bookmarks = true;
        }
        log::debug!(
            "home: restore_state state={:?} selected={}",
            self.state,
            selected
        );
    }

    pub fn load_recent(&mut self, k: &mut KernelHandle<'_>) {
        let mut buf = [0u8; 196];
        match k.read_app_data_start(RECENT_FILE, &mut buf) {
            Ok((_, n)) if n > 0 => self.parse_recent(&buf[..n]),
            _ => self.recent_book_len = 0,
        }
        // load reading stats and cover thumb for the recent book
        if self.recent_book_len > 0 {
            let fname =
                core::str::from_utf8(&self.recent_book[..self.recent_book_len]).unwrap_or("");
            if let Some((pages, time, _sessions)) = crate::apps::stats::load_book_stats(k, fname) {
                self.recent_stats_pages = pages;
                self.recent_stats_time = time;
            } else {
                self.recent_stats_pages = 0;
                self.recent_stats_time = 0;
            }
            // try to load cached cover thumbnail
            let dir_buf = crate::apps::cover_cache::cache_dir_for_filename(
                &self.recent_book[..self.recent_book_len],
            );
            let dir = smol_epub::cache::dir_name_str(&dir_buf);
            self.recent_cover = crate::apps::cover_cache::load_cover_thumb(k, dir);
            if self.recent_cover.is_some() {
                log::debug!("home: loaded cover thumbnail for recent book");
            }
        } else {
            self.recent_cover = None;
        }
        self.rebuild_item_count();
        self.needs_load_recent = false;
    }

    fn parse_recent(&mut self, data: &[u8]) {
        // format: filename\0title\0author\0progress_byte
        // fallback: if no \0 found, treat entire data as filename (old format)
        let mut fields = data.splitn(4, |&b| b == 0);

        // filename
        if let Some(fname) = fields.next() {
            let n = fname.len().min(32);
            self.recent_book[..n].copy_from_slice(&fname[..n]);
            self.recent_book_len = n;
        } else {
            self.recent_book_len = 0;
            return;
        }

        // title
        if let Some(title) = fields.next() {
            let n = title.len().min(64);
            self.recent_title[..n].copy_from_slice(&title[..n]);
            self.recent_title_len = n as u8;
        } else {
            self.recent_title_len = 0;
        }

        // author
        if let Some(author) = fields.next() {
            let n = author.len().min(64);
            self.recent_author[..n].copy_from_slice(&author[..n]);
            self.recent_author_len = n as u8;
        } else {
            self.recent_author_len = 0;
        }

        // progress (single byte after third \0)
        if let Some(rest) = fields.next() {
            self.recent_progress = rest.first().copied().unwrap_or(0);
        } else {
            self.recent_progress = 0;
        }
    }

    fn rebuild_item_count(&mut self) {
        // card (item 0) is always present; items 1..5 are menu buttons
        self.item_count = 6;
        if self.selected >= self.item_count {
            self.selected = 0;
        }
    }

    fn has_recent(&self) -> bool {
        self.recent_book_len > 0
    }

    fn recent_display_title(&self) -> &str {
        if self.recent_title_len > 0 {
            core::str::from_utf8(&self.recent_title[..self.recent_title_len as usize])
                .unwrap_or(self.recent_filename())
        } else {
            self.recent_filename()
        }
    }

    fn recent_filename(&self) -> &str {
        core::str::from_utf8(&self.recent_book[..self.recent_book_len]).unwrap_or("Book")
    }

    fn recent_author_str(&self) -> &str {
        if self.recent_author_len > 0 {
            core::str::from_utf8(&self.recent_author[..self.recent_author_len as usize])
                .unwrap_or("")
        } else {
            ""
        }
    }

    fn item_label(&self, idx: usize) -> &str {
        match idx {
            0 => "", // card, not drawn as button
            1 => "Files",
            2 => "Bookmarks",
            3 => "Stats",
            4 => "Settings",
            _ => "Upload",
        }
    }

    fn item_action(&self, idx: usize) -> MenuAction {
        match idx {
            0 => MenuAction::Continue,
            1 => MenuAction::Push(AppId::Files),
            2 => MenuAction::OpenBookmarks,
            3 => MenuAction::Push(AppId::Stats),
            4 => MenuAction::Push(AppId::Settings),
            _ => MenuAction::Push(AppId::Upload),
        }
    }

    fn selection_region(&self, idx: usize) -> Region {
        if idx == 0 {
            CARD_REGION
        } else {
            self.item_regions[idx - 1]
        }
    }

    fn move_selection(&mut self, delta: isize, ctx: &mut AppContext) {
        let count = self.item_count;
        if count == 0 {
            return;
        }
        let new = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        if new != self.selected {
            ctx.mark_dirty(self.selection_region(self.selected));
            self.selected = new;
            ctx.mark_dirty(self.selection_region(self.selected));
        }
    }

    fn bm_list_y(&self) -> u16 {
        BM_TITLE_Y + self.ui_fonts.heading.line_height + BM_HEADER_LIST_GAP
    }

    fn bm_visible_lines(&self) -> usize {
        let available = SCREEN_H.saturating_sub(self.bm_list_y());
        let rows = (available / BM_ROW_STRIDE) as usize;
        rows.max(1).min(bookmarks::SLOTS)
    }

    fn bm_row_region(&self, i: usize) -> Region {
        Region::new(
            LARGE_MARGIN,
            self.bm_list_y() + i as u16 * BM_ROW_STRIDE,
            FULL_CONTENT_W,
            BM_ROW_H,
        )
    }

    fn bm_list_region(&self) -> Region {
        let vis = self.bm_visible_lines();
        Region::new(
            LARGE_MARGIN,
            self.bm_list_y(),
            FULL_CONTENT_W,
            BM_ROW_STRIDE * vis as u16,
        )
    }

    fn bm_status_region(&self) -> Region {
        Region::new(
            BM_STATUS_X,
            BM_TITLE_Y,
            BM_STATUS_W,
            self.ui_fonts.heading.line_height,
        )
    }
}

impl App<AppId> for HomeApp {
    fn on_enter(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        ctx.clear_message();
        self.state = HomeState::Menu;
        self.selected = 0;
        self.bat_pct = battery::battery_percentage(k.battery_mv());
        ctx.mark_dirty(CONTENT_REGION);
    }

    fn on_resume(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        self.state = HomeState::Menu;
        self.selected = 0;
        self.needs_load_recent = true;
        self.bat_pct = battery::battery_percentage(k.battery_mv());
        ctx.mark_dirty(CONTENT_REGION);
    }

    async fn background(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        if self.needs_load_recent {
            let old_count = self.item_count;
            let mut buf = [0u8; 196];
            match k.read_app_data_start(RECENT_FILE, &mut buf) {
                Ok((_, n)) if n > 0 => self.parse_recent(&buf[..n]),
                _ => self.recent_book_len = 0,
            }
            // load reading stats for the recent book
            if self.recent_book_len > 0 {
                let fname =
                    core::str::from_utf8(&self.recent_book[..self.recent_book_len]).unwrap_or("");
                if let Some((pages, time, _sessions)) =
                    crate::apps::stats::load_book_stats(k, fname)
                {
                    self.recent_stats_pages = pages;
                    self.recent_stats_time = time;
                } else {
                    self.recent_stats_pages = 0;
                    self.recent_stats_time = 0;
                }
                // load cached cover thumbnail
                let dir_buf = crate::apps::cover_cache::cache_dir_for_filename(
                    &self.recent_book[..self.recent_book_len],
                );
                let dir = smol_epub::cache::dir_name_str(&dir_buf);
                self.recent_cover = crate::apps::cover_cache::load_cover_thumb(k, dir);
            } else {
                self.recent_cover = None;
            }
            self.rebuild_item_count();
            self.needs_load_recent = false;
            if self.item_count != old_count {
                ctx.request_full_redraw();
            }
        }

        if self.needs_load_bookmarks {
            self.bm_count = k.bookmark_cache().load_all(&mut self.bm_entries);
            // resolve titles from dir cache
            let _ = k.ensure_dir_cache_loaded();
            for i in 0..self.bm_count {
                let entry = &self.bm_entries[i];
                let fname = &entry.filename[..entry.name_len as usize];
                if let Some((title, len)) = k.dir_cache_mut().find_title(fname) {
                    let mut tbuf = [0u8; 96];
                    let n = (len as usize).min(96);
                    tbuf[..n].copy_from_slice(&title[..n]);
                    self.bm_entries[i].set_title(&tbuf[..n]);
                } else {
                    // inline humanize: lowercase all-upper SFN filenames
                    humanize_bm_entry(&mut self.bm_entries[i]);
                }
            }
            self.needs_load_bookmarks = false;
            if self.state == HomeState::ShowBookmarks {
                ctx.mark_dirty(self.bm_list_region());
            }
        }
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        match self.state {
            HomeState::Menu => self.on_event_menu(event, ctx),
            HomeState::ShowBookmarks => self.on_event_bookmarks(event, ctx),
        }
    }

    fn draw(&self, strip: &mut StripBuffer) {
        match self.state {
            HomeState::Menu => self.draw_menu(strip),
            HomeState::ShowBookmarks => self.draw_bookmarks(strip),
        }
    }
}

impl HomeApp {
    fn on_event_menu(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        match event {
            ActionEvent::Press(Action::Next) => {
                self.move_selection(1, ctx);
                Transition::None
            }
            ActionEvent::Press(Action::Prev) => {
                self.move_selection(-1, ctx);
                Transition::None
            }
            ActionEvent::Press(Action::Select) => match self.item_action(self.selected) {
                MenuAction::Continue => {
                    if self.has_recent() {
                        ctx.set_message(&self.recent_book[..self.recent_book_len]);
                    }
                    Transition::Push(AppId::Reader)
                }
                MenuAction::Push(app) => Transition::Push(app),
                MenuAction::OpenBookmarks => {
                    self.bm_selected = 0;
                    self.bm_scroll = 0;
                    self.needs_load_bookmarks = true;
                    self.state = HomeState::ShowBookmarks;
                    ctx.request_full_redraw();
                    Transition::None
                }
            },
            _ => Transition::None,
        }
    }

    fn on_event_bookmarks(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        match event {
            ActionEvent::Press(Action::Back) | ActionEvent::LongPress(Action::Back) => {
                self.state = HomeState::Menu;
                ctx.request_full_redraw();
                Transition::None
            }

            ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                if self.bm_count > 0 {
                    let old = self.bm_selected;
                    let vis = self.bm_visible_lines();
                    if self.bm_selected + 1 < self.bm_count {
                        self.bm_selected += 1;
                        if self.bm_selected >= self.bm_scroll + vis {
                            self.bm_scroll = self.bm_selected + 1 - vis;
                            ctx.mark_dirty(self.bm_list_region());
                        } else {
                            ctx.mark_dirty(self.bm_row_region(old - self.bm_scroll));
                            ctx.mark_dirty(self.bm_row_region(self.bm_selected - self.bm_scroll));
                        }
                    } else {
                        self.bm_selected = 0;
                        self.bm_scroll = 0;
                        ctx.mark_dirty(self.bm_list_region());
                    }
                    ctx.mark_dirty(self.bm_status_region());
                }
                Transition::None
            }

            ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                if self.bm_count > 0 {
                    let old = self.bm_selected;
                    let vis = self.bm_visible_lines();
                    if self.bm_selected > 0 {
                        self.bm_selected -= 1;
                        if self.bm_selected < self.bm_scroll {
                            self.bm_scroll = self.bm_selected;
                            ctx.mark_dirty(self.bm_list_region());
                        } else {
                            ctx.mark_dirty(self.bm_row_region(old - self.bm_scroll));
                            ctx.mark_dirty(self.bm_row_region(self.bm_selected - self.bm_scroll));
                        }
                    } else {
                        self.bm_selected = self.bm_count - 1;
                        if self.bm_selected >= vis {
                            self.bm_scroll = self.bm_selected + 1 - vis;
                        }
                        ctx.mark_dirty(self.bm_list_region());
                    }
                    ctx.mark_dirty(self.bm_status_region());
                }
                Transition::None
            }

            ActionEvent::Press(Action::NextJump) => {
                if self.bm_count > 0 {
                    let vis = self.bm_visible_lines();
                    self.bm_selected = (self.bm_selected + vis).min(self.bm_count - 1);
                    if self.bm_selected >= self.bm_scroll + vis {
                        self.bm_scroll = self.bm_selected + 1 - vis;
                    }
                    ctx.mark_dirty(self.bm_list_region());
                    ctx.mark_dirty(self.bm_status_region());
                }
                Transition::None
            }

            ActionEvent::Press(Action::PrevJump) => {
                let vis = self.bm_visible_lines();
                self.bm_selected = self.bm_selected.saturating_sub(vis);
                if self.bm_selected < self.bm_scroll {
                    self.bm_scroll = self.bm_selected;
                }
                ctx.mark_dirty(self.bm_list_region());
                ctx.mark_dirty(self.bm_status_region());
                Transition::None
            }

            ActionEvent::Press(Action::Select) => {
                if self.bm_count > 0 && self.bm_selected < self.bm_count {
                    let slot = &self.bm_entries[self.bm_selected];
                    ctx.set_message(&slot.filename[..slot.name_len as usize]);
                    self.state = HomeState::Menu;
                    Transition::Push(AppId::Reader)
                } else {
                    Transition::None
                }
            }

            _ => Transition::None,
        }
    }
}

impl HomeApp {
    fn draw_menu(&self, strip: &mut StripBuffer) {
        use embedded_graphics::pixelcolor::BinaryColor;
        use embedded_graphics::prelude::*;
        use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};

        const R_CARD: Size = Size::new(8, 8);
        const R_BTN: Size = Size::new(4, 4);

        // status bar: "pulp-os" left, battery right
        BitmapLabel::new(STATUS_TITLE_REGION, "pulp-os", self.ui_fonts.body)
            .alignment(Alignment::CenterLeft)
            .draw(strip)
            .unwrap();

        let mut bat_buf = BitmapDynLabel::<8>::new(STATUS_BAT_REGION, self.ui_fonts.body)
            .alignment(Alignment::CenterRight);
        let _ = write!(bat_buf, "{}%", self.bat_pct);
        bat_buf.draw(strip).unwrap();

        // book card (item 0) — always shown
        {
            let selected = self.selected == 0;
            let (bg, fg) = if selected {
                (BinaryColor::On, BinaryColor::Off)
            } else {
                (BinaryColor::Off, BinaryColor::On)
            };

            // card background + border
            let card_rect = Rectangle::new(
                Point::new(CARD_X as i32, CARD_Y as i32),
                Size::new(CARD_W as u32, CARD_H as u32),
            );
            RoundedRectangle::with_equal_corners(card_rect, R_CARD)
                .into_styled(PrimitiveStyle::with_fill(bg))
                .draw(strip)
                .unwrap();
            RoundedRectangle::with_equal_corners(card_rect, R_CARD)
                .into_styled(PrimitiveStyle::with_stroke(fg, 3))
                .draw(strip)
                .unwrap();

            let inner_x = CARD_X + CARD_PAD;
            let inner_w = CARD_W - 2 * CARD_PAD;
            let line_h = self.ui_fonts.body.line_height;
            let heading_h = self.ui_fonts.heading.line_height;

            if self.has_recent() {
                // layout: if we have a cover thumbnail, show it on the
                // left with text/stats on the right; otherwise use the
                // full-width centered text layout.
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
                        !selected, // invert when card is selected
                    );
                    let tx = inner_x + img.width + COVER_GAP;
                    let tw = (CARD_X + CARD_W - CARD_PAD).saturating_sub(tx);
                    (tx, tw, Alignment::TopLeft)
                } else {
                    (inner_x, inner_w, Alignment::Center)
                };

                // title (truncate with ellipsis if too wide)
                let title_y = CARD_Y + CARD_PAD + 20;
                let title_region = Region::new(text_x, title_y, text_w, heading_h);
                let title = self.recent_display_title();
                let title_cut = self.ui_fonts.heading.truncate_len(title, text_w);
                if title_cut >= title.len() {
                    self.ui_fonts
                        .heading
                        .draw_aligned(strip, title_region, title, text_align, fg);
                } else {
                    let mut tbuf = [0u8; 68]; // 64 + "…"
                    let n = title_cut.min(64);
                    tbuf[..n].copy_from_slice(&title.as_bytes()[..n]);
                    // append UTF-8 for '…' (0xE2 0x80 0xA6)
                    tbuf[n] = 0xE2;
                    tbuf[n + 1] = 0x80;
                    tbuf[n + 2] = 0xA6;
                    let truncated = core::str::from_utf8(&tbuf[..n + 3]).unwrap_or(title);
                    self.ui_fonts.heading.draw_aligned(
                        strip,
                        title_region,
                        truncated,
                        text_align,
                        fg,
                    );
                }

                // author (truncate with ellipsis if too wide)
                let author = self.recent_author_str();
                if !author.is_empty() {
                    let author_y = title_y + heading_h + 8;
                    let author_region = Region::new(text_x, author_y, text_w, line_h);
                    let author_cut = self.ui_fonts.body.truncate_len(author, text_w);
                    if author_cut >= author.len() {
                        self.ui_fonts.body.draw_aligned(
                            strip,
                            author_region,
                            author,
                            text_align,
                            fg,
                        );
                    } else {
                        let mut abuf = [0u8; 68];
                        let n = author_cut.min(64);
                        abuf[..n].copy_from_slice(&author.as_bytes()[..n]);
                        abuf[n] = 0xE2;
                        abuf[n + 1] = 0x80;
                        abuf[n + 2] = 0xA6;
                        let truncated = core::str::from_utf8(&abuf[..n + 3]).unwrap_or(author);
                        self.ui_fonts.body.draw_aligned(
                            strip,
                            author_region,
                            truncated,
                            text_align,
                            fg,
                        );
                    }
                }

                // progress bar
                let bar_y = CARD_Y + CARD_H - CARD_PAD - CARD_PROGRESS_H - line_h - 8;
                let bar_w = text_w as u32;
                let filled = (bar_w * self.recent_progress as u32) / 100;

                let bar_rect = Rectangle::new(
                    Point::new(text_x as i32, bar_y as i32),
                    Size::new(bar_w, CARD_PROGRESS_H as u32),
                );
                RoundedRectangle::with_equal_corners(bar_rect, Size::new(3, 3))
                    .into_styled(PrimitiveStyle::with_stroke(fg, 1))
                    .draw(strip)
                    .unwrap();

                if filled > 0 {
                    RoundedRectangle::with_equal_corners(
                        Rectangle::new(
                            Point::new(text_x as i32, bar_y as i32),
                            Size::new(filled, CARD_PROGRESS_H as u32),
                        ),
                        Size::new(3, 3),
                    )
                    .into_styled(PrimitiveStyle::with_fill(fg))
                    .draw(strip)
                    .unwrap();
                }

                // stats text: "342 pages · 5h 23m" or "X% read" if no stats
                let pct_y = bar_y + CARD_PROGRESS_H + 4;
                let pct_region = Region::new(text_x, pct_y, text_w, line_h);
                let mut pct_buf = BitmapDynLabel::<28>::new(pct_region, self.ui_fonts.body)
                    .alignment(text_align)
                    .inverted(selected);
                if self.recent_stats_pages > 0 || self.recent_stats_time > 0 {
                    let _ = write!(pct_buf, "{} pages", self.recent_stats_pages);
                    if self.recent_stats_time > 0 {
                        let hours = self.recent_stats_time / 3600;
                        let mins = (self.recent_stats_time % 3600) / 60;
                        if hours > 0 {
                            let _ = write!(pct_buf, " \u{b7} {}h {}m", hours, mins);
                        } else {
                            let _ = write!(pct_buf, " \u{b7} {}m", mins);
                        }
                    }
                } else {
                    let _ = write!(pct_buf, "{}% read", self.recent_progress);
                }
                pct_buf.draw(strip).unwrap();
            } else {
                // empty state
                let center_y = CARD_Y + (CARD_H - line_h) / 2;
                let center_region = Region::new(inner_x, center_y, inner_w, line_h);
                self.ui_fonts.body.draw_aligned(
                    strip,
                    center_region,
                    "No book opened yet",
                    Alignment::Center,
                    fg,
                );
            }
        }

        // menu items (1..4)
        for i in 1..self.item_count {
            let label = self.item_label(i);
            let region = self.item_regions[i - 1];
            let selected = i == self.selected;
            let (bg, fg) = if selected {
                (BinaryColor::On, BinaryColor::Off)
            } else {
                (BinaryColor::Off, BinaryColor::On)
            };

            let rect = Rectangle::new(
                Point::new(region.x as i32, region.y as i32),
                Size::new(region.w as u32, region.h as u32),
            );
            RoundedRectangle::with_equal_corners(rect, R_BTN)
                .into_styled(PrimitiveStyle::with_fill(bg))
                .draw(strip)
                .unwrap();
            self.ui_fonts
                .body
                .draw_aligned(strip, region, label, Alignment::Center, fg);
        }
    }

    fn draw_bookmarks(&self, strip: &mut StripBuffer) {
        let header_region = Region::new(
            LARGE_MARGIN,
            BM_TITLE_Y,
            HEADER_W,
            self.ui_fonts.heading.line_height,
        );
        BitmapLabel::new(header_region, "Bookmarks", self.ui_fonts.heading)
            .alignment(Alignment::CenterLeft)
            .draw(strip)
            .unwrap();

        if self.bm_count > 0 {
            let mut status = BitmapDynLabel::<20>::new(self.bm_status_region(), self.ui_fonts.body)
                .alignment(Alignment::CenterRight);
            let _ = write!(status, "{}/{}", self.bm_selected + 1, self.bm_count);
            status.draw(strip).unwrap();
        }

        if self.bm_count == 0 {
            BitmapLabel::new(self.bm_row_region(0), "No bookmarks", self.ui_fonts.body)
                .alignment(Alignment::CenterLeft)
                .draw(strip)
                .unwrap();
            return;
        }

        let vis = self.bm_visible_lines();
        let visible = vis.min(self.bm_count.saturating_sub(self.bm_scroll));

        for i in 0..vis {
            let region = self.bm_row_region(i);
            if i < visible {
                let idx = self.bm_scroll + i;
                let entry = &self.bm_entries[idx];
                let name = entry.display_name();

                BitmapLabel::new(region, name, self.ui_fonts.body)
                    .alignment(Alignment::CenterLeft)
                    .inverted(idx == self.bm_selected)
                    .draw(strip)
                    .unwrap();
            }
        }
    }
}

// humanize an all-uppercase SFN bookmark filename into the title field
fn humanize_bm_entry(entry: &mut BmListEntry) {
    let nlen = entry.name_len as usize;
    if nlen == 0 || entry.title_len > 0 {
        return;
    }
    let src = &entry.filename[..nlen];
    let all_upper = src.iter().all(|&b| !b.is_ascii_lowercase());
    if !all_upper {
        return;
    }
    let n = nlen.min(entry.title.len());
    let dot_pos = src.iter().position(|&b| b == b'.').unwrap_or(n);
    for i in 0..n {
        entry.title[i] = if i == 0 {
            src[i]
        } else if i > dot_pos {
            src[i].to_ascii_lowercase()
        } else {
            src[i].to_ascii_lowercase()
        };
    }
    entry.title_len = n as u8;
}
