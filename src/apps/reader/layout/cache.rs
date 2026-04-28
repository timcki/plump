//! On-disk PIDX v2 cache: chapter-keyed load / save / invalidate.
//!
//! Wraps the bundle byte-layout records (`LayoutIdxHeader`,
//! `ChapterLayoutDir`, `PageRecord`, `LineRecord`) with a thin
//! reader-facing API. The PIDX section lives at the bundle tail
//! and is only written after `FLAG_CORE_READY` is set, so post-CORE
//! appends naturally land inside the PIDX section without
//! colliding with frozen content / cover / image bytes above.
//!
//! Phase 1 always saves with `lines: &[]` (no per-line records).
//! Phase 2+ will populate line records from the breaker output.
//! The format is the same in both modes; an empty `lines` array
//! sets `line_count = 0` and `lines_offset = 0` in the chapter dir.

use alloc::vec::Vec;

use plump_kernel::kernel::bundle;

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

    let hdr = bundle::read_header(k.sd(), name_hash)?;
    if !hdr.has_flag(bundle::FLAG_PAGEIDX_READY)
        || hdr.pageidx_offset == 0
        || hdr.pageidx_size < bundle::PAGEIDX_HDR_V2_SIZE as u32
    {
        return None;
    }

    // check the layout header (rejects v1 bundles via format byte)
    let mut hdr_buf = [0u8; bundle::PAGEIDX_HDR_V2_SIZE];
    let n = bundle::read_at(k.sd(), name_hash, hdr.pageidx_offset, &mut hdr_buf).ok()?;
    if n < bundle::PAGEIDX_HDR_V2_SIZE {
        return None;
    }
    let lhdr = bundle::LayoutIdxHeader::decode(&hdr_buf)?;
    if !key.matches_header(&lhdr) {
        return None;
    }

    // chapter dir entry
    let dir_off = hdr.pageidx_offset
        + bundle::PAGEIDX_HDR_V2_SIZE as u32
        + (ch * bundle::CHAPTER_LAYOUT_DIR_SIZE) as u32;
    let mut dir_buf = [0u8; bundle::CHAPTER_LAYOUT_DIR_SIZE];
    let n = bundle::read_at(k.sd(), name_hash, dir_off, &mut dir_buf).ok()?;
    if n < bundle::CHAPTER_LAYOUT_DIR_SIZE {
        return None;
    }
    let dir = bundle::ChapterLayoutDir::decode(&dir_buf)?;
    if dir.page_count == 0 || dir.pages_offset == 0 {
        return None;
    }
    let page_count = dir.page_count as usize;
    let line_count = dir.line_count as usize;
    if !super::fits_in_caps(page_count, line_count) {
        return None;
    }

    // pages array
    let pages_abs = hdr.pageidx_offset + dir.pages_offset;
    let pages_bytes = page_count * bundle::PAGE_RECORD_SIZE;
    let mut pages_raw = Vec::new();
    pages_raw.try_reserve_exact(pages_bytes).ok()?;
    pages_raw.resize(pages_bytes, 0);
    let n = bundle::read_at(k.sd(), name_hash, pages_abs, &mut pages_raw).ok()?;
    if n < pages_bytes {
        return None;
    }
    let mut pages = Vec::new();
    pages.try_reserve_exact(page_count).ok()?;
    for i in 0..page_count {
        let off = i * bundle::PAGE_RECORD_SIZE;
        let rec =
            bundle::PageRecord::decode(&pages_raw[off..off + bundle::PAGE_RECORD_SIZE])?;
        pages.push(PageLayout::from_record(&rec));
    }

    // lines array (may be empty)
    let mut lines = Vec::new();
    if line_count > 0 && dir.lines_offset != 0 {
        let lines_abs = hdr.pageidx_offset + dir.lines_offset;
        let lines_bytes = line_count * bundle::LINE_RECORD_SIZE;
        let mut lines_raw = Vec::new();
        lines_raw.try_reserve_exact(lines_bytes).ok()?;
        lines_raw.resize(lines_bytes, 0);
        let n = bundle::read_at(k.sd(), name_hash, lines_abs, &mut lines_raw).ok()?;
        if n < lines_bytes {
            return None;
        }
        lines.try_reserve_exact(line_count).ok()?;
        for i in 0..line_count {
            let off = i * bundle::LINE_RECORD_SIZE;
            let rec =
                bundle::LineRecord::decode(&lines_raw[off..off + bundle::LINE_RECORD_SIZE])?;
            lines.push(LineLayout::from_record(&rec));
        }
    }

    Some(LoadedChapter {
        pages,
        lines,
        byte_size: dir.byte_size,
    })
}

/// persist a chapter's layout. Phase 1 callers always pass an empty
/// `lines` slice. on key mismatch the PIDX section is reset in place
/// before writing so file growth stays bounded.
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
    let mut hdr = bundle::read_header(k.sd(), name_hash).ok_or_else(|| {
        Error::new(ErrorKind::NotFound, "save_layoutidx: bundle missing")
    })?;

    if !hdr.has_flag(bundle::FLAG_CORE_READY) {
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

    // detect "need to init or reset": no PIDX yet, format mismatch,
    // or key mismatch. on mismatch we reset PIDX in place using the
    // existing offset so the file doesn't grow on every font cycle.
    let need_init = needs_reset(k, name_hash, hdr.pageidx_offset, hdr.pageidx_size, key);

    if need_init {
        if hdr.pageidx_offset == 0 {
            // first-ever PIDX: place at current EOF.
            hdr.pageidx_offset = bundle::file_size(k.sd(), name_hash)?;
        }
        write_pidx_header_and_zero_dir(k, name_hash, hdr.pageidx_offset, spine_len, key)?;

        let dir_size = spine_len * bundle::CHAPTER_LAYOUT_DIR_SIZE;
        hdr.pageidx_size = bundle::PAGEIDX_HDR_V2_SIZE as u32 + dir_size as u32;
        hdr.pageidx_font_idx = key.font_idx;
        hdr.set_flag(bundle::FLAG_PAGEIDX_READY, true);
        bundle::write_header(k.sd(), name_hash, &hdr)?;
    }

    // append pages array at current EOF
    let pages_abs = bundle::file_size(k.sd(), name_hash)?;
    let pages_rel = pages_abs - hdr.pageidx_offset;
    let pages_bytes = pages.len() * bundle::PAGE_RECORD_SIZE;
    if pages_bytes > 0 {
        let mut pages_buf = Vec::new();
        pages_buf
            .try_reserve_exact(pages_bytes)
            .map_err(|_| Error::new(ErrorKind::OutOfMemory, "save_layoutidx: pages buf"))?;
        pages_buf.resize(pages_bytes, 0);
        for (i, p) in pages.iter().enumerate() {
            let off = i * bundle::PAGE_RECORD_SIZE;
            pages_buf[off..off + bundle::PAGE_RECORD_SIZE]
                .copy_from_slice(&p.to_record().encode());
        }
        bundle::write_at(k.sd(), name_hash, pages_abs, &pages_buf)?;
    }

    // append lines array immediately after pages (only if non-empty)
    let lines_abs = pages_abs + pages_bytes as u32;
    let lines_bytes = lines.len() * bundle::LINE_RECORD_SIZE;
    let lines_rel = if lines.is_empty() {
        0
    } else {
        let mut lines_buf = Vec::new();
        lines_buf
            .try_reserve_exact(lines_bytes)
            .map_err(|_| Error::new(ErrorKind::OutOfMemory, "save_layoutidx: lines buf"))?;
        lines_buf.resize(lines_bytes, 0);
        for (i, l) in lines.iter().enumerate() {
            let off = i * bundle::LINE_RECORD_SIZE;
            lines_buf[off..off + bundle::LINE_RECORD_SIZE]
                .copy_from_slice(&l.to_record().encode());
        }
        bundle::write_at(k.sd(), name_hash, lines_abs, &lines_buf)?;
        lines_abs - hdr.pageidx_offset
    };

    // chapter dir entry
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
    let dir_off = hdr.pageidx_offset
        + bundle::PAGEIDX_HDR_V2_SIZE as u32
        + (ch * bundle::CHAPTER_LAYOUT_DIR_SIZE) as u32;
    bundle::write_at(k.sd(), name_hash, dir_off, &dir_entry.encode())?;

    // grow recorded pageidx_size, ensure ready flag
    let new_pidx_size = (lines_abs + lines_bytes as u32) - hdr.pageidx_offset;
    let mut dirty = false;
    if new_pidx_size > hdr.pageidx_size {
        hdr.pageidx_size = new_pidx_size;
        dirty = true;
    }
    if !hdr.has_flag(bundle::FLAG_PAGEIDX_READY) {
        hdr.set_flag(bundle::FLAG_PAGEIDX_READY, true);
        dirty = true;
    }
    if dirty {
        bundle::write_header(k.sd(), name_hash, &hdr)?;
    }

    Ok(())
}

/// drop all cached layout records and re-stamp the PIDX header with
/// `new_key`. matches the v1 invalidate-in-place semantics so font
/// cycles don't grow the bundle file.
pub fn invalidate_layoutidx(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    spine_len: usize,
    new_key: &LayoutKey,
) -> crate::error::Result<()> {
    let mut hdr = match bundle::read_header(k.sd(), name_hash) {
        Some(h) => h,
        None => return Ok(()),
    };
    if hdr.pageidx_offset == 0 {
        return Ok(());
    }

    write_pidx_header_and_zero_dir(k, name_hash, hdr.pageidx_offset, spine_len, new_key)?;

    let dir_size = spine_len * bundle::CHAPTER_LAYOUT_DIR_SIZE;
    hdr.pageidx_size = bundle::PAGEIDX_HDR_V2_SIZE as u32 + dir_size as u32;
    hdr.pageidx_font_idx = new_key.font_idx;
    hdr.set_flag(bundle::FLAG_PAGEIDX_READY, false);
    bundle::write_header(k.sd(), name_hash, &hdr)?;
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────

/// returns true if PIDX is missing, too small, the format byte
/// doesn't match v2, or the on-disk key differs from `key`.
fn needs_reset(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    pageidx_offset: u32,
    pageidx_size: u32,
    key: &LayoutKey,
) -> bool {
    if pageidx_offset == 0 || pageidx_size < bundle::PAGEIDX_HDR_V2_SIZE as u32 {
        return true;
    }
    let mut buf = [0u8; bundle::PAGEIDX_HDR_V2_SIZE];
    match bundle::read_at(k.sd(), name_hash, pageidx_offset, &mut buf) {
        Ok(n) if n >= bundle::PAGEIDX_HDR_V2_SIZE => match bundle::LayoutIdxHeader::decode(&buf) {
            Some(lhdr) => !key.matches_header(&lhdr),
            None => true,
        },
        _ => true,
    }
}

/// write a fresh `LayoutIdxHeader` and a zeroed `ChapterLayoutDir`
/// array at `pageidx_offset`. used by both init and invalidate.
fn write_pidx_header_and_zero_dir(
    k: &mut KernelHandle<'_>,
    name_hash: u32,
    pageidx_offset: u32,
    spine_len: usize,
    key: &LayoutKey,
) -> crate::error::Result<()> {
    let lhdr = key.to_header(0);
    bundle::write_at(k.sd(), name_hash, pageidx_offset, &lhdr.encode())?;

    let dir_size = spine_len * bundle::CHAPTER_LAYOUT_DIR_SIZE;
    let zeros = [0u8; 64];
    let mut remaining = dir_size;
    let mut off = pageidx_offset + bundle::PAGEIDX_HDR_V2_SIZE as u32;
    while remaining > 0 {
        let chunk = remaining.min(zeros.len());
        bundle::write_at(k.sd(), name_hash, off, &zeros[..chunk])?;
        off += chunk as u32;
        remaining -= chunk;
    }
    Ok(())
}
