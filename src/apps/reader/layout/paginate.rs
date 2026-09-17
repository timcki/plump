//! Paginator + convert adapters.
//!
//! `paginate()` turns a chapter line table into a page table,
//! honoring forced page breaks, image-block atomicity, paragraph
//! spacing and a lightweight widow/orphan post-pass. Page fill is
//! counted in quarter-lines: a line costs four, plus the gap above it
//! (its block record's `space_above`, converted with the current font
//! metrics) unless it is the first line on the page, where the gap is
//! dropped the way TeX drops glue at the top of a page.
//!
//! `convert::append_lines` translates one paragraph's K-P
//! `BreakChoice`s into `LineLayout` records, packing per-gap
//! justification spare into `LineLayout::extra` and the paragraph's
//! block properties into `indent` / `align`.
//!
//! `convert::append_greedy_fallback` is the in-pipeline fallback
//! used when the K-P breaker rejects a paragraph (oversized item
//! list, no feasible breaks). It does first-fit over the same
//! items the K-P stage already built, so it has no font lookups
//! and no image-fit logic of its own (images are already their
//! own paragraphs at the items.rs layer).

use alloc::vec::Vec;

use smol_epub::markup::{Align, Style};

use super::items::{Item, ItemKind, ParagraphMeta};
use super::{LineLayout, PageLayout};

// ── pagination ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaginateError {
    EmptyChapter,
    TooManyPages,
}

/// The font metrics that turn a line's quarter-em gap into page fill.
#[derive(Clone, Copy, Debug)]
pub struct PageSpacing {
    pub em_px: u16,
    pub line_h: u16,
}

impl PageSpacing {
    #[inline]
    pub fn gap_quarters(&self, qem: u8) -> u16 {
        super::gap_quarters(qem, self.em_px, self.line_h)
    }
}

/// quarter-lines one line of text occupies without its gap
const LINE_Q: u16 = 4;

/// fill cost of `line` in quarter-lines; the gap above only counts
/// when the line is not the first on its page
#[inline]
fn line_cost_q(line: &LineLayout, first_on_page: bool, spacing: &PageSpacing) -> u16 {
    if first_on_page {
        LINE_Q
    } else {
        LINE_Q + spacing.gap_quarters(line.gap_qem())
    }
}

/// total fill of one page in quarter-lines
fn page_fill_q(lines: &[LineLayout], page: &PageLayout, spacing: &PageSpacing) -> u16 {
    let first = page.first_line as usize;
    let end = (first + page.line_count as usize).min(lines.len());
    let mut q = 0u16;
    for (k, line) in lines[first..end].iter().enumerate() {
        q = q.saturating_add(line_cost_q(line, k == 0, spacing));
    }
    q
}

/// Walk a chapter's `LineLayout` table and emit `PageLayout`s.
///
/// `image_block_lines[i] > 0` indicates that `lines[i]` is an
/// image-origin line and `image_block_lines[i]` consecutive entries
/// (origin + filler) must stay on the same page. For non-image
/// lines, `image_block_lines[i]` should be 0.
pub fn paginate(
    lines: &[LineLayout],
    max_lines: u8,
    image_block_lines: &[u8],
    spacing: PageSpacing,
    out_pages: &mut Vec<PageLayout>,
) -> Result<(), PaginateError> {
    if lines.is_empty() {
        return Err(PaginateError::EmptyChapter);
    }
    if image_block_lines.len() < lines.len() {
        // caller bug; refuse rather than panic
        return Err(PaginateError::EmptyChapter);
    }

    let cap = max_lines as usize;
    if cap == 0 {
        return Err(PaginateError::TooManyPages);
    }
    let cap_q = max_lines as u16 * LINE_Q;
    if out_pages
        .try_reserve_exact(lines.len().div_ceil(cap.max(1)))
        .is_err()
    {
        return Err(PaginateError::TooManyPages);
    }

    let mut i = 0usize;
    while i < lines.len() {
        let page_start = i;
        let mut count = 0usize;
        let mut used_q = 0u16;

        while i < lines.len() && count < cap {
            let line = &lines[i];

            // forced page break before this line: end current page
            if count > 0 && line.is_page_break_before() {
                break;
            }

            // image atomicity
            let block = if line.is_image() {
                image_block_lines[i].max(1) as usize
            } else {
                1
            };
            let cost = line_cost_q(line, count == 0, &spacing)
                .saturating_add((block as u16 - 1) * LINE_Q);

            // page-fit: an image block bigger than the page is emitted
            // on its own page anyway (renderer clips bottom)
            if cost > cap_q {
                if count > 0 {
                    break; // flush current page first
                }
                count = block.min(cap);
                i += block;
                break;
            }

            if used_q + cost > cap_q {
                break;
            }

            used_q += cost;
            count += block;
            i += block;
        }

        if count == 0 {
            // single oversized image landed on its own page above; we
            // already advanced `i`. guard the empty case anyway.
            count = (i - page_start).max(1);
        }

        let last_idx = (page_start + count).min(lines.len()) - 1;
        out_pages.push(PageLayout {
            first_line: page_first_line_to_u16(page_start),
            line_count: count.min(u8::MAX as usize) as u8,
            flags: 0,
            start_byte: lines[page_start].start_byte,
            end_byte: lines[last_idx].end_byte,
        });

        if out_pages.len() > super::super::MAX_PAGES {
            return Err(PaginateError::TooManyPages);
        }
    }

    apply_widow_orphan(lines, image_block_lines, max_lines, spacing, out_pages);

    Ok(())
}

#[inline]
fn page_first_line_to_u16(idx: usize) -> u16 {
    idx.min(u16::MAX as usize) as u16
}

/// Widow / orphan post-pass: at most one line moves from the end of
/// a page to the start of the next.
///
/// WIDOW: the last line of a paragraph would sit alone at the top of a
/// page. Pull the previous line down with it.
///
/// ORPHAN: the first line of a paragraph would sit alone at the bottom
/// of a page. Move it forward to the next page (creates a slightly
/// short page above; acceptable).
///
/// Both adjustments only fire when the previous page has more than
/// `max_lines / 2` lines, to avoid cascading shrinkage, and only when
/// the receiving page has room in quarter-lines for the moved line and
/// for the gap its old first line regains. Headings are skipped
/// (heading-on-its-own is intentional).
fn apply_widow_orphan(
    lines: &[LineLayout],
    image_block_lines: &[u8],
    max_lines: u8,
    spacing: PageSpacing,
    pages: &mut [PageLayout],
) {
    if pages.len() < 2 {
        return;
    }
    let half = (max_lines as usize) / 2;
    let cap_q = max_lines as u16 * LINE_Q;

    for p in 1..pages.len() {
        let prev = pages[p - 1];
        let cur = pages[p];
        let cur_first = cur.first_line as usize;
        let prev_last = (prev.first_line as usize) + (prev.line_count as usize) - 1;

        if prev.line_count as usize <= half + 1 {
            continue; // previous page already short; don't shrink further
        }

        // never grow the current page past capacity: the renderer
        // positions lines from the top, so an overfull page prints its
        // extra line over the footer chrome. the moved line becomes the
        // new first line (no gap) and the old first line regains its gap
        let grown_q = page_fill_q(lines, &cur, &spacing)
            .saturating_add(LINE_Q)
            .saturating_add(spacing.gap_quarters(lines[cur_first].gap_qem()));
        if grown_q > cap_q {
            continue;
        }

        // Skip if either side touches an image block (image atomicity wins).
        if cur_first < image_block_lines.len() && image_block_lines[cur_first] > 0 {
            continue;
        }
        if prev_last < image_block_lines.len() && image_block_lines[prev_last] > 0 {
            continue;
        }

        // WIDOW: cur first line is the LAST of a paragraph and prev's
        // last line is in the SAME paragraph (i.e. prev_last is not
        // a paragraph end). Demote prev_last to current page.
        let cur_first_line = &lines[cur_first];
        let prev_last_line = &lines[prev_last];
        let widow_here = cur_first_line.is_paragraph_end()
            && !prev_last_line.is_paragraph_end()
            && !cur_first_line.is_heading();
        if widow_here {
            pages[p - 1].line_count -= 1;
            pages[p - 1].end_byte = lines[prev_last - 1].end_byte;
            pages[p].first_line -= 1;
            pages[p].line_count += 1;
            pages[p].start_byte = lines[prev_last].start_byte;
            continue;
        }

        // ORPHAN: prev_last is the FIRST line of a paragraph that
        // continues into the next page. Demote prev_last to current page
        // so the paragraph starts cleanly on the new page.
        // We detect "first line of paragraph" by looking at prev_last - 1:
        // if it's a paragraph end (or prev_last is page-start), prev_last
        // is the start of a new paragraph.
        let is_paragraph_start = prev_last == prev.first_line as usize
            || lines[prev_last - 1].is_paragraph_end();
        if is_paragraph_start
            && !prev_last_line.is_paragraph_end()
            && !prev_last_line.is_heading()
        {
            pages[p - 1].line_count -= 1;
            pages[p - 1].end_byte = lines[prev_last - 1].end_byte;
            pages[p].first_line -= 1;
            pages[p].line_count += 1;
            pages[p].start_byte = lines[prev_last].start_byte;
        }
    }
}

// ── convert: K-P choices → LineLayouts ────────────────────────────

pub mod convert {
    use super::*;
    use crate::apps::reader::layout::breaker::{Adjustment, BreakChoice, ChoiceFlags};

    /// Translate one paragraph's break choices into `LineLayout`s.
    ///
    /// `page_break_pending` is consumed: if `true` on entry, the first
    /// emitted line is stamped with `FLAG_PAGE_BREAK_BEFORE` and the
    /// flag is cleared. The caller threads this across paragraphs.
    pub fn append_lines(
        items: &[Item],
        choices: &[BreakChoice],
        meta: &ParagraphMeta,
        // kept on the API surface for future per-line diagnostics
        _line_width: u16,
        page_break_pending: &mut bool,
        out: &mut Vec<LineLayout>,
    ) {
        let mut prev_idx: Option<usize> = None;
        for (line_idx, ch) in choices.iter().enumerate() {
            let item_idx = ch.item_idx as usize;
            let lo = prev_idx.map(|x| x + 1).unwrap_or(0);

            let start_byte = items
                .get(lo)
                .map(|it| it.byte_offset)
                .unwrap_or(meta.byte_start);
            let end_byte = items
                .get(item_idx)
                .map(|it| it.byte_offset)
                .unwrap_or(meta.byte_end);

            let start_style = first_box_style(items, lo, item_idx);
            let mut flags = LineLayout::style_flags(start_style);

            if *page_break_pending && line_idx == 0 {
                flags |= LineLayout::FLAG_PAGE_BREAK_BEFORE;
                *page_break_pending = false;
            }
            if (ch.flags.contains(ChoiceFlags::LAST_LINE)
                || ch.flags.contains(ChoiceFlags::FORCED_BREAK))
                && line_idx + 1 == choices.len()
            {
                flags |= LineLayout::FLAG_PARAGRAPH_END;
            }

            // `no_stretch` covers the typography rule: don't fully-justify
            // a paragraph's trailing line and don't stretch headings.
            // shrink is preserved regardless — K-P emits it precisely
            // because the natural width overshoots the column, and
            // dropping it would leave the line clipping past the margin
            // (the common Stories-of-Your-Life single-line shrink-fit
            // paragraph). see `encode_extra` for the per-direction split.
            let no_stretch = (flags
                & (LineLayout::FLAG_PARAGRAPH_END | LineLayout::FLAG_HEADING))
                != 0;
            let gap_count = count_gaps(items, lo, item_idx);
            let extra = encode_extra(ch, gap_count, no_stretch);

            let first = line_idx == 0;
            out.push(LineLayout {
                start_byte,
                end_byte,
                flags,
                indent: LineLayout::pack_indent(
                    meta.block.left,
                    if first { meta.block.text_indent_qem } else { 0 },
                ),
                align: LineLayout::pack_align(
                    meta.block.align,
                    if first { meta.block.space_above_qem } else { 0 },
                    start_style,
                ),
                extra,
            });
            prev_idx = Some(item_idx);
        }
    }

    /// First-fit fallback over the same items the K-P stage already
    /// built. Used when `break_paragraph_with_fallback` returns `Err`.
    pub fn append_greedy_fallback(
        items: &[Item],
        meta: &ParagraphMeta,
        line_width: u16,
        page_break_pending: &mut bool,
        out: &mut Vec<LineLayout>,
    ) {
        if items.is_empty() {
            return;
        }
        let lw = line_width as u32;
        let mut line_start_idx = 0usize;
        let mut last_glue_idx: Option<usize> = None;
        let mut cursor: u32 = 0;
        let mut emitted_any = false;

        let mut i = 0;
        while i < items.len() {
            let it = items[i];
            match it.kind() {
                ItemKind::Box => {
                    cursor = cursor.saturating_add(it.width as u32);
                    if cursor > lw {
                        let break_at = last_glue_idx.unwrap_or(i);
                        emit_fallback_line(
                            items,
                            line_start_idx,
                            break_at,
                            meta,
                            page_break_pending,
                            !emitted_any,
                            false,
                            out,
                        );
                        emitted_any = true;
                        line_start_idx = break_at + 1;
                        last_glue_idx = None;
                        cursor = sum_widths(items, line_start_idx, i + 1);
                    }
                }
                ItemKind::Glue => {
                    cursor = cursor.saturating_add(it.width as u32);
                    last_glue_idx = Some(i);
                }
                ItemKind::Penalty => {
                    if it.is_forced() {
                        emit_fallback_line(
                            items,
                            line_start_idx,
                            i,
                            meta,
                            page_break_pending,
                            !emitted_any,
                            true,
                            out,
                        );
                        emitted_any = true;
                        line_start_idx = i + 1;
                        last_glue_idx = None;
                        cursor = 0;
                    }
                }
            }
            i += 1;
        }

        // Tail: anything still buffered (forgive trailing greed since
        // the K-P pipeline always closes paragraphs with a forced
        // break, but be defensive in case of malformed input).
        if line_start_idx < items.len() {
            emit_fallback_line(
                items,
                line_start_idx,
                items.len() - 1,
                meta,
                page_break_pending,
                !emitted_any,
                true,
                out,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_fallback_line(
        items: &[Item],
        lo: usize,
        breakpoint: usize,
        meta: &ParagraphMeta,
        page_break_pending: &mut bool,
        is_first_line_of_paragraph: bool,
        is_last_line: bool,
        out: &mut Vec<LineLayout>,
    ) {
        let start_byte = items
            .get(lo)
            .map(|it| it.byte_offset)
            .unwrap_or(meta.byte_start);
        let end_byte = items
            .get(breakpoint)
            .map(|it| it.byte_offset)
            .unwrap_or(meta.byte_end);

        let start_style = first_box_style(items, lo, breakpoint);
        let mut flags = LineLayout::style_flags(start_style);
        if is_first_line_of_paragraph && *page_break_pending {
            flags |= LineLayout::FLAG_PAGE_BREAK_BEFORE;
            *page_break_pending = false;
        }
        if is_last_line {
            flags |= LineLayout::FLAG_PARAGRAPH_END;
        }

        let first = is_first_line_of_paragraph;
        out.push(LineLayout {
            start_byte,
            end_byte,
            flags,
            indent: LineLayout::pack_indent(
                meta.block.left,
                if first { meta.block.text_indent_qem } else { 0 },
            ),
            align: LineLayout::pack_align(
                meta.block.align,
                if first { meta.block.space_above_qem } else { 0 },
                start_style,
            ),
            extra: 0, // greedy doesn't justify
        });
    }

    /// Append an image-paragraph LineLayout (one origin line + filler
    /// fillers). The origin's byte range is the whole IMG_REF record,
    /// marker included, so a page that starts on the image loads the
    /// header, alt text and path into its buffer.
    ///
    /// `reserved_h` is the pixel height the block reserves; it rides
    /// the origin's `extra` byte in 4 px units so a later spacing
    /// change can rebuild the filler count without re-typesetting.
    pub fn append_image_block(
        meta: &ParagraphMeta,
        image_lines: u8,
        reserved_h: u16,
        page_break_pending: &mut bool,
        out: &mut Vec<LineLayout>,
    ) -> Option<()> {
        let img = meta.image?;
        let mut flags = LineLayout::FLAG_IMAGE | LineLayout::FLAG_PARAGRAPH_END;
        if *page_break_pending {
            flags |= LineLayout::FLAG_PAGE_BREAK_BEFORE;
            *page_break_pending = false;
        }
        out.push(LineLayout {
            start_byte: img.start,
            end_byte: img.end,
            flags,
            indent: 0,
            align: LineLayout::pack_align(
                Align::Default,
                meta.block.space_above_qem,
                Style::PLAIN,
            ),
            extra: reserved_h.div_ceil(4).min(u8::MAX as u16) as u8,
        });
        // filler lines (start/end zero; renderer treats len==0 + IMAGE flag as filler)
        for _ in 1..image_lines {
            out.push(LineLayout {
                start_byte: 0,
                end_byte: 0,
                flags: LineLayout::FLAG_IMAGE,
                indent: 0,
                align: LineLayout::ALIGN_DEFAULT,
                extra: 0,
            });
        }
        Some(())
    }

    /// the style of the first Box in `lo..=breakpoint`: what the line's
    /// first glyph is drawn in. a line whose first Box is bold and whose
    /// later Boxes are regular still carries bold here; the renderer's
    /// decoder flips back when it meets the close marker mid-line
    fn first_box_style(items: &[Item], lo: usize, breakpoint: usize) -> Style {
        for it in &items[lo..=breakpoint.min(items.len().saturating_sub(1))] {
            if matches!(it.kind(), ItemKind::Box) {
                return it.style();
            }
        }
        Style::PLAIN
    }

    fn count_gaps(items: &[Item], lo: usize, breakpoint: usize) -> u16 {
        let mut count: u16 = 0;
        let hi = breakpoint.min(items.len());
        for it in &items[lo..hi] {
            if matches!(it.kind(), ItemKind::Glue) {
                count = count.saturating_add(1);
            }
        }
        count
    }

    fn sum_widths(items: &[Item], lo: usize, hi_exclusive: usize) -> u32 {
        let mut sum: u32 = 0;
        let hi = hi_exclusive.min(items.len());
        for it in &items[lo..hi] {
            if !matches!(it.kind(), ItemKind::Penalty) {
                sum = sum.saturating_add(it.width as u32);
            }
        }
        sum
    }

    fn encode_extra(choice: &BreakChoice, gap_count: u16, no_stretch: bool) -> u8 {
        if gap_count == 0 {
            return 0;
        }
        match choice.adjustment() {
            Adjustment::Overflow | Adjustment::Perfect => 0,
            // suppress stretch on paragraph-end and heading lines: those
            // either pick up u16::MAX from the forced-break glue (saturating
            // extra to +127 px-per-gap) or violate the typography rule
            // against fully-justifying a paragraph's trailing line.
            Adjustment::Stretch(_) if no_stretch => 0,
            Adjustment::Stretch(r_q8) => {
                let total_spare = (choice.stretch_total as u32 * r_q8 as u32) / 256;
                let per_gap = (total_spare / gap_count as u32).min(127) as u8;
                per_gap & LineLayout::EXTRA_MAG_MASK
            }
            Adjustment::Shrink(r_q8) => {
                // shrink is layout-driven (the paragraph won't fit otherwise);
                // preserve it even on paragraph-end / heading lines so the
                // renderer can squeeze the inter-word spaces. dropping the
                // signal here leaves the line overflowing the column.
                let total_squeeze = (choice.shrink_total as u32 * r_q8 as u32) / 256;
                let per_gap = (total_squeeze / gap_count as u32).min(127) as u8;
                (per_gap & LineLayout::EXTRA_MAG_MASK) | LineLayout::EXTRA_SIGN_SHRINK
            }
        }
    }

    // ── tests ──────────────────────────────────────────────────
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::apps::reader::layout::breaker::{BreakConfig, BreakScratch, break_paragraph};
        use crate::apps::reader::layout::items::ParagraphEnd;
        use smol_epub::markup::BlockProps;

        fn meta() -> ParagraphMeta {
            ParagraphMeta {
                block: BlockProps::DEFAULT,
                style_at_start: Style::PLAIN,
                end_kind: ParagraphEnd::ParagraphBreak,
                byte_start: 0,
                byte_end: 0,
                image: None,
            }
        }

        fn forced_triple() -> [Item; 3] {
            [
                Item::penalty(Item::PENALTY_FORBIDDEN, false, false, 100),
                Item::glue(0, u16::MAX, 0, 100),
                Item::penalty(Item::PENALTY_FORCE, false, true, 100),
            ]
        }

        #[test]
        fn append_lines_marks_paragraph_end_on_last_line() {
            let mut items = vec![
                Item::boxed(10, 0),
                Item::glue(5, 2, 1, 10),
                Item::boxed(10, 11),
            ];
            items.extend(forced_triple());

            let cfg = BreakConfig {
                line_width: 100,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut BreakScratch::new(), &mut choices).unwrap();

            let mut out = Vec::new();
            let mut pending = false;
            append_lines(&items, &choices, &meta(), 100, &mut pending, &mut out);
            assert!(out.last().unwrap().is_paragraph_end());
            assert_eq!(out.last().unwrap().extra, 0);
        }

        #[test]
        fn first_line_carries_block_indent_and_gap() {
            let mut items = vec![Item::boxed(10, 0), Item::glue(5, 2, 1, 10), Item::boxed(10, 11)];
            items.extend(forced_triple());
            let mut m = meta();
            m.block = BlockProps {
                align: Align::Center,
                left: 2,
                text_indent_qem: 6,
                space_above_qem: 4,
            };
            let mut out = Vec::new();
            let mut pending = false;
            append_greedy_fallback(&items, &m, 12, &mut pending, &mut out);
            assert!(out.len() >= 2);
            assert_eq!(out[0].left_levels(), 2);
            assert_eq!(out[0].first_indent_qem(), 6);
            assert_eq!(out[0].gap_qem(), 4);
            assert_eq!(out[0].align(), LineLayout::ALIGN_CENTER);
            assert_eq!(out[1].first_indent_qem(), 0);
            assert_eq!(out[1].gap_qem(), 0);
            assert_eq!(out[1].left_levels(), 2);
        }

        #[test]
        fn dropcap_first_line_flags_bold_and_underline() {
            let bold = Style {
                bold: true,
                underline: true,
                ..Style::PLAIN
            };
            let mut items = vec![
                Item::boxed(3, 0).with_style(bold),
                Item::boxed(15, 5),
                Item::glue(5, 2, 1, 20),
                Item::boxed(15, 21),
            ];
            items.extend(forced_triple());
            let cfg = BreakConfig {
                line_width: 100,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut BreakScratch::new(), &mut choices).unwrap();
            let mut out = Vec::new();
            let mut pending = false;
            append_lines(&items, &choices, &meta(), 100, &mut pending, &mut out);
            let first = out[0];
            assert!(first.flags & LineLayout::FLAG_BOLD != 0);
            assert!(first.starts_underline());
            assert_eq!(first.start_style(), bold);
        }

        #[test]
        fn heading_tiers_round_trip() {
            for lvl in 1..=6u8 {
                let s = Style {
                    heading: lvl,
                    ..Style::PLAIN
                };
                let ll = LineLayout {
                    flags: LineLayout::style_flags(s),
                    align: LineLayout::pack_align(Align::Default, 0, s),
                    ..LineLayout::EMPTY
                };
                assert_eq!(ll.start_style().heading, lvl.min(4));
            }
        }

        #[test]
        fn greedy_fallback_breaks_at_overflow() {
            let mut items = Vec::new();
            for k in 0..10 {
                items.push(Item::boxed(20, k as u32 * 100));
                items.push(Item::glue(5, 2, 1, k as u32 * 100 + 50));
            }
            let mut out = Vec::new();
            let mut pending = false;
            append_greedy_fallback(&items, &meta(), 50, &mut pending, &mut out);
            assert!(out.len() >= 4, "expected ≥4 lines, got {}", out.len());
            assert!(out.last().unwrap().is_paragraph_end());
        }
    }
}

// ── tests (paginate) ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SP: PageSpacing = PageSpacing { em_px: 20, line_h: 28 };

    fn line(start: u32, end: u32, flags: u8) -> LineLayout {
        LineLayout {
            start_byte: start,
            end_byte: end,
            flags,
            indent: 0,
            align: 0,
            extra: 0,
        }
    }

    #[test]
    fn empty_chapter_returns_error() {
        let mut pages = Vec::new();
        assert_eq!(paginate(&[], 10, &[], SP, &mut pages), Err(PaginateError::EmptyChapter));
    }

    #[test]
    fn fills_pages_to_max_lines() {
        let mut lines = Vec::new();
        for i in 0..25u32 {
            lines.push(line(i * 10, i * 10 + 5, LineLayout::FLAG_PARAGRAPH_END));
        }
        let img = vec![0u8; lines.len()];
        let mut pages = Vec::new();
        paginate(&lines, 10, &img, SP, &mut pages).unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].line_count, 10);
        assert_eq!(pages[2].line_count, 5);
    }

    #[test]
    fn gaps_take_page_room_except_on_the_first_line() {
        // 1 em gaps at 20 px/em over 28 px lines: 80/28 = 2.86 → 3
        // quarter-lines each. a 10-line page holds 40 quarters: the first
        // line costs 4, every later one 7, so 6 lines fit (4 + 5*7 = 39)
        let mut lines = Vec::new();
        for i in 0..12u32 {
            let mut l = line(i * 10, i * 10 + 5, LineLayout::FLAG_PARAGRAPH_END);
            l.align = LineLayout::pack_align(Align::Default, 4, Style::PLAIN);
            lines.push(l);
        }
        let img = vec![0u8; lines.len()];
        let mut pages = Vec::new();
        paginate(&lines, 10, &img, SP, &mut pages).unwrap();
        assert_eq!(pages[0].line_count, 6);
        assert_eq!(pages[1].line_count, 6);
    }

    #[test]
    fn forced_page_break_starts_new_page() {
        let mut lines = Vec::new();
        for i in 0..6u32 {
            lines.push(line(i * 10, i * 10 + 5, 0));
        }
        lines[3].flags |= LineLayout::FLAG_PAGE_BREAK_BEFORE;
        let img = vec![0u8; lines.len()];
        let mut pages = Vec::new();
        paginate(&lines, 10, &img, SP, &mut pages).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].line_count, 3);
        assert_eq!(pages[1].first_line, 3);
    }

    #[test]
    fn image_block_stays_together() {
        let mut lines = Vec::new();
        for i in 0..8u32 {
            lines.push(line(i * 10, i * 10 + 5, 0));
        }
        for i in 0..4u32 {
            lines.push(line(80 + i * 10, 80 + i * 10 + 5, LineLayout::FLAG_IMAGE));
        }
        let mut img = vec![0u8; lines.len()];
        img[8] = 4;
        let mut pages = Vec::new();
        paginate(&lines, 10, &img, SP, &mut pages).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].line_count, 8);
        assert_eq!(pages[1].line_count, 4);
    }

    #[test]
    fn widow_pulls_orphan_back() {
        let mut lines = Vec::new();
        for i in 0..8u32 {
            lines.push(line(i * 10, i * 10 + 5, 0));
        }
        lines[6].flags |= LineLayout::FLAG_PARAGRAPH_END;
        let img = vec![0u8; lines.len()];
        let mut pages = Vec::new();
        paginate(&lines, 6, &img, SP, &mut pages).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].line_count, 5);
        assert_eq!(pages[1].first_line, 5);
    }
}
