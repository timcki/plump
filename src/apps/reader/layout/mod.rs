//! Reader layout: paragraph-level (Knuth-Plass) line breaking with
//! a chapter-level cached page/line index.
//!
//! Module map:
//!   - byte-layout records live in `plump_kernel::kernel::bundle`
//!     (`LayoutIdxHeader`, `ChapterLayoutDir`, `PageRecord`,
//!     `LineRecord`); this module owns the in-RAM mirrors.
//!   - `smol_epub::markup::Events` is the tokenizer over the stripped
//!     chapter byte stream (shared with the renderer)
//!   - `items`: converts the event stream into K-P items
//!     (paragraph-scoped)
//!   - `breaker`: bounded-DP K-P paragraph breaker
//!   - `paginate`: page builder + `convert::*` adapters that pack
//!     break choices into `LineLayout`
//!   - `pipeline`: `LayoutPipeline` RAII: drives events -> items
//!     -> breaker -> paginate per paragraph
//!   - `cache`: PIDX v2 load / save / invalidate
//!
//! `paging.rs::preindex_all_pages` is the reader-side entry point;
//! it tries the PIDX cache first, runs the K-P pipeline on miss,
//! and falls back to a greedy first-fit only when the breaker
//! rejects an input outright.

// `LineLayout::EMPTY`, several scanner accessors, and a few
// fitness/flag accessors are exercised by host-test infrastructure
// that doesn't compile on the device target — silence dead-code
// warnings here rather than peppering individual `#[allow]`s.
#![allow(dead_code)]

pub mod cache;

pub mod items;
pub mod breaker;
pub mod paginate;
pub mod pipeline;

use plump_kernel::kernel::bundle;
use smol_epub::markup::{Align, Style};

/// Pixels of first-line indent for `qem` quarter-em at the given em
/// size. Shared by the K-P item builder and the renderer so the box the
/// breaker measured is exactly what the page shows.
#[inline]
pub fn indent_px(qem: u8, em_px: u16) -> u16 {
    ((qem as u32 * em_px as u32) / 4) as u16
}

/// Vertical space in quarter-lines for a gap of `qem` quarter-em: the
/// paginator counts page fill in these and the renderer positions lines
/// from them, so both round the same way. Capped at two lines.
#[inline]
pub fn gap_quarters(qem: u8, em_px: u16, line_h: u16) -> u16 {
    let lh = line_h.max(1) as u32;
    let px4 = qem as u32 * em_px as u32; // gap in px, times four
    (((px4 + lh / 2) / lh) as u16).min(8)
}

/// hard cap on the number of `LineRecord`s stored per chapter.
/// chapters that would exceed this fall back to greedy wrapping.
pub const MAX_LINES_PER_CHAPTER: usize = 4096;

/// cache key for the persisted layout. the typeset fields (versions,
/// font, family, content format, text width) gate the LINE table:
/// any of them changing moves the break points, so the cache is
/// discarded and the chapter re-typesets. `line_h` / `max_lines`
/// only gate the PAGE table: breaks are spacing-independent, so a
/// spacing or page-capacity change keeps the lines and re-paginates
/// in RAM (`lines_match` gates the header; the loader compares the
/// spacing fields against the per-chapter dir entry). text_alignment
/// is intentionally not keyed at all: line breaks are alignment-
/// independent, so toggling left/justify must not re-typeset.
///
/// `font_family` is the reader font's `to_idx()` value (0=Bookerly,
/// 1=Atkinson). it must be keyed because Atkinson and Bookerly have
/// different advances at the same size, so wrap points differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutKey {
    pub format_version: u8,
    pub algo_version: u8,
    pub font_idx: u8,
    pub font_family: u8,
    pub content_fmt: u8,
    pub text_w: u16,
    pub line_h: u16,
    pub max_lines: u8,
}

impl LayoutKey {
    /// build a key tagged with the current PIDX format and algo
    /// versions. callers fill in the layout dimensions from the
    /// reader's live state.
    pub fn current(
        font_idx: u8,
        font_family: u8,
        content_fmt: u8,
        text_w: u16,
        line_h: u16,
        max_lines: u8,
    ) -> Self {
        Self {
            format_version: bundle::PAGEIDX_FORMAT_VERSION,
            algo_version: bundle::LAYOUT_ALGO_VERSION,
            font_idx,
            font_family,
            content_fmt,
            text_w,
            line_h,
            max_lines,
        }
    }

    /// one number for the whole key, stored with a page hint so the
    /// hint is trusted only under the layout it was counted under
    pub fn hash(&self) -> u32 {
        plump_kernel::util::hash::fnv1a(&[
            self.format_version,
            self.algo_version,
            self.font_idx,
            self.font_family,
            self.content_fmt,
            (self.text_w & 0xff) as u8,
            (self.text_w >> 8) as u8,
            (self.line_h & 0xff) as u8,
            (self.line_h >> 8) as u8,
            self.max_lines,
        ])
    }

    /// true when the on-disk LINE table is reusable: every typeset
    /// input matches. spacing fields are deliberately excluded.
    pub fn lines_match(&self, h: &bundle::LayoutIdxHeader) -> bool {
        h.format_version == self.format_version
            && h.algo_version == self.algo_version
            && h.font_idx == self.font_idx
            && h.font_family == self.font_family
            && h.content_fmt == self.content_fmt
            && h.text_w == self.text_w
    }


    /// build a header with this key's dimensions and the given
    /// `total_pages`. flags default to 0.
    pub fn to_header(&self, total_pages: u32) -> bundle::LayoutIdxHeader {
        bundle::LayoutIdxHeader {
            format_version: self.format_version,
            algo_version: self.algo_version,
            font_idx: self.font_idx,
            content_fmt: self.content_fmt,
            text_w: self.text_w,
            line_h: self.line_h,
            max_lines: self.max_lines,
            flags: 0,
            font_family: self.font_family,
            total_pages,
        }
    }
}

/// in-RAM mirror of `bundle::PageRecord`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageLayout {
    pub first_line: u16,
    pub line_count: u8,
    pub flags: u8,
    pub start_byte: u32,
    pub end_byte: u32,
}

impl PageLayout {
    pub const EMPTY: Self = Self {
        first_line: 0,
        line_count: 0,
        flags: 0,
        start_byte: 0,
        end_byte: 0,
    };

    pub fn to_record(&self) -> bundle::PageRecord {
        bundle::PageRecord {
            first_line: self.first_line,
            line_count: self.line_count,
            flags: self.flags,
            start_byte: self.start_byte,
            end_byte: self.end_byte,
        }
    }

    pub fn from_record(r: &bundle::PageRecord) -> Self {
        Self {
            first_line: r.first_line,
            line_count: r.line_count,
            flags: r.flags,
            start_byte: r.start_byte,
            end_byte: r.end_byte,
        }
    }
}

/// in-RAM mirror of `bundle::LineRecord`.
///
/// `flags` byte layout (defined for algo_version >= 2):
///   bit 0    FLAG_BOLD
///   bit 1    FLAG_ITALIC
///   bit 2    FLAG_HEADING
///   bit 3    FLAG_IMAGE
///   bit 4    FLAG_PAGE_BREAK_BEFORE  (was LineSpan END_HARD slot)
///   bit 5    FLAG_PARAGRAPH_END      (was LineSpan END_SOFT slot)
///   bits 6-7 HLEVEL_MASK             (only meaningful when FLAG_HEADING set)
///
/// `indent` byte (algo_version >= 18): low nibble left indent levels
/// (`INDENT_PX` each), high nibble first-line indent in quarter-em on a
/// paragraph's first line, else 0.
///
/// `align` byte (algo_version >= 18): bits 0-1 alignment, bit 2 the
/// line starts underlined, bit 3 struck through, high nibble the space
/// above in quarter-em on a paragraph's first line, else 0. Image
/// lines keep `indent` 0 and use only the gap.
///
/// `extra` byte layout (algo_version >= 2): per-gap justification spare in px.
///   bit 7    sign (1 = shrink, 0 = stretch)
///   bits 0-6 magnitude in px-per-gap (cap 127)
/// on image ORIGIN lines (algo_version >= 17) `extra` instead holds
/// the reserved block height in 4 px units (0 = unknown); the
/// renderer never justifies image lines, so the slot is free.
///
/// `align` values mirror `LineSpan::ALIGN_*`: 0 = default (honor user setting),
/// 1 = left, 2 = center, 3 = right.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineLayout {
    pub start_byte: u32,
    pub end_byte: u32,
    pub flags: u8,
    pub indent: u8,
    pub align: u8,
    pub extra: u8,
}

impl LineLayout {
    pub const EMPTY: Self = Self {
        start_byte: 0,
        end_byte: 0,
        flags: 0,
        indent: 0,
        align: 0,
        extra: 0,
    };

    // style + structural flags (bits 0-5)
    pub const FLAG_BOLD: u8 = 1 << 0;
    pub const FLAG_ITALIC: u8 = 1 << 1;
    pub const FLAG_HEADING: u8 = 1 << 2;
    pub const FLAG_IMAGE: u8 = 1 << 3;
    pub const FLAG_PAGE_BREAK_BEFORE: u8 = 1 << 4;
    pub const FLAG_PARAGRAPH_END: u8 = 1 << 5;

    // heading level (bits 6-7); only meaningful when FLAG_HEADING is set.
    // values mirror the LineSpan tier ordering so renderer logic ports across.
    pub const HLEVEL_SHIFT: u8 = 6;
    pub const HLEVEL_MASK: u8 = 0b11 << Self::HLEVEL_SHIFT;
    pub const HLEVEL_H3: u8 = 0 << Self::HLEVEL_SHIFT;
    pub const HLEVEL_H2: u8 = 1 << Self::HLEVEL_SHIFT;
    pub const HLEVEL_H1: u8 = 2 << Self::HLEVEL_SHIFT;
    /// h4-h6: rendered in the bold body face, not the heading face
    pub const HLEVEL_H4: u8 = 3 << Self::HLEVEL_SHIFT;

    // align mirrors LineSpan; kept independent so this module compiles
    // without depending on the renderer-side type.
    pub const ALIGN_DEFAULT: u8 = 0;
    pub const ALIGN_LEFT: u8 = 1;
    pub const ALIGN_CENTER: u8 = 2;
    pub const ALIGN_RIGHT: u8 = 3;
    pub const ALIGN_MASK: u8 = 0x03;
    pub const START_UNDERLINE: u8 = 1 << 2;
    pub const START_STRIKE: u8 = 1 << 3;
    pub const GAP_SHIFT: u8 = 4;
    pub const LEFT_MASK: u8 = 0x0F;
    pub const FIRST_INDENT_SHIFT: u8 = 4;
    /// largest quarter-em value a nibble holds (3.75 em)
    pub const NIBBLE_MAX: u8 = 15;

    // extra byte sign bit; magnitude is bits 0..6.
    pub const EXTRA_SIGN_SHRINK: u8 = 1 << 7;
    pub const EXTRA_MAG_MASK: u8 = 0x7F;

    #[inline]
    pub fn is_image(&self) -> bool { self.flags & Self::FLAG_IMAGE != 0 }
    #[inline]
    pub fn is_paragraph_end(&self) -> bool { self.flags & Self::FLAG_PARAGRAPH_END != 0 }
    #[inline]
    pub fn is_page_break_before(&self) -> bool {
        self.flags & Self::FLAG_PAGE_BREAK_BEFORE != 0
    }
    #[inline]
    pub fn left_levels(&self) -> u8 {
        self.indent & Self::LEFT_MASK
    }
    #[inline]
    pub fn first_indent_qem(&self) -> u8 {
        self.indent >> Self::FIRST_INDENT_SHIFT
    }
    #[inline]
    pub fn align(&self) -> u8 {
        self.align & Self::ALIGN_MASK
    }
    #[inline]
    pub fn gap_qem(&self) -> u8 {
        self.align >> Self::GAP_SHIFT
    }
    #[inline]
    pub fn starts_underline(&self) -> bool {
        self.align & Self::START_UNDERLINE != 0
    }
    #[inline]
    pub fn starts_strike(&self) -> bool {
        self.align & Self::START_STRIKE != 0
    }

    /// pack the `indent` byte: left levels and, on a paragraph's first
    /// line, the first-line indent in quarter-em
    #[inline]
    pub fn pack_indent(left: u8, first_qem: u8) -> u8 {
        left.min(Self::LEFT_MASK) | (first_qem.min(Self::NIBBLE_MAX) << Self::FIRST_INDENT_SHIFT)
    }

    /// pack the `align` byte from the alignment, the gap above (first
    /// line only) and the decorations in force at the line's first byte
    #[inline]
    pub fn pack_align(align: Align, gap_qem: u8, style: Style) -> u8 {
        (align as u8 & Self::ALIGN_MASK)
            | if style.underline { Self::START_UNDERLINE } else { 0 }
            | if style.strike { Self::START_STRIKE } else { 0 }
            | (gap_qem.min(Self::NIBBLE_MAX) << Self::GAP_SHIFT)
    }

    /// the style flag bits for a line whose first byte is drawn in
    /// `style`: bold, italic and the heading tier
    pub fn style_flags(style: Style) -> u8 {
        let mut f = 0u8;
        if style.bold {
            f |= Self::FLAG_BOLD;
        }
        if style.italic {
            f |= Self::FLAG_ITALIC;
        }
        if style.heading != 0 {
            f |= Self::FLAG_HEADING;
            f |= match style.heading {
                1 => Self::HLEVEL_H1,
                2 => Self::HLEVEL_H2,
                3 => Self::HLEVEL_H3,
                _ => Self::HLEVEL_H4,
            };
        }
        f
    }

    /// the inline style in force at the line's first byte, as recorded
    /// by the typesetter; the renderer seeds its decoder with it
    pub fn start_style(&self) -> Style {
        let heading = if self.is_heading() {
            match self.hlevel() {
                Self::HLEVEL_H1 => 1,
                Self::HLEVEL_H2 => 2,
                Self::HLEVEL_H3 => 3,
                _ => 4,
            }
        } else {
            0
        };
        Style {
            bold: self.flags & Self::FLAG_BOLD != 0,
            italic: self.flags & Self::FLAG_ITALIC != 0,
            underline: self.starts_underline(),
            strike: self.starts_strike(),
            heading,
        }
    }
    #[inline]
    pub fn is_heading(&self) -> bool { self.flags & Self::FLAG_HEADING != 0 }
    #[inline]
    pub fn hlevel(&self) -> u8 { self.flags & Self::HLEVEL_MASK }

    /// True when the renderer is allowed to distribute extra-byte spare
    /// across inter-word gaps for justification.
    #[inline]
    pub fn may_justify(&self) -> bool {
        !self.is_paragraph_end() && !self.is_heading() && !self.is_image()
    }

    pub fn to_record(&self) -> bundle::LineRecord {
        bundle::LineRecord {
            start_byte: self.start_byte,
            end_byte: self.end_byte,
            flags: self.flags,
            indent: self.indent,
            align: self.align,
            extra: self.extra,
        }
    }

    pub fn from_record(r: &bundle::LineRecord) -> Self {
        Self {
            start_byte: r.start_byte,
            end_byte: r.end_byte,
            flags: r.flags,
            indent: r.indent,
            align: r.align,
            extra: r.extra,
        }
    }
}

/// reject layouts that would overflow the per-chapter caps.
#[inline]
pub fn fits_in_caps(pages: usize, lines: usize) -> bool {
    pages <= super::MAX_PAGES && lines <= MAX_LINES_PER_CHAPTER
}
