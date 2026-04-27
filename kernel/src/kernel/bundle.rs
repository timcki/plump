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
pub const HEADER_VERSION: u16 = 2;

// content stream format inside the content section. evolves independently
// of HEADER_VERSION so future marker additions can invalidate stored bundles
// without forcing a full layout rev.
//   0 = legacy (Phase 1 marker set: BOLD/ITALIC/H1-H6/U/S/QUOTE/IMG_REF)
//   1 = Phase 2 (adds ALIGN_*/PAGE_BREAK/FIGCAPTION; tag-keyed defaults)
pub const CONTENT_FMT_LATEST: u8 = 1;

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
const OFF_PAGEIDX_OFFSET: usize = 220; // 4
const OFF_PAGEIDX_SIZE: usize = 224; // 4
const OFF_PAGEIDX_FONT_IDX: usize = 228; // 1
const OFF_CONTENT_FMT: usize = 229; // 1
// 230..256 reserved

// bookmark flags bits (inside `bm_flags`)
pub const BM_FLAG_VALID: u8 = 1 << 0;

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
    pub pageidx_offset: u32,
    pub pageidx_size: u32,
    pub pageidx_font_idx: u8,
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
        pageidx_offset: 0,
        pageidx_size: 0,
        pageidx_font_idx: 0,
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
            pageidx_offset: r_u32(buf, OFF_PAGEIDX_OFFSET),
            pageidx_size: r_u32(buf, OFF_PAGEIDX_SIZE),
            pageidx_font_idx: buf[OFF_PAGEIDX_FONT_IDX],
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
        w_u32(&mut out, OFF_PAGEIDX_OFFSET, self.pageidx_offset);
        w_u32(&mut out, OFF_PAGEIDX_SIZE, self.pageidx_size);
        out[OFF_PAGEIDX_FONT_IDX] = self.pageidx_font_idx;
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

// ── page index section ─────────────────────────────────────────────
//
// always the last section of a bundle; rewriting it on font change
// does not touch anything above.
//
// layout inside the pageidx section:
//   [PageIdxHeader]                       12 bytes
//   [ChapterPageDir; spine_count]         12 bytes each
//   [break arrays, packed in dir order]   u32 per page break

pub const PAGEIDX_HDR_SIZE: usize = 12;
pub const PAGEIDX_MAGIC: [u8; 4] = *b"PIDX";
pub const CHAPTER_DIR_ENTRY_SIZE: usize = 12;

// pageidx flags
pub const PIDX_FLAG_FULLY_INDEXED: u8 = 1 << 0;

#[derive(Clone, Copy)]
pub struct PageIdxHeader {
    pub font_idx: u8,
    pub flags: u8,
    pub total_pages: u32,
}

impl PageIdxHeader {
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < PAGEIDX_HDR_SIZE {
            return None;
        }
        if buf[0..4] != PAGEIDX_MAGIC {
            return None;
        }
        Some(Self {
            font_idx: buf[4],
            flags: buf[5],
            // 6..8 pad
            total_pages: r_u32(buf, 8),
        })
    }

    pub fn encode(&self) -> [u8; PAGEIDX_HDR_SIZE] {
        let mut out = [0u8; PAGEIDX_HDR_SIZE];
        out[0..4].copy_from_slice(&PAGEIDX_MAGIC);
        out[4] = self.font_idx;
        out[5] = self.flags;
        // 6..8 pad
        w_u32(&mut out, 8, self.total_pages);
        out
    }
}

#[derive(Clone, Copy)]
pub struct ChapterPageDir {
    pub chapter_index: u16,
    pub page_count: u16,
    pub breaks_offset: u32, // within pageidx section
    pub _reserved: u32,
}

impl ChapterPageDir {
    pub const EMPTY: Self = Self {
        chapter_index: 0,
        page_count: 0,
        breaks_offset: 0,
        _reserved: 0,
    };

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CHAPTER_DIR_ENTRY_SIZE {
            return None;
        }
        Some(Self {
            chapter_index: r_u16(buf, 0),
            page_count: r_u16(buf, 2),
            breaks_offset: r_u32(buf, 4),
            _reserved: r_u32(buf, 8),
        })
    }

    pub fn encode(&self) -> [u8; CHAPTER_DIR_ENTRY_SIZE] {
        let mut out = [0u8; CHAPTER_DIR_ENTRY_SIZE];
        w_u16(&mut out, 0, self.chapter_index);
        w_u16(&mut out, 2, self.page_count);
        w_u32(&mut out, 4, self.breaks_offset);
        w_u32(&mut out, 8, self._reserved);
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
    assert!(OFF_PAGEIDX_FONT_IDX < HEADER_SIZE);
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
