// text wrapping, page navigation, and load/prefetch

use alloc::vec::Vec;

use smol_epub::markup::{self, BlockProps, ByteSource, Event, Events, SliceSource, Style};

use crate::fonts;
use crate::fonts::bitmap;
use crate::kernel::KernelHandle;

use super::layout::pipeline::{ImageBudget, LayoutPipeline, StepOutcome, TypesetError};
use super::layout::paginate::PageSpacing;
use super::layout::{paginate, LineLayout, PageLayout};

use super::{
    DEFAULT_IMG_H, INDENT_PX, LINES_PER_PAGE, LineSpan, MAX_PAGES, MAX_PAGE_RUNS,
    MAX_RUNS_PER_LINE, NO_PREFETCH, PAGE_BUF, PendingPositionChange, ReaderApp, Run, State,
    decode_utf8_char, inline_img_max_h,
};
use crate::kernel::work_queue;

/// How `preindex_all_pages` ended. `RetryLater` means K-P hit an
/// allocation failure while an image decode was in flight; the caller
/// keeps `State::NeedIndex` and re-runs after the worker frees its
/// buffers, so the chapter still gets K-P layout instead of greedy.
pub(super) enum PreindexOutcome {
    Done,
    RetryLater,
}

/// Consecutive OOM waits before typeset stops waiting on the worker
/// and starts shedding memory itself (livelock guard; each wait is one
/// background step, normally bounded by <= 2 queued decode jobs).
const TYPESET_OOM_WAIT_CAP: u8 = 16;

impl ReaderApp {
    pub(super) fn wrap_lines_counted(&mut self, n: usize) -> usize {
        let fonts_copy = self.fonts;

        if let Some(fs) = fonts_copy {
            let heights = &self.img_heights[..self.img_height_count as usize];
            let (c, count) = wrap_proportional(
                &self.pg.buf,
                n,
                &fs,
                &mut self.pg.lines,
                self.max_lines as usize,
                self.text_w,
                heights,
                self.font_line_h,
                fs.em_px(),
            );
            self.pg.line_count = count;
            c
        } else {
            self.wrap_monospace(n)
        }
    }

    pub(super) fn wrap_monospace(&mut self, n: usize) -> usize {
        use super::CHARS_PER_LINE;

        let max = self.max_lines as usize;
        self.pg.line_count = 0;
        let mut col: usize = 0;
        let mut line_start: usize = 0;
        let mut skipped_leading_blank = false;

        for i in 0..n {
            let b = self.pg.buf[i];
            match b {
                b'\r' => {}
                b'\n' => {
                    let end = trim_trailing_cr(&self.pg.buf, line_start, i);
                    if self.pg.line_count == 0 && end == line_start && !skipped_leading_blank {
                        skipped_leading_blank = true;
                        line_start = i + 1;
                        col = 0;
                        continue;
                    }
                    self.push_line(line_start, end);
                    line_start = i + 1;
                    col = 0;
                    if self.pg.line_count >= max {
                        return line_start;
                    }
                }
                _ => {
                    col += 1;
                    if col >= CHARS_PER_LINE {
                        self.push_line(line_start, i + 1);
                        line_start = i + 1;
                        col = 0;
                        if self.pg.line_count >= max {
                            return line_start;
                        }
                    }
                }
            }
        }

        if line_start < n && self.pg.line_count < max {
            let end = trim_trailing_cr(&self.pg.buf, line_start, n);
            self.push_line(line_start, end);
        }

        n
    }

    pub(super) fn push_line(&mut self, start: usize, end: usize) {
        if self.pg.line_count < LINES_PER_PAGE {
            self.pg.lines[self.pg.line_count] = LineSpan {
                start: start as u16,
                len: (end - start) as u16,
                flags: 0,
                indent: 0,
                align: LineSpan::ALIGN_DEFAULT,
                extra: 0,
            };
            self.pg.line_count += 1;
        }
    }

    pub(super) fn reset_paging(&mut self) {
        self.pg.page = 0;
        self.pg.offsets[0] = 0;
        self.pg.total_pages = 1;
        self.pg.fully_indexed = false;
        self.pg.buf_len = 0;
        self.pg.line_count = 0;
        self.pg.prefetch_page = NO_PREFETCH;
        self.pg.prefetch_len = 0;
        self.img_height_count = 0;
        self.page_img = None;
        self.fullscreen_img = false;
        // free prior chapter's K-P state. has_kp_layout() returns true
        // while these vecs are non-empty; load_page_dispatched would
        // otherwise route the new chapter's bytes through the old
        // chapter's line layout and produce garbled formatting. Also
        // releases ~22 KB of heap so the new chapter's ch_cache can
        // allocate without OOM on tight heaps.
        self.pg.clear_kp_layout();
    }

    pub(super) fn locate_page_for_offset(
        &self,
        target_off: u32,
        page_hint: Option<usize>,
    ) -> usize {
        let total = self.pg.total_pages.max(1);

        if let Some(page) = page_hint.filter(|&page| page < total) {
            let start = self.pg.offsets[page];
            let end = if page + 1 < total {
                self.pg.offsets[page + 1]
            } else {
                u32::MAX
            };
            if target_off >= start && target_off < end {
                return page;
            }
        }

        let mut lo = 0usize;
        let mut hi = total;
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.pg.offsets[mid] <= target_off {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub(super) fn load_and_prefetch(
        &mut self,
        k: &mut KernelHandle<'_>,
    ) -> crate::error::Result<()> {
        plump_kernel::perf_begin!(_lp_t0);

        if !self.epub.ch_cache.is_empty() {
            plump_kernel::perf_begin!(_t_read);
            let start = (self.pg.offsets[self.pg.page] as usize).min(self.epub.ch_cache.len());
            let end = (start + PAGE_BUF).min(self.epub.ch_cache.len());
            let n = end - start;
            if n > 0 {
                self.pg.buf[..n].copy_from_slice(&self.epub.ch_cache[start..end]);
            }
            self.pg.buf_len = n;
            self.pg.prefetch_page = NO_PREFETCH;
            self.pg.prefetch_len = 0;
            plump_kernel::perf_event!(
                "reader",
                "load_prefetch.read src=ch_cache bytes={} elapsed_ms={}",
                n,
                _t_read.elapsed().as_millis()
            );

            plump_kernel::perf_begin!(_t_prescan);
            self.prescan_image_heights(k, n);
            plump_kernel::perf_event!(
                "reader",
                "load_prefetch.prescan bytes={} elapsed_ms={}",
                n,
                _t_prescan.elapsed().as_millis()
            );

            plump_kernel::perf_begin!(_t_wrap);
            self.wrap_lines_counted(n);
            self.build_page_runs();
            plump_kernel::perf_event!(
                "reader",
                "load_prefetch.wrap+metrics lines={} elapsed_ms={}",
                self.pg.line_count,
                _t_wrap.elapsed().as_millis()
            );

            plump_kernel::perf_begin!(_t_decode);
            self.decode_page_images(k);
            plump_kernel::perf_event!(
                "reader",
                "load_prefetch.decode elapsed_ms={}",
                _t_decode.elapsed().as_millis()
            );

            plump_kernel::perf_event!(
                "reader",
                "load_prefetch page={} src=ch_cache bytes={} lines={} elapsed_ms={}",
                self.pg.page,
                n,
                self.pg.line_count,
                _lp_t0.elapsed().as_millis()
            );
            return Ok(());
        }

        let fname = self.filename;
        let name = fname.as_str();

        // -- read stage --
        plump_kernel::perf_begin!(_t_read);
        let mut _read_src = "sd";
        if self.pg.prefetch_page == self.pg.page {
            _read_src = "prefetch";
            let pf_len = self.pg.prefetch_len;
            self.pg.buf[..pf_len].copy_from_slice(&self.pg.prefetch[..pf_len]);
            self.pg.buf_len = pf_len;
            self.pg.prefetch_page = NO_PREFETCH;
            self.pg.prefetch_len = 0;
        } else if self.is_epub && self.epub.chapters_cached {
            _read_src = "cache";
            let ch = self.epub.chapter as usize;
            let ch_base = self.epub.chapter_table[ch].0;
            let n = plump_kernel::kernel::bundle::read_at(
                k.sd(),
                self.epub.name_hash,
                ch_base + self.pg.offsets[self.pg.page],
                &mut self.pg.buf,
            )?;
            self.pg.buf_len = n;
        } else if self.file_size == 0 {
            _read_src = "first_read";
            let (size, n) = k.sd().read_file_start(name, &mut self.pg.buf)?;
            self.file_size = size;
            self.pg.buf_len = n;
            log::info!("reader: opened {} ({} bytes)", name, size);

            if size == 0 {
                self.pg.fully_indexed = true;
                self.pg.line_count = 0;
                return Ok(());
            }
        } else {
            let n = k.sd().read_file_chunk(name, self.pg.offsets[self.pg.page], &mut self.pg.buf)?;
            self.pg.buf_len = n;
        }
        plump_kernel::perf_event!(
            "reader",
            "load_prefetch.read src={} bytes={} elapsed_ms={}",
            _read_src,
            self.pg.buf_len,
            _t_read.elapsed().as_millis()
        );

        // -- prescan + wrap stages --
        plump_kernel::perf_begin!(_t_prescan);
        self.prescan_image_heights(k, self.pg.buf_len);
        plump_kernel::perf_event!(
            "reader",
            "load_prefetch.prescan bytes={} elapsed_ms={}",
            self.pg.buf_len,
            _t_prescan.elapsed().as_millis()
        );

        plump_kernel::perf_begin!(_t_wrap);
        let consumed = self.wrap_lines_counted(self.pg.buf_len);
        self.build_page_runs();
        plump_kernel::perf_event!(
            "reader",
            "load_prefetch.wrap+metrics lines={} consumed={} elapsed_ms={}",
            self.pg.line_count,
            consumed,
            _t_wrap.elapsed().as_millis()
        );

        let next_offset = self.pg.offsets[self.pg.page] + consumed as u32;

        if self.pg.page + 1 >= self.pg.total_pages && !self.pg.fully_indexed {
            if self.pg.line_count >= self.max_lines as usize && next_offset < self.file_size {
                if self.pg.total_pages < MAX_PAGES {
                    self.pg.offsets[self.pg.total_pages] = next_offset;
                    self.pg.total_pages += 1;
                } else {
                    self.pg.fully_indexed = true;
                }
            } else {
                self.pg.fully_indexed = true;
            }
        }

        // -- prefetch stage --
        plump_kernel::perf_begin!(_t_pf);
        if self.pg.page + 1 < self.pg.total_pages {
            if self.pg.prefetch.len() < PAGE_BUF {
                self.pg.prefetch.resize(PAGE_BUF, 0);
            }
            let pf_offset = self.pg.offsets[self.pg.page + 1];
            let pf_result = if self.is_epub && self.epub.chapters_cached {
                let ch = self.epub.chapter as usize;
                let ch_base = self.epub.chapter_table[ch].0;
                plump_kernel::kernel::bundle::read_at(
                    k.sd(),
                    self.epub.name_hash,
                    ch_base + pf_offset,
                    &mut self.pg.prefetch,
                )
            } else {
                k.sd().read_file_chunk(name, pf_offset, &mut self.pg.prefetch)
            };
            match pf_result {
                Ok(n) => {
                    self.pg.prefetch_len = n;
                    self.pg.prefetch_page = self.pg.page + 1;
                }
                Err(_) => {
                    self.pg.prefetch_page = NO_PREFETCH;
                    self.pg.prefetch_len = 0;
                }
            }
        } else {
            self.pg.prefetch_page = NO_PREFETCH;
            self.pg.prefetch_len = 0;
        }
        plump_kernel::perf_event!(
            "reader",
            "load_prefetch.prefetch did_prefetch={} elapsed_ms={}",
            self.pg.prefetch_page != NO_PREFETCH,
            _t_pf.elapsed().as_millis()
        );

        // -- decode stage --
        plump_kernel::perf_begin!(_t_decode);
        self.decode_page_images(k);
        plump_kernel::perf_event!(
            "reader",
            "load_prefetch.decode elapsed_ms={}",
            _t_decode.elapsed().as_millis()
        );

        plump_kernel::perf_event!(
            "reader",
            "load_prefetch page={} src={} bytes={} lines={} elapsed_ms={}",
            self.pg.page,
            _read_src,
            self.pg.buf_len,
            self.pg.line_count,
            _lp_t0.elapsed().as_millis()
        );
        Ok(())
    }

    pub(super) fn preindex_all_pages(&mut self, k: &mut KernelHandle<'_>) -> PreindexOutcome {
        plump_kernel::perf_begin!(_pi_t0);

        // Empty ch_cache no longer bypasses K-P: `run_kp_typeset` falls
        // back to streaming via `BundleByteSource` when ch_cache didn't
        // load. Pre-empting it with greedy-bundle here would force the
        // greedy path even on chapters K-P can handle just fine.

        let ch = self.epub.chapter as usize;
        let spine_len = self.epub.spine.len();
        let name_hash = self.epub.name_hash;
        let key = super::layout::LayoutKey::current(
            self.book_font_size_idx,
            self.reader_font.to_idx(),
            plump_kernel::kernel::bundle::CONTENT_FMT_LATEST,
            self.text_w as u16,
            self.font_line_h,
            self.max_lines,
        );

        // 1. cache hit (algo=2 with line records): adopt directly.
        // pages come back empty when they were built at a different
        // line_h / max_lines; the line table is still valid, so
        // rebuild image fillers and re-paginate in RAM instead of
        // re-typesetting.
        if let Some(loaded) =
            super::layout::cache::load_layoutidx(k, name_hash, ch, spine_len, &key)
        {
            if !loaded.lines.is_empty() {
                // perf-only label; the macro is a no-op without the
                // perf feature, hence the underscore
                let (adopted, _src) = if !loaded.pages.is_empty() {
                    self.adopt_loaded_chapter(loaded);
                    (true, "bundle")
                } else {
                    (self.adopt_lines_repaginate(loaded), "repaginate")
                };
                if adopted {
                    self.typeset_oom_waits = 0;
                    plump_kernel::perf_event!(
                        "reader",
                        "preindex src={} pages={} lines={} elapsed_ms={}",
                        _src,
                        self.pg.total_pages,
                        self.pg.chapter_lines.len(),
                        _pi_t0.elapsed().as_millis()
                    );
                    return PreindexOutcome::Done;
                }
            }
            // legacy cache without lines (or a failed re-pagination):
            // ignore, fall through to typeset
        }

        // 2. K-P typeset (primary path).
        self.pg.clear_kp_layout();
        match self.run_kp_typeset(k) {
            Ok(()) => {
                self.typeset_oom_waits = 0;
                let _ = self.save_kp_to_pidx(k, &key, ch, spine_len, name_hash);
                plump_kernel::perf_event!(
                    "reader",
                    "preindex src=kp pages={} lines={} elapsed_ms={}",
                    self.pg.kp_pages.len(),
                    self.pg.chapter_lines.len(),
                    _pi_t0.elapsed().as_millis()
                );
                return PreindexOutcome::Done;
            }
            Err(TypesetError::OutOfMemory) => {
                // memory-governor escalation. step 1: an in-flight image
                // decode holds its band buffer only until it completes,
                // so wait for the worker instead of degrading the layout
                if !work_queue::is_idle() && self.typeset_oom_waits < TYPESET_OOM_WAIT_CAP {
                    self.typeset_oom_waits += 1;
                    log::info!(
                        "reader: typeset OOM ch{}, waiting for decode (wait {})",
                        ch,
                        self.typeset_oom_waits,
                    );
                    return PreindexOutcome::RetryLater;
                }
                // step 2: shed the chapter cache; run_kp_typeset falls
                // back to streaming the chapter from the bundle on SD
                if !self.epub.ch_cache.is_empty() {
                    log::info!(
                        "reader: typeset OOM ch{}, shedding {}K ch_cache and streaming",
                        ch,
                        self.epub.ch_cache.len() / 1024,
                    );
                    self.epub.ch_cache = Vec::new();
                    self.pg.clear_kp_layout();
                    match self.run_kp_typeset(k) {
                        Ok(()) => {
                            self.typeset_oom_waits = 0;
                            let _ = self.save_kp_to_pidx(k, &key, ch, spine_len, name_hash);
                            plump_kernel::perf_event!(
                                "reader",
                                "preindex src=kp-stream pages={} lines={} elapsed_ms={}",
                                self.pg.kp_pages.len(),
                                self.pg.chapter_lines.len(),
                                _pi_t0.elapsed().as_millis()
                            );
                            return PreindexOutcome::Done;
                        }
                        Err(e) => {
                            log::warn!(
                                "reader: K-P typeset failed ch{}: {:?}; greedy fallback",
                                ch,
                                e,
                            );
                        }
                    }
                } else {
                    // step 3: nothing left to wait for or shed
                    log::warn!("reader: typeset OOM ch{} unrecoverable; greedy fallback", ch);
                }
            }
            Err(e) => {
                log::warn!("reader: K-P typeset failed ch{}: {:?}; greedy fallback", ch, e);
            }
        }
        self.typeset_oom_waits = 0;

        // 3. greedy fallback (no PIDX save — algo=2 is reserved for K-P).
        self.greedy_preindex_compute(k);
        plump_kernel::perf_event!(
            "reader",
            "preindex src=greedy pages={} elapsed_ms={}",
            self.pg.total_pages,
            _pi_t0.elapsed().as_millis()
        );
        PreindexOutcome::Done
    }

    /// Adopt a `LoadedChapter` from PIDX into `pg`. Populates K-P
    /// fields (chapter_lines, kp_pages, image_block_lines) and
    /// mirrors the page-start offsets into the legacy navigation
    /// arrays.
    fn adopt_loaded_chapter(&mut self, loaded: super::layout::cache::LoadedChapter) {
        self.pg.clear_kp_layout();
        // move rather than copy; avoids double residency of the page
        // table during adoption (load already enforces the caps via
        // fits_in_caps, the truncate is a belt-and-braces guard)
        let mut pages = loaded.pages;
        pages.truncate(MAX_PAGES);
        self.pg.kp_pages = pages;
        self.pg.chapter_lines = loaded.lines;
        // recompute image_block_lines from the line table by counting
        // consecutive FLAG_IMAGE entries starting at each origin
        // (an "origin" is the first FLAG_IMAGE line in a run).
        self.pg.image_block_lines.clear();
        self.pg.image_block_lines.resize(self.pg.chapter_lines.len(), 0);
        let mut i = 0;
        while i < self.pg.chapter_lines.len() {
            if self.pg.chapter_lines[i].is_image() {
                let mut block = 1usize;
                while i + block < self.pg.chapter_lines.len()
                    && self.pg.chapter_lines[i + block].is_image()
                    && self.pg.chapter_lines[i + block].start_byte == 0
                    && self.pg.chapter_lines[i + block].end_byte == 0
                {
                    block += 1;
                }
                self.pg.image_block_lines[i] = block.min(u8::MAX as usize) as u8;
                i += block;
            } else {
                i += 1;
            }
        }

        let n = self.pg.kp_pages.len();
        for i in 0..n {
            self.pg.offsets[i] = self.pg.kp_pages[i].start_byte;
        }
        self.pg.total_pages = n.max(1);
        self.pg.fully_indexed = true;
    }

    /// Adopt a lines-only `LoadedChapter` whose page table was built
    /// at a different line_h / max_lines: rebuild each image block's
    /// filler count from the reserved height stored on its origin
    /// line (extra byte, 4 px units), re-run pagination at the
    /// current metrics, and publish. Returns false when the rebuilt
    /// table would exceed the per-chapter caps, an allocation fails,
    /// or pagination errors; the caller falls back to a full typeset.
    fn adopt_lines_repaginate(
        &mut self,
        loaded: super::layout::cache::LoadedChapter,
    ) -> bool {
        use super::layout::{LineLayout, MAX_LINES_PER_CHAPTER, PageLayout};

        let line_h = self.font_line_h.max(1) as u32;
        let src = loaded.lines;
        let mut lines: Vec<LineLayout> = Vec::new();
        let mut blocks: Vec<u8> = Vec::new();
        if lines.try_reserve(src.len()).is_err() || blocks.try_reserve(src.len()).is_err() {
            return false;
        }

        let mut i = 0usize;
        while i < src.len() {
            let l = src[i];
            if l.is_image() {
                // origin + its stored filler run (filler = zero-byte
                // IMAGE line, same shape adopt_loaded_chapter counts)
                let mut old_block = 1usize;
                while i + old_block < src.len()
                    && src[i + old_block].is_image()
                    && src[i + old_block].start_byte == 0
                    && src[i + old_block].end_byte == 0
                {
                    old_block += 1;
                }
                // reserved height rides the origin's extra byte in
                // 4 px units; 0 = unknown, keep the stored count
                let stored_h = (l.extra as u32) * 4;
                let new_block = if stored_h > 0 {
                    stored_h.div_ceil(line_h).clamp(1, u8::MAX as u32) as usize
                } else {
                    old_block
                };
                if lines.len() + new_block > MAX_LINES_PER_CHAPTER {
                    return false;
                }
                if lines.try_reserve(new_block).is_err()
                    || blocks.try_reserve(new_block).is_err()
                {
                    return false;
                }
                lines.push(l);
                blocks.push(new_block.min(u8::MAX as usize) as u8);
                for _ in 1..new_block {
                    lines.push(LineLayout {
                        start_byte: 0,
                        end_byte: 0,
                        flags: LineLayout::FLAG_IMAGE,
                        indent: 0,
                        align: LineLayout::ALIGN_DEFAULT,
                        extra: 0,
                    });
                    blocks.push(0);
                }
                i += old_block;
            } else {
                lines.push(l);
                blocks.push(0);
                i += 1;
            }
        }

        let spacing = PageSpacing {
            em_px: self.fonts.map(|f| f.em_px()).unwrap_or(self.font_line_h),
            line_h: self.font_line_h,
        };
        let mut pages: Vec<PageLayout> = Vec::new();
        if super::layout::paginate::paginate(&lines, self.max_lines, &blocks, spacing, &mut pages)
            .is_err()
        {
            return false;
        }

        self.pg.clear_kp_layout();
        pages.truncate(MAX_PAGES);
        self.pg.kp_pages = pages;
        self.pg.chapter_lines = lines;
        self.pg.image_block_lines = blocks;
        let n = self.pg.kp_pages.len();
        for i in 0..n {
            self.pg.offsets[i] = self.pg.kp_pages[i].start_byte;
        }
        self.pg.total_pages = n.max(1);
        self.pg.fully_indexed = true;
        true
    }

    /// Persist K-P typeset output to the bundle's PIDX section.
    fn save_kp_to_pidx(
        &self,
        k: &mut KernelHandle<'_>,
        key: &super::layout::LayoutKey,
        ch: usize,
        spine_len: usize,
        name_hash: u32,
    ) -> crate::error::Result<()> {
        let total_bytes = self.epub.ch_cache.len() as u32;
        super::layout::cache::save_layoutidx(
            k,
            name_hash,
            ch,
            spine_len,
            key,
            &self.pg.kp_pages,
            &self.pg.chapter_lines,
            total_bytes,
        )
    }

    /// Greedy first-fit pre-indexer (used only as fallback when K-P
    /// fails). Populates `pg.offsets` and `pg.total_pages` so the
    /// existing per-page wrap path can drive navigation; does NOT
    /// save to PIDX (that section is reserved for K-P data).
    ///
    /// Reads from `epub.ch_cache` when populated; falls back to
    /// streaming the chapter from the on-disk bundle when ch_cache
    /// was rejected as oversized (see CHAPTER_CACHE_MAX). The
    /// bundle-streamed path keeps long chapters navigable even
    /// when they don't fit in RAM.
    fn greedy_preindex_compute(&mut self, k: &mut KernelHandle<'_>) {
        self.pg.clear_kp_layout();
        self.pg.offsets[0] = 0;
        self.pg.total_pages = 1;

        let from_bundle = self.epub.ch_cache.is_empty()
            && self.is_epub
            && self.epub.chapters_cached;
        let total = if from_bundle {
            let ch = self.epub.chapter as usize;
            self.epub.chapter_table[ch].1 as usize
        } else {
            self.epub.ch_cache.len()
        };

        if total == 0 {
            self.pg.fully_indexed = true;
            return;
        }

        let mut offset = 0usize;
        while offset < total && self.pg.total_pages < MAX_PAGES {
            let n = if from_bundle {
                let ch = self.epub.chapter as usize;
                let ch_base = self.epub.chapter_table[ch].0;
                let want = (total - offset).min(PAGE_BUF);
                let buf = &mut self.pg.buf[..want];
                match plump_kernel::kernel::bundle::read_at(
                    k.sd(),
                    self.epub.name_hash,
                    ch_base + offset as u32,
                    buf,
                ) {
                    Ok(n) if n > 0 => n,
                    _ => break,
                }
            } else {
                let end = (offset + PAGE_BUF).min(total);
                let len = end - offset;
                self.pg.buf[..len].copy_from_slice(&self.epub.ch_cache[offset..end]);
                len
            };
            self.pg.buf_len = n;
            self.prescan_image_heights(k, n);

            let consumed = self.wrap_lines_counted(n);
            let next_offset = offset + consumed;

            if self.pg.line_count >= self.max_lines as usize && next_offset < total {
                self.pg.offsets[self.pg.total_pages] = next_offset as u32;
                self.pg.total_pages += 1;
                offset = next_offset;
            } else {
                break;
            }
        }
        self.pg.fully_indexed = true;
    }

    /// Knuth-Plass typeset path: drives `LayoutPipeline` over the
    /// cached chapter bytes, paginates the resulting line table, and
    /// publishes both into `pg.chapter_lines` / `pg.kp_pages` /
    /// `pg.image_block_lines`. On any error, leaves `pg` untouched
    /// so the caller can fall back to greedy pre-indexing.
    ///
    /// Caller invariants: `epub.ch_cache` is populated, `fonts` is
    /// `Some`, and `text_w`/`font_line_h`/`max_lines` are current.
    pub(super) fn run_kp_typeset(
        &mut self,
        k: &mut KernelHandle<'_>,
    ) -> Result<(), TypesetError> {
        plump_kernel::perf_begin!(_kp_t0);

        let Some(fs) = self.fonts.as_ref().copied() else {
            return Err(TypesetError::EmptyChapter);
        };

        // ch_cache empty but the chapter IS in the bundle: stream via
        // BundleByteSource (sliding 4 KB window over SD reads). Only
        // truly-empty chapters (no bundle entry, no SD content) bail.
        let ch_idx_check = self.epub.chapter as usize;
        let ch_size_in_bundle = if self.is_epub
            && self.epub.chapters_cached
            && ch_idx_check < smol_epub::cache::MAX_CACHE_CHAPTERS
        {
            self.epub.chapter_table[ch_idx_check].1
        } else {
            0
        };
        if self.epub.ch_cache.is_empty() && ch_size_in_bundle == 0 {
            return Err(TypesetError::EmptyChapter);
        }

        // KPDIAG-D: probe FontSet at K-P entry. If these advances are
        // tiny (~1-2 px) but the renderer draws at ~8-12 px, K-P will
        // measure lines ~6x narrower than reality, fail tolerance on
        // every interior break, and collapse paragraphs to one line.
        log::debug!(
            "KPDIAG fonts size_idx={} text_w={} line_h={} ascent={} adv_A_reg={} adv_A_bold={} adv_a_reg={} adv_M_reg={} adv_i_reg={} adv_space_reg={} adv_space_bold={} adv_period_reg={} adv_emdash_reg={} adv_apos_reg={} adv_smartapos_reg={}",
            self.book_font_size_idx,
            self.text_w,
            self.font_line_h,
            self.font_ascent,
            fs.advance('A', crate::fonts::Style::Regular),
            fs.advance('A', crate::fonts::Style::Bold),
            fs.advance('a', crate::fonts::Style::Regular),
            fs.advance('M', crate::fonts::Style::Regular),
            fs.advance('i', crate::fonts::Style::Regular),
            fs.advance(' ', crate::fonts::Style::Regular),
            fs.advance(' ', crate::fonts::Style::Bold),
            fs.advance('.', crate::fonts::Style::Regular),
            fs.advance('\u{2014}', crate::fonts::Style::Regular),
            fs.advance('\'', crate::fonts::Style::Regular),
            fs.advance('\u{2019}', crate::fonts::Style::Regular),
        );

        // the book's language picks the hyphenation patterns; none means
        // only soft hyphens and explicit hyphens can break a word
        let lang = smol_epub::hyphen::lang_from_tag(self.epub.meta.lang());
        let mut pipeline = LayoutPipeline::new(lang);
        let mut out_lines: Vec<LineLayout> = Vec::new();
        let mut out_image_blocks: Vec<u8> = Vec::new();

        // Bounded pre-allocation: avoid OOM mid-typeset.
        let _ = out_lines.try_reserve(64);
        let _ = out_image_blocks.try_reserve(64);

        let budget = ImageBudget {
            text_w: self.text_w as u16,
            inline_cap: inline_img_max_h(self.text_area_h),
            line_h: self.font_line_h,
        };

        // Image-height hint that peeks JPEG/PNG dimensions for any
        // <img> tag missing width/height attrs, so K-P reserves
        // exactly the lines the decoder will fill.
        let ch_idx = self.epub.chapter as usize;
        let ch_zip_idx = self.epub.spine.items[ch_idx] as usize;
        let ch_path = self.epub.zip.entry_name(ch_zip_idx);
        let ch_dir = ch_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let cache_dir = self.epub.cache_dir_str();
        let fname = self.filename;
        let epub_name = fname.as_str();
        let text_w_u32 = self.text_w as u32;
        let text_area_h = self.text_area_h;
        let name_hash = self.epub.name_hash;
        let ch_base = self.epub.chapter_table[ch_idx].0;
        let use_bundle_source = self.epub.ch_cache.is_empty();

        // Single immutable SD borrow shared between peek and the
        // streaming source. Both ChapterImagePeek and BundleByteSource
        // hold &SdStorage (not &mut KernelHandle), so they coexist as
        // disjoint immutable borrows of *k.
        let sd_ref = k.sd();

        let mut peek = super::images::ChapterImagePeek::new(
            sd_ref,
            &self.epub.zip,
            epub_name,
            ch_dir,
            cache_dir,
            text_w_u32,
            text_area_h,
        );

        // Pick source. Each branch scopes its own source for the typeset
        // loop, then the loop body is identical, so we run it inline.
        if use_bundle_source {
            let mut src = BundleByteSource::new(sd_ref, name_hash, ch_base, ch_size_in_bundle);
            let mut scanner = Events::new(&mut src);
            loop {
                let outcome = pipeline.step(
                    &mut scanner,
                    &fs,
                    budget,
                    &mut peek,
                    &mut out_lines,
                    &mut out_image_blocks,
                )?;
                if matches!(outcome, StepOutcome::Done) {
                    break;
                }
            }
        } else {
            let mut src = SliceSource(&self.epub.ch_cache);
            let mut scanner = Events::new(&mut src);
            loop {
                let outcome = pipeline.step(
                    &mut scanner,
                    &fs,
                    budget,
                    &mut peek,
                    &mut out_lines,
                    &mut out_image_blocks,
                )?;
                if matches!(outcome, StepOutcome::Done) {
                    break;
                }
            }
        }
        drop(peek);

        if out_lines.is_empty() {
            // chapter contained only markers / whitespace; emit a single
            // empty page so navigation still works.
            self.pg.clear_kp_layout();
            self.pg.kp_pages.push(PageLayout::EMPTY);
            return Ok(());
        }

        let mut pages: Vec<PageLayout> = Vec::new();
        let _ = pages.try_reserve(8);
        let spacing = PageSpacing {
            em_px: fs.em_px(),
            line_h: self.font_line_h,
        };
        if let Err(e) = paginate::paginate(
            &out_lines,
            self.max_lines,
            &out_image_blocks,
            spacing,
            &mut pages,
        ) {
            log::warn!("reader: paginate failed: {:?}", e);
            return Err(TypesetError::EmptyChapter);
        }

        plump_kernel::perf_event!(
            "reader",
            "preindex.kp paragraphs={} pages={} lines={} hyphenated={} fallbacks={} elapsed_ms={}",
            pipeline.paragraphs,
            pages.len(),
            out_lines.len(),
            pipeline.hyphenated_lines,
            pipeline.fallback_count,
            _kp_t0.elapsed().as_millis()
        );

        // publish
        self.pg.chapter_lines = out_lines;
        self.pg.kp_pages = pages;
        self.pg.image_block_lines = out_image_blocks;

        // mirror page table into the legacy offsets array so the
        // restore-position / locate-page-for-offset / progress-bar
        // helpers keep working without per-call branching. This will
        // go away with R4 (slim PageState) once everything reads
        // kp_pages directly.
        let n = self.pg.kp_pages.len().min(MAX_PAGES);
        for i in 0..n {
            self.pg.offsets[i] = self.pg.kp_pages[i].start_byte;
        }
        self.pg.total_pages = n.max(1);
        self.pg.fully_indexed = true;

        Ok(())
    }

    /// Dispatch the page-load between K-P and greedy. Used by NeedPage
    /// after step 6 wiring lands.
    pub(super) fn load_page_dispatched(
        &mut self,
        k: &mut KernelHandle<'_>,
    ) -> crate::error::Result<()> {
        if self.pg.has_kp_layout() && self.pg.page < self.pg.kp_pages.len() {
            self.load_page_from_kp(k)
        } else {
            self.load_and_prefetch(k)
        }
    }

    /// K-P-driven page loader. Reads the precomputed `LineLayout`s
    /// for the current page out of `pg.chapter_lines`, copies the
    /// chapter byte slice into `pg.buf`, and translates LineLayout
    /// records into LineSpan records (page-buffer-relative offsets)
    /// for the unmodified renderer.
    ///
    /// Caller invariants: `pg.has_kp_layout()` is true and `pg.page <
    /// pg.kp_pages.len()`.
    pub(super) fn load_page_from_kp(
        &mut self,
        k: &mut KernelHandle<'_>,
    ) -> crate::error::Result<()> {
        plump_kernel::perf_begin!(_lp_t0);

        let page = self.pg.kp_pages[self.pg.page];
        let first = page.first_line as usize;
        let count = page.line_count as usize;
        let start_byte = page.start_byte as usize;
        let end_byte = page.end_byte as usize;
        // expand end_byte slightly to include the breakpoint byte itself,
        // since LineLayout.end_byte is the BREAK position (exclusive).
        let end_byte = end_byte.max(start_byte);

        // copy chapter bytes for this page into pg.buf for the renderer.
        // ch_cache is the source of truth; if it was dropped under
        // memory pressure, fall through to the on-disk bundle read.
        let n = if !self.epub.ch_cache.is_empty() {
            let ch_len = self.epub.ch_cache.len();
            let src_end = end_byte.min(ch_len);
            let src_start = start_byte.min(src_end);
            let want = (src_end - src_start).min(PAGE_BUF);
            self.pg.buf[..want]
                .copy_from_slice(&self.epub.ch_cache[src_start..src_start + want]);
            want
        } else {
            // ch_cache missing: re-read from bundle. uses the same chapter
            // base used by the cache-hit path in load_and_prefetch above.
            let ch = self.epub.chapter as usize;
            let ch_base = self.epub.chapter_table[ch].0;
            plump_kernel::kernel::bundle::read_at(
                k.sd(),
                self.epub.name_hash,
                ch_base + start_byte as u32,
                &mut self.pg.buf,
            )?
        };
        self.pg.buf_len = n;

        // translate LineLayout records into per-page LineSpans.
        self.pg.line_count = 0;
        let last = (first + count).min(self.pg.chapter_lines.len());
        for idx in first..last {
            if self.pg.line_count >= LINES_PER_PAGE {
                break;
            }
            let ll = self.pg.chapter_lines[idx];
            let span = linelayout_to_span(&ll, start_byte as u32, n as u32);
            self.pg.lines[self.pg.line_count] = span;
            self.pg.line_count += 1;
        }

        // image height prescan so render-side image decode picks the
        // right dimensions (the K-P paginator already reserved the
        // line slots, but the byte-stream image marker still needs
        // its inline header re-read for actual decode metadata).
        self.prescan_image_heights(k, n);
        self.build_page_runs();
        self.decode_page_images(k);

        // disable greedy prefetch when K-P is driving navigation.
        self.pg.prefetch_page = NO_PREFETCH;
        self.pg.prefetch_len = 0;

        plump_kernel::perf_event!(
            "reader",
            "load_kp page={} bytes={} lines={} elapsed_ms={}",
            self.pg.page,
            n,
            self.pg.line_count,
            _lp_t0.elapsed().as_millis()
        );
        Ok(())
    }

    pub(super) fn scan_to_last_page(
        &mut self,
        k: &mut KernelHandle<'_>,
    ) -> crate::error::Result<()> {
        // K-P populates fully_indexed in one shot, so the discovery
        // loop only fires on the greedy fallback path.
        while !self.pg.fully_indexed && self.pg.total_pages < MAX_PAGES {
            self.pg.page = self.pg.total_pages - 1;
            self.load_page_dispatched(k)?;
            if self.pg.page + 1 < self.pg.total_pages {
                self.pg.page += 1;
            } else {
                break;
            }
        }
        if self.pg.total_pages > 0 {
            self.pg.page = self.pg.total_pages - 1;
        }
        self.pg.prefetch_page = NO_PREFETCH;
        self.load_page_dispatched(k)
    }

    pub(super) fn page_forward(&mut self) -> bool {
        if self.state != State::Ready {
            return false;
        }

        if self.pg.page + 1 < self.pg.total_pages {
            self.pg.page += 1;
            self.queue_position_change(PendingPositionChange::PageTurn);
            self.state = State::NeedPage;
            return true;
        }

        if self.is_epub
            && self.pg.fully_indexed
            && (self.epub.chapter as usize + 1) < self.epub.spine.len()
        {
            self.epub.chapter += 1;
            self.queue_position_change(PendingPositionChange::PageTurn);
            self.goto_last_page = false;
            self.state = State::NeedIndex;
            return true;
        }

        false
    }

    pub(super) fn page_backward(&mut self) -> bool {
        if self.state != State::Ready {
            return false;
        }

        if self.pg.page > 0 {
            self.pg.page -= 1;
            self.queue_position_change(PendingPositionChange::PageTurn);
            self.state = State::NeedPage;
            return true;
        }

        if self.is_epub && self.epub.chapter > 0 {
            self.epub.chapter -= 1;
            self.queue_position_change(PendingPositionChange::PageTurn);
            self.goto_last_page = true;
            self.state = State::NeedIndex;
            return true;
        }

        false
    }

    // next chapter (EPUB) or +10 pages (TXT)
    pub(super) fn jump_forward(&mut self) -> bool {
        if self.state != State::Ready {
            return false;
        }
        if self.is_epub {
            if (self.epub.chapter as usize + 1) < self.epub.spine.len() {
                self.epub.chapter += 1;
                // jump: update RECENT but don't count as page turn
                self.queue_position_change(PendingPositionChange::Jump);
                self.goto_last_page = false;
                self.state = State::NeedIndex;
                return true;
            }
        } else {
            let last = if self.pg.total_pages > 0 {
                self.pg.total_pages - 1
            } else {
                0
            };
            let target = (self.pg.page + 10).min(last);
            if target != self.pg.page {
                self.pg.page = target;
                // jump: update RECENT but don't count as page turn
                self.queue_position_change(PendingPositionChange::Jump);
                self.state = State::NeedPage;
                return true;
            }
        }
        false
    }

    // prev chapter (EPUB) or -10 pages (TXT)
    pub(super) fn jump_backward(&mut self) -> bool {
        if self.state != State::Ready {
            return false;
        }
        if self.is_epub {
            if self.epub.chapter > 0 {
                self.epub.chapter -= 1;
                // jump: update RECENT but don't count as page turn
                self.queue_position_change(PendingPositionChange::Jump);
                self.goto_last_page = false;
                self.state = State::NeedIndex;
                return true;
            }
        } else {
            let target = self.pg.page.saturating_sub(10);
            if target != self.pg.page {
                self.pg.page = target;
                // jump: update RECENT but don't count as page turn
                self.queue_position_change(PendingPositionChange::Jump);
                self.state = State::NeedPage;
                return true;
            }
        }
        false
    }
}

// ── page runs: one decode per page ──────────────────────────────────
//
// a page is decoded once, when it is loaded: every line's vertical
// position, its natural width and gap count, its justification, and
// the style runs the strip passes draw from. the twelve strip passes
// then only blit glyphs; no marker is decoded at draw time.

/// Result of measuring a single line span for justification.
#[derive(Clone, Copy, Default)]
pub(in crate::apps) struct LineMeasure {
    /// Rendered width in pixels (excluding trailing whitespace).
    pub width: u32,
    /// Number of stretchable inter-word gaps (ASCII spaces only; NBSP excluded).
    pub gaps: u16,
}

/// Per-gap justification for one line: pixels added to every inter-word
/// gap (negative to shrink) and how many leading gaps take one pixel
/// more (or fewer) to spread the remainder.
///
/// stretch and shrink are decided independently:
///
/// - stretch is aesthetic (justify-vs-left preference): only fires when
///   the user picked justified text AND this is a mid-paragraph soft-wrap.
/// - shrink is layout-driven: fires whenever K-P signalled it via
///   `extra`, regardless of user alignment preference and regardless of
///   paragraph-end status. without this, single-line shrink-fit
///   paragraphs overflow the column (Stories of Your Life's dense prose,
///   Leviathan ch5's "Using the Knight..." paragraph).
///
/// headings and explicit-align lines skip both directions.
fn justify_params(
    text_alignment: u8,
    span: &LineSpan,
    m: LineMeasure,
    avail: i32,
    space_w: i32,
) -> (i32, i32) {
    let is_heading = span.flags & LineSpan::FLAG_HEADING != 0;
    let explicit = span.is_explicit_align();
    let can_stretch = text_alignment == 1 && span.is_soft_wrap() && !is_heading && !explicit;
    let can_shrink = span.extra_is_shrink() && !is_heading && !explicit;
    if !(can_stretch || can_shrink) {
        return (0, 0);
    }
    let spare = avail - m.width as i32;
    let gaps = m.gaps as i32;
    if gaps < 2 {
        return (0, 0);
    }
    if can_stretch && spare >= 3 && spare * 5 < avail * 2 {
        // stretch: distribute positive spare across gaps, capped at 3x
        // natural space width per gap so a sparse line doesn't open rivers
        let per = spare / gaps;
        if per <= space_w.saturating_mul(3) {
            (per, spare - per * gaps)
        } else {
            (0, 0)
        }
    } else if can_shrink && spare <= -1 {
        // shrink: K-P decided this paragraph fits by squeezing inter-word
        // spaces. honor it up to K-P's own per-glue shrink budget
        // (space/2, matching `Item::glue`'s shrink in items.rs and
        // `RATIO_SHRINK_MAX` in breaker.rs)
        let per = spare / gaps; // signed; rounds toward 0
        let floor = -(space_w / 2).max(1);
        if per >= floor {
            (per, spare - per * gaps)
        } else {
            // K-P / renderer disagree on widths beyond the shrink budget:
            // draw at natural width rather than crush letters together
            (0, 0)
        }
    } else {
        (0, 0)
    }
}

/// natural width of one word's bytes in `style`, pair kerning between
/// its consecutive glyphs included (the same rule as the K-P measure
/// and the draw loop). soft hyphens are zero-width and end the pair
/// (the fonts ship SHY as a visible glyph, so the decoder-side policy
/// is to skip it; build.rs excludes it too)
fn measure_bytes(bytes: &[u8], fs: &fonts::FontSet, style: fonts::Style) -> u32 {
    let mut w: i32 = 0;
    let mut prev: Option<char> = None;
    let mut j = 0usize;
    while j < bytes.len() {
        let b = bytes[j];
        let (ch, len) = if b >= 0xC0 {
            decode_utf8_char(bytes, j)
        } else if !(bitmap::FIRST_CHAR..0x80).contains(&b) {
            prev = None;
            j += 1;
            continue;
        } else {
            (b as char, 1)
        };
        j += len.max(1);
        if ch == '\u{00AD}' || ch == ' ' {
            if ch == ' ' {
                w += fs.advance(' ', style) as i32;
            }
            prev = None;
            continue;
        }
        if let Some(p) = prev {
            w += fs.kern(p, ch, style) as i32;
        }
        w += fs.advance(ch, style) as i32;
        prev = Some(ch);
    }
    w.max(0) as u32
}

impl ReaderApp {
    /// Decode the loaded page once: per-line vertical positions, natural
    /// widths and gap counts, justification, and the style runs the
    /// strip passes draw from.
    pub(super) fn build_page_runs(&mut self) {
        self.pg.run_count = 0;
        let Some(fs) = self.fonts else {
            for i in 0..self.pg.line_count {
                self.pg.line_y[i] = (i as u32 * self.font_line_h as u32) as u16;
                self.pg.run_len[i] = 0;
            }
            return;
        };
        let em = fs.em_px();
        let line_h = self.font_line_h;
        let text_w = self.text_w as i32;
        let margin = self.text_margin as i32;
        let alignment = self.text_alignment;
        let buf_len = self.pg.buf_len;
        let pg = &mut self.pg;

        let mut y_q: u32 = 0;
        for i in 0..pg.line_count {
            let span = pg.lines[i];
            if i > 0 {
                y_q += super::layout::gap_quarters(span.gap_qem(), em, line_h) as u32;
            }
            pg.line_y[i] = ((y_q * line_h as u32) / 4) as u16;
            y_q += 4;
            pg.run_first[i] = pg.run_count as u16;
            pg.run_len[i] = 0;
            pg.line_just[i] = (0, 0);
            pg.line_x_end[i] = 0;
            if span.is_image() || span.len == 0 {
                continue;
            }
            let start = span.start as usize;
            let end = (start + span.len as usize).min(buf_len);
            if start >= end {
                continue;
            }

            // pass 1: runs at natural width. a run is a byte-contiguous
            // stretch of text events under one style; `x` holds its
            // natural width until pass 2 places it
            let first_run = pg.run_count;
            let mut width: u32 = 0;
            let mut gaps: u16 = 0;
            let mut trailing: u32 = 0;
            {
                let buf = &pg.buf;
                let runs = &mut pg.runs;
                let mut src = SliceSource(&buf[..end]);
                let mut ev = Events::resume(&mut src, start, span.start_style(), BlockProps::DEFAULT);
                let mut n_runs = 0usize;
                while let Some(e) = ev.next_event() {
                    let (s, e_end, style, adv, is_gap) = match e {
                        Event::Word { start, end, style } => {
                            let fsty = fonts::Style::from_markup(style);
                            let w = measure_bytes(&buf[start as usize..end as usize], &fs, fsty);
                            (start, end, style, w, false)
                        }
                        Event::Space { start, end, style } => {
                            let w = fs.advance(' ', fonts::Style::from_markup(style)) as u32;
                            (start, end, style, w, true)
                        }
                        Event::Nbsp { start, end, style } => {
                            let w = fs.advance(' ', fonts::Style::from_markup(style)) as u32;
                            (start, end, style, w, false)
                        }
                        Event::SoftHyphen { start, end, style } => (start, end, style, 0, false),
                        // block records and unknown markers sit between
                        // words; breaks never occur inside a line
                        Event::Block { .. } | Event::Unknown { .. } => continue,
                        _ => break,
                    };
                    width += adv;
                    if is_gap {
                        trailing += adv;
                    } else {
                        trailing = 0;
                    }
                    let packed = style.pack();
                    let extend = n_runs > 0 && {
                        let last = &runs[pg.run_count - 1];
                        (last.style == packed && last.start as u32 + last.len as u32 == s)
                            || n_runs >= MAX_RUNS_PER_LINE
                            || pg.run_count >= MAX_PAGE_RUNS
                    };
                    if extend {
                        let last = &mut runs[pg.run_count - 1];
                        last.len = (e_end - last.start as u32) as u16;
                        last.x = last.x.saturating_add(adv as i16);
                    } else {
                        runs[pg.run_count] = Run {
                            start: s as u16,
                            len: (e_end - s) as u16,
                            x: adv as i16,
                            style: packed,
                            gap0: gaps.min(u8::MAX as u16) as u8,
                        };
                        pg.run_count += 1;
                        n_runs += 1;
                    }
                    if is_gap {
                        gaps = gaps.saturating_add(1);
                    }
                }
            }
            pg.run_len[i] = (pg.run_count - first_run).min(u8::MAX as usize) as u8;

            // a line that ends inside a word took a discretionary break
            // (soft hyphen or dictionary): it gets the hyphen glyph K-P
            // measured into it. an explicit hyphen already sits there
            let hyphenated = end < buf_len
                && is_word_byte(pg.buf[end])
                && is_word_byte(pg.buf[end - 1])
                && pg.buf[end - 1] != b'-';
            pg.line_hyphen[i] = hyphenated;
            let hyphen_w = if hyphenated {
                let last_style = if pg.run_count > first_run {
                    let r = pg.runs[pg.run_count - 1];
                    fonts::Style::from_markup(markup::Style::unpack(r.style))
                } else {
                    span.style()
                };
                fs.advance('-', last_style) as u32
            } else {
                0
            };

            let m = LineMeasure {
                width: width.saturating_sub(trailing) + hyphen_w,
                gaps,
            };

            // pass 2: place the runs. the left and first-line indents
            // narrow the column; centred and right-aligned lines shift by
            // the spare; justified lines spread it over the gaps
            let left_px = INDENT_PX as i32 * span.left_levels() as i32;
            let first_px = super::layout::indent_px(span.first_indent_qem(), em) as i32;
            let avail = (text_w - left_px - first_px).max(0);
            let space_w = fs.advance(' ', span.style()) as i32;
            let (per, rem) = justify_params(alignment, &span, m, avail, space_w);
            pg.line_just[i] = (per as i16, rem as i16);
            let align_offset = if span.is_explicit_align() {
                let spare = (avail - m.width as i32).max(0);
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
            let mut cx = margin + left_px + first_px + align_offset;
            let last_run = pg.run_count;
            for r in first_run..last_run {
                let g0 = pg.runs[r].gap0 as i32;
                let g_end = if r + 1 < last_run {
                    pg.runs[r + 1].gap0 as i32
                } else {
                    gaps as i32
                };
                let run = &mut pg.runs[r];
                let mut w = run.x as i32 + per * (g_end - g0);
                // the remainder goes one pixel at a time to the leading gaps
                if rem > 0 {
                    w += (rem.min(g_end) - g0).max(0);
                } else if rem < 0 {
                    w -= ((-rem).min(g_end) - g0).max(0);
                }
                run.x = cx.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                cx += w;
            }
            pg.line_x_end[i] = cx.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    }
}

/// a byte that belongs to a word in the markup stream: printable, not a
/// space, not a marker (UTF-8 lead and continuation bytes included)
#[inline]
fn is_word_byte(b: u8) -> bool {
    b > b' ' && b != smol_epub::markup::MARKER
}

/// drop trailing carriage returns from a line range
pub(super) fn trim_trailing_cr(buf: &[u8], start: usize, end: usize) -> usize {
    let mut e = end;
    while e > start && buf[e - 1] == b'\r' {
        e -= 1;
    }
    e
}

// ── LineLayout → LineSpan ──────────────────────────────────────────

fn linelayout_to_span(ll: &LineLayout, page_start_byte: u32, buf_len: u32) -> LineSpan {
    // image lines: filler entries carry start=0/len=0; origin entries
    // cover the IMG_REF record. both encodings use chapter-relative
    // bytes; translate to page-buffer-relative the same way.
    let raw_start = ll.start_byte.saturating_sub(page_start_byte);
    let raw_len = ll.end_byte.saturating_sub(ll.start_byte);
    let start = raw_start.min(buf_len) as u16;
    let len = raw_len.min(buf_len.saturating_sub(start as u32)) as u16;

    let mut flags: u8 = ll.flags & (LineLayout::FLAG_BOLD | LineLayout::FLAG_ITALIC);
    if ll.flags & LineLayout::FLAG_HEADING != 0 {
        flags |= LineSpan::FLAG_HEADING;
        // heading-tier translation: LineLayout uses the same bit pattern
        // (HLEVEL_H1 etc) so we can copy bits 6-7 verbatim.
        flags |= ll.flags & LineLayout::HLEVEL_MASK;
    }
    if ll.is_image() {
        flags |= LineSpan::FLAG_IMAGE;
    }
    // Map paragraph-end / page-break-before flags into LineSpan's
    // end-kind field. The renderer consults END_MASK to decide
    // whether a line may justify (it doesn't justify END_HARD lines).
    if ll.is_paragraph_end() {
        flags |= LineSpan::END_HARD;
    } else {
        flags |= LineSpan::END_SOFT;
    }

    // indent and align bytes share their encoding between the two types
    LineSpan {
        start,
        len,
        flags,
        indent: ll.indent,
        align: ll.align,
        extra: ll.extra,
    }
}

// ── greedy wrapper (.txt files and the K-P fallback) ────────────────

/// Greedy first-fit line wrapper over the markup stream in `buf[..n]`.
///
/// Returns `(consumed, line_count)`: the byte where the next page
/// starts and how many lines were filled. Paragraph gaps from block
/// records are ignored here; a paragraph break produces one empty
/// line the way plain text does, so `.txt` files keep their look and
/// the K-P fallback stays readable.
#[allow(clippy::too_many_arguments)]
pub(super) fn wrap_proportional(
    buf: &[u8],
    n: usize,
    fonts: &fonts::FontSet,
    lines: &mut [LineSpan],
    max_lines: usize,
    max_width_px: u32,
    img_heights: &[u16],
    line_h: u16,
    em_px: u16,
) -> (usize, usize) {
    let max_l = max_lines.min(lines.len());
    let mut src = SliceSource(&buf[..n]);
    let mut ev = Events::new(&mut src);

    let mut count = 0usize;
    let mut line_start = 0usize;
    let mut line_style = ev.style();
    let mut cursor: u32 = 0;
    // last break opportunity: (line end, next line start, cursor at the
    // opportunity, style at the next line start)
    let mut brk: Option<(usize, usize, u32, Style)> = None;
    let mut block = BlockProps::DEFAULT;
    let mut first_line = true;
    let mut img_idx = 0usize;
    let mut last_end = 0usize;

    let width_for = |block: &BlockProps, first: bool| -> u32 {
        let left = INDENT_PX * block.left as u32;
        let first_px = if first {
            super::layout::indent_px(block.text_indent_qem, em_px) as u32
        } else {
            0
        };
        max_width_px.saturating_sub(left + first_px)
    };

    // push one line; returns false when the page is full
    macro_rules! emit {
        ($start:expr, $end:expr, $end_kind:expr, $first:expr) => {{
            let s = $start;
            let e = ($end).max(s);
            lines[count] = LineSpan {
                start: s as u16,
                len: (e - s) as u16,
                flags: LineSpan::flags_for(line_style, $end_kind),
                indent: LineLayout::pack_indent(
                    block.left,
                    if $first { block.text_indent_qem } else { 0 },
                ),
                align: LineLayout::pack_align(block.align, 0, line_style),
                extra: 0,
            };
            count += 1;
            count < max_l
        }};
    }

    loop {
        let Some(e) = ev.next_event() else { break };
        match e {
            Event::Word { start, end, style }
            | Event::Nbsp { start, end, style } => {
                let w = if matches!(e, Event::Nbsp { .. }) {
                    fonts.advance(' ', fonts::Style::from_markup(style)) as u32
                } else {
                    measure_bytes(&buf[start as usize..end as usize], fonts, fonts::Style::from_markup(style))
                };
                let max_w = width_for(&block, first_line);
                if cursor + w > max_w && cursor > 0 {
                    // overflow: break at the last opportunity, else ahead
                    // of this word (an overlong word overflows its line)
                    let (line_end, next_start, cursor_at, next_style) = match brk {
                        Some(b) => b,
                        None => (start as usize, start as usize, cursor, style),
                    };
                    let more = emit!(line_start, line_end, LineSpan::END_SOFT, first_line);
                    first_line = false;
                    line_start = next_start;
                    if !more {
                        return (line_start, count);
                    }
                    cursor -= cursor_at;
                    line_style = next_style;
                    brk = None;
                }
                cursor += w;
                last_end = end as usize;
            }

            Event::Space { start, end, style } => {
                let adv = fonts.advance(' ', fonts::Style::from_markup(style)) as u32;
                cursor += adv;
                last_end = end as usize;
                brk = Some((start as usize, end as usize, cursor, style));
                if cursor > width_for(&block, first_line) {
                    let more = emit!(line_start, start as usize, LineSpan::END_SOFT, first_line);
                    first_line = false;
                    line_start = end as usize;
                    if !more {
                        return (line_start, count);
                    }
                    cursor = 0;
                    line_style = style;
                    brk = None;
                }
            }

            Event::SoftHyphen { end, style, .. } => {
                // zero-width break opportunity after the hyphen point
                brk = Some((end as usize, end as usize, cursor, style));
                last_end = end as usize;
            }

            Event::HardBreak { start, end } => {
                let more = emit!(line_start, start as usize, LineSpan::END_HARD, first_line);
                first_line = false;
                line_start = end as usize;
                if !more {
                    return (line_start, count);
                }
                cursor = 0;
                brk = None;
                block = ev.block();
                line_style = ev.style();
            }

            Event::ParagraphBreak { start, end } => {
                let had_text = (start as usize) > line_start;
                if had_text || count > 0 {
                    let more = emit!(line_start, start as usize, LineSpan::END_HARD, first_line);
                    if !more {
                        return (end as usize, count);
                    }
                }
                // the blank line between paragraphs, skipped at page top
                if count > 0 {
                    line_start = end as usize;
                    block = BlockProps::DEFAULT;
                    let more = emit!(line_start, line_start, LineSpan::END_HARD, false);
                    if !more {
                        return (line_start, count);
                    }
                }
                line_start = end as usize;
                cursor = 0;
                brk = None;
                block = BlockProps::DEFAULT;
                first_line = true;
                line_style = ev.style();
            }

            Event::PageBreak { start, end } => {
                // explicit page break: end the current page at the marker
                // unless the chapter just started (no content yet)
                let has_content = (start as usize) > line_start || count > 0;
                if has_content {
                    if (start as usize) > line_start {
                        let _ = emit!(line_start, start as usize, LineSpan::END_HARD, first_line);
                    }
                    return (end as usize, count);
                }
                line_start = end as usize;
                line_style = ev.style();
            }

            Event::ThematicBreak { end, .. } => {
                // the stripper already put a paragraph break around it
                if (end as usize) > line_start && cursor == 0 {
                    line_start = end as usize;
                    line_style = ev.style();
                }
            }

            Event::Image(img) => {
                if (img.start as usize) > line_start && cursor > 0 {
                    let more = emit!(line_start, img.start as usize, LineSpan::END_HARD, first_line);
                    if !more {
                        return (img.start as usize, count);
                    }
                }
                // prefer the pre-scanned height (peeked from the image
                // header by images.rs); fall back to DEFAULT_IMG_H
                let img_h = if img_idx < img_heights.len() && img_heights[img_idx] > 0 {
                    img_heights[img_idx]
                } else {
                    DEFAULT_IMG_H
                };
                img_idx += 1;
                let img_lines = img_h.div_ceil(line_h.max(1)).max(1) as usize;

                // page-fit: if the image needs more lines than the page has
                // left AND text is already on this page, push the whole
                // image to the next page. an image bigger than the page is
                // emitted anyway and clipped at the bottom
                if count > 0 && count + img_lines > max_l {
                    return (img.start as usize, count);
                }

                if count < max_l {
                    lines[count] = LineSpan {
                        start: img.start as u16,
                        len: (img.end - img.start) as u16,
                        flags: LineSpan::FLAG_IMAGE,
                        indent: 0,
                        align: LineSpan::ALIGN_DEFAULT,
                        extra: 0,
                    };
                    count += 1;
                }
                for _ in 1..img_lines {
                    if count >= max_l {
                        break;
                    }
                    lines[count] = LineSpan {
                        start: 0,
                        len: 0,
                        flags: LineSpan::FLAG_IMAGE,
                        indent: 0,
                        align: LineSpan::ALIGN_DEFAULT,
                        extra: 0,
                    };
                    count += 1;
                }

                line_start = img.end as usize;
                last_end = line_start;
                cursor = 0;
                brk = None;
                block = BlockProps::DEFAULT;
                first_line = true;
                line_style = ev.style();
                if count >= max_l {
                    return (line_start, count);
                }
            }

            Event::Block { block: b, .. } => {
                block = b;
                first_line = true;
            }

            Event::Unknown { .. } => {}
        }
    }

    if last_end > line_start && count < max_l {
        let _ = emit!(line_start, last_end, LineSpan::END_BUFFER, first_line);
    }

    (n, count)
}

// by `run_kp_typeset` when the chapter is too big to fit in `ch_cache`
// (largest contiguous heap region on ESP32-C3 is ~108 KB; chapters
// past that — e.g. Stories of Your Life "Seventy-Two Letters" at
// ~109 KB stripped — would otherwise OOM at allocation).
//
// Mirrors `smol_epub::jpeg::ChunkReader`'s pattern: small fixed-size
// internal buffer, refills on miss, sequential access is hot. The K-P
// scanner's access pattern is mostly sequential with occasional small
// re-reads for word measurement (which usually stay within the same
// 4 KB window), so refill rate is roughly `ceil(chapter_size / 4096)`
// SD reads per typeset pass.

const BUNDLE_SOURCE_CHUNK: usize = 4096;

pub(super) struct BundleByteSource<'a> {
    sd: &'a plump_kernel::board::SdStorage,
    name_hash: u32,
    /// Absolute offset into the bundle of byte 0 of this source.
    base: u32,
    /// Total bytes addressable.
    total_len: u32,
    buf: [u8; BUNDLE_SOURCE_CHUNK],
    /// Source-relative offset of `buf[0]` (chunk-aligned).
    buf_offset: u32,
    /// Bytes valid in `buf` (always <= BUNDLE_SOURCE_CHUNK).
    buf_valid: usize,
}

impl<'a> BundleByteSource<'a> {
    pub(super) fn new(
        sd: &'a plump_kernel::board::SdStorage,
        name_hash: u32,
        base: u32,
        total_len: u32,
    ) -> Self {
        Self {
            sd,
            name_hash,
            base,
            total_len,
            buf: [0u8; BUNDLE_SOURCE_CHUNK],
            buf_offset: 0,
            buf_valid: 0,
        }
    }

    /// Ensure the internal window covers `pos`. Returns `true` when the
    /// position is in-window after the call. Refills are chunk-aligned
    /// so consecutive sequential reads cost one SD read per chunk.
    fn ensure_window(&mut self, pos: u32) -> bool {
        if self.buf_valid > 0
            && pos >= self.buf_offset
            && pos < self.buf_offset + self.buf_valid as u32
        {
            return true;
        }
        if pos >= self.total_len {
            return false;
        }
        let chunk = pos & !((BUNDLE_SOURCE_CHUNK - 1) as u32);
        let want = (self.total_len - chunk).min(BUNDLE_SOURCE_CHUNK as u32) as usize;
        match plump_kernel::kernel::bundle::read_at(
            self.sd,
            self.name_hash,
            self.base + chunk,
            &mut self.buf[..want],
        ) {
            Ok(n) if n > 0 => {
                self.buf_offset = chunk;
                self.buf_valid = n;
                pos < self.buf_offset + self.buf_valid as u32
            }
            _ => {
                self.buf_valid = 0;
                false
            }
        }
    }
}

impl ByteSource for BundleByteSource<'_> {
    #[inline]
    fn len(&self) -> usize {
        self.total_len as usize
    }

    fn byte_at(&mut self, pos: usize) -> Option<u8> {
        if pos as u32 >= self.total_len {
            return None;
        }
        if !self.ensure_window(pos as u32) {
            return None;
        }
        Some(self.buf[(pos as u32 - self.buf_offset) as usize])
    }

    fn read_into(&mut self, pos: usize, dst: &mut [u8]) -> usize {
        let mut written = 0usize;
        let mut cur = pos as u32;
        while written < dst.len() && cur < self.total_len {
            if !self.ensure_window(cur) {
                break;
            }
            let buf_off = (cur - self.buf_offset) as usize;
            let avail = self.buf_valid - buf_off;
            let to_copy = (dst.len() - written).min(avail);
            dst[written..written + to_copy]
                .copy_from_slice(&self.buf[buf_off..buf_off + to_copy]);
            written += to_copy;
            cur += to_copy as u32;
        }
        written
    }
}
