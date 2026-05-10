//! On-disk PIDX v3 cache: chapter-keyed load / save / invalidate.
//!
//! v3 splits the PIDX into two sections owned by `BundleFile`:
//!
//!   * `PidxDir` — fixed size (`PAGEIDX_HDR_V2_SIZE + spine_len * CHAPTER_LAYOUT_DIR_SIZE`),
//!     allocated by `epubs.rs::finish_cache` immediately after the
//!     `Content` section.
//!   * `PidxData` — append-only, allocated lazily on the first
//!     `save_layoutidx`, grown by `grow_tail` for each subsequent
//!     chapter.
//!
//! Chapter-dir `pages_offset` / `lines_offset` are now byte offsets
//! within `PidxData` (not within a single combined PIDX span). The
//! `Section` bounds check in `bundle.rs` prevents a stray write from
//! reaching covers or content.

use alloc::vec::Vec;

use plump_kernel::kernel::bundle;
use plump_kernel::kernel::bundle::{BundleError, BundleFile, SectionId};
// `BundleError` -> `crate::error::Error` via impl `From` in bundle.rs;
// callers use `?` directly on `Result<T, BundleError>` to convert.

use crate::error::{Error, ErrorKind};
use crate::kernel::KernelHandle;

use super::{LayoutKey, LineLayout, MAX_LINES_PER_CHAPTER, PageLayout};

/// fully-decoded layout for one chapter.
pub struct LoadedChapter {
    pub pages: Vec<PageLayout>,
    pub lines: Vec<LineLayout>,
    /// chapter content size at layout time; callers can compare
    /// against the live spine entry to detect content drift.
    pub byte_size: u32,
}

/// load cached layout for `ch` if it exists and the on-disk key
/// matches. returns `None` when the bundle is missing, PIDX is
/// not ready, the format/key doesn't match, the chapter has no
/// records yet, or any read fails.
pub fn load_layoutidx(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    ch: usize,
    spine_len: usize,
    key: &LayoutKey,
) -> Option<LoadedChapter> {
    if ch >= spine_len {
        return None;
    }

    let bf = BundleFile::open(k.sd(), name_hash).ok()?;
    if !bf.header().has_flag(bundle::FLAG_PAGEIDX_READY) {
        return None;
    }
    let dir = bf.section(SectionId::PidxDir)?;
    let data = bf.section(SectionId::PidxData)?;
    if dir.size < bundle::PAGEIDX_HDR_V2_SIZE as u32 {
        return None;
    }

    // PIDX header sits at the start of the PidxDir section
    let mut hdr_buf = [0u8; bundle::PAGEIDX_HDR_V2_SIZE];
    let n = bundle::read_at(k.sd(), name_hash, dir.offset, &mut hdr_buf).ok()?;
    if n < bundle::PAGEIDX_HDR_V2_SIZE {
        return None;
    }
    let lhdr = bundle::LayoutIdxHeader::decode(&hdr_buf)?;
    if !key.matches_header(&lhdr) {
        return None;
    }

    // chapter dir entry (inside PidxDir)
    let dir_entry_rel = bundle::PAGEIDX_HDR_V2_SIZE as u32
        + (ch as u32).checked_mul(bundle::CHAPTER_LAYOUT_DIR_SIZE as u32)?;
    if dir_entry_rel + bundle::CHAPTER_LAYOUT_DIR_SIZE as u32 > dir.size {
        return None;
    }
    let mut dir_buf = [0u8; bundle::CHAPTER_LAYOUT_DIR_SIZE];
    let n = bundle::read_at(k.sd(), name_hash, dir.offset + dir_entry_rel, &mut dir_buf).ok()?;
    if n < bundle::CHAPTER_LAYOUT_DIR_SIZE {
        return None;
    }
    let dir_entry = bundle::ChapterLayoutDir::decode(&dir_buf)?;
    if dir_entry.page_count == 0 {
        return None;
    }
    let page_count = dir_entry.page_count as usize;
    let line_count = dir_entry.line_count as usize;
    if !super::fits_in_caps(page_count, line_count) {
        return None;
    }

    // pages array lives inside PidxData at `dir_entry.pages_offset`
    let pages_bytes = page_count * bundle::PAGE_RECORD_SIZE;
    let pages_end = dir_entry
        .pages_offset
        .checked_add(pages_bytes as u32)?;
    if pages_end > data.size {
        return None;
    }
    let mut pages_raw = Vec::new();
    pages_raw.try_reserve_exact(pages_bytes).ok()?;
    pages_raw.resize(pages_bytes, 0);
    let n = bundle::read_at(
        k.sd(),
        name_hash,
        data.offset + dir_entry.pages_offset,
        &mut pages_raw,
    )
    .ok()?;
    if n < pages_bytes {
        return None;
    }
    let mut pages = Vec::new();
    pages.try_reserve_exact(page_count).ok()?;
    for i in 0..page_count {
        let off = i * bundle::PAGE_RECORD_SIZE;
        let rec = bundle::PageRecord::decode(&pages_raw[off..off + bundle::PAGE_RECORD_SIZE])?;
        pages.push(PageLayout::from_record(&rec));
    }

    // lines array (may be empty)
    let mut lines = Vec::new();
    if line_count > 0 && dir_entry.lines_offset != 0 {
        let lines_bytes = line_count * bundle::LINE_RECORD_SIZE;
        let lines_end = dir_entry.lines_offset.checked_add(lines_bytes as u32)?;
        if lines_end > data.size {
            return None;
        }
        let mut lines_raw = Vec::new();
        lines_raw.try_reserve_exact(lines_bytes).ok()?;
        lines_raw.resize(lines_bytes, 0);
        let n = bundle::read_at(
            k.sd(),
            name_hash,
            data.offset + dir_entry.lines_offset,
            &mut lines_raw,
        )
        .ok()?;
        if n < lines_bytes {
            return None;
        }
        lines.try_reserve_exact(line_count).ok()?;
        for i in 0..line_count {
            let off = i * bundle::LINE_RECORD_SIZE;
            let rec = bundle::LineRecord::decode(&lines_raw[off..off + bundle::LINE_RECORD_SIZE])?;
            lines.push(LineLayout::from_record(&rec));
        }
    }

    Some(LoadedChapter {
        pages,
        lines,
        byte_size: dir_entry.byte_size,
    })
}

/// persist a chapter's layout. on key mismatch the PIDX dir is reset
/// in place (`Section::fill_zero`) and a fresh `LayoutIdxHeader` is
/// stamped; PidxData is not relocated, so the file does not grow
/// unboundedly on font cycles — its stale bytes simply become
/// unreferenced and get overwritten on the next chapter save.
///
/// no-op when `FLAG_CORE_READY` is not set, when the chapter index
/// is out of range, or when the layout would exceed the per-chapter
/// caps.
pub fn save_layoutidx(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    ch: usize,
    spine_len: usize,
    key: &LayoutKey,
    pages: &[PageLayout],
    lines: &[LineLayout],
    byte_size: u32,
) -> crate::error::Result<()> {
    let mut bf = open_bundle_for_write(k, name_hash)?;

    if !bf.header().has_flag(bundle::FLAG_CORE_READY) {
        return Ok(());
    }
    if ch >= spine_len {
        return Ok(());
    }
    if !super::fits_in_caps(pages.len(), lines.len()) {
        log::warn!(
            "save_layoutidx: ch{} pages={} lines={} exceeds caps (max_pages={} max_lines={})",
            ch,
            pages.len(),
            lines.len(),
            super::super::MAX_PAGES,
            MAX_LINES_PER_CHAPTER,
        );
        return Ok(());
    }

    // PidxDir must already exist (allocated by epubs.rs::finish_cache).
    let dir_range = bf
        .section(SectionId::PidxDir)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "save_layoutidx: PidxDir not allocated"))?;

    // detect "need to init or reset": dir empty, format mismatch, or key
    // mismatch. on mismatch we re-stamp the dir header and zero the
    // chapter entries in place.
    let stamp_ok = read_pidx_stamp(&mut bf, dir_range, key)?;
    if !stamp_ok {
        reset_pidx_dir(&mut bf, spine_len, key)?;
    }

    // Make sure PidxData exists; allocate it on first save with size
    // equal to this chapter's bytes. Subsequent saves grow_tail.
    let pages_bytes = pages.len() * bundle::PAGE_RECORD_SIZE;
    let lines_bytes = lines.len() * bundle::LINE_RECORD_SIZE;
    let chapter_bytes = (pages_bytes + lines_bytes) as u32;

    let (pages_rel, lines_rel) = append_chapter_records(
        &mut bf,
        pages,
        lines,
        pages_bytes,
        lines_bytes,
        chapter_bytes,
    )?;

    // chapter dir entry, written through the bounded PidxDir section.
    let dir_entry = bundle::ChapterLayoutDir {
        chapter_index: ch as u16,
        page_count: pages.len() as u16,
        line_count: lines.len() as u16,
        flags: 0,
        pages_offset: pages_rel,
        lines_offset: lines_rel,
        byte_size,
        _reserved: 0,
    };
    let entry_rel = bundle::PAGEIDX_HDR_V2_SIZE as u32
        + (ch * bundle::CHAPTER_LAYOUT_DIR_SIZE) as u32;
    let mut dir = bf.section_mut(SectionId::PidxDir)?;
    dir.write_at(entry_rel, &dir_entry.encode())
        ?;
    drop(dir);

    if !bf.header().has_flag(bundle::FLAG_PAGEIDX_READY) {
        bf.header_mut()
            .set_flag(bundle::FLAG_PAGEIDX_READY, true);
        bf.commit_header()?;
    }

    Ok(())
}

/// drop all cached layout records and re-stamp the PIDX header with
/// `new_key`. matches v2 invalidate-in-place semantics so font cycles
/// don't grow the bundle file: only PidxDir is zeroed; PidxData's
/// stale bytes are left alone and become unreferenced.
pub fn invalidate_layoutidx(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    spine_len: usize,
    new_key: &LayoutKey,
) -> crate::error::Result<()> {
    let mut bf = match BundleFile::open(k.sd(), name_hash) {
        Ok(bf) => bf,
        // missing or stale-version: nothing to invalidate
        Err(BundleError::MissingOrCorrupt) | Err(BundleError::StaleVersion(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if bf.section(SectionId::PidxDir).is_none() {
        return Ok(());
    }
    reset_pidx_dir(&mut bf, spine_len, new_key)?;
    bf.header_mut().pidx_font_idx = new_key.font_idx;
    bf.header_mut()
        .set_flag(bundle::FLAG_PAGEIDX_READY, false);
    bf.commit_header()?;
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────

/// Read the `LayoutIdxHeader` stamp at the start of PidxDir and check
/// it matches the requested key. Returns `Ok(true)` when the stamp
/// matches (no reset needed), `Ok(false)` when it doesn't.
fn read_pidx_stamp(
    bf: &mut BundleFile<'_>,
    dir: bundle::SectionRange,
    key: &LayoutKey,
) -> crate::error::Result<bool> {
    if dir.size < bundle::PAGEIDX_HDR_V2_SIZE as u32 {
        return Ok(false);
    }
    let mut buf = [0u8; bundle::PAGEIDX_HDR_V2_SIZE];
    let dir_section = bf.section_mut(SectionId::PidxDir)?;
    let n = dir_section.read_at(0, &mut buf)?;
    if n < bundle::PAGEIDX_HDR_V2_SIZE {
        return Ok(false);
    }
    match bundle::LayoutIdxHeader::decode(&buf) {
        Some(lhdr) => Ok(key.matches_header(&lhdr)),
        None => Ok(false),
    }
}

/// Zero the PidxDir contents (via the bounded Section handle — cannot
/// touch neighbouring sections) and re-stamp the `LayoutIdxHeader`.
fn reset_pidx_dir(
    bf: &mut BundleFile<'_>,
    _spine_len: usize,
    key: &LayoutKey,
) -> crate::error::Result<()> {
    let mut dir = bf.section_mut(SectionId::PidxDir)?;
    dir.fill_zero()?;
    let lhdr = key.to_header(0);
    dir.write_at(0, &lhdr.encode())?;
    Ok(())
}

/// Append this chapter's pages and lines arrays into `PidxData`,
/// allocating PidxData on first use and `grow_tail`-ing it on
/// subsequent calls. Returns `(pages_offset, lines_offset)` — both
/// relative to `PidxData`'s start, ready for the chapter dir entry.
fn append_chapter_records(
    bf: &mut BundleFile<'_>,
    pages: &[PageLayout],
    lines: &[LineLayout],
    pages_bytes: usize,
    lines_bytes: usize,
    chapter_bytes: u32,
) -> crate::error::Result<(u32, u32)> {
    let existing = bf.section(SectionId::PidxData);
    let base_size = existing.map(|r| r.size).unwrap_or(0);

    // pre-stage the bytes so any allocation failure happens before we
    // touch the file
    let pages_buf = encode_pages(pages, pages_bytes)?;
    let lines_buf = encode_lines(lines, lines_bytes)?;

    let mut data = if existing.is_some() {
        bf.grow_tail(SectionId::PidxData, chapter_bytes)
            ?
    } else {
        bf.allocate(SectionId::PidxData, chapter_bytes)
            ?
    };

    let pages_rel = base_size;
    if !pages_buf.is_empty() {
        data.write_at(pages_rel, &pages_buf)?;
    }

    let lines_rel = if lines_buf.is_empty() {
        0
    } else {
        let rel = pages_rel + pages_bytes as u32;
        data.write_at(rel, &lines_buf)?;
        rel
    };

    Ok((pages_rel, lines_rel))
}

fn encode_pages(pages: &[PageLayout], pages_bytes: usize) -> crate::error::Result<Vec<u8>> {
    if pages_bytes == 0 {
        return Ok(Vec::new());
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(pages_bytes)
        .map_err(|_| Error::new(ErrorKind::OutOfMemory, "save_layoutidx: pages buf"))?;
    buf.resize(pages_bytes, 0);
    for (i, p) in pages.iter().enumerate() {
        let off = i * bundle::PAGE_RECORD_SIZE;
        buf[off..off + bundle::PAGE_RECORD_SIZE].copy_from_slice(&p.to_record().encode());
    }
    Ok(buf)
}

fn encode_lines(lines: &[LineLayout], lines_bytes: usize) -> crate::error::Result<Vec<u8>> {
    if lines_bytes == 0 {
        return Ok(Vec::new());
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(lines_bytes)
        .map_err(|_| Error::new(ErrorKind::OutOfMemory, "save_layoutidx: lines buf"))?;
    buf.resize(lines_bytes, 0);
    for (i, l) in lines.iter().enumerate() {
        let off = i * bundle::LINE_RECORD_SIZE;
        buf[off..off + bundle::LINE_RECORD_SIZE].copy_from_slice(&l.to_record().encode());
    }
    Ok(buf)
}

/// Open the bundle for write, deleting and rebuilding the header on
/// stale-version. Returns `Err(NotFound)` if the bundle is genuinely
/// missing — the caller's normal flow re-creates it.
fn open_bundle_for_write<'a>(
    k: &'a mut KernelHandle<'_>,
    name_hash: u32,
) -> crate::error::Result<BundleFile<'a>> {
    match BundleFile::open(k.sd(), name_hash) {
        Ok(bf) => Ok(bf),
        Err(BundleError::StaleVersion(v)) => {
            log::info!(
                "bundle: v{} stale on save_layoutidx, deleting {:#010X}",
                v,
                name_hash,
            );
            bundle::delete(k.sd(), name_hash)?;
            Err(Error::new(
                ErrorKind::NotFound,
                "save_layoutidx: stale bundle, rebuild from source",
            ))
        }
        Err(BundleError::MissingOrCorrupt) => Err(Error::new(
            ErrorKind::NotFound,
            "save_layoutidx: bundle missing",
        )),
        Err(e) => Err(e.into()),
    }
}

