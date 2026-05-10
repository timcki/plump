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
//   toc       [toc_offset     .. toc_offset     + toc_size)
//   content   [content_offset .. content_offset + content_size)
//   images    [images_offset  .. images_offset  + images_size)
//   pageidx   [pageidx_offset .. pageidx_offset + pageidx_size)  <- tail
//
// byte-layout types have no I/O deps; at the bottom of the file a thin
// io module wraps SdStorage to read/write bundle bytes by name_hash.

use crate::drivers::sdcard::SdStorage;
use crate::util::FixedStr;

// ── on-disk layout: directory + filename ───────────────────────────

/// subdirectory under `_PLUMP/` holding per-book bundles
pub const BOOKS_DIR: &str = "BOOKS";

/// recent-pointer filename in the data directory
pub const RECENT_FILE: &str = "RECENT";

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
pub const CONTENT_FMT_LATEST: u8 = 2;

pub const TITLE_CAP: usize = 80;
pub const AUTHOR_CAP: usize = 40;

// flags bits
pub const FLAG_CORE_READY: u32 = 1 << 0;
pub const FLAG_COVERS_READY: u32 = 1 << 1;
pub const FLAG_PAGEIDX_READY: u32 = 1 << 2;
pub const FLAG_HAS_BOOKMARK: u32 = 1 << 3;
pub const FLAG_IS_EPUB: u32 = 1 << 4;

// header field offsets (kept explicit so the on-disk layout is auditable)
const OFF_MAGIC: usize = 0; // 4
const OFF_VERSION: usize = 4; // 2
const OFF_HEADER_SIZE: usize = 6; // 2
const OFF_SOURCE_SIZE: usize = 8; // 4
const OFF_NAME_HASH: usize = 12; // 4
const OFF_FLAGS: usize = 16; // 4
const OFF_LAST_OPEN_GEN: usize = 20; // 4

const OFF_TITLE_LEN: usize = 24; // 1
const OFF_TITLE: usize = 25; // 80
const OFF_AUTHOR_LEN: usize = 105; // 1
const OFF_AUTHOR: usize = 106; // 40
const OFF_CHAPTER_COUNT: usize = 146; // 2
const OFF_SPINE_COUNT: usize = 148; // 2
// 150..152 pad

const OFF_BM_CHAPTER: usize = 152; // 2
const OFF_BM_PAGE_HINT: usize = 154; // 2
const OFF_BM_BYTE_OFFSET: usize = 156; // 4
const OFF_BM_FONT_IDX: usize = 160; // 1
const OFF_BM_FLAGS: usize = 161; // 1
// 162..168 pad

const OFF_PAGES_READ: usize = 168; // 4
const OFF_TIME_SPENT_SECS: usize = 172; // 4
const OFF_SESSIONS: usize = 176; // 2
const OFF_PROGRESS_PCT: usize = 178; // 1
// 179 pad

const OFF_COVERS_OFFSET: usize = 180; // 4
const OFF_COVERS_SIZE: usize = 184; // 4
const OFF_SPINE_OFFSET: usize = 188; // 4
const OFF_SPINE_SIZE: usize = 192; // 4
const OFF_TOC_OFFSET: usize = 196; // 4
const OFF_TOC_SIZE: usize = 200; // 4
const OFF_CONTENT_OFFSET: usize = 204; // 4
const OFF_CONTENT_SIZE: usize = 208; // 4
const OFF_IMAGES_OFFSET: usize = 212; // 4
const OFF_IMAGES_SIZE: usize = 216; // 4
const OFF_PIDX_DIR_OFFSET: usize = 220; // 4
const OFF_PIDX_DIR_SIZE: usize = 224; // 4
const OFF_PIDX_FONT_IDX: usize = 228; // 1
const OFF_CONTENT_FMT: usize = 229; // 1
const OFF_PIDX_DATA_OFFSET: usize = 230; // 4
const OFF_PIDX_DATA_SIZE: usize = 234; // 4
// 238..256 reserved

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
}

/// Half-open byte range `[offset, offset + size)` within a bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionRange {
    pub offset: u32,
    pub size: u32,
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

/// owned, decoded bundle header
#[derive(Clone, Copy)]
pub struct BundleHeader {
    pub source_size: u32,
    pub name_hash: u32,
    pub flags: u32,
    pub last_open_gen: u32,

    pub title: FixedStr<TITLE_CAP>,
    pub author: FixedStr<AUTHOR_CAP>,
    pub chapter_count: u16,
    pub spine_count: u16,

    pub bm_chapter: u16,
    pub bm_page_hint: u16,
    pub bm_byte_offset: u32,
    pub bm_font_idx: u8,
    pub bm_flags: u8,

    pub pages_read: u32,
    pub time_spent_secs: u32,
    pub sessions: u16,
    pub progress_pct: u8,

    pub covers_offset: u32,
    pub covers_size: u32,
    pub spine_offset: u32,
    pub spine_size: u32,
    pub toc_offset: u32,
    pub toc_size: u32,
    pub content_offset: u32,
    pub content_size: u32,
    pub images_offset: u32,
    pub images_size: u32,
    pub pidx_dir_offset: u32,
    pub pidx_dir_size: u32,
    pub pidx_data_offset: u32,
    pub pidx_data_size: u32,
    pub pidx_font_idx: u8,
    pub content_fmt: u8,
}

impl BundleHeader {
    pub const EMPTY: Self = Self {
        source_size: 0,
        name_hash: 0,
        flags: 0,
        last_open_gen: 0,
        title: FixedStr::EMPTY,
        author: FixedStr::EMPTY,
        chapter_count: 0,
        spine_count: 0,
        bm_chapter: 0,
        bm_page_hint: 0,
        bm_byte_offset: 0,
        bm_font_idx: 0,
        bm_flags: 0,
        pages_read: 0,
        time_spent_secs: 0,
        sessions: 0,
        progress_pct: 0,
        covers_offset: 0,
        covers_size: 0,
        spine_offset: 0,
        spine_size: 0,
        toc_offset: 0,
        toc_size: 0,
        content_offset: 0,
        content_size: 0,
        images_offset: 0,
        images_size: 0,
        pidx_dir_offset: 0,
        pidx_dir_size: 0,
        pidx_data_offset: 0,
        pidx_data_size: 0,
        pidx_font_idx: 0,
        content_fmt: CONTENT_FMT_LATEST,
    };

    /// decode a 256-byte header. returns None when magic/version/header_size
    /// do not match v1.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_SIZE {
            return None;
        }
        if buf[OFF_MAGIC..OFF_MAGIC + 4] != HEADER_MAGIC {
            return None;
        }
        if r_u16(buf, OFF_VERSION) != HEADER_VERSION {
            return None;
        }
        if r_u16(buf, OFF_HEADER_SIZE) as usize != HEADER_SIZE {
            return None;
        }

        let title = decode_fixed::<TITLE_CAP>(buf, OFF_TITLE_LEN, OFF_TITLE);
        let author = decode_fixed::<AUTHOR_CAP>(buf, OFF_AUTHOR_LEN, OFF_AUTHOR);

        Some(Self {
            source_size: r_u32(buf, OFF_SOURCE_SIZE),
            name_hash: r_u32(buf, OFF_NAME_HASH),
            flags: r_u32(buf, OFF_FLAGS),
            last_open_gen: r_u32(buf, OFF_LAST_OPEN_GEN),
            title,
            author,
            chapter_count: r_u16(buf, OFF_CHAPTER_COUNT),
            spine_count: r_u16(buf, OFF_SPINE_COUNT),
            bm_chapter: r_u16(buf, OFF_BM_CHAPTER),
            bm_page_hint: r_u16(buf, OFF_BM_PAGE_HINT),
            bm_byte_offset: r_u32(buf, OFF_BM_BYTE_OFFSET),
            bm_font_idx: buf[OFF_BM_FONT_IDX],
            bm_flags: buf[OFF_BM_FLAGS],
            pages_read: r_u32(buf, OFF_PAGES_READ),
            time_spent_secs: r_u32(buf, OFF_TIME_SPENT_SECS),
            sessions: r_u16(buf, OFF_SESSIONS),
            progress_pct: buf[OFF_PROGRESS_PCT],
            covers_offset: r_u32(buf, OFF_COVERS_OFFSET),
            covers_size: r_u32(buf, OFF_COVERS_SIZE),
            spine_offset: r_u32(buf, OFF_SPINE_OFFSET),
            spine_size: r_u32(buf, OFF_SPINE_SIZE),
            toc_offset: r_u32(buf, OFF_TOC_OFFSET),
            toc_size: r_u32(buf, OFF_TOC_SIZE),
            content_offset: r_u32(buf, OFF_CONTENT_OFFSET),
            content_size: r_u32(buf, OFF_CONTENT_SIZE),
            images_offset: r_u32(buf, OFF_IMAGES_OFFSET),
            images_size: r_u32(buf, OFF_IMAGES_SIZE),
            pidx_dir_offset: r_u32(buf, OFF_PIDX_DIR_OFFSET),
            pidx_dir_size: r_u32(buf, OFF_PIDX_DIR_SIZE),
            pidx_data_offset: r_u32(buf, OFF_PIDX_DATA_OFFSET),
            pidx_data_size: r_u32(buf, OFF_PIDX_DATA_SIZE),
            pidx_font_idx: buf[OFF_PIDX_FONT_IDX],
            content_fmt: buf[OFF_CONTENT_FMT],
        })
    }

    /// encode the header into a fresh 256-byte buffer
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&HEADER_MAGIC);
        w_u16(&mut out, OFF_VERSION, HEADER_VERSION);
        w_u16(&mut out, OFF_HEADER_SIZE, HEADER_SIZE as u16);
        w_u32(&mut out, OFF_SOURCE_SIZE, self.source_size);
        w_u32(&mut out, OFF_NAME_HASH, self.name_hash);
        w_u32(&mut out, OFF_FLAGS, self.flags);
        w_u32(&mut out, OFF_LAST_OPEN_GEN, self.last_open_gen);

        encode_fixed(&mut out, OFF_TITLE_LEN, OFF_TITLE, TITLE_CAP, &self.title);
        encode_fixed(
            &mut out,
            OFF_AUTHOR_LEN,
            OFF_AUTHOR,
            AUTHOR_CAP,
            &self.author,
        );
        w_u16(&mut out, OFF_CHAPTER_COUNT, self.chapter_count);
        w_u16(&mut out, OFF_SPINE_COUNT, self.spine_count);

        w_u16(&mut out, OFF_BM_CHAPTER, self.bm_chapter);
        w_u16(&mut out, OFF_BM_PAGE_HINT, self.bm_page_hint);
        w_u32(&mut out, OFF_BM_BYTE_OFFSET, self.bm_byte_offset);
        out[OFF_BM_FONT_IDX] = self.bm_font_idx;
        out[OFF_BM_FLAGS] = self.bm_flags;

        w_u32(&mut out, OFF_PAGES_READ, self.pages_read);
        w_u32(&mut out, OFF_TIME_SPENT_SECS, self.time_spent_secs);
        w_u16(&mut out, OFF_SESSIONS, self.sessions);
        out[OFF_PROGRESS_PCT] = self.progress_pct;

        w_u32(&mut out, OFF_COVERS_OFFSET, self.covers_offset);
        w_u32(&mut out, OFF_COVERS_SIZE, self.covers_size);
        w_u32(&mut out, OFF_SPINE_OFFSET, self.spine_offset);
        w_u32(&mut out, OFF_SPINE_SIZE, self.spine_size);
        w_u32(&mut out, OFF_TOC_OFFSET, self.toc_offset);
        w_u32(&mut out, OFF_TOC_SIZE, self.toc_size);
        w_u32(&mut out, OFF_CONTENT_OFFSET, self.content_offset);
        w_u32(&mut out, OFF_CONTENT_SIZE, self.content_size);
        w_u32(&mut out, OFF_IMAGES_OFFSET, self.images_offset);
        w_u32(&mut out, OFF_IMAGES_SIZE, self.images_size);
        w_u32(&mut out, OFF_PIDX_DIR_OFFSET, self.pidx_dir_offset);
        w_u32(&mut out, OFF_PIDX_DIR_SIZE, self.pidx_dir_size);
        w_u32(&mut out, OFF_PIDX_DATA_OFFSET, self.pidx_data_offset);
        w_u32(&mut out, OFF_PIDX_DATA_SIZE, self.pidx_data_size);
        out[OFF_PIDX_FONT_IDX] = self.pidx_font_idx;
        out[OFF_CONTENT_FMT] = self.content_fmt;

        out
    }

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
    pub fn section(&self, id: SectionId) -> Option<SectionRange> {
        let r = match id {
            SectionId::Spine => SectionRange {
                offset: self.spine_offset,
                size: self.spine_size,
            },
            SectionId::Content => SectionRange {
                offset: self.content_offset,
                size: self.content_size,
            },
            SectionId::PidxDir => SectionRange {
                offset: self.pidx_dir_offset,
                size: self.pidx_dir_size,
            },
            SectionId::Covers => SectionRange {
                offset: self.covers_offset,
                size: self.covers_size,
            },
            SectionId::PidxData => SectionRange {
                offset: self.pidx_data_offset,
                size: self.pidx_data_size,
            },
        };
        if r.is_empty() { None } else { Some(r) }
    }

    /// Set the `SectionRange` for `id` in the header (does not write
    /// to disk; caller must invoke `write_header` separately).
    pub fn set_section(&mut self, id: SectionId, range: SectionRange) {
        match id {
            SectionId::Spine => {
                self.spine_offset = range.offset;
                self.spine_size = range.size;
            }
            SectionId::Content => {
                self.content_offset = range.offset;
                self.content_size = range.size;
            }
            SectionId::PidxDir => {
                self.pidx_dir_offset = range.offset;
                self.pidx_dir_size = range.size;
            }
            SectionId::Covers => {
                self.covers_offset = range.offset;
                self.covers_size = range.size;
            }
            SectionId::PidxData => {
                self.pidx_data_offset = range.offset;
                self.pidx_data_size = range.size;
            }
        }
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
pub const RAW_FMT_JPEG: u8 = 1;
pub const RAW_FMT_PNG: u8 = 2;

// cover variant kinds
pub const COVER_KIND_TINY: u8 = 0; // 48x64 file-browser row icon
pub const COVER_KIND_SMALL: u8 = 1; // 120x160 grid/picker
pub const COVER_KIND_CARD: u8 = 2; // 200x260 home recent card
pub const COVER_KIND_DETAIL: u8 = 3; // 320x400 book info screen

#[derive(Clone, Copy)]
pub struct CoversHeader {
    pub variant_count: u8,
    pub raw_format: u8,
    pub raw_offset: u32, // offset of raw source bytes within covers section
    pub raw_size: u32,   // size of raw source bytes (0 when absent)
}

impl CoversHeader {
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < COVERS_HDR_SIZE {
            return None;
        }
        Some(Self {
            variant_count: buf[0],
            raw_format: buf[1],
            // buf[2..4] pad
            raw_offset: r_u32(buf, 4),
            raw_size: r_u32(buf, 8),
        })
    }

    pub fn encode(&self) -> [u8; COVERS_HDR_SIZE] {
        let mut out = [0u8; COVERS_HDR_SIZE];
        out[0] = self.variant_count;
        out[1] = self.raw_format;
        // buf[2..4] pad
        w_u32(&mut out, 4, self.raw_offset);
        w_u32(&mut out, 8, self.raw_size);
        out
    }
}

#[derive(Clone, Copy)]
pub struct CoverVariant {
    pub kind: u8,
    pub width: u16,
    pub height: u16,
    pub stride: u16,
    pub data_offset: u32, // within covers section
    pub data_size: u32,
}

impl CoverVariant {
    pub const EMPTY: Self = Self {
        kind: 0,
        width: 0,
        height: 0,
        stride: 0,
        data_offset: 0,
        data_size: 0,
    };

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < COVER_VARIANT_SIZE {
            return None;
        }
        Some(Self {
            kind: buf[0],
            // buf[1] pad
            width: r_u16(buf, 2),
            height: r_u16(buf, 4),
            stride: r_u16(buf, 6),
            data_offset: r_u32(buf, 8),
            data_size: r_u32(buf, 12),
        })
    }

    pub fn encode(&self) -> [u8; COVER_VARIANT_SIZE] {
        let mut out = [0u8; COVER_VARIANT_SIZE];
        out[0] = self.kind;
        // out[1] pad
        w_u16(&mut out, 2, self.width);
        w_u16(&mut out, 4, self.height);
        w_u16(&mut out, 6, self.stride);
        w_u32(&mut out, 8, self.data_offset);
        w_u32(&mut out, 12, self.data_size);
        out
    }
}

// ── spine table ────────────────────────────────────────────────────
//
// one entry per chapter in EPUB spine order. offsets are relative
// to the start of the content section.

pub const SPINE_ENTRY_SIZE: usize = 16;

// spine entry flags
pub const SPINE_FLAG_CACHED: u16 = 1 << 0;

#[derive(Clone, Copy)]
pub struct SpineEntry {
    /// absolute byte offset within the bundle file where the
    /// chapter's stripped content begins (not relative to the
    /// content section). keeps reads a single lookup.
    pub content_offset: u32,
    /// number of bytes of chapter content at `content_offset`
    pub content_size: u32,
    /// cached text bytes after HTML stripping (may equal content_size)
    pub text_bytes: u32,
    pub flags: u16,
    pub _reserved: u16,
}

impl SpineEntry {
    pub const EMPTY: Self = Self {
        content_offset: 0,
        content_size: 0,
        text_bytes: 0,
        flags: 0,
        _reserved: 0,
    };

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < SPINE_ENTRY_SIZE {
            return None;
        }
        Some(Self {
            content_offset: r_u32(buf, 0),
            content_size: r_u32(buf, 4),
            text_bytes: r_u32(buf, 8),
            flags: r_u16(buf, 12),
            _reserved: r_u16(buf, 14),
        })
    }

    pub fn encode(&self) -> [u8; SPINE_ENTRY_SIZE] {
        let mut out = [0u8; SPINE_ENTRY_SIZE];
        w_u32(&mut out, 0, self.content_offset);
        w_u32(&mut out, 4, self.content_size);
        w_u32(&mut out, 8, self.text_bytes);
        w_u16(&mut out, 12, self.flags);
        w_u16(&mut out, 14, self._reserved);
        out
    }

    #[inline]
    pub fn is_cached(&self) -> bool {
        self.flags & SPINE_FLAG_CACHED != 0
    }
}

// ── image table + blobs ────────────────────────────────────────────
//
// layout inside the images section:
//   [ImageEntry; count]  16 bytes each
//   [image blob data; packed]
//
// count is recorded at the start of the section as u16 + pad.

pub const IMAGE_TABLE_HDR_SIZE: usize = 4;
pub const IMAGE_ENTRY_SIZE: usize = 20;

// image status values (write one of these; never combine)
pub const IMG_STATUS_NOT_ATTEMPTED: u16 = 0;
pub const IMG_STATUS_READY: u16 = 1;
pub const IMG_STATUS_UNSUPPORTED: u16 = 2;
pub const IMG_STATUS_DECODE_FAILED: u16 = 3;
pub const IMG_STATUS_TOO_LARGE: u16 = 4;

#[derive(Clone, Copy)]
pub struct ImageEntry {
    pub path_hash: u32,
    pub data_offset: u32, // within images section
    pub data_size: u32,
    pub width: u16,
    pub height: u16,
    pub status: u16,
    pub _reserved: u16,
}

impl ImageEntry {
    pub const EMPTY: Self = Self {
        path_hash: 0,
        data_offset: 0,
        data_size: 0,
        width: 0,
        height: 0,
        status: IMG_STATUS_NOT_ATTEMPTED,
        _reserved: 0,
    };

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < IMAGE_ENTRY_SIZE {
            return None;
        }
        Some(Self {
            path_hash: r_u32(buf, 0),
            data_offset: r_u32(buf, 4),
            data_size: r_u32(buf, 8),
            width: r_u16(buf, 12),
            height: r_u16(buf, 14),
            status: r_u16(buf, 16),
            _reserved: r_u16(buf, 18),
        })
    }

    pub fn encode(&self) -> [u8; IMAGE_ENTRY_SIZE] {
        let mut out = [0u8; IMAGE_ENTRY_SIZE];
        w_u32(&mut out, 0, self.path_hash);
        w_u32(&mut out, 4, self.data_offset);
        w_u32(&mut out, 8, self.data_size);
        w_u16(&mut out, 12, self.width);
        w_u16(&mut out, 14, self.height);
        w_u16(&mut out, 16, self.status);
        w_u16(&mut out, 18, self._reserved);
        out
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
pub const LAYOUT_ALGO_VERSION: u8 = 12;

pub const PAGEIDX_HDR_V2_SIZE: usize = 20;
pub const CHAPTER_LAYOUT_DIR_SIZE: usize = 24;
pub const PAGE_RECORD_SIZE: usize = 12;
pub const LINE_RECORD_SIZE: usize = 12;

// LayoutIdxHeader byte layout (20 bytes):
//   0..4   magic           [u8; 4] = b"PIDX"
//   4      format_version  u8
//   5      algo_version    u8
//   6      font_idx        u8
//   7      content_fmt     u8
//   8..10  text_w          u16
//   10..12 line_h          u16
//   12     max_lines       u8
//   13     flags           u8
//   14     font_family     u8   reader font family (0=Bookerly, 1=Atkinson)
//   15     _pad            1 byte
//   16..20 total_pages     u32
//
// font_family default 0 keeps pre-upgrade caches valid for users who
// stay on Bookerly; switching reader font invalidates the cache because
// wrap points differ between families at the same size.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutIdxHeader {
    pub format_version: u8,
    pub algo_version: u8,
    pub font_idx: u8,
    pub content_fmt: u8,
    pub text_w: u16,
    pub line_h: u16,
    pub max_lines: u8,
    pub flags: u8,
    pub font_family: u8,
    pub total_pages: u32,
}

impl LayoutIdxHeader {
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < PAGEIDX_HDR_V2_SIZE {
            return None;
        }
        if buf[0..4] != PAGEIDX_MAGIC {
            return None;
        }
        if buf[4] != PAGEIDX_FORMAT_VERSION {
            return None;
        }
        Some(Self {
            format_version: buf[4],
            algo_version: buf[5],
            font_idx: buf[6],
            content_fmt: buf[7],
            text_w: r_u16(buf, 8),
            line_h: r_u16(buf, 10),
            max_lines: buf[12],
            flags: buf[13],
            font_family: buf[14],
            // 15 pad
            total_pages: r_u32(buf, 16),
        })
    }

    pub fn encode(&self) -> [u8; PAGEIDX_HDR_V2_SIZE] {
        let mut out = [0u8; PAGEIDX_HDR_V2_SIZE];
        out[0..4].copy_from_slice(&PAGEIDX_MAGIC);
        out[4] = self.format_version;
        out[5] = self.algo_version;
        out[6] = self.font_idx;
        out[7] = self.content_fmt;
        w_u16(&mut out, 8, self.text_w);
        w_u16(&mut out, 10, self.line_h);
        out[12] = self.max_lines;
        out[13] = self.flags;
        out[14] = self.font_family;
        // 15 pad
        w_u32(&mut out, 16, self.total_pages);
        out
    }
}

// ChapterLayoutDir byte layout (24 bytes):
//   0..2   chapter_index  u16
//   2..4   page_count     u16
//   4..6   line_count     u16
//   6..8   flags          u16
//   8..12  pages_offset   u32  (within pageidx section)
//   12..16 lines_offset   u32  (within pageidx section; 0 when no lines)
//   16..20 byte_size      u32  (chapter content byte size at layout time)
//   20..24 _reserved      u32

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChapterLayoutDir {
    pub chapter_index: u16,
    pub page_count: u16,
    pub line_count: u16,
    pub flags: u16,
    pub pages_offset: u32,
    pub lines_offset: u32,
    pub byte_size: u32,
    pub _reserved: u32,
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
        _reserved: 0,
    };

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CHAPTER_LAYOUT_DIR_SIZE {
            return None;
        }
        Some(Self {
            chapter_index: r_u16(buf, 0),
            page_count: r_u16(buf, 2),
            line_count: r_u16(buf, 4),
            flags: r_u16(buf, 6),
            pages_offset: r_u32(buf, 8),
            lines_offset: r_u32(buf, 12),
            byte_size: r_u32(buf, 16),
            _reserved: r_u32(buf, 20),
        })
    }

    pub fn encode(&self) -> [u8; CHAPTER_LAYOUT_DIR_SIZE] {
        let mut out = [0u8; CHAPTER_LAYOUT_DIR_SIZE];
        w_u16(&mut out, 0, self.chapter_index);
        w_u16(&mut out, 2, self.page_count);
        w_u16(&mut out, 4, self.line_count);
        w_u16(&mut out, 6, self.flags);
        w_u32(&mut out, 8, self.pages_offset);
        w_u32(&mut out, 12, self.lines_offset);
        w_u32(&mut out, 16, self.byte_size);
        w_u32(&mut out, 20, self._reserved);
        out
    }
}

// PageRecord byte layout (12 bytes):
//   0..2   first_line  u16  (index into chapter line table)
//   2      line_count  u8
//   3      flags       u8
//   4..8   start_byte  u32  (chapter-relative)
//   8..12  end_byte    u32  (chapter-relative)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRecord {
    pub first_line: u16,
    pub line_count: u8,
    pub flags: u8,
    pub start_byte: u32,
    pub end_byte: u32,
}

impl PageRecord {
    pub const EMPTY: Self = Self {
        first_line: 0,
        line_count: 0,
        flags: 0,
        start_byte: 0,
        end_byte: 0,
    };

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < PAGE_RECORD_SIZE {
            return None;
        }
        Some(Self {
            first_line: r_u16(buf, 0),
            line_count: buf[2],
            flags: buf[3],
            start_byte: r_u32(buf, 4),
            end_byte: r_u32(buf, 8),
        })
    }

    pub fn encode(&self) -> [u8; PAGE_RECORD_SIZE] {
        let mut out = [0u8; PAGE_RECORD_SIZE];
        w_u16(&mut out, 0, self.first_line);
        out[2] = self.line_count;
        out[3] = self.flags;
        w_u32(&mut out, 4, self.start_byte);
        w_u32(&mut out, 8, self.end_byte);
        out
    }
}

// LineRecord byte layout (12 bytes):
//   0..4   start_byte  u32  (chapter-relative)
//   4..8   end_byte    u32  (chapter-relative)
//   8      flags       u8   (see LineLayout::FLAG_* in src/apps/reader/layout/mod.rs)
//   9      indent      u8   (or alt_len for image-origin lines)
//   10     align       u8
//   11     extra       u8   (algo_version >= 2: per-gap stretch/shrink in px;
//                            bit 7 = sign (1 = shrink, 0 = stretch),
//                            bits 0-6 = magnitude in px-per-gap, cap 127.
//                            algo_version == 1: layout-only, e.g. visible soft-hyphen)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRecord {
    pub start_byte: u32,
    pub end_byte: u32,
    pub flags: u8,
    pub indent: u8,
    pub align: u8,
    pub extra: u8,
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

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < LINE_RECORD_SIZE {
            return None;
        }
        Some(Self {
            start_byte: r_u32(buf, 0),
            end_byte: r_u32(buf, 4),
            flags: buf[8],
            indent: buf[9],
            align: buf[10],
            extra: buf[11],
        })
    }

    pub fn encode(&self) -> [u8; LINE_RECORD_SIZE] {
        let mut out = [0u8; LINE_RECORD_SIZE];
        w_u32(&mut out, 0, self.start_byte);
        w_u32(&mut out, 4, self.end_byte);
        out[8] = self.flags;
        out[9] = self.indent;
        out[10] = self.align;
        out[11] = self.extra;
        out
    }
}

// ── RECENT pointer file ────────────────────────────────────────────

pub const RECENT_SIZE: usize = 16;
pub const RECENT_MAGIC: [u8; 4] = *b"RCNT";
pub const RECENT_VERSION: u16 = 1;

#[derive(Clone, Copy)]
pub struct Recent {
    pub name_hash: u32,
    pub last_open_gen: u32,
}

impl Recent {
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < RECENT_SIZE {
            return None;
        }
        if buf[0..4] != RECENT_MAGIC {
            return None;
        }
        if r_u16(buf, 4) != RECENT_VERSION {
            return None;
        }
        Some(Self {
            // 6..8 pad
            name_hash: r_u32(buf, 8),
            last_open_gen: r_u32(buf, 12),
        })
    }

    pub fn encode(&self) -> [u8; RECENT_SIZE] {
        let mut out = [0u8; RECENT_SIZE];
        out[0..4].copy_from_slice(&RECENT_MAGIC);
        w_u16(&mut out, 4, RECENT_VERSION);
        // 6..8 pad
        w_u32(&mut out, 8, self.name_hash);
        w_u32(&mut out, 12, self.last_open_gen);
        out
    }
}

// ── compile-time layout asserts ────────────────────────────────────
//
// these catch off-by-one errors in the `OFF_*` constants if anyone
// adds or reorders fields without updating the running total.

const _: () = {
    assert!(OFF_LAST_OPEN_GEN + 4 == OFF_TITLE_LEN);
    assert!(OFF_TITLE + TITLE_CAP == OFF_AUTHOR_LEN);
    assert!(OFF_AUTHOR + AUTHOR_CAP == OFF_CHAPTER_COUNT);
    assert!(OFF_SPINE_COUNT + 2 + 2 == OFF_BM_CHAPTER);
    assert!(OFF_BM_FLAGS + 1 + 6 == OFF_PAGES_READ);
    assert!(OFF_PROGRESS_PCT + 1 + 1 == OFF_COVERS_OFFSET);
    assert!(OFF_PIDX_FONT_IDX < HEADER_SIZE);
    assert!(OFF_PIDX_DATA_OFFSET + 4 == OFF_PIDX_DATA_SIZE);
    assert!(OFF_PIDX_DATA_SIZE + 4 <= HEADER_SIZE);

    // PIDX v2 record sizes
    assert!(PAGEIDX_HDR_V2_SIZE == 20);
    assert!(CHAPTER_LAYOUT_DIR_SIZE == 24);
    assert!(PAGE_RECORD_SIZE == 12);
    assert!(LINE_RECORD_SIZE == 12);
};

// ── little-endian helpers ──────────────────────────────────────────

#[inline]
fn r_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn r_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn w_u16(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

#[inline]
fn w_u32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn decode_fixed<const N: usize>(buf: &[u8], len_off: usize, body_off: usize) -> FixedStr<N> {
    let n = (buf[len_off] as usize).min(N);
    let mut b = [0u8; N];
    b[..n].copy_from_slice(&buf[body_off..body_off + n]);
    FixedStr::from_raw(b, n as u8)
}

fn encode_fixed<const N: usize>(
    buf: &mut [u8],
    len_off: usize,
    body_off: usize,
    cap: usize,
    s: &FixedStr<N>,
) {
    let n = s.len().min(cap);
    buf[len_off] = n as u8;
    buf[body_off..body_off + n].copy_from_slice(&s.raw_buf()[..n]);
}

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

/// overwrite the entire bundle with the given data (create/truncate)
pub fn write_all(sd: &SdStorage, name_hash: u32, data: &[u8]) -> crate::error::Result<()> {
    let n = bundle_file_name(name_hash);
    sd.write_in_plump_subdir(BOOKS_DIR, bundle_file_str(&n), data)
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
        let on_disk_version = r_u16(&buf, OFF_VERSION);
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

    /// Create a fresh bundle file by writing `header` at offset 0.
    /// Caller is responsible for populating section ranges in
    /// `header` before invoking; typically `BundleHeader::EMPTY`
    /// suffices for the first call.
    pub fn create(
        sd: &'a SdStorage,
        name_hash: u32,
        mut header: BundleHeader,
    ) -> Result<Self, BundleError> {
        header.name_hash = name_hash;
        header.verify_layout()?;
        write_header(sd, name_hash, &header)?;
        let file_size = file_size(sd, name_hash)?;
        Ok(Self {
            sd,
            name_hash,
            header,
            file_size,
        })
    }

    /// Open an existing bundle, or create a fresh one when missing.
    /// Returns `StaleVersion` when the on-disk version is wrong so
    /// the caller can choose to delete and rebuild.
    pub fn open_or_create(
        sd: &'a SdStorage,
        name_hash: u32,
        on_create: BundleHeader,
    ) -> Result<Self, BundleError> {
        if exists(sd, name_hash) {
            Self::open(sd, name_hash)
        } else {
            Self::create(sd, name_hash, on_create)
        }
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
    #[inline]
    pub fn id(&self) -> SectionId {
        self.id
    }

    #[inline]
    pub fn range(&self) -> SectionRange {
        SectionRange {
            offset: self.offset,
            size: self.size,
        }
    }

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

/// read the RECENT pointer from `_PLUMP/RECENT`; returns None when
/// missing or invalid
pub fn read_recent(sd: &SdStorage) -> Option<Recent> {
    let mut buf = [0u8; RECENT_SIZE];
    let n = sd.read_chunk_in_plump(RECENT_FILE, 0, &mut buf).ok()?;
    if n < RECENT_SIZE {
        return None;
    }
    Recent::decode(&buf)
}

/// write the RECENT pointer to `_PLUMP/RECENT`
pub fn write_recent(sd: &SdStorage, recent: &Recent) -> crate::error::Result<()> {
    let bytes = recent.encode();
    sd.write_in_plump(RECENT_FILE, &bytes)
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
            _reserved: 0x7777_8888,
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
        hdr.spine_offset = 256;
        hdr.spine_size = 1104;
        hdr.content_offset = 1360;
        hdr.content_size = 1_013_793;
        hdr.pidx_dir_offset = 1_015_153;
        hdr.pidx_dir_size = 1676;
        hdr.covers_offset = 1_016_065;
        hdr.covers_size = 4006;

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
        hdr.spine_offset = 256;
        hdr.spine_size = 1104;
        hdr.content_offset = 1360;
        hdr.content_size = 1_013_793;
        hdr.pidx_dir_offset = 1_015_153;
        hdr.pidx_dir_size = 1676;
        hdr.covers_offset = 1_016_829;
        hdr.covers_size = 4006;
        hdr.pidx_data_offset = 1_020_835;
        hdr.pidx_data_size = 8000;

        hdr.verify_layout().expect("contiguous disjoint must pass");
    }

    #[test]
    fn verify_layout_accepts_unallocated_sections() {
        // Only spine recorded; everything else is empty.
        let mut hdr = BundleHeader::EMPTY;
        hdr.spine_offset = 256;
        hdr.spine_size = 1104;
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
        h.spine_offset = 256;
        h.spine_size = 1104;
        h.content_offset = 1360;
        h.content_size = 100_000;
        h.pidx_dir_offset = 101_360;
        h.pidx_dir_size = 1676;
        h.pidx_data_offset = 107_042;
        h.pidx_data_size = 8000;
        h.pidx_font_idx = 5;
        let bytes = h.encode();
        let back = BundleHeader::decode(&bytes).expect("round-trip");
        assert_eq!(back.pidx_dir_offset, 101_360);
        assert_eq!(back.pidx_dir_size, 1676);
        assert_eq!(back.pidx_data_offset, 107_042);
        assert_eq!(back.pidx_data_size, 8000);
        assert_eq!(back.pidx_font_idx, 5);
    }

    #[test]
    fn header_decode_rejects_v2_magic_version() {
        // Forge a v2 header (magic ok, version=2). decode rejects it
        // on the version mismatch path so v2 bundles can't be reopened
        // and silently reinterpreted as v3.
        let mut buf = [0u8; HEADER_SIZE];
        buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&HEADER_MAGIC);
        w_u16(&mut buf, OFF_VERSION, 2);
        w_u16(&mut buf, OFF_HEADER_SIZE, HEADER_SIZE as u16);
        assert!(BundleHeader::decode(&buf).is_none());
    }
}

#[cfg(test)]
impl LayoutIdxHeader {
    // a zero header for size assertions; not exposed in production code
    const EMPTY_BYTES: [u8; PAGEIDX_HDR_V2_SIZE] = [0u8; PAGEIDX_HDR_V2_SIZE];
}
