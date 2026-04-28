//! Reader layout: paragraph-level (Knuth-Plass) line breaking with
//! a chapter-level cached page/line index.
//!
//! Phase 1 (this commit) lays the groundwork:
//!   - byte-layout records live in `plump_kernel::kernel::bundle`
//!     (`LayoutIdxHeader`, `ChapterLayoutDir`, `PageRecord`,
//!     `LineRecord`).
//!   - this module owns the in-RAM mirrors (`PageLayout`,
//!     `LineLayout`), the cache key (`LayoutKey`), and the on-disk
//!     load/save/invalidate glue (`cache`).
//!   - `scan` / `items` / `breaker` / `paginate` are stubs filled
//!     in by later phases.
//!
//! Until later phases land, `paging.rs` keeps using the greedy
//! `wrap_proportional` wrapper. The cache round-trips through the
//! new format with empty line records, so font-cycle invalidation
//! and warm page-offset reuse continue to work bit-for-bit.

// Phase 1 only consumes `LayoutKey`, `PageLayout`, and the cache
// load/save/invalidate fns. The remaining surface (`LineLayout`,
// `EMPTY` constants, `LoadedChapter::byte_size`) is consumed by
// Phase 2+; suppress dead-code warnings until then.
#![allow(dead_code)]

pub mod cache;

// stubs, populated in later phases. kept as empty modules so that
// future commits can land scanner/breaker code without restructuring
// imports.
pub mod scan;
pub mod items;
pub mod breaker;
pub mod paginate;

use plump_kernel::kernel::bundle;

/// hard cap on the number of `LineRecord`s stored per chapter.
/// chapters that would exceed this fall back to greedy wrapping.
pub const MAX_LINES_PER_CHAPTER: usize = 4096;

/// cache key for the persisted layout. any field changing
/// invalidates the cache and forces a re-layout. text_alignment is
/// intentionally NOT keyed: line breaks are alignment-independent,
/// so toggling left/justify must not re-typeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutKey {
    pub format_version: u8,
    pub algo_version: u8,
    pub font_idx: u8,
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
        content_fmt: u8,
        text_w: u16,
        line_h: u16,
        max_lines: u8,
    ) -> Self {
        Self {
            format_version: bundle::PAGEIDX_FORMAT_VERSION,
            algo_version: bundle::LAYOUT_ALGO_VERSION,
            font_idx,
            content_fmt,
            text_w,
            line_h,
            max_lines,
        }
    }

    /// true when an on-disk header matches every field of this key.
    pub fn matches_header(&self, h: &bundle::LayoutIdxHeader) -> bool {
        h.format_version == self.format_version
            && h.algo_version == self.algo_version
            && h.font_idx == self.font_idx
            && h.content_fmt == self.content_fmt
            && h.text_w == self.text_w
            && h.line_h == self.line_h
            && h.max_lines == self.max_lines
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
