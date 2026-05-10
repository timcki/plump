mod epubs;
mod images;
mod layout;
mod paging;

pub use plump_kernel::util::decode_utf8_char;
use plump_kernel::util::FixedStr;

use crate::apps::PendingSetting;
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
use crate::ui::{Alignment, HEADER_W, ProgressBar, Region, StackFmt};
use smol_epub::cache;
use smol_epub::epub::{self, EpubMeta, EpubSpine, EpubToc, TocSource};
use smol_epub::html_strip::{
    BOLD_OFF, BOLD_ON, H1_OFF, H1_ON, H2_OFF, H2_ON, H3_OFF, H3_ON, H4_OFF, H4_ON, H5_OFF, H5_ON,
    H6_OFF, H6_ON, HEADING_OFF, HEADING_ON, ITALIC_OFF, ITALIC_ON, MARKER, STRIKE_OFF, STRIKE_ON,
    UNDERLINE_OFF, UNDERLINE_ON,
};
use smol_epub::zip::{self, ZipIndex};

// chrome margin: used for bottom info bar, loading indicator.
// this never changes; only the text content area responds to the reading theme.
pub(super) const MARGIN: u16 = 8;

// screen edge padding (display clips pixels at very edge)
pub(super) const SCREEN_PAD: u16 = 4;

// thin bottom progress bar for page-in-chapter progress
const BOTTOM_BAR_H: u16 = 7;
const BOTTOM_BAR_BOTTOM_PAD: u16 = 1;
const BOTTOM_BAR_Y: u16 = SCREEN_H - BOTTOM_BAR_H - BOTTOM_BAR_BOTTOM_PAD;

// bottom chrome: book title + chapter counter, kept above the progress bar
pub(super) const CHROME_H: u16 = 18;
pub(super) const CHROME_PAD: u16 = 2;
const CHROME_BAR_GAP: u16 = 3;
pub(super) const CHROME_Y: u16 = BOTTOM_BAR_Y - CHROME_H - CHROME_BAR_GAP;

pub(super) const TEXT_Y: u16 = SCREEN_PAD + 4;

pub(super) const LINE_H: u16 = 20;

pub(super) const CHARS_PER_LINE: usize = 51;

pub(super) const LINES_PER_PAGE: usize = 37;

pub(super) const PAGE_BUF: usize = 8192;

pub(super) const MAX_PAGES: usize = 512;

pub(super) const HEADER_REGION: Region = Region::new(MARGIN, CHROME_Y, HEADER_W, CHROME_H);

const STATUS_X: u16 = MARGIN + HEADER_W + 8;
const STATUS_W: u16 = SCREEN_W - STATUS_X - MARGIN;
pub(super) const STATUS_REGION: Region = Region::new(STATUS_X, CHROME_Y, STATUS_W, CHROME_H);

const BOTTOM_BAR_W: u16 = SCREEN_W - 2 * SCREEN_PAD;
pub(super) const BOTTOM_BAR_REGION: Region =
    Region::new(SCREEN_PAD, BOTTOM_BAR_Y, BOTTOM_BAR_W, BOTTOM_BAR_H);

pub(super) const PAGE_REGION: Region = Region::new(0, 0, SCREEN_W, SCREEN_H);

pub(super) const NO_PREFETCH: usize = usize::MAX;

pub(super) const TEXT_W: u32 = (SCREEN_W - 2 * MARGIN) as u32;

pub(super) const TEXT_AREA_H: u16 = CHROME_Y - CHROME_PAD - TEXT_Y;

pub(super) const EOCD_TAIL: usize = 512;

pub(super) const INDENT_PX: u32 = 24;

// max inline images tracked per page buffer for dimension pre-scan
pub(super) const MAX_IMAGES_PER_PAGE: usize = 8;

// default image height budget (half text area) used when actual
// dimensions are unavailable (e.g. uncached deflated images, or
// during preindex_all_pages where no pre-scan runs)
pub(super) const DEFAULT_IMG_H: u16 = 350;

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
    pub(super) const ALIGN_LEFT: u8 = 1;
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
    //   11 = reserved
    pub(super) const HLEVEL_SHIFT: u8 = 6;
    pub(super) const HLEVEL_MASK: u8 = 0b11 << Self::HLEVEL_SHIFT;
    pub(super) const HLEVEL_H3: u8 = 0 << Self::HLEVEL_SHIFT;
    pub(super) const HLEVEL_H2: u8 = 1 << Self::HLEVEL_SHIFT;
    pub(super) const HLEVEL_H1: u8 = 2 << Self::HLEVEL_SHIFT;

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
            _ => 3,
        }
    }

    #[inline]
    pub(super) fn is_centered(&self) -> bool {
        self.align == Self::ALIGN_CENTER
    }

    #[inline]
    pub(super) fn is_right_aligned(&self) -> bool {
        self.align == Self::ALIGN_RIGHT
    }

    #[inline]
    pub(super) fn is_explicit_align(&self) -> bool {
        self.align != Self::ALIGN_DEFAULT
    }

    /// Pack style + line-ending flags. `end` is one of END_BUFFER, END_HARD, END_SOFT.
    /// `hlevel` is one of HLEVEL_H3, HLEVEL_H2, HLEVEL_H1 (already shifted).
    pub(super) fn pack_flags(
        bold: bool,
        italic: bool,
        heading: bool,
        hlevel: u8,
        end: u8,
    ) -> u8 {
        (bold as u8)
            | ((italic as u8) << 1)
            | ((heading as u8) << 2)
            | end
            | (hlevel & Self::HLEVEL_MASK)
    }
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

    /// Cached justification metrics per line, precomputed after wrapping.
    /// Avoids re-running `measure_line()` on every strip pass during draw.
    pub(super) line_measures: [paging::LineMeasure; LINES_PER_PAGE],

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
            line_measures: [paging::LineMeasure::ZERO; LINES_PER_PAGE],
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
    pub(super) work_gen: u16,

    pub(super) img_cache_ch: u16,
    pub(super) img_cache_offset: u32,
    pub(super) img_scan_wrapped: bool,
    pub(super) skip_large_img: bool,
    pub(super) img_found_count: u16,
    pub(super) img_cached_count: u16,

    pub(super) cache_step: Option<smol_epub::cache::StreamStripStep>,

    pub(super) toc: Option<Box<EpubToc>>,
    pub(super) toc_source: Option<TocSource>,
    pub(super) toc_selected: usize,
    pub(super) toc_scroll: usize,

    // hash of the source filename; used as the bundle identity
    // (bundle path = `_PLUMP/BOOKS/<name_hash>.BIN`)
    pub(super) name_hash: u32,
    // source file size; header mismatch triggers bundle rebuild
    pub(super) archive_size: u32,
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
            chapter_table: [(0u32, 0u32); cache::MAX_CACHE_CHAPTERS],
            chapters_cached: false,
            cache_chapter: 0,
            ch_cached: [false; cache::MAX_CACHE_CHAPTERS],
            ch_cache: Vec::new(),
            bg_cache: BgCacheState::Idle,
            work_gen: 0,
            img_cache_ch: 0,
            img_cache_offset: 0,
            img_scan_wrapped: false,
            skip_large_img: false,
            img_found_count: 0,
            img_cached_count: 0,
            cache_step: None,
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

    #[inline]
    pub(super) fn chapter_size(&self, ch: usize) -> u32 {
        if ch < cache::MAX_CACHE_CHAPTERS {
            self.chapter_table[ch].1
        } else {
            0
        }
    }

    /// Try `f()` once; on failure, drop `ch_cache` to free heap and retry.
    ///
    /// The chapter cache can hold up to 96 KB.  Large DEFLATED cover/inline
    /// JPEGs need ~90 KB for the decoder, so both cannot coexist on the
    /// 172 KB heap.  After a successful retry the cache stays empty — it is
    /// lazily reloaded on the next chapter navigation via `try_cache_chapter`.
    pub(super) fn oom_retry<E: core::fmt::Display, F>(
        &mut self,
        label: &str,
        mut f: F,
    ) -> Result<DecodedImage, E>
    where
        F: FnMut() -> Result<DecodedImage, E>,
    {
        let result = f();
        match result {
            Ok(_) => result,
            Err(e) if !self.ch_cache.is_empty() => {
                log::debug!(
                    "{}: decode failed ({}), releasing {} KB ch_cache and retrying",
                    label,
                    e,
                    self.ch_cache.len() / 1024,
                );
                self.ch_cache = Vec::new();
                f()
            }
            Err(_) => result,
        }
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
    pub(super) restore_offset: Option<u32>,
    pub(super) restore_page_hint: Option<usize>,
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
    pub(super) show_chrome: bool,
    pub(super) text_alignment: u8, // 0 = Left, 1 = Justify

    // pre-scanned image heights for the current page buffer;
    // populated before wrapping so the pager can reserve the exact
    // number of lines each image needs at its natural aspect ratio
    pub(super) img_heights: [u16; MAX_IMAGES_PER_PAGE],
    pub(super) img_height_count: u8,

    pub(super) book_font_size_idx: u8,
    pub(super) applied_font_idx: u8,
    pub(super) reader_font: ReaderFont,
    pub(super) applied_reader_font: ReaderFont,

    pub(super) chrome_font: Option<&'static BitmapFont>,
    pub(super) qa_buf: [QuickAction; QA_MAX],
    pub(super) qa_count: u8,

    // reading statistics (accumulated per book, flushed to SD)
    pub(super) stats: crate::apps::stats::ReadingStats,
    pub(super) stats_last_uptime: u32, // uptime_secs at last page turn / enter
    pub(super) stats_dirty: bool,
    pub(super) stats_clock_running: bool, // false when suspended/exited

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
            restore_offset: None,
            restore_page_hint: None,
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

            fonts: None,
            font_line_h: LINE_H,
            font_ascent: LINE_H,
            max_lines: LINES_PER_PAGE as u8,

            text_margin: MARGIN,
            text_y: TEXT_Y,
            text_w: TEXT_W,
            text_area_h: TEXT_AREA_H,
            reading_theme_idx: 0,
            show_chrome: true,
            text_alignment: 0,

            img_heights: [0u16; MAX_IMAGES_PER_PAGE],
            img_height_count: 0,

            book_font_size_idx: 0,
            applied_font_idx: 0,
            reader_font: ReaderFont::Bookerly,
            applied_reader_font: ReaderFont::Bookerly,

            chrome_font: None,

            qa_buf: [QuickAction::trigger(0, "", ""); QA_MAX],
            qa_count: 0,

            stats: crate::apps::stats::ReadingStats::EMPTY,
            stats_last_uptime: 0,
            stats_dirty: false,
            stats_clock_running: false,

            persist_next_flush_at: None,
        }
    }

    // 0 = XSmall, 1 = Small, 2 = Medium, 3 = Large, 4 = XLarge
    pub fn set_book_font_size(&mut self, idx: u8) {
        self.book_font_size_idx = idx;
        self.apply_font_metrics();
        self.rebuild_quick_actions();
    }

    pub fn set_reader_font(&mut self, font: ReaderFont) {
        self.reader_font = font;
        self.apply_font_metrics();
        self.rebuild_quick_actions();
    }

    pub fn set_reading_theme(&mut self, idx: u8) {
        self.reading_theme_idx = idx;
        self.apply_theme_layout();
        self.apply_font_metrics();
    }

    pub fn set_text_alignment(&mut self, alignment: u8) {
        self.text_alignment = alignment;
    }

    pub fn set_show_chrome(&mut self, show: bool) {
        if self.show_chrome != show {
            self.show_chrome = show;
            self.apply_theme_layout();
            self.apply_font_metrics();
        }
    }

    fn apply_theme_layout(&mut self) {
        let theme = crate::kernel::config::ReadingTheme::from_idx(self.reading_theme_idx);
        self.text_margin = theme.margin_h;
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

    pub fn wants_grayscale(&self) -> bool {
        matches!(self.state, State::Ready | State::ShowToc)
    }

    pub fn shows_loading_screen(&self) -> bool {
        !matches!(self.state, State::Ready | State::ShowToc | State::Error)
    }

    fn shows_rich_loading_screen(&self) -> bool {
        self.shows_loading_screen()
            && matches!(
                self.pending_position_change,
                Some(
                    PendingPositionChange::OpenReady | PendingPositionChange::RestoreReady
                )
            )
    }

    fn loading_visual_region(&self) -> Region {
        Region::new(
            self.text_margin,
            self.text_y,
            self.text_w as u16,
            self.text_area_h,
        )
    }

    fn set_loading_ui(&self, ctx: &mut AppContext, msg: &str, pct: u8) {
        ctx.set_loading(LOADING_REGION, msg, pct);
        if self.shows_loading_screen() {
            ctx.mark_dirty(self.loading_visual_region());
        }
    }

    fn loading_title(&self) -> Option<&str> {
        if self.title_is_real && !self.title.is_empty() {
            Some(self.display_name())
        } else {
            None
        }
    }

    fn loading_visual(&self) -> (&'static str, u8) {
        match self.state {
            State::NeedBookmark => ("Opening book", 0),
            State::NeedInit => ("Reading file", 10),
            State::NeedOpf => ("Reading metadata", 25),
            State::NeedToc => ("Preparing contents", 40),
            State::NeedCache => {
                let cached_ch = self.cached_chapter_count();
                let total_ch = self.epub.spine.len();
                let img_found = self.epub.img_found_count as usize;
                let img_cached = self.epub.img_cached_count as usize;
                let in_chapter_phase = matches!(
                    self.epub.bg_cache,
                    BgCacheState::CacheChapter | BgCacheState::WaitNearbyImage
                ) && cached_ch < total_ch;

                if in_chapter_phase {
                    let pct = if total_ch > 0 {
                        55 + ((cached_ch * 25) / total_ch).min(25) as u8
                    } else {
                        55
                    };
                    ("Caching chapters", pct)
                } else {
                    let pct = if img_found > 0 {
                        80 + ((img_cached * 20) / img_found).min(20) as u8
                    } else {
                        80
                    };
                    ("Caching images", pct)
                }
            }
            State::NeedIndex => ("Building pages", 75),
            State::NeedPage => ("Opening page", 90),
            State::Ready | State::ShowToc => ("Ready", 100),
            State::Error => ("Error", 0),
        }
    }

    fn draw_loading_screen(&self, strip: &mut StripBuffer) {
        if strip.gray_mode() != GrayMode::Bw {
            return;
        }

        let title = self.loading_title();
        // stage label is chrome — always Inter
        let stage_font = fonts::ui_body_font(1);
        // book title uses the reader font so it matches the body
        let reader_family = self.reader_font.family();
        let (stage, pct) = self.loading_visual();

        let content = self.loading_visual_region();
        let bar_w = content.w.saturating_sub(48).max(160);

        if let Some(img) = self.loading_cover.as_ref() {
            let title_font = title.map(|title| {
                if title.len() > 28 {
                    fonts::heading_font(reader_family, 2)
                } else {
                    fonts::heading_font(reader_family, 3)
                }
            });
            let title_h = title_font.map_or(0, |font| font.line_height);
            let pre_stage_gap = if title_h > 0 { 24 + title_h + 14 } else { 18 };
            let total_h = img
                .height
                .saturating_add(pre_stage_gap)
                .saturating_add(stage_font.line_height)
                .saturating_add(20)
                .saturating_add(10);
            let mut y = content.y + content.h.saturating_sub(total_h) / 2;
            let img_x = content.x + content.w.saturating_sub(img.width) / 2;

            strip.blit_1bpp(
                &img.data,
                0,
                img.width as usize,
                img.height as usize,
                img.stride,
                img_x as i32,
                y as i32,
                true,
            );
            y = y.saturating_add(img.height);

            if let Some(title) = title {
                y = y.saturating_add(24);
                let font = title_font.unwrap();
                let title_region = Region::new(content.x, y, content.w, font.line_height);
                draw_truncated_text(
                    strip,
                    font,
                    title_region,
                    title,
                    Alignment::Center,
                    BinaryColor::On,
                );
                y = y.saturating_add(font.line_height).saturating_add(14);
            } else {
                y = y.saturating_add(18);
            }

            let stage_region = Region::new(content.x, y, content.w, stage_font.line_height);
            let bar_region = Region::new(
                content.x + (content.w.saturating_sub(bar_w)) / 2,
                stage_region.y + stage_region.h + 20,
                bar_w,
                10,
            );

            draw_truncated_text(
                strip,
                stage_font,
                stage_region,
                stage,
                Alignment::Center,
                BinaryColor::On,
            );
            ProgressBar::new(bar_region, pct).draw(strip);
            return;
        }

        if let Some(title) = title {
            let title_font = if title.len() > 28 {
                fonts::heading_font(reader_family, 2)
            } else {
                fonts::heading_font(reader_family, 3)
            };
            let title_y = content.y + content.h / 3;
            let title_region = Region::new(content.x, title_y, content.w, title_font.line_height);
            let stage_region = Region::new(
                content.x,
                title_region.y + title_region.h + 14,
                content.w,
                stage_font.line_height,
            );
            let bar_region = Region::new(
                content.x + (content.w.saturating_sub(bar_w)) / 2,
                stage_region.y + stage_region.h + 20,
                bar_w,
                10,
            );

            draw_truncated_text(
                strip,
                title_font,
                title_region,
                title,
                Alignment::Center,
                BinaryColor::On,
            );
            draw_truncated_text(
                strip,
                stage_font,
                stage_region,
                stage,
                Alignment::Center,
                BinaryColor::On,
            );
            ProgressBar::new(bar_region, pct).draw(strip);
        } else {
            // no title — purely chrome, use Inter
            let heading_font = fonts::ui_heading_font(2);
            let stage_y = content.y + content.h / 3 + 6;
            let stage_region = Region::new(content.x, stage_y, content.w, heading_font.line_height);
            let bar_region = Region::new(
                content.x + (content.w.saturating_sub(bar_w)) / 2,
                stage_region.y + stage_region.h + 22,
                bar_w,
                10,
            );

            draw_truncated_text(
                strip,
                heading_font,
                stage_region,
                stage,
                Alignment::Center,
                BinaryColor::On,
            );
            ProgressBar::new(bar_region, pct).draw(strip);
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

        if let Err(e) = k.sd().save_title(self.filename.as_str(), self.title.as_str()) {
            log::warn!("epub: failed to save title mapping: {}", e);
        }
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

        let pct = if in_chapter_phase {
            let _ = write!(lbuf, "Caching {}/{}", cached_ch, total_ch);
            // chapters: 0% to 80%
            if total_ch > 0 {
                ((cached_ch * 80) / total_ch).min(80) as u8
            } else {
                80
            }
        } else {
            // image phase: 80% to 100%
            if img_found > 0 {
                let _ = write!(lbuf, "Caching images {}/{}", img_cached, img_found);
                (80 + (img_cached * 20) / img_found).min(100) as u8
            } else {
                let _ = write!(lbuf, "Caching images");
                80
            }
        };

        ctx.set_loading(LOADING_REGION, lbuf.as_str(), pct);
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
        if let Some(change) = self.pending_position_change.take() {
            self.commit_position_change(change);
        }
        ctx.clear_loading();
        ctx.mark_dirty(PAGE_REGION);
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
        }
        self.stats_last_uptime = now;
        self.stats.pages = self.stats.pages.saturating_add(1);
        self.stats_dirty = true;
    }

    // load stats from SD for the current book
    fn stats_load(&mut self, k: &mut KernelHandle<'_>) {
        self.stats = crate::apps::stats::ReadingStats::load(k, self.name())
            .unwrap_or(crate::apps::stats::ReadingStats::EMPTY);
        // new session
        self.stats.sessions = self.stats.sessions.saturating_add(1);
        self.stats_dirty = true;
    }

    // flush stats to SD; returns Err on write failure (dirty state kept)
    fn stats_flush(&mut self, k: &mut KernelHandle<'_>) -> crate::error::Result<()> {
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

        self.stats.save(k, self.name())?;
        self.stats_dirty = false;
        plump_kernel::perf_event!(
            "reader",
            "stats_flush pages={} time_s={} elapsed_ms={}",
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
        ctx.mark_dirty(PAGE_REGION);
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

        // contents first (most useful after core items)
        if self.is_epub && self.epub.toc.as_ref().map_or(false, |t| !t.is_empty()) {
            self.qa_buf[n] = QuickAction::trigger(QA_TOC, "Contents", "Open");
            n += 1;
        }

        // font size last
        self.qa_buf[n] = QuickAction::cycle(
            QA_FONT_SIZE,
            "Book Font",
            self.book_font_size_idx,
            fonts::FONT_SIZE_NAMES,
        );
        n += 1;

        self.qa_count = n as u8;
    }

    fn apply_font_metrics(&mut self) {
        self.fonts = None;
        self.font_line_h = LINE_H;
        self.font_ascent = LINE_H;
        self.max_lines = LINES_PER_PAGE as u8;

        let theme = crate::kernel::config::ReadingTheme::from_idx(self.reading_theme_idx);
        let spacing_pct = theme.line_spacing_pct;

        if self.reader_font.family().has_regular() {
            let fs = fonts::FontSet::for_reader(self.reader_font, self.book_font_size_idx);
            let native_h = fs.line_height(fonts::Style::Regular).max(1);
            // apply line spacing: scale native line height by theme percentage
            self.font_line_h = ((native_h as u32 * spacing_pct as u32) / 100).max(1) as u16;
            self.font_ascent = fs.ascent(fonts::Style::Regular);
            self.max_lines =
                ((self.text_area_h / self.font_line_h) as usize).min(LINES_PER_PAGE) as u8;
            log::debug!(
                "font: family={} size_idx={} line_h={} (native {} x {}%) ascent={} max_lines={} margin={}",
                self.reader_font.name(),
                self.book_font_size_idx,
                self.font_line_h,
                native_h,
                spacing_pct,
                self.font_ascent,
                self.max_lines,
                self.text_margin,
            );
            self.fonts = Some(fs);
        }
        self.applied_font_idx = self.book_font_size_idx;
        self.applied_reader_font = self.reader_font;
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
    pub fn restore_state(
        &mut self,
        filename: &[u8],
        is_epub: bool,
        chapter: u16,
        page: usize,
        byte_offset: u32,
        font_size: u8,
    ) {
        self.filename.set(filename);

        // set title from filename initially (will be replaced by
        // epub metadata once the book is loaded)
        self.title.set(self.filename.as_bytes());
        self.title_is_real = false;

        self.is_epub = is_epub;
        self.epub.chapter = chapter;
        self.restore_offset = Some(byte_offset);
        self.restore_page_hint = Some(page);
        self.book_font_size_idx = font_size;

        // reset work queue for clean start
        self.epub.work_gen = work_queue::reset();
        self.epub.bg_cache = BgCacheState::Idle;
        self.epub.ch_cached = [false; smol_epub::cache::MAX_CACHE_CHAPTERS];
        self.epub.img_scan_wrapped = false;
        self.epub.skip_large_img = false;

        // set up reader pipeline — enter at NeedBookmark but with
        // chapter/offset already populated from RTC, so bookmark_load
        // will find our pre-set values and the pipeline proceeds
        self.rebuild_quick_actions();
        self.apply_theme_layout();
        self.reset_paging();
        self.epub.ch_cache = Vec::new();
        self.file_size = 0;
        self.error = None;
        self.show_position = false;
        self.defer_image_decode = true;
        self.goto_last_page = false;
        self.recent_dirty = false;
        self.pending_position_change = Some(PendingPositionChange::RestoreReady);
        self.defer_open_work_once = false;
        self.pending_toc_parse = false;
        self.pending_title_save = false;
        self.pending_cover_thumb = false;
        self.loading_cover = None;
        self.persist_next_flush_at = None;
        self.apply_font_metrics();

        // reading statistics
        self.stats_last_uptime = crate::kernel::uptime_secs();
        self.stats_dirty = false;
        self.stats_clock_running = false;

        // enter state machine — NeedBookmark will check the bookmark
        // cache, but our chapter/offset from RTC are already set, so
        // even if bookmark_load overwrites them with slightly different
        // values, the pipeline proceeds correctly
        self.state = State::NeedBookmark;

        log::debug!(
            "reader: restore_state file={} ch={} off={} font={}",
            self.name(),
            chapter,
            byte_offset,
            font_size
        );
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

    /// Write the current bookmark state into the bundle header. This
    /// runs in addition to save_position (dual write) until the in-RAM
    /// BookmarkCache is retired. No-op when the bundle file doesn't
    /// exist yet (pre-first-chapter-cache).
    pub(super) fn save_bookmark_to_bundle(&self, k: &mut KernelHandle<'_>) {
        if self.state != State::Ready || !self.is_epub {
            return;
        }
        let name_hash = self.epub.name_hash;
        let Some(mut hdr) = plump_kernel::kernel::bundle::read_header(k.sd(), name_hash)
        else {
            return;
        };
        hdr.bm_chapter = self.epub.chapter;
        hdr.bm_page_hint = (self.pg.page as u16).min(u16::MAX);
        hdr.bm_byte_offset = self.pg.offsets[self.pg.page];
        hdr.bm_font_idx = self.book_font_size_idx;
        hdr.bm_flags = plump_kernel::kernel::bundle::BM_FLAG_VALID;
        hdr.set_flag(plump_kernel::kernel::bundle::FLAG_HAS_BOOKMARK, true);
        if let Err(e) = plump_kernel::kernel::bundle::write_header(k.sd(), name_hash, &hdr) {
            log::warn!("reader: bundle bookmark write failed: {}", e);
        }
    }

    fn bookmark_load(&mut self, k: &mut KernelHandle<'_>) -> bool {
        // if restore_offset is already set (from RTC/SD session restore),
        // keep it — the session has the most recent position, while the
        // bookmark cache may be stale (only flushed periodically or on
        // navigation, not on every page turn)
        if self.restore_offset.is_some() {
            log::debug!(
                "bookmark: skipping load, session restore_offset={} ch={} for {}",
                self.restore_offset.unwrap_or(0),
                self.epub.chapter,
                self.name(),
            );
            return true;
        }

        // prefer the bundle header if it has a valid bookmark: written
        // on every save_position, it's at least as current as BKMK.BIN
        // (which is flushed periodically). falls back to the RAM cache
        // when the bundle doesn't yet exist (first-ever open) or has
        // no bookmark recorded.
        if self.is_epub {
            if let Some(hdr) =
                plump_kernel::kernel::bundle::read_header(k.sd(), self.epub.name_hash)
            {
                if hdr.has_valid_bookmark() && hdr.source_size == self.epub.archive_size {
                    log::debug!(
                        "bookmark: restoring from bundle off={} ch={} for {}",
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

        if let Some(slot) = k.bookmark_cache().find(self.filename.as_bytes()) {
            log::debug!(
                "bookmark: restoring from BKMK.BIN off={} ch={} for {}",
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

        self.loading_cover =
            crate::apps::cover_cache::load_cover_for(k, self.filename.as_bytes());

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

    fn chapter_page_bar_fill_width(&self) -> Option<u16> {
        if !self.is_epub || self.pg.total_pages == 0 {
            return None;
        }

        let page = (self.pg.page + 1).min(self.pg.total_pages);
        let filled = ((BOTTOM_BAR_REGION.w as usize * page) / self.pg.total_pages)
            .max(1)
            .min(BOTTOM_BAR_REGION.w as usize);
        Some(filled as u16)
    }

    // write extended RECENT file: filename\0title\0author\0progress
    // returns Err on write failure (dirty state kept for retry)
    fn write_recent(&mut self, k: &mut KernelHandle<'_>) -> crate::error::Result<()> {
        plump_kernel::perf_begin!(_wr_t0);
        let mut buf = [0u8; 196];
        let mut pos = 0usize;

        // filename
        let fl = self.filename.len();
        buf[pos..pos + fl].copy_from_slice(self.filename.as_bytes());
        pos += fl;
        buf[pos] = 0;
        pos += 1;

        // title
        let tl = self.title.len();
        buf[pos..pos + tl].copy_from_slice(self.title.as_bytes());
        pos += tl;
        buf[pos] = 0;
        pos += 1;

        // author
        let al = if self.is_epub {
            (self.epub.meta.author_len as usize).min(64)
        } else {
            0
        };
        if al > 0 {
            buf[pos..pos + al].copy_from_slice(&self.epub.meta.author[..al]);
        }
        pos += al;
        buf[pos] = 0;
        pos += 1;

        // progress percentage as a single byte
        buf[pos] = self.progress_pct();
        pos += 1;

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

fn draw_bottom_fill_bar(strip: &mut StripBuffer, region: Region, filled_w: u16) {
    region
        .to_rect()
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
        .draw(strip)
        .unwrap();

    if filled_w == 0 {
        return;
    }

    Rectangle::new(
        Point::new(region.x as i32, region.y as i32),
        Size::new(filled_w as u32, region.h as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
    .draw(strip)
    .unwrap();
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

impl App<AppId> for ReaderApp {
    fn on_enter(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        let msg = ctx.message();
        self.filename.set(msg);

        self.title.set(self.filename.as_bytes());
        self.title_is_real = false;

        // Bump to a new work-queue generation and drain stale work
        // from any previous book (covers the case where on_enter is
        // called without a preceding on_exit, e.g. Replace transition).
        self.epub.work_gen = work_queue::reset();
        self.epub.bg_cache = BgCacheState::Idle;
        self.epub.ch_cached = [false; cache::MAX_CACHE_CHAPTERS];
        self.epub.img_scan_wrapped = false;
        self.epub.skip_large_img = false;

        self.is_epub = epub::is_epub_filename(self.name());
        self.rebuild_quick_actions();
        self.apply_theme_layout();
        self.reset_paging();
        self.epub.ch_cache = Vec::new();
        self.file_size = 0;
        self.epub.chapter = 0;
        self.error = None;
        self.show_position = false;
        self.defer_image_decode = true;
        self.goto_last_page = false;
        self.restore_offset = None;
        self.restore_page_hint = None;
        self.recent_dirty = false;
        self.pending_position_change = Some(PendingPositionChange::OpenReady);
        self.defer_open_work_once = false;
        self.pending_toc_parse = false;
        self.pending_title_save = false;
        self.pending_cover_thumb = false;
        self.loading_cover = None;
        self.persist_next_flush_at = None;

        self.apply_font_metrics();

        let _ = self.try_load_cached_cover_thumb(k);

        // load existing stats for this book
        self.stats_last_uptime = crate::kernel::uptime_secs();
        self.stats_dirty = false;
        self.stats_clock_running = false;

        self.state = State::NeedBookmark;

        log::info!("reader: opening {}", self.name());

        self.set_loading_ui(ctx, "Opening", 0);
        ctx.mark_dirty(PAGE_REGION);
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
        if self.epub.work_gen != 0 {
            work_queue::set_active_generation(self.epub.work_gen);
        }

        // re-derive text area geometry from the (possibly changed) theme
        self.apply_theme_layout();

        let font_changed = self.book_font_size_idx != self.applied_font_idx
            || self.reader_font != self.applied_reader_font;
        self.apply_font_metrics();
        if font_changed {
            self.reset_paging();
            // invalidate any persisted layout: the saved breaks are
            // keyed to the old font and would wrap differently now.
            // the new key carries the live text_w / line_h / max_lines
            // so subsequent saves match.
            if self.is_epub {
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
        }
        ctx.mark_dirty(PAGE_REGION);
    }

    fn background_step(
        &mut self,
        ctx: &mut AppContext,
        k: &mut KernelHandle<'_>,
        _budget: BgBudget,
    ) -> BgOutcome {
        // Phase 1: Open pipeline (NeedBookmark..NeedPage)
        // Each state does ONE step and returns Progress { more: true }
        match self.state {
            State::NeedBookmark => {
                plump_kernel::perf_begin!(_t0);
                self.bookmark_load(k);
                self.stats_load(k);
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
                    self.set_loading_ui(ctx, "Loading", 10);
                } else {
                    self.state = State::NeedPage;
                    self.set_loading_ui(ctx, "Loading", 50);
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
                        self.set_loading_ui(ctx, "Loading", 25);
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
                        self.set_loading_ui(ctx, "Loading", 40);
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
                self.pending_toc_parse = self.epub.toc_source.is_some();
                self.state = State::NeedCache;
                self.set_loading_ui(ctx, "Caching", 55);
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
                        self.set_loading_ui(ctx, "Indexing", 75);
                        plump_kernel::perf_event!(
                            "reader",
                            "NeedCache hit=true to=NeedIndex elapsed_ms={}",
                            _t0.elapsed().as_millis()
                        );
                        return BgOutcome::Progress { more: true };
                    }
                    Ok(false) => {
                        // cache miss: start/continue caching the current chapter
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
                                self.set_loading_ui(ctx, "Indexing", 75);
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

                let want_last = self.goto_last_page;
                self.goto_last_page = false;

                self.epub_index_chapter();

                if self.is_epub {
                    // try_cache_chapter is best-effort: it can return false
                    // (oversized chapter, OOM). preindex_all_pages must run
                    // either way so it can clear the prior chapter's K-P
                    // state (otherwise has_kp_layout() stays true with
                    // stale data) and fall back to bundle-streamed greedy
                    // when ch_cache is empty.
                    self.epub.try_cache_chapter(k);
                    self.preindex_all_pages(k);
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
                    self.set_loading_ui(ctx, "Loading page", 90);
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
            if !ctx.loading_active() {
                self.set_cache_loading(ctx);
            }
            let prev_count = self.cached_chapter_count();
            let prev_bg = self.epub.bg_cache;
            let prev_img_found = self.epub.img_found_count;
            let prev_img_cached = self.epub.img_cached_count;
            let outcome = self.bg_cache_step_sync(k);
            if self.epub.bg_cache == BgCacheState::Idle {
                ctx.clear_loading();
            } else if self.cached_chapter_count() != prev_count
                || self.epub.bg_cache != prev_bg
                || self.epub.img_found_count != prev_img_found
                || self.epub.img_cached_count != prev_img_cached
            {
                self.set_cache_loading(ctx);
            }
            return outcome;
        }

        BgOutcome::Idle
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        if self.state == State::ShowToc {
            match event {
                ActionEvent::Press(Action::Back) => {
                    self.state = State::Ready;
                    ctx.mark_dirty(PAGE_REGION);
                    return Transition::None;
                }
                ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                    let len = self.epub.toc.as_ref().map_or(0, |t| t.len());
                    if len > 0 {
                        if self.epub.toc_selected + 1 < len {
                            self.epub.toc_selected += 1;
                        } else {
                            self.epub.toc_selected = 0;
                            self.epub.toc_scroll = 0;
                        }
                        let vis = (self.text_area_h / self.font_line_h) as usize;
                        if self.epub.toc_selected >= self.epub.toc_scroll + vis {
                            self.epub.toc_scroll = self.epub.toc_selected + 1 - vis;
                        }
                        ctx.mark_dirty(PAGE_REGION);
                    }
                    return Transition::None;
                }
                ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                    let len = self.epub.toc.as_ref().map_or(0, |t| t.len());
                    if len > 0 {
                        if self.epub.toc_selected > 0 {
                            self.epub.toc_selected -= 1;
                        } else {
                            self.epub.toc_selected = len - 1;
                            let vis = (self.text_area_h / self.font_line_h) as usize;
                            if self.epub.toc_selected >= vis {
                                self.epub.toc_scroll = self.epub.toc_selected + 1 - vis;
                            }
                        }
                        if self.epub.toc_selected < self.epub.toc_scroll {
                            self.epub.toc_scroll = self.epub.toc_selected;
                        }
                        ctx.mark_dirty(PAGE_REGION);
                    }
                    return Transition::None;
                }
                ActionEvent::Press(Action::Select) | ActionEvent::Press(Action::NextJump) => {
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
                        ctx.mark_dirty(PAGE_REGION);
                    } else {
                        log::warn!(
                            "toc: entry \"{}\" unresolved (spine_idx=0xFFFF), ignoring",
                            entry.title_str()
                        );
                        self.state = State::Ready;
                        ctx.mark_dirty(PAGE_REGION);
                    }
                    return Transition::None;
                }
                _ => return Transition::None,
            }
        }

        match event {
            ActionEvent::Press(Action::Back) => Transition::Pop,
            ActionEvent::LongPress(Action::Back) => Transition::Home,

            ActionEvent::LongPress(Action::Next) => {
                if self.state == State::Ready {
                    self.show_position = true;
                }
                if self.page_forward() {
                    ctx.mark_dirty(PAGE_REGION);
                }
                Transition::None
            }
            ActionEvent::LongPress(Action::Prev) => {
                if self.state == State::Ready {
                    self.show_position = true;
                }
                if self.page_backward() {
                    ctx.mark_dirty(PAGE_REGION);
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
                    ctx.mark_dirty(PAGE_REGION);
                }
                Transition::None
            }

            ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                if self.page_backward() {
                    ctx.mark_dirty(PAGE_REGION);
                }
                Transition::None
            }

            ActionEvent::Press(Action::NextJump) | ActionEvent::Repeat(Action::NextJump) => {
                if self.jump_forward() {
                    ctx.mark_dirty(PAGE_REGION);
                }
                Transition::None
            }

            ActionEvent::Press(Action::PrevJump) | ActionEvent::Repeat(Action::PrevJump) => {
                if self.jump_backward() {
                    ctx.mark_dirty(PAGE_REGION);
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

    fn on_quick_trigger(&mut self, id: u8, ctx: &mut AppContext) {
        match id {
            QA_PREV_CHAPTER => {
                if self.is_epub && self.epub.chapter > 0 {
                    self.epub.chapter -= 1;
                    // jump: update RECENT but don't count as page turn
                    self.queue_position_change(PendingPositionChange::Jump);
                    self.goto_last_page = false;
                    self.state = State::NeedIndex;
                }
            }
            QA_NEXT_CHAPTER => {
                if self.is_epub && (self.epub.chapter as usize + 1) < self.epub.spine.len() {
                    self.epub.chapter += 1;
                    // jump: update RECENT but don't count as page turn
                    self.queue_position_change(PendingPositionChange::Jump);
                    self.goto_last_page = false;
                    self.state = State::NeedIndex;
                }
            }
            QA_TOC => {
                if self.is_epub && self.epub.toc.as_ref().map_or(false, |t| !t.is_empty()) {
                    let toc = self.epub.toc.as_ref().unwrap();
                    log::debug!("toc: opening ({} entries)", toc.len());
                    self.epub.toc_selected = 0;
                    self.epub.toc_scroll = 0;
                    for i in 0..toc.len() {
                        if toc.entries[i].spine_idx == self.epub.chapter {
                            self.epub.toc_selected = i;
                            let vis = (self.text_area_h / self.font_line_h) as usize;
                            if self.epub.toc_selected >= vis {
                                self.epub.toc_scroll = self.epub.toc_selected + 1 - vis;
                            }
                            break;
                        }
                    }
                    self.state = State::ShowToc;
                    ctx.mark_dirty(PAGE_REGION);
                }
            }
            _ => {}
        }
    }

    // NOTE: sync_quick_menu() calls this on every close, even when nothing
    // changed. the early-return guard here avoids a spurious re-index. if
    // more cycle values are added to the quick menu, the changed-value
    // check should move into sync_quick_menu() itself.
    fn on_quick_cycle_update(&mut self, id: u8, value: u8, _ctx: &mut AppContext) {
        if id == QA_FONT_SIZE && value != self.book_font_size_idx {
            self.book_font_size_idx = value;
            self.apply_font_metrics();
            if self.state == State::Ready {
                if self.is_epub && self.epub.chapters_cached {
                    self.state = State::NeedIndex;
                } else {
                    self.state = State::NeedPage;
                }
            }
            self.rebuild_quick_actions();
        }
    }

    fn pending_setting(&self) -> Option<PendingSetting> {
        Some(PendingSetting::BookFontSize(self.book_font_size_idx))
    }

    fn hide_button_bar(&self) -> bool {
        true
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
            // dual-write the bookmark into the bundle header so bundle
            // data stays current; the BookmarkCache still flushes on
            // its own cadence via kernel housekeeping
            self.save_bookmark_to_bundle(k);
        }

        if self.stats_dirty {
            if let Err(e) = self.stats_flush(k) {
                log::warn!("reader: deferred stats_flush failed: {}", e);
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
        let cf = self.chrome_font;
        let gray_pass = strip.gray_mode() != GrayMode::Bw;

        if self.show_chrome && !gray_pass && matches!(self.state, State::Ready | State::ShowToc) {
            draw_chrome_text(
                strip,
                HEADER_REGION,
                self.display_name(),
                Alignment::CenterLeft,
                cf,
            );

            if self.state == State::ShowToc {
                draw_chrome_text(strip, STATUS_REGION, "Contents", Alignment::CenterRight, cf);
            } else if self.is_epub && !self.epub.spine.is_empty() {
                let mut sbuf = StackFmt::<40>::new();
                let mut has_status = false;
                if self.epub.spine.len() > 1 {
                    let _ = write!(sbuf, "{}/{}", self.epub.chapter + 1, self.epub.spine.len());
                    has_status = true;
                }
                if self.epub.bg_cache != BgCacheState::Idle {
                    if has_status {
                        let _ = write!(sbuf, " ");
                    }
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
                draw_chrome_text(
                    strip,
                    STATUS_REGION,
                    sbuf.as_str(),
                    Alignment::CenterRight,
                    cf,
                );
                if let Some(filled_w) = self.chapter_page_bar_fill_width() {
                    draw_bottom_fill_bar(strip, BOTTOM_BAR_REGION, filled_w);
                }
            } else if self.file_size > 0 {
                let mut sbuf = StackFmt::<24>::new();
                if self.pg.fully_indexed {
                    let _ = write!(sbuf, "{}/{}", self.pg.page + 1, self.pg.total_pages);
                } else {
                    let _ = write!(sbuf, "p{}", self.pg.page + 1);
                }
                draw_chrome_text(
                    strip,
                    STATUS_REGION,
                    sbuf.as_str(),
                    Alignment::CenterRight,
                    cf,
                );
            }
        }

        if let Some(e) = self.error {
            let mut ebuf = StackFmt::<32>::new();
            let _ = write!(ebuf, "{}", e);
            draw_chrome_text(
                strip,
                LOADING_REGION,
                ebuf.as_str(),
                Alignment::CenterLeft,
                cf,
            );
            return;
        }

        // loading states: fresh opens/restores get the centered
        // cover/title loading screen, while in-reader page turns and
        // chapter jumps stay intentionally blank so they don't flash
        // book metadata between chapters.
        if self.shows_loading_screen() {
            if self.shows_rich_loading_screen() {
                self.draw_loading_screen(strip);
            }
            return;
        }

        if self.state == State::ShowToc {
            let toc_ref = self.epub.toc.as_ref().unwrap();
            let toc_len = toc_ref.len();
            let tx = self.text_margin as i32;
            let ty = self.text_y as i32;
            if self.fonts.is_some() {
                // ToC entries are book content, so they follow the reader font
                let font = fonts::body_font(self.reader_font.family(), self.book_font_size_idx);
                let line_h = font.line_height as i32;
                let ascent = font.ascent as i32;
                let vis_max = (self.text_area_h / font.line_height) as usize;
                let visible = vis_max.min(toc_len.saturating_sub(self.epub.toc_scroll));
                for i in 0..visible {
                    let idx = self.epub.toc_scroll + i;
                    let entry = &toc_ref.entries[idx];
                    let y_top = ty + i as i32 * line_h;
                    let baseline = y_top + ascent;
                    let selected = idx == self.epub.toc_selected;

                    if selected {
                        Rectangle::new(
                            Point::new(0, y_top),
                            Size::new(SCREEN_W as u32, line_h as u32),
                        )
                        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                        .draw(strip)
                        .unwrap();
                    }

                    let fg = if selected {
                        BinaryColor::Off
                    } else {
                        BinaryColor::On
                    };
                    let mut cx = tx;
                    if entry.spine_idx != 0xFFFF && entry.spine_idx == self.epub.chapter {
                        cx += font.draw_char_fg(strip, '>', fg, cx, baseline) as i32;
                        cx += font.draw_char_fg(strip, ' ', fg, cx, baseline) as i32;
                    }
                    font.draw_str_fg(strip, entry.title_str(), fg, cx, baseline);
                }
            } else {
                let style = MonoTextStyle::new(&FONT_9X18, BinaryColor::On);
                let vis_max = (self.text_area_h / LINE_H) as usize;
                let visible = vis_max.min(toc_len.saturating_sub(self.epub.toc_scroll));
                for i in 0..visible {
                    let idx = self.epub.toc_scroll + i;
                    let entry = &toc_ref.entries[idx];
                    let y = ty + i as i32 * LINE_H as i32 + LINE_H as i32;
                    let marker = if idx == self.epub.toc_selected {
                        "> "
                    } else {
                        "  "
                    };
                    Text::new(marker, Point::new(0, y), style)
                        .draw(strip)
                        .unwrap();
                    Text::new(entry.title_str(), Point::new(tx, y), style)
                        .draw(strip)
                        .unwrap();
                }
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
                let mut img_rendered = false;
                for i in 0..self.pg.line_count {
                    let span = &self.pg.lines[i];

                    if span.is_image() {
                        if span.is_image_origin() && !img_rendered {
                            let y_top = self.text_y as i32 + i as i32 * line_h;
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
                                    (self.text_area_h as i32 - i as i32 * line_h).max(0);
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
                                // image LineSpan stores alt_len in `indent`;
                                // alt bytes sit at buf[start - alt_len..start].
                                let alt_len = span.indent as usize;
                                let alt_origin = if alt_len > 0
                                    && (span.start as usize) >= alt_len
                                {
                                    Some(span.start as usize - alt_len)
                                } else {
                                    None
                                };
                                let alt: &[u8] = match alt_origin {
                                    Some(s) => &self.pg.buf[s..span.start as usize],
                                    None => b"[image]",
                                };
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

                    let start = span.start as usize;
                    let end = start + span.len as usize;
                    let baseline = self.text_y as i32 + i as i32 * line_h + ascent;
                    let x_indent = INDENT_PX as i32 * span.indent as i32;

                    let line = &self.pg.buf[start..end];

                    // alignment offset: shift cursor for ALIGN_CENTER / RIGHT
                    // by (avail - measured_width) / N. ALIGN_LEFT and DEFAULT
                    // stay at the indent. headings without an ALIGN marker
                    // (typically h3-tier) also keep the default left position.
                    let align_offset: i32 = if span.is_explicit_align() {
                        let m = self.pg.line_measures[i];
                        let avail = self.text_w.saturating_sub(INDENT_PX * span.indent as u32);
                        let spare = avail.saturating_sub(m.width) as i32;
                        if span.is_centered() {
                            spare / 2
                        } else if span.is_right_aligned() {
                            spare
                        } else {
                            0
                        }
                    } else {
                        0
                    };
                    let mut cx = self.text_margin as i32 + x_indent + align_offset;

                    // justification: distribute spare (signed) across inter-word
                    // gaps. stretch and shrink are decided independently:
                    //
                    // - stretch is aesthetic (justify-vs-left preference) — only
                    //   fires when the user picked justified text AND this is a
                    //   mid-paragraph soft-wrap.
                    // - shrink is layout-driven — fires whenever K-P signalled
                    //   it via `extra`, regardless of user alignment preference
                    //   and regardless of paragraph-end status. without this,
                    //   single-line shrink-fit paragraphs overflow the column
                    //   (Stories of Your Life's dense prose, Leviathan ch5's
                    //   "Using the Knight..." paragraph).
                    //
                    // headings and explicit-align lines skip both directions.
                    let is_heading = (span.flags & LineSpan::FLAG_HEADING) != 0;
                    let explicit = span.is_explicit_align();
                    let can_stretch = self.text_alignment == 1
                        && span.is_soft_wrap()
                        && !is_heading
                        && !explicit;
                    let can_shrink = span.extra_is_shrink() && !is_heading && !explicit;
                    let (extra_per_gap, remainder) = if can_stretch || can_shrink {
                        let m = self.pg.line_measures[i];
                        let avail = self
                            .text_w
                            .saturating_sub(INDENT_PX * span.indent as u32)
                            as i32;
                        let spare = avail - m.width as i32;
                        let gaps = m.gaps as i32;
                        let space_w = fs.advance(' ', span.style()) as i32;

                        if gaps < 2 {
                            (0, 0)
                        } else if can_stretch && spare >= 3 && spare * 5 < avail * 2 {
                            // stretch: distribute positive spare across gaps,
                            // capped at 3× natural space width per gap so a
                            // sparse line doesn't open rivers.
                            let per = spare / gaps;
                            if per <= space_w.saturating_mul(3) {
                                (per, spare - per * gaps)
                            } else {
                                (0, 0)
                            }
                        } else if can_shrink && spare <= -1 {
                            // shrink: K-P decided this paragraph fits by
                            // squeezing inter-word spaces. honor it up to K-P's
                            // own per-glue shrink budget (space/2, matching
                            // `Item::glue`'s shrink in items.rs and
                            // `RATIO_SHRINK_MAX` in breaker.rs).
                            let per = spare / gaps; // signed; rounds toward 0
                            let floor = -(space_w / 2).max(1);
                            if per >= floor {
                                (per, spare - per * gaps)
                            } else {
                                // K-P / renderer disagree on widths beyond the
                                // shrink budget — draw at natural width rather
                                // than crush letters together.
                                (0, 0)
                            }
                        } else {
                            (0, 0)
                        }
                    } else {
                        (0, 0)
                    };
                    let mut gap_idx: i32 = 0;

                    // Track style by accumulated flags (matches K-P's
                    // `fonts::Style::from_flags` resolver) so nested
                    // markup (e.g. `<b><i>x</i></b>`) draws under the
                    // same style K-P measured. The line's initial
                    // flags come from `LineSpan::style()`'s underlying
                    // bits.
                    let mut in_bold = span.flags & LineSpan::FLAG_BOLD != 0;
                    let mut in_italic = span.flags & LineSpan::FLAG_ITALIC != 0;
                    let mut in_heading = span.flags & LineSpan::FLAG_HEADING != 0;
                    let mut hlevel: u8 = if in_heading { span.start_hlevel() } else { 0 };
                    let mut sty =
                        fonts::Style::from_flags(in_bold, in_italic, in_heading, hlevel);

                    // underline / strikethrough are rendered as 1-px horizontal
                    // strokes drawn after a run ends; each *_x_start records cx
                    // when the marker turned the decoration on. y offsets:
                    //   underline = baseline + 1 (just below glyphs)
                    //   strike    = baseline - ascent / 3 (rough mid-line)
                    let mut underline_active = false;
                    let mut strike_active = false;
                    let mut underline_x_start: i32 = 0;
                    let mut strike_x_start: i32 = 0;
                    let strike_y = baseline - ascent / 3;

                    let mut j = 0usize;
                    while j < line.len() {
                        let b = line[j];
                        if b == MARKER && j + 1 < line.len() {
                            match line[j + 1] {
                                BOLD_ON => in_bold = true,
                                BOLD_OFF => in_bold = false,
                                ITALIC_ON => in_italic = true,
                                ITALIC_OFF => in_italic = false,
                                HEADING_ON => {
                                    in_heading = true;
                                    if hlevel == 0 {
                                        hlevel = 3;
                                    }
                                }
                                HEADING_OFF => {
                                    in_heading = false;
                                    hlevel = 0;
                                }
                                H1_ON => {
                                    in_heading = true;
                                    hlevel = 1;
                                }
                                H2_ON => {
                                    in_heading = true;
                                    hlevel = 2;
                                }
                                H3_ON => {
                                    in_heading = true;
                                    hlevel = 3;
                                }
                                H4_ON => {
                                    in_heading = true;
                                    hlevel = 4;
                                }
                                H5_ON => {
                                    in_heading = true;
                                    hlevel = 5;
                                }
                                H6_ON => {
                                    in_heading = true;
                                    hlevel = 6;
                                }
                                H1_OFF | H2_OFF | H3_OFF | H4_OFF | H5_OFF | H6_OFF => {
                                    in_heading = false;
                                    hlevel = 0;
                                }
                                UNDERLINE_ON => {
                                    if !underline_active {
                                        underline_active = true;
                                        underline_x_start = cx;
                                    }
                                }
                                UNDERLINE_OFF => {
                                    if underline_active && cx > underline_x_start {
                                        Rectangle::new(
                                            Point::new(underline_x_start, baseline + 1),
                                            Size::new((cx - underline_x_start) as u32, 1),
                                        )
                                        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                                        .draw(strip)
                                        .ok();
                                    }
                                    underline_active = false;
                                }
                                STRIKE_ON => {
                                    if !strike_active {
                                        strike_active = true;
                                        strike_x_start = cx;
                                    }
                                }
                                STRIKE_OFF => {
                                    if strike_active && cx > strike_x_start {
                                        Rectangle::new(
                                            Point::new(strike_x_start, strike_y),
                                            Size::new((cx - strike_x_start) as u32, 1),
                                        )
                                        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                                        .draw(strip)
                                        .ok();
                                    }
                                    strike_active = false;
                                }
                                _ => {}
                            }
                            sty =
                                fonts::Style::from_flags(in_bold, in_italic, in_heading, hlevel);
                            j += 2;
                            continue;
                        }
                        if b >= 0xC0 {
                            let (ch, seq_len) = decode_utf8_char(line, j);
                            // SHY (U+00AD): K-P models soft hyphens as
                            // zero-width Penalty items (items.rs:280)
                            // so the draw path must also contribute
                            // zero advance. fonts ship SHY as a visible
                            // hyphen glyph with positive advance, which
                            // would overflow every line carrying soft
                            // hyphens and draw spurious mid-word marks.
                            // build.rs excludes SHY from the font tables
                            // for the same reason; this skip is the
                            // matching renderer-side policy.
                            if ch == '\u{00AD}' {
                                j += seq_len;
                                continue;
                            }
                            cx += fs.draw_char(strip, ch, sty, cx, baseline) as i32;
                            // justify: add extra space (or shrink, when
                            // extra_per_gap is negative). NBSP is intentionally
                            // skipped — it's not a stretchable gap.
                            if ch == ' ' && (extra_per_gap != 0 || remainder != 0) {
                                cx += extra_per_gap;
                                if remainder > 0 && gap_idx < remainder {
                                    cx += 1;
                                } else if remainder < 0 && gap_idx < -remainder {
                                    cx -= 1;
                                }
                                gap_idx += 1;
                            }
                            j += seq_len;
                            continue;
                        }
                        if b >= 0x80 {
                            // continuation byte mid-stream (already consumed
                            // by a lead byte above, or stray), skip
                            j += 1;
                            continue;
                        }
                        if b < bitmap::FIRST_CHAR {
                            j += 1;
                            continue; // control char
                        }
                        cx += fs.draw_char(strip, b as char, sty, cx, baseline) as i32;
                        // justify: distribute extra (signed) at ASCII space gaps
                        if b == b' ' && (extra_per_gap != 0 || remainder != 0) {
                            cx += extra_per_gap;
                            if remainder > 0 && gap_idx < remainder {
                                cx += 1;
                            } else if remainder < 0 && gap_idx < -remainder {
                                cx -= 1;
                            }
                            gap_idx += 1;
                        }
                        j += 1;
                    }

                    // flush any underline / strike that extends to end of line
                    // (no closing marker arrived before the buffer ran out)
                    if underline_active && cx > underline_x_start {
                        Rectangle::new(
                            Point::new(underline_x_start, baseline + 1),
                            Size::new((cx - underline_x_start) as u32, 1),
                        )
                        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                        .draw(strip)
                        .ok();
                    }
                    if strike_active && cx > strike_x_start {
                        Rectangle::new(
                            Point::new(strike_x_start, strike_y),
                            Size::new((cx - strike_x_start) as u32, 1),
                        )
                        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                        .draw(strip)
                        .ok();
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
}
