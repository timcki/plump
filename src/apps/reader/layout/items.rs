//! Knuth-Plass item builder.
//!
//! Walks the `smol_epub::markup::Events` stream and emits one
//! paragraph worth of K-P items into a caller-provided `Vec<Item>`.
//! The breaker takes that slice as input. Items are 12 B so a 3 KB
//! working budget covers ~256 items per paragraph; truly oversized
//! paragraphs return `BreakError::ItemBudgetExceeded` at the breaker
//! stage.
//!
//! A paragraph ends at any of: `ParagraphBreak`, `HardBreak`,
//! `PageBreak`, `ThematicBreak`, `Image`, a `Block` record arriving
//! mid-paragraph, or end-of-buffer. The caller drives the loop, so
//! block-state / page-break / image dispatch happens at paragraph
//! granularity.
//!
//! The paragraph's first-line indent (from its block record) goes in
//! as a fixed, unstretchable `Box` ahead of the first item, so the
//! breaker and the renderer agree on the first line's width without
//! either knowing about indents.

use alloc::vec::Vec;

use smol_epub::markup::{BlockProps, Event, Events, ImageRef, Style, decode_utf8};

// ── Item ───────────────────────────────────────────────────────────

/// Knuth-Plass item.
///
/// `flags` byte layout:
///   bits 0-1  ItemKind discriminant (Box=0, Glue=1, Penalty=2)
///   bit 2     `flagged` (K-P flagged-penalty bit; informs hyphen demerit)
///   bit 3     `forced`  (forces a break here when set, even at +∞ stretch)
///
/// `style` is the packed `markup::Style` the item was measured in.
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
    style: u8,
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
            style: 0,
            byte_offset,
        }
    }

    pub fn glue(width: u16, stretch: u16, shrink: u8, byte_offset: u32) -> Self {
        Self {
            width,
            stretch,
            shrink,
            flags: ItemKind::Glue as u8,
            style: 0,
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
            style: 0,
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

    /// Stamp the inline style the item was measured in, so the convert
    /// adapter can pick the line-start style off the first Box of each
    /// line.
    pub fn with_style(mut self, style: Style) -> Self {
        self.style = style.pack();
        self
    }

    #[inline]
    pub fn style(&self) -> Style {
        Style::unpack(self.style)
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
    /// a block record arrived mid-paragraph; the next paragraph picks
    /// up the new properties
    BlockChanged,
}

#[derive(Clone, Copy, Debug)]
pub struct ParagraphMeta {
    /// the paragraph's block properties (from its block record, or the
    /// defaults / continuation of the block a hard break split)
    pub block: BlockProps,
    pub style_at_start: Style,
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

/// Maximum word size measured by `build_paragraph`. Bytes beyond this
/// are silently dropped from the width calculation; the breaker still
/// emits the right Item via single_overfull_box logic for unbreakable
/// runs. Sized for English prose + URLs (~100 bytes typical).
const WORD_MEASURE_BUF: usize = 256;

// ── builder ───────────────────────────────────────────────────────

/// Drive the event stream until one paragraph is consumed; emit its
/// K-P items into `out`. Returns metadata about the paragraph boundary.
///
/// The caller is expected to clear `out` before each call.
///
/// `advance` is called per character of every Word token to derive
/// natural width: it returns the pen movement for `ch` when it follows
/// `prev` (the glyph advance plus the pair kerning, or the bare advance
/// at a word start). The closure form keeps this module free of
/// font/HAL imports for host testability. `em_px` sizes the first-line
/// indent box.
pub fn build_paragraph(
    events: &mut Events<'_>,
    mut advance: impl FnMut(Option<char>, char, Style) -> i16,
    em_px: u16,
    out: &mut Vec<Item>,
) -> ParagraphMeta {
    let style_at_start = events.style();
    let byte_start = events.offset();
    let mut block = events.block();
    let mut last_end: u32 = byte_start;
    // stack scratch for word bytes. typical English words / URLs
    // comfortably fit in 256 bytes; longer runs are truncated and the
    // K-P breaker still handles them correctly via single_overfull_box.
    let mut word_buf = [0u8; WORD_MEASURE_BUF];
    // the first-line indent box goes in ahead of the first text item
    let mut indent_pending = true;

    macro_rules! lead_in {
        ($at:expr, $style:expr) => {
            if indent_pending {
                indent_pending = false;
                let w = super::indent_px(block.text_indent_qem, em_px);
                if w > 0 {
                    out.push(Item::boxed(w, $at).with_style($style));
                }
            }
        };
    }

    loop {
        let Some(ev) = events.next_event() else {
            return finish(byte_start, last_end, block, style_at_start, ParagraphEnd::EndOfBuffer, None);
        };

        match ev {
            Event::Word { start, end, style } => {
                lead_in!(start, style);
                let len = (end - start) as usize;
                let n = events.read_into(start, &mut word_buf[..len.min(WORD_MEASURE_BUF)]);
                let width = measure_word(&word_buf[..n], style, &mut advance);
                out.push(Item::boxed(width, start).with_style(style));
                last_end = end;
            }

            Event::Space { start, end, style } => {
                lead_in!(start, style);
                let space = advance(None, ' ', style).max(0) as u32;
                // TeX cmr10's classical ratios. The breaker's pass-2
                // fallback (`break_paragraph_with_fallback`) adds
                // emergencystretch per line so narrow-column paragraphs
                // that overflow this budget on pass 1 still find a
                // solution on pass 2 without widening per-glue here.
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

            Event::Nbsp { start, end, style } => {
                lead_in!(start, style);
                // fixed glue: render at space width but never break here
                let space = advance(None, ' ', style).max(0) as u32;
                out.push(Item::boxed(space.min(u16::MAX as u32) as u16, start).with_style(style));
                last_end = end;
            }

            Event::SoftHyphen { start, end, .. } => {
                // discretionary break: zero-width penalty (visible
                // hyphen rendering is deferred — see plan §5).
                out.push(Item::penalty(HYPHEN_PENALTY, true, false, start));
                last_end = end;
            }

            Event::HardBreak { start, end } => {
                // single \n inside a block: treat as paragraph end for
                // typesetting purposes; the decoder carries the block's
                // alignment and left indent over to the continuation
                push_forced_break_triple(out, start);
                return finish(byte_start, end, block, style_at_start, ParagraphEnd::ParagraphBreak, None);
            }

            Event::ParagraphBreak { start, end } => {
                push_forced_break_triple(out, start);
                return finish(byte_start, end, block, style_at_start, ParagraphEnd::ParagraphBreak, None);
            }

            Event::PageBreak { start, end } => {
                if !out.is_empty() {
                    push_forced_break_triple(out, start);
                }
                return finish(byte_start, end, block, style_at_start, ParagraphEnd::PageBreakAfter, None);
            }

            Event::ThematicBreak { start, end } => {
                if !out.is_empty() {
                    push_forced_break_triple(out, start);
                }
                return finish(byte_start, end, block, style_at_start, ParagraphEnd::ThematicBreak, None);
            }

            Event::Image(img) => {
                if !out.is_empty() {
                    push_forced_break_triple(out, img.start);
                }
                return finish(byte_start, img.end, block, style_at_start, ParagraphEnd::ImageBlock, Some(img));
            }

            Event::Block { at, block: b } => {
                if out.is_empty() {
                    // the record for the paragraph about to start
                    block = b;
                    last_end = at;
                } else {
                    push_forced_break_triple(out, at);
                    return finish(byte_start, at, block, style_at_start, ParagraphEnd::BlockChanged, None);
                }
            }

            Event::Unknown { end, .. } => {
                last_end = end;
            }
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────

#[inline]
fn finish(
    byte_start: u32,
    byte_end: u32,
    block: BlockProps,
    style_at_start: Style,
    end_kind: ParagraphEnd,
    image: Option<ImageRef>,
) -> ParagraphMeta {
    // a paragraph that produced no items at all (e.g. consecutive
    // page breaks, or an image at chapter start) is signaled with
    // an empty `out`; the caller skips break + paginate for it.
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
    style: Style,
    advance: &mut impl FnMut(Option<char>, char, Style) -> i16,
) -> u16 {
    // kerning applies between consecutive glyphs of the word
    let mut prev: Option<char> = None;
    let mut width: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (ch, len) = if bytes[i] < 0x80 {
            (bytes[i] as char, 1)
        } else {
            decode_utf8(&bytes[i..])
        };
        width = width.saturating_add(advance(prev, ch, style) as i32);
        prev = Some(ch);
        i += len.max(1);
    }
    width.clamp(0, u16::MAX as i32) as u16
}

// ── tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use smol_epub::markup::{
        Align, BOLD_OFF, BOLD_ON, IMG_REF, ITALIC_OFF, ITALIC_ON, MARKER, PAGE_BREAK, SliceSource,
    };

    /// minimal mock advance: every char is 1 px regardless of style.
    fn unit_advance(_p: Option<char>, _c: char, _s: Style) -> i16 {
        1
    }

    fn build(bytes: &[u8]) -> (Vec<Item>, ParagraphMeta) {
        build_em(bytes, 16)
    }

    fn build_em(bytes: &[u8], em_px: u16) -> (Vec<Item>, ParagraphMeta) {
        let mut src = SliceSource(bytes);
        let mut events = Events::new(&mut src);
        let mut out: Vec<Item> = Vec::new();
        let meta = build_paragraph(&mut events, unit_advance, em_px, &mut out);
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
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].kind(), ItemKind::Glue);
        assert_eq!(items[1].width, 1);
    }

    #[test]
    fn nbsp_becomes_unbreakable_box() {
        let (items, _) = build(b"a\xC2\xA0b");
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].kind(), ItemKind::Box);
    }

    #[test]
    fn soft_hyphen_becomes_flagged_penalty() {
        let (items, _) = build(b"ab\xC2\xADcd");
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].kind(), ItemKind::Penalty);
        assert!(items[1].is_flagged());
        assert_eq!(items[1].penalty_value(), HYPHEN_PENALTY);
    }

    #[test]
    fn paragraph_break_emits_forced_triple_and_returns() {
        let (items, meta) = build(b"a\n\nb");
        assert_eq!(items.len(), 4);
        assert!(items[3].is_forced());
        assert_eq!(meta.end_kind, ParagraphEnd::ParagraphBreak);
        assert_eq!(meta.byte_end, 3);
    }

    #[test]
    fn page_break_at_chapter_start_emits_no_items() {
        let (items, meta) = build(&[MARKER, PAGE_BREAK, b'a']);
        assert!(items.is_empty());
        assert_eq!(meta.end_kind, ParagraphEnd::PageBreakAfter);
    }

    #[test]
    fn block_record_sets_meta_and_adds_indent_box() {
        let props = BlockProps {
            align: Align::Right,
            left: 1,
            text_indent_qem: 6,
            space_above_qem: 4,
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&props.encode());
        bytes.extend_from_slice(b"ab cd");
        let (items, meta) = build_em(&bytes, 20);
        assert_eq!(meta.block, props);
        // indent box (6 qem at 20 px/em = 30 px), word, glue, word
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].kind(), ItemKind::Box);
        assert_eq!(items[0].width, 30);
        assert_eq!(items[0].byte_offset, 5);
        assert_eq!(items[1].width, 2);
    }

    #[test]
    fn no_indent_box_without_text_indent() {
        let (items, _) = build(b"ab");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn bold_marker_propagates_to_word_style() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[MARKER, BOLD_ON]);
        bytes.extend_from_slice(b"x");
        bytes.extend_from_slice(&[MARKER, BOLD_OFF]);
        bytes.extend_from_slice(b" y");
        let (items, _) = build(&bytes);
        assert!(items[0].style().bold);
        assert!(!items[2].style().bold);
    }

    #[test]
    fn nested_bold_italic_stamps_combined_style() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[MARKER, BOLD_ON, MARKER, ITALIC_ON]);
        bytes.extend_from_slice(b"x");
        bytes.extend_from_slice(&[MARKER, ITALIC_OFF, MARKER, BOLD_OFF]);
        let (items, _) = build(&bytes);
        let s = items[0].style();
        assert!(s.bold && s.italic);
    }

    #[test]
    fn image_after_text_flushes_paragraph_with_image_meta() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"a\n\n");
        bytes.extend_from_slice(&[MARKER, IMG_REF, 0, 0, 0, 0, 0, 0, 3]);
        bytes.extend_from_slice(b"x.j");
        let mut src = SliceSource(&bytes);
        let mut events = Events::new(&mut src);
        let mut out = Vec::new();
        let first = build_paragraph(&mut events, unit_advance, 16, &mut out);
        assert_eq!(first.end_kind, ParagraphEnd::ParagraphBreak);
        out.clear();
        let second = build_paragraph(&mut events, unit_advance, 16, &mut out);
        assert!(out.is_empty());
        assert_eq!(second.end_kind, ParagraphEnd::ImageBlock);
        let img = second.image.unwrap();
        assert_eq!(img.start, 3);
        assert_eq!(img.path_len, 3);
    }

    #[test]
    fn unknown_marker_is_skipped() {
        let (items, _) = build(&[b'a', MARKER, b'?', b'b']);
        assert_eq!(items.len(), 2);
    }
}
