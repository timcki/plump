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

use super::breaker::{break_paragraph_with_fallback, BreakChoice, BreakConfig};
use super::items::{self, Item, ParagraphEnd, ParagraphMeta};
use super::paginate::convert;
use super::scan::{MarkupScanner, TextStyle};
use super::{LineLayout, MAX_LINES_PER_CHAPTER};

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
        text_w: u16,
        line_h: u16,
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
            let image_lines = image_lines_for(meta.image, line_h);
            let lines_before = out_lines.len();
            convert::append_image_block(&meta, image_lines, &mut self.page_break_pending, out_lines);
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
        let line_width = text_w.saturating_sub(indent_px);

        let cfg = BreakConfig {
            line_width,
            ..BreakConfig::DEFAULT
        };

        let lines_before = out_lines.len();
        match break_paragraph_with_fallback(&self.items, &cfg, &mut self.choices) {
            Ok(()) => {
                convert::append_lines(
                    &self.items,
                    &self.choices,
                    &meta,
                    &mut self.page_break_pending,
                    out_lines,
                );
            }
            Err(_) => {
                self.fallback_count = self.fallback_count.saturating_add(1);
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
    }
}

// ── helpers ───────────────────────────────────────────────────────

#[inline]
fn advance_for(fonts: &FontSet, ch: char, style: TextStyle) -> u8 {
    fonts.advance(ch, text_style_to_font_style(style))
}

#[inline]
fn text_style_to_font_style(style: TextStyle) -> Style {
    if style.heading {
        Style::Heading
    } else if style.bold {
        Style::Bold
    } else if style.italic {
        Style::Italic
    } else {
        Style::Regular
    }
}

/// Number of LineLayouts to reserve for an image block. Falls back
/// to `DEFAULT_IMG_H` when the image header didn't carry attribute
/// dimensions; the renderer can still letterbox if real dimensions
/// differ at decode time.
fn image_lines_for(image: Option<super::scan::ImageRef>, line_h: u16) -> u8 {
    let h = image
        .map(|img| if img.attr_h > 0 { img.attr_h } else { super::super::DEFAULT_IMG_H })
        .unwrap_or(super::super::DEFAULT_IMG_H);
    let lh = line_h.max(1);
    let lines = h.div_ceil(lh);
    lines.clamp(1, u8::MAX as u16) as u8
}

fn check_cap(out_lines: &Vec<LineLayout>) -> Result<(), TypesetError> {
    if out_lines.len() > MAX_LINES_PER_CHAPTER {
        Err(TypesetError::LineCapExceeded)
    } else {
        Ok(())
    }
}

fn ensure_parallel(out_image_blocks: &mut Vec<u8>, target_len: usize) {
    while out_image_blocks.len() < target_len {
        out_image_blocks.push(0);
    }
}
