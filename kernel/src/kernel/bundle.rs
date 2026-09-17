// book bundle: self-contained per-book cache file
//
// every book cached by the reader lives in a single file under
//   _PLUMP/BOOKS/<name_hash>.BIN
// holding a fixed 256-byte header plus a series of sections whose
// offsets/sizes are recorded in the header. sections are laid out so
// the page index is last, letting a font-size change rewrite only the
// tail without touching the heavy content/cover/image sections.
//
// physical layout (offsets in header):
//   header    [0 .. 256)
//   covers    [covers_offset  .. covers_offset  + covers_size)
//   spine     [spine_offset   .. spine_offset   + spine_size)
//   content   [content_offset .. content_offset + content_size)
//   pageidx   [pageidx_offset .. pageidx_offset + pageidx_size)  <- tail
//
// byte-layout types have no I/O deps; at the bottom of the file a thin
// io module wraps SdStorage to read/write bundle bytes by name_hash.

use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage::FileReader;
use crate::util::FixedStr;

// ── on-disk layout: directory + filename ───────────────────────────

/// subdirectory under `_PLUMP/` holding per-book bundles
pub const BOOKS_DIR: &str = "BOOKS";

/// compute a bundle filename for a given filename hash: `XXXXXXXX.BIN`
///
/// using the same lowercase-folded FNV-1a of the source filename that
/// the rest of the codebase uses (`smol_epub::cache::fnv1a_icase`); the
/// returned buffer is 12 bytes, valid ASCII, 8.3-safe.
pub fn bundle_file_name(name_hash: u32) -> [u8; 12] {
    let mut n = *b"00000000.BIN";
    for i in 0..8 {
        let nibble = ((name_hash >> (28 - i * 4)) & 0xF) as u8;
        n[i] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'A' + nibble - 10
        };
    }
    n
}

/// view a bundle filename byte buffer as &str
pub fn bundle_file_str(buf: &[u8; 12]) -> &str {
    core::str::from_utf8(buf).unwrap_or("00000000.BIN")
}

// ── bundle header ──────────────────────────────────────────────────

pub const HEADER_SIZE: usize = 256;
pub const HEADER_MAGIC: [u8; 4] = *b"PLMP";
// v3 splits PIDX into a fixed-size `PidxDir` section and a growable
// `PidxData` section; chapter-dir `pages_offset` / `lines_offset` are
// now relative to PidxData. v2 bundles cannot be reinterpreted (the
// covers region in v2 routinely overlapped PidxDir/content) so they
// are deleted and rebuilt on open.
pub const HEADER_VERSION: u16 = 3;

// content stream format inside the content section. evolves independently
// of HEADER_VERSION so future marker additions can invalidate stored bundles
// without forcing a full layout rev.
//   0 = legacy (Phase 1 marker set: BOLD/ITALIC/H1-H6/U/S/QUOTE/IMG_REF)
//   1 = Phase 2 (adds ALIGN_*/PAGE_BREAK/FIGCAPTION; tag-keyed defaults)
//   2 = Phase 3 (extended IMG_REF payload: flags + width + height + alt)
//   3 = Phase 4 (CSS cascade resolved in the stripper; block properties
//       travel as one absolute BLOCK record per paragraph with alignment,
//       left indent, first-line indent and space above; the QUOTE_*,
//       ALIGN_* and FIGCAPTION_* toggles are gone)
pub const CONTENT_FMT_LATEST: u8 = 3;

pub const TITLE_CAP: usize = 80;
pub const AUTHOR_CAP: usize = 40;

// flags bits
pub const FLAG_CORE_READY: u32 = 1 << 0;
pub const FLAG_COVERS_READY: u32 = 1 << 1;
pub const FLAG_PAGEIDX_READY: u32 = 1 << 2;
pub const FLAG_HAS_BOOKMARK: u32 = 1 << 3;
pub const FLAG_IS_EPUB: u32 = 1 << 4;

// the header's field offsets live in the `record!` field list below;
// these two are named because `BundleFile::open` peeks at them before
// it is willing to trust a full decode
const OFF_MAGIC: usize = 0; // 4
const OFF_VERSION: usize = 4; // 2

// bookmark flags bits (inside `bm_flags`)
pub const BM_FLAG_VALID: u8 = 1 << 0;

// ── typed section table ────────────────────────────────────────────
//
// `SectionId` enumerates the named regions inside a bundle file. Each
// section has a fixed allocation order (encoded by `predecessor`) so
// that `BundleFile::allocate` can only place a new section at the
// current file tail, immediately after its predecessor. This makes
// section-range overlap impossible by construction — the bug that
// silently destroyed cover bytes in the v2 layout when the covers
// region happened to land inside the PIDX zero-fill range.

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionId {
    Spine = 0,
    Content = 1,
    PidxDir = 2,
    Covers = 3,
    PidxData = 4,
}

pub const SECTION_COUNT: usize = 5;

impl SectionId {
    /// Allocation order: each section requires its predecessor to
    /// exist before it can be allocated. `Spine` has no predecessor.
    pub const fn predecessor(self) -> Option<SectionId> {
        match self {
            SectionId::Spine => None,
            SectionId::Content => Some(SectionId::Spine),
            SectionId::PidxDir => Some(SectionId::Content),
            SectionId::Covers => Some(SectionId::PidxDir),
            SectionId::PidxData => Some(SectionId::Covers),
        }
    }

    pub const ALL: [SectionId; SECTION_COUNT] = [
        SectionId::Spine,
        SectionId::Content,
        SectionId::PidxDir,
        SectionId::Covers,
        SectionId::PidxData,
    ];

    /// Index into `BundleHeader::sections` and `SECTION_HDR_OFF`.
    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Byte offset of each section's `{offset, size}` pair inside the
/// header, indexed by `SectionId`. The gaps between entries are the
/// reserved words left by removed v1/v2 fields and must stay zero.
const SECTION_HDR_OFF: [usize; SECTION_COUNT] = [
    188, // Spine
    204, // Content
    220, // PidxDir
    180, // Covers
    230, // PidxData
];

/// Half-open byte range `[offset, offset + size)` within a bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionRange {
    pub offset: u32,
    pub size: u32,
}

// a section range is one u32 offset followed by one u32 size, so the
// header's whole section table decodes as an array over SECTION_HDR_OFF
impl crate::util::Field for SectionRange {
    const WIDTH: usize = 8;
    const ZERO: Self = Self::EMPTY;

    #[inline]
    fn read(src: &[u8]) -> Option<Self> {
        Some(Self {
            offset: u32::read(src)?,
            size: u32::read(src.get(4..)?)?,
        })
    }

    #[inline]
    fn write(self, dst: &mut [u8]) {
        self.offset.write(dst);
        self.size.write(&mut dst[4..]);
    }
}

impl SectionRange {
    pub const EMPTY: Self = Self { offset: 0, size: 0 };

    #[inline]
    pub fn end(&self) -> u32 {
        self.offset.saturating_add(self.size)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// True when the two ranges share at least one byte. Two empty
    /// ranges never overlap (an unallocated section occupies no
    /// bytes).
    pub fn overlaps(&self, other: &Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.offset < other.end()
            && other.offset < self.end()
    }
}

/// Errors returned by the typed `BundleFile` API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleError {
    /// `allocate(id)` called when `id` already has a range in the header.
    AlreadyAllocated(SectionId),
    /// `section_mut(id)` / `grow_tail(id)` called when `id` has no range.
    NotAllocated(SectionId),
    /// `allocate(id)` called before `id.predecessor()` was allocated.
    PredecessorMissing { id: SectionId, needs: SectionId },
    /// `grow_tail(id)` called when `id` is not the section at file tail.
    NotAtTail(SectionId),
    /// `Section::write_at` / `read_at` outside the section's bounded size.
    OutOfBounds { section: SectionId, rel: u32, len: u32, size: u32 },
    /// Two recorded sections overlap (detected by `verify_layout`).
    Overlap { a: SectionId, b: SectionId },
    /// The bundle on disk has an older `HEADER_VERSION` than this code
    /// understands; caller should delete and rebuild.
    StaleVersion(u16),
    /// The bundle file is missing or its header could not be decoded.
    MissingOrCorrupt,
    /// An underlying SD I/O call failed.
    Io(crate::error::Error),
}

impl From<crate::error::Error> for BundleError {
    fn from(e: crate::error::Error) -> Self {
        BundleError::Io(e)
    }
}

impl From<BundleError> for crate::error::Error {
    fn from(e: BundleError) -> Self {
        match e {
            BundleError::Io(err) => err,
            BundleError::OutOfBounds {
                section,
                rel,
                len,
                size,
            } => {
                log::warn!(
                    "bundle: out-of-bounds write in {:?}: rel={} len={} size={}",
                    section,
                    rel,
                    len,
                    size,
                );
                crate::error::Error::new(
                    crate::error::ErrorKind::InvalidData,
                    "bundle: section bounds",
                )
            }
            BundleError::Overlap { a, b } => {
                log::warn!("bundle: section overlap {:?} vs {:?}", a, b);
                crate::error::Error::new(
                    crate::error::ErrorKind::InvalidData,
                    "bundle: section overlap",
                )
            }
            BundleError::PredecessorMissing { id, needs } => {
                log::warn!("bundle: {:?} needs predecessor {:?}", id, needs);
                crate::error::Error::new(
                    crate::error::ErrorKind::InvalidData,
                    "bundle: predecessor missing",
                )
            }
            BundleError::AlreadyAllocated(id) => {
                log::warn!("bundle: {:?} already allocated", id);
                crate::error::Error::new(
                    crate::error::ErrorKind::InvalidData,
                    "bundle: already allocated",
                )
            }
            BundleError::NotAllocated(_) => crate::error::Error::new(
                crate::error::ErrorKind::NotFound,
                "bundle: section not allocated",
            ),
            BundleError::NotAtTail(id) => {
                log::warn!("bundle: {:?} not at tail", id);
                crate::error::Error::new(
                    crate::error::ErrorKind::InvalidData,
                    "bundle: section not at tail",
                )
            }
            BundleError::StaleVersion(v) => {
                log::warn!("bundle: stale v{}", v);
                crate::error::Error::new(crate::error::ErrorKind::NotFound, "bundle: stale version")
            }
            BundleError::MissingOrCorrupt => crate::error::Error::new(
                crate::error::ErrorKind::NotFound,
                "bundle: missing or corrupt",
            ),
        }
    }
}

crate::record! {
    /// owned, decoded bundle header
    #[derive(Clone, Copy)]
    pub struct BundleHeader [HEADER_SIZE] {
        source_size: u32 @ 8,
        name_hash:   u32 @ 12,
        flags:       u32 @ 16,
        // 20..24 reserved (was last_open_gen)
        title:  {str TITLE_CAP}  @ (24, 25),
        author: {str AUTHOR_CAP} @ (105, 106),
        chapter_count: u16 @ 146,
        spine_count:   u16 @ 148,
        // 150..152 pad
        bm_chapter:     u16 @ 152,
        bm_page_hint:   u16 @ 154,
        bm_byte_offset: u32 @ 156,
        bm_font_idx:     u8 @ 160,
        bm_flags:        u8 @ 161,
        // 162..180 reserved (was pages_read / time_spent_secs /
        // sessions / progress_pct, plus pad)
        /// section ranges, indexed by `SectionId`
        sections: {arr SectionRange; SECTION_COUNT} @ SECTION_HDR_OFF,
        pidx_font_idx: u8 @ 228,
        content_fmt:   u8 @ 229,
        // 238..256 reserved
    }
    fixed {
        magic:       [u8; 4] @ OFF_MAGIC = HEADER_MAGIC,
        version:         u16 @ OFF_VERSION = HEADER_VERSION,
        header_size:     u16 @ 6 = HEADER_SIZE as u16,
    }
}

impl BundleHeader {
    pub const EMPTY: Self = Self {
        source_size: 0,
        name_hash: 0,
        flags: 0,
        title: FixedStr::EMPTY,
        author: FixedStr::EMPTY,
        chapter_count: 0,
        spine_count: 0,
        bm_chapter: 0,
        bm_page_hint: 0,
        bm_byte_offset: 0,
        bm_font_idx: 0,
        bm_flags: 0,
        sections: [SectionRange::EMPTY; SECTION_COUNT],
        pidx_font_idx: 0,
        content_fmt: CONTENT_FMT_LATEST,
    };

    #[inline]
    pub fn has_flag(&self, bit: u32) -> bool {
        self.flags & bit != 0
    }

    #[inline]
    pub fn set_flag(&mut self, bit: u32, on: bool) {
        if on {
            self.flags |= bit;
        } else {
            self.flags &= !bit;
        }
    }

    #[inline]
    pub fn has_valid_bookmark(&self) -> bool {
        self.bm_flags & BM_FLAG_VALID != 0 && self.has_flag(FLAG_HAS_BOOKMARK)
    }

    /// Return the `SectionRange` recorded in the header for `id`, or
    /// `None` when the section has not been allocated yet (size == 0).
    #[inline]
    pub fn section(&self, id: SectionId) -> Option<SectionRange> {
        let r = self.range(id);
        if r.is_empty() { None } else { Some(r) }
    }

    /// Raw recorded range, empty when unallocated. Use `section` when
    /// "not allocated yet" needs to be distinguished.
    #[inline]
    pub fn range(&self, id: SectionId) -> SectionRange {
        self.sections[id.index()]
    }

    /// Set the `SectionRange` for `id` in the header (does not write
    /// to disk; caller must invoke `write_header` separately).
    #[inline]
    pub fn set_section(&mut self, id: SectionId, range: SectionRange) {
        self.sections[id.index()] = range;
    }

    /// Walk every allocated section and assert pairwise non-overlap.
    /// Catches misordered section placement (the v2 covers/PidxDir
    /// overlap bug) before any write is committed.
    pub fn verify_layout(&self) -> Result<(), BundleError> {
        for i in 0..SECTION_COUNT {
            let a_id = SectionId::ALL[i];
            let Some(a) = self.section(a_id) else { continue };
            for b_id in &SectionId::ALL[i + 1..] {
                let Some(b) = self.section(*b_id) else { continue };
                if a.overlaps(&b) {
                    return Err(BundleError::Overlap { a: a_id, b: *b_id });
                }
            }
        }
        Ok(())
    }
}

// ── covers section ─────────────────────────────────────────────────
//
// layout inside the covers section:
//   [CoversHeader]  8 bytes
//   [variant[0]..variant[variant_count-1]]  16 bytes each
//   [bitmap data + raw bytes, packed in data_offset order]
//
// offsets stored in the section are all RELATIVE to the section start.

pub const COVERS_HDR_SIZE: usize = 12;
pub const COVER_VARIANT_SIZE: usize = 16;

// raw_format values
pub const RAW_FMT_NONE: u8 = 0;

// cover variant kinds. on-disk byte = discriminant.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverKind {
    Tiny = 0,    // file-browser row icon
    Small = 1,   // grid / picker
    Card = 2,    // home recent card
    Mini = 4,    // home recent-row thumb (64x96)
}

impl CoverKind {
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Tiny),
            1 => Some(Self::Small),
            2 => Some(Self::Card),
            4 => Some(Self::Mini),
            _ => None,
        }
    }
}

// a variant entry carries its kind on disk; an unknown discriminant
// makes the whole entry decode to None so the caller skips it rather
// than silently mistyping it
impl crate::util::Field for CoverKind {
    const WIDTH: usize = 1;
    const ZERO: Self = CoverKind::Tiny;

    #[inline]
    fn read(src: &[u8]) -> Option<Self> {
        Self::from_u8(*src.first()?)
    }

    #[inline]
    fn write(self, dst: &mut [u8]) {
        dst[0] = self.as_u8();
    }
}

crate::record! {
    #[derive(Clone, Copy)]
    pub struct CoversHeader [COVERS_HDR_SIZE] {
        variant_count: u8 @ 0,
        raw_format:    u8 @ 1,
        // 2..4 pad
        /// offset of raw source bytes within the covers section
        raw_offset: u32 @ 4,
        /// size of raw source bytes (0 when absent)
        raw_size: u32 @ 8,
    }
}

crate::record! {
    #[derive(Clone, Copy)]
    pub struct CoverVariant [COVER_VARIANT_SIZE] {
        kind: CoverKind @ 0,
        // 1 pad
        width:  u16 @ 2,
        height: u16 @ 4,
        stride: u16 @ 6,
        /// within the covers section
        data_offset: u32 @ 8,
        data_size:   u32 @ 12,
    }
}

impl CoverVariant {
    pub const EMPTY: Self = Self {
        kind: CoverKind::Tiny,
        width: 0,
        height: 0,
        stride: 0,
        data_offset: 0,
        data_size: 0,
    };
}

// ── spine table ────────────────────────────────────────────────────
//
// one entry per chapter in EPUB spine order. offsets are relative
// to the start of the content section.

pub const SPINE_ENTRY_SIZE: usize = 16;

// spine entry flags
pub const SPINE_FLAG_CACHED: u16 = 1 << 0;

crate::record! {
    #[derive(Clone, Copy)]
    pub struct SpineEntry [SPINE_ENTRY_SIZE] {
        /// absolute byte offset within the bundle file where the
        /// chapter's stripped content begins (not relative to the
        /// content section). keeps reads a single lookup.
        content_offset: u32 @ 0,
        /// number of bytes of chapter content at `content_offset`
        content_size: u32 @ 4,
        /// cached text bytes after HTML stripping (may equal content_size)
        text_bytes: u32 @ 8,
        flags:     u16 @ 12,
        _reserved: u16 @ 14,
    }
}

impl SpineEntry {
    pub const EMPTY: Self = Self {
        content_offset: 0,
        content_size: 0,
        text_bytes: 0,
        flags: 0,
        _reserved: 0,
    };

    #[inline]
    pub fn is_cached(&self) -> bool {
        self.flags & SPINE_FLAG_CACHED != 0
    }
}

// ── layout index section (v2) ──────────────────────────────────────
//
// always the last section of a bundle; rewriting it on font/dimension
// change does not touch anything above.
//
// v2 stores per-page AND per-line records, supporting paragraph-level
// line breaking (Knuth-Plass) cached at the chapter level. v1 stored
// only page start byte offsets. v1 bundles are detected by the format
// version byte at offset 4 and are rebuilt by callers when the loader
// rejects them.
//
// layout inside the section:
//   [LayoutIdxHeader]                          PAGEIDX_HDR_V2_SIZE bytes
//   [ChapterLayoutDir; spine_count]            CHAPTER_LAYOUT_DIR_SIZE each
//   [PageRecord; chapter_A_pages]              packed in dir order
//   [LineRecord; chapter_A_lines]              packed in dir order
//   ...
//
// per-chapter `pages_offset` and `lines_offset` are PIDX-section
// relative. a chapter with `page_count == 0` is treated as not yet
// indexed.

pub const PAGEIDX_MAGIC: [u8; 4] = *b"PIDX";
pub const PAGEIDX_FORMAT_VERSION: u8 = 2;
// algo_version 1 = greedy first-fit; 2 = Knuth-Plass.
// bump only when the runtime typesetter switches; greedy results
// stamped with the K-P version would be served as if they had been
// produced by K-P, which would be wrong.
//
// v3 also bumps for a behaviour change without an algo swap: K-P now
// zeroes the per-gap stretch on paragraph-end / heading lines (lines
// the renderer never justifies). v2 caches stamped these lines with
// stretch saturated to +127 px-per-gap, which mismatched what the
// renderer actually drew and is corrected by re-typesetting.
//
// v4 fixes two K-P regressions caught on Leviathan Wakes ch5:
// 1. `badness` returned 100·r³·256 instead of 100·r³ (missing final
//    Q8 normalisation), so the default tolerance rejected every
//    realistic intermediate breakpoint and K-P collapsed each source
//    paragraph into a single Overflow line.
// 2. Image-line reservation used the raw HTML `attr_h` instead of
//    the decoder's downscaled rendered height, leaving large empty
//    space above and below small inline images.
//
// v5 plumbs JPEG/PNG source-dimension peeks into K-P typesetting so
// images with no HTML width/height attrs (EPUBs that size via CSS,
// e.g. Leviathan Wakes ornaments) reserve the exact rendered height
// instead of falling back to DEFAULT_IMG_H. v4 caches contain over-
// reserved image blocks and must be regenerated.
//
// v6 makes the K-P hint actually return real image heights: it now
// checks the decoded-image cache first (4-byte header, exact pixel
// height) before falling back to a ZIP source-peek, and the source-
// peek path now handles DEFLATE-compressed entries via DeflateReader.
// v5 caches contain the same DEFAULT_IMG_H over-reservation as v4
// for any image whose source peek silently bailed (DEFLATE entries,
// every uncached image on a Calibre-packaged EPUB).
//
// v7 unifies font-style resolution between K-P measurement and the
// renderer (`fonts::Style::from_flags`). v6 caches measured nested
// markup like `<b><i>…</i></b>` as Bold while the renderer drew it
// as Italic, so K-P-chosen wrap points overran the column on any
// line containing such spans (markup-order-sensitive). The new
// resolver also routes h4-h6 to Bold on the K-P side, matching the
// renderer's existing behaviour.
//
// v8 preserves K-P `Adjustment::Shrink` in `LineLayout::extra` on
// paragraph-end and heading lines. v7 zeroed extra for those lines
// via a blanket no-justify guard, so single-line shrink-fit
// paragraphs (the common Stories of Your Life pattern) cached with
// extra=0 and the renderer drew them at natural width past the
// right margin. v8 only suppresses stretch in that guard; shrink
// flows through to the renderer so it can squeeze inter-word
// spacing.
//
// v10 changes the breaker so it stops collapsing paragraphs to a
// single forced-terminal line on narrow columns. Three breaker
// changes: (1) BreakConfig::DEFAULT.tolerance bumped 200 -> 10000
// so loose-but-acceptable interior breaks pass the badness check;
// (2) over-shrunk non-forced interior lines now emit Overflow
// instead of being rejected, so narrow paragraphs always have a
// feasible (if expensive) interior path; (3) the forced shrink-
// clamp boosts badness to BADNESS_INFINITY in the demerits sum so
// K-P stops preferring single-line collapses over multi-line
// alternatives. Pre-v10 caches measured Stories of Your Life
// "Evolution of Human Science" as ~12 single-line paragraphs at
// 2700-6800 px width into a 464 px column.
//
// v11 widens the per-glue elasticity in items.rs from (space/2,
// space/3) to (space, space/2). v10's tighter ratios saturated
// loose-line badness against BADNESS_INFINITY for narrow columns
// + chunky-glyph fonts (Atkinson Small at 464 px), so K-P could
// not distinguish a loose-but-readable line from a true overflow
// line — half the body paragraphs still collapsed because every
// candidate multi-line path looked equally bad demerits-wise.
// v11 gives K-P a meaningful badness gradient so it picks
// multi-line even on the hardest paragraphs. The renderer's
// shrink floor in mod.rs is moved from space/3 to space/2 to
// match.
//
// v12 migrates `MarkupScanner` to a `ByteSource` trait so K-P
// can stream chapters that don't fit in `ch_cache` (the largest
// contiguous heap region on ESP32-C3 is ~108 KB; novella-length
// chapters like Stories of Your Life "Seventy-Two Letters" at
// ~109 KB stripped) via the same in-RAM-buffer-over-SD pattern
// the JPEG decoder uses for big covers. Two impls behind the
// trait: `SliceByteSource` (zero-cost over `&ch_cache`, the
// fast path) and `BundleByteSource` (4 KB sliding window,
// `bundle::read_at` on miss). Layout output should be byte-
// identical to v11 for chapters that fit either path; bumping
// for safety in case rounding or buffer-boundary edge cases
// shift any single break decision.
//
// v16 caps the widow/orphan adjustment at page capacity: demoting
// a line onto an already-full page produced max_lines + 1 pages
// whose last line rendered over the reader footer chrome. cached
// v15 layouts may contain such overfull pages, so they must
// re-typeset.
//
// v17 stores the reserved image height (4 px units) in the origin
// image line's `extra` byte so a line-spacing change can rebuild
// filler counts and re-paginate the cached line table instead of
// re-typesetting. v16 records carry extra=0 there, which v17 reads
// as "height unknown"; bumping so filler data is always present.
// v18 goes with content_fmt 3: LineRecord `indent` splits into left
// levels (low nibble) and first-line indent in quarter-em (high nibble),
// `align` into alignment (bits 0-1), underline / strike line-start bits
// (2-3) and the gap above in quarter-em (high nibble); the first-line
// indent is a K-P box so breaks moved; image-origin lines now start at
// the IMG_REF marker so a page beginning on an image holds its header.
pub const LAYOUT_ALGO_VERSION: u8 = 18;

pub const PAGEIDX_HDR_V2_SIZE: usize = 20;
pub const CHAPTER_LAYOUT_DIR_SIZE: usize = 24;
pub const PAGE_RECORD_SIZE: usize = 12;
pub const LINE_RECORD_SIZE: usize = 12;

// font_family default 0 keeps pre-upgrade caches valid for users who
// stay on Bookerly; switching reader font invalidates the cache because
// wrap points differ between families at the same size.

crate::record! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct LayoutIdxHeader [PAGEIDX_HDR_V2_SIZE] {
        format_version: u8 @ 4,
        algo_version:   u8 @ 5,
        font_idx:       u8 @ 6,
        content_fmt:    u8 @ 7,
        text_w:    u16 @ 8,
        line_h:    u16 @ 10,
        max_lines:  u8 @ 12,
        flags:      u8 @ 13,
        /// reader font family (0 = Bookerly, 1 = Atkinson)
        font_family: u8 @ 14,
        // 15 pad
        total_pages: u32 @ 16,
    }
    fixed {
        magic: [u8; 4] @ 0 = PAGEIDX_MAGIC,
    }
    verify |h: &LayoutIdxHeader| h.format_version == PAGEIDX_FORMAT_VERSION;
}

// pages_line_h / pages_max_lines are PER CHAPTER because the global
// LayoutIdxHeader can only advertise one spacing: after a spacing
// change, chapters typeset later save pages at the new metrics while
// untouched chapters keep pages at the old ones. the loader compares
// against the dir entry, never the header, so stale pages are always
// re-paginated from the (spacing-independent) line table.

crate::record! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ChapterLayoutDir [CHAPTER_LAYOUT_DIR_SIZE] {
        chapter_index: u16 @ 0,
        page_count:    u16 @ 2,
        line_count:    u16 @ 4,
        flags:         u16 @ 6,
        /// within the pageidx section
        pages_offset: u32 @ 8,
        /// within the pageidx section; 0 when no lines
        lines_offset: u32 @ 12,
        /// chapter content byte size at layout time
        byte_size: u32 @ 16,
        /// line_h this chapter's PAGES were built at
        pages_line_h: u16 @ 20,
        /// max_lines ditto
        pages_max_lines: u8 @ 22,
        _reserved:       u8 @ 23,
    }
}

impl ChapterLayoutDir {
    pub const EMPTY: Self = Self {
        chapter_index: 0,
        page_count: 0,
        line_count: 0,
        flags: 0,
        pages_offset: 0,
        lines_offset: 0,
        byte_size: 0,
        pages_line_h: 0,
        pages_max_lines: 0,
        _reserved: 0,
    };
}

crate::record! {
    /// one page of a chapter: which lines it holds and the chapter-
    /// relative byte span it covers
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct PageRecord [PAGE_RECORD_SIZE] {
        /// index into the chapter line table
        first_line: u16 @ 0,
        line_count: u8  @ 2,
        flags:      u8  @ 3,
        start_byte: u32 @ 4,
        end_byte:   u32 @ 8,
    }
}

impl PageRecord {
    pub const EMPTY: Self = Self {
        first_line: 0,
        line_count: 0,
        flags: 0,
        start_byte: 0,
        end_byte: 0,
    };
}

crate::record! {
    /// one laid-out line, chapter-relative
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct LineRecord [LINE_RECORD_SIZE] {
        start_byte: u32 @ 0,
        end_byte:   u32 @ 4,
        /// see LineLayout::FLAG_* in src/apps/reader/layout/mod.rs
        flags:  u8 @ 8,
        /// low nibble: left indent levels; high nibble: first-line indent
        /// in quarter-em (paragraph-first lines only)
        indent: u8 @ 9,
        /// bits 0-1: alignment; bit 2 / 3: line starts underlined /
        /// struck; high nibble: space above in quarter-em (paragraph-
        /// first lines only)
        align:  u8 @ 10,
        /// algo_version >= 2: per-gap stretch/shrink in px; bit 7 = sign
        /// (1 = shrink, 0 = stretch), bits 0-6 = magnitude per gap, cap
        /// 127. algo_version == 1: layout-only, e.g. visible soft-hyphen
        extra: u8 @ 11,
    }
}

impl LineRecord {
    pub const EMPTY: Self = Self {
        start_byte: 0,
        end_byte: 0,
        flags: 0,
        indent: 0,
        align: 0,
        extra: 0,
    };
}

// ── compile-time layout asserts ────────────────────────────────────
//
// each `record!` above proves its own fields are disjoint and land
// inside the record; these pin the sizes the on-disk format promises.

const _: () = {
    assert!(PAGEIDX_HDR_V2_SIZE == 20);
    assert!(CHAPTER_LAYOUT_DIR_SIZE == 24);
    assert!(PAGE_RECORD_SIZE == 12);
    assert!(LINE_RECORD_SIZE == 12);
};

// ── bundle I/O ─────────────────────────────────────────────────────
//
// thin wrappers around `SdStorage::*_in_plump_subdir` that build the
// bundle filename from `name_hash` and hide the BOOKS/ subdir.

/// ensure the BOOKS/ subdir exists; call once at boot
pub fn ensure_books_dir(sd: &SdStorage) -> crate::error::Result<()> {
    sd.ensure_plump_subdir(BOOKS_DIR)
}

/// return whether a bundle file exists for the given name_hash
pub fn exists(sd: &SdStorage, name_hash: u32) -> bool {
    let n = bundle_file_name(name_hash);
    sd.file_size_in_plump_subdir(BOOKS_DIR, bundle_file_str(&n))
        .is_ok()
}

/// size of an existing bundle in bytes
pub fn file_size(sd: &SdStorage, name_hash: u32) -> crate::error::Result<u32> {
    let n = bundle_file_name(name_hash);
    sd.file_size_in_plump_subdir(BOOKS_DIR, bundle_file_str(&n))
}

/// read a chunk at `offset` from the bundle into `buf`
pub fn read_at(
    sd: &SdStorage,
    name_hash: u32,
    offset: u32,
    buf: &mut [u8],
) -> crate::error::Result<usize> {
    let n = bundle_file_name(name_hash);
    sd.read_chunk_in_plump_subdir(BOOKS_DIR, bundle_file_str(&n), offset, buf)
}

/// append bytes to the bundle (creates the file if missing)
pub fn append(sd: &SdStorage, name_hash: u32, data: &[u8]) -> crate::error::Result<()> {
    let n = bundle_file_name(name_hash);
    sd.append_in_plump_subdir(BOOKS_DIR, bundle_file_str(&n), data)
}

/// write bytes at `offset` (creates the file if missing; does NOT
/// truncate trailing bytes past `offset + data.len()`)
pub fn write_at(
    sd: &SdStorage,
    name_hash: u32,
    offset: u32,
    data: &[u8],
) -> crate::error::Result<()> {
    let n = bundle_file_name(name_hash);
    sd.write_at_in_plump_subdir(BOOKS_DIR, bundle_file_str(&n), offset, data)
}

/// delete the bundle file (no-op if already missing)
pub fn delete(sd: &SdStorage, name_hash: u32) -> crate::error::Result<()> {
    let n = bundle_file_name(name_hash);
    match sd.delete_in_plump_subdir(BOOKS_DIR, bundle_file_str(&n)) {
        Ok(()) => Ok(()),
        // treat missing-file as success; other errors propagate
        Err(e) if matches!(e.kind(), crate::error::ErrorKind::OpenFile) => Ok(()),
        Err(e) => Err(e),
    }
}

/// read the 256-byte header and decode it; returns None when the
/// file is missing, too short, or has wrong magic/version
pub fn read_header(sd: &SdStorage, name_hash: u32) -> Option<BundleHeader> {
    let mut buf = [0u8; HEADER_SIZE];
    let nread = read_at(sd, name_hash, 0, &mut buf).ok()?;
    if nread < HEADER_SIZE {
        return None;
    }
    BundleHeader::decode(&buf)
}

/// open the bundle once and run `f` against it. the free `read_at`
/// pays a directory lookup per call; a session pays it once, so use
/// this whenever a caller reads several pieces of one bundle (header,
/// then a section). fails when the bundle is missing.
pub fn with_reader<T>(
    sd: &SdStorage,
    name_hash: u32,
    f: impl FnOnce(&mut FileReader<'_>) -> crate::error::Result<T>,
) -> crate::error::Result<T> {
    let n = bundle_file_name(name_hash);
    sd.with_file_in_plump_subdir(BOOKS_DIR, bundle_file_str(&n), f)
}

/// decode the header of an open bundle; None when short or invalid
pub fn read_header_in(r: &mut FileReader<'_>) -> Option<BundleHeader> {
    let mut buf = [0u8; HEADER_SIZE];
    let nread = r.read_at(0, &mut buf).ok()?;
    if nread < HEADER_SIZE {
        return None;
    }
    BundleHeader::decode(&buf)
}

/// sum the cached per-chapter page counts from the PidxDir section.
/// the on-disk `LayoutIdxHeader.total_pages` field is never stamped
/// with a real value, so the chapter dir entries are the source of
/// truth. returns None when the bundle or its page index is missing
/// or invalid; Some(0) when the index exists but no chapter has been
/// laid out yet. the count reflects whatever font/layout the index
/// was last built for, so treat it as approximate after a reader
/// settings change, and as a lower bound while a book is still being
/// indexed in the background.
pub fn cached_total_pages(sd: &SdStorage, name_hash: u32) -> Option<u32> {
    with_reader(sd, name_hash, |r| {
        Ok(read_header_in(r).and_then(|hdr| total_pages_in(r, &hdr)))
    })
    .ok()
    .flatten()
}

/// `cached_total_pages` for an open bundle whose header is already
/// decoded, so a caller reading the cover in the same session pays no
/// second file open
pub fn total_pages_in(r: &mut FileReader<'_>, hdr: &BundleHeader) -> Option<u32> {
    if !hdr.has_flag(FLAG_PAGEIDX_READY) {
        return None;
    }
    let dir = hdr.section(SectionId::PidxDir)?;
    if dir.size < PAGEIDX_HDR_V2_SIZE as u32 {
        return None;
    }

    // validate the PIDX stamp (magic + format) before trusting the
    // entries; a mismatched key still yields a usable approximation
    let mut hbuf = [0u8; PAGEIDX_HDR_V2_SIZE];
    let n = r.read_at(dir.offset, &mut hbuf).ok()?;
    if n < PAGEIDX_HDR_V2_SIZE {
        return None;
    }
    LayoutIdxHeader::decode(&hbuf)?;

    let avail = (dir.size as usize - PAGEIDX_HDR_V2_SIZE) / CHAPTER_LAYOUT_DIR_SIZE;
    let n_entries = (hdr.spine_count as usize).min(avail);

    // read the chapter dir in small batches to bound the stack buffer
    const BATCH: usize = 20;
    let mut buf = [0u8; BATCH * CHAPTER_LAYOUT_DIR_SIZE];
    let mut total = 0u32;
    let mut i = 0usize;
    while i < n_entries {
        let batch = (n_entries - i).min(BATCH);
        let bytes = batch * CHAPTER_LAYOUT_DIR_SIZE;
        let off = dir.offset + PAGEIDX_HDR_V2_SIZE as u32 + (i * CHAPTER_LAYOUT_DIR_SIZE) as u32;
        let n = r.read_at(off, &mut buf[..bytes]).ok()?;
        if n < bytes {
            return None;
        }
        for j in 0..batch {
            let rec = &buf[j * CHAPTER_LAYOUT_DIR_SIZE..(j + 1) * CHAPTER_LAYOUT_DIR_SIZE];
            if let Some(d) = ChapterLayoutDir::decode(rec) {
                total = total.saturating_add(d.page_count as u32);
            }
        }
        i += batch;
    }
    Some(total)
}

/// write just the 256-byte header to offset 0 (creates the file if
/// missing). use this for frequent header-only updates (bookmark,
/// stats, progress).
pub fn write_header(
    sd: &SdStorage,
    name_hash: u32,
    header: &BundleHeader,
) -> crate::error::Result<()> {
    let bytes = header.encode();
    write_at(sd, name_hash, 0, &bytes)
}

// ── typed BundleFile / Section API ─────────────────────────────────
//
// `BundleFile` is the only sanctioned way to mutate a bundle's section
// layout. It enforces, via `SectionId::predecessor` and
// `BundleHeader::verify_layout`, that:
//
//   * sections are allocated in fixed order from the file tail;
//   * a section's range is recorded in the header before any byte of
//     it is written;
//   * `Section::write_at` is bounded by the recorded `size`, so a
//     mis-computed offset cannot punch into a neighbouring section
//     (the v2 cover-overwrite bug).
//
// Free `write_at` / `read_at` remain available as raw primitives but
// new code should funnel through `BundleFile`.

use core::marker::PhantomData;

/// Decoded view of an existing bundle file, with bounded section
/// access. Created via [`BundleFile::open`] (existing file) or
/// [`BundleFile::create_or_open`] (creates an empty bundle on first use).
pub struct BundleFile<'a> {
    sd: &'a SdStorage,
    name_hash: u32,
    header: BundleHeader,
    file_size: u32,
}

impl<'a> BundleFile<'a> {
    /// Open an existing bundle, decoding and validating its header.
    /// Returns `MissingOrCorrupt` when the file is absent or its
    /// header doesn't decode, `StaleVersion(v)` when the on-disk
    /// version differs from `HEADER_VERSION`, or `Overlap` when the
    /// recorded sections do not pass `verify_layout`.
    pub fn open(sd: &'a SdStorage, name_hash: u32) -> Result<Self, BundleError> {
        let mut buf = [0u8; HEADER_SIZE];
        let nread = read_at(sd, name_hash, 0, &mut buf)?;
        if nread < HEADER_SIZE {
            return Err(BundleError::MissingOrCorrupt);
        }
        // detect stale on-disk version before BundleHeader::decode
        // rejects it on the version mismatch path
        if buf[OFF_MAGIC..OFF_MAGIC + 4] != HEADER_MAGIC {
            return Err(BundleError::MissingOrCorrupt);
        }
        let on_disk_version =
            <u16 as crate::util::Field>::read(&buf[OFF_VERSION..]).unwrap_or(0);
        if on_disk_version != HEADER_VERSION {
            return Err(BundleError::StaleVersion(on_disk_version));
        }
        let header = BundleHeader::decode(&buf).ok_or(BundleError::MissingOrCorrupt)?;
        header.verify_layout()?;
        let file_size = file_size(sd, name_hash)?;
        Ok(Self {
            sd,
            name_hash,
            header,
            file_size,
        })
    }

    #[inline]
    pub fn header(&self) -> &BundleHeader {
        &self.header
    }

    #[inline]
    pub fn header_mut(&mut self) -> &mut BundleHeader {
        &mut self.header
    }

    #[inline]
    pub fn name_hash(&self) -> u32 {
        self.name_hash
    }

    #[inline]
    pub fn file_size(&self) -> u32 {
        self.file_size
    }

    /// Persist the in-memory header back to disk. Validates layout
    /// before writing, refusing to commit an overlapping layout.
    pub fn commit_header(&mut self) -> Result<(), BundleError> {
        self.header.verify_layout()?;
        write_header(self.sd, self.name_hash, &self.header)?;
        Ok(())
    }

    /// Section range, if allocated.
    #[inline]
    pub fn section(&self, id: SectionId) -> Option<SectionRange> {
        self.header.section(id)
    }

    /// Allocate `id` at the current bundle tail with `reserved` zero
    /// bytes pre-written. Updates the in-memory header, calls
    /// `verify_layout`, then commits the header to disk before
    /// returning the bounded handle.
    pub fn allocate(
        &mut self,
        id: SectionId,
        reserved: u32,
    ) -> Result<Section<'_, 'a>, BundleError> {
        if self.header.section(id).is_some() {
            return Err(BundleError::AlreadyAllocated(id));
        }
        if let Some(prev) = id.predecessor() {
            if self.header.section(prev).is_none() {
                return Err(BundleError::PredecessorMissing { id, needs: prev });
            }
        }
        let offset = self.file_size;
        let new_range = SectionRange {
            offset,
            size: reserved,
        };

        // record + validate before any write — this is the first
        // moment we can detect overlap (shouldn't happen with the
        // predecessor invariant, but we belt-and-brace it).
        self.header.set_section(id, new_range);
        if let Err(e) = self.header.verify_layout() {
            self.header.set_section(id, SectionRange::EMPTY);
            return Err(e);
        }
        // zero-fill the reserved bytes so the section is well-defined
        // even before the caller writes its first byte
        zero_fill(self.sd, self.name_hash, offset, reserved)?;
        self.file_size = offset.saturating_add(reserved);
        // commit header after the bytes are on disk so a power-loss
        // mid-allocation leaves the section invisible rather than
        // half-zeroed
        if let Err(e) = self.commit_header() {
            // failed commit: revert the in-memory range so the file
            // and header agree (the zero-filled bytes become garbage
            // but no other section's bounds reference them)
            self.header.set_section(id, SectionRange::EMPTY);
            return Err(e);
        }
        Ok(self.make_section(id, new_range))
    }

    /// Open an already-allocated section for write/read.
    pub fn section_mut(&mut self, id: SectionId) -> Result<Section<'_, 'a>, BundleError> {
        let range = self
            .header
            .section(id)
            .ok_or(BundleError::NotAllocated(id))?;
        Ok(self.make_section(id, range))
    }

    /// Append `extra` bytes to a section whose range ends at the
    /// current file tail. Updates `pidx_data_size` (or whichever
    /// section's size field corresponds to `id`) and commits the
    /// header. Returns a handle to the *full* (grown) section.
    pub fn grow_tail(
        &mut self,
        id: SectionId,
        extra: u32,
    ) -> Result<Section<'_, 'a>, BundleError> {
        let current = self
            .header
            .section(id)
            .ok_or(BundleError::NotAllocated(id))?;
        if current.end() != self.file_size {
            return Err(BundleError::NotAtTail(id));
        }
        let new_size = current.size.saturating_add(extra);
        let new_range = SectionRange {
            offset: current.offset,
            size: new_size,
        };
        // pre-extend the file with zero bytes so partial writes never
        // shrink the recorded range without on-disk backing
        zero_fill(self.sd, self.name_hash, current.end(), extra)?;
        self.file_size = current.end().saturating_add(extra);
        self.header.set_section(id, new_range);
        if let Err(e) = self.commit_header() {
            // header commit failed; revert in-memory size to avoid
            // diverging from the bytes that were just appended
            self.header.set_section(id, current);
            return Err(e);
        }
        Ok(self.make_section(id, new_range))
    }

    fn make_section(&mut self, id: SectionId, range: SectionRange) -> Section<'_, 'a> {
        Section {
            sd: self.sd,
            name_hash: self.name_hash,
            id,
            offset: range.offset,
            size: range.size,
            _h: PhantomData,
        }
    }
}

/// Bounded write/read handle for a single section. Outlives only as
/// long as the borrowing `BundleFile`. All offsets are *relative* to
/// the section's start; `write_at(rel, data)` checks
/// `rel + data.len() <= size` and returns `OutOfBounds` otherwise —
/// it can never silently touch a neighbouring section.
pub struct Section<'h, 'sd: 'h> {
    sd: &'sd SdStorage,
    name_hash: u32,
    id: SectionId,
    offset: u32,
    size: u32,
    _h: PhantomData<&'h mut ()>,
}

impl<'h, 'sd> Section<'h, 'sd> {
    /// Write `data` at `rel` bytes from the start of this section.
    pub fn write_at(&mut self, rel: u32, data: &[u8]) -> Result<(), BundleError> {
        let len = data.len() as u32;
        let end = rel
            .checked_add(len)
            .ok_or(BundleError::OutOfBounds {
                section: self.id,
                rel,
                len,
                size: self.size,
            })?;
        if end > self.size {
            return Err(BundleError::OutOfBounds {
                section: self.id,
                rel,
                len,
                size: self.size,
            });
        }
        write_at(self.sd, self.name_hash, self.offset + rel, data)?;
        Ok(())
    }

    /// Read `buf.len()` bytes starting at `rel`. Returns the number
    /// of bytes actually read.
    pub fn read_at(&self, rel: u32, buf: &mut [u8]) -> Result<usize, BundleError> {
        let len = buf.len() as u32;
        let end = rel
            .checked_add(len)
            .ok_or(BundleError::OutOfBounds {
                section: self.id,
                rel,
                len,
                size: self.size,
            })?;
        if end > self.size {
            return Err(BundleError::OutOfBounds {
                section: self.id,
                rel,
                len,
                size: self.size,
            });
        }
        let n = read_at(self.sd, self.name_hash, self.offset + rel, buf)?;
        Ok(n)
    }

    /// Zero-fill the entire section in place (e.g. to invalidate the
    /// PIDX chapter dir). Bounded by the section's own `size` — by
    /// construction this cannot reach into another section.
    pub fn fill_zero(&mut self) -> Result<(), BundleError> {
        zero_fill(self.sd, self.name_hash, self.offset, self.size).map_err(BundleError::Io)
    }
}

// 256-byte zero block used by `zero_fill` to avoid allocating.
const ZERO_BLOCK: [u8; 256] = [0u8; 256];

fn zero_fill(
    sd: &SdStorage,
    name_hash: u32,
    offset: u32,
    bytes: u32,
) -> crate::error::Result<()> {
    let mut written: u32 = 0;
    while written < bytes {
        let remaining = bytes - written;
        let chunk = remaining.min(ZERO_BLOCK.len() as u32) as usize;
        write_at(sd, name_hash, offset + written, &ZERO_BLOCK[..chunk])?;
        written += chunk as u32;
    }
    Ok(())
}


// ── host-runnable byte-layout tests ──────────────────────────────────
//
// the kernel crate currently can't be host-tested directly because
// esp-hal is a non-optional dependency and only builds for the
// riscv target. these tests still document the expected round-trip
// invariants and become live the moment a host-test target is
// added; on-device they're inert (cfg(test) is set only by `cargo
// test`, never on a normal riscv build).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_idx_header_round_trip() {
        let h = LayoutIdxHeader {
            format_version: PAGEIDX_FORMAT_VERSION,
            algo_version: LAYOUT_ALGO_VERSION,
            font_idx: 3,
            content_fmt: CONTENT_FMT_LATEST,
            text_w: 472,
            line_h: 22,
            max_lines: 37,
            flags: 0,
            total_pages: 0xDEAD_BEEF,
        };
        let bytes = h.encode();
        assert_eq!(&bytes[0..4], &PAGEIDX_MAGIC);
        assert_eq!(bytes[4], PAGEIDX_FORMAT_VERSION);
        let back = LayoutIdxHeader::decode(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn layout_idx_header_rejects_v1() {
        // v1 bytes have format byte where v2 expects format_version=2;
        // a v1 header writes font_idx into byte 4 (could be 0..n_fonts),
        // never 2 in well-formed bundles. simulate a v1 header byte 4=0
        // and verify v2 decoder rejects it.
        let mut buf = [0u8; PAGEIDX_HDR_V2_SIZE];
        buf[0..4].copy_from_slice(&PAGEIDX_MAGIC);
        buf[4] = 0; // not PAGEIDX_FORMAT_VERSION
        assert!(LayoutIdxHeader::decode(&buf).is_none());
    }

    #[test]
    fn chapter_layout_dir_round_trip() {
        let d = ChapterLayoutDir {
            chapter_index: 0x1234,
            page_count: 0x5678,
            line_count: 0x9ABC,
            flags: 0xDEF0,
            pages_offset: 0x1111_2222,
            lines_offset: 0x3333_4444,
            byte_size: 0x5555_6666,
            pages_line_h: 0x7777,
            pages_max_lines: 0x88,
            _reserved: 0x99,
        };
        let bytes = d.encode();
        let back = ChapterLayoutDir::decode(&bytes).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn page_record_round_trip() {
        let p = PageRecord {
            first_line: 0x1234,
            line_count: 37,
            flags: 0xA5,
            start_byte: 0x1111_2222,
            end_byte: 0x3333_4444,
        };
        let bytes = p.encode();
        let back = PageRecord::decode(&bytes).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn line_record_round_trip() {
        let l = LineRecord {
            start_byte: 0x1111_2222,
            end_byte: 0x3333_4444,
            flags: 0x55,
            indent: 0x66,
            align: 0x77,
            extra: 0x88,
        };
        let bytes = l.encode();
        let back = LineRecord::decode(&bytes).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn record_sizes_match_constants() {
        assert_eq!(LayoutIdxHeader::EMPTY_BYTES.len(), PAGEIDX_HDR_V2_SIZE);
        assert_eq!(ChapterLayoutDir::EMPTY.encode().len(), CHAPTER_LAYOUT_DIR_SIZE);
        assert_eq!(PageRecord::EMPTY.encode().len(), PAGE_RECORD_SIZE);
        assert_eq!(LineRecord::EMPTY.encode().len(), LINE_RECORD_SIZE);
    }

    // ── section table / overlap unit tests ──────────────────────────────

    #[test]
    fn section_range_overlaps_truth_table() {
        let a = SectionRange { offset: 10, size: 5 }; // [10, 15)
        let b = SectionRange { offset: 15, size: 5 }; // [15, 20) -- adjacent, no overlap
        let c = SectionRange { offset: 12, size: 5 }; // [12, 17) -- straddles a's tail
        let d = SectionRange { offset: 5, size: 6 }; //  [5, 11)  -- straddles a's head
        let e = SectionRange { offset: 11, size: 2 }; // [11, 13) -- strictly inside a
        let empty = SectionRange::EMPTY;

        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
        assert!(a.overlaps(&c));
        assert!(c.overlaps(&a));
        assert!(a.overlaps(&d));
        assert!(a.overlaps(&e));
        // empty ranges never overlap anything
        assert!(!empty.overlaps(&a));
        assert!(!a.overlaps(&empty));
        assert!(!empty.overlaps(&empty));
    }

    #[test]
    fn section_predecessor_chain() {
        assert_eq!(SectionId::Spine.predecessor(), None);
        assert_eq!(SectionId::Content.predecessor(), Some(SectionId::Spine));
        assert_eq!(SectionId::PidxDir.predecessor(), Some(SectionId::Content));
        assert_eq!(SectionId::Covers.predecessor(), Some(SectionId::PidxDir));
        assert_eq!(SectionId::PidxData.predecessor(), Some(SectionId::Covers));
    }

    #[test]
    fn verify_layout_rejects_covers_inside_pidx_dir() {
        // This is the actual v2 bug we're regressing against: PidxDir
        // [1015153, +1676) and Covers [1016065, +4006). The covers
        // section's start sits inside the dir region; verify_layout
        // must catch this before the header is committed to disk.
        let mut hdr = BundleHeader::EMPTY;
        hdr.set_section(SectionId::Spine, SectionRange { offset: 256, size: 1104 });
        hdr.set_section(SectionId::Content, SectionRange { offset: 1360, size: 1_013_793 });
        hdr.set_section(SectionId::PidxDir, SectionRange { offset: 1_015_153, size: 1676 });
        hdr.set_section(SectionId::Covers, SectionRange { offset: 1_016_065, size: 4006 });

        match hdr.verify_layout() {
            Err(BundleError::Overlap { a, b }) => {
                let pair = if a == SectionId::PidxDir && b == SectionId::Covers
                    || a == SectionId::Covers && b == SectionId::PidxDir
                {
                    true
                } else {
                    false
                };
                assert!(pair, "expected PidxDir/Covers overlap, got {:?} / {:?}", a, b);
            }
            other => panic!("expected Err(Overlap), got {:?}", other),
        }
    }

    #[test]
    fn verify_layout_accepts_contiguous_disjoint_layout() {
        // Spine -> Content -> PidxDir -> Covers -> PidxData, end-to-end.
        let mut hdr = BundleHeader::EMPTY;
        hdr.set_section(SectionId::Spine, SectionRange { offset: 256, size: 1104 });
        hdr.set_section(SectionId::Content, SectionRange { offset: 1360, size: 1_013_793 });
        hdr.set_section(SectionId::PidxDir, SectionRange { offset: 1_015_153, size: 1676 });
        hdr.set_section(SectionId::Covers, SectionRange { offset: 1_016_829, size: 4006 });
        hdr.set_section(SectionId::PidxData, SectionRange { offset: 1_020_835, size: 8000 });

        hdr.verify_layout().expect("contiguous disjoint must pass");
    }

    #[test]
    fn verify_layout_accepts_unallocated_sections() {
        // Only spine recorded; everything else is empty.
        let mut hdr = BundleHeader::EMPTY;
        hdr.set_section(SectionId::Spine, SectionRange { offset: 256, size: 1104 });
        hdr.verify_layout().expect("empty sections never overlap");
        assert_eq!(hdr.section(SectionId::Content), None);
        assert!(hdr.section(SectionId::Spine).is_some());
    }

    #[test]
    fn set_section_round_trips() {
        let mut hdr = BundleHeader::EMPTY;
        let range = SectionRange { offset: 4096, size: 64 };
        hdr.set_section(SectionId::Covers, range);
        assert_eq!(hdr.section(SectionId::Covers), Some(range));
        // setting back to EMPTY drops it
        hdr.set_section(SectionId::Covers, SectionRange::EMPTY);
        assert_eq!(hdr.section(SectionId::Covers), None);
    }

    #[test]
    fn header_encode_decode_preserves_v3_pidx_fields() {
        let mut h = BundleHeader::EMPTY;
        h.set_section(SectionId::Spine, SectionRange { offset: 256, size: 1104 });
        h.set_section(SectionId::Content, SectionRange { offset: 1360, size: 100_000 });
        h.set_section(SectionId::PidxDir, SectionRange { offset: 101_360, size: 1676 });
        h.set_section(SectionId::PidxData, SectionRange { offset: 107_042, size: 8000 });
        h.pidx_font_idx = 5;
        let bytes = h.encode();
        let back = BundleHeader::decode(&bytes).expect("round-trip");
        assert_eq!(
            back.range(SectionId::PidxDir),
            SectionRange { offset: 101_360, size: 1676 }
        );
        assert_eq!(
            back.range(SectionId::PidxData),
            SectionRange { offset: 107_042, size: 8000 }
        );
        assert_eq!(back.pidx_font_idx, 5);
    }

    #[test]
    fn header_decode_rejects_v2_magic_version() {
        // Forge a v2 header (magic ok, version=2). decode rejects it
        // on the version mismatch path so v2 bundles can't be reopened
        // and silently reinterpreted as v3.
        let mut buf = [0u8; HEADER_SIZE];
        buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&HEADER_MAGIC);
        use crate::util::Field;
        2u16.write(&mut buf[OFF_VERSION..]);
        (HEADER_SIZE as u16).write(&mut buf[6..]);
        assert!(BundleHeader::decode(&buf).is_none());
    }

    #[test]
    fn cover_kind_round_trip() {
        for k in [
            CoverKind::Tiny,
            CoverKind::Small,
            CoverKind::Card,
            CoverKind::Mini,
        ] {
            assert_eq!(CoverKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(CoverKind::from_u8(255), None);
    }

    #[test]
    fn cover_variant_decode_skips_unknown_kind() {
        // unknown kind byte -> decode returns None so the caller can
        // skip the variant rather than misinterpret its dimensions.
        let mut buf = [0u8; COVER_VARIANT_SIZE];
        buf[0] = 99;
        assert!(CoverVariant::decode(&buf).is_none());
    }

    #[test]
    fn cover_variant_round_trip() {
        let v = CoverVariant {
            kind: CoverKind::Mini,
            width: 64,
            height: 96,
            stride: 8,
            data_offset: 12 + 32,
            data_size: 768,
        };
        let bytes = v.encode();
        let back = CoverVariant::decode(&bytes).expect("decode");
        assert_eq!(back.kind, CoverKind::Mini);
        assert_eq!(back.width, 64);
        assert_eq!(back.height, 96);
        assert_eq!(back.stride, 8);
        assert_eq!(back.data_offset, 44);
        assert_eq!(back.data_size, 768);
    }
}

#[cfg(test)]
impl LayoutIdxHeader {
    // a zero header for size assertions; not exposed in production code
    const EMPTY_BYTES: [u8; PAGEIDX_HDR_V2_SIZE] = [0u8; PAGEIDX_HDR_V2_SIZE];
}
