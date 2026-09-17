mod epubs;
mod images;
mod layout;
mod paging;

pub use plump_kernel::util::decode_utf8_char;
use plump_kernel::util::FixedStr;

use crate::apps::MSG_TAG_OPEN_CONTENTS;
use crate::apps::PendingSetting;
use crate::apps::widgets::row;
use crate::apps::widgets::sheet::{
    self, HintSlot, RowLead, RowSpec, SheetFonts, SheetGeom, ValueChip,
};
use crate::apps::recent::{self, RecentRecord};
use crate::fonts::bitmap::{self, BitmapFont};

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_9X18;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::Text;

use crate::apps::{App, AppContext, AppId, BgBudget, BgOutcome, DeferredPersistenceReason, RECENT_FILE, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::{GrayMode, StripBuffer};
use crate::error::{Error, ErrorKind};
use crate::fonts;
use crate::fonts::ReaderFont;
use crate::kernel::KernelHandle;
use crate::kernel::QuickAction;
use crate::kernel::bookmarks;
use crate::kernel::work_queue;
use crate::kernel::work_queue::DecodedImage;
use crate::ui::{Alignment, Region, StackFmt};
use smol_epub::cache;
use smol_epub::epub::{self, EpubMeta, EpubSpine, EpubToc, MAX_SPINE, TocSource};
use smol_epub::markup::{self, ImageRef, Style as MarkupStyle};
use layout::LineLayout;
use smol_epub::zip::{self, ZipIndex};

// chrome margin: used for bottom info bar, loading indicator.
// this never changes; only the text content area responds to the reading theme.
pub(super) const MARGIN: u16 = 8;

// screen edge padding (display clips pixels at very edge)
pub(super) const SCREEN_PAD: u16 = 4;

// the reader's one line of chrome: the book's title on the left and
// how far through the book you are on the right, drawn as the sleep
// card's tube bar (mockups/xteink_x4_reader_footer.html, option A).
//
// it sits under the page. it was tried at the top, on the line every
// other screen puts its own chrome on, and reads worse there: over a
// column of prose a header is a thing to look past on the way in,
// where a footer is a thing to glance at on the way out.
//
// the chapter used to live here too -- a fill bar for the position
// inside it, and the chapter number in the corner -- and neither is
// here now. the Menu sheet names the chapter in its header and gives
// its number as the Contents row's value, which is where you look
// when you want it; on the page it was two figures competing with the
// one that answers "how much is left".
pub(super) const CHROME_H: u16 = 18;
pub(super) const CHROME_PAD: u16 = 4;
// clear of the bottom edge: the display clips its last rows and the
// line should read as a footer, not as the last line of the page
const FOOTER_BOTTOM_PAD: u16 = 12;
pub(super) const CHROME_Y: u16 = SCREEN_H - FOOTER_BOTTOM_PAD - CHROME_H;

// reader text always starts just below the physical screen pad: the
// shared top status bar is never shown in the reader (its per-turn
// repaints cost a separate refresh), so the full height belongs to
// the page. the chrome setting only affects the bottom info bar.
pub(super) const TEXT_Y: u16 = SCREEN_PAD + 4;

pub(super) const LINE_H: u16 = 20;

pub(super) const CHARS_PER_LINE: usize = 51;

pub(super) const LINES_PER_PAGE: usize = 37;

pub(super) const PAGE_BUF: usize = 8192;

pub(super) const MAX_PAGES: usize = 512;

// the position tube in the right corner, and the gap between it and
// whatever the title leaves
const BAR_GAP: u16 = 12;
const BAR_W: u16 = 96;
pub(super) const FOOTER_REGION: Region =
    Region::new(MARGIN, CHROME_Y, SCREEN_W - 2 * MARGIN, CHROME_H);

// loading stage row: a tracked caption with the percent on the
// right and a hairline bar beneath, sitting above the footer. the
// only part of a loading screen that repaints while a book opens
const STAGE_H: u16 = 34;
const STAGE_CAP_H: u16 = 18;
const STAGE_BOTTOM_GAP: u16 = 20;
pub(super) const STAGE_REGION: Region = Region::new(
    MARGIN,
    CHROME_Y - CHROME_PAD - STAGE_BOTTOM_GAP - STAGE_H,
    SCREEN_W - 2 * MARGIN,
    STAGE_H,
);

// how long a loading state may run before its screen is painted:
// anything faster lands as one refresh, the page itself. a wake
// starts from the sleep wallpaper, which is fine to look at, and
// its bundle reopen routinely takes a second: hold longer there so
// the plate does not flash for a moment before the page
const LOADING_HOLD_MS: u64 = 400;
const LOADING_HOLD_RESUME_MS: u64 = 1500;

// contents sheet: rows in the bottom-anchored state
const CONTENTS_ROWS: usize = 8;

pub(super) const PAGE_REGION: Region = Region::new(0, 0, SCREEN_W, SCREEN_H);

pub(super) const NO_PREFETCH: usize = usize::MAX;

pub(super) const TEXT_W: u32 = (SCREEN_W - 2 * MARGIN) as u32;

pub(super) const TEXT_AREA_H: u16 = CHROME_Y - CHROME_PAD - TEXT_Y;

pub(super) const EOCD_TAIL: usize = 512;

pub(super) const INDENT_PX: u32 = 24;

// the per-page run table: a line rarely changes style more than a few
// times; past the per-line cap the rest of the line joins the last run
pub(super) const MAX_RUNS_PER_LINE: usize = 6;
pub(super) const MAX_PAGE_RUNS: usize = LINES_PER_PAGE * MAX_RUNS_PER_LINE;

// max inline images tracked per page buffer for dimension pre-scan
pub(super) const MAX_IMAGES_PER_PAGE: usize = 8;

// default image height budget (half text area) used when actual
// dimensions are unavailable (e.g. uncached deflated images, or
// during preindex_all_pages where no pre-scan runs)
pub(super) const DEFAULT_IMG_H: u16 = 350;

// how many times a decode may halve its target size when the heap
// cannot hold the output plane, and the dimension below which a
// further step is not worth the SD pass
pub(super) const IMG_BUDGET_STEPS: u8 = 2;
pub(super) const IMG_BUDGET_MIN: u16 = 48;

// consecutive large-image decode failures before the reader stops
// trying them for this session
pub(super) const LARGE_IMG_FAIL_LIMIT: u8 = 2;

// inline images are capped at this fraction of the text area height.
// keeps illustrations proportional to surrounding text, similar to
// Kindle / Apple Books.  fullscreen images (sole content on a page)
// are not affected — they use the full text_area_h budget.
pub(super) const INLINE_IMG_MAX_PCT: u16 = 40;

#[inline]
pub(super) fn inline_img_max_h(text_area_h: u16) -> u16 {
    ((text_area_h as u32 * INLINE_IMG_MAX_PCT as u32) / 100) as u16
}

// 128 KB. Sized to hold long-novella chapters like Stories of Your Life's
// "Seventy-Two Letters" (~109 KB stripped). Old 96 KB cap silently rejected
// such chapters and degraded the reader to a single greedy page (the rest
// of the chapter became unreachable). See greedy_preindex_compute for the
// bundle-streamed fallback when even this cap is exceeded.
pub(super) const CHAPTER_CACHE_MAX: usize = 131072;

// images <= this size are dispatched to async worker for decoding;
// images > this size are decoded on main loop via streaming SD reads
pub(super) const PRECACHE_IMG_MAX: u32 = 30 * 1024;

const POSITION_OVERLAY_W: u16 = 280;
const POSITION_OVERLAY_H: u16 = 40;
pub(super) const POSITION_OVERLAY: Region = Region::new(
    (SCREEN_W - POSITION_OVERLAY_W) / 2,
    (SCREEN_H - POSITION_OVERLAY_H) / 2,
    POSITION_OVERLAY_W,
    POSITION_OVERLAY_H,
);

const LOADING_W: u16 = SCREEN_W - 2 * MARGIN - 16;
const LOADING_H: u16 = 24;
pub(super) const LOADING_REGION: Region = Region::new(MARGIN, TEXT_Y, LOADING_W, LOADING_H);

pub const QA_FONT_SIZE: u8 = 1;
pub(super) const QA_PREV_CHAPTER: u8 = 3;
pub(super) const QA_NEXT_CHAPTER: u8 = 4;
pub(super) const QA_TOC: u8 = 5;

pub(super) const QA_MAX: usize = 4;

// reader state machine:
// NeedBookmark -> NeedInit -> NeedOpf -> NeedToc -> NeedCache -> NeedIndex -> NeedPage -> Ready
// Ready <-> ShowToc (toc overlay); any state -> Error on failure
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum State {
    NeedBookmark,
    NeedInit,
    NeedOpf,
    NeedToc,
    NeedCache,
    NeedIndex,
    NeedPage,
    Ready,
    ShowToc,
    Error,
}

// background caching progress, runs independently of the reading
// state so the user can read while chapters/images are cached
#[derive(Clone, Copy, PartialEq)]
pub(super) enum BgCacheState {
    // nothing to do
    Idle,
    CacheChapter,
    WaitNearbyImage,
    CacheImage,
    WaitImage,
}

#[derive(Clone, Copy)]
pub(super) struct LineSpan {
    pub(super) start: u16,
    pub(super) len: u16,
    pub(super) flags: u8,
    pub(super) indent: u8,
    pub(super) align: u8,
    // mirrors `LineLayout::extra` encoding: bit 7 sign (1 = shrink), bits
    // 0-6 magnitude. 0 means "no justify direction signaled" — greedy
    // lines and K-P Perfect/Overflow lines all land here. The renderer
    // re-derives the actual per-gap magnitude from measure_line at draw
    // time; only the sign bit affects the justify decision.
    pub(super) extra: u8,
}

impl LineSpan {
    pub(super) const EMPTY: Self = Self {
        start: 0,
        len: 0,
        flags: 0,
        indent: 0,
        align: Self::ALIGN_DEFAULT,
        extra: 0,
    };

    pub(super) const EXTRA_SIGN_SHRINK: u8 = 0x80;

    #[inline]
    pub(super) fn extra_is_shrink(&self) -> bool {
        self.extra & Self::EXTRA_SIGN_SHRINK != 0
    }

    pub(super) const FLAG_BOLD: u8 = 1 << 0;
    pub(super) const FLAG_ITALIC: u8 = 1 << 1;
    pub(super) const FLAG_HEADING: u8 = 1 << 2;
    pub(super) const FLAG_IMAGE: u8 = 1 << 3;

    // per-line alignment, set by ALIGN_* markers in the content stream.
    // ALIGN_DEFAULT honors the user's text_alignment setting (typically
    // justify); the others override it for that line only.
    pub(super) const ALIGN_DEFAULT: u8 = 0;
    pub(super) const ALIGN_CENTER: u8 = 2;
    pub(super) const ALIGN_RIGHT: u8 = 3;

    // Line-ending kind, stored in bits 4-5:
    //   00 = BufferEnd (last line, end of page buffer or chapter)
    //   01 = HardBreak (line ended at \n)
    //   10 = SoftWrap  (line ended by word-wrap overflow)
    //   11 = reserved
    pub(super) const END_SHIFT: u8 = 4;
    pub(super) const END_MASK: u8 = 0b11 << Self::END_SHIFT;
    pub(super) const END_BUFFER: u8 = 0 << Self::END_SHIFT;
    pub(super) const END_HARD: u8 = 1 << Self::END_SHIFT;
    pub(super) const END_SOFT: u8 = 2 << Self::END_SHIFT;

    // Heading level, stored in bits 6-7 (only meaningful when FLAG_HEADING set):
    //   00 = h3-tier  (heading font, left-aligned)
    //   01 = h2-tier  (heading font, centered)
    //   10 = h1-tier  (heading font, centered, page-break-before)
    //   11 = h4-h6    (bold body face)
    pub(super) const HLEVEL_SHIFT: u8 = 6;
    pub(super) const HLEVEL_MASK: u8 = 0b11 << Self::HLEVEL_SHIFT;
    pub(super) const HLEVEL_H3: u8 = 0 << Self::HLEVEL_SHIFT;
    pub(super) const HLEVEL_H2: u8 = 1 << Self::HLEVEL_SHIFT;
    pub(super) const HLEVEL_H1: u8 = 2 << Self::HLEVEL_SHIFT;

    // `indent` and `align` share `LineLayout`'s encoding: left levels
    // and first-line indent nibbles; alignment, underline / strike
    // line-start bits and the gap-above nibble
    #[inline]
    pub(super) fn left_levels(&self) -> u8 {
        self.indent & LineLayout::LEFT_MASK
    }

    #[inline]
    pub(super) fn first_indent_qem(&self) -> u8 {
        self.indent >> LineLayout::FIRST_INDENT_SHIFT
    }

    #[inline]
    pub(super) fn align(&self) -> u8 {
        self.align & LineLayout::ALIGN_MASK
    }

    #[inline]
    pub(super) fn gap_qem(&self) -> u8 {
        self.align >> LineLayout::GAP_SHIFT
    }

    /// the inline style at the line's first byte; seeds the decoder
    pub(super) fn start_style(&self) -> MarkupStyle {
        let heading = if self.flags & Self::FLAG_HEADING != 0 {
            self.start_hlevel()
        } else {
            0
        };
        MarkupStyle {
            bold: self.flags & Self::FLAG_BOLD != 0,
            italic: self.flags & Self::FLAG_ITALIC != 0,
            underline: self.align & LineLayout::START_UNDERLINE != 0,
            strike: self.align & LineLayout::START_STRIKE != 0,
            heading,
        }
    }

    /// flags for a line whose first byte is drawn in `style`
    pub(super) fn flags_for(style: MarkupStyle, end: u8) -> u8 {
        LineLayout::style_flags(style) | end
    }

    #[inline]
    pub(super) fn is_soft_wrap(&self) -> bool {
        (self.flags & Self::END_MASK) == Self::END_SOFT
    }

    #[inline]
    pub(super) fn is_image(&self) -> bool {
        self.flags & Self::FLAG_IMAGE != 0
    }

    #[inline]
    pub(super) fn is_image_origin(&self) -> bool {
        self.is_image() && self.len > 0
    }

    pub(super) fn style(&self) -> fonts::Style {
        let bold = self.flags & Self::FLAG_BOLD != 0;
        let italic = self.flags & Self::FLAG_ITALIC != 0;
        let heading = self.flags & Self::FLAG_HEADING != 0;
        let hlevel = if heading { self.start_hlevel() } else { 0 };
        fonts::Style::from_flags(bold, italic, heading, hlevel)
    }

    /// Decode the line's initial heading level from `HLEVEL_*` flag
    /// bits. Only meaningful when `FLAG_HEADING` is set; returns the
    /// 1-based level (1 / 2 / 3). h4-h6 collapse to 3 in the
    /// LineLayout encoding today — see plan note.
    #[inline]
    pub(super) fn start_hlevel(&self) -> u8 {
        match self.flags & Self::HLEVEL_MASK {
            Self::HLEVEL_H1 => 1,
            Self::HLEVEL_H2 => 2,
            Self::HLEVEL_H3 => 3,
            _ => 4,
        }
    }

    #[inline]
    pub(super) fn is_centered(&self) -> bool {
        self.align() == Self::ALIGN_CENTER
    }

    #[inline]
    pub(super) fn is_right_aligned(&self) -> bool {
        self.align() == Self::ALIGN_RIGHT
    }

    #[inline]
    pub(super) fn is_explicit_align(&self) -> bool {
        self.align() != Self::ALIGN_DEFAULT
    }

}

/// one placed style run of a page line: a byte-contiguous stretch of
/// text under one inline style, its x after justification and the
/// index of its first inter-word gap (for the remainder pixels). a
/// run ends where the next one starts, the line's last at `line_x_end`
#[derive(Clone, Copy)]
pub(super) struct Run {
    pub(super) start: u16,
    pub(super) len: u16,
    pub(super) x: i16,
    /// packed `markup::Style`
    pub(super) style: u8,
    pub(super) gap0: u8,
}

impl Run {
    pub(super) const EMPTY: Self = Self {
        start: 0,
        len: 0,
        x: 0,
        style: 0,
        gap0: 0,
    };
}

// page index, content buffer, and read-ahead state
pub(super) struct PageState {
    pub(super) offsets: [u32; MAX_PAGES],
    pub(super) total_pages: usize,
    pub(super) fully_indexed: bool,

    pub(super) page: usize,
    pub(super) buf: [u8; PAGE_BUF],
    pub(super) buf_len: usize,
    pub(super) lines: [LineSpan; LINES_PER_PAGE],
    pub(super) line_count: usize,

    /// Per-line results of `build_page_runs`: top y within the text
    /// area, x where the last run ends, per-gap justification (extra
    /// px, remainder), and the run table slice
    pub(super) line_y: [u16; LINES_PER_PAGE],
    pub(super) line_x_end: [i16; LINES_PER_PAGE],
    pub(super) line_hyphen: [bool; LINES_PER_PAGE],
    pub(super) line_just: [(i16, i16); LINES_PER_PAGE],
    pub(super) run_first: [u16; LINES_PER_PAGE],
    pub(super) run_len: [u8; LINES_PER_PAGE],
    pub(super) runs: [Run; MAX_PAGE_RUNS],
    pub(super) run_count: usize,

    pub(super) prefetch: Vec<u8>,
    pub(super) prefetch_len: usize,
    pub(super) prefetch_page: usize,

    // ── K-P pipeline (algo_version 2) ──────────────────────────────
    // populated by `run_kp_typeset()`; when non-empty, NeedPage uses
    // these instead of the greedy wrap path. Empty for .txt files
    // and as the steady state before K-P runs / after invalidation.
    pub(super) chapter_lines: Vec<layout::LineLayout>,
    pub(super) kp_pages: Vec<layout::PageLayout>,
    pub(super) image_block_lines: Vec<u8>,
}

impl PageState {
    pub(super) const fn new() -> Self {
        Self {
            offsets: [0u32; MAX_PAGES],
            total_pages: 0,
            fully_indexed: false,
            page: 0,
            buf: [0u8; PAGE_BUF],
            buf_len: 0,
            lines: [LineSpan::EMPTY; LINES_PER_PAGE],
            line_count: 0,
            line_y: [0u16; LINES_PER_PAGE],
            line_x_end: [0i16; LINES_PER_PAGE],
            line_hyphen: [false; LINES_PER_PAGE],
            line_just: [(0i16, 0i16); LINES_PER_PAGE],
            run_first: [0u16; LINES_PER_PAGE],
            run_len: [0u8; LINES_PER_PAGE],
            runs: [Run::EMPTY; MAX_PAGE_RUNS],
            run_count: 0,
            prefetch: Vec::new(),
            prefetch_len: 0,
            prefetch_page: NO_PREFETCH,
            chapter_lines: Vec::new(),
            kp_pages: Vec::new(),
            image_block_lines: Vec::new(),
        }
    }

    /// True when the K-P pipeline has populated chapter_lines/kp_pages
    /// for the current chapter; callers can rely on these to drive
    /// page navigation and rendering.
    #[inline]
    pub(super) fn has_kp_layout(&self) -> bool {
        !self.kp_pages.is_empty()
    }

    /// Drop K-P state (e.g. on chapter change or font cycle). Releases
    /// vec capacities (not just length) so the heap is freed for the
    /// next chapter's ch_cache allocation. `chapter_lines` for a long
    /// chapter is ~22 KB; without the explicit shrink, that capacity
    /// stays allocated until the next K-P typeset reallocates, which
    /// can cause `try_cache_chapter` to OOM mid-jump on tight heaps.
    pub(super) fn clear_kp_layout(&mut self) {
        self.chapter_lines.clear();
        self.chapter_lines.shrink_to_fit();
        self.kp_pages.clear();
        self.kp_pages.shrink_to_fit();
        self.image_block_lines.clear();
        self.image_block_lines.shrink_to_fit();
    }
}

// epub-specific state: zip index, metadata, spine, toc, chapter
// cache, background cache progress, image cache scan position
pub(super) struct EpubState {
    // --- publicly accessible from sibling modules ---
    pub(super) zip: ZipIndex,
    pub(super) meta: EpubMeta,
    pub(super) spine: EpubSpine,
    pub(super) chapter: u16,

    // legacy per-book subdir (`_XXXXXXX`) for the inline image cache;
    // still used by images.rs until the image table is ported into
    // the bundle (Phase 2 follow-up).
    pub(super) cache_dir: [u8; 8],
    pub(super) chapter_table: [(u32, u32); cache::MAX_CACHE_CHAPTERS],
    pub(super) chapters_cached: bool,
    pub(super) cache_chapter: u16,
    pub(super) ch_cached: [bool; cache::MAX_CACHE_CHAPTERS],
    pub(super) ch_cache: Vec<u8>,

    pub(super) bg_cache: BgCacheState,
    // generation this book's queued work was submitted under; None
    // until the first reset. only work_queue::reset mints one
    pub(super) work_gen: Option<work_queue::WorkGen>,

    pub(super) img_cache_ch: u16,
    pub(super) img_cache_offset: u32,
    pub(super) img_scan_wrapped: bool,
    pub(super) skip_large_img: bool,
    // consecutive streaming-decode failures; raises skip_large_img
    // once it reaches LARGE_IMG_FAIL_LIMIT
    pub(super) large_img_fails: u8,
    // the image whose worker decode is being retried, and how many
    // budget steps it has already taken. bounded by IMG_BUDGET_STEPS,
    // after which the image is marked skipped rather than redispatched
    pub(super) img_retry: Option<(u32, u8)>,
    pub(super) img_found_count: u16,
    pub(super) img_cached_count: u16,

    pub(super) cache_step: Option<smol_epub::cache::StreamStripStep>,
    // the book's stylesheets, parsed at NeedOpf when the bundle still
    // needs building and dropped once every chapter is cached; the
    // stripper resolves every element against them. one entry or none:
    // the 4.6 KB table lives on the heap only while it is needed
    pub(super) css: Vec<smol_epub::css::CssRules>,

    pub(super) toc: Option<Box<EpubToc>>,
    pub(super) toc_source: Option<TocSource>,
    pub(super) toc_selected: usize,
    pub(super) toc_scroll: usize,

    // hash of the source filename; used as the bundle identity
    // (bundle path = `_PLUMP/BOOKS/<name_hash>.BIN`)
    pub(super) name_hash: u32,
    // source file size; header mismatch triggers bundle rebuild
    pub(super) archive_size: u32,
    // content format of the bundle's chapter streams, set by check_cache
    pub(super) bundle_content_fmt: u8,
}

impl EpubState {
    pub(super) const fn new() -> Self {
        Self {
            zip: ZipIndex::new(),
            meta: EpubMeta::new(),
            spine: EpubSpine::new(),
            chapter: 0,
            cache_dir: [0u8; 8],
            name_hash: 0,
            archive_size: 0,
            bundle_content_fmt: plump_kernel::kernel::bundle::CONTENT_FMT_LATEST,
            chapter_table: [(0u32, 0u32); cache::MAX_CACHE_CHAPTERS],
            chapters_cached: false,
            cache_chapter: 0,
            ch_cached: [false; cache::MAX_CACHE_CHAPTERS],
            ch_cache: Vec::new(),
            bg_cache: BgCacheState::Idle,
            work_gen: None,
            img_cache_ch: 0,
            img_cache_offset: 0,
            img_scan_wrapped: false,
            skip_large_img: false,
            large_img_fails: 0,
            img_retry: None,
            img_found_count: 0,
            img_cached_count: 0,
            cache_step: None,
            css: Vec::new(),
            toc: None,
            toc_source: None,
            toc_selected: 0,
            toc_scroll: 0,
        }
    }

    #[inline]
    pub(super) fn cache_dir_str(&self) -> &str {
        cache::dir_name_str(&self.cache_dir)
    }

    /// The decode budget to dispatch `path_hash` at.
    ///
    /// Worker decodes cannot step down in place the way [`oom_retry`]
    /// does — the worker owns no reader state and its input buffer is
    /// gone by the time the failure comes back — so the ladder lives
    /// here instead: each recorded retry halves the budget the next
    /// dispatch asks for.
    ///
    /// [`oom_retry`]: Self::oom_retry
    pub(super) fn img_budget(&self, path_hash: u32, base_w: u16, base_h: u16) -> (u16, u16) {
        let steps = match self.img_retry {
            Some((h, steps)) if h == path_hash => steps,
            _ => 0,
        };
        let (w, h) = (base_w >> steps, base_h >> steps);
        if w < IMG_BUDGET_MIN || h < IMG_BUDGET_MIN {
            (base_w, base_h)
        } else {
            (w, h)
        }
    }

    #[inline]
    pub(super) fn chapter_size(&self, ch: usize) -> u32 {
        if ch < cache::MAX_CACHE_CHAPTERS {
            self.chapter_table[ch].1
        } else {
            0
        }
    }

    /// Decode at `max_w` x `max_h`, giving ground to the heap on the way.
    ///
    /// Two things can be released when a decode does not fit.  First the
    /// chapter cache, which can hold up to 96 KB: a large DEFLATED
    /// cover or inline JPEG wants most of the heap for itself, so the
    /// two cannot coexist.  After a successful retry the cache stays
    /// empty; the next chapter navigation reloads it via
    /// `try_cache_chapter`.
    ///
    /// Then the target size itself.  The decoder's output plane is its
    /// largest reservation and shrinks with the square of the budget
    /// (halving the budget doubles the integer downscale), so a step
    /// down turns a ~40 KB block into a ~10 KB one — which fits a
    /// fragmented heap far more often.  A softer picture beats a blank
    /// one, and the page renders a cached image at whatever size it
    /// was decoded at.
    ///
    /// Only budget failures step down: a truncated or unsupported file
    /// fails the same way at every size, and each retry is a fresh
    /// streaming pass over the SD card.
    pub(super) fn oom_retry<E, F>(
        &mut self,
        label: &str,
        max_w: u16,
        max_h: u16,
        mut f: F,
    ) -> Result<DecodedImage, E>
    where
        E: core::fmt::Display + IsOom,
        F: FnMut(u16, u16) -> Result<DecodedImage, E>,
    {
        let mut result = f(max_w, max_h);

        if matches!(&result, Err(e) if e.is_oom()) && !self.ch_cache.is_empty() {
            let heap = esp_alloc::HEAP.stats();
            log::info!(
                "{}: decode out of memory at {}/{}K, releasing {} KB ch_cache and retrying",
                label,
                heap.current_usage / 1024,
                heap.size / 1024,
                self.ch_cache.len() / 1024,
            );
            self.ch_cache = Vec::new();
            result = f(max_w, max_h);
        }

        let (mut w, mut h) = (max_w, max_h);
        for _ in 0..IMG_BUDGET_STEPS {
            if !matches!(&result, Err(e) if e.is_oom()) {
                break;
            }
            w /= 2;
            h /= 2;
            if w < IMG_BUDGET_MIN || h < IMG_BUDGET_MIN {
                break;
            }
            log::info!("{}: decode out of memory, retrying at {}x{}", label, w, h);
            result = f(w, h);
        }

        result
    }
}

/// Whether a decode failure was the heap giving out rather than the
/// file being bad. The two decode call sites carry different error
/// types: the inline path keeps smol-epub's `&'static str`, the
/// precache path has already mapped it to [`Error`].
pub(super) trait IsOom {
    fn is_oom(&self) -> bool;
}

impl IsOom for &'static str {
    #[inline]
    fn is_oom(&self) -> bool {
        self.contains("OOM")
    }
}

impl IsOom for crate::error::Error {
    #[inline]
    fn is_oom(&self) -> bool {
        self.kind() == crate::error::ErrorKind::OutOfMemory
    }
}

impl Default for ReaderApp {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingPositionChange {
    OpenReady,
    RestoreReady,
    PageTurn,
    Jump,
}

/// Why the reader is loading. Entering a book (open, wake) shows
/// the plate; moving inside it (chapter change, new font) shows the
/// strip. See mockups/xteink_x4_reader_loading.html.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadingReason {
    Open,
    Resume,
    Chapter,
    Font,
}

pub struct ReaderApp {
    pub(super) filename: FixedStr<32>,
    pub(super) title: FixedStr<64>,
    pub(super) title_is_real: bool,
    pub(super) file_size: u32,

    pub(super) pg: PageState,
    pub(super) epub: EpubState,

    pub(super) state: State,
    pub(super) error: Option<Error>,
    pub(super) show_position: bool,

    pub(super) is_epub: bool,
    pub(super) goto_last_page: bool,
    // consecutive typeset OOM waits on the decode worker; see the
    // memory-governor escalation in preindex_all_pages. retry_ch pins
    // the wait to a chapter so navigation during a wait re-indexes
    pub(super) typeset_oom_waits: u8,
    typeset_retry_ch: u16,
    pub(super) restore_offset: Option<u32>,
    pub(super) restore_page_hint: Option<usize>,
    // the book record's position, held from NeedBookmark until the
    // spine is known and it can be checked against the file
    pending_position: Option<crate::apps::book_record::Position>,
    // format-independent locator, resolved to a byte offset at NeedPage
    // when the bundle's content format is not the one `restore_offset`
    // was counted in
    restore_anchor: Option<smol_epub::markup::Anchor>,
    restore_fmt: u8,
    restore_layout_key: u32,
    // the position last written to the record, kept so a stats-only
    // flush outside Ready never erases the place
    last_saved_pos: Option<crate::apps::book_record::Position>,
    record_seq: u32,
    pub(super) recent_dirty: bool,
    pending_position_change: Option<PendingPositionChange>,
    pub(super) defer_open_work_once: bool,
    pub(super) pending_toc_parse: bool,
    pub(super) pending_title_save: bool,
    pub(super) pending_cover_thumb: bool,
    pub(super) loading_cover: Option<DecodedImage>,

    pub(super) page_img: Option<DecodedImage>,
    pub(super) fullscreen_img: bool,
    pub(super) defer_image_decode: bool,
    // caching-indicator update deferred out of a waveform-window step
    cache_ui_stale: bool,

    pub(super) fonts: Option<fonts::FontSet>,
    pub(super) font_line_h: u16,
    pub(super) font_ascent: u16,
    pub(super) max_lines: u8,

    // reading theme: runtime layout derived from READING_THEMES
    pub(super) text_margin: u16, // horizontal margin for text content (from theme)
    pub(super) text_y: u16,      // top of text area (TEXT_Y + theme vertical margin)
    pub(super) text_w: u32,      // text content width (SCREEN_W - 2 * text_margin)
    pub(super) text_area_h: u16, // height of text area (SCREEN_H - text_y - bottom_pad)
    pub(super) reading_theme_idx: u8,
    pub(super) line_spacing_idx: u8, // index into config::LINE_SPACING_PCT
    pub(super) show_chrome: bool,
    pub(super) text_alignment: u8, // 0 = Left, 1 = Justify

    // pre-scanned image heights for the current page buffer;
    // populated before wrapping so the pager can reserve the exact
    // number of lines each image needs at its natural aspect ratio
    pub(super) img_heights: [u16; MAX_IMAGES_PER_PAGE],
    pub(super) img_height_count: u8,

    pub(super) book_font_size_idx: u8,
    pub(super) reader_font: ReaderFont,
    // layout inputs changed via a setter while suspended; on_resume
    // must re-index the chapter. typeset_stale means the line breaks
    // moved (font, family, text width) and the persisted layout must
    // be invalidated; pagination_stale means only line_h / max_lines
    // changed, so the cached line table survives and the chapter
    // just re-paginates
    pub(super) typeset_stale: bool,
    pub(super) pagination_stale: bool,

    pub(super) chrome_font: Option<&'static BitmapFont>,
    ui_font_idx: u8,

    // contents sheet: grown to the top margin; page count per spine
    // chapter from the layout directory (0 = not laid out yet) and
    // whether those counts need a reload
    toc_expanded: bool,
    toc_pages: [u16; MAX_SPINE],
    toc_pages_stale: bool,
    // home menu "Contents": show the sheet once the book is ready
    open_contents_on_ready: bool,
    loading_reason: LoadingReason,
    /// the loading screen is on the panel for this episode
    loading_painted: bool,
    loading_since: embassy_time::Instant,
    /// first frame after entering the book or waking: full clear
    first_paint_full: bool,
    /// chapter count from the bundle header, for the plate before
    /// the OPF is parsed (wake); 0 when unknown
    spine_hint: u16,
    pub(super) qa_buf: [QuickAction; QA_MAX],
    pub(super) qa_count: u8,

    // reading statistics (accumulated per book, flushed to SD)
    pub(super) stats: crate::apps::stats::ReadingStats,
    pub(super) stats_last_uptime: u32, // uptime_secs at last page turn / enter
    pub(super) stats_dirty: bool,
    pub(super) stats_clock_running: bool, // false when suspended/exited

    // pending day-stats deltas (chunk F). incremented alongside the
    // per-book counters; flushed into `KernelHandle::day_stats_mut`
    // from `flush_deferred_persistence`, then zeroed.
    pub(super) day_pages_pending: u16,
    pub(super) day_secs_pending: u32,

    // deferred persistence: debounce deadline (uptime_secs)
    pub(super) persist_next_flush_at: Option<u32>,
}

impl ReaderApp {
    pub const fn new() -> Self {
        Self {
            filename: FixedStr::EMPTY,
            title: FixedStr::EMPTY,
            title_is_real: false,
            file_size: 0,

            pg: PageState::new(),
            epub: EpubState::new(),

            state: State::NeedPage,
            error: None,
            show_position: false,

            is_epub: false,
            goto_last_page: false,
            typeset_oom_waits: 0,
            typeset_retry_ch: 0,
            restore_offset: None,
            restore_page_hint: None,
            pending_position: None,
            restore_anchor: None,
            restore_fmt: 0,
            restore_layout_key: 0,
            last_saved_pos: None,
            record_seq: 0,
            recent_dirty: false,
            pending_position_change: None,
            defer_open_work_once: false,
            pending_toc_parse: false,
            pending_title_save: false,
            pending_cover_thumb: false,
            loading_cover: None,

            page_img: None,
            fullscreen_img: false,
            defer_image_decode: false,
            cache_ui_stale: false,

            fonts: None,
            font_line_h: LINE_H,
            font_ascent: LINE_H,
            max_lines: LINES_PER_PAGE as u8,

            text_margin: MARGIN,
            text_y: TEXT_Y,
            text_w: TEXT_W,
            text_area_h: TEXT_AREA_H,
            reading_theme_idx: 0,
            line_spacing_idx: crate::kernel::config::DEFAULT_LINE_SPACING,
            show_chrome: true,
            text_alignment: 0,

            img_heights: [0u16; MAX_IMAGES_PER_PAGE],
            img_height_count: 0,

            book_font_size_idx: 0,
            reader_font: ReaderFont::Bookerly,
            typeset_stale: false,
            pagination_stale: false,

            chrome_font: None,
            ui_font_idx: 1,
            toc_expanded: false,
            toc_pages: [0; MAX_SPINE],
            toc_pages_stale: false,
            open_contents_on_ready: false,
            loading_reason: LoadingReason::Open,
            loading_painted: false,
            loading_since: embassy_time::Instant::from_ticks(0),
            first_paint_full: false,
            spine_hint: 0,

            qa_buf: [QuickAction::trigger(0, "", ""); QA_MAX],
            qa_count: 0,

            stats: crate::apps::stats::ReadingStats::EMPTY,
            stats_last_uptime: 0,
            stats_dirty: false,
            stats_clock_running: false,

            day_pages_pending: 0,
            day_secs_pending: 0,

            persist_next_flush_at: None,
        }
    }

    // 0 = XSmall, 1 = Small, 2 = Medium, 3 = Large, 4 = XLarge
    pub fn set_book_font_size(&mut self, idx: u8) {
        self.typeset_stale |= idx != self.book_font_size_idx;
        self.book_font_size_idx = idx;
        self.apply_font_metrics();
        self.rebuild_quick_actions();
    }

    pub fn set_reader_font(&mut self, font: ReaderFont) {
        self.typeset_stale |= font != self.reader_font;
        self.reader_font = font;
        self.apply_font_metrics();
        self.rebuild_quick_actions();
    }

    pub fn set_reading_theme(&mut self, idx: u8) {
        if idx != self.reading_theme_idx {
            let old = crate::kernel::config::ReadingTheme::from_idx(self.reading_theme_idx);
            let new = crate::kernel::config::ReadingTheme::from_idx(idx);
            // margin_h moves text_w so the breaks change; margin_v
            // only changes the page capacity
            if new.margin_h != old.margin_h {
                self.typeset_stale = true;
            } else if new.margin_v != old.margin_v {
                self.pagination_stale = true;
            }
        }
        self.reading_theme_idx = idx;
        self.apply_theme_layout();
        self.apply_font_metrics();
    }

    pub fn set_line_spacing(&mut self, idx: u8) {
        self.pagination_stale |= idx != self.line_spacing_idx;
        self.line_spacing_idx = idx;
        self.apply_font_metrics();
    }

    pub fn set_text_alignment(&mut self, alignment: u8) {
        // alignment is not a layout input: breaks are alignment-
        // independent, so no re-index on change
        self.text_alignment = alignment;
    }

    pub fn set_show_chrome(&mut self, show: bool) {
        if self.show_chrome != show {
            self.show_chrome = show;
            // text_area_h changes max_lines only; text_w is untouched
            self.pagination_stale = true;
            self.apply_theme_layout();
            self.apply_font_metrics();
        }
    }

    fn apply_theme_layout(&mut self) {
        let theme = crate::kernel::config::ReadingTheme::from_idx(self.reading_theme_idx);
        self.text_margin = theme.margin_h;
        // no top status bar in the reader; text always starts at the
        // top edge, chrome only reserves the bottom info bar
        self.text_y = TEXT_Y + theme.margin_v;
        self.text_w = (SCREEN_W - 2 * self.text_margin) as u32;
        let bottom = if self.show_chrome {
            CHROME_Y - CHROME_PAD
        } else {
            SCREEN_H - SCREEN_PAD
        };
        self.text_area_h = bottom.saturating_sub(self.text_y);
    }

    pub fn set_chrome_font(&mut self, font: &'static BitmapFont) {
        self.chrome_font = Some(font);
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_font_idx = idx;
    }

    // the contents sheet is plain BW like every other overlay: its
    // inverted rows have no gray coding, and a gray pass over the
    // page beneath would code text the sheet already covers
    pub fn wants_grayscale(&self) -> bool {
        self.state == State::Ready
    }

    pub fn shows_loading_screen(&self) -> bool {
        !matches!(self.state, State::Ready | State::ShowToc | State::Error)
    }

    fn loading_visual_region(&self) -> Region {
        Region::new(
            self.text_margin,
            self.text_y,
            self.text_w as u16,
            self.text_area_h,
        )
    }

    /// Start a loading episode. nothing is painted until the hold runs
    /// out or a slow step is next (`loading_tick`, `paint_loading`), so
    /// a fast open, wake or chapter change lands as one refresh
    fn begin_loading(&mut self, reason: LoadingReason) {
        self.loading_reason = reason;
        self.loading_painted = false;
        self.loading_since = embassy_time::Instant::now();
    }

    /// Put the loading screen up now: plate or strip, plus the footer
    fn paint_loading(&mut self, ctx: &mut AppContext) {
        if self.loading_painted {
            return;
        }
        self.loading_painted = true;
        log::info!(
            "reader: loading screen {:?} after {} ms in {:?}",
            self.loading_reason,
            self.loading_since.elapsed().as_millis(),
            self.state
        );
        if self.first_paint_full {
            self.first_paint_full = false;
            ctx.request_full_redraw();
        } else if matches!(self.loading_reason, LoadingReason::Open | LoadingReason::Resume) {
            ctx.mark_dirty(PAGE_REGION);
        } else {
            ctx.mark_dirty(self.loading_visual_region());
        }
    }

    /// Hold check, run before every pipeline step
    fn loading_tick(&mut self, ctx: &mut AppContext) {
        if self.loading_painted || !self.shows_loading_screen() {
            return;
        }
        let hold = if self.loading_reason == LoadingReason::Resume {
            LOADING_HOLD_RESUME_MS
        } else {
            LOADING_HOLD_MS
        };
        if self.loading_since.elapsed().as_millis() >= hold {
            self.paint_loading(ctx);
        }
    }

    /// The page, or an error, is ready: repaint, as a full clear when
    /// this is the first frame after entering the book or waking
    fn mark_page_ready(&mut self, ctx: &mut AppContext) {
        if self.first_paint_full {
            self.first_paint_full = false;
            ctx.request_full_redraw();
        } else {
            ctx.mark_dirty(PAGE_REGION);
        }
    }

    /// A stage changed: repaint the stage row if the screen is up
    fn set_loading_ui(&self, ctx: &mut AppContext) {
        if self.loading_painted && self.shows_loading_screen() {
            ctx.mark_dirty(STAGE_REGION);
        }
    }

    /// Caption (uppercase, tracked) and right-hand value of the stage
    /// row; the percent for the bar, None for an error
    fn loading_stage(&self, cap: &mut StackFmt<48>, val: &mut StackFmt<48>) -> Option<u8> {
        if let Some(e) = self.error {
            let _ = write!(cap, "COULD NOT OPEN");
            let _ = write!(val, "{}", e);
            return None;
        }
        let n = self.epub.spine.len();
        let ch = self.epub.chapter as usize + 1;
        let pct = match self.state {
            State::NeedBookmark | State::NeedInit | State::NeedOpf | State::NeedToc => {
                let _ = write!(
                    cap,
                    "{}",
                    if self.loading_reason == LoadingReason::Resume {
                        "RESUMING"
                    } else {
                        "OPENING"
                    }
                );
                match self.state {
                    State::NeedBookmark => 5,
                    State::NeedInit => 15,
                    State::NeedOpf => 25,
                    _ => 40,
                }
            }
            State::NeedCache => {
                if n > 0 {
                    let _ = write!(cap, "CACHING CHAPTER {} OF {}", ch, n);
                } else {
                    let _ = write!(cap, "CACHING");
                }
                55
            }
            State::NeedIndex => {
                if self.typeset_oom_waits > 0 {
                    let _ = write!(cap, "WAITING FOR IMAGES");
                } else if self.loading_reason == LoadingReason::Font {
                    let _ = write!(cap, "TYPESETTING \u{00B7} ");
                    let name = fonts::FONT_SIZE_NAMES
                        .get(self.book_font_size_idx as usize)
                        .copied()
                        .unwrap_or("");
                    for c in name.chars() {
                        let _ = cap.write_char(c.to_ascii_uppercase());
                    }
                } else if self.is_epub && n > 0 {
                    let _ = write!(cap, "TYPESETTING CHAPTER {} OF {}", ch, n);
                } else {
                    let _ = write!(cap, "TYPESETTING");
                }
                75
            }
            State::NeedPage => {
                let _ = write!(cap, "OPENING PAGE");
                90
            }
            State::Ready | State::ShowToc => {
                let _ = write!(cap, "READY");
                100
            }
            State::Error => 0,
        };
        let _ = write!(val, "{}%", pct);
        Some(pct)
    }

    /// Where the book will open: chapter and percent, "from the start"
    /// for a book without a bookmark, page or size for plain text
    fn loading_resume_line(&self, out: &mut StackFmt<48>) {
        if self.is_epub {
            let n = self.epub.spine.len();
            let ch = self.epub.chapter as usize + 1;
            let fresh = self.epub.chapter == 0 && self.restore_offset.unwrap_or(0) == 0;
            if n == 0 {
                // before the OPF: what the bundle header knows
                match (ch > 1, self.spine_hint as usize) {
                    (true, hint) if hint > 0 => {
                        let _ = write!(out, "Chapter {} of {}", ch, hint);
                    }
                    (true, _) => {
                        let _ = write!(out, "Chapter {}", ch);
                    }
                    (false, hint) if hint > 0 => {
                        let _ = write!(out, "{} chapters", hint);
                    }
                    _ => {}
                }
            } else if fresh {
                let _ = write!(out, "From the start \u{00B7} {} chapters", n);
            } else {
                let _ = write!(
                    out,
                    "Chapter {} of {} \u{00B7} {}% read",
                    ch,
                    n,
                    self.progress_pct()
                );
            }
        } else if self.file_size > 0 {
            match self.restore_page_hint {
                Some(p) if p > 0 => {
                    let _ = write!(out, "Page {}", p + 1);
                }
                _ => {
                    let _ = write!(out, "{} KB", self.file_size / 1024);
                }
            }
        }
    }

    fn placeholder_id(&self) -> u32 {
        if self.is_epub && self.epub.name_hash != 0 {
            self.epub.name_hash
        } else {
            self.filename
                .as_bytes()
                .iter()
                .fold(0x811c_9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
        }
    }

    /// The loading screen (mockups/xteink_x4_reader_loading.html,
    /// option A). entering a book shows the plate: cover, title,
    /// author, rule, resume line. moving inside it shows the strip:
    /// the text area cleared. both end in the stage row above the
    /// footer, and only that row repaints while the pipeline runs
    fn draw_loading_screen(&self, strip: &mut StripBuffer) {
        if strip.gray_mode() != GrayMode::Bw {
            return;
        }
        if matches!(self.loading_reason, LoadingReason::Open | LoadingReason::Resume) {
            self.draw_loading_plate(strip, self.loading_visual_region());
        }
        self.draw_stage_row(strip);
        if self.show_chrome {
            self.draw_footer(strip);
        }
    }

    fn draw_loading_plate(&self, strip: &mut StripBuffer, area: Region) {
        use crate::apps::cover_cache::{CARD_THUMB_H, CARD_THUMB_W};
        const COVER_TOP: u16 = 104;
        const TITLE_GAP: u16 = 30;
        const RULE_W: u16 = 34;

        let cover = Region::new(
            area.x + area.w.saturating_sub(CARD_THUMB_W) / 2,
            area.y + COVER_TOP,
            CARD_THUMB_W,
            CARD_THUMB_H,
        );
        match self.loading_cover.as_ref() {
            Some(img) => {
                let x = cover.x + cover.w.saturating_sub(img.width) / 2;
                let y = cover.y + cover.h.saturating_sub(img.height) / 2;
                strip.blit_1bpp(
                    &img.data,
                    0,
                    img.width as usize,
                    img.height as usize,
                    img.stride,
                    x as i32,
                    y as i32,
                    true,
                );
            }
            None => {
                // the library's placeholder shape stands in until the
                // real cover is extracted after the page is up
                let theme = plump_kernel::ui::Theme::default_v1();
                let mut p = plump_kernel::ui::Painter::new(strip, &theme);
                crate::apps::cover_placeholder::draw_cover(&mut p, cover, self.placeholder_id());
            }
        }

        let family = self.reader_font.family();
        let title = self.display_name();
        let title_font = if title.len() > 40 {
            fonts::heading_font(family, 2)
        } else {
            fonts::heading_font(family, 3)
        };
        let lh = title_font.line_height;
        let (first, rest) = split_title_line(title_font, title, area.w);
        let mut y = cover.y + cover.h + TITLE_GAP;
        title_font.draw_aligned(
            strip,
            Region::new(area.x, y, area.w, lh),
            first,
            Alignment::Center,
            BinaryColor::On,
        );
        y += lh;
        if !rest.is_empty() {
            draw_truncated_text(
                strip,
                title_font,
                Region::new(area.x, y, area.w, lh),
                rest,
                Alignment::Center,
                BinaryColor::On,
            );
            y += lh;
        }

        let author = if self.is_epub {
            self.epub.meta.author_str()
        } else {
            ""
        };
        if !author.is_empty() {
            let small = fonts::ui_body_font(1);
            y += 8;
            draw_truncated_text(
                strip,
                small,
                Region::new(area.x, y, area.w, small.line_height),
                author,
                Alignment::Center,
                BinaryColor::On,
            );
            y += small.line_height;
        }

        y += 22;
        Rectangle::new(
            Point::new((area.x + area.w.saturating_sub(RULE_W) / 2) as i32, y as i32),
            Size::new(RULE_W as u32, 1),
        )
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(strip)
        .ok();
        y += 20;

        let mut line = StackFmt::<48>::new();
        self.loading_resume_line(&mut line);
        if !line.is_empty() {
            let f = fonts::chrome_font();
            draw_truncated_text(
                strip,
                f,
                Region::new(area.x, y, area.w, f.line_height),
                line.as_str(),
                Alignment::Center,
                BinaryColor::On,
            );
        }
    }

    fn draw_stage_row(&self, strip: &mut StripBuffer) {
        let r = STAGE_REGION;
        if !r.intersects(strip.logical_window()) {
            return;
        }
        let font = fonts::chrome_font();
        let mut cap = StackFmt::<48>::new();
        let mut val = StackFmt::<48>::new();
        let pct = self.loading_stage(&mut cap, &mut val);
        let baseline = r.y as i32 + ((STAGE_CAP_H + font.ascent) / 2) as i32;
        sheet::draw_tracked(strip, font, cap.as_str(), 1, r.x as i32, baseline, BinaryColor::On);
        match pct {
            Some(pct) => {
                if !val.is_empty() {
                    let w = sheet::tracked_width(font, val.as_str(), 1);
                    sheet::draw_tracked(
                        strip,
                        font,
                        val.as_str(),
                        1,
                        (r.x + r.w - w) as i32,
                        baseline,
                        BinaryColor::On,
                    );
                }
                // track: 1 px line across the width, fill: 2 px high up
                // to the position, like the footer bar
                let bar_y = r.y + STAGE_CAP_H + 8;
                Rectangle::new(
                    Point::new(r.x as i32, (bar_y + 1) as i32),
                    Size::new(r.w as u32, 1),
                )
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(strip)
                .ok();
                let filled = (r.w as u32 * pct.min(100) as u32) / 100;
                if filled > 0 {
                    Rectangle::new(Point::new(r.x as i32, bar_y as i32), Size::new(filled, 2))
                        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                        .draw(strip)
                        .ok();
                }
            }
            None => {
                // error: the message takes the bar's place
                sheet::draw_ellipsized(
                    strip,
                    font,
                    Region::new(r.x, r.y + STAGE_CAP_H, r.w, r.h - STAGE_CAP_H),
                    val.as_str(),
                    BinaryColor::On,
                );
            }
        }
    }

    pub fn has_bg_work(&self) -> bool {
        self.is_epub && self.epub.bg_cache != BgCacheState::Idle
    }

    pub(super) fn cached_chapter_count(&self) -> usize {
        let n = self.epub.spine.len().min(cache::MAX_CACHE_CHAPTERS);
        self.epub.ch_cached[..n].iter().filter(|&&c| c).count()
    }

    #[inline]
    fn has_pending_open_work(&self) -> bool {
        self.pending_toc_parse || self.pending_title_save || self.pending_cover_thumb
    }

    #[inline]
    fn arm_deferred_open_work(&mut self) {
        if self.has_pending_open_work() {
            self.defer_open_work_once = true;
        }
    }

    fn save_title_mapping(&self, k: &mut KernelHandle<'_>) {
        if !self.title_is_real || self.title.is_empty() || self.filename.is_empty() {
            return;
        }

        // save_title appends to TITLES.BIN, so a blind save on every
        // open grows the file without bound. skip when the mapping is
        // already current (dir cache mirrors TITLES.BIN + humanized
        // fallbacks; a humanized entry won't match a real title, so
        // first-time saves still go through).
        let _ = k.ensure_dir_cache_loaded();
        if k.dir_cache_mut().find_title(self.filename.as_bytes()) == Some(self.title.as_bytes()) {
            return;
        }

        if let Err(e) = k.sd().save_title(self.filename.as_str(), self.title.as_str()) {
            log::warn!("epub: failed to save title mapping: {}", e);
            return;
        }
        // keep the RAM cache in sync so home / library rows pick the
        // new title up this session and repeat opens skip the save
        k.dir_cache_mut()
            .update_title(self.filename.as_bytes(), self.title.as_bytes());
    }

    fn load_toc(&mut self, k: &mut KernelHandle<'_>) {
        let Some(source) = self.epub.toc_source.take() else {
            return;
        };

        let fname = self.filename;
        let name = fname.as_str();
        let toc_idx = source.zip_index();

        let mut toc_dir_buf = [0u8; 256];
        let toc_dir_len = {
            let toc_path = self.epub.zip.entry_name(toc_idx);
            let dir = toc_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let n = dir.len().min(toc_dir_buf.len());
            toc_dir_buf[..n].copy_from_slice(dir.as_bytes());
            n
        };
        let toc_dir = core::str::from_utf8(&toc_dir_buf[..toc_dir_len]).unwrap_or("");

        match extract_zip_entry(k, name, &self.epub.zip, toc_idx) {
            Ok(toc_data) => {
                let mut toc = Box::new(EpubToc::new());
                epub::parse_toc(
                    source,
                    &toc_data,
                    toc_dir,
                    &self.epub.spine,
                    &self.epub.zip,
                    &mut toc,
                );
                log::debug!("epub: TOC has {} entries", toc.len());
                self.epub.toc = Some(toc);
            }
            Err(_e) => {
                log::warn!("epub: failed to read TOC");
            }
        }
    }

    fn run_deferred_open_work(&mut self, k: &mut KernelHandle<'_>) -> bool {
        if self.pending_toc_parse {
            self.pending_toc_parse = false;
            self.load_toc(k);
            self.rebuild_quick_actions();
            return true;
        }

        if self.pending_title_save {
            self.pending_title_save = false;
            self.save_title_mapping(k);
            return true;
        }

        if self.pending_cover_thumb {
            self.pending_cover_thumb = false;
            // arming is gated on FLAG_CORE_READY at every callsite (OPF
            // parse + finish_cache success), so generate_cover_thumb is
            // guaranteed to find CORE ready here.
            self.generate_cover_thumb(k);
            return true;
        }

        false
    }

    // update the kernel loading indicator with current caching progress.
    // uses a unified percentage: chapters contribute 0-80%, images 80-100%.
    fn cache_loading_pct(&self) -> u8 {
        let cached_ch = self.cached_chapter_count();
        let total_ch = self.epub.spine.len();
        let img_found = self.epub.img_found_count as usize;
        let img_cached = self.epub.img_cached_count as usize;

        let in_chapter_phase = matches!(
            self.epub.bg_cache,
            BgCacheState::CacheChapter | BgCacheState::WaitNearbyImage
        ) && cached_ch < total_ch;

        if in_chapter_phase {
            // chapters: 0% to 80%
            if total_ch > 0 {
                ((cached_ch * 80) / total_ch).min(80) as u8
            } else {
                80
            }
        } else {
            // image phase: 80% to 100%
            if img_found > 0 {
                (80 + (img_cached * 20) / img_found).min(100) as u8
            } else {
                80
            }
        }
    }

    // drain pending day-stats deltas (chunk F) into the kernel's
    // shared DayStats. cheap (a couple of u16/u32 adds + rollover
    // check), safe to run every pass
    fn drain_day_stats(&mut self, k: &mut KernelHandle<'_>) {
        if self.day_pages_pending > 0 || self.day_secs_pending > 0 {
            let today = k.today_key();
            let ds = k.day_stats_mut();
            ds.add_pages(today, self.day_pages_pending);
            ds.add_secs(today, self.day_secs_pending);
            self.day_pages_pending = 0;
            self.day_secs_pending = 0;
        }
    }

    fn set_cache_loading(&self, ctx: &mut AppContext) {
        let cached_ch = self.cached_chapter_count();
        let total_ch = self.epub.spine.len();
        let img_found = self.epub.img_found_count as usize;
        let img_cached = self.epub.img_cached_count as usize;

        let mut lbuf = StackFmt::<28>::new();

        let in_chapter_phase = matches!(
            self.epub.bg_cache,
            BgCacheState::CacheChapter | BgCacheState::WaitNearbyImage
        ) && cached_ch < total_ch;

        if in_chapter_phase {
            let _ = write!(lbuf, "Caching {}/{}", cached_ch, total_ch);
        } else if img_found > 0 {
            let _ = write!(lbuf, "Caching images {}/{}", img_cached, img_found);
        } else {
            let _ = write!(lbuf, "Caching images");
        }

        ctx.set_loading(LOADING_REGION, lbuf.as_str(), self.cache_loading_pct());
    }

    // ── deferred persistence ────────────────────────────────────────

    const PERSIST_DEBOUNCE_SECS: u32 = 30;

    /// Schedule a deferred flush in PERSIST_DEBOUNCE_SECS from now.
    fn arm_persist_debounce(&mut self) {
        let deadline = crate::kernel::uptime_secs() + Self::PERSIST_DEBOUNCE_SECS;
        self.persist_next_flush_at = Some(deadline);
    }

    /// Queue a position change to be committed once the target page is visible.
    fn queue_position_change(&mut self, change: PendingPositionChange) {
        self.pending_position_change = Some(change);
    }

    /// Commit a visible position change and schedule deferred persistence.
    fn commit_position_change(&mut self, change: PendingPositionChange) {
        self.recent_dirty = true;
        self.stats_dirty = true;
        match change {
            PendingPositionChange::PageTurn => self.stats_record_page_turn(),
            PendingPositionChange::OpenReady | PendingPositionChange::RestoreReady => {
                self.stats_resume_clock();
            }
            PendingPositionChange::Jump => {}
        }
        self.arm_persist_debounce();
    }

    /// Finalize a Ready transition once the target page is fully loaded.
    fn finish_ready_transition(&mut self, ctx: &mut AppContext) {
        self.defer_image_decode = false;
        self.state = State::Ready;
        self.arm_deferred_open_work();
        // chapter counts feed the footer's book position and the
        // contents rows; a layout may have landed since the last load
        self.toc_pages_stale = true;
        // the Contents row carries the chapter position
        self.rebuild_quick_actions();
        if self.open_contents_on_ready {
            self.open_contents_on_ready = false;
            self.open_contents(ctx);
        }
        if let Some(change) = self.pending_position_change.take() {
            self.commit_position_change(change);
        }
        ctx.clear_loading();
        self.mark_page_ready(ctx);
    }

    /// Pause the reading-time clock (call on suspend/exit).
    /// Accumulates any elapsed time so deferred flushes don't
    /// count non-reader time.
    ///
    /// Returns true if new reading time was accumulated.
    fn stats_pause_clock(&mut self) -> bool {
        let mut added_elapsed = false;
        if self.stats_clock_running {
            let now = crate::kernel::uptime_secs();
            let delta = now.saturating_sub(self.stats_last_uptime);
            if delta > 0 && delta < 600 {
                self.stats.time_secs = self.stats.time_secs.saturating_add(delta);
                self.day_secs_pending = self.day_secs_pending.saturating_add(delta);
                added_elapsed = true;
            }
            self.stats_clock_running = false;
        }
        added_elapsed
    }

    /// Resume the reading-time clock (call on resume/enter-ready).
    fn stats_resume_clock(&mut self) {
        self.stats_last_uptime = crate::kernel::uptime_secs();
        self.stats_clock_running = true;
    }

    // ── reading statistics ──────────────────────────────────────────

    // call on each page turn to accumulate time and increment page count
    fn stats_record_page_turn(&mut self) {
        let now = crate::kernel::uptime_secs();
        let delta = now.saturating_sub(self.stats_last_uptime);
        // ignore deltas > 10 min (user was idle / fell asleep)
        if delta < 600 {
            self.stats.time_secs = self.stats.time_secs.saturating_add(delta);
            self.day_secs_pending = self.day_secs_pending.saturating_add(delta);
        }
        self.stats_last_uptime = now;
        self.stats.pages = self.stats.pages.saturating_add(1);
        self.day_pages_pending = self.day_pages_pending.saturating_add(1);
        self.stats_dirty = true;
    }

    // load the book record: stats, and the position to restore
    fn record_load(&mut self, k: &mut KernelHandle<'_>) {
        let rec = crate::apps::book_record::BookRecord::load(k, self.name())
            .unwrap_or(crate::apps::book_record::BookRecord::EMPTY);
        self.stats = rec.stats;
        self.record_seq = rec.seq;
        self.last_saved_pos = rec.pos;
        self.pending_position = rec.pos;
        match rec.pos {
            Some(p) => log::info!(
                "record: {} ch{} off={} anchor={}/{} page={} seq={}",
                self.name(),
                p.chapter,
                p.byte_offset,
                p.para,
                p.word,
                p.page,
                rec.seq
            ),
            None => log::info!("record: {} has no position (seq={})", self.name(), rec.seq),
        }
        // new session
        self.stats.sessions = self.stats.sessions.saturating_add(1);
        self.stats_dirty = true;
    }

    /// The reader's place in every unit the record stores; None until
    /// a page is on screen. The anchor is counted from the chapter's
    /// start, from RAM when the chapter is cached there and from the
    /// bundle otherwise.
    fn current_position(
        &mut self,
        k: &mut KernelHandle<'_>,
    ) -> Option<crate::apps::book_record::Position> {
        use crate::apps::book_record::{NO_ANCHOR, Position};
        if !matches!(self.state, State::Ready | State::ShowToc)
            || self.pg.total_pages == 0
            || self.pg.page >= self.pg.total_pages
        {
            return None;
        }
        let byte_offset = self.pg.offsets[self.pg.page];
        let mut p = Position {
            archive_size: if self.is_epub {
                self.epub.archive_size
            } else {
                self.file_size
            },
            chapter: if self.is_epub { self.epub.chapter } else { 0 },
            byte_offset,
            content_fmt: if self.is_epub {
                self.epub.bundle_content_fmt
            } else {
                0
            },
            para: NO_ANCHOR,
            word: NO_ANCHOR,
            layout_key: self.current_layout_key().hash(),
            page: self.pg.page.min(u16::MAX as usize) as u16,
            chapter_no: 0,
            chapter_count: 0,
            progress_pct: self.progress_pct(),
            font_idx: self.book_font_size_idx,
        };
        if self.is_epub {
            let (no, count) = self.chapter_numbering();
            p.chapter_no = no;
            p.chapter_count = count;
            if let Some(a) = self.chapter_anchor_at(k, byte_offset as usize) {
                p.para = a.para;
                p.word = a.word;
            }
        }
        Some(p)
    }

    // the anchor of `offset` in the current chapter's stream
    fn chapter_anchor_at(
        &mut self,
        k: &mut KernelHandle<'_>,
        offset: usize,
    ) -> Option<smol_epub::markup::Anchor> {
        use smol_epub::markup::{SliceSource, anchor_at};
        if !self.epub.ch_cache.is_empty() {
            return Some(anchor_at(&mut SliceSource(&self.epub.ch_cache), offset));
        }
        let ch = self.epub.chapter as usize;
        if !self.epub.chapters_cached || ch >= smol_epub::cache::MAX_CACHE_CHAPTERS {
            return None;
        }
        let (base, size) = self.epub.chapter_table[ch];
        if size == 0 {
            return None;
        }
        let mut src = paging::BundleByteSource::new(k.sd(), self.epub.name_hash, base, size);
        Some(anchor_at(&mut src, offset))
    }

    // the byte offset of `anchor` in the current chapter's stream
    fn chapter_offset_of(
        &mut self,
        k: &mut KernelHandle<'_>,
        anchor: smol_epub::markup::Anchor,
    ) -> Option<u32> {
        use smol_epub::markup::{SliceSource, offset_of};
        if !self.epub.ch_cache.is_empty() {
            return Some(offset_of(&mut SliceSource(&self.epub.ch_cache), anchor));
        }
        let ch = self.epub.chapter as usize;
        if !self.epub.chapters_cached || ch >= smol_epub::cache::MAX_CACHE_CHAPTERS {
            return None;
        }
        let (base, size) = self.epub.chapter_table[ch];
        if size == 0 {
            return None;
        }
        let mut src = paging::BundleByteSource::new(k.sd(), self.epub.name_hash, base, size);
        Some(offset_of(&mut src, anchor))
    }

    fn current_layout_key(&self) -> layout::LayoutKey {
        layout::LayoutKey::current(
            self.book_font_size_idx,
            self.reader_font.to_idx(),
            plump_kernel::kernel::bundle::CONTENT_FMT_LATEST,
            self.text_w as u16,
            self.font_line_h,
            self.max_lines,
        )
    }

    /// (number, count) of the current chapter as the device names it:
    /// TOC entries when there is a TOC, spine items otherwise. the
    /// contents sheet, the sleep card and the home card all say this
    fn chapter_numbering(&self) -> (u16, u16) {
        let ch = self.epub.chapter;
        match self.epub.toc.as_ref().filter(|t| !t.is_empty()) {
            Some(toc) => {
                let entries = &toc.entries[..toc.len()];
                match entries.iter().position(|e| e.spine_idx == ch) {
                    Some(i) => (i as u16 + 1, toc.len() as u16),
                    None => (0, toc.len() as u16),
                }
            }
            None => (ch + 1, self.epub.spine.len() as u16),
        }
    }

    // flush the book record (stats and position) to SD; returns Err on
    // write failure with the dirty state kept for a retry
    fn record_flush(&mut self, k: &mut KernelHandle<'_>) -> crate::error::Result<()> {
        if !self.stats_dirty || self.filename.is_empty() {
            return Ok(());
        }
        plump_kernel::perf_begin!(_sf_t0);
        // accumulate any unrecorded time only if the clock is running
        if self.stats_clock_running {
            let now = crate::kernel::uptime_secs();
            let delta = now.saturating_sub(self.stats_last_uptime);
            if delta < 600 {
                self.stats.time_secs = self.stats.time_secs.saturating_add(delta);
            }
            self.stats_last_uptime = now;
        }

        let pos = self.current_position(k).or(self.last_saved_pos);
        let rec = crate::apps::book_record::BookRecord {
            stats: self.stats,
            pos,
            seq: self.record_seq.wrapping_add(1),
        };
        rec.save(k, self.name())?;
        self.record_seq = rec.seq;
        self.last_saved_pos = pos;
        self.stats_dirty = false;
        if let Some(p) = pos {
            log::info!(
                "record: saved ch{} off={} anchor={}/{} page={} chapter {}/{} {}% seq={}",
                p.chapter,
                p.byte_offset,
                p.para,
                p.word,
                p.page,
                p.chapter_no,
                p.chapter_count,
                p.progress_pct,
                rec.seq
            );
        }
        plump_kernel::perf_event!(
            "reader",
            "record_flush pages={} time_s={} elapsed_ms={}",
            self.stats.pages,
            self.stats.time_secs,
            _sf_t0.elapsed().as_millis()
        );
        Ok(())
    }

    // public accessors for home screen display
    pub fn reading_stats(&self) -> &crate::apps::stats::ReadingStats {
        &self.stats
    }

    #[inline]
    pub fn has_book(&self) -> bool {
        !self.filename.is_empty()
    }

    /// Describe the open book to the sleep card. Runs before
    /// `on_pre_sleep`, so the TOC and stats are still in memory.
    pub fn fill_sleep_card(&self, card: &mut crate::apps::widgets::SleepCard) {
        let author = if self.is_epub {
            let n = (self.epub.meta.author_len as usize).min(self.epub.meta.author.len());
            core::str::from_utf8(&self.epub.meta.author[..n]).unwrap_or("")
        } else {
            ""
        };
        card.set_book(self.display_name(), author, self.filename.as_bytes());
        card.set_progress(self.progress_pct());
        card.set_stats(self.stats.pages, self.stats.time_secs);

        if self.pg.fully_indexed && self.pg.total_pages > 0 {
            card.set_chapter_pages(self.pg.page as u32 + 1, self.pg.total_pages as u32);
        }
        if !self.is_epub {
            // a txt book is one chapter: its pages are the book's
            if self.pg.fully_indexed && self.pg.total_pages > 0 {
                card.set_book_pages(self.pg.page as u32 + 1, self.pg.total_pages as u32);
            }
            return;
        }
        if let Some((page, total)) = self.book_position() {
            card.set_book_pages(page, total);
        }
        let ch = self.epub.chapter;
        if let Some(toc) = self.epub.toc.as_ref().filter(|t| !t.is_empty()) {
            let entries = &toc.entries[..toc.len()];
            if let Some(i) = entries.iter().position(|e| e.spine_idx == ch) {
                card.set_chapter_title(entries[i].title_str());
            }
        }
        let (no, count) = self.chapter_numbering();
        if no > 0 {
            card.set_chapter_number(no, count);
        }
    }

    // transition to error state with consistent handling
    fn enter_error(&mut self, ctx: &mut AppContext, e: Error) {
        if self.stats_pause_clock() {
            self.stats_dirty = true;
            self.arm_persist_debounce();
        }
        self.pending_position_change = None;
        self.error = Some(e);
        self.state = State::Error;
        ctx.clear_loading();
        self.mark_page_ready(ctx);
    }

    // run one step of image work queue polling while suspended;
    // chapter caching is async and only runs during active background,
    // so this only handles the sync image recv states
    pub fn bg_work_tick(&mut self, k: &mut KernelHandle<'_>) {
        match self.epub.bg_cache {
            BgCacheState::WaitNearbyImage => match self.epub_recv_image_result(k) {
                Ok(Some(_)) => {
                    if !self.try_dispatch_nearby_image(k) {
                        self.epub.bg_cache = BgCacheState::CacheChapter;
                    }
                }
                Ok(None) if work_queue::is_idle() => {
                    log::warn!("bg: worker idle with no result (suspended), recovering");
                    self.epub.bg_cache = BgCacheState::CacheChapter;
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("bg: nearby image error (suspended): {}", e);
                    self.epub.bg_cache = BgCacheState::CacheChapter;
                }
            },
            BgCacheState::WaitImage => match self.epub_recv_image_result(k) {
                Ok(Some(_)) => self.epub.bg_cache = BgCacheState::CacheImage,
                Ok(None) if work_queue::is_idle() => {
                    log::warn!("bg: worker idle with no result (suspended), recovering");
                    self.epub.bg_cache = BgCacheState::CacheImage;
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("bg: image recv error (suspended): {}", e);
                    self.epub.bg_cache = BgCacheState::CacheImage;
                }
            },
            _ => {}
        }
    }

    fn rebuild_quick_actions(&mut self) {
        let mut n = 0usize;

        // contents first, with the chapter position as its value
        if self.is_epub && self.epub.toc.as_ref().is_some_and(|t| !t.is_empty()) {
            let mut pos = StackFmt::<16>::new();
            let _ = write!(pos, "{} / {}", self.epub.chapter + 1, self.epub.spine.len());
            self.qa_buf[n] = QuickAction::trigger(QA_TOC, "Contents", "Open")
                .with_icon(sheet::ICON_LIST)
                .with_value(pos.as_str());
            n += 1;
        }

        // font size last
        self.qa_buf[n] = QuickAction::cycle(
            QA_FONT_SIZE,
            "Book font",
            self.book_font_size_idx,
            fonts::FONT_SIZE_NAMES,
        )
        .with_icon(sheet::ICON_TEXT_AA);
        n += 1;

        self.qa_count = n as u8;
    }

    fn apply_font_metrics(&mut self) {
        self.fonts = None;
        self.font_line_h = LINE_H;
        self.font_ascent = LINE_H;
        self.max_lines = LINES_PER_PAGE as u8;

        let spacing_pct = crate::kernel::config::line_spacing_pct(self.line_spacing_idx);

        if self.reader_font.family().has_regular() {
            let fs = fonts::FontSet::for_reader(self.reader_font, self.book_font_size_idx);
            let native_h = fs.line_height(fonts::Style::Regular).max(1) as u32;
            let em = fs.em_px().max(1) as u32;
            // line spacing is a multiple of the em size, so a step
            // reads identically in every family; the native metric is
            // the floor so adjacent lines never collide
            self.font_line_h = ((em * spacing_pct as u32 + 50) / 100).max(native_h) as u16;
            self.font_ascent = fs.ascent(fonts::Style::Regular);
            self.max_lines =
                ((self.text_area_h / self.font_line_h) as usize).min(LINES_PER_PAGE) as u8;
            log::debug!(
                "font: family={} size_idx={} line_h={} (em {} x {}%, native {}) ascent={} max_lines={} margin={}",
                self.reader_font.name(),
                self.book_font_size_idx,
                self.font_line_h,
                em,
                spacing_pct,
                native_h,
                self.font_ascent,
                self.max_lines,
                self.text_margin,
            );
            self.fonts = Some(fs);
        }
    }

    fn name(&self) -> &str {
        self.filename.as_str()
    }

    // Session state accessors for RTC persistence
    #[inline]
    pub fn filename_len(&self) -> usize {
        self.filename.len()
    }

    #[inline]
    pub fn filename_bytes(&self) -> &[u8] {
        self.filename.as_bytes()
    }

    #[inline]
    pub fn is_epub(&self) -> bool {
        self.is_epub
    }

    #[inline]
    pub fn chapter(&self) -> u16 {
        self.epub.chapter
    }

    #[inline]
    pub fn page(&self) -> usize {
        self.pg.page
    }

    #[inline]
    pub fn byte_offset(&self) -> u32 {
        if self.pg.page < self.pg.total_pages {
            self.pg.offsets[self.pg.page]
        } else {
            0
        }
    }

    #[inline]
    pub fn font_size_idx(&self) -> u8 {
        self.book_font_size_idx
    }

    // restore reader state from RTC session data
    //
    // sets up ALL state needed to resume without calling on_enter().
    // on_enter() would reset epub.chapter=0 and restore_offset=None,
    // clobbering the chapter/offset we just restored from RTC memory.
    // instead, we set up the state machine to enter at NeedBookmark
    // with chapter/offset pre-populated, so the reader pipeline will
    // skip the bookmark lookup and go straight to initializing the book.
    pub fn restore_state(&mut self, filename: &[u8], is_epub: bool, font_size: u8) {
        self.filename.set(filename);

        // set title from filename initially (will be replaced by
        // epub metadata once the book is loaded)
        self.title.set(self.filename.as_bytes());
        self.title_is_real = false;

        self.is_epub = is_epub;
        // the place comes from the book record at NeedBookmark, which
        // the pre-sleep flush wrote after the last page turn
        self.epub.chapter = 0;
        self.restore_offset = None;
        self.restore_page_hint = None;
        self.pending_position = None;
        self.restore_anchor = None;
        self.last_saved_pos = None;
        self.book_font_size_idx = font_size;

        // reset work queue for clean start
        self.epub.work_gen = Some(work_queue::reset());
        self.epub.bg_cache = BgCacheState::Idle;
        self.epub.ch_cached = [false; smol_epub::cache::MAX_CACHE_CHAPTERS];
        self.epub.img_scan_wrapped = false;
        self.epub.skip_large_img = false;
        self.epub.large_img_fails = 0;
        self.epub.img_retry = None;

        // set up reader pipeline: enter at NeedBookmark like a fresh open
        self.rebuild_quick_actions();
        self.apply_theme_layout();
        self.reset_paging();
        self.typeset_stale = false;
        self.pagination_stale = false;
        self.epub.ch_cache = Vec::new();
        self.file_size = 0;
        self.error = None;
        self.show_position = false;
        self.defer_image_decode = true;
        self.cache_ui_stale = false;
        self.goto_last_page = false;
        self.recent_dirty = false;
        self.pending_position_change = Some(PendingPositionChange::RestoreReady);
        self.defer_open_work_once = false;
        self.pending_toc_parse = false;
        self.pending_title_save = false;
        self.pending_cover_thumb = false;
        self.loading_cover = None;
        self.spine_hint = 0;
        self.persist_next_flush_at = None;
        self.apply_font_metrics();

        // reading statistics
        self.stats_last_uptime = crate::kernel::uptime_secs();
        self.stats_dirty = false;
        self.stats_clock_running = false;

        // enter the state machine; NeedBookmark reads the book record
        self.state = State::NeedBookmark;
        self.begin_loading(LoadingReason::Resume);
        self.first_paint_full = true;

        log::info!("reader: restore_state file={} font={}", self.name(), font_size);
    }

    pub fn save_position(&self, bm: &mut bookmarks::BookmarkCache) {
        if self.state == State::Ready {
            bm.save(
                self.filename.as_bytes(),
                self.pg.offsets[self.pg.page],
                self.epub.chapter,
            );
        }
    }

    /// Where to open the book. The book record is the source; the
    /// bundle header's bookmark and the BKMK.BIN slot are read only
    /// for a book that has no record yet (written by older firmware),
    /// and the next flush writes the record so they are never read
    /// again.
    fn bookmark_load(&mut self, k: &mut KernelHandle<'_>) -> bool {
        if let Some(p) = self.pending_position {
            if self.is_epub {
                // checked against the file once the spine is known
                return true;
            }
            self.pending_position = None;
            self.restore_offset = Some(p.byte_offset);
            self.restore_page_hint = None;
            return true;
        }

        if self.is_epub {
            if let Some(hdr) =
                plump_kernel::kernel::bundle::read_header(k.sd(), self.epub.name_hash)
            {
                if hdr.has_valid_bookmark() {
                    log::info!(
                        "bookmark: importing bundle bookmark off={} ch={} for {}",
                        hdr.bm_byte_offset,
                        hdr.bm_chapter,
                        self.name(),
                    );
                    self.epub.chapter = hdr.bm_chapter;
                    self.restore_offset = Some(hdr.bm_byte_offset);
                    self.restore_page_hint = if hdr.bm_page_hint == 0 {
                        None
                    } else {
                        Some(hdr.bm_page_hint as usize)
                    };
                    return true;
                }
            }
        }

        if let Some(slot) = k.bookmarks().find(self.filename.as_bytes()) {
            log::info!(
                "bookmark: importing BKMK.BIN slot off={} ch={} for {}",
                slot.byte_offset,
                slot.chapter,
                slot.filename_str(),
            );
            self.epub.chapter = slot.chapter;
            self.restore_offset = Some(slot.byte_offset);
            self.restore_page_hint = None;
            true
        } else {
            false
        }
    }

    /// Take the record's position once the spine and the file size
    /// are known: a replaced file starts over, everything else restores
    /// by byte offset, with the anchor resolved at NeedPage should the
    /// bundle's content format differ from the one the offset was
    /// counted in.
    fn apply_pending_position(&mut self, spine_len: usize) {
        let Some(p) = self.pending_position.take() else {
            return;
        };
        if p.archive_size != self.epub.archive_size {
            log::info!(
                "record: file size {} != recorded {}, starting over",
                self.epub.archive_size,
                p.archive_size
            );
            return;
        }
        if spine_len > 0 && p.chapter as usize >= spine_len {
            log::info!("record: chapter {} beyond spine {}, starting over", p.chapter, spine_len);
            return;
        }
        self.epub.chapter = p.chapter;
        self.restore_offset = Some(p.byte_offset);
        self.restore_anchor = p.anchor();
        self.restore_fmt = p.content_fmt;
        self.restore_layout_key = p.layout_key;
        self.restore_page_hint = Some(p.page as usize);
    }

    /// At NeedPage: the byte offset to seek, re-derived from the anchor
    /// when the chapter stream is not the one it was counted in, and
    /// the page hint only under the layout it was counted under.
    fn resolve_restore(&mut self, k: &mut KernelHandle<'_>) {
        if self.is_epub && self.epub.bundle_content_fmt != self.restore_fmt {
            if let Some(anchor) = self.restore_anchor.take() {
                match self.chapter_offset_of(k, anchor) {
                    Some(off) => {
                        log::info!(
                            "record: content fmt {} -> {}, anchor {}/{} resolves to off={}",
                            self.restore_fmt,
                            self.epub.bundle_content_fmt,
                            anchor.para,
                            anchor.word,
                            off
                        );
                        self.restore_offset = Some(off);
                    }
                    None => log::info!("record: anchor unresolvable, keeping byte offset"),
                }
            }
            self.restore_page_hint = None;
        }
        if self.restore_page_hint.is_some()
            && self.restore_layout_key != self.current_layout_key().hash()
        {
            self.restore_page_hint = None;
        }
        self.restore_anchor = None;
    }

    fn display_name(&self) -> &str {
        if !self.title.is_empty() {
            self.title.as_str()
        } else {
            self.name()
        }
    }

    fn try_load_cached_cover_thumb(&mut self, k: &mut KernelHandle<'_>) -> bool {
        if !self.is_epub || self.filename.is_empty() || self.loading_cover.is_some() {
            return false;
        }

        self.loading_cover = crate::apps::cover_cache::load_cover_variant_for(
            k,
            self.filename.as_bytes(),
            plump_kernel::kernel::bundle::CoverKind::Card,
        );

        if let Some(ref img) = self.loading_cover {
            log::debug!(
                "reader: loaded cached cover thumb {}x{} for {}",
                img.width,
                img.height,
                self.name()
            );
            true
        } else {
            false
        }
    }

    pub(crate) fn prepare_restore_loading_screen(&mut self, k: &mut KernelHandle<'_>) {
        let _ = self.try_load_cached_cover_thumb(k);
        self.prefill_title_from_bundle(k);
    }

    /// Title and chapter count from the bundle header, before the
    /// zip is even opened: the wake plate and footer then show the
    /// book, not its file name. the pipeline re-reads both later
    fn prefill_title_from_bundle(&mut self, k: &mut KernelHandle<'_>) {
        use plump_kernel::kernel::bundle;
        if !self.is_epub || self.title_is_real {
            return;
        }
        let hash = plump_kernel::util::hash::fnv1a(self.filename.as_bytes());
        let Some(hdr) = bundle::read_header(k.sd(), hash) else {
            return;
        };
        if hdr.name_hash != hash || !hdr.has_flag(bundle::FLAG_CORE_READY) {
            return;
        }
        if !hdr.title.is_empty() {
            self.title.set(hdr.title.as_bytes());
            self.title_is_real = true;
        }
        self.spine_hint = hdr.spine_count;
    }

    fn progress_pct(&self) -> u8 {
        if self.is_epub && !self.epub.spine.is_empty() {
            let spine_len = self.epub.spine.len() as u64;
            let ch = self.epub.chapter as u64;

            if ch + 1 >= spine_len
                && self.pg.fully_indexed
                && self.pg.page + 1 >= self.pg.total_pages
            {
                return 100;
            }

            let in_ch = if self.file_size == 0 {
                0u64
            } else {
                let pos = self.pg.offsets[self.pg.page] as u64;
                let size = self.file_size as u64;
                ((pos * 100) / size).min(100)
            };

            let overall = (ch * 100 + in_ch) / spine_len;
            return overall.min(100) as u8;
        }

        if self.file_size == 0 {
            return 100;
        }
        if self.pg.fully_indexed && self.pg.page + 1 >= self.pg.total_pages {
            return 100;
        }
        let pos = self.pg.offsets[self.pg.page] as u64;
        let size = self.file_size as u64;
        ((pos * 100) / size).min(100) as u8
    }

    // write the RECENT file; layout lives in apps::recent
    // returns Err on write failure (dirty state kept for retry)
    fn write_recent(&mut self, k: &mut KernelHandle<'_>) -> crate::error::Result<()> {
        plump_kernel::perf_begin!(_wr_t0);
        // txt books carry no author metadata
        let author: &[u8] = if self.is_epub {
            let al = (self.epub.meta.author_len as usize).min(recent::AUTHOR_CAP);
            &self.epub.meta.author[..al]
        } else {
            &[]
        };
        let mut buf = [0u8; recent::BUF_LEN];
        let pos = RecentRecord {
            filename: self.filename.as_bytes(),
            title: self.title.as_bytes(),
            author,
            progress: self.progress_pct(),
        }
        .encode(&mut buf);

        k.sd().write_file_in_dir(k.sd().data_dir(), RECENT_FILE, &buf[..pos])?;
        self.recent_dirty = false;
        plump_kernel::perf_event!(
            "reader",
            "write_recent bytes={} elapsed_ms={}",
            pos,
            _wr_t0.elapsed().as_millis()
        );
        Ok(())
    }
}

// read_full: read exactly buf.len() bytes from name at offset
pub(super) fn read_full(
    k: &mut KernelHandle<'_>,
    name: &str,
    offset: u32,
    buf: &mut [u8],
) -> crate::error::Result<()> {
    let mut total = 0usize;
    while total < buf.len() {
        let n = k.sd().read_file_chunk(name, offset + total as u32, &mut buf[total..])?;
        if n == 0 {
            return Err(Error::new(
                ErrorKind::ReadFailed,
                "read_full: unexpected EOF",
            ));
        }
        total += n;
    }
    Ok(())
}

// extract_zip_entry: decompress or copy one ZIP entry to a Vec
pub(super) fn extract_zip_entry(
    k: &mut KernelHandle<'_>,
    name: &str,
    zip_index: &ZipIndex,
    entry_idx: usize,
) -> Result<alloc::vec::Vec<u8>, &'static str> {
    use core::cell::RefCell;
    let entry = zip_index.entry(entry_idx);
    let k = RefCell::new(k);
    zip::extract_entry(entry, entry.local_offset, |offset, buf| {
        k.borrow_mut()
            .sd().read_file_chunk(name, offset, buf)
            .map_err(|e: Error| -> &'static str { e.into() })
    })
}

fn draw_chrome_text(
    strip: &mut StripBuffer,
    region: Region,
    text: &str,
    align: Alignment,
    font: Option<&'static BitmapFont>,
) {
    region
        .to_rect()
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
        .draw(strip)
        .unwrap();
    if text.is_empty() {
        return;
    }
    if let Some(f) = font {
        f.draw_aligned(strip, region, text, align, BinaryColor::On);
    } else {
        let tw = text.len() as u32 * 9;
        let pos = align.position(region, embedded_graphics::geometry::Size::new(tw, 18));
        let style = MonoTextStyle::new(&FONT_9X18, BinaryColor::On);
        Text::new(text, Point::new(pos.x, pos.y + 18), style)
            .draw(strip)
            .unwrap();
    }
}

/// First line of a title that may wrap once: the longest prefix
/// that fits, cut back to a word boundary, and the remainder
fn split_title_line<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> (&'a str, &'a str) {
    let cut = font.truncate_len(text, max_w);
    if cut >= text.len() {
        return (text, "");
    }
    let mut n = cut;
    while n > 0 && !text.is_char_boundary(n) {
        n -= 1;
    }
    match text[..n].rfind(' ') {
        Some(sp) if sp > 0 => (&text[..sp], text[sp + 1..].trim_start()),
        _ => (&text[..n], &text[n..]),
    }
}

fn draw_truncated_text(
    strip: &mut StripBuffer,
    font: &'static BitmapFont,
    region: Region,
    text: &str,
    align: Alignment,
    fg: BinaryColor,
) {
    let cut = font.truncate_len(text, region.w);
    if cut >= text.len() {
        font.draw_aligned(strip, region, text, align, fg);
        return;
    }

    let mut buf = [0u8; 96];
    let mut n = cut.min(buf.len().saturating_sub(3));
    while n > 0 && !text.is_char_boundary(n) {
        n -= 1;
    }
    buf[..n].copy_from_slice(&text.as_bytes()[..n]);
    buf[n] = 0xE2;
    buf[n + 1] = 0x80;
    buf[n + 2] = 0xA6;
    let truncated = core::str::from_utf8(&buf[..n + 3]).unwrap_or(text);
    font.draw_aligned(strip, region, truncated, align, fg);
}

// contents sheet, footer position and the page painter
impl ReaderApp {
    /// One-line footer: the title, and how far through the book you
    /// are as the tube the sleep card draws. Under the page and under
    /// every loading screen, in the same place.
    fn draw_footer(&self, strip: &mut StripBuffer) {
        let cf = self.chrome_font;
        FOOTER_REGION
            .to_rect()
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
            .draw(strip)
            .unwrap();

        // a book with nothing to measure (an unreadable file) gets the
        // title alone rather than an empty tube, which would read as
        // no progress rather than no figure
        let measurable = self.file_size > 0 || (self.is_epub && !self.epub.spine.is_empty());
        let mut right_x = MARGIN + FOOTER_REGION.w;
        if measurable {
            right_x -= BAR_W;
            // centred on the title's own midline, not on the middle of
            // the strip: the line box sits high in the strip, so a bar
            // centred on the strip reads as sitting under the title
            let mid = cf.map_or(CHROME_Y + CHROME_H / 2, |f| f.midline_in(FOOTER_REGION));
            let bar = Region::new(
                right_x,
                mid.saturating_sub(row::BAR_H / 2),
                BAR_W,
                row::BAR_H,
            );
            // the same figure the sleep card fills its tube with, so
            // the two agree when the screen goes to sleep mid-page
            row::draw_tube_bar(
                strip,
                bar,
                self.progress_pct() as u32,
                100,
                BinaryColor::On,
            );
        }

        // background caching is the one thing left that reports from
        // down here: it is transient, and nothing else says the book
        // is still being read off the card
        let mut sbuf = StackFmt::<24>::new();
        if self.is_epub && self.epub.bg_cache != BgCacheState::Idle {
            let cached = self.cached_chapter_count();
            let total = self.epub.spine.len();
            if cached < total {
                let _ = write!(sbuf, "[{}/{}]", cached, total);
            } else if self.epub.img_found_count > 0 {
                let _ = write!(
                    sbuf,
                    "[img {}/{}]",
                    self.epub.img_cached_count, self.epub.img_found_count,
                );
            } else {
                let _ = write!(sbuf, "[img]");
            }
        }
        if !sbuf.as_str().is_empty() {
            let w = cf.map_or(0, |font| font.measure_str(sbuf.as_str()));
            right_x = right_x.saturating_sub(BAR_GAP + w);
            draw_chrome_text(
                strip,
                Region::new(right_x, CHROME_Y, w, CHROME_H),
                sbuf.as_str(),
                Alignment::CenterRight,
                cf,
            );
        }

        let name = self.display_name();
        let title_w = right_x.saturating_sub(MARGIN + BAR_GAP);
        let title_r = Region::new(MARGIN, CHROME_Y, title_w, CHROME_H);
        match cf {
            Some(font) => draw_truncated_text(
                strip,
                font,
                title_r,
                name,
                Alignment::CenterLeft,
                BinaryColor::On,
            ),
            None => draw_chrome_text(strip, title_r, name, Alignment::CenterLeft, cf),
        }
    }

    fn draw_page(&self, strip: &mut StripBuffer) {
        let cf = self.chrome_font;
        let gray_pass = strip.gray_mode() != GrayMode::Bw;

        if self.show_chrome && !gray_pass && matches!(self.state, State::Ready | State::ShowToc) {
            self.draw_footer(strip);
        }

        if self.error.is_some() {
            self.draw_loading_screen(strip);
            return;
        }

        // loading states: fresh opens/restores get the centered
        // cover/title loading screen, while in-reader page turns and
        // chapter jumps stay intentionally blank so they don't flash
        // book metadata between chapters.
        if self.shows_loading_screen() {
            // the hold: nothing until the loading screen is due
            if self.loading_painted {
                self.draw_loading_screen(strip);
            }
            return;
        }

        if let Some(ref fs) = self.fonts {
            let line_h = self.font_line_h as i32;
            let ascent = self.font_ascent as i32;

            // fullscreen image: centre in text area, skip normal line layout
            if self.fullscreen_img {
                if let Some(ref img) = self.page_img {
                    let img_x = self.text_margin as i32
                        + ((self.text_w as i32 - img.width as i32) / 2).max(0);
                    let img_y = self.text_y as i32
                        + ((self.text_area_h as i32 - img.height as i32) / 2).max(0);
                    let img_region = Region::new(
                        img_x as u16,
                        img_y as u16,
                        img.width,
                        img.height,
                    );
                    if img_region.intersects(strip.logical_window()) {
                        strip.blit_1bpp(
                            &img.data,
                            0,
                            img.width as usize,
                            img.height as usize,
                            img.stride,
                            img_x,
                            img_y,
                            true,
                        );
                    }
                }
            } else {
                // strips are 40px-wide vertical columns in logical space
                // (portrait via 270deg rotation), so lines are culled by
                // their x extent, not by row
                let lw = strip.logical_window();
                let win_l = lw.x as i32;
                let win_r = lw.x as i32 + lw.w as i32;
                let mut img_rendered = false;
                for i in 0..self.pg.line_count {
                    let span = &self.pg.lines[i];
                    let y_top = self.text_y as i32 + self.pg.line_y[i] as i32;

                    if span.is_image() {
                        if span.is_image_origin() && !img_rendered {
                            if let Some(ref img) = self.page_img {
                                let img_x = self.text_margin as i32
                                    + ((self.text_w as i32 - img.width as i32) / 2).max(0);

                                // count reserved image lines for vertical centering
                                let mut img_line_count = 0i32;
                                for j in i..self.pg.line_count {
                                    if self.pg.lines[j].is_image() {
                                        img_line_count += 1;
                                    } else {
                                        break;
                                    }
                                }
                                let reserved_h = img_line_count * line_h;

                                // the image is already decoded at the correct
                                // budget (inline or fullscreen); just clamp to
                                // remaining vertical space as a safety net
                                let space_below =
                                    (self.text_area_h as i32 - self.pg.line_y[i] as i32).max(0);
                                let blit_h = (img.height as i32).min(space_below).max(0) as usize;

                                // center vertically within reserved lines
                                let y_offset = ((reserved_h - blit_h as i32) / 2).max(0);

                                // skip blit if image doesn't intersect strip
                                let img_region = Region::new(
                                    img_x as u16,
                                    (y_top + y_offset) as u16,
                                    img.width,
                                    blit_h as u16,
                                );
                                if img_region.intersects(strip.logical_window()) {
                                    strip.blit_1bpp(
                                        &img.data,
                                        0,
                                        img.width as usize,
                                        blit_h,
                                        img.stride,
                                        img_x,
                                        y_top + y_offset,
                                        true,
                                    );
                                }
                                img_rendered = true;
                            } else {
                                // alt-text fallback when no decoded image is
                                // available (decode failed or hasn't run yet).
                                // the origin span covers the whole IMG_REF
                                // record, so the alt text is right there
                                let page = &self.pg.buf[..self.pg.buf_len];
                                let alt: &[u8] = ImageRef::parse(page, span.start as usize)
                                    .map(|r| r.alt(page))
                                    .filter(|a| !a.is_empty())
                                    .unwrap_or(b"[image]");
                                // measure pixel width to center the run
                                let mut alt_w: u32 = 0;
                                let mut k = 0usize;
                                while k < alt.len() {
                                    let ab = alt[k];
                                    if ab >= 0xC0 {
                                        let (ch, sl) = decode_utf8_char(alt, k);
                                        alt_w += fs.advance(ch, fonts::Style::Italic) as u32;
                                        k += sl;
                                        continue;
                                    }
                                    if ab >= 0x80 {
                                        k += 1;
                                        continue;
                                    }
                                    if ab < bitmap::FIRST_CHAR && ab != b' ' {
                                        k += 1;
                                        continue;
                                    }
                                    alt_w += fs.advance(ab as char, fonts::Style::Italic) as u32;
                                    k += 1;
                                }
                                let baseline = y_top + ascent;
                                let alt_x = self.text_margin as i32
                                    + ((self.text_w as i32 - alt_w as i32).max(0)) / 2;
                                fs.draw_bytes(
                                    strip,
                                    alt,
                                    fonts::Style::Italic,
                                    alt_x,
                                    baseline,
                                );
                            }
                        }
                        continue;
                    }

                    // text: the page was decoded once at load into placed
                    // style runs (`build_page_runs`); a strip pass only
                    // blits the runs that cross its column
                    let baseline = y_top + ascent;
                    let strike_y = baseline - ascent / 3;
                    let (per, rem) = self.pg.line_just[i];
                    let (per, rem) = (per as i32, rem as i32);
                    let rf = self.pg.run_first[i] as usize;
                    let rn = self.pg.run_len[i] as usize;
                    for r in rf..rf + rn {
                        let run = self.pg.runs[r];
                        let x0 = run.x as i32;
                        let x1 = if r + 1 < rf + rn {
                            self.pg.runs[r + 1].x as i32
                        } else {
                            self.pg.line_x_end[i] as i32
                        };
                        if x0 >= win_r + 8 {
                            break;
                        }
                        if x1 < win_l - 8 {
                            continue;
                        }
                        let ms = markup::Style::unpack(run.style);
                        let sty = fonts::Style::from_markup(ms);
                        let run_end = (run.start as usize + run.len as usize).min(self.pg.buf_len);
                        let bytes = &self.pg.buf[run.start as usize..run_end];
                        let mut cx = x0;
                        let mut gap_idx = run.gap0 as i32;
                        // kerning pairs consecutive glyphs of a word; a
                        // space, soft hyphen or control byte ends the pair
                        let mut prev: Option<char> = None;
                        let mut j = 0usize;
                        while j < bytes.len() {
                            // pen passed the strip's right edge; nothing
                            // further on this run can touch it
                            if cx >= win_r + 8 {
                                break;
                            }
                            let b = bytes[j];
                            let (ch, seq_len) = if b >= 0xC0 {
                                decode_utf8_char(bytes, j)
                            } else if !(bitmap::FIRST_CHAR..0x80).contains(&b) {
                                prev = None;
                                j += 1;
                                continue;
                            } else {
                                (b as char, 1)
                            };
                            j += seq_len.max(1);
                            // SHY (U+00AD) is a zero-width break opportunity:
                            // the fonts ship it as a visible hyphen, so never
                            // draw it
                            if ch == '\u{00AD}' {
                                prev = None;
                                continue;
                            }
                            if let Some(p) = prev {
                                cx += fs.kern(p, ch, sty) as i32;
                            }
                            cx += fs.draw_char(strip, ch, sty, cx, baseline) as i32;
                            prev = if ch == ' ' { None } else { Some(ch) };
                            // justify: distribute the spare (signed) at ASCII
                            // space gaps; the leading `rem` gaps take one
                            // more pixel each
                            if ch == ' ' && (per != 0 || rem != 0) {
                                cx += per;
                                if rem > 0 && gap_idx < rem {
                                    cx += 1;
                                } else if rem < 0 && gap_idx < -rem {
                                    cx -= 1;
                                }
                                gap_idx += 1;
                            }
                        }

                        // decorations are 1-px strokes over the run's placed
                        // extent; the strip clips them to its window
                        if x1 > x0 {
                            if ms.underline {
                                Rectangle::new(
                                    Point::new(x0, baseline + 1),
                                    Size::new((x1 - x0) as u32, 1),
                                )
                                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                                .draw(strip)
                                .ok();
                            }
                            if ms.strike {
                                Rectangle::new(
                                    Point::new(x0, strike_y),
                                    Size::new((x1 - x0) as u32, 1),
                                )
                                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                                .draw(strip)
                                .ok();
                            }
                        }
                    }

                    // the hyphen of a line broken inside a word, in the
                    // face of the line's last run, right where it ended
                    if self.pg.line_hyphen[i] && rn > 0 {
                        let hx = self.pg.line_x_end[i] as i32;
                        let last = self.pg.runs[rf + rn - 1];
                        let sty = fonts::Style::from_markup(markup::Style::unpack(last.style));
                        let hw = fs.advance('-', sty) as i32;
                        if hx < win_r + 8 && hx + hw > win_l - 8 {
                            fs.draw_char(strip, '-', sty, hx, baseline);
                        }
                    }
                }
            }
        } else {
            let style = MonoTextStyle::new(&FONT_9X18, BinaryColor::On);
            for i in 0..self.pg.line_count {
                let span = self.pg.lines[i];
                let start = span.start as usize;
                let end = start + span.len as usize;
                let text = core::str::from_utf8(&self.pg.buf[start..end]).unwrap_or("");
                let y = self.text_y as i32 + i as i32 * LINE_H as i32 + LINE_H as i32;
                Text::new(text, Point::new(self.text_margin as i32, y), style)
                    .draw(strip)
                    .unwrap();
            }
        }

        if self.show_position
            && self.state == State::Ready
            && !gray_pass
            && POSITION_OVERLAY.intersects(strip.logical_window())
        {
            let mut pbuf = StackFmt::<48>::new();
            if self.is_epub && self.epub.spine.len() > 1 {
                if self.pg.fully_indexed {
                    let _ = write!(
                        pbuf,
                        "Ch {}/{}  Page {}/{}",
                        self.epub.chapter + 1,
                        self.epub.spine.len(),
                        self.pg.page + 1,
                        self.pg.total_pages
                    );
                } else {
                    let _ = write!(
                        pbuf,
                        "Ch {}/{}  Page {}",
                        self.epub.chapter + 1,
                        self.epub.spine.len(),
                        self.pg.page + 1
                    );
                }
            } else if self.pg.fully_indexed {
                let _ = write!(pbuf, "Page {}/{}", self.pg.page + 1, self.pg.total_pages);
            } else {
                let _ = write!(
                    pbuf,
                    "Page {}  ({}%)",
                    self.pg.page + 1,
                    self.progress_pct()
                );
            }

            POSITION_OVERLAY
                .to_rect()
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(strip)
                .unwrap();
            let text = pbuf.as_str();
            if let Some(f) = cf {
                f.draw_aligned(
                    strip,
                    POSITION_OVERLAY,
                    text,
                    Alignment::Center,
                    BinaryColor::Off,
                );
            } else {
                let tw = text.len() as u32 * 9;
                let pos = Alignment::Center.position(POSITION_OVERLAY, Size::new(tw, 18));
                let style = MonoTextStyle::new(&FONT_9X18, BinaryColor::Off);
                Text::new(text, Point::new(pos.x, pos.y + 18), style)
                    .draw(strip)
                    .unwrap();
            }
        }
    }

    /// (page, total) across the whole book from the layout directory
    /// counts; None until every chapter has a layout.
    fn book_position(&self) -> Option<(u32, u32)> {
        let n = self.epub.spine.len();
        if !self.is_epub || n == 0 || self.pg.total_pages == 0 {
            return None;
        }
        let mut before = 0u32;
        let mut total = 0u32;
        for (i, &count) in self.toc_pages.iter().take(n).enumerate() {
            if count == 0 {
                return None;
            }
            if i < self.epub.chapter as usize {
                before += count as u32;
            }
            total += count as u32;
        }
        Some((before + self.pg.page as u32 + 1, total))
    }

    /// Whether the chapter about to be indexed already has a layout at
    /// this font. reads the layout directory only when the cached
    /// counts do not know the chapter yet; the counts are taken as
    /// stored, never overridden from the page state, which at this
    /// point still describes the chapter being left
    fn chapter_has_layout(&mut self, k: &mut KernelHandle<'_>) -> bool {
        let ch = self.epub.chapter as usize;
        let n = self.epub.spine.len().min(MAX_SPINE);
        if ch >= n {
            return true;
        }
        if self.toc_pages[ch] == 0 {
            layout::cache::chapter_page_counts(k, self.epub.name_hash, &mut self.toc_pages[..n]);
        }
        self.toc_pages[ch] > 0
    }

    /// Reload the per-chapter page counts; true when they changed.
    fn refresh_chapter_pages(&mut self, k: &mut KernelHandle<'_>) -> bool {
        if !self.is_epub {
            return false;
        }
        let n = self.epub.spine.len().min(MAX_SPINE);
        let mut fresh = [0u16; MAX_SPINE];
        layout::cache::chapter_page_counts(k, self.epub.name_hash, &mut fresh[..n]);
        // the chapter on screen knows its own count best
        let ch = self.epub.chapter as usize;
        if ch < n && self.pg.fully_indexed && self.pg.total_pages > 0 {
            fresh[ch] = self.pg.total_pages.min(u16::MAX as usize) as u16;
        }
        let changed = fresh[..n] != self.toc_pages[..n];
        self.toc_pages[..n].copy_from_slice(&fresh[..n]);
        changed
    }

    fn contents_geom(&self) -> SheetGeom {
        // a chapter row is a title in the book face over the reading
        // bar the current chapter carries, so it is sized for both
        let fonts = SheetFonts::for_ui(self.ui_font_idx);
        let row_h = fonts.row_h(self.contents_title_font(), false, true);
        if self.toc_expanded {
            SheetGeom::full(None, row_h)
        } else {
            SheetGeom::anchored(CONTENTS_ROWS, None, row_h)
        }
    }

    /// Chapter titles are book content, so they take the reader's own
    /// face at the small tier rather than the UI face.
    fn contents_title_font(&self) -> &'static BitmapFont {
        fonts::body_font(self.reader_font.family(), 1)
    }

    fn toc_scroll_into_view(&mut self) {
        let len = self.epub.toc.as_ref().map_or(0, |t| t.len());
        let vis = self.contents_geom().rows.max(1);
        if self.epub.toc_selected < self.epub.toc_scroll {
            self.epub.toc_scroll = self.epub.toc_selected;
        } else if self.epub.toc_selected >= self.epub.toc_scroll + vis {
            self.epub.toc_scroll = self.epub.toc_selected + 1 - vis;
        }
        let max_scroll = len.saturating_sub(vis);
        if self.epub.toc_scroll > max_scroll {
            self.epub.toc_scroll = max_scroll;
        }
    }

    /// Open the contents sheet centred on the chapter being read.
    fn open_contents(&mut self, ctx: &mut AppContext) {
        let Some(toc) = self.epub.toc.as_ref() else {
            log::info!("reader: contents requested without a toc");
            return;
        };
        if !self.is_epub || toc.is_empty() {
            log::info!("reader: contents requested, toc empty");
            return;
        }
        log::info!("reader: contents sheet open, {} entries", toc.len());
        self.epub.toc_selected = 0;
        for i in 0..toc.len() {
            if toc.entries[i].spine_idx == self.epub.chapter {
                self.epub.toc_selected = i;
                break;
            }
        }
        self.toc_expanded = false;
        let vis = self.contents_geom().rows.max(1);
        self.epub.toc_scroll = self.epub.toc_selected.saturating_sub(vis / 2);
        self.toc_scroll_into_view();
        self.toc_pages_stale = true;
        self.state = State::ShowToc;
        ctx.mark_dirty(PAGE_REGION);
    }

    fn close_contents(&mut self, ctx: &mut AppContext) {
        self.state = State::Ready;
        ctx.mark_dirty(PAGE_REGION);
    }

    /// Move the contents cursor, wrapping at both ends. A scroll
    /// repaints the sheet; a move within the window only its two rows.
    fn contents_move(&mut self, delta: i32, ctx: &mut AppContext) {
        let len = self.epub.toc.as_ref().map_or(0, |t| t.len());
        if len == 0 {
            return;
        }
        let old = self.epub.toc_selected;
        let old_scroll = self.epub.toc_scroll;
        self.epub.toc_selected = (old as i32 + delta).rem_euclid(len as i32) as usize;
        self.toc_scroll_into_view();
        let geom = self.contents_geom();
        if self.epub.toc_scroll != old_scroll {
            ctx.mark_dirty(geom.region);
        } else {
            ctx.mark_dirty(geom.row_region(old - self.epub.toc_scroll));
            ctx.mark_dirty(geom.row_region(self.epub.toc_selected - self.epub.toc_scroll));
        }
    }

    fn draw_contents_sheet(&self, strip: &mut StripBuffer) {
        let Some(toc) = self.epub.toc.as_ref() else {
            return;
        };
        let geom = self.contents_geom();
        let fonts = SheetFonts::for_ui(self.ui_font_idx);
        let title_font = self.contents_title_font();

        sheet::draw_frame(strip, &geom);

        let mut meta = StackFmt::<64>::new();
        let author = self.epub.meta.author_str();
        if !author.is_empty() {
            let _ = write!(meta, "{} \u{00B7} ", author);
        }
        let n = self.epub.spine.len();
        let _ = write!(meta, "{} chapters", n);
        let unknown = self.toc_pages.iter().take(n).filter(|&&c| c == 0).count();
        if unknown == 0 {
            let total: u32 = self.toc_pages.iter().take(n).map(|&c| c as u32).sum();
            let _ = write!(meta, " \u{00B7} {} pages", total);
        } else {
            let _ = write!(meta, " \u{00B7} {} not yet laid out", unknown);
        }
        sheet::draw_header(strip, &geom, &fonts, self.display_name(), "CONTENTS", meta.as_str());
        sheet::draw_groups(strip, &geom);

        let len = toc.len();
        let scroll = self.epub.toc_scroll;
        let vis = geom.rows.min(len.saturating_sub(scroll));
        let mut val = StackFmt::<24>::new();
        for i in 0..vis {
            let idx = scroll + i;
            let entry = &toc.entries[idx];
            let here = entry.spine_idx != 0xFFFF && entry.spine_idx == self.epub.chapter;
            val.clear();
            let progress = if here && self.pg.total_pages > 0 {
                let _ = write!(val, "{} / {}", self.pg.page + 1, self.pg.total_pages);
                Some((self.pg.page as u32 + 1, self.pg.total_pages as u32))
            } else {
                let pages = self
                    .toc_pages
                    .get(entry.spine_idx as usize)
                    .copied()
                    .unwrap_or(0);
                if pages > 0 {
                    let _ = write!(val, "{}", pages);
                } else {
                    let _ = write!(val, "\u{00B7}");
                }
                None
            };
            sheet::draw_row(
                strip,
                &geom,
                i,
                &fonts,
                &RowSpec {
                    lead: if here {
                        RowLead::Bookmark
                    } else {
                        RowLead::Number(idx as u16 + 1)
                    },
                    text: entry.title_str(),
                    text_font: title_font,
                    value: val.as_str(),
                    selected: idx == self.epub.toc_selected,
                    sub: "",
                    progress,
                    chip: ValueChip::None,
                },
            );
        }

        let hints: &[(HintSlot, &str)] = if self.toc_expanded {
            &[
                (HintSlot::Back, "CLOSE"),
                (HintSlot::Ok, "GO"),
                (HintSlot::Left, "\u{2039} SHRINK"),
            ]
        } else {
            &[
                (HintSlot::Back, "CLOSE"),
                (HintSlot::Ok, "GO"),
                (HintSlot::Right, "EXPAND \u{203A}"),
            ]
        };
        sheet::draw_hints(strip, &geom, &fonts, hints);
    }
}

impl App<AppId> for ReaderApp {
    fn on_enter(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        let msg = ctx.message();
        self.filename.set(msg);
        self.open_contents_on_ready = ctx.message_tag() == MSG_TAG_OPEN_CONTENTS;
        self.toc_expanded = false;
        self.toc_pages = [0; MAX_SPINE];

        self.title.set(self.filename.as_bytes());
        self.title_is_real = false;

        // Bump to a new work-queue generation and drain stale work
        // from any previous book (covers the case where on_enter is
        // called without a preceding on_exit, e.g. Replace transition).
        self.epub.work_gen = Some(work_queue::reset());
        self.epub.bg_cache = BgCacheState::Idle;
        self.epub.ch_cached = [false; cache::MAX_CACHE_CHAPTERS];
        self.epub.img_scan_wrapped = false;
        self.epub.skip_large_img = false;
        self.epub.large_img_fails = 0;
        self.epub.img_retry = None;

        self.is_epub = epub::is_epub_filename(self.name());
        self.rebuild_quick_actions();
        self.apply_theme_layout();
        self.reset_paging();
        self.typeset_stale = false;
        self.pagination_stale = false;
        self.epub.ch_cache = Vec::new();
        self.file_size = 0;
        self.epub.chapter = 0;
        self.error = None;
        self.show_position = false;
        self.defer_image_decode = true;
        self.cache_ui_stale = false;
        self.goto_last_page = false;
        self.restore_offset = None;
        self.restore_page_hint = None;
        self.pending_position = None;
        self.restore_anchor = None;
        self.restore_fmt = 0;
        self.restore_layout_key = 0;
        self.last_saved_pos = None;
        self.record_seq = 0;
        self.recent_dirty = false;
        self.pending_position_change = Some(PendingPositionChange::OpenReady);
        self.defer_open_work_once = false;
        self.pending_toc_parse = false;
        self.pending_title_save = false;
        self.pending_cover_thumb = false;
        self.loading_cover = None;
        self.spine_hint = 0;
        self.persist_next_flush_at = None;

        self.apply_font_metrics();

        let _ = self.try_load_cached_cover_thumb(k);

        // load existing stats for this book
        self.stats_last_uptime = crate::kernel::uptime_secs();
        self.stats_dirty = false;
        self.stats_clock_running = false;

        self.state = State::NeedBookmark;

        log::info!("reader: opening {}", self.name());

        self.begin_loading(LoadingReason::Open);
        self.first_paint_full = true;
    }

    fn on_exit(&mut self) {
        // Cancel any in-flight background cache work so the worker
        // doesn't write stale results after we switch books.
        if self.is_epub {
            work_queue::reset();
            self.epub.bg_cache = BgCacheState::Idle;
        }

        self.pg.line_count = 0;
        self.pg.buf_len = 0;
        self.pg.prefetch_page = NO_PREFETCH;
        if self.stats_pause_clock() {
            self.stats_dirty = true;
            self.arm_persist_debounce();
        }

        self.pg.prefetch_len = 0;
        self.restore_offset = None;
        self.restore_page_hint = None;
        // NOTE: recent_dirty / stats_dirty intentionally NOT cleared here;
        // flush_deferred_persistence(Transition) runs before on_exit and
        // handles them. if it failed, dirty state is kept for retry.
        self.pending_position_change = None;
        self.defer_open_work_once = false;
        self.pending_toc_parse = false;
        self.pending_title_save = false;
        self.pending_cover_thumb = false;
        self.loading_cover = None;
        self.show_position = false;
        self.epub.ch_cache = Vec::new();
        self.page_img = None;

        if self.is_epub {
            self.epub.toc = None;
            self.epub.toc_source = None;
        }
    }

    fn on_suspend(&mut self) {
        // pause the reading-time clock so menu/Home time isn't counted
        if self.stats_pause_clock() {
            self.stats_dirty = true;
            self.arm_persist_debounce();
        }
        // background caching continues while suspended -- the worker
        // task runs independently and our work_gen stays valid
    }

    fn on_pre_sleep(&mut self, _k: &mut KernelHandle<'_>) {
        // drop transient heap the reader no longer needs before deep
        // sleep. MCU resets on wake so any retained heap is lost
        // anyway; freeing now makes room for the sleep wallpaper
        // allocator (~96KB) which would otherwise OOM and fall back
        // to the plain text screen.
        //
        // wake restores via restore_state which resets the state
        // machine to NeedBookmark; NeedOpf/NeedToc re-parse metadata
        // from SD, NeedIndex re-reads the chapter cache. one-time
        // cost: ~1-2s on the first page turn after wake.
        let kp_state = self.pg.chapter_lines.capacity() * size_of::<crate::apps::reader::layout::LineLayout>()
            + self.pg.kp_pages.capacity() * size_of::<crate::apps::reader::layout::PageLayout>()
            + self.pg.image_block_lines.capacity();
        let freed = self.epub.ch_cache.capacity()
            + self.pg.prefetch.capacity()
            + self.page_img.as_ref().map_or(0, |i| i.data.capacity())
            + self.loading_cover.as_ref().map_or(0, |i| i.data.capacity())
            + self.epub.toc.as_ref().map_or(0, |_| size_of::<EpubToc>())
            + kp_state;

        // cancel any in-flight image decode so the worker drops its buffer
        work_queue::reset();

        self.epub.ch_cache = Vec::new();
        self.pg.prefetch = Vec::new();
        self.pg.prefetch_len = 0;
        self.page_img = None;
        self.loading_cover = None;
        // toc/toc_source re-parsed on wake via NeedToc state
        self.epub.toc = None;
        self.epub.toc_source = None;

        // K-P chapter state is biggest hidden retainer for long
        // chapters: 2113 LineLayout × 16 B ≈ 33 KB for "Seventy-Two
        // Letters". With ch_cache freed but K-P state retained, the
        // wallpaper allocator OOMs. The on-disk PIDX cache means
        // re-typesetting on wake is fast (cache hit, no K-P pass).
        self.pg.clear_kp_layout();

        log::info!(
            "reader: pre-sleep freed ~{}KB of transient heap",
            freed / 1024
        );
    }

    fn on_resume(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        // resume reading-time clock
        self.stats_resume_clock();

        // Restore our generation so the worker considers in-flight
        // results current again (another app may have submitted work
        // under a different generation while we were suspended).
        if let Some(g) = self.epub.work_gen {
            work_queue::resume(g);
        }

        // re-derive text area geometry from the (possibly changed) theme
        self.apply_theme_layout();
        self.apply_font_metrics();

        // the propagate_fonts setters flag staleness when a layout
        // input changed while we were suspended; the in-RAM page
        // tables are keyed to the old metrics. states before
        // NeedIndex have no page tables yet and finish the open with
        // the new metrics on their own, so only re-index once the
        // pipeline has reached (or passed) indexing.
        let layout_changed = self.typeset_stale || self.pagination_stale;
        let typeset_changed = self.typeset_stale;
        self.typeset_stale = false;
        self.pagination_stale = false;
        if layout_changed
            && matches!(
                self.state,
                State::NeedIndex | State::NeedPage | State::Ready | State::ShowToc
            )
        {
            // keep the reading position across the re-layout: NeedPage
            // maps the byte offset back to a page once re-indexed
            if self.restore_offset.is_none()
                && matches!(self.state, State::Ready | State::ShowToc)
            {
                self.restore_offset = Some(self.byte_offset());
                self.restore_page_hint = Some(self.pg.page);
            }
            self.reset_paging();
            // invalidate the persisted layout only when the breaks
            // moved (font, family, text width): a spacing-only change
            // keeps the cached line table and preindex re-paginates it
            // in RAM instead of re-typesetting.
            if typeset_changed && self.is_epub {
                let new_key = layout::LayoutKey::current(
                    self.book_font_size_idx,
                    self.reader_font.to_idx(),
                    plump_kernel::kernel::bundle::CONTENT_FMT_LATEST,
                    self.text_w as u16,
                    self.font_line_h,
                    self.max_lines,
                );
                let spine_len = self.epub.spine.len();
                let name_hash = self.epub.name_hash;
                if let Err(e) =
                    layout::cache::invalidate_layoutidx(k, name_hash, spine_len, &new_key)
                {
                    log::warn!("reader: invalidate_layoutidx failed: {}", e);
                }
            }
            if self.is_epub && self.epub.chapters_cached {
                self.state = State::NeedIndex;
            } else {
                self.state = State::NeedPage;
            }
            self.begin_loading(LoadingReason::Font);
            self.paint_loading(ctx);
        }
        ctx.mark_dirty(PAGE_REGION);
    }

    fn background_step(
        &mut self,
        ctx: &mut AppContext,
        k: &mut KernelHandle<'_>,
        budget: BgBudget,
    ) -> BgOutcome {
        // drain every pass so the stats stay current no matter which
        // pass flushes them
        self.drain_day_stats(k);

        // apply a caching-indicator update that was held back during
        // a waveform-window step
        if self.cache_ui_stale && budget.allows_repaint() {
            self.cache_ui_stale = false;
            if self.epub.bg_cache == BgCacheState::Idle {
                ctx.clear_loading();
            } else {
                self.set_cache_loading(ctx);
            }
        }

        // loading hold: the screen goes up once it is overdue
        self.loading_tick(ctx);

        // Phase 1: Open pipeline (NeedBookmark..NeedPage)
        // Each state does ONE step and returns Progress { more: true }
        match self.state {
            State::NeedBookmark => {
                plump_kernel::perf_begin!(_t0);
                self.record_load(k);
                self.bookmark_load(k);
                if self.try_load_cached_cover_thumb(k) {
                    ctx.mark_dirty(self.loading_visual_region());
                }

                if self.is_epub {
                    self.epub.zip.clear();
                    self.epub.meta = EpubMeta::new();
                    self.epub.spine = EpubSpine::new();
                    self.epub.chapters_cached = false;
                    self.goto_last_page = false;
                    self.state = State::NeedInit;
                    self.set_loading_ui(ctx);
                } else {
                    self.state = State::NeedPage;
                    self.set_loading_ui(ctx);
                }
                plump_kernel::perf_event!(
                    "reader",
                    "NeedBookmark to={:?} elapsed_ms={}",
                    self.state,
                    _t0.elapsed().as_millis()
                );
                return BgOutcome::Progress { more: true };
            }

            State::NeedInit => {
                plump_kernel::perf_begin!(_t0);
                let fname = self.filename;
        let name = fname.as_str();
                match self.epub.init_zip(k, name, &mut self.pg.buf) {
                    Ok(()) => {
                        self.try_prefill_title_from_cache_header(k);
                        self.state = State::NeedOpf;
                        self.set_loading_ui(ctx);
                        plump_kernel::perf_event!(
                            "reader",
                            "NeedInit ok to=NeedOpf elapsed_ms={}",
                            _t0.elapsed().as_millis()
                        );
                    }
                    Err(e) => {
                        plump_kernel::perf_event!(
                            "reader",
                            "NeedInit err_kind={:?} err_src={} elapsed_ms={}",
                            e.kind(),
                            e.source_tag(),
                            _t0.elapsed().as_millis()
                        );
                        log::info!("reader: epub init (zip) failed: {}", e);
                        self.epub.cache_step = None;
                        self.enter_error(ctx, e);
                    }
                }
                return BgOutcome::Progress { more: true };
            }

            State::NeedOpf => {
                plump_kernel::perf_begin!(_t0);
                match self.epub_init_opf(k) {
                    Ok(()) => {
                        let spine_len = self.epub.spine.len();
                        self.apply_pending_position(spine_len);
                        if spine_len > 0 && self.epub.chapter as usize >= spine_len {
                            self.epub.chapter = (spine_len - 1) as u16;
                        }
                        self.pending_title_save = self.title_is_real;
                        // only arm when the bundle is already complete — for
                        // incomplete bundles the fresh-import path
                        // (epubs.rs::bg_cache_step_sync) arms the flag on the
                        // CORE_READY rising edge. arming before then spins
                        // `run_deferred_open_work` (defer + re-arm) and
                        // starves the background caching that flips
                        // CORE_READY.
                        self.pending_cover_thumb = self.epub.meta.has_cover()
                            && plump_kernel::kernel::bundle::read_header(
                                k.sd(),
                                self.epub.name_hash,
                            )
                            .is_some_and(|h| {
                                h.has_flag(plump_kernel::kernel::bundle::FLAG_CORE_READY)
                            });
                        self.state = State::NeedToc;
                        self.set_loading_ui(ctx);
                        plump_kernel::perf_event!(
                            "reader",
                            "NeedOpf ok to=NeedToc spine_len={} elapsed_ms={}",
                            spine_len,
                            _t0.elapsed().as_millis()
                        );
                    }
                    Err(e) => {
                        plump_kernel::perf_event!(
                            "reader",
                            "NeedOpf err_kind={:?} err_src={} elapsed_ms={}",
                            e.kind(),
                            e.source_tag(),
                            _t0.elapsed().as_millis()
                        );
                        log::info!("reader: epub init (opf) failed: {}", e);
                        self.epub.cache_step = None;
                        self.enter_error(ctx, e);
                    }
                }
                return BgOutcome::Progress { more: true };
            }

            State::NeedToc => {
                plump_kernel::perf_begin!(_t0);
                // parse the TOC here while the heap is still empty. the
                // deferred parse after the first page ran with the
                // chapter cache resident, and on a 107 KB chapter the
                // zip inflate could not find 4 KB contiguous: OOM panic
                // on every open of a book bookmarked in such a chapter
                self.pending_toc_parse = false;
                self.load_toc(k);
                // the Contents row exists only once the toc does
                self.rebuild_quick_actions();
                self.state = State::NeedCache;
                self.set_loading_ui(ctx);
                plump_kernel::perf_event!(
                    "reader",
                    "NeedToc to=NeedCache elapsed_ms={}",
                    _t0.elapsed().as_millis()
                );
                return BgOutcome::Progress { more: true };
            }

            State::NeedCache => {
                plump_kernel::perf_begin!(_t0);
                match self.epub.check_cache(k, &mut self.pg.buf) {
                    Ok(true) => {
                        // cache hit
                        self.state = State::NeedIndex;
                        self.set_loading_ui(ctx);
                        plump_kernel::perf_event!(
                            "reader",
                            "NeedCache hit=true to=NeedIndex elapsed_ms={}",
                            _t0.elapsed().as_millis()
                        );
                        return BgOutcome::Progress { more: true };
                    }
                    Ok(false) => {
                        // cache miss: start/continue caching the current chapter.
                        // seconds of work, so the loading screen goes up first
                        self.paint_loading(ctx);
                        let ch = self.epub.chapter as usize;
                        let fname = self.filename;
                let epub_name = fname.as_str();

                        if self.epub.cache_step.is_none() {
                            // begin caching
                            if let Err(e) = self.epub.cache_chapter_begin(k, ch, &epub_name) {
                                plump_kernel::perf_event!(
                                    "reader",
                                    "NeedCache err_kind={:?} err_src={} ch={} elapsed_ms={}",
                                    e.kind(),
                                    e.source_tag(),
                                    ch,
                                    _t0.elapsed().as_millis()
                                );
                                log::info!("reader: cache ch{} begin failed: {}", ch, e);
                                self.epub.cache_step = None;
                                self.enter_error(ctx, e);
                                return BgOutcome::Progress { more: true };
                            }
                        }

                        // advance one step
                        match self.epub.cache_chapter_step(k, ch, &epub_name) {
                            Ok(false) => {
                                // more work remains
                                return BgOutcome::Progress { more: true };
                            }
                            Ok(true) => {
                                // chapter done
                                self.epub.chapters_cached = true;
                                self.epub.cache_chapter = 0;

                                if self.try_dispatch_nearby_image(k) {
                                    self.epub.bg_cache = BgCacheState::WaitNearbyImage;
                                } else {
                                    self.epub.bg_cache = BgCacheState::CacheChapter;
                                }

                                self.state = State::NeedIndex;
                                self.set_loading_ui(ctx);
                                plump_kernel::perf_event!(
                                    "reader",
                                    "NeedCache hit=false ch={} to=NeedIndex elapsed_ms={}",
                                    ch,
                                    _t0.elapsed().as_millis()
                                );
                                return BgOutcome::Progress { more: true };
                            }
                            Err(e) => {
                                plump_kernel::perf_event!(
                                    "reader",
                                    "NeedCache err_kind={:?} err_src={} ch={} elapsed_ms={}",
                                    e.kind(),
                                    e.source_tag(),
                                    ch,
                                    _t0.elapsed().as_millis()
                                );
                                log::info!("reader: cache ch{} failed: {}", ch, e);
                                self.epub.cache_step = None;
                                self.enter_error(ctx, e);
                                return BgOutcome::Progress { more: true };
                            }
                        }
                    }
                    Err(e) => {
                        plump_kernel::perf_event!(
                            "reader",
                            "NeedCache err_kind={:?} err_src={} elapsed_ms={}",
                            e.kind(),
                            e.source_tag(),
                            _t0.elapsed().as_millis()
                        );
                        log::info!("reader: cache check failed: {}", e);
                        self.epub.cache_step = None;
                        self.enter_error(ctx, e);
                        return BgOutcome::Progress { more: true };
                    }
                }
            }

            State::NeedIndex => {
                plump_kernel::perf_begin!(_t0);
                // ensure the target chapter is cached before indexing
                if self.is_epub
                    && self.epub.chapters_cached
                    && !self.epub.ch_cached[self.epub.chapter as usize]
                {
                    let ch = self.epub.chapter as usize;
                    let fname = self.filename;
                let epub_name = fname.as_str();

                    // the chapter still has to be cached: seconds, so the
                    // loading screen goes up first
                    self.paint_loading(ctx);
                    if self.epub.cache_step.is_none() {
                        if let Err(e) = self.epub.cache_chapter_begin(k, ch, &epub_name) {
                            plump_kernel::perf_event!(
                                "reader",
                                "NeedIndex stage=cache_prereq err_kind={:?} err_src={} elapsed_ms={}",
                                e.kind(),
                                e.source_tag(),
                                _t0.elapsed().as_millis()
                            );
                            self.epub.cache_step = None;
                            self.enter_error(ctx, e);
                            return BgOutcome::Progress { more: true };
                        }
                    }

                    match self.epub.cache_chapter_step(k, ch, &epub_name) {
                        Ok(false) => {
                            // prereq not done yet
                            return BgOutcome::Progress { more: true };
                        }
                        Ok(true) => {
                            // prereq done, fall through to indexing below
                        }
                        Err(e) => {
                            plump_kernel::perf_event!(
                                "reader",
                                "NeedIndex stage=cache_prereq err_kind={:?} err_src={} elapsed_ms={}",
                                e.kind(),
                                e.source_tag(),
                                _t0.elapsed().as_millis()
                            );
                            self.epub.cache_step = None;
                            self.enter_error(ctx, e);
                            return BgOutcome::Progress { more: true };
                        }
                    }
                }

                // no layout for this chapter at this font means a full
                // typeset, seconds on a long chapter: show the screen
                // before it, not after
                if !self.loading_painted && self.is_epub && !self.chapter_has_layout(k) {
                    self.paint_loading(ctx);
                }

                let want_last = self.goto_last_page;
                self.goto_last_page = false;

                // a typeset OOM retry re-enters this arm with the chapter
                // already indexed and ch_cache loaded; re-running
                // epub_index_chapter would free ch_cache and force a full
                // SD re-read per wait step
                let retrying = self.typeset_oom_waits > 0
                    && self.typeset_retry_ch == self.epub.chapter;
                if !retrying {
                    self.typeset_oom_waits = 0;
                    self.epub_index_chapter();
                }

                if self.is_epub {
                    // try_cache_chapter is best-effort: it can return false
                    // (oversized chapter, OOM). preindex_all_pages must run
                    // either way so it can clear the prior chapter's K-P
                    // state (otherwise has_kp_layout() stays true with
                    // stale data) and fall back to bundle-streamed greedy
                    // when ch_cache is empty.
                    if !retrying {
                        self.epub.try_cache_chapter(k);
                    }
                    if matches!(
                        self.preindex_all_pages(k),
                        paging::PreindexOutcome::RetryLater
                    ) {
                        // typeset is waiting on the decode worker to free
                        // its buffers. drain a finished result now (frees
                        // memory and persists the image), stay in
                        // NeedIndex, and let the scheduler wake us on the
                        // next worker completion
                        self.goto_last_page = want_last;
                        self.typeset_retry_ch = self.epub.chapter;
                        let _ = self.epub_recv_image_result(k);
                        return BgOutcome::WaitingExternal;
                    }
                }

                if want_last {
                    match self.scan_to_last_page(k) {
                        Ok(()) => {
                            self.finish_ready_transition(ctx);
                            plump_kernel::perf_event!(
                                "reader",
                                "NeedIndex mode=last_page to=Ready pages={} elapsed_ms={}",
                                self.pg.total_pages,
                                _t0.elapsed().as_millis()
                            );
                        }
                        Err(e) => {
                            plump_kernel::perf_event!(
                                "reader",
                                "NeedIndex mode=last_page err_kind={:?} err_src={} elapsed_ms={}",
                                e.kind(),
                                e.source_tag(),
                                _t0.elapsed().as_millis()
                            );
                            self.epub.cache_step = None;
                            self.enter_error(ctx, e);
                        }
                    }
                } else {
                    self.state = State::NeedPage;
                    self.set_loading_ui(ctx);
                    plump_kernel::perf_event!(
                        "reader",
                        "NeedIndex to=NeedPage pages={} fully_indexed={} elapsed_ms={}",
                        self.pg.total_pages,
                        self.pg.fully_indexed,
                        _t0.elapsed().as_millis()
                    );
                }
                return BgOutcome::Progress { more: true };
            }

            State::NeedPage => {
                plump_kernel::perf_begin!(_t0);
                self.resolve_restore(k);
                let page_hint = self.restore_page_hint.take();
                if let Some(target_off) = self.restore_offset.take() {
                    if self.pg.fully_indexed && self.pg.total_pages > 0 {
                        self.pg.page = self.locate_page_for_offset(target_off, page_hint);
                        if let Err(e) = self.load_page_dispatched(k) {
                            plump_kernel::perf_event!(
                                "reader",
                                "NeedPage mode=restore_indexed err_kind={:?} err_src={} elapsed_ms={}",
                                e.kind(),
                                e.source_tag(),
                                _t0.elapsed().as_millis()
                            );
                            self.epub.cache_step = None;
                            self.enter_error(ctx, e);
                        }
                    } else {
                        self.pg.page = 0;
                        loop {
                            match self.load_page_dispatched(k) {
                                Ok(()) => {}
                                Err(e) => {
                                    plump_kernel::perf_event!(
                                        "reader",
                                        "NeedPage mode=restore_scan err_kind={:?} err_src={} elapsed_ms={}",
                                        e.kind(),
                                        e.source_tag(),
                                        _t0.elapsed().as_millis()
                                    );
                                    self.epub.cache_step = None;
                                    self.enter_error(ctx, e);
                                    break;
                                }
                            }
                            if self.pg.page + 1 >= self.pg.total_pages {
                                break;
                            }
                            if self.pg.offsets[self.pg.page + 1] > target_off {
                                break;
                            }
                            self.pg.page += 1;
                        }
                    }
                    if self.state != State::Error {
                        self.finish_ready_transition(ctx);
                        plump_kernel::perf_event!(
                            "reader",
                            "NeedPage mode=restore to=Ready page={} elapsed_ms={}",
                            self.pg.page,
                            _t0.elapsed().as_millis()
                        );
                    }
                } else {
                    match self.load_page_dispatched(k) {
                        Ok(()) => {
                            self.finish_ready_transition(ctx);
                            plump_kernel::perf_event!(
                                "reader",
                                "NeedPage mode=open to=Ready page={} elapsed_ms={}",
                                self.pg.page,
                                _t0.elapsed().as_millis()
                            );
                        }
                        Err(e) => {
                            plump_kernel::perf_event!(
                                "reader",
                                "NeedPage mode=open err_kind={:?} err_src={} elapsed_ms={}",
                                e.kind(),
                                e.source_tag(),
                                _t0.elapsed().as_millis()
                            );
                            log::info!("reader: load failed: {}", e);
                            self.epub.cache_step = None;
                            self.enter_error(ctx, e);
                        }
                    }
                }
                return BgOutcome::Progress { more: true };
            }

            State::Error => return BgOutcome::Idle,

            // Ready / ShowToc: handled below
            _ => {}
        }

        // Phase 2: Deferred open work (Ready/ShowToc state)
        if matches!(self.state, State::Ready | State::ShowToc) {
            if self.defer_open_work_once {
                self.defer_open_work_once = false;
                return BgOutcome::Progress { more: self.has_pending_open_work() || self.has_bg_work() };
            }
            if self.toc_pages_stale {
                self.toc_pages_stale = false;
                if self.refresh_chapter_pages(k) {
                    let region = if self.state == State::ShowToc {
                        self.contents_geom().region
                    } else {
                        FOOTER_REGION
                    };
                    ctx.mark_dirty(region);
                }
            }
            if self.run_deferred_open_work(k) {
                return BgOutcome::Progress { more: self.has_pending_open_work() || self.has_bg_work() };
            }
        }

        // Phase 3: Background caching
        if matches!(
            self.state,
            State::Ready | State::ShowToc | State::NeedIndex | State::NeedPage
        ) && self.epub.bg_cache != BgCacheState::Idle
        {
            // indicator repaints are held back during waveform-window
            // steps: the mark would force the closing phase to abandon
            // and re-drive the whole region (serial signature:
            // partial_abandon right after a chapter-cross turn). the
            // deferred paint lands via cache_ui_stale on the next
            // permissive step
            let can_paint = budget.allows_repaint();
            if !ctx.loading_active() {
                if can_paint {
                    self.set_cache_loading(ctx);
                } else {
                    self.cache_ui_stale = true;
                }
            }
            let prev_count = self.cached_chapter_count();
            let prev_bg = self.epub.bg_cache;
            let prev_img_found = self.epub.img_found_count;
            let prev_img_cached = self.epub.img_cached_count;
            let prev_pct = self.cache_loading_pct();
            let outcome = self.bg_cache_step_sync(k);
            if self.epub.bg_cache == BgCacheState::Idle {
                if can_paint {
                    ctx.clear_loading();
                    self.cache_ui_stale = false;
                } else {
                    self.cache_ui_stale = true;
                }
            } else if self.cached_chapter_count() != prev_count
                || self.epub.bg_cache != prev_bg
                || self.epub.img_found_count != prev_img_found
                || self.epub.img_cached_count != prev_img_cached
            {
                // with the page visible, every indicator repaint costs a
                // DU refresh, so throttle to 10% steps; the loading
                // screen keeps per-chapter granularity
                if self.shows_loading_screen()
                    || self.cache_loading_pct() / 10 != prev_pct / 10
                {
                    if can_paint {
                        self.set_cache_loading(ctx);
                        self.cache_ui_stale = false;
                    } else {
                        self.cache_ui_stale = true;
                    }
                }
            }
            return outcome;
        }

        BgOutcome::Idle
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        if self.state == State::ShowToc {
            match event {
                ActionEvent::Press(Action::Back) | ActionEvent::Press(Action::Menu) => {
                    self.close_contents(ctx);
                }
                ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                    self.contents_move(1, ctx);
                }
                ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                    self.contents_move(-1, ctx);
                }
                // right grows the sheet to the top margin, left shrinks
                // it back; chapter jumps make no sense inside the list
                ActionEvent::Press(Action::NextJump) => {
                    if !self.toc_expanded {
                        self.toc_expanded = true;
                        self.toc_scroll_into_view();
                        ctx.mark_dirty(PAGE_REGION);
                    }
                }
                ActionEvent::Press(Action::PrevJump) => {
                    if self.toc_expanded {
                        self.toc_expanded = false;
                        self.toc_scroll_into_view();
                        ctx.mark_dirty(PAGE_REGION);
                    }
                }
                ActionEvent::Press(Action::Select) => {
                    let entry = &self.epub.toc.as_ref().unwrap().entries[self.epub.toc_selected];
                    if entry.spine_idx != 0xFFFF {
                        log::debug!(
                            "toc: jumping to \"{}\" -> spine {}",
                            entry.title_str(),
                            entry.spine_idx
                        );
                        self.epub.chapter = entry.spine_idx;
                        self.pg.page = 0;
                        // TOC jump: update RECENT but don't count as page turn
                        self.queue_position_change(PendingPositionChange::Jump);
                        self.goto_last_page = false;
                        self.state = State::NeedIndex;
                        // the sheet stays up until the chapter is ready
                        self.begin_loading(LoadingReason::Chapter);
                    } else {
                        log::warn!(
                            "toc: entry \"{}\" unresolved (spine_idx=0xFFFF), ignoring",
                            entry.title_str()
                        );
                        self.close_contents(ctx);
                    }
                }
                _ => {}
            }
            return Transition::None;
        }

        match event {
            ActionEvent::Press(Action::Back) => Transition::Pop,
            ActionEvent::LongPress(Action::Back) => Transition::Home,

            ActionEvent::LongPress(Action::Next) => {
                if self.state == State::Ready {
                    self.show_position = true;
                }
                if self.page_forward() {
                    self.begin_loading(LoadingReason::Chapter);
                }
                Transition::None
            }
            ActionEvent::LongPress(Action::Prev) => {
                if self.state == State::Ready {
                    self.show_position = true;
                }
                if self.page_backward() {
                    self.begin_loading(LoadingReason::Chapter);
                }
                Transition::None
            }

            ActionEvent::Release(Action::Next) | ActionEvent::Release(Action::Prev) => {
                if self.show_position {
                    self.show_position = false;
                    ctx.mark_dirty(POSITION_OVERLAY);
                }
                Transition::None
            }

            ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                if self.page_forward() {
                    self.begin_loading(LoadingReason::Chapter);
                }
                Transition::None
            }

            ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                if self.page_backward() {
                    self.begin_loading(LoadingReason::Chapter);
                }
                Transition::None
            }

            ActionEvent::Press(Action::NextJump) | ActionEvent::Repeat(Action::NextJump) => {
                if self.jump_forward() {
                    self.begin_loading(LoadingReason::Chapter);
                }
                Transition::None
            }

            ActionEvent::Press(Action::PrevJump) | ActionEvent::Repeat(Action::PrevJump) => {
                if self.jump_backward() {
                    self.begin_loading(LoadingReason::Chapter);
                }
                Transition::None
            }

            // LongPress(NextJump): jump to end of current chapter
            ActionEvent::LongPress(Action::NextJump) => {
                if self.state == State::Ready && self.pg.total_pages > 0 {
                    self.pg.page = self.pg.total_pages - 1;
                    // jump: update RECENT but don't count as page turn
                    self.commit_position_change(PendingPositionChange::Jump);
                    ctx.mark_dirty(PAGE_REGION);
                }
                Transition::None
            }

            // LongPress(PrevJump): jump to start of current chapter
            ActionEvent::LongPress(Action::PrevJump) => {
                if self.state == State::Ready {
                    self.pg.page = 0;
                    // jump: update RECENT but don't count as page turn
                    self.commit_position_change(PendingPositionChange::Jump);
                    ctx.mark_dirty(PAGE_REGION);
                }
                Transition::None
            }

            // LongPress(Select): reserved for bookmark toggle (Phase 6)
            // ActionEvent::LongPress(Action::Select) => { ... }
            _ => Transition::None,
        }
    }

    fn quick_actions(&self) -> &[QuickAction] {
        &self.qa_buf[..self.qa_count as usize]
    }

    fn on_quick_trigger(&mut self, id: u8, ctx: &mut AppContext) -> Transition {
        match id {
            QA_PREV_CHAPTER => {
                if self.is_epub && self.epub.chapter > 0 {
                    self.epub.chapter -= 1;
                    // jump: update RECENT but don't count as page turn
                    self.queue_position_change(PendingPositionChange::Jump);
                    self.goto_last_page = false;
                    self.state = State::NeedIndex;
                    self.begin_loading(LoadingReason::Chapter);
                }
            }
            QA_NEXT_CHAPTER => {
                if self.is_epub && (self.epub.chapter as usize + 1) < self.epub.spine.len() {
                    self.epub.chapter += 1;
                    // jump: update RECENT but don't count as page turn
                    self.queue_position_change(PendingPositionChange::Jump);
                    self.goto_last_page = false;
                    self.state = State::NeedIndex;
                    self.begin_loading(LoadingReason::Chapter);
                }
            }
            QA_TOC => self.open_contents(ctx),
            _ => {}
        }
        Transition::None
    }

    // NOTE: sync_quick_menu() calls this on every close, even when nothing
    // changed. the early-return guard here avoids a spurious re-index. if
    // more cycle values are added to the quick menu, the changed-value
    // check should move into sync_quick_menu() itself.
    fn on_quick_cycle_update(&mut self, id: u8, value: u8, ctx: &mut AppContext) {
        if id == QA_FONT_SIZE && value != self.book_font_size_idx {
            self.book_font_size_idx = value;
            self.apply_font_metrics();
            if self.state == State::Ready {
                if self.is_epub && self.epub.chapters_cached {
                    self.state = State::NeedIndex;
                } else {
                    self.state = State::NeedPage;
                }
                // re-typesetting is always slow enough to show, and
                // the menu is closing in the same refresh anyway
                self.begin_loading(LoadingReason::Font);
                self.paint_loading(ctx);
            }
            self.rebuild_quick_actions();
        }
    }

    fn pending_setting(&self) -> Option<PendingSetting> {
        Some(PendingSetting::BookFontSize(self.book_font_size_idx))
    }

    fn captures_menu(&self) -> bool {
        self.state == State::ShowToc
    }

    fn menu_title(&self) -> &str {
        self.display_name()
    }

    // the chapter being read and how far into the book, so the
    // sheet header reads the same over the page as on the home tabs
    fn menu_meta(&self, out: &mut StackFmt<64>) {
        if !matches!(self.state, State::Ready | State::ShowToc) {
            return;
        }
        if self.is_epub && !self.epub.spine.is_empty() {
            let ch = self.epub.chapter;
            let named = self.epub.toc.as_ref().and_then(|t| {
                (0..t.len())
                    .map(|i| &t.entries[i])
                    .find(|e| e.spine_idx == ch && !e.title_str().is_empty())
            });
            match named {
                Some(e) => {
                    let _ = write!(out, "{}", e.title_str());
                }
                None => {
                    let _ = write!(out, "Chapter {} of {}", ch + 1, self.epub.spine.len());
                }
            }
        } else if self.pg.total_pages > 0 {
            let _ = write!(out, "Page {} of {}", self.pg.page + 1, self.pg.total_pages);
        } else {
            return;
        }
        let _ = write!(out, " \u{00B7} {}% read", self.progress_pct());
    }

    fn hide_button_bar(&self) -> bool {
        true
    }

    /// The reader never shows the shared top status bar: its stat
    /// fields change with every page turn, so each turn used to pay a
    /// separate bar refresh (DU + gray pass) right after the page
    /// settled. The "show chrome" setting still controls the reader's
    /// own bottom info bar.
    fn show_top_status(&self) -> bool {
        false
    }

    fn show_tab_bar(&self) -> bool {
        false
    }

    fn save_state(&self, bm: &mut bookmarks::BookmarkCache) {
        self.save_position(bm);
    }

    fn flush_deferred_persistence(
        &mut self,
        k: &mut KernelHandle<'_>,
        reason: DeferredPersistenceReason,
    ) -> crate::error::Result<()> {
        let force = reason.is_forced();
        if force && self.stats_pause_clock() {
            self.stats_dirty = true;
        }

        // drain again here so forced flushes (sleep) stay correct even
        // when background_step did not run this pass; idempotent
        self.drain_day_stats(k);

        let any_dirty = self.recent_dirty || self.stats_dirty;
        if !any_dirty {
            return Ok(());
        }

        // opportunistic flushes respect the debounce deadline
        if !force {
            match self.persist_next_flush_at {
                Some(deadline) if crate::kernel::uptime_secs() < deadline => return Ok(()),
                None => return Ok(()), // no deadline armed = nothing pending
                _ => {}
            }
        }

        let _attempted_recent = self.recent_dirty;
        let _attempted_stats = self.stats_dirty;
        plump_kernel::perf_begin!(_fd_t0);

        let mut first_error = None;

        if self.recent_dirty {
            if let Err(e) = self.write_recent(k) {
                log::warn!("reader: deferred write_recent failed: {}", e);
                first_error.get_or_insert(e);
            }
        }

        if self.stats_dirty {
            if let Err(e) = self.record_flush(k) {
                log::warn!("reader: deferred record_flush failed: {}", e);
                first_error.get_or_insert(e);
            }
        }

        let _ok = first_error.is_none();
        plump_kernel::perf_event!(
            "reader",
            "flush_deferred reason={} force={} state={:?} recent={} stats={} ok={} elapsed_ms={}",
            reason.as_str(),
            force,
            self.state,
            _attempted_recent,
            _attempted_stats,
            _ok,
            _fd_t0.elapsed().as_millis()
        );

        if let Some(err) = first_error {
            // re-arm debounce for retry
            self.arm_persist_debounce();
            Err(err)
        } else {
            self.persist_next_flush_at = None;
            Ok(())
        }
    }

    fn background_suspended_step(
        &mut self,
        k: &mut KernelHandle<'_>,
        _budget: BgBudget,
    ) -> BgOutcome {
        if self.has_bg_work() {
            self.bg_work_tick(k);
            if self.has_bg_work() {
                BgOutcome::WaitingExternal
            } else {
                BgOutcome::Progress { more: false }
            }
        } else {
            BgOutcome::Idle
        }
    }

    fn draw(&self, strip: &mut StripBuffer) {
        self.draw_page(strip);
        if self.state == State::ShowToc && strip.gray_mode() == GrayMode::Bw {
            self.draw_contents_sheet(strip);
        }
    }

}
