//! K-P typesetting pipeline.
//!
//! Glues `MarkupScanner` → `items::build_paragraph` →
//! `breaker::break_paragraph_with_fallback` → `convert::append_lines`
//! together. Owns two heap vecs (items + break choices) reused
//! across every paragraph in a chapter. `Drop` shrinks them back
//! to zero so the pipeline's working budget (~16 KB peak) is
//! guaranteed released the moment it leaves scope.
//!
//! Designed as a step-driven state machine: one `step()` call
//! processes one paragraph and returns `More` or `Done`. The
//! caller in `paging.rs` yields between steps so the executor
//! stays responsive during long chapters.

use alloc::vec::Vec;

use crate::fonts::{FontSet, Style};

use super::breaker::{break_paragraph_with_fallback, BreakChoice, BreakConfig, BreakScratch};
use super::items::{self, Item, ParagraphEnd, ParagraphMeta};
use super::paginate::convert;
use super::scan::{ImageRef, MarkupScanner, TextStyle};
use super::{LineLayout, MAX_LINES_PER_CHAPTER};

// ── image budget ──────────────────────────────────────────────────

/// Inputs governing image-line reservation. The pipeline replicates
/// the decoder's integer downscale (see `images.rs::peek_source_dimensions`
/// and the smol-epub jpeg/png decoders) so the reserved height matches
/// what `decode_page_images` will actually blit.
#[derive(Clone, Copy, Debug)]
pub struct ImageBudget {
    /// Column width in pixels (matches the K-P breaker's line width).
    pub text_w: u16,
    /// Inline-image height cap, typically `inline_img_max_h(text_area_h)`.
    pub inline_cap: u16,
    /// Renderer line height in pixels.
    pub line_h: u16,
}

// ── image dimension hint ──────────────────────────────────────────

/// Resolves the rendered height of an image whose HTML `width`/`height`
/// attributes are missing or zero. The pipeline calls this only when
/// the `ImageRef` lacks usable attrs; implementations are responsible
/// for resolving the `src` bytes to a ZIP entry and peeking the
/// JPEG/PNG header.
///
/// Returning `None` makes the pipeline fall back to `DEFAULT_IMG_H`
/// — caller can treat `None` as "I can't tell" rather than "image
/// is exactly this default size".
pub trait ImageHeightHint {
    /// `src` is the raw `<img src="...">` value bytes (chapter-buffer
    /// slice). Returns the rendered pixel height in the same units
    /// the decoder will produce.
    fn rendered_height(&mut self, src: &[u8]) -> Option<u16>;
}

/// No-op `ImageHeightHint` used by host tests and the legacy code path
/// that has no SD access. Always returns `None`, preserving the
/// pre-trait fallback behaviour.
pub struct NoImageHint;

impl ImageHeightHint for NoImageHint {
    #[inline]
    fn rendered_height(&mut self, _src: &[u8]) -> Option<u16> {
        None
    }
}

// ── pipeline ──────────────────────────────────────────────────────

#[derive(Debug)]
pub enum TypesetError {
    /// Chapter exceeded `MAX_LINES_PER_CHAPTER`; caller should drop
    /// the partial output and re-typeset with a coarser strategy
    /// (or accept the chapter as un-paginatable).
    LineCapExceeded,
    /// All-empty chapter (e.g. only markers). Caller should emit an
    /// empty page rather than failing.
    EmptyChapter,
    /// Growing the line buffers failed; typeset transients share the
    /// heap with image-decode bands, so this must surface as a clean
    /// error (caller falls back to the greedy pager) instead of the
    /// infallible-alloc panic path.
    OutOfMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    More,
    Done,
}

/// RAII container for the K-P working buffers. The `Drop` impl
/// shrinks both vecs back to zero so peak heap is bounded by the
/// pipeline's lexical scope.
pub struct LayoutPipeline {
    pub items: Vec<Item>,
    pub choices: Vec<BreakChoice>,
    /// reusable K-P DP scratch; allocated once per chapter typeset
    /// instead of per paragraph
    scratch: BreakScratch,
    /// number of paragraphs that fell back to greedy this run
    pub fallback_count: u32,
    /// number of paragraphs processed this run
    pub paragraphs: u32,
    /// number of image blocks emitted this run
    pub images: u32,
    /// pending page-break flag, threaded across paragraphs
    pub page_break_pending: bool,
}

impl LayoutPipeline {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            choices: Vec::new(),
            scratch: BreakScratch::new(),
            fallback_count: 0,
            paragraphs: 0,
            images: 0,
            page_break_pending: false,
        }
    }

    /// Process one paragraph and append the resulting `LineLayout`s
    /// (and image-block size entries) to the output vecs.
    ///
    /// `out_image_blocks` is kept parallel to `out_lines`: for each
    /// image-origin line, the corresponding entry holds the total
    /// number of `LineLayout`s in the image block (origin + filler);
    /// for all other lines, the entry is 0. The paginator consumes
    /// this to enforce image atomicity.
    pub fn step(
        &mut self,
        scanner: &mut MarkupScanner<'_>,
        fonts: &FontSet,
        budget: ImageBudget,
        hint: &mut dyn ImageHeightHint,
        out_lines: &mut Vec<LineLayout>,
        out_image_blocks: &mut Vec<u8>,
    ) -> Result<StepOutcome, TypesetError> {
        if scanner.is_eof() {
            return Ok(StepOutcome::Done);
        }

        self.items.clear();
        self.choices.clear();

        // Build one paragraph's items.
        let meta = items::build_paragraph(
            scanner,
            |c, s| advance_for(fonts, c, s) as u16,
            &mut self.items,
        );

        self.paragraphs = self.paragraphs.saturating_add(1);

        // Image paragraph: emit image LineLayouts directly.
        if matches!(meta.end_kind, ParagraphEnd::ImageBlock) {
            // path_len is u8 by the IMG_REF format, so 256 always fits.
            let mut src_buf = [0u8; 256];
            let src = read_image_src(scanner, meta.image, &mut src_buf);
            let (image_lines, reserved_h) = image_lines_for(meta.image, budget, src, hint);
            let lines_before = out_lines.len();
            try_reserve_out(out_lines, out_image_blocks, image_lines as usize + 1)?;
            convert::append_image_block(
                &meta,
                image_lines,
                reserved_h,
                &mut self.page_break_pending,
                out_lines,
            );
            // parallel image_block_lines: stamp `image_lines` at the
            // origin slot, 0 for fillers
            ensure_parallel(out_image_blocks, lines_before);
            for k in lines_before..out_lines.len() {
                let val = if k == lines_before { image_lines } else { 0 };
                out_image_blocks.push(val);
            }
            self.images = self.images.saturating_add(1);
            check_cap(out_lines)?;
            return self.post_step(meta);
        }

        // Empty paragraph (e.g. trailing PageBreak): nothing to lay out.
        if self.items.is_empty() {
            return self.post_step(meta);
        }

        // Width budget: text_w shrunk by indent ⇒ INDENT_PX*indent.
        let indent_px = (super::super::INDENT_PX * meta.block.indent as u32) as u16;
        let line_width = budget.text_w.saturating_sub(indent_px);

        let cfg = BreakConfig {
            line_width,
            ..BreakConfig::DEFAULT
        };

        let lines_before = out_lines.len();
        let used_fallback;
        let kp_choice_count;
        match break_paragraph_with_fallback(&self.items, &cfg, &mut self.scratch, &mut self.choices) {
            Ok(()) => {
                kp_choice_count = self.choices.len();
                used_fallback = false;
                try_reserve_out(out_lines, out_image_blocks, self.choices.len())?;
                convert::append_lines(
                    &self.items,
                    &self.choices,
                    &meta,
                    line_width,
                    &mut self.page_break_pending,
                    out_lines,
                );
            }
            Err(_) => {
                self.fallback_count = self.fallback_count.saturating_add(1);
                kp_choice_count = 0;
                used_fallback = true;
                // worst case one line per item, plus the trailing flush
                try_reserve_out(out_lines, out_image_blocks, self.items.len() + 1)?;
                convert::append_greedy_fallback(
                    &self.items,
                    &meta,
                    line_width,
                    &mut self.page_break_pending,
                    out_lines,
                );
            }
        }
        // pad out_image_blocks for the lines we just emitted
        ensure_parallel(out_image_blocks, lines_before);
        for _ in lines_before..out_lines.len() {
            out_image_blocks.push(0);
        }
        check_cap(out_lines)?;

        // suppress unused-warning for these now that the KPDIAG-A
        // per-paragraph log is gone. summary is in `preindex.kp`.
        let _ = (kp_choice_count, used_fallback);

        self.post_step(meta)
    }

    fn post_step(&mut self, meta: ParagraphMeta) -> Result<StepOutcome, TypesetError> {
        if matches!(meta.end_kind, ParagraphEnd::PageBreakAfter) {
            self.page_break_pending = true;
        }
        Ok(StepOutcome::More)
    }
}

impl Default for LayoutPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LayoutPipeline {
    fn drop(&mut self) {
        self.items.clear();
        self.items.shrink_to_fit();
        self.choices.clear();
        self.choices.shrink_to_fit();
        self.scratch.release();
    }
}

// ── helpers ───────────────────────────────────────────────────────

#[inline]
fn advance_for(fonts: &FontSet, ch: char, style: TextStyle) -> u8 {
    fonts.advance(ch, text_style_to_font_style(style))
}

#[inline]
fn text_style_to_font_style(style: TextStyle) -> Style {
    // Single source of truth — see `fonts::Style::from_flags`. K-P and
    // the renderer must agree on this resolution or measured line
    // widths diverge from drawn line widths on nested markup.
    Style::from_flags(style.bold, style.italic, style.heading, style.hlevel)
}

/// Number of LineLayouts to reserve for an image block, plus the
/// reserved pixel height it was derived from (persisted on the origin
/// line so a spacing change can recompute the count).
///
/// The reservation must match what `decode_page_images` will blit, or
/// the renderer ends up vertically centring a small bitmap inside an
/// over-large reserved block. The `src` slice carries the `<img
/// src="...">` bytes; when HTML attrs are missing the pipeline asks
/// `hint` to peek the source file. A `None` hint result falls back
/// to `DEFAULT_IMG_H`.
fn image_lines_for(
    image: Option<ImageRef>,
    budget: ImageBudget,
    src: &[u8],
    hint: &mut dyn ImageHeightHint,
) -> (u8, u16) {
    let lh = budget.line_h.max(1);
    let reserved = reserved_image_height(image, budget, src, hint);
    let lines = reserved.div_ceil(lh);
    (lines.clamp(1, u8::MAX as u16) as u8, reserved)
}

/// Read the `<img src="...">` bytes for `image` into `dst` and return
/// a slice of what was read. Empty slice when there's no image, or
/// when the source reports a short read (out-of-range path bytes).
fn read_image_src<'b>(
    scanner: &mut super::scan::MarkupScanner<'_>,
    image: Option<ImageRef>,
    dst: &'b mut [u8],
) -> &'b [u8] {
    let Some(img) = image else { return &[] };
    let want = (img.path_len as usize).min(dst.len());
    let n = scanner.read_into(img.path_start, &mut dst[..want]);
    &dst[..n]
}

/// Compute the reserved image height for an inline image block.
/// Resolution priority:
///   1. Both HTML attrs present → decoder-style integer downscale.
///   2. Either attr missing      → ask the hint to peek the source.
///   3. Hint returned `None`     → use whatever height attr we have,
///                                 else `DEFAULT_IMG_H`, both clamped
///                                 to the inline cap.
fn reserved_image_height(
    image: Option<ImageRef>,
    budget: ImageBudget,
    src: &[u8],
    hint: &mut dyn ImageHeightHint,
) -> u16 {
    let cap = budget.inline_cap.max(budget.line_h.max(1));
    let Some(img) = image else {
        return super::super::DEFAULT_IMG_H.min(cap);
    };

    // 1. Both HTML attrs: replicate the decoder's integer downscale.
    if img.attr_w > 0 && img.attr_h > 0 {
        let sw = img.attr_w.div_ceil(budget.text_w.max(1));
        let sh = img.attr_h.div_ceil(cap);
        let scale = sw.max(sh).max(1);
        return (img.attr_h / scale).min(cap);
    }

    // 2. No usable attrs: peek the source file.
    if let Some(h) = hint.rendered_height(src) {
        return h.min(cap).max(budget.line_h);
    }

    // 3. Peek unavailable: best-effort fallback.
    if img.attr_h > 0 {
        return img.attr_h.min(cap);
    }
    super::super::DEFAULT_IMG_H.min(cap)
}

fn check_cap(out_lines: &Vec<LineLayout>) -> Result<(), TypesetError> {
    if out_lines.len() > MAX_LINES_PER_CHAPTER {
        Err(TypesetError::LineCapExceeded)
    } else {
        Ok(())
    }
}

// reserve room for one paragraph's worth of output in both parallel
// vectors before appending, so the append loops never take Vec's
// infallible growth path
fn try_reserve_out(
    out_lines: &mut Vec<LineLayout>,
    out_image_blocks: &mut Vec<u8>,
    additional: usize,
) -> Result<(), TypesetError> {
    if out_lines.try_reserve(additional).is_err()
        || out_image_blocks.try_reserve(additional).is_err()
    {
        return Err(TypesetError::OutOfMemory);
    }
    Ok(())
}

fn ensure_parallel(out_image_blocks: &mut Vec<u8>, target_len: usize) {
    while out_image_blocks.len() < target_len {
        out_image_blocks.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(attr_w: u16, attr_h: u16) -> ImageRef {
        ImageRef {
            start: 0,
            alt_start: 0,
            path_start: 0,
            end: 0,
            flags: 0,
            attr_w,
            attr_h,
            alt_len: 0,
            path_len: 0,
        }
    }

    fn budget(text_w: u16, inline_cap: u16, line_h: u16) -> ImageBudget {
        ImageBudget { text_w, inline_cap, line_h }
    }

    /// Mock hint that returns a fixed height regardless of `src`.
    struct FixedHint(u16);
    impl ImageHeightHint for FixedHint {
        fn rendered_height(&mut self, _src: &[u8]) -> Option<u16> {
            Some(self.0)
        }
    }

    /// Mock hint that fails every call. Used to assert the
    /// with-attrs fast path never asks for a peek.
    struct PanickingHint;
    impl ImageHeightHint for PanickingHint {
        fn rendered_height(&mut self, _src: &[u8]) -> Option<u16> {
            panic!("hint must not be called when attrs are present");
        }
    }

    #[test]
    fn image_lines_for_applies_decoder_scale() {
        // 1600×2400 source into a 400×288 box: sw=4, sh=9, scale=9,
        // reserved = 2400/9 = 266 px → ceil(266/22) = 13 lines.
        let (lines, _) = image_lines_for(
            Some(image(1600, 2400)),
            budget(400, 288, 22),
            b"",
            &mut NoImageHint,
        );
        assert_eq!(lines, 13);
    }

    #[test]
    fn image_lines_for_uses_default_when_no_hint() {
        // No attribute hints AND no peek hint: fall back to
        // DEFAULT_IMG_H=350, clamped to inline_cap=288 → 14 lines.
        let (lines, _) = image_lines_for(
            Some(image(0, 0)),
            budget(400, 288, 22),
            b"",
            &mut NoImageHint,
        );
        let expected = (super::super::super::DEFAULT_IMG_H.min(288)).div_ceil(22) as u8;
        assert_eq!(lines, expected);
    }

    #[test]
    fn image_lines_for_height_only_hint_clamps_to_cap() {
        // width omitted, height = 2000, no peek hint: with no scale
        // info, clamp attr_h to inline_cap=200 → ceil(200/22) = 10.
        let (lines, _) = image_lines_for(
            Some(image(0, 2000)),
            budget(400, 200, 22),
            b"",
            &mut NoImageHint,
        );
        assert_eq!(lines, 10);
    }

    #[test]
    fn image_lines_for_small_image_fits_natural_size() {
        // 100×80 source, no downscale needed (sw=1, sh=1, scale=1):
        // reserved = 80 px → ceil(80/22) = 4 lines.
        let (lines, _) = image_lines_for(
            Some(image(100, 80)),
            budget(400, 288, 22),
            b"",
            &mut NoImageHint,
        );
        assert_eq!(lines, 4);
    }

    #[test]
    fn image_lines_for_clamps_to_u8_max() {
        // Pathological reservation: even after the cap, line_h=1 would
        // yield more than 255 lines; the clamp keeps it in u8.
        let (lines, _) = image_lines_for(
            Some(image(0, u16::MAX)),
            budget(400, u16::MAX, 1),
            b"",
            &mut NoImageHint,
        );
        assert_eq!(lines, u8::MAX);
    }

    #[test]
    fn image_lines_for_consults_hint_when_attrs_missing() {
        // attr_w == attr_h == 0, hint says 120 px. Cap=288, line_h=22
        // → ceil(120/22) = 6 lines. No DEFAULT_IMG_H over-reservation.
        let mut hint = FixedHint(120);
        let (lines, _) = image_lines_for(
            Some(image(0, 0)),
            budget(400, 288, 22),
            b"cover.jpg",
            &mut hint,
        );
        assert_eq!(lines, 6);
    }

    #[test]
    fn image_lines_for_hint_clamped_to_inline_cap() {
        // Hint reports a height larger than the inline cap; the result
        // is clamped so a single inline image can't dominate the page.
        let mut hint = FixedHint(500);
        let (lines, _) = image_lines_for(
            Some(image(0, 0)),
            budget(400, 200, 22),
            b"x",
            &mut hint,
        );
        assert_eq!(lines, (200u16.div_ceil(22)) as u8);
    }

    #[test]
    fn image_lines_for_hint_floor_is_one_line() {
        // Hint reports a tiny height; reservation never drops below
        // a single line, otherwise the image atomicity rule would
        // pack the bitmap into a 0-line slot.
        let mut hint = FixedHint(5);
        let (lines, _) = image_lines_for(
            Some(image(0, 0)),
            budget(400, 288, 22),
            b"x",
            &mut hint,
        );
        // floor enforced by `h.min(cap).max(line_h)` then div_ceil
        assert_eq!(lines, 1);
    }

    #[test]
    fn image_lines_for_falls_back_when_hint_none() {
        // Hint returns None (deflate-compressed entry, missing file,
        // I/O error). With attr_h also 0 we land on DEFAULT_IMG_H.
        struct NullHint;
        impl ImageHeightHint for NullHint {
            fn rendered_height(&mut self, _src: &[u8]) -> Option<u16> {
                None
            }
        }
        let (lines, _) = image_lines_for(
            Some(image(0, 0)),
            budget(400, 288, 22),
            b"x",
            &mut NullHint,
        );
        let expected = (super::super::super::DEFAULT_IMG_H.min(288)).div_ceil(22) as u8;
        assert_eq!(lines, expected);
    }

    #[test]
    fn image_lines_for_with_both_attrs_skips_hint() {
        // Fast path: when both attrs are present the hint is never
        // consulted, even if it would panic.
        let (lines, _) = image_lines_for(
            Some(image(1600, 2400)),
            budget(400, 288, 22),
            b"x",
            &mut PanickingHint,
        );
        assert_eq!(lines, 13);
    }

    // ── shared Style::from_flags resolver ─────────────────────────

    #[test]
    fn style_from_flags_regular_when_no_flags() {
        assert_eq!(Style::from_flags(false, false, false, 0), Style::Regular);
    }

    #[test]
    fn style_from_flags_bold_italic_picks_bold() {
        // The actual K-P↔renderer divergence case: nested <b><i>…</i></b>
        // produces (bold=true, italic=true). Bold must win.
        assert_eq!(Style::from_flags(true, true, false, 0), Style::Bold);
    }

    #[test]
    fn style_from_flags_h1_to_h3_pick_heading() {
        assert_eq!(Style::from_flags(false, false, true, 1), Style::Heading);
        assert_eq!(Style::from_flags(false, false, true, 2), Style::Heading);
        assert_eq!(Style::from_flags(false, false, true, 3), Style::Heading);
    }

    #[test]
    fn style_from_flags_h4_to_h6_pick_bold() {
        // TextStyle::is_h4_h6_bold intent — h4-h6 render as body bold,
        // not heading font. K-P must agree with the renderer here.
        assert_eq!(Style::from_flags(false, false, true, 4), Style::Bold);
        assert_eq!(Style::from_flags(false, false, true, 5), Style::Bold);
        assert_eq!(Style::from_flags(false, false, true, 6), Style::Bold);
    }

    #[test]
    fn style_from_flags_heading_beats_bold_at_h1() {
        // bold+h1 still picks Heading (heading-level priority).
        assert_eq!(Style::from_flags(true, false, true, 1), Style::Heading);
    }

    #[test]
    fn style_from_flags_italic_only_when_nothing_else() {
        assert_eq!(Style::from_flags(false, true, false, 0), Style::Italic);
        // bold+italic still picks bold
        assert_eq!(Style::from_flags(true, true, false, 0), Style::Bold);
    }
}
