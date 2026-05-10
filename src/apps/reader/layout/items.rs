//! Knuth-Plass item builder.
//!
//! Walks the `MarkupScanner` token stream and emits one paragraph
//! worth of K-P items into a caller-provided `Vec<Item>`. The
//! breaker takes that slice as input. Items are 8 B packed so a
//! 2 KB working budget covers ~256 items per paragraph; truly
//! oversized paragraphs return `BreakError::ItemBudgetExceeded`
//! at the breaker stage.
//!
//! A paragraph ends at any of: `ParagraphBreak`, `PageBreak`,
//! `ThematicBreak`, `Image`, `BlockChanged`, or end-of-buffer.
//! The caller drives the loop, so block-state / page-break /
//! image dispatch happens at paragraph granularity.

use alloc::vec::Vec;

use plump_kernel::util::decode_utf8_char;

use super::scan::{BlockState, ImageRef, MarkupScanner, TextStyle, Token};

// ── Item ───────────────────────────────────────────────────────────

/// Knuth-Plass item, packed to 8 bytes.
///
/// `flags` byte layout:
///   bits 0-1  ItemKind discriminant (Box=0, Glue=1, Penalty=2)
///   bit 2     `flagged` (K-P flagged-penalty bit; informs hyphen demerit)
///   bit 3     `forced`  (forces a break here when set, even at +∞ stretch)
///   bit 4     STYLE_BOLD
///   bit 5     STYLE_ITALIC
///   bits 6-7  STYLE_HEADING_TIER (00=not heading, 01=H3, 10=H2, 11=H1)
///
/// For `Penalty` items, `width` carries the penalty cost as `i16`
/// (bit-reinterpret); `i16::MAX` means "forbidden break", `i16::MIN`
/// means "forced break". For `Box` and `Glue`, `width` is plain
/// unsigned px.
#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub width: u16,
    pub stretch: u16,
    pub shrink: u8,
    flags: u8,
    pub byte_offset: u32,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Box = 0,
    Glue = 1,
    Penalty = 2,
}

impl Item {
    const KIND_MASK: u8 = 0b11;
    const FLAG_FLAGGED: u8 = 1 << 2;
    const FLAG_FORCED: u8 = 1 << 3;

    pub const STYLE_BOLD: u8 = 1 << 4;
    pub const STYLE_ITALIC: u8 = 1 << 5;
    pub const STYLE_HEADING_TIER_SHIFT: u8 = 6;
    pub const STYLE_HEADING_TIER_MASK: u8 = 0b11 << Self::STYLE_HEADING_TIER_SHIFT;
    pub const STYLE_TIER_NONE: u8 = 0;
    pub const STYLE_TIER_H3: u8 = 1 << Self::STYLE_HEADING_TIER_SHIFT;
    pub const STYLE_TIER_H2: u8 = 2 << Self::STYLE_HEADING_TIER_SHIFT;
    pub const STYLE_TIER_H1: u8 = 3 << Self::STYLE_HEADING_TIER_SHIFT;

    /// "+∞" penalty: forbid a break here (used as the first item of
    /// the forced-break triple at paragraph end).
    pub const PENALTY_FORBIDDEN: i16 = i16::MAX;
    /// "−∞" penalty: forces a break here (used as the third item of
    /// the forced-break triple).
    pub const PENALTY_FORCE: i16 = i16::MIN;

    pub fn boxed(width: u16, byte_offset: u32) -> Self {
        Self {
            width,
            stretch: 0,
            shrink: 0,
            flags: ItemKind::Box as u8,
            byte_offset,
        }
    }

    pub fn glue(width: u16, stretch: u16, shrink: u8, byte_offset: u32) -> Self {
        Self {
            width,
            stretch,
            shrink,
            flags: ItemKind::Glue as u8,
            byte_offset,
        }
    }

    pub fn penalty(p: i16, flagged: bool, forced: bool, byte_offset: u32) -> Self {
        let mut flags = ItemKind::Penalty as u8;
        if flagged {
            flags |= Self::FLAG_FLAGGED;
        }
        if forced {
            flags |= Self::FLAG_FORCED;
        }
        Self {
            width: p as u16, // bit-reinterpret i16 → u16
            stretch: 0,
            shrink: 0,
            flags,
            byte_offset,
        }
    }

    #[inline]
    pub fn kind(&self) -> ItemKind {
        match self.flags & Self::KIND_MASK {
            0 => ItemKind::Box,
            1 => ItemKind::Glue,
            _ => ItemKind::Penalty,
        }
    }

    #[inline]
    pub fn is_flagged(&self) -> bool {
        self.flags & Self::FLAG_FLAGGED != 0
    }

    #[inline]
    pub fn is_forced(&self) -> bool {
        self.flags & Self::FLAG_FORCED != 0
    }

    /// Penalty cost, reinterpreting `width` as `i16`. Only meaningful
    /// when `kind() == ItemKind::Penalty`.
    #[inline]
    pub fn penalty_value(&self) -> i16 {
        self.width as i16
    }

    /// Stamp inline style (bold/italic/heading-tier) into the
    /// reserved bits. Used at item-build time so the convert adapter
    /// can pick the line-start style off the first Box of each line.
    pub fn with_style(mut self, style: TextStyle) -> Self {
        if style.bold {
            self.flags |= Self::STYLE_BOLD;
        }
        if style.italic {
            self.flags |= Self::STYLE_ITALIC;
        }
        if style.heading {
            let tier = match style.hlevel {
                1 => Self::STYLE_TIER_H1,
                2 => Self::STYLE_TIER_H2,
                _ => Self::STYLE_TIER_H3,
            };
            self.flags |= tier;
        }
        self
    }

    #[inline]
    pub fn style_is_bold(&self) -> bool {
        self.flags & Self::STYLE_BOLD != 0
    }
    #[inline]
    pub fn style_is_italic(&self) -> bool {
        self.flags & Self::STYLE_ITALIC != 0
    }
    /// Returns the heading tier byte (`STYLE_TIER_NONE` / `H3` / `H2` / `H1`)
    /// already shifted into bits 6-7 — convenient to OR into a LineLayout
    /// flags byte after masking.
    #[inline]
    pub fn style_heading_tier(&self) -> u8 {
        self.flags & Self::STYLE_HEADING_TIER_MASK
    }
}

// ── ParagraphMeta ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParagraphEnd {
    EndOfBuffer,
    ParagraphBreak,
    /// next paragraph's first line should carry FLAG_PAGE_BREAK_BEFORE
    PageBreakAfter,
    /// caller emits an image LineLayout block; no items were produced
    ImageBlock,
    /// a `<hr>`-style break; caller may emit a spacer line if desired
    ThematicBreak,
    /// indent / align changed; new paragraph picks up the new BlockState
    BlockChanged,
}

#[derive(Clone, Copy, Debug)]
pub struct ParagraphMeta {
    pub block: BlockState,
    pub style_at_start: TextStyle,
    pub end_kind: ParagraphEnd,
    pub byte_start: u32,
    pub byte_end: u32,
    /// populated when `end_kind == ImageBlock`
    pub image: Option<ImageRef>,
}

// ── tunables ──────────────────────────────────────────────────────

/// Penalty for accepting a soft-hyphen break. Small positive cost so
/// the breaker prefers no-hyphen lines unless they meaningfully reduce
/// badness.
pub const HYPHEN_PENALTY: i16 = 50;

// ── builder ───────────────────────────────────────────────────────

/// Drive the scanner until one paragraph is consumed; emit its K-P
/// items into `out`. Returns metadata about the paragraph boundary.
///
/// The caller is expected to clear `out` before each call.
///
/// `advance` is called per character of every Word token to derive
/// natural width. Implementations should call `FontSet::advance(ch,
/// style)`; the closure form keeps this module free of font/HAL
/// imports for host testability.
pub fn build_paragraph(
    scanner: &mut MarkupScanner<'_>,
    mut advance: impl FnMut(char, TextStyle) -> u16,
    out: &mut Vec<Item>,
) -> ParagraphMeta {
    let block_at_start = scanner.block_state();
    let style_at_start = scanner.text_style();
    let byte_start = scanner.position();
    let buf = scanner.buffer();
    let mut last_end: u32 = byte_start;

    loop {
        let Some(tok) = scanner.next() else {
            return finish(
                out,
                byte_start,
                last_end,
                block_at_start,
                style_at_start,
                ParagraphEnd::EndOfBuffer,
                None,
            );
        };

        match tok {
            Token::Word { start, end, style } => {
                let bytes = &buf[start as usize..end as usize];
                let width = measure_word(bytes, style, &mut advance);
                out.push(Item::boxed(width, start).with_style(style));
                last_end = end;
            }

            Token::Space { start, style, end } => {
                let space = advance(' ', style) as u32;
                let stretch = space / 2;
                let shrink = (space / 3).min(255) as u8;
                out.push(
                    Item::glue(
                        space.min(u16::MAX as u32) as u16,
                        stretch.min(u16::MAX as u32) as u16,
                        shrink,
                        start,
                    )
                    .with_style(style),
                );
                last_end = end;
            }

            Token::Nbsp { start, end, style } => {
                // fixed glue: render at space width but never break here
                let space = advance(' ', style) as u32;
                out.push(
                    Item::boxed(space.min(u16::MAX as u32) as u16, start).with_style(style),
                );
                last_end = end;
            }

            Token::SoftHyphen { start, end, .. } => {
                // discretionary break: zero-width penalty (visible
                // hyphen rendering is deferred — see plan §5).
                out.push(Item::penalty(HYPHEN_PENALTY, true, false, start));
                last_end = end;
            }

            Token::HardBreak { start, end } => {
                // single \n inside a block: treat as paragraph end
                // for typesetting purposes (the renderer also flushes
                // the line). This matches greedy's behavior at
                // paging.rs:1009-1031 where \n closes the line.
                push_forced_break_triple(out, start);
                return finish(
                    out,
                    byte_start,
                    end,
                    block_at_start,
                    style_at_start,
                    ParagraphEnd::ParagraphBreak,
                    None,
                );
            }

            Token::ParagraphBreak { start, end } => {
                push_forced_break_triple(out, start);
                return finish(
                    out,
                    byte_start,
                    end,
                    block_at_start,
                    style_at_start,
                    ParagraphEnd::ParagraphBreak,
                    None,
                );
            }

            Token::PageBreak { start, end } => {
                if !out.is_empty() {
                    push_forced_break_triple(out, start);
                }
                return finish(
                    out,
                    byte_start,
                    end,
                    block_at_start,
                    style_at_start,
                    ParagraphEnd::PageBreakAfter,
                    None,
                );
            }

            Token::ThematicBreak { start, end } => {
                if !out.is_empty() {
                    push_forced_break_triple(out, start);
                }
                return finish(
                    out,
                    byte_start,
                    end,
                    block_at_start,
                    style_at_start,
                    ParagraphEnd::ThematicBreak,
                    None,
                );
            }

            Token::Image(img) => {
                if !out.is_empty() {
                    push_forced_break_triple(out, img.start);
                }
                return finish(
                    out,
                    byte_start,
                    img.end,
                    block_at_start,
                    style_at_start,
                    ParagraphEnd::ImageBlock,
                    Some(img),
                );
            }

            Token::BlockChanged { at, .. } => {
                if !out.is_empty() {
                    push_forced_break_triple(out, at);
                }
                return finish(
                    out,
                    byte_start,
                    at,
                    block_at_start,
                    style_at_start,
                    ParagraphEnd::BlockChanged,
                    None,
                );
            }

            Token::UnknownMarker { end, .. } => {
                last_end = end;
            }
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────

#[inline]
fn finish(
    out: &mut Vec<Item>,
    byte_start: u32,
    byte_end: u32,
    block: BlockState,
    style_at_start: TextStyle,
    end_kind: ParagraphEnd,
    image: Option<ImageRef>,
) -> ParagraphMeta {
    // a paragraph that produced no items at all (e.g. consecutive
    // page breaks, or an image at chapter start) is signaled with
    // an empty `out`; the caller skips break + paginate for it.
    let _ = out;
    ParagraphMeta {
        block,
        style_at_start,
        end_kind,
        byte_start,
        byte_end,
        image,
    }
}

/// Standard K-P paragraph-end triple: `Penalty(+∞), Glue(0,∞,0),
/// Penalty(−∞, forced)`. Stops a break at the prior glue, absorbs all
/// remaining width, then forces a break.
fn push_forced_break_triple(out: &mut Vec<Item>, byte_offset: u32) {
    out.push(Item::penalty(Item::PENALTY_FORBIDDEN, false, false, byte_offset));
    out.push(Item::glue(0, u16::MAX, 0, byte_offset));
    out.push(Item::penalty(Item::PENALTY_FORCE, false, true, byte_offset));
}

#[inline]
fn measure_word(
    bytes: &[u8],
    style: TextStyle,
    advance: &mut impl FnMut(char, TextStyle) -> u16,
) -> u16 {
    let mut width: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            width = width.saturating_add(advance(b as char, style) as u32);
            i += 1;
        } else {
            let (ch, len) = decode_utf8_char(bytes, i);
            width = width.saturating_add(advance(ch, style) as u32);
            i += len.max(1);
        }
    }
    width.min(u16::MAX as u32) as u16
}

// ── tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::reader::layout::scan::{MarkupScanner, TextStyle};
    use smol_epub::html_strip::{
        ALIGN_CENTER, BOLD_OFF, BOLD_ON, IMG_REF, ITALIC_OFF, ITALIC_ON, MARKER, PAGE_BREAK,
        QUOTE_ON,
    };

    /// minimal mock advance: every char is 1 px regardless of style.
    fn unit_advance(_c: char, _s: TextStyle) -> u16 {
        1
    }

    fn build(bytes: &[u8]) -> (Vec<Item>, ParagraphMeta) {
        let mut scanner = MarkupScanner::new(bytes);
        let mut out: Vec<Item> = Vec::new();
        let meta = build_paragraph(&mut scanner, unit_advance, &mut out);
        (out, meta)
    }

    #[test]
    fn empty_buffer_yields_empty_paragraph() {
        let (items, meta) = build(b"");
        assert!(items.is_empty());
        assert_eq!(meta.end_kind, ParagraphEnd::EndOfBuffer);
    }

    #[test]
    fn ascii_word_becomes_box_with_summed_advance() {
        let (items, meta) = build(b"hello");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), ItemKind::Box);
        assert_eq!(items[0].width, 5);
        assert_eq!(items[0].byte_offset, 0);
        assert_eq!(meta.end_kind, ParagraphEnd::EndOfBuffer);
    }

    #[test]
    fn space_becomes_glue_with_stretch_and_shrink() {
        let (items, _) = build(b"a b");
        // word 'a', glue ' ', word 'b'
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].kind(), ItemKind::Glue);
        assert_eq!(items[1].width, 1);
        assert_eq!(items[1].stretch, 0); // 1/2 truncates to 0
        assert_eq!(items[1].shrink, 0); // 1/3 truncates to 0
    }

    #[test]
    fn nbsp_becomes_unbreakable_box() {
        // "a NBSP b"
        let bytes = b"a\xC2\xA0b";
        let (items, _) = build(bytes);
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].kind(), ItemKind::Box);
        assert_eq!(items[1].width, 1);
    }

    #[test]
    fn soft_hyphen_becomes_flagged_penalty() {
        // "ab SHY cd"
        let bytes = b"ab\xC2\xADcd";
        let (items, _) = build(bytes);
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].kind(), ItemKind::Penalty);
        assert!(items[1].is_flagged());
        assert!(!items[1].is_forced());
        assert_eq!(items[1].penalty_value(), HYPHEN_PENALTY);
    }

    #[test]
    fn paragraph_break_emits_forced_triple_and_returns() {
        let (items, meta) = build(b"a\n\nb");
        // 'a' Box, then forced-triple, then we return; 'b' is for next call
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].kind(), ItemKind::Box);
        assert_eq!(items[1].kind(), ItemKind::Penalty);
        assert_eq!(items[1].penalty_value(), Item::PENALTY_FORBIDDEN);
        assert_eq!(items[2].kind(), ItemKind::Glue);
        assert_eq!(items[2].stretch, u16::MAX);
        assert_eq!(items[3].kind(), ItemKind::Penalty);
        assert_eq!(items[3].penalty_value(), Item::PENALTY_FORCE);
        assert!(items[3].is_forced());
        assert_eq!(meta.end_kind, ParagraphEnd::ParagraphBreak);
    }

    #[test]
    fn hard_break_acts_like_paragraph_break() {
        let (items, meta) = build(b"a\nb");
        assert_eq!(items.len(), 4);
        assert_eq!(meta.end_kind, ParagraphEnd::ParagraphBreak);
    }

    #[test]
    fn page_break_returns_page_break_after_kind() {
        let bytes = [b'a', MARKER, PAGE_BREAK];
        let (items, meta) = build(&bytes);
        // 'a' Box + forced triple
        assert_eq!(items.len(), 4);
        assert_eq!(meta.end_kind, ParagraphEnd::PageBreakAfter);
    }

    #[test]
    fn page_break_at_chapter_start_emits_no_items() {
        let bytes = [MARKER, PAGE_BREAK, b'a'];
        let (items, meta) = build(&bytes);
        assert!(items.is_empty());
        assert_eq!(meta.end_kind, ParagraphEnd::PageBreakAfter);
    }

    #[test]
    fn block_changed_ends_paragraph() {
        let bytes = [b'a', MARKER, ALIGN_CENTER, b'b'];
        let (items, meta) = build(&bytes);
        assert_eq!(items.len(), 4); // 'a' + forced triple
        assert_eq!(meta.end_kind, ParagraphEnd::BlockChanged);
    }

    #[test]
    fn quote_on_changes_block_state_and_ends_paragraph() {
        let bytes = [b'a', MARKER, QUOTE_ON, b'b'];
        let (items, meta) = build(&bytes);
        assert_eq!(meta.end_kind, ParagraphEnd::BlockChanged);
        // first paragraph started with indent 0
        assert_eq!(meta.block.indent, 0);
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn bold_marker_propagates_to_word_style() {
        let mut bytes = Vec::new();
        bytes.push(b'h');
        bytes.push(MARKER);
        bytes.push(BOLD_ON);
        bytes.push(b'i');
        let (items, _) = build(&bytes);
        // both words rendered as boxes regardless of style (advance is constant 1px)
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind(), ItemKind::Box);
        assert_eq!(items[1].kind(), ItemKind::Box);
        // first Box is pre-bold; second carries STYLE_BOLD
        assert!(!items[0].style_is_bold());
        assert!(items[1].style_is_bold());
    }

    #[test]
    fn single_letter_bold_dropcap_stamps_only_first_box() {
        // The Leviathan pattern: <b>M</b>iller — a single bold-letter
        // drop-cap followed by regular continuation. Items must reflect
        // the per-Box style so K-P measures bold-M with the bold font
        // and so first_box_style picks the right initial flag.
        let mut bytes = Vec::new();
        bytes.push(MARKER);
        bytes.push(BOLD_ON);
        bytes.push(b'M');
        bytes.push(MARKER);
        bytes.push(BOLD_OFF);
        bytes.extend_from_slice(b"iller");
        let (items, _) = build(&bytes);
        // expect Box("M", bold) + Box("iller", regular). No Glue, the
        // markers join the two words logically (no whitespace between).
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind(), ItemKind::Box);
        assert!(
            items[0].style_is_bold(),
            "first Box (drop-cap M) must carry STYLE_BOLD"
        );
        assert_eq!(items[1].kind(), ItemKind::Box);
        assert!(
            !items[1].style_is_bold(),
            "second Box (regular 'iller') must NOT carry STYLE_BOLD"
        );
    }

    #[test]
    fn nested_bold_italic_stamps_combined_style() {
        // BOLD_ON + ITALIC_ON + "x" + ITALIC_OFF + "y" + BOLD_OFF + "z"
        // → Box("x", bold+italic), Box("y", bold), Box("z", regular)
        let mut bytes = Vec::new();
        bytes.push(MARKER);
        bytes.push(BOLD_ON);
        bytes.push(MARKER);
        bytes.push(ITALIC_ON);
        bytes.push(b'x');
        bytes.push(MARKER);
        bytes.push(ITALIC_OFF);
        bytes.push(b'y');
        bytes.push(MARKER);
        bytes.push(BOLD_OFF);
        bytes.push(b'z');
        let (items, _) = build(&bytes);
        assert_eq!(items.len(), 3);
        let boxes: Vec<&Item> = items.iter().filter(|it| it.kind() == ItemKind::Box).collect();
        assert!(boxes[0].style_is_bold() && boxes[0].style_is_italic());
        assert!(boxes[1].style_is_bold() && !boxes[1].style_is_italic());
        assert!(!boxes[2].style_is_bold() && !boxes[2].style_is_italic());
    }

    #[test]
    fn glue_inherits_leading_word_style() {
        // BOLD_ON + "a" + " " (space) + "b" + BOLD_OFF + " " + "c"
        // The space after "a" is encountered while bold=true; the
        // intermediate space after "b" is also bold (close happens
        // AFTER the space). This documents the existing convention.
        let mut bytes = Vec::new();
        bytes.push(MARKER);
        bytes.push(BOLD_ON);
        bytes.extend_from_slice(b"a b");
        bytes.push(MARKER);
        bytes.push(BOLD_OFF);
        bytes.extend_from_slice(b" c");
        let (items, _) = build(&bytes);
        // Box("a", bold) + Glue(bold) + Box("b", bold) + Glue(regular) + Box("c", regular)
        assert!(items.len() >= 5);
        let kinds: Vec<ItemKind> = items.iter().map(|it| it.kind()).collect();
        assert_eq!(
            &kinds[..5],
            &[
                ItemKind::Box,
                ItemKind::Glue,
                ItemKind::Box,
                ItemKind::Glue,
                ItemKind::Box,
            ]
        );
        assert!(items[0].style_is_bold(), "Box 'a' must be bold");
        assert!(items[1].style_is_bold(), "Glue after 'a' inherits bold");
        assert!(items[2].style_is_bold(), "Box 'b' still bold (close not seen yet)");
        // Glue after "b" — by the time we encounter the space, we've
        // already seen the BOLD_OFF marker between "b" and " ", so it
        // carries regular style.
        assert!(!items[3].style_is_bold(), "Glue after BOLD_OFF must be regular");
        assert!(!items[4].style_is_bold(), "Box 'c' must be regular");
    }

    #[test]
    fn width_measurement_is_style_aware() {
        // Custom advance: regular = 1 px, bold = 3 px per character.
        // Confirms per-character style propagates into K-P widths.
        fn styled_advance(_c: char, s: TextStyle) -> u16 {
            if s.bold { 3 } else { 1 }
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ab ");
        bytes.push(MARKER);
        bytes.push(BOLD_ON);
        bytes.extend_from_slice(b"cd");
        let mut scanner = MarkupScanner::new(&bytes);
        let mut out: Vec<Item> = Vec::new();
        build_paragraph(&mut scanner, styled_advance, &mut out);
        // Box("ab") width 2 + Glue(space) width 1 + Box("cd", bold) width 6
        let box_widths: Vec<u16> = out
            .iter()
            .filter(|it| it.kind() == ItemKind::Box)
            .map(|it| it.width)
            .collect();
        assert_eq!(box_widths, vec![2, 6]);
    }

    #[test]
    fn image_at_chapter_start_returns_image_block_with_no_items() {
        // [MARKER, IMG_REF, flags, w_lo, w_hi, h_lo, h_hi, alt_len, path_len, alt..., path...]
        let alt = b"alt";
        let path = b"x.jpg";
        let mut bytes = Vec::new();
        bytes.push(MARKER);
        bytes.push(IMG_REF);
        bytes.push(0); // flags
        bytes.extend_from_slice(&100u16.to_le_bytes()); // attr_w
        bytes.extend_from_slice(&200u16.to_le_bytes()); // attr_h
        bytes.push(alt.len() as u8);
        bytes.push(path.len() as u8);
        bytes.extend_from_slice(alt);
        bytes.extend_from_slice(path);
        let (items, meta) = build(&bytes);
        assert!(items.is_empty());
        assert_eq!(meta.end_kind, ParagraphEnd::ImageBlock);
        assert!(meta.image.is_some());
    }

    #[test]
    fn image_after_text_flushes_paragraph_with_image_meta() {
        let alt = b"";
        let path = b"x.jpg";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"hi ");
        bytes.push(MARKER);
        bytes.push(IMG_REF);
        bytes.push(0);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.push(0);
        bytes.push(path.len() as u8);
        bytes.extend_from_slice(alt);
        bytes.extend_from_slice(path);
        let (items, meta) = build(&bytes);
        assert_eq!(meta.end_kind, ParagraphEnd::ImageBlock);
        assert!(meta.image.is_some());
        // 'hi' + space + forced triple
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn unknown_marker_is_skipped() {
        let bytes = [b'a', MARKER, b'?', b'b'];
        let (items, _) = build(&bytes);
        // 'a' word and 'b' word survive; unknown marker dropped
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn item_size_is_eight_bytes() {
        assert_eq!(core::mem::size_of::<Item>(), 8);
    }
}
