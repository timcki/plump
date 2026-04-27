// text wrapping, page navigation, and load/prefetch

use smol_epub::html_strip::{
    ALIGN_CENTER, ALIGN_JUSTIFY, ALIGN_LEFT, ALIGN_RESET, ALIGN_RIGHT, BOLD_OFF, BOLD_ON, H1_OFF,
    H1_ON, H2_OFF, H2_ON, H3_OFF, H3_ON, H4_OFF, H4_ON, H5_OFF, H5_ON, H6_OFF, H6_ON, HEADING_OFF,
    HEADING_ON, IMG_HEADER_LEN, IMG_REF, ITALIC_OFF, ITALIC_ON, MARKER, PAGE_BREAK, QUOTE_OFF,
    QUOTE_ON,
};

use crate::fonts;
use crate::fonts::bitmap::{self, FIRST_CHAR};
use crate::kernel::KernelHandle;

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
        let font_idx = self.book_font_size_idx;

        // try to load a cached page index for this (chapter, font) from
        // the bundle. only meaningful after FLAG_CORE_READY (load_pageidx
        // checks FLAG_PAGEIDX_READY internally).
        if let Some((offsets, count)) = self.epub.load_pageidx(k, ch, font_idx) {
            self.pg.offsets = offsets;
            self.pg.total_pages = count.max(1);
            self.pg.fully_indexed = true;
            log::debug!(
                "chapter pre-indexed from bundle: ch{} {} pages",
                ch,
                self.pg.total_pages,
            );
            plump_kernel::perf_event!(
                "reader",
                "preindex_all_pages src=bundle pages={} elapsed_ms={}",
                self.pg.total_pages,
                _pi_t0.elapsed().as_millis()
            );
            return;
        }

        let total = self.epub.ch_cache.len();
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
        log::debug!("chapter pre-indexed: {} pages", self.pg.total_pages);
        plump_kernel::perf_event!(
            "reader",
            "preindex_all_pages src=compute pages={} ch_bytes={} elapsed_ms={}",
            self.pg.total_pages,
            total,
            _pi_t0.elapsed().as_millis()
        );

        // persist to bundle for next warm open. no-op if CORE_READY
        // isn't set yet (bundle still being built).
        let pages = self.pg.total_pages;
        if let Err(e) = self.epub.save_pageidx(k, ch, font_idx, &self.pg.offsets[..pages]) {
            log::warn!("reader: save_pageidx ch{} failed: {}", ch, e);
        }
    }

    pub(super) fn scan_to_last_page(
        &mut self,
        k: &mut KernelHandle<'_>,
    ) -> crate::error::Result<()> {
        while !self.pg.fully_indexed && self.pg.total_pages < MAX_PAGES {
            self.pg.page = self.pg.total_pages - 1;
            self.load_and_prefetch(k)?;
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
        self.load_and_prefetch(k)
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
