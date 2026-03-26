// per-book reading statistics screen
//
// stats are stored as individual key=value files in _PULP/STATS/<filename>
// the screen shows global totals and a scrollable per-book list

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::PrimitiveStyle;

use crate::apps::{App, AppContext, AppId, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::ui::{
    Alignment, BUTTON_BAR_H, BitmapDynLabel, BitmapLabel, CONTENT_TOP, FULL_CONTENT_W, HEADER_W,
    LARGE_MARGIN, Region, SECTION_GAP, StackFmt, TITLE_Y_OFFSET, wrap_next, wrap_prev,
};

const MAX_BOOKS: usize = 20;

const ROW_H: u16 = 52;
const ROW_GAP: u16 = 4;
const ROW_STRIDE: u16 = ROW_H + ROW_GAP;

const TITLE_Y: u16 = CONTENT_TOP + TITLE_Y_OFFSET;
const HEADING_SUMMARY_GAP: u16 = SECTION_GAP;
const SUMMARY_LIST_GAP: u16 = SECTION_GAP;

const STATS_DIR: &str = "STATS";

// ── BookStats ────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct BookStats {
    filename: [u8; 32],
    filename_len: u8,
    title: [u8; 64],
    title_len: u8,
    pages: u32,
    time_secs: u32,
    sessions: u16,
}

impl BookStats {
    const EMPTY: Self = Self {
        filename: [0u8; 32],
        filename_len: 0,
        title: [0u8; 64],
        title_len: 0,
        pages: 0,
        time_secs: 0,
        sessions: 0,
    };

    fn display_name(&self) -> &str {
        let tlen = self.title_len as usize;
        if tlen > 0 {
            core::str::from_utf8(&self.title[..tlen]).unwrap_or(self.filename_str())
        } else {
            self.filename_str()
        }
    }

    fn filename_str(&self) -> &str {
        core::str::from_utf8(&self.filename[..self.filename_len as usize]).unwrap_or("?")
    }
}

// ── time formatting ──────────────────────────────────────────────────

fn fmt_compact_duration<const N: usize>(secs: u32, buf: &mut BitmapDynLabel<N>) {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        let _ = write!(buf, "{}h {}m", hours, mins);
    } else {
        let _ = write!(buf, "{}m", mins);
    }
}

// ── parsing ──────────────────────────────────────────────────────────

fn parse_stats(data: &[u8], stats: &mut BookStats) {
    for line in data.split(|&b| b == b'\n') {
        let line = trim_bytes(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let eq = match line.iter().position(|&b| b == b'=') {
            Some(p) => p,
            None => continue,
        };
        let key = trim_bytes(&line[..eq]);
        let val = trim_bytes(&line[eq + 1..]);
        match key {
            b"pages" => stats.pages = parse_u32(val),
            b"time" => stats.time_secs = parse_u32(val),
            b"sessions" => stats.sessions = parse_u32(val) as u16,
            _ => {}
        }
    }
}

fn parse_u32(s: &[u8]) -> u32 {
    let mut n: u32 = 0;
    for &b in s {
        if b.is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add((b - b'0') as u32);
        }
    }
    n
}

fn trim_bytes(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(start);
    &s[start..end]
}

// ── public helpers for reader to save/load stats ─────────────────────

pub fn save_book_stats(
    k: &mut KernelHandle<'_>,
    filename: &str,
    pages: u32,
    time_secs: u32,
    sessions: u16,
) -> crate::error::Result<()> {
    let mut buf = [0u8; 64];
    let mut fmt = StackFmt::<64>::new();
    let _ = write!(
        fmt,
        "pages={}\ntime={}\nsessions={}\n",
        pages, time_secs, sessions
    );
    let s = fmt.as_str().as_bytes();
    let len = s.len().min(buf.len());
    buf[..len].copy_from_slice(&s[..len]);
    k.sd().ensure_pulp_subdir(STATS_DIR)?;
    k.sd().write_in_pulp_subdir(STATS_DIR, filename, &buf[..len])
}

pub fn load_book_stats(k: &mut KernelHandle<'_>, filename: &str) -> Option<(u32, u32, u16)> {
    let mut buf = [0u8; 128];
    let n = k
        .sd().read_chunk_in_pulp_subdir(STATS_DIR, filename, 0, &mut buf)
        .ok()?;
    if n == 0 {
        return None;
    }
    let mut bs = BookStats::EMPTY;
    parse_stats(&buf[..n], &mut bs);
    if bs.pages == 0 && bs.time_secs == 0 && bs.sessions == 0 {
        return None;
    }
    Some((bs.pages, bs.time_secs, bs.sessions))
}

// ── StatsApp ─────────────────────────────────────────────────────────

pub struct StatsApp {
    books: [BookStats; MAX_BOOKS],
    book_count: usize,
    selected: usize,
    scroll: usize,
    loaded: bool,
    ui_fonts: fonts::UiFonts,
    summary_y: u16,
    list_y: u16,
    // aggregate
    total_pages: u32,
    total_time: u32,
    total_books: u16,
}

impl Default for StatsApp {
    fn default() -> Self {
        Self::new()
    }
}

impl StatsApp {
    pub fn new() -> Self {
        let uf = fonts::UiFonts::for_size(0);
        let summary_y = TITLE_Y + uf.heading.line_height + HEADING_SUMMARY_GAP;
        let list_y = summary_y + uf.body.line_height + SUMMARY_LIST_GAP;
        Self {
            books: [BookStats::EMPTY; MAX_BOOKS],
            book_count: 0,
            selected: 0,
            scroll: 0,
            loaded: false,
            ui_fonts: uf,
            summary_y,
            list_y,
            total_pages: 0,
            total_time: 0,
            total_books: 0,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
        self.summary_y = TITLE_Y + self.ui_fonts.heading.line_height + HEADING_SUMMARY_GAP;
        self.list_y = self.summary_y + self.ui_fonts.body.line_height + SUMMARY_LIST_GAP;
    }

    fn visible_rows(&self) -> usize {
        let avail = SCREEN_H.saturating_sub(self.list_y + BUTTON_BAR_H);
        let count = (avail / ROW_STRIDE) as usize;
        count.max(1)
    }

    fn scroll_into_view(&mut self) {
        let vis = self.visible_rows();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + vis {
            self.scroll = self.selected + 1 - vis;
        }
    }

    fn row_region(&self, vis_index: usize) -> Region {
        Region::new(
            LARGE_MARGIN,
            self.list_y + vis_index as u16 * ROW_STRIDE,
            FULL_CONTENT_W,
            ROW_H,
        )
    }

    fn list_region(&self) -> Region {
        let vis = self.visible_rows() as u16;
        Region::new(LARGE_MARGIN, self.list_y, FULL_CONTENT_W, ROW_STRIDE * vis)
    }

    fn load_stats(&mut self, k: &mut KernelHandle<'_>) {
        self.book_count = 0;
        self.total_pages = 0;
        self.total_time = 0;
        self.total_books = 0;

        // ensure dir cache is loaded so we can iterate known book files
        if k.ensure_dir_cache_loaded().is_err() {
            self.loaded = true;
            return;
        }

        // fetch all entries from the dir cache
        let mut entries = [crate::drivers::storage::DirEntry::EMPTY; 128];
        let page = k
            .dir_page(0, &mut entries)
            .unwrap_or(crate::drivers::storage::DirPage { total: 0, count: 0 });

        let mut buf = [0u8; 128];
        for i in 0..page.count {
            if self.book_count >= MAX_BOOKS {
                break;
            }
            let entry = &entries[i];
            if entry.is_dir {
                continue;
            }
            let fname = entry.name_str();

            // try to load stats for this file
            let n = match k.sd().read_chunk_in_pulp_subdir(STATS_DIR, fname, 0, &mut buf) {
                Ok(n) if n > 0 => n,
                _ => continue,
            };

            let idx = self.book_count;
            self.books[idx] = BookStats::EMPTY;

            // copy filename
            let fname_bytes = fname.as_bytes();
            let flen = fname_bytes.len().min(32);
            self.books[idx].filename[..flen].copy_from_slice(&fname_bytes[..flen]);
            self.books[idx].filename_len = flen as u8;

            // copy display title
            let display = entry.display_name();
            let dlen = display.len().min(64);
            self.books[idx].title[..dlen].copy_from_slice(&display.as_bytes()[..dlen]);
            self.books[idx].title_len = dlen as u8;

            parse_stats(&buf[..n], &mut self.books[idx]);

            // skip entries with zero stats
            if self.books[idx].pages == 0
                && self.books[idx].time_secs == 0
                && self.books[idx].sessions == 0
            {
                continue;
            }

            self.total_pages += self.books[idx].pages;
            self.total_time += self.books[idx].time_secs;
            self.total_books += 1;
            self.book_count += 1;
        }

        self.loaded = true;
    }
}

impl App<AppId> for StatsApp {
    fn on_enter(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        self.selected = 0;
        self.scroll = 0;
        self.loaded = false;
        ctx.mark_dirty(Region::new(
            0,
            CONTENT_TOP,
            SCREEN_W,
            SCREEN_H - CONTENT_TOP,
        ));
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        if self.book_count == 0 {
            return match event {
                ActionEvent::Press(Action::Back) => Transition::Pop,
                ActionEvent::LongPress(Action::Back) => Transition::Home,
                _ => Transition::None,
            };
        }

        let vis = self.visible_rows();

        match event {
            ActionEvent::Press(Action::Back) => Transition::Pop,
            ActionEvent::LongPress(Action::Back) => Transition::Home,

            ActionEvent::Press(Action::Next) => {
                let old_selected = self.selected;
                let old_scroll = self.scroll;
                self.selected = wrap_next(self.selected, self.book_count);
                if self.selected < old_selected {
                    self.scroll = 0;
                } else {
                    self.scroll_into_view();
                }
                if self.scroll != old_scroll {
                    ctx.mark_dirty(self.list_region());
                } else if self.selected != old_selected {
                    let old_vis = old_selected - old_scroll;
                    let new_vis = self.selected - self.scroll;
                    ctx.mark_dirty(self.row_region(old_vis));
                    ctx.mark_dirty(self.row_region(new_vis));
                }
                Transition::None
            }

            ActionEvent::Press(Action::Prev) => {
                let old_selected = self.selected;
                let old_scroll = self.scroll;
                self.selected = wrap_prev(self.selected, self.book_count);
                if self.selected > old_selected {
                    self.scroll = self.book_count.saturating_sub(vis);
                } else {
                    self.scroll_into_view();
                }
                if self.scroll != old_scroll {
                    ctx.mark_dirty(self.list_region());
                } else if self.selected != old_selected {
                    let old_vis = old_selected - old_scroll;
                    let new_vis = self.selected - self.scroll;
                    ctx.mark_dirty(self.row_region(old_vis));
                    ctx.mark_dirty(self.row_region(new_vis));
                }
                Transition::None
            }

            _ => Transition::None,
        }
    }

    async fn background(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        if !self.loaded {
            self.load_stats(k);
            ctx.request_full_redraw();
        }
    }

    fn draw(&self, strip: &mut StripBuffer) {
        // heading
        let header_region = Region::new(
            LARGE_MARGIN,
            TITLE_Y,
            HEADER_W,
            self.ui_fonts.heading.line_height,
        );
        BitmapLabel::new(header_region, "Reading Stats", self.ui_fonts.heading)
            .alignment(Alignment::CenterLeft)
            .draw(strip)
            .unwrap();

        // summary line
        let summary_region = Region::new(
            LARGE_MARGIN,
            self.summary_y,
            FULL_CONTENT_W,
            self.ui_fonts.body.line_height,
        );

        if !self.loaded {
            BitmapLabel::new(summary_region, "Loading...", self.ui_fonts.body)
                .alignment(Alignment::CenterLeft)
                .draw(strip)
                .unwrap();
            return;
        }

        if self.book_count == 0 {
            BitmapLabel::new(summary_region, "No reading stats yet", self.ui_fonts.body)
                .alignment(Alignment::CenterLeft)
                .draw(strip)
                .unwrap();
            return;
        }

        {
            let mut summary = BitmapDynLabel::<48>::new(summary_region, self.ui_fonts.body)
                .alignment(Alignment::CenterLeft);
            let _ = write!(summary, "{} book", self.total_books);
            if self.total_books != 1 {
                let _ = write!(summary, "s");
            }
            let _ = write!(summary, " \u{b7} {} pages \u{b7} ", self.total_pages);
            fmt_compact_duration(self.total_time, &mut summary);
            summary.draw(strip).unwrap();
        }

        // per-book list
        let vis = self.visible_rows();
        let visible_count = vis.min(self.book_count.saturating_sub(self.scroll));

        // approximate column split: left 2/3 for title, right 1/3 for stats
        let title_w = FULL_CONTENT_W * 2 / 3;
        let stat_w = FULL_CONTENT_W - title_w;

        for vi in 0..vis {
            let row_y = self.list_y + vi as u16 * ROW_STRIDE;

            if vi < visible_count {
                let idx = self.scroll + vi;
                let book = &self.books[idx];
                let selected = idx == self.selected;

                // title (left side)
                let title_region = Region::new(LARGE_MARGIN, row_y, title_w, ROW_H);
                BitmapLabel::new(title_region, book.display_name(), self.ui_fonts.body)
                    .alignment(Alignment::CenterLeft)
                    .inverted(selected)
                    .draw(strip)
                    .unwrap();

                // stats (right side): "342p · 12h 3m"
                let stat_region = Region::new(LARGE_MARGIN + title_w, row_y, stat_w, ROW_H);
                let mut stat_label = BitmapDynLabel::<24>::new(stat_region, self.ui_fonts.body)
                    .alignment(Alignment::CenterRight)
                    .inverted(selected);
                let _ = write!(stat_label, "{}p \u{b7} ", book.pages);
                fmt_compact_duration(book.time_secs, &mut stat_label);
                stat_label.draw(strip).unwrap();
            } else {
                // clear empty rows
                let region = Region::new(LARGE_MARGIN, row_y, FULL_CONTENT_W, ROW_H);
                region
                    .to_rect()
                    .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
                    .draw(strip)
                    .unwrap();
            }
        }
    }
}
