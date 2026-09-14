// per-book reading statistics screen
//
// stats are stored as individual key=value files in _PLUMP/STATS/<filename>
// the screen shows global totals and a scrollable per-book list

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;

use crate::apps::widgets::row::{self, RowFonts, RowGroup, RowLead, RowSpec, ValueChip};
use crate::apps::{App, AppContext, AppId, Transition};
use crate::board::action::ActionEvent;
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::ui::{
    Alignment, BitmapLabel, CONTENT_TOP, FULL_CONTENT_W, LARGE_MARGIN, Region, SectionLabel,
    StackFmt,
};

const MAX_BOOKS: usize = 20;

/// Directory entries read per pass. One `DirEntry` is ~84 bytes, so
/// this is the trade between passes over the cache and stack.
const PAGE: usize = 20;

pub const STATS_DIR: &str = "STATS";

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
    top_books: [usize; TOP_N],
    top_count: usize,
    // today's reading (refreshed in background_step from kernel.day_stats).
    today_pages: u16,
    today_secs: u32,
    // lifetime aggregates summed from per-book stats.
    total_pages: u32,
    total_time: u32,
    total_books: u16,
    /// sessions across every book, for the lifetime group's sub line
    total_sessions: u32,
    /// books with a file on the card, which is not the same number as
    /// books that have ever been opened
    on_card: u16,
    /// a book's title is set in the book's own face
    book_font: &'static fonts::bitmap::BitmapFont,
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
            top_books: [0; TOP_N],
            top_count: 0,
            today_pages: 0,
            today_secs: 0,
            total_pages: 0,
            total_time: 0,
            total_books: 0,
            total_sessions: 0,
            on_card: 0,
            book_font: fonts::body_font(fonts::ReaderFont::Bookerly.family(), 1),
        }
    }

    /// A book's title is set in the book's own face, the way the
    /// reader's contents sheet sets a chapter's.
    pub fn set_reader_font(&mut self, font: fonts::ReaderFont) {
        self.book_font = fonts::body_font(font.family(), 1);
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
    }

    fn load_stats(&mut self, k: &mut KernelHandle<'_>) {
        self.book_count = 0;
        self.total_pages = 0;
        self.total_time = 0;
        self.total_books = 0;
        self.total_sessions = 0;
        self.on_card = 0;
        self.top_count = 0;

        if k.ensure_dir_cache_loaded().is_err() {
            self.loaded = true;
            return;
        }

        // paged rather than one 128-entry buffer: a DirEntry is ~84
        // bytes, so the old frame was 10.7 KB of stack, the largest on
        // the device, and all of it to look at one field per entry
        let mut page = [crate::drivers::storage::DirEntry::EMPTY; PAGE];
        let mut buf = [0u8; 128];
        let mut offset = 0usize;
        while let Ok(res) = k.dir_page(offset, &mut page) {
            if res.count == 0 {
                break;
            }
            if offset == 0 {
                self.on_card = res.total as u16;
            }
            for entry in page.iter().take(res.count) {
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
                self.total_sessions += self.books[idx].stats.sessions as u32;
                self.total_books += 1;
                self.book_count += 1;
            }
            offset += res.count;
            if offset >= res.total || self.book_count >= MAX_BOOKS {
                break;
            }
        }

        // sort the top books by time_secs descending (insertion-style;
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
            if pos < TOP_N {
                // shift right, insert.
                let end = self.top_count.min(TOP_N - 1);
                for j in (pos..end).rev() {
                    self.top_books[j + 1] = self.top_books[j];
                }
                self.top_books[pos] = i;
                if self.top_count < TOP_N {
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
        let chrome = fonts::chrome_font();
        let theme = plump_kernel::ui::Theme::default_v1();
        let row_fonts = RowFonts {
            text: body,
            small: chrome,
            icon: fonts::icon_font(1),
        };

        if !self.loaded {
            let r = Region::new(LARGE_MARGIN, CONTENT_TOP, FULL_CONTENT_W, body.line_height);
            BitmapLabel::new(r, "Loading...", body)
                .alignment(Alignment::CenterLeft)
                .draw(strip)
                .unwrap();
            return;
        }

        let mut y = CONTENT_TOP;

        // ── today: the reason to open this screen, so it keeps its
        // figures. three bordered cells at heading scale, not three
        // loose numerals at three times body size
        y = self.draw_caption(strip, &theme, y, "TODAY", "");
        self.draw_today(strip, y);
        y += self.fig_h() + CAPTION_LEAD;

        // ── lifetime: rows, which is what a record looks like. two of
        // the three carry a sub line, and a group is one height, so
        // all three are sized for the tallest of them
        y = self.draw_caption(strip, &theme, y, "LIFETIME", "");
        let life_row_h = row::row_height(body, chrome, true, false);
        let life = RowGroup::new(LARGE_MARGIN, y, FULL_CONTENT_W, 3, life_row_h);
        life.draw_outline(strip);

        let mut books_v = ValueBuf::new();
        let _ = write!(books_v, "{}", self.total_books);
        let mut books_sub = SubBuf::new();
        if self.on_card > 0 {
            let _ = write!(books_sub, "{} on the card", self.on_card);
        }
        life.draw_row(
            strip,
            0,
            &row_fonts,
            &RowSpec::label("Books read", books_v.as_str(), body).with_sub(books_sub.as_str()),
        );

        let mut pages_v = ValueBuf::new();
        write_grouped(&mut pages_v, self.total_pages);
        life.draw_row(
            strip,
            1,
            &row_fonts,
            &RowSpec::label("Pages turned", pages_v.as_str(), body),
        );

        let mut time_v = ValueBuf::new();
        write_long_duration(&mut time_v, self.total_time);
        let mut time_sub = SubBuf::new();
        if self.total_sessions > 0 {
            let _ = write!(time_sub, "across {} sessions", self.total_sessions);
        }
        life.draw_row(
            strip,
            2,
            &row_fonts,
            &RowSpec::label("Time reading", time_v.as_str(), body).with_sub(time_sub.as_str()),
        );
        y = life.bottom() + CAPTION_LEAD;

        // ── most time spent: the same row every other list uses, with
        // the bars comparing the books to the longest of them rather
        // than to the lifetime total, which at this scale would leave
        // three indistinguishable slivers
        if self.top_count == 0 {
            self.draw_empty(strip, y);
            return;
        }
        // a book row here carries a title in the book's own face, a
        // sub line and a bar; how many of them the page has left room
        // for is whatever the two groups above it did not take, which
        // moves with the UI font. the caption is only worth drawing
        // if at least one row follows it
        let top_row_h = row::row_height(self.book_font, chrome, true, true);
        let caption_h = self.ui_fonts.body.line_height + CAPTION_PAD;
        let below = theme.content_bottom().saturating_sub(y + caption_h);
        let shown = self.top_count.min((below / top_row_h) as usize);
        if shown == 0 {
            return;
        }
        y = self.draw_caption(strip, &theme, y, "MOST TIME SPENT", "");
        let longest = self.books[self.top_books[0]].stats.time_secs.max(1);
        let top = RowGroup::new(LARGE_MARGIN, y, FULL_CONTENT_W, shown, top_row_h);
        top.draw_outline(strip);
        for i in 0..shown {
            let book = &self.books[self.top_books[i]];
            let mut value = ValueBuf::new();
            write_duration(&mut value, book.stats.time_secs);
            let mut sub = SubBuf::new();
            if book.stats.sessions > 0 {
                let _ = write!(sub, "{} sessions", book.stats.sessions);
            }
            if book.stats.pages > 0 {
                if !sub.as_str().is_empty() {
                    let _ = sub.write_str(" \u{00B7} ");
                }
                let _ = write!(sub, "{} pages", book.stats.pages);
            }
            top.draw_row(
                strip,
                i,
                &row_fonts,
                &RowSpec {
                    lead: RowLead::Number(i as u16 + 1),
                    text: book.display_name(),
                    text_font: self.book_font,
                    value: value.as_str(),
                    selected: false,
                    sub: sub.as_str(),
                    progress: Some((book.stats.time_secs, longest)),
                    chip: ValueChip::None,
                },
            );
        }
    }
}

impl StatsApp {
    /// Tracked caption, its lead gap above and its pad below. Returns
    /// the y the group under it starts at.
    fn draw_caption(
        &self,
        strip: &mut StripBuffer,
        theme: &plump_kernel::ui::Theme,
        y: u16,
        left: &str,
        right: &str,
    ) -> u16 {
        let caption_h = self.ui_fonts.body.line_height;
        let r = Region::new(LARGE_MARGIN, y, FULL_CONTENT_W, caption_h);
        let mut p = plump_kernel::ui::Painter::new(strip, theme);
        SectionLabel::new(r, left).draw(&mut p, self.ui_fonts.body);
        if !right.is_empty() {
            SectionLabel::new(r, right)
                .right_aligned()
                .draw(&mut p, fonts::chrome_font());
        }
        y + caption_h + CAPTION_PAD
    }

    /// Height of a Today cell: the figure, the caption under it, and
    /// the pad that keeps the two off each other and off the border.
    /// It used to be a flat 62, which at the default UI font left the
    /// caption nine pixels inside the figure it sits under.
    fn fig_h(&self) -> u16 {
        2 * FIG_PAD
            + self.ui_fonts.heading.line_height
            + FIG_LEAD
            + fonts::chrome_font().line_height
    }

    /// Three bordered cells: the count, the time, and the pace between
    /// them. A rate with nothing behind it prints as an em dash rather
    /// than a zero, which would read as a measurement.
    fn draw_today(&self, strip: &mut StripBuffer, y: u16) {
        let heading = self.ui_fonts.heading;
        let chrome = fonts::chrome_font();
        let fig_h = self.fig_h();
        let cell_w = (FULL_CONTENT_W - 2 * FIG_GAP) / 3;

        let mut pages = ValueBuf::new();
        let _ = write!(pages, "{}", self.today_pages);

        let mut time = ValueBuf::new();
        if self.today_secs > 0 {
            write_duration(&mut time, self.today_secs);
        } else {
            let _ = time.write_str("\u{2014}");
        }

        let mut pace = ValueBuf::new();
        match self.pace_secs_per_page() {
            Some(secs) => write_pace(&mut pace, secs),
            None => {
                let _ = pace.write_str("\u{2014}");
            }
        }

        for (i, (value, label)) in [
            (pages.as_str(), "PAGES"),
            (time.as_str(), "READING"),
            (pace.as_str(), "A PAGE"),
        ]
        .into_iter()
        .enumerate()
        {
            let x = LARGE_MARGIN + i as u16 * (cell_w + FIG_GAP);
            let cell = Region::new(x, y, cell_w, fig_h);
            row::draw_group_outline(strip, cell);
            heading.draw_aligned(
                strip,
                Region::new(x + FIG_PAD, y + FIG_PAD, cell_w - 2 * FIG_PAD, heading.line_height),
                value,
                Alignment::CenterLeft,
                BinaryColor::On,
            );
            chrome.draw_aligned(
                strip,
                Region::new(
                    x + FIG_PAD,
                    y + fig_h - FIG_PAD - chrome.line_height,
                    cell_w - 2 * FIG_PAD,
                    chrome.line_height,
                ),
                label,
                Alignment::CenterLeft,
                BinaryColor::On,
            );
        }
    }

    /// Seconds a page took today. Needs enough of a sample to mean
    /// something: the same floor the sleep card's pace uses.
    fn pace_secs_per_page(&self) -> Option<u32> {
        if self.today_pages < MIN_PACE_PAGES || self.today_secs < MIN_PACE_SECS {
            return None;
        }
        Some(self.today_secs / self.today_pages as u32)
    }

    fn draw_empty(&self, strip: &mut StripBuffer, y: u16) {
        let line = Region::new(LARGE_MARGIN, y + 24, FULL_CONTENT_W, self.book_font.line_height);
        self.book_font.draw_aligned(
            strip,
            line,
            "No reading recorded yet.",
            Alignment::Center,
            BinaryColor::On,
        );
        let chrome = fonts::chrome_font();
        chrome.draw_aligned(
            strip,
            Region::new(
                LARGE_MARGIN,
                y + 24 + self.book_font.line_height + 4,
                FULL_CONTENT_W,
                chrome.line_height,
            ),
            "Open a book and this page fills itself in.",
            Alignment::Center,
            BinaryColor::On,
        );
    }
}

// ── layout helpers ──────────────────────────────────────────────────

// a caption sits close to the group it labels and far from the one
// above it
const CAPTION_LEAD: u16 = 13;
const CAPTION_PAD: u16 = 6;

// today's three cells
const FIG_GAP: u16 = 10;
const FIG_PAD: u16 = 10;
/// between the figure and the caption under it
const FIG_LEAD: u16 = 4;

/// Books the most-time-spent list will hold at most. How many
/// actually fit is worked out from what is left of the page after
/// the two groups above it, which moves with the UI font: the cap is
/// only there to bound the scan.
const TOP_N: usize = 6;

/// A pace under this much of a sample is noise, so the cell says
/// nothing rather than reporting it.
const MIN_PACE_PAGES: u16 = 5;
const MIN_PACE_SECS: u32 = 120;

type ValueBuf = StackFmt<20>;
type SubBuf = StackFmt<40>;

// ── formatting ──────────────────────────────────────────────────────

/// `1h 12m`, or minutes alone under the hour.
fn write_duration(out: &mut impl core::fmt::Write, secs: u32) {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        let _ = write!(out, "{}h {}m", hours, mins);
    } else {
        let _ = write!(out, "{}m", mins);
    }
}

/// A lifetime total, where minutes stopped mattering: `187h`.
fn write_long_duration(out: &mut impl core::fmt::Write, secs: u32) {
    let hours = secs / 3600;
    if hours > 0 {
        let _ = write!(out, "{}h", hours);
    } else {
        let _ = write!(out, "{}m", secs / 60);
    }
}

/// Seconds a page, as `1m 32s` or `48s`.
fn write_pace(out: &mut impl core::fmt::Write, secs: u32) {
    if secs >= 60 {
        let _ = write!(out, "{}m {}s", secs / 60, secs % 60);
    } else {
        let _ = write!(out, "{}s", secs);
    }
}

/// Thousands separated, so a five-figure page count stays readable:
/// `4,728`. No allocation and no float, which rules out the usual
/// tricks.
fn write_grouped(out: &mut impl core::fmt::Write, mut n: u32) {
    // at most 10 digits in a u32, so 4 groups of 3
    let mut groups = [0u32; 4];
    let mut count = 0;
    loop {
        groups[count] = n % 1000;
        n /= 1000;
        count += 1;
        if n == 0 || count == groups.len() {
            break;
        }
    }
    let _ = write!(out, "{}", groups[count - 1]);
    for i in (0..count - 1).rev() {
        let _ = write!(out, ",{:03}", groups[i]);
    }
}
