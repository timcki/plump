// text wrapping, page navigation, and load/prefetch

use alloc::vec::Vec;

use smol_epub::html_strip::{
    ALIGN_CENTER, ALIGN_JUSTIFY, ALIGN_LEFT, ALIGN_RESET, ALIGN_RIGHT, BOLD_OFF, BOLD_ON, H1_OFF,
    H1_ON, H2_OFF, H2_ON, H3_OFF, H3_ON, H4_OFF, H4_ON, H5_OFF, H5_ON, H6_OFF, H6_ON, HEADING_OFF,
    HEADING_ON, IMG_HEADER_LEN, IMG_REF, ITALIC_OFF, ITALIC_ON, MARKER, PAGE_BREAK, QUOTE_OFF,
    QUOTE_ON,
};

use crate::fonts;
use crate::fonts::bitmap::{self, FIRST_CHAR};
use crate::kernel::KernelHandle;

use super::layout::pipeline::{LayoutPipeline, StepOutcome, TypesetError};
use super::layout::scan::MarkupScanner;
use super::layout::{paginate, LineLayout, PageLayout};

use super::{
    DEFAULT_IMG_H, INDENT_PX, LINES_PER_PAGE, LineSpan, MAX_PAGES, NO_PREFETCH, PAGE_BUF,
    PendingPositionChange, ReaderApp, State, decode_utf8_char,
};

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
            );
            self.pg.line_count = count;
            c
        } else {
            self.wrap_monospace(n)
        }
    }

    /// Precompute justification metrics for every line on the current page.
    /// Called once after wrapping so that `draw()` can reuse cached values
    /// across all strip passes instead of re-running `measure_line()` per strip.
    pub(super) fn precompute_line_metrics(&mut self) {
        if let Some(ref fs) = self.fonts {
            for i in 0..self.pg.line_count {
                let span = &self.pg.lines[i];
                // Images and empty spans don't need measurement.
                if span.is_image() || span.len == 0 {
                    self.pg.line_measures[i] = LineMeasure::default();
                } else {
                    self.pg.line_measures[i] = measure_line(&self.pg.buf, span, fs);
                }
            }
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
            self.precompute_line_metrics();
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
        self.precompute_line_metrics();
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

    pub(super) fn preindex_all_pages(&mut self, k: &mut KernelHandle<'_>) {
        if self.epub.ch_cache.is_empty() {
            return;
        }

        plump_kernel::perf_begin!(_pi_t0);
        let ch = self.epub.chapter as usize;
        let spine_len = self.epub.spine.len();
        let name_hash = self.epub.name_hash;
        let key = super::layout::LayoutKey::current(
            self.book_font_size_idx,
            plump_kernel::kernel::bundle::CONTENT_FMT_LATEST,
            self.text_w as u16,
            self.font_line_h,
            self.max_lines,
        );

        // 1. cache hit (algo=2 with line records): adopt directly.
        if let Some(loaded) =
            super::layout::cache::load_layoutidx(k, name_hash, ch, spine_len, &key)
        {
            if !loaded.lines.is_empty() && !loaded.pages.is_empty() {
                self.adopt_loaded_chapter(loaded);
                plump_kernel::perf_event!(
                    "reader",
                    "preindex src=bundle pages={} lines={} elapsed_ms={}",
                    self.pg.total_pages,
                    self.pg.chapter_lines.len(),
                    _pi_t0.elapsed().as_millis()
                );
                return;
            }
            // legacy cache without lines: ignore, fall through to typeset
        }

        // 2. K-P typeset (primary path).
        self.pg.clear_kp_layout();
        match self.run_kp_typeset() {
            Ok(()) => {
                let _ = self.save_kp_to_pidx(k, &key, ch, spine_len, name_hash);
                plump_kernel::perf_event!(
                    "reader",
                    "preindex src=kp pages={} lines={} elapsed_ms={}",
                    self.pg.kp_pages.len(),
                    self.pg.chapter_lines.len(),
                    _pi_t0.elapsed().as_millis()
                );
                return;
            }
            Err(e) => {
                log::warn!("reader: K-P typeset failed ch{}: {:?}; greedy fallback", ch, e);
            }
        }

        // 3. greedy fallback (no PIDX save — algo=2 is reserved for K-P).
        self.greedy_preindex_compute(k);
        plump_kernel::perf_event!(
            "reader",
            "preindex src=greedy pages={} elapsed_ms={}",
            self.pg.total_pages,
            _pi_t0.elapsed().as_millis()
        );
    }

    /// Adopt a `LoadedChapter` from PIDX into `pg`. Populates K-P
    /// fields (chapter_lines, kp_pages, image_block_lines) and
    /// mirrors the page-start offsets into the legacy navigation
    /// arrays.
    fn adopt_loaded_chapter(&mut self, loaded: super::layout::cache::LoadedChapter) {
        let n = loaded.pages.len().min(MAX_PAGES);
        self.pg.clear_kp_layout();
        self.pg.kp_pages.extend_from_slice(&loaded.pages[..n]);
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

        for i in 0..n {
            self.pg.offsets[i] = self.pg.kp_pages[i].start_byte;
        }
        self.pg.total_pages = n.max(1);
        self.pg.fully_indexed = true;
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
    fn greedy_preindex_compute(&mut self, k: &mut KernelHandle<'_>) {
        let total = self.epub.ch_cache.len();
        self.pg.clear_kp_layout();
        self.pg.offsets[0] = 0;
        self.pg.total_pages = 1;

        let mut offset = 0usize;
        while offset < total && self.pg.total_pages < MAX_PAGES {
            let end = (offset + PAGE_BUF).min(total);
            let n = end - offset;
            self.pg.buf[..n].copy_from_slice(&self.epub.ch_cache[offset..end]);
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
    pub(super) fn run_kp_typeset(&mut self) -> Result<(), TypesetError> {
        plump_kernel::perf_begin!(_kp_t0);

        let Some(fs) = self.fonts.as_ref().copied() else {
            return Err(TypesetError::EmptyChapter);
        };
        if self.epub.ch_cache.is_empty() {
            return Err(TypesetError::EmptyChapter);
        }

        let mut pipeline = LayoutPipeline::new();
        let mut out_lines: Vec<LineLayout> = Vec::new();
        let mut out_image_blocks: Vec<u8> = Vec::new();

        // Bounded pre-allocation: avoid OOM mid-typeset.
        let _ = out_lines.try_reserve(64);
        let _ = out_image_blocks.try_reserve(64);

        let mut scanner = MarkupScanner::new(&self.epub.ch_cache);

        loop {
            let outcome = pipeline.step(
                &mut scanner,
                &fs,
                self.text_w as u16,
                self.font_line_h,
                &mut out_lines,
                &mut out_image_blocks,
            )?;
            if matches!(outcome, StepOutcome::Done) {
                break;
            }
        }

        if out_lines.is_empty() {
            // chapter contained only markers / whitespace; emit a single
            // empty page so navigation still works.
            self.pg.clear_kp_layout();
            self.pg.kp_pages.push(PageLayout::EMPTY);
            return Ok(());
        }

        let mut pages: Vec<PageLayout> = Vec::new();
        let _ = pages.try_reserve(8);
        if let Err(e) = paginate::paginate(
            &out_lines,
            self.max_lines,
            &out_image_blocks,
            &mut pages,
        ) {
            log::warn!("reader: paginate failed: {:?}", e);
            return Err(TypesetError::EmptyChapter);
        }

        plump_kernel::perf_event!(
            "reader",
            "preindex.kp paragraphs={} pages={} lines={} fallbacks={} elapsed_ms={}",
            pipeline.paragraphs,
            pages.len(),
            out_lines.len(),
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
        self.precompute_line_metrics();
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

// ── Phase 3: line analysis helpers for justification ─────────────────

/// Result of measuring a single line span for justification.
#[derive(Clone, Copy, Default)]
pub(in crate::apps) struct LineMeasure {
    /// Rendered width in pixels (excluding trailing whitespace).
    pub width: u32,
    /// Number of stretchable inter-word gaps (ASCII spaces only; NBSP excluded).
    pub gaps: u16,
}

impl LineMeasure {
    pub const ZERO: Self = Self { width: 0, gaps: 0 };
}

/// Measure one text line's natural rendered width and count stretchable gaps.
///
/// Stretchable gaps are ASCII spaces (0x20) only — NBSP (U+00A0) is rendered
/// but not stretched, and style markers are zero-width. Trailing spaces at
/// the end of a soft-wrapped line are excluded from width measurement.
pub(super) fn measure_line(
    buf: &[u8],
    span: &super::LineSpan,
    fonts: &fonts::FontSet,
) -> LineMeasure {
    let start = span.start as usize;
    let end = start + span.len as usize;
    let line = &buf[start..end];
    let sty_initial = span.style();

    let mut width: u32 = 0;
    let mut gaps: u16 = 0;
    let mut sty = sty_initial;
    let mut last_space_width: u32 = 0; // width contribution of the last trailing space run
    let mut in_trailing_space = false;

    let mut j = 0usize;
    while j < line.len() {
        let b = line[j];

        // style markers: zero width, update style
        if b == MARKER && j + 1 < line.len() {
            sty = match line[j + 1] {
                BOLD_ON => fonts::Style::Bold,
                ITALIC_ON => fonts::Style::Italic,
                HEADING_ON | H1_ON | H2_ON | H3_ON => fonts::Style::Heading,
                H4_ON | H5_ON | H6_ON => fonts::Style::Bold,
                BOLD_OFF | ITALIC_OFF | HEADING_OFF => fonts::Style::Regular,
                H1_OFF | H2_OFF | H3_OFF => fonts::Style::Regular,
                H4_OFF | H5_OFF | H6_OFF => fonts::Style::Regular,
                _ => sty,
            };
            j += 2;
            continue;
        }

        // UTF-8 multi-byte
        if b >= 0xC0 {
            let (ch, seq_len) = decode_utf8_char(line, j);

            // soft hyphen: zero-width
            if ch == '\u{00AD}' {
                j += seq_len;
                continue;
            }

            // NBSP: rendered as space width but NOT a stretchable gap
            if ch == '\u{00A0}' {
                let adv = fonts.advance(' ', sty) as u32;
                width += adv;
                in_trailing_space = false; // NBSP is not a trailing space
                j += seq_len;
                continue;
            }

            // regular space (multi-byte won't normally be space, but be safe)
            if ch == ' ' {
                let adv = fonts.advance(' ', sty) as u32;
                width += adv;
                gaps += 1;
                if !in_trailing_space {
                    in_trailing_space = true;
                    last_space_width = adv;
                } else {
                    last_space_width += adv;
                }
                j += seq_len;
                continue;
            }

            let adv = fonts.advance(ch, sty) as u32;
            width += adv;
            in_trailing_space = false;
            j += seq_len;
            continue;
        }

        // stray continuation byte
        if b >= 0x80 {
            j += 1;
            continue;
        }

        // control chars (except space)
        if b < bitmap::FIRST_CHAR && b != b' ' {
            j += 1;
            continue;
        }

        // ASCII space
        if b == b' ' {
            let adv = fonts.advance(' ', sty) as u32;
            width += adv;
            gaps += 1;
            if !in_trailing_space {
                in_trailing_space = true;
                last_space_width = adv;
            } else {
                last_space_width += adv;
            }
            j += 1;
            continue;
        }

        // printable ASCII
        let adv = fonts.advance(b as char, sty) as u32;
        width += adv;
        in_trailing_space = false;
        j += 1;
    }

    // strip trailing spaces from width measurement (soft-wrapped lines
    // often include the trailing space before the break point)
    if in_trailing_space {
        width = width.saturating_sub(last_space_width);
        // trailing spaces don't count as stretchable gaps
        // (they're invisible at the end of the line)
        // Count how many trailing spaces we had and subtract from gaps
        // Simple approach: we tracked last_space_width which is the sum
        // of the trailing run. Divide by single space advance to get count.
        let space_adv = fonts.advance(' ', sty_initial) as u32;
        if space_adv > 0 {
            let trailing_count = (last_space_width / space_adv) as u16;
            gaps = gaps.saturating_sub(trailing_count);
        }
    }

    LineMeasure { width, gaps }
}

// UTF-8 decoding is provided by plump_kernel::util::decode_utf8_char
// (re-exported via super::decode_utf8_char)

/// Translate a chapter-relative `LineLayout` (K-P output) into a
/// page-buffer-relative `LineSpan` (greedy renderer's input). Maps
/// flag bits across the two encodings and clamps offsets so the
/// renderer never reads past `pg.buf_len`.
fn linelayout_to_span(ll: &LineLayout, page_start_byte: u32, buf_len: u32) -> LineSpan {
    // image lines: filler entries carry start=0/len=0; origin entries
    // carry path_start/path_len. Both encodings use chapter-relative
    // bytes; translate to page-buffer-relative the same way.
    let raw_start = ll.start_byte.saturating_sub(page_start_byte);
    let raw_len = ll.end_byte.saturating_sub(ll.start_byte);
    let (start, len) = if ll.is_image() {
        // image origin: clamp into buf so `&buf[start..start+len]`
        // stays valid even if the path bytes brush the buf edge.
        let s = raw_start.min(buf_len) as u16;
        let l = raw_len.min(buf_len.saturating_sub(s as u32)) as u16;
        (s, l)
    } else {
        let s = raw_start.min(buf_len) as u16;
        let l = raw_len.min(buf_len.saturating_sub(s as u32)) as u16;
        (s, l)
    };

    let mut flags: u8 = 0;
    if ll.flags & LineLayout::FLAG_BOLD != 0 {
        flags |= LineSpan::FLAG_BOLD;
    }
    if ll.flags & LineLayout::FLAG_ITALIC != 0 {
        flags |= LineSpan::FLAG_ITALIC;
    }
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

    // align values are identical between the two types.
    let align = ll.align;

    LineSpan {
        start,
        len,
        flags,
        indent: ll.indent,
        align,
    }
}

pub(super) fn trim_trailing_cr(buf: &[u8], start: usize, end: usize) -> usize {
    if end > start && buf[end - 1] == b'\r' {
        end - 1
    } else {
        end
    }
}

// true if ch is a word-separator for line-wrapping (space, NBSP, etc)
#[inline]
fn is_wrap_space(ch: char) -> bool {
    matches!(ch, ' ' | '\u{00A0}')
}

pub(super) fn wrap_proportional(
    buf: &[u8],
    n: usize,
    fonts: &fonts::FontSet,
    lines: &mut [LineSpan],
    max_lines: usize,
    max_width_px: u32,
    img_heights: &[u16],
) -> (usize, usize) {
    let max_l = max_lines.min(lines.len());
    let base_max_w = max_width_px;
    let mut line_count: usize = 0;
    let mut line_start: usize = 0;
    let mut cursor_x: u32 = 0;
    let mut last_space: usize = 0;
    let mut cursor_at_space: u32 = 0;

    let mut bold = false;
    let mut italic = false;
    let mut heading = false;
    // shifted (already in HLEVEL_MASK position); valid when `heading` is set
    let mut hlevel: u8 = LineSpan::HLEVEL_H3;
    // current paragraph alignment, set by ALIGN_* markers (Phase 2)
    let mut align: u8 = LineSpan::ALIGN_DEFAULT;
    let mut indent: u8 = 0;
    let mut max_w = base_max_w;
    let mut img_idx: usize = 0;
    let mut skipped_leading_blank = false;

    #[inline]
    fn current_style(bold: bool, italic: bool, heading: bool) -> fonts::Style {
        if heading {
            fonts::Style::Heading
        } else if bold {
            fonts::Style::Bold
        } else if italic {
            fonts::Style::Italic
        } else {
            fonts::Style::Regular
        }
    }

    macro_rules! emit {
        ($start:expr, $end:expr, $end_kind:expr) => {
            if line_count < max_l {
                let e = trim_trailing_cr(buf, $start, $end);
                lines[line_count] = LineSpan {
                    start: ($start) as u16,
                    len: (e - ($start)) as u16,
                    flags: LineSpan::pack_flags(bold, italic, heading, hlevel, $end_kind),
                    indent,
                    align,
                };
                line_count += 1;
            }
        };
    }

    let mut i = 0;
    while i < n {
        let b = buf[i];

        if b == MARKER && i + 1 < n {
            if buf[i + 1] == IMG_REF && i + IMG_HEADER_LEN <= n {
                // [MARKER, IMG_REF, flags, w_lo, w_hi, h_lo, h_hi, alt_len, path_len, alt..., path...]
                let alt_len = buf[i + 7] as usize;
                let path_len = buf[i + 8] as usize;
                let alt_start = i + IMG_HEADER_LEN;
                let path_start = alt_start + alt_len;
                let payload_end = path_start + path_len;
                if payload_end <= n && path_len > 0 {
                    if line_start < i {
                        emit!(line_start, i, LineSpan::END_HARD);
                        if line_count >= max_l {
                            return (i, line_count);
                        }
                    }

                    let line_h = fonts.line_height(fonts::Style::Regular);
                    // prefer pre-scanned height (peeked from PNG/JPEG header
                    // by images.rs); fall back to DEFAULT_IMG_H. attribute
                    // hints from <img width/height> are checked in images.rs
                    // when prescan is run, so the height we land on already
                    // accounts for them.
                    let img_h = if img_idx < img_heights.len() && img_heights[img_idx] > 0 {
                        img_heights[img_idx]
                    } else {
                        DEFAULT_IMG_H
                    };
                    img_idx += 1;
                    let img_lines = img_h.div_ceil(line_h).max(1) as usize;

                    // page-fit: if the image needs more lines than the page
                    // has left AND we've already emitted text on this page,
                    // push the entire image to the next page by returning at
                    // the marker offset. wrap_proportional is re-entered on
                    // the next page with i pointing at the IMG_REF marker, so
                    // the image (and its still-active alt + dims metadata)
                    // is consumed there. an image that's bigger than the
                    // whole page is emitted anyway and clipped at the bottom
                    // (better than infinite-looping).
                    if line_count > 0 && line_count + img_lines > max_l {
                        return (i, line_count);
                    }

                    if line_count < max_l {
                        lines[line_count] = LineSpan {
                            start: path_start as u16,
                            len: path_len as u16,
                            flags: LineSpan::FLAG_IMAGE,
                            // store alt_len in the indent slot for image
                            // origins (indent isn't used by image renders);
                            // draw can recover the alt span back from
                            // `path_start - alt_len`.
                            indent: alt_len as u8,
                            align: LineSpan::ALIGN_DEFAULT,
                        };
                        line_count += 1;
                    }

                    for _ in 1..img_lines {
                        if line_count >= max_l {
                            break;
                        }
                        lines[line_count] = LineSpan {
                            start: 0,
                            len: 0,
                            flags: LineSpan::FLAG_IMAGE,
                            indent: 0,
                            align: LineSpan::ALIGN_DEFAULT,
                        };
                        line_count += 1;
                    }

                    i = payload_end;
                    line_start = i;
                    cursor_x = 0;
                    last_space = line_start;
                    cursor_at_space = 0;
                    if line_count >= max_l {
                        return (line_start, line_count);
                    }
                    continue;
                }
            }

            // explicit page break: end the current page at the marker offset
            // unless the chapter just started (no content yet). next page
            // resumes at the marker so it's consumed cleanly on re-entry.
            if buf[i + 1] == PAGE_BREAK {
                let has_content = line_start < i || line_count > 0;
                if has_content {
                    if line_start < i {
                        emit!(line_start, i, LineSpan::END_HARD);
                    }
                    // skip past the marker so we don't loop on the next pass
                    return (i + 2, line_count);
                }
                // chapter-start PAGE_BREAK is a no-op; consume and continue
                i += 2;
                continue;
            }

            match buf[i + 1] {
                BOLD_ON => bold = true,
                BOLD_OFF => bold = false,
                ITALIC_ON => italic = true,
                ITALIC_OFF => italic = false,
                // legacy v1 bundles: heading marker without level. fall back to
                // h3-tier (left, no centering) so old bundles don't shift.
                HEADING_ON => {
                    heading = true;
                    hlevel = LineSpan::HLEVEL_H3;
                }
                HEADING_OFF => {
                    heading = false;
                    hlevel = LineSpan::HLEVEL_H3;
                }
                H1_ON => {
                    heading = true;
                    hlevel = LineSpan::HLEVEL_H1;
                }
                H2_ON => {
                    heading = true;
                    hlevel = LineSpan::HLEVEL_H2;
                }
                H3_ON => {
                    heading = true;
                    hlevel = LineSpan::HLEVEL_H3;
                }
                // h4-h6 render as bold body text rather than a heading font
                H4_ON | H5_ON | H6_ON => bold = true,
                H1_OFF | H2_OFF | H3_OFF => {
                    heading = false;
                    hlevel = LineSpan::HLEVEL_H3;
                }
                H4_OFF | H5_OFF | H6_OFF => bold = false,
                ALIGN_LEFT => align = LineSpan::ALIGN_LEFT,
                ALIGN_CENTER => align = LineSpan::ALIGN_CENTER,
                ALIGN_RIGHT => align = LineSpan::ALIGN_RIGHT,
                ALIGN_JUSTIFY | ALIGN_RESET => align = LineSpan::ALIGN_DEFAULT,
                QUOTE_ON => {
                    indent = indent.saturating_add(1);
                    max_w = base_max_w.saturating_sub(INDENT_PX * indent as u32);
                }
                QUOTE_OFF => {
                    indent = indent.saturating_sub(1);
                    max_w = base_max_w.saturating_sub(INDENT_PX * indent as u32);
                }
                _ => {}
            }
            i += 2;
            continue;
        }

        if b == b'\r' {
            i += 1;
            continue;
        }

        if b == b'\n' {
            let end = trim_trailing_cr(buf, line_start, i);
            if line_count == 0 && end == line_start && !skipped_leading_blank {
                skipped_leading_blank = true;
                line_start = i + 1;
                cursor_x = 0;
                last_space = line_start;
                cursor_at_space = 0;
                i += 1;
                continue;
            }

            emit!(line_start, i, LineSpan::END_HARD);
            line_start = i + 1;
            cursor_x = 0;
            last_space = line_start;
            cursor_at_space = 0;
            if line_count >= max_l {
                return (line_start, line_count);
            }
            i += 1;
            continue;
        }

        // UTF-8 multi-byte: decode the full character and measure it
        // using the font's extended glyph tables
        if b >= 0xC0 {
            let (ch, seq_len) = decode_utf8_char(buf, i);

            // soft hyphen (U+00AD): zero-width break opportunity
            if ch == '\u{00AD}' {
                last_space = i + seq_len;
                cursor_at_space = cursor_x;
                i += seq_len;
                continue;
            }

            // NBSP and regular spaces: word-break opportunity
            if is_wrap_space(ch) {
                let sty = current_style(bold, italic, heading);
                cursor_x += fonts.advance(' ', sty) as u32;
                last_space = i + seq_len;
                cursor_at_space = cursor_x;
                if cursor_x > max_w {
                    emit!(line_start, i, LineSpan::END_SOFT);
                    line_start = i + seq_len;
                    cursor_x = 0;
                    last_space = line_start;
                    cursor_at_space = 0;
                    if line_count >= max_l {
                        return (line_start, line_count);
                    }
                }
                i += seq_len;
                continue;
            }

            let sty = current_style(bold, italic, heading);
            let adv = fonts.advance(ch, sty) as u32;
            cursor_x += adv;
            if cursor_x > max_w {
                if last_space > line_start {
                    emit!(line_start, last_space, LineSpan::END_SOFT);
                    cursor_x -= cursor_at_space;
                    line_start = last_space;
                } else {
                    emit!(line_start, i, LineSpan::END_SOFT);
                    line_start = i;
                    cursor_x = adv;
                }
                last_space = line_start;
                cursor_at_space = 0;
                if line_count >= max_l {
                    return (line_start, line_count);
                }
            }
            i += seq_len;
            continue;
        }
        if b >= 0x80 {
            // stray continuation byte
            i += 1;
            continue;
        }

        // --- ASCII fast path: batch space and word runs ---
        let sty = current_style(bold, italic, heading);
        let font = fonts.font(sty);
        let glyphs = font.glyphs;

        if b == b' ' {
            let adv = glyphs[(b' ' - FIRST_CHAR) as usize].advance as u32;
            cursor_x += adv;
            last_space = i + 1;
            cursor_at_space = cursor_x;
            if cursor_x > max_w {
                emit!(line_start, i, LineSpan::END_SOFT);
                line_start = i + 1;
                cursor_x = 0;
                last_space = line_start;
                cursor_at_space = 0;
                if line_count >= max_l {
                    return (line_start, line_count);
                }
            }
            i += 1;
            continue;
        }

        // Printable non-space ASCII (0x21..=0x7E): batch-scan the word run.
        // Find end of contiguous printable non-space ASCII bytes, sum advances.
        let word_start = i;
        let remaining = max_w.saturating_sub(cursor_x);
        let mut run_adv: u32 = 0;
        let mut j = i;
        while j < n {
            let c = buf[j];
            // stop at space, control chars, MARKER, high-bit bytes
            if c <= b' ' || c > 0x7E {
                break;
            }
            let a = glyphs[(c - FIRST_CHAR) as usize].advance as u32;
            if run_adv + a > remaining && j > word_start {
                // would overflow; stop batch here so we handle break properly
                break;
            }
            run_adv += a;
            j += 1;
        }

        if j > i {
            // consumed j - i bytes as a batch
            cursor_x += run_adv;
            i = j;
            if cursor_x > max_w {
                // overflow: break at last space or at word start
                if last_space > line_start {
                    emit!(line_start, last_space, LineSpan::END_SOFT);
                    cursor_x -= cursor_at_space;
                    line_start = last_space;
                } else {
                    emit!(line_start, word_start, LineSpan::END_SOFT);
                    line_start = word_start;
                    // recompute cursor_x from line_start..i
                    cursor_x = 0;
                    for k in line_start..i {
                        let c = buf[k];
                        if c >= FIRST_CHAR && c <= 0x7E {
                            cursor_x += glyphs[(c - FIRST_CHAR) as usize].advance as u32;
                        }
                    }
                }
                last_space = line_start;
                cursor_at_space = 0;
                if line_count >= max_l {
                    return (line_start, line_count);
                }
            }
            continue;
        }

        // single non-printable byte that wasn't caught above; skip
        i += 1;
    }

    if line_start < n && line_count < max_l {
        let e = trim_trailing_cr(buf, line_start, n);
        if e > line_start {
            lines[line_count] = LineSpan {
                start: line_start as u16,
                len: (e - line_start) as u16,
                flags: LineSpan::pack_flags(bold, italic, heading, hlevel, LineSpan::END_BUFFER),
                indent,
                align,
            };
            line_count += 1;
        }
    }

    (n, line_count)
}
