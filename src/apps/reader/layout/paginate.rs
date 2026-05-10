//! Paginator + convert adapters.
//!
//! `paginate()` turns a chapter line table into a page table,
//! honoring forced page breaks, image-block atomicity, and a
//! lightweight widow/orphan post-pass.
//!
//! `convert::append_lines` translates one paragraph's K-P
//! `BreakChoice`s into `LineLayout` records, packing per-gap
//! justification spare into `LineLayout::extra`.
//!
//! `convert::append_greedy_fallback` is the in-pipeline fallback
//! used when the K-P breaker rejects a paragraph (oversized item
//! list, no feasible breaks). It does first-fit over the same
//! items the K-P stage already built, so it has no font lookups
//! and no image-fit logic of its own (images are already their
//! own paragraphs at the items.rs layer).

use alloc::vec::Vec;

use super::items::{Item, ItemKind, ParagraphMeta};
use super::scan::BlockAlign;
use super::{LineLayout, PageLayout};

// ── pagination ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaginateError {
    EmptyChapter,
    TooManyPages,
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

            // page-fit: image block bigger than `cap` is emitted on
            // its own page anyway (renderer clips bottom)
            if block > cap {
                if count > 0 {
                    break; // flush current page first
                }
                count = block.min(cap);
                i += block;
                break;
            }

            if count + block > cap {
                break;
            }

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

    apply_widow_orphan(lines, image_block_lines, max_lines, out_pages);

    Ok(())
}

#[inline]
fn page_first_line_to_u16(idx: usize) -> u16 {
    idx.min(u16::MAX as usize) as u16
}

/// Single-pass widow/orphan adjustment.
///
/// Widow: a paragraph whose last line lands alone at the top of a
/// page, while the page above is more than half-full of the SAME
/// paragraph. Demote one line from the previous page so two move
/// together.
///
/// Orphan: a paragraph whose first line is the last line of a page,
/// while the paragraph has ≥3 lines. Move the orphan forward to the
/// next page (creates a slightly short page above; acceptable).
///
/// Both adjustments only fire when the previous page has more than
/// `max_lines / 2` lines, to avoid cascading shrinkage. Headings are
/// skipped (heading-on-its-own is intentional).
fn apply_widow_orphan(
    lines: &[LineLayout],
    image_block_lines: &[u8],
    max_lines: u8,
    pages: &mut [PageLayout],
) {
    if pages.len() < 2 {
        return;
    }
    let half = (max_lines as usize) / 2;

    for p in 1..pages.len() {
        let prev = pages[p - 1];
        let cur = pages[p];
        let cur_first = cur.first_line as usize;
        let prev_last = (prev.first_line as usize) + (prev.line_count as usize) - 1;

        if prev.line_count as usize <= half + 1 {
            continue; // previous page already short; don't shrink further
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
            // shrink previous page by 1, grow current page by 1
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

            let style_byte = first_box_style(items, lo, item_idx);
            let mut flags = style_byte;

            if *page_break_pending && line_idx == 0 {
                flags |= LineLayout::FLAG_PAGE_BREAK_BEFORE;
                *page_break_pending = false;
            }
            if ch.flags.contains(ChoiceFlags::LAST_LINE) || ch.flags.contains(ChoiceFlags::FORCED_BREAK)
            {
                if line_idx + 1 == choices.len() {
                    flags |= LineLayout::FLAG_PARAGRAPH_END;
                }
            }

            // skip computing per-gap stretch on lines the renderer
            // will never justify: paragraph-end (END_HARD) and headings.
            // mirrors LineLayout::may_justify so cached extra matches
            // runtime behaviour. without this, paragraph-final lines
            // pick up u16::MAX stretch from the forced-break glue and
            // saturate extra to +127 px-per-gap, which is wrong even
            // though the renderer currently ignores it.
            let no_justify = (flags
                & (LineLayout::FLAG_PARAGRAPH_END | LineLayout::FLAG_HEADING))
                != 0;
            let extra = if no_justify {
                0
            } else {
                let gap_count = count_gaps(items, lo, item_idx);
                encode_extra(ch, gap_count)
            };

            out.push(LineLayout {
                start_byte,
                end_byte,
                flags,
                indent: meta.block.indent.min(u8::MAX),
                align: align_to_u8(meta.block.align),
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

        let mut flags = first_box_style(items, lo, breakpoint);
        if is_first_line_of_paragraph && *page_break_pending {
            flags |= LineLayout::FLAG_PAGE_BREAK_BEFORE;
            *page_break_pending = false;
        }
        if is_last_line {
            flags |= LineLayout::FLAG_PARAGRAPH_END;
        }

        out.push(LineLayout {
            start_byte,
            end_byte,
            flags,
            indent: meta.block.indent.min(u8::MAX),
            align: align_to_u8(meta.block.align),
            extra: 0, // greedy doesn't justify
        });
    }

    /// Append an image-paragraph LineLayout (one origin line + filler
    /// fillers) for an image whose alt text and path live at the
    /// stored byte offsets in the chapter buffer. The caller already
    /// has the `ImageRef` from `ParagraphMeta::image`.
    pub fn append_image_block(
        meta: &ParagraphMeta,
        image_lines: u8,
        page_break_pending: &mut bool,
        out: &mut Vec<LineLayout>,
    ) -> Option<()> {
        let img = meta.image?;
        let mut flags = LineLayout::FLAG_IMAGE | LineLayout::FLAG_PARAGRAPH_END;
        if *page_break_pending {
            flags |= LineLayout::FLAG_PAGE_BREAK_BEFORE;
            *page_break_pending = false;
        }
        // origin line: start_byte points at path_start so the renderer
        // can reach the path slice; end_byte is one-past path end.
        // alt_len lives in `indent` (greedy convention preserved at
        // src/apps/reader/paging.rs:903 for renderer parity).
        out.push(LineLayout {
            start_byte: img.path_start,
            end_byte: img.path_start + img.path_len as u32,
            flags,
            indent: img.alt_len,
            align: LineLayout::ALIGN_DEFAULT,
            extra: 0,
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
        let _ = meta.end_kind; // ImageBlock; suppress warning
        Some(())
    }

    fn first_box_style(items: &[Item], lo: usize, breakpoint: usize) -> u8 {
        for k in lo..=breakpoint.min(items.len().saturating_sub(1)) {
            let it = &items[k];
            if matches!(it.kind(), ItemKind::Box) {
                let mut f: u8 = 0;
                if it.style_is_bold() {
                    f |= LineLayout::FLAG_BOLD;
                }
                if it.style_is_italic() {
                    f |= LineLayout::FLAG_ITALIC;
                }
                let tier = it.style_heading_tier();
                if tier != Item::STYLE_TIER_NONE {
                    f |= LineLayout::FLAG_HEADING;
                    f |= match tier {
                        Item::STYLE_TIER_H1 => LineLayout::HLEVEL_H1,
                        Item::STYLE_TIER_H2 => LineLayout::HLEVEL_H2,
                        _ => LineLayout::HLEVEL_H3,
                    };
                }
                return f;
            }
        }
        0
    }

    fn count_gaps(items: &[Item], lo: usize, breakpoint: usize) -> u16 {
        let mut count: u16 = 0;
        let hi = breakpoint.min(items.len());
        for k in lo..hi {
            if matches!(items[k].kind(), ItemKind::Glue) {
                count = count.saturating_add(1);
            }
        }
        count
    }

    fn sum_widths(items: &[Item], lo: usize, hi_exclusive: usize) -> u32 {
        let mut sum: u32 = 0;
        let hi = hi_exclusive.min(items.len());
        for k in lo..hi {
            let it = items[k];
            if !matches!(it.kind(), ItemKind::Penalty) {
                sum = sum.saturating_add(it.width as u32);
            }
        }
        sum
    }

    fn encode_extra(choice: &BreakChoice, gap_count: u16) -> u8 {
        if gap_count == 0 {
            return 0;
        }
        match choice.adjustment() {
            Adjustment::Overflow | Adjustment::Perfect => 0,
            Adjustment::Stretch(r_q8) => {
                let total_spare = (choice.stretch_total as u32 * r_q8 as u32) / 256;
                let per_gap = (total_spare / gap_count as u32).min(127) as u8;
                per_gap & LineLayout::EXTRA_MAG_MASK
            }
            Adjustment::Shrink(r_q8) => {
                let total_squeeze = (choice.shrink_total as u32 * r_q8 as u32) / 256;
                let per_gap = (total_squeeze / gap_count as u32).min(127) as u8;
                (per_gap & LineLayout::EXTRA_MAG_MASK) | LineLayout::EXTRA_SIGN_SHRINK
            }
        }
    }

    fn align_to_u8(align: BlockAlign) -> u8 {
        match align {
            BlockAlign::Default => LineLayout::ALIGN_DEFAULT,
            BlockAlign::Left => LineLayout::ALIGN_LEFT,
            BlockAlign::Center => LineLayout::ALIGN_CENTER,
            BlockAlign::Right => LineLayout::ALIGN_RIGHT,
            BlockAlign::Justify => LineLayout::ALIGN_DEFAULT,
        }
    }

    // ── tests ──────────────────────────────────────────────────
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::apps::reader::layout::breaker::{break_paragraph, BreakConfig};
        use crate::apps::reader::layout::items::ParagraphEnd;
        use crate::apps::reader::layout::scan::{BlockState, TextStyle};

        fn meta() -> ParagraphMeta {
            ParagraphMeta {
                block: BlockState::default(),
                style_at_start: TextStyle::default(),
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
            let mut items = Vec::new();
            items.push(Item::boxed(10, 0));
            items.push(Item::glue(5, 2, 1, 10));
            items.push(Item::boxed(10, 11));
            items.extend(forced_triple());

            let cfg = BreakConfig {
                line_width: 100,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut choices).unwrap();

            let mut out = Vec::new();
            let mut pending = false;
            append_lines(&items, &choices, &meta(), &mut pending, &mut out);
            assert_eq!(out.len(), choices.len());
            assert!(out.last().unwrap().is_paragraph_end());
        }

        #[test]
        fn append_lines_consumes_page_break_pending() {
            let mut items = Vec::new();
            items.push(Item::boxed(10, 0));
            items.extend(forced_triple());
            let cfg = BreakConfig {
                line_width: 100,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut choices).unwrap();

            let mut out = Vec::new();
            let mut pending = true;
            append_lines(&items, &choices, &meta(), &mut pending, &mut out);
            assert!(out[0].is_page_break_before());
            assert!(!pending);
        }

        #[test]
        fn paragraph_end_line_has_zero_extra() {
            // a short paragraph fitting on one line. The K-P forced-break
            // triple carries u16::MAX stretch on its terminal glue, which
            // (before the no-justify guard) inflated encode_extra to the
            // +127 cap even though the renderer never justifies
            // paragraph-end lines.
            let mut items = Vec::new();
            items.push(Item::boxed(10, 0));
            items.push(Item::glue(5, 2, 1, 10));
            items.push(Item::boxed(10, 11));
            items.extend(forced_triple());

            let cfg = BreakConfig {
                line_width: 464,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut choices).unwrap();

            let mut out = Vec::new();
            let mut pending = false;
            append_lines(&items, &choices, &meta(), &mut pending, &mut out);
            let last = out.last().unwrap();
            assert!(last.is_paragraph_end());
            assert_eq!(
                last.extra, 0,
                "paragraph-end extra must be 0, got {:#x}",
                last.extra
            );
        }

        #[test]
        fn empty_paragraph_has_zero_extra() {
            // the exact pattern seen in Leviathan ch11 line 1: a paragraph
            // that produces no Words/Spaces, only the forced-break triple.
            let mut items = Vec::new();
            items.extend(forced_triple());

            let cfg = BreakConfig {
                line_width: 464,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut choices).unwrap();

            let mut out = Vec::new();
            let mut pending = false;
            append_lines(&items, &choices, &meta(), &mut pending, &mut out);
            // breaker may emit 0 lines for an empty paragraph; if it
            // emits any, none may have non-zero extra.
            for ll in &out {
                assert_eq!(ll.extra, 0, "empty-paragraph line extra must be 0");
            }
        }

        #[test]
        fn heading_line_has_zero_extra() {
            // headings render in the heading font with no justification.
            // The cached extra must reflect that.
            let mut items = Vec::new();
            let heading_style = TextStyle {
                heading: true,
                hlevel: 2,
                ..TextStyle::default()
            };
            items.push(Item::boxed(40, 0).with_style(heading_style));
            items.push(Item::glue(5, 2, 1, 40).with_style(heading_style));
            items.push(Item::boxed(40, 50).with_style(heading_style));
            items.extend(forced_triple());

            let cfg = BreakConfig {
                line_width: 464,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut choices).unwrap();

            let mut out = Vec::new();
            let mut pending = false;
            append_lines(&items, &choices, &meta(), &mut pending, &mut out);
            for ll in &out {
                assert!(ll.is_heading() || ll.is_paragraph_end());
                assert_eq!(ll.extra, 0, "heading line extra must be 0");
            }
        }

        #[test]
        fn dropcap_first_line_flags_bold() {
            // A line whose first Box is the bold drop-cap "M" and whose
            // subsequent Boxes are regular. first_box_style must return
            // FLAG_BOLD so the renderer's initial sty is Bold; the
            // renderer then re-walks markers and flips to Regular at the
            // BOLD_OFF marker inside the line's byte range.
            let bold_style = TextStyle {
                bold: true,
                ..TextStyle::default()
            };
            let regular = TextStyle::default();
            let mut items = Vec::new();
            // bold "M" at byte 0
            items.push(Item::boxed(3, 0).with_style(bold_style));
            // (no glue; markers join the words logically — the BOLD_OFF
            // marker bytes live between the two boxes in the source
            // stream but are zero-width in the K-P model.)
            items.push(Item::boxed(15, 5).with_style(regular));
            items.extend(forced_triple());

            let cfg = BreakConfig {
                line_width: 464,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut choices).unwrap();

            let mut out = Vec::new();
            let mut pending = false;
            append_lines(&items, &choices, &meta(), &mut pending, &mut out);
            let first = &out[0];
            assert!(
                first.flags & LineLayout::FLAG_BOLD != 0,
                "drop-cap line must carry FLAG_BOLD (got flags={:#x})",
                first.flags
            );
            assert!(first.is_paragraph_end(), "single-line paragraph is paragraph-end");
            assert_eq!(first.extra, 0, "paragraph-end ⇒ extra zeroed");
        }

        #[test]
        fn mid_paragraph_line_can_carry_extra() {
            // a long paragraph forced into multiple lines: intermediate
            // (non-final) lines may carry per-gap stretch from the K-P
            // adjustment ratio. Confirms the no-justify guard isn't
            // over-zealous and zeroing every line.
            let mut items = Vec::new();
            for k in 0..16u32 {
                items.push(Item::boxed(20, k * 100));
                items.push(Item::glue(5, 3, 2, k * 100 + 50));
            }
            items.extend(forced_triple());

            let cfg = BreakConfig {
                line_width: 80,
                ..BreakConfig::DEFAULT
            };
            let mut choices = Vec::new();
            break_paragraph(&items, &cfg, &mut choices).unwrap();

            let mut out = Vec::new();
            let mut pending = false;
            append_lines(&items, &choices, &meta(), &mut pending, &mut out);
            assert!(out.len() >= 4, "expected multi-line break, got {}", out.len());
            // last line is paragraph-end → extra=0 (the fix).
            assert_eq!(out.last().unwrap().extra, 0);
            // at least one intermediate line should carry non-zero extra
            // (Stretch or Shrink adjustment); otherwise the breaker
            // produced only Perfect/Overflow which would be unusual.
            let any_extra = out.iter().take(out.len() - 1).any(|ll| ll.extra != 0);
            assert!(
                any_extra,
                "expected at least one mid-paragraph line with non-zero extra"
            );
        }

        // ── marker-replay contract (Phase 3) ──────────────────────
        //
        // The renderer's draw loop (`mod.rs` around line 2989) initialises
        // its style from `LineSpan::style()` and re-walks marker bytes
        // per glyph, swapping the active font at every `0x01 X` marker.
        // The cache stamps the line's *initial* style into
        // `LineLayout.flags` via `first_box_style`. This test pair locks
        // the contract: given a synthetic line, the (position, style)
        // trace produced by replaying markers against the initial style
        // must match what the renderer would draw.

        /// Style enum mirroring `fonts::Style`. Kept local so this test
        /// has no dependency on the rendering crate.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum StyleTrace {
            Regular,
            Bold,
            Italic,
            Heading,
        }

        /// Walk `bytes` exactly the way `mod.rs:draw()` does for one
        /// line: start at `initial`, flip on inline-style markers, and
        /// emit a `(glyph_byte_index, style)` for every printable byte.
        fn replay_markers(bytes: &[u8], initial: StyleTrace) -> Vec<(usize, StyleTrace)> {
            use smol_epub::html_strip::{
                BOLD_OFF, BOLD_ON, H1_OFF, H1_ON, H2_OFF, H2_ON, H3_OFF, H3_ON, H4_OFF, H4_ON,
                H5_OFF, H5_ON, H6_OFF, H6_ON, ITALIC_OFF, ITALIC_ON, MARKER,
            };
            let mut sty = initial;
            let mut out = Vec::new();
            let mut j = 0usize;
            while j < bytes.len() {
                let b = bytes[j];
                if b == MARKER && j + 1 < bytes.len() {
                    let tag = bytes[j + 1];
                    sty = match tag {
                        BOLD_ON => StyleTrace::Bold,
                        BOLD_OFF => StyleTrace::Regular,
                        ITALIC_ON => StyleTrace::Italic,
                        ITALIC_OFF => StyleTrace::Regular,
                        H1_ON | H2_ON | H3_ON | H4_ON | H5_ON | H6_ON => StyleTrace::Heading,
                        H1_OFF | H2_OFF | H3_OFF | H4_OFF | H5_OFF | H6_OFF => StyleTrace::Regular,
                        _ => sty, // unhandled marker (align, page break, etc.) leaves style alone
                    };
                    j += 2;
                    continue;
                }
                out.push((j, sty));
                j += 1;
            }
            out
        }

        #[test]
        fn marker_replay_dropcap_swaps_at_close() {
            // The Leviathan ch11 line 3 byte pattern (compressed):
            // "M<01>biller" where 'M' is the bold drop-cap (line start
            // is AFTER the BOLD_ON marker that lives upstream).
            // Initial style = Bold (LineLayout.flags carries FLAG_BOLD).
            use smol_epub::html_strip::{BOLD_OFF, MARKER};
            let mut line = Vec::new();
            line.push(b'M');
            line.push(MARKER);
            line.push(BOLD_OFF);
            line.extend_from_slice(b"iller");
            let trace = replay_markers(&line, StyleTrace::Bold);
            // 6 printable bytes: M, i, l, l, e, r. Only 'M' is bold;
            // the rest flip to Regular after the BOLD_OFF marker.
            assert_eq!(trace.len(), 6);
            assert_eq!(trace[0], (0, StyleTrace::Bold), "M must be bold");
            for (pos, sty) in &trace[1..] {
                assert_eq!(*sty, StyleTrace::Regular, "byte {} must be regular", pos);
            }
        }

        #[test]
        fn marker_replay_line_begins_mid_bold_span() {
            // A continuation line whose start_byte sits inside a bold
            // span that opened on a previous line. The line's bytes
            // contain no BOLD_OFF marker; the entire line draws bold.
            // first_box_style sets FLAG_BOLD ⇒ initial=Bold.
            let line = b"continuation in bold";
            let trace = replay_markers(line, StyleTrace::Bold);
            assert!(trace.iter().all(|(_, s)| *s == StyleTrace::Bold));
        }

        #[test]
        fn marker_replay_nested_italic_inside_bold() {
            // "bold <i>italic-in-bold</i> still-bold"
            // initial=Bold; ITALIC_ON flips to Italic; ITALIC_OFF flips
            // back to Regular (NOT Bold) — this is the current draw
            // loop's behaviour. Off markers reset to Regular; nesting
            // is flattened. The renderer doesn't maintain a style
            // stack — markers are commutative resets. Document the
            // contract here.
            use smol_epub::html_strip::{ITALIC_OFF, ITALIC_ON, MARKER};
            let mut line = Vec::new();
            line.extend_from_slice(b"bold ");
            line.push(MARKER);
            line.push(ITALIC_ON);
            line.extend_from_slice(b"em");
            line.push(MARKER);
            line.push(ITALIC_OFF);
            line.extend_from_slice(b" stl");
            let trace = replay_markers(&line, StyleTrace::Bold);
            // "bold " in Bold, "em" in Italic, " stl" in Regular (the
            // ITALIC_OFF reset overrides any prior bold).
            // This is a known limitation: nested styles can't survive
            // OFF markers without a style stack. The fix is on the
            // smol-epub side: emit the *outer* OPEN marker again when
            // closing an inner inline that overlapped. Documented for
            // future work — not addressed here.
            let style_at = |b: usize| trace.iter().find(|(p, _)| *p == b).map(|(_, s)| *s);
            assert_eq!(style_at(0), Some(StyleTrace::Bold)); // 'b' of "bold"
            assert_eq!(style_at(7), Some(StyleTrace::Italic)); // 'e' of "em"
            assert_eq!(style_at(12), Some(StyleTrace::Regular)); // ' ' before "stl"
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

        #[test]
        fn greedy_fallback_handles_forced_break_in_middle() {
            let mut items = Vec::new();
            items.push(Item::boxed(10, 0));
            items.push(Item::penalty(Item::PENALTY_FORCE, false, true, 11));
            items.push(Item::boxed(10, 12));
            items.extend(forced_triple());
            let mut out = Vec::new();
            let mut pending = false;
            append_greedy_fallback(&items, &meta(), 100, &mut pending, &mut out);
            assert!(out.len() >= 2);
        }
    }
}

// ── tests (paginate) ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(paginate(&[], 10, &[], &mut pages), Err(PaginateError::EmptyChapter));
    }

    #[test]
    fn fills_pages_to_max_lines() {
        let mut lines = Vec::new();
        for i in 0..25 {
            lines.push(line(i * 10, i * 10 + 5, LineLayout::FLAG_PARAGRAPH_END));
        }
        let img = vec![0u8; lines.len()];
        let mut pages = Vec::new();
        paginate(&lines, 10, &img, &mut pages).unwrap();
        assert_eq!(pages.len(), 3); // 10 + 10 + 5
        assert_eq!(pages[0].line_count, 10);
        assert_eq!(pages[1].line_count, 10);
        assert_eq!(pages[2].line_count, 5);
    }

    #[test]
    fn forced_page_break_starts_new_page() {
        let mut lines = Vec::new();
        for i in 0..5 {
            lines.push(line(i * 10, i * 10 + 5, 0));
        }
        // line 3 carries page-break-before
        lines[3].flags |= LineLayout::FLAG_PAGE_BREAK_BEFORE;
        let img = vec![0u8; lines.len()];
        let mut pages = Vec::new();
        paginate(&lines, 10, &img, &mut pages).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].line_count, 3);
        assert_eq!(pages[1].line_count, 2);
    }

    #[test]
    fn image_block_stays_together() {
        let mut lines = Vec::new();
        for i in 0..8 {
            lines.push(line(i * 10, i * 10 + 5, 0));
        }
        // line 7 is an image origin needing 5 filler lines (block of 5)
        // — we fudge by shrinking the lines so all 8 originals + 5 image
        // lines are present
        for i in 0..5 {
            lines.push(line(80 + i * 10, 80 + i * 10 + 5, LineLayout::FLAG_IMAGE));
        }
        // mark line 8 (first image filler) as the origin
        lines[8].flags |= LineLayout::FLAG_IMAGE;
        let mut img = vec![0u8; lines.len()];
        img[8] = 5; // image block = 5 lines starting at index 8
        let mut pages = Vec::new();
        // max_lines = 10. First 8 plain lines + 5 image lines = 13.
        // The image block should be pushed to its own page (8 + 5 > 10).
        paginate(&lines, 10, &img, &mut pages).unwrap();
        assert!(pages.len() >= 2);
        // image must not span pages: find which page contains line 8
        let img_page = pages
            .iter()
            .position(|p| {
                let first = p.first_line as usize;
                let last = first + p.line_count as usize;
                (8..8 + 5).all(|i| i >= first && i < last)
            })
            .expect("image block not contained on a single page");
        let _ = img_page;
    }

    #[test]
    fn widow_pulls_orphan_back() {
        // 11 lines total; max_lines=6 → 6 + 5 split.
        // line 5 is mid-paragraph (not paragraph end); line 6 IS paragraph end.
        // First page would end at line 5 (mid-para). Last line of paragraph
        // (line 6) lands alone on top of next page → widow → demote line 5.
        let mut lines = Vec::new();
        for i in 0..11 {
            lines.push(line(i * 10, i * 10 + 5, 0));
        }
        lines[6].flags |= LineLayout::FLAG_PARAGRAPH_END;
        let img = vec![0u8; lines.len()];
        let mut pages = Vec::new();
        paginate(&lines, 6, &img, &mut pages).unwrap();
        // After widow adjustment, page 0 should hold 5 lines and page 1 should have 6.
        // (Or we accept either as long as the paragraph end is not the only
        // paragraph-internal line on page 1.)
        assert!(pages.len() == 2);
        // page boundary must not split a paragraph such that its last line is alone
        let p1 = &pages[1];
        let p1_first = p1.first_line as usize;
        // p1's first line should NOT be a paragraph-end line directly preceded
        // by a non-paragraph-end on the previous page (i.e. not a widow)
        assert!(
            !(lines[p1_first].is_paragraph_end()
                && !lines[p1_first - 1].is_paragraph_end()
                && p1.line_count > 0),
            "widow detected after pagination",
        );
    }
}
