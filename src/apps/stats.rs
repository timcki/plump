// per-book reading statistics screen
//
// stats are stored as individual key=value files in _PLUMP/STATS/<filename>
// the screen shows global totals and a scrollable per-book list

use core::fmt::Write as _;

use crate::apps::{App, AppContext, AppId, Transition};
use crate::board::action::ActionEvent;
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::ui::{
    Alignment, BitmapDynLabel, BitmapLabel, CONTENT_TOP, FULL_CONTENT_W, LARGE_MARGIN, Region,
    StackFmt,
};

const MAX_BOOKS: usize = 20;

const STATS_DIR: &str = "STATS";

// ── BookStats ────────────────────────────────────────────────────────

/// Per-book reading statistics (pages turned, time spent, sessions).
///
/// Stored as key=value text files in `_PLUMP/STATS/<filename>`.
#[derive(Clone, Copy, Default)]
pub struct ReadingStats {
    pub pages: u32,
    pub time_secs: u32,
    pub sessions: u16,
}

impl ReadingStats {
    pub const EMPTY: Self = Self {
        pages: 0,
        time_secs: 0,
        sessions: 0,
    };

    /// Load stats for a book from SD.
    pub fn load(k: &mut KernelHandle<'_>, filename: &str) -> Option<Self> {
        let mut buf = [0u8; 128];
        let n = k
            .sd()
            .read_chunk_in_plump_subdir(STATS_DIR, filename, 0, &mut buf)
            .ok()?;
        if n == 0 {
            return None;
        }
        let stats = Self::parse(&buf[..n]);
        if stats.is_empty() {
            return None;
        }
        Some(stats)
    }

    /// Save stats for a book to SD.
    pub fn save(
        &self,
        k: &mut KernelHandle<'_>,
        filename: &str,
    ) -> crate::error::Result<()> {
        let mut buf = [0u8; 64];
        let mut fmt = StackFmt::<64>::new();
        let _ = write!(
            fmt,
            "pages={}\ntime={}\nsessions={}\n",
            self.pages, self.time_secs, self.sessions
        );
        let s = fmt.as_str().as_bytes();
        let len = s.len().min(buf.len());
        buf[..len].copy_from_slice(&s[..len]);
        k.sd().ensure_plump_subdir(STATS_DIR)?;
        k.sd()
            .write_in_plump_subdir(STATS_DIR, filename, &buf[..len])
    }

    /// Returns true if all fields are zero.
    pub fn is_empty(self) -> bool {
        self.pages == 0 && self.time_secs == 0 && self.sessions == 0
    }
}

#[derive(Clone, Copy)]
struct BookStats {
    filename: plump_kernel::util::FixedStr<32>,
    title: plump_kernel::util::FixedStr<64>,
    stats: ReadingStats,
}

impl BookStats {
    const EMPTY: Self = Self {
        filename: plump_kernel::util::FixedStr::EMPTY,
        title: plump_kernel::util::FixedStr::EMPTY,
        stats: ReadingStats::EMPTY,
    };

    fn display_name(&self) -> &str {
        if !self.title.is_empty() {
            self.title.as_str()
        } else {
            self.filename.as_str()
        }
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

impl ReadingStats {
    pub fn parse(data: &[u8]) -> Self {
        let mut stats = Self::EMPTY;
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
        stats
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



// ── StatsApp ─────────────────────────────────────────────────────────

pub struct StatsApp {
    books: [BookStats; MAX_BOOKS],
    book_count: usize,
    loaded: bool,
    ui_fonts: fonts::UiFonts,
    // top 3 books by time_secs, indices into `books`.
    top_books: [usize; 3],
    top_count: usize,
    // today's reading (refreshed in background_step from kernel.day_stats).
    today_pages: u16,
    today_secs: u32,
    // lifetime aggregates summed from per-book stats.
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
        Self {
            books: [BookStats::EMPTY; MAX_BOOKS],
            book_count: 0,
            loaded: false,
            ui_fonts: uf,
            top_books: [0; 3],
            top_count: 0,
            today_pages: 0,
            today_secs: 0,
            total_pages: 0,
            total_time: 0,
            total_books: 0,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
    }

    fn load_stats(&mut self, k: &mut KernelHandle<'_>) {
        self.book_count = 0;
        self.total_pages = 0;
        self.total_time = 0;
        self.total_books = 0;
        self.top_count = 0;

        if k.ensure_dir_cache_loaded().is_err() {
            self.loaded = true;
            return;
        }

        let mut entries = [crate::drivers::storage::DirEntry::EMPTY; 128];
        let page = k
            .dir_page(0, &mut entries)
            .unwrap_or(crate::drivers::storage::DirPage { total: 0, count: 0 });

        let mut buf = [0u8; 128];
        for entry in entries.iter().take(page.count) {
            if self.book_count >= MAX_BOOKS {
                break;
            }
            if entry.is_dir {
                continue;
            }
            let fname = entry.name_str();
            let n = match k
                .sd()
                .read_chunk_in_plump_subdir(STATS_DIR, fname, 0, &mut buf)
            {
                Ok(n) if n > 0 => n,
                _ => continue,
            };

            let idx = self.book_count;
            self.books[idx] = BookStats::EMPTY;
            self.books[idx].filename.set(fname.as_bytes());
            self.books[idx].title.set(entry.display_name().as_bytes());
            self.books[idx].stats = ReadingStats::parse(&buf[..n]);

            if self.books[idx].stats.is_empty() {
                continue;
            }

            self.total_pages += self.books[idx].stats.pages;
            self.total_time += self.books[idx].stats.time_secs;
            self.total_books += 1;
            self.book_count += 1;
        }

        // sort top 3 books by time_secs descending (insertion-style;
        // book_count is small).
        for i in 0..self.book_count {
            let t = self.books[i].stats.time_secs;
            // find insertion position in top_books (descending by time).
            let mut pos = self.top_count;
            for j in 0..self.top_count {
                if self.books[self.top_books[j]].stats.time_secs < t {
                    pos = j;
                    break;
                }
            }
            if pos < 3 {
                // shift right, insert.
                let end = self.top_count.min(2);
                for j in (pos..end).rev() {
                    self.top_books[j + 1] = self.top_books[j];
                }
                self.top_books[pos] = i;
                if self.top_count < 3 {
                    self.top_count += 1;
                }
            }
        }

        // pull today values from kernel (chunk F).
        let ds = k.day_stats();
        self.today_pages = ds.pages();
        self.today_secs = ds.secs_today();

        self.loaded = true;
    }
}

impl App<AppId> for StatsApp {
    fn on_enter(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        self.loaded = false;
        ctx.mark_dirty(Region::new(
            0,
            CONTENT_TOP,
            SCREEN_W,
            SCREEN_H - CONTENT_TOP,
        ));
    }

    fn on_event(&mut self, _event: ActionEvent, _ctx: &mut AppContext) -> Transition {
        // stats screen is a static dashboard; no cursor interaction.
        Transition::None
    }

    fn background_step(
        &mut self,
        ctx: &mut AppContext,
        k: &mut KernelHandle<'_>,
        _budget: crate::apps::BgBudget,
    ) -> crate::apps::BgOutcome {
        if !self.loaded {
            self.load_stats(k);
            ctx.request_full_redraw();
            return crate::apps::BgOutcome::Progress { more: false };
        }
        crate::apps::BgOutcome::Idle
    }

    fn draw(&self, strip: &mut StripBuffer) {
        let body = self.ui_fonts.body;
        let heading = self.ui_fonts.heading;

        // captions + metric rows, top-to-bottom.
        let mut y = CONTENT_TOP;

        // TODAY caption + two big numbers.
        draw_caption(strip, body, y, "TODAY");
        y += CAPTION_H + 4;
        draw_metric_row_two(strip, heading, body, y, self.today_pages as u32, "PAGES",
            self.today_secs, "READING");
        y += METRIC_ROW_H + SECTION_GAP_BIG;

        // LIFETIME caption + three big numbers.
        draw_caption(strip, body, y, "LIFETIME");
        y += CAPTION_H + 4;
        draw_metric_row_three(strip, heading, body, y,
            self.total_books as u32, "BOOKS",
            self.total_pages, "PAGES",
            self.total_time / 3600, "TOTAL");
        y += METRIC_ROW_H + SECTION_GAP_BIG;

        // MOST TIME SPENT caption + top 3 rows.
        draw_caption(strip, body, y, "MOST TIME SPENT");
        y += CAPTION_H + 4;
        for i in 0..self.top_count {
            let book_idx = self.top_books[i];
            let book = &self.books[book_idx];
            draw_top_book_row(strip, body, y, book.display_name(), book.stats.time_secs);
            y += TOP_ROW_H + 4;
        }

        if !self.loaded {
            // small "loading" hint at bottom; the chrome already shows
            // the user we're on the Stats tab.
            let r = Region::new(LARGE_MARGIN, y, FULL_CONTENT_W, body.line_height);
            BitmapLabel::new(r, "Loading...", body)
                .alignment(Alignment::CenterLeft)
                .draw(strip)
                .unwrap();
        } else if self.book_count == 0 && self.today_pages == 0 {
            let r = Region::new(LARGE_MARGIN, y + 8, FULL_CONTENT_W, body.line_height);
            BitmapLabel::new(r, "No reading yet", body)
                .alignment(Alignment::CenterLeft)
                .draw(strip)
                .unwrap();
        }
    }
}

// ── layout helpers ──────────────────────────────────────────────────

const CAPTION_H: u16 = 14;
const METRIC_ROW_H: u16 = 56;
const TOP_ROW_H: u16 = 44;
const SECTION_GAP_BIG: u16 = 14;

fn draw_caption(
    strip: &mut StripBuffer,
    font: &'static crate::fonts::bitmap::BitmapFont,
    y: u16,
    text: &str,
) {
    let r = Region::new(LARGE_MARGIN, y, FULL_CONTENT_W, CAPTION_H);
    BitmapLabel::new(r, text, font)
        .alignment(Alignment::CenterLeft)
        .draw(strip)
        .unwrap();
}

fn draw_metric_row_two(
    strip: &mut StripBuffer,
    big_font: &'static crate::fonts::bitmap::BitmapFont,
    small_font: &'static crate::fonts::bitmap::BitmapFont,
    y: u16,
    v1: u32,
    l1: &str,
    secs: u32,
    l2: &str,
) {
    let col_w = FULL_CONTENT_W / 2;
    // left: number / label.
    let r_num1 = Region::new(LARGE_MARGIN, y, col_w, big_font.line_height);
    let mut buf1 = BitmapDynLabel::<16>::new(r_num1, big_font).alignment(Alignment::CenterLeft);
    let _ = write!(buf1, "{}", v1);
    buf1.draw(strip).ok();
    let r_lbl1 = Region::new(
        LARGE_MARGIN,
        y + big_font.line_height,
        col_w,
        small_font.line_height,
    );
    BitmapLabel::new(r_lbl1, l1, small_font)
        .alignment(Alignment::CenterLeft)
        .draw(strip)
        .unwrap();

    // right: hh:mm / label.
    let r_num2 = Region::new(LARGE_MARGIN + col_w, y, col_w, big_font.line_height);
    let mut buf2 = BitmapDynLabel::<16>::new(r_num2, big_font).alignment(Alignment::CenterLeft);
    fmt_compact_duration(secs, &mut buf2);
    buf2.draw(strip).ok();
    let r_lbl2 = Region::new(
        LARGE_MARGIN + col_w,
        y + big_font.line_height,
        col_w,
        small_font.line_height,
    );
    BitmapLabel::new(r_lbl2, l2, small_font)
        .alignment(Alignment::CenterLeft)
        .draw(strip)
        .unwrap();
}

fn draw_metric_row_three(
    strip: &mut StripBuffer,
    big_font: &'static crate::fonts::bitmap::BitmapFont,
    small_font: &'static crate::fonts::bitmap::BitmapFont,
    y: u16,
    v1: u32,
    l1: &str,
    v2: u32,
    l2: &str,
    v3_h: u32,
    l3: &str,
) {
    let col_w = FULL_CONTENT_W / 3;
    let cells: [(u32, &str, bool); 3] = [(v1, l1, false), (v2, l2, false), (v3_h, l3, true)];
    for (i, (v, lbl, hrs)) in cells.iter().enumerate() {
        let x = LARGE_MARGIN + i as u16 * col_w;
        let r_num = Region::new(x, y, col_w, big_font.line_height);
        let mut buf = BitmapDynLabel::<16>::new(r_num, big_font).alignment(Alignment::CenterLeft);
        if *hrs {
            let _ = write!(buf, "{}h", v);
        } else {
            let _ = write!(buf, "{}", v);
        }
        buf.draw(strip).ok();
        let r_lbl = Region::new(x, y + big_font.line_height, col_w, small_font.line_height);
        BitmapLabel::new(r_lbl, lbl, small_font)
            .alignment(Alignment::CenterLeft)
            .draw(strip)
            .unwrap();
    }
}

fn draw_top_book_row(
    strip: &mut StripBuffer,
    font: &'static crate::fonts::bitmap::BitmapFont,
    y: u16,
    title: &str,
    secs: u32,
) {
    let title_w = FULL_CONTENT_W * 3 / 4;
    let time_w = FULL_CONTENT_W - title_w;
    let title_r = Region::new(LARGE_MARGIN, y, title_w, TOP_ROW_H);
    BitmapLabel::new(title_r, title, font)
        .alignment(Alignment::CenterLeft)
        .draw(strip)
        .unwrap();
    let time_r = Region::new(LARGE_MARGIN + title_w, y, time_w, TOP_ROW_H);
    let mut buf = BitmapDynLabel::<16>::new(time_r, font).alignment(Alignment::CenterRight);
    fmt_compact_duration(secs, &mut buf);
    buf.draw(strip).ok();
}
