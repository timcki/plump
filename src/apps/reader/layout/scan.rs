//! Markup-stream scanner for the smol-epub chapter byte stream.
//!
//! Walks the raw byte stream and produces a flat sequence of
//! `Token`s tagged with byte spans and the live text style.
//! Style and block markers are consumed internally and reflected
//! either via per-token `style` fields or via explicit
//! `BlockChanged` events; the byte ranges in non-marker tokens
//! never include marker bytes, so a line's `start_byte..end_byte`
//! span still naturally covers any markers that fall between
//! tokens.
//!
//! Coalescing rule: contiguous non-whitespace, non-marker bytes
//! from one `TextStyle` collapse into a single `Token::Word`.
//! NBSP and soft hyphen split a word so the breaker can model
//! them as fixed glue and discretionary penalty respectively.
//!
//! The scanner has no `plump-kernel` or `esp-hal` dependencies
//! beyond the `smol_epub::html_strip` constants, so it can move
//! into a sibling host-testable crate later without code changes.

use smol_epub::html_strip::{
    ALIGN_CENTER, ALIGN_JUSTIFY, ALIGN_LEFT, ALIGN_RESET, ALIGN_RIGHT, BOLD_OFF, BOLD_ON, BREAK,
    FIGCAPTION_OFF, FIGCAPTION_ON, H1_OFF, H1_ON, H2_OFF, H2_ON, H3_OFF, H3_ON, H4_OFF, H4_ON,
    H5_OFF, H5_ON, H6_OFF, H6_ON, HEADING_OFF, HEADING_ON, IMG_HEADER_LEN, IMG_REF, ITALIC_OFF,
    ITALIC_ON, MARKER, PAGE_BREAK, QUOTE_OFF, QUOTE_ON, STRIKE_OFF, STRIKE_ON, UNDERLINE_OFF,
    UNDERLINE_ON,
};

// ── public types ───────────────────────────────────────────────────

/// font-relevant style state captured per text-bearing token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
    pub heading: bool,
    /// 1..=6 when `heading` is true, else 0
    pub hlevel: u8,
    pub underline: bool,
    pub strike: bool,
}

impl TextStyle {
    /// `true` when the per-paragraph default should treat this run
    /// as body bold (h4-h6 in our renderer).
    pub fn is_h4_h6_bold(&self) -> bool {
        self.heading && self.hlevel >= 4
    }
}

/// block-level alignment, set by ALIGN_* markers. `Default` means
/// honor the reader's `text_alignment` setting at draw time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlockAlign {
    #[default]
    Default,
    Left,
    Center,
    Right,
    Justify,
}

/// block-level state that is independent of the inline text style.
/// changes here typically imply a paragraph or block boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockState {
    pub align: BlockAlign,
    pub indent: u8,
    pub figcaption: bool,
}

/// fully-parsed `IMG_REF` payload. byte offsets are absolute into
/// the chapter buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRef {
    /// offset of the leading MARKER byte
    pub start: u32,
    pub alt_start: u32,
    pub path_start: u32,
    /// one-past the last path byte; equals `start + IMG_HEADER_LEN
    /// + alt_len + path_len`
    pub end: u32,
    pub flags: u8,
    pub attr_w: u16,
    pub attr_h: u16,
    pub alt_len: u8,
    pub path_len: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// run of printable bytes from one `TextStyle` (no spaces, no
    /// markers, no NBSP, no soft hyphen).
    Word { start: u32, end: u32, style: TextStyle },
    /// ASCII 0x20: breakable inter-word space (one byte).
    Space { start: u32, end: u32, style: TextStyle },
    /// U+00A0: fixed non-breaking space (two bytes).
    Nbsp { start: u32, end: u32, style: TextStyle },
    /// U+00AD: zero-width discretionary break opportunity.
    SoftHyphen { start: u32, end: u32, style: TextStyle },
    /// single `\n`: hard line break inside the current block.
    HardBreak { start: u32, end: u32 },
    /// run of `\n` of length >= 2: end of paragraph.
    ParagraphBreak { start: u32, end: u32 },
    /// `PAGE_BREAK` marker: forced page boundary.
    PageBreak { start: u32, end: u32 },
    /// `BREAK` marker: thematic break / `<hr>`.
    ThematicBreak { start: u32, end: u32 },
    /// `IMG_REF` marker with full extended payload.
    Image(ImageRef),
    /// block state changed at byte `at` (one-past the marker that
    /// caused the change). emitted only when `BlockState` actually
    /// changed value, not on every align marker.
    BlockChanged { at: u32, block: BlockState },
    /// marker tag the scanner doesn't recognize; two bytes consumed.
    UnknownMarker { start: u32, end: u32, tag: u8 },
}

// ── scanner ────────────────────────────────────────────────────────

pub struct MarkupScanner<'a> {
    buf: &'a [u8],
    pos: usize,
    style: TextStyle,
    block: BlockState,
}

impl<'a> MarkupScanner<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            style: TextStyle::default(),
            block: BlockState::default(),
        }
    }

    #[inline]
    pub fn position(&self) -> u32 {
        self.pos as u32
    }

    #[inline]
    pub fn buffer_len(&self) -> u32 {
        self.buf.len() as u32
    }

    #[inline]
    pub fn text_style(&self) -> TextStyle {
        self.style
    }

    #[inline]
    pub fn block_state(&self) -> BlockState {
        self.block
    }

    /// pull the next token. returns `None` once the buffer is
    /// exhausted; subsequent calls keep returning `None`.
    pub fn next(&mut self) -> Option<Token> {
        loop {
            if self.pos >= self.buf.len() {
                return None;
            }
            let b = self.buf[self.pos];

            // marker pair
            if b == MARKER {
                if self.pos + 1 >= self.buf.len() {
                    // dangling MARKER at end: drop one byte and stop
                    self.pos += 1;
                    continue;
                }
                if let Some(tok) = self.consume_marker() {
                    return Some(tok);
                }
                // marker mutated style only, loop and continue
                continue;
            }

            // newlines
            if b == b'\n' {
                let start = self.pos;
                self.pos += 1;
                // collapse a run of >= 2 newlines into ParagraphBreak
                if self.pos < self.buf.len() && self.buf[self.pos] == b'\n' {
                    while self.pos < self.buf.len() && self.buf[self.pos] == b'\n' {
                        self.pos += 1;
                    }
                    return Some(Token::ParagraphBreak {
                        start: start as u32,
                        end: self.pos as u32,
                    });
                }
                return Some(Token::HardBreak {
                    start: start as u32,
                    end: self.pos as u32,
                });
            }

            // \r and other low control bytes: silently skipped to
            // mirror the greedy wrapper. \t included here too.
            if b < 0x20 {
                self.pos += 1;
                continue;
            }

            // ASCII space: one byte at a time so the breaker can
            // model each gap as glue. consecutive spaces produce
            // consecutive Space tokens (rare in well-formed output).
            if b == b' ' {
                let start = self.pos;
                self.pos += 1;
                return Some(Token::Space {
                    start: start as u32,
                    end: self.pos as u32,
                    style: self.style,
                });
            }

            // multi-byte UTF-8: special-case NBSP and soft hyphen
            if b >= 0xC0 {
                let (ch, len) = decode_utf8(self.buf, self.pos);
                let start = self.pos;
                self.pos += len;
                match ch {
                    '\u{00A0}' => {
                        return Some(Token::Nbsp {
                            start: start as u32,
                            end: self.pos as u32,
                            style: self.style,
                        });
                    }
                    '\u{00AD}' => {
                        return Some(Token::SoftHyphen {
                            start: start as u32,
                            end: self.pos as u32,
                            style: self.style,
                        });
                    }
                    _ => {
                        // begin a word at `start`, continue scanning
                        return Some(self.consume_word(start));
                    }
                }
            }

            // stray UTF-8 continuation byte: skip
            if b >= 0x80 {
                self.pos += 1;
                continue;
            }

            // printable ASCII: start a word
            return Some(self.consume_word(self.pos));
        }
    }

    /// at `self.buf[self.pos] == MARKER` with at least one more byte
    /// available. consumes the marker pair (or extended payload for
    /// `IMG_REF`) and returns the matching `Token`, or `None` when
    /// the marker only mutated `self.style` and the caller should
    /// loop.
    fn consume_marker(&mut self) -> Option<Token> {
        let marker_start = self.pos;
        let tag = self.buf[self.pos + 1];

        match tag {
            IMG_REF => return self.consume_img_ref(marker_start),
            PAGE_BREAK => {
                self.pos += 2;
                return Some(Token::PageBreak {
                    start: marker_start as u32,
                    end: self.pos as u32,
                });
            }
            BREAK => {
                self.pos += 2;
                return Some(Token::ThematicBreak {
                    start: marker_start as u32,
                    end: self.pos as u32,
                });
            }

            // inline style toggles: mutate self.style, no token
            BOLD_ON => self.style.bold = true,
            BOLD_OFF => self.style.bold = false,
            ITALIC_ON => self.style.italic = true,
            ITALIC_OFF => self.style.italic = false,
            UNDERLINE_ON => self.style.underline = true,
            UNDERLINE_OFF => self.style.underline = false,
            STRIKE_ON => self.style.strike = true,
            STRIKE_OFF => self.style.strike = false,
            HEADING_ON => {
                self.style.heading = true;
                // legacy heading marker without level: treat as h2-tier
                self.style.hlevel = 2;
            }
            HEADING_OFF => {
                self.style.heading = false;
                self.style.hlevel = 0;
            }
            H1_ON => {
                self.style.heading = true;
                self.style.hlevel = 1;
            }
            H2_ON => {
                self.style.heading = true;
                self.style.hlevel = 2;
            }
            H3_ON => {
                self.style.heading = true;
                self.style.hlevel = 3;
            }
            H4_ON => {
                self.style.heading = true;
                self.style.hlevel = 4;
            }
            H5_ON => {
                self.style.heading = true;
                self.style.hlevel = 5;
            }
            H6_ON => {
                self.style.heading = true;
                self.style.hlevel = 6;
            }
            H1_OFF | H2_OFF | H3_OFF | H4_OFF | H5_OFF | H6_OFF => {
                self.style.heading = false;
                self.style.hlevel = 0;
            }

            // block-level changes: mutate self.block and emit
            // BlockChanged when the value actually changed
            QUOTE_ON => {
                self.pos += 2;
                let prev = self.block;
                self.block.indent = self.block.indent.saturating_add(1);
                if self.block != prev {
                    return Some(Token::BlockChanged {
                        at: self.pos as u32,
                        block: self.block,
                    });
                }
                return None;
            }
            QUOTE_OFF => {
                self.pos += 2;
                let prev = self.block;
                self.block.indent = self.block.indent.saturating_sub(1);
                if self.block != prev {
                    return Some(Token::BlockChanged {
                        at: self.pos as u32,
                        block: self.block,
                    });
                }
                return None;
            }
            ALIGN_LEFT => return self.set_align(BlockAlign::Left),
            ALIGN_CENTER => return self.set_align(BlockAlign::Center),
            ALIGN_RIGHT => return self.set_align(BlockAlign::Right),
            ALIGN_JUSTIFY => return self.set_align(BlockAlign::Justify),
            ALIGN_RESET => return self.set_align(BlockAlign::Default),
            FIGCAPTION_ON => {
                self.pos += 2;
                let prev = self.block;
                self.block.figcaption = true;
                if self.block != prev {
                    return Some(Token::BlockChanged {
                        at: self.pos as u32,
                        block: self.block,
                    });
                }
                return None;
            }
            FIGCAPTION_OFF => {
                self.pos += 2;
                let prev = self.block;
                self.block.figcaption = false;
                if self.block != prev {
                    return Some(Token::BlockChanged {
                        at: self.pos as u32,
                        block: self.block,
                    });
                }
                return None;
            }

            // unknown tag: consume two bytes and surface to caller
            _ => {
                self.pos += 2;
                return Some(Token::UnknownMarker {
                    start: marker_start as u32,
                    end: self.pos as u32,
                    tag,
                });
            }
        }

        // common tail for inline-style toggles
        self.pos += 2;
        None
    }

    fn set_align(&mut self, new_align: BlockAlign) -> Option<Token> {
        self.pos += 2;
        let prev = self.block;
        self.block.align = new_align;
        if self.block != prev {
            return Some(Token::BlockChanged {
                at: self.pos as u32,
                block: self.block,
            });
        }
        None
    }

    /// consume an `IMG_REF` payload. on a malformed / truncated
    /// payload the marker pair is still consumed (two bytes) and
    /// `UnknownMarker { tag: IMG_REF }` is returned so the caller
    /// can log without panicking.
    fn consume_img_ref(&mut self, marker_start: usize) -> Option<Token> {
        if self.pos + IMG_HEADER_LEN > self.buf.len() {
            self.pos += 2;
            return Some(Token::UnknownMarker {
                start: marker_start as u32,
                end: self.pos as u32,
                tag: IMG_REF,
            });
        }
        let header = &self.buf[self.pos..self.pos + IMG_HEADER_LEN];
        let flags = header[2];
        let attr_w = u16::from_le_bytes([header[3], header[4]]);
        let attr_h = u16::from_le_bytes([header[5], header[6]]);
        let alt_len = header[7];
        let path_len = header[8];
        let alt_start = self.pos + IMG_HEADER_LEN;
        let path_start = alt_start + alt_len as usize;
        let end = path_start + path_len as usize;
        if end > self.buf.len() || path_len == 0 {
            self.pos += 2;
            return Some(Token::UnknownMarker {
                start: marker_start as u32,
                end: self.pos as u32,
                tag: IMG_REF,
            });
        }
        self.pos = end;
        Some(Token::Image(ImageRef {
            start: marker_start as u32,
            alt_start: alt_start as u32,
            path_start: path_start as u32,
            end: end as u32,
            flags,
            attr_w,
            attr_h,
            alt_len,
            path_len,
        }))
    }

    /// at `self.buf[start]` is a wordy byte (printable ASCII or
    /// multi-byte non-NBSP/SHY). `self.pos` points anywhere in
    /// `[start, end_of_word)`; we extend it to the end of the word.
    fn consume_word(&mut self, start: usize) -> Token {
        let style = self.style;
        let mut p = if self.pos > start { self.pos } else { start };
        // include the byte at `start` if we haven't moved yet
        if p == start && start < self.buf.len() {
            // sniff one byte/code-point worth of advance into the word
            let b = self.buf[p];
            if b >= 0xC0 {
                let (_, len) = decode_utf8(self.buf, p);
                p += len;
            } else {
                p += 1;
            }
        }
        while p < self.buf.len() {
            let c = self.buf[p];
            if c == MARKER || c == b' ' || c == b'\n' || c == b'\r' || c < 0x20 {
                break;
            }
            if c >= 0xC0 {
                let (ch, len) = decode_utf8(self.buf, p);
                if ch == '\u{00A0}' || ch == '\u{00AD}' {
                    break;
                }
                p += len;
                continue;
            }
            if c >= 0x80 {
                // stray continuation byte: drop and keep scanning
                p += 1;
                continue;
            }
            p += 1;
        }
        self.pos = p;
        Token::Word {
            start: start as u32,
            end: p as u32,
            style,
        }
    }
}

// ── inline UTF-8 decoder ───────────────────────────────────────────
//
// duplicated from `plump_kernel::util::decode_utf8_char` so this
// module has zero dependency on plump-kernel/esp-hal and can move
// into a host-testable sibling crate without code changes.

#[inline]
fn decode_utf8(buf: &[u8], pos: usize) -> (char, usize) {
    let b0 = buf[pos];
    if b0 < 0x80 {
        return (b0 as char, 1);
    }
    let (mut cp, expected) = if b0 < 0xC0 {
        return ('\u{FFFD}', 1);
    } else if b0 < 0xE0 {
        ((b0 as u32) & 0x1F, 2)
    } else if b0 < 0xF0 {
        ((b0 as u32) & 0x0F, 3)
    } else if b0 < 0xF8 {
        ((b0 as u32) & 0x07, 4)
    } else {
        return ('\u{FFFD}', 1);
    };
    let len = buf.len();
    if pos + expected > len {
        return ('\u{FFFD}', len - pos);
    }
    for i in 1..expected {
        let cont = buf[pos + i];
        if cont & 0xC0 != 0x80 {
            return ('\u{FFFD}', i);
        }
        cp = (cp << 6) | (cont as u32 & 0x3F);
    }
    let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
    (ch, expected)
}

// ── tests ──────────────────────────────────────────────────────────
//
// these are inert in the device build (cfg(test) is only set by
// `cargo test`). they document expected behavior and become live
// when the layout core is extracted into a host-testable sibling
// crate; the scanner is intentionally decoupled from plump-kernel
// so that move is mechanical.

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(bytes: &[u8]) -> Vec<Token> {
        let mut s = MarkupScanner::new(bytes);
        let mut out = Vec::new();
        while let Some(t) = s.next() {
            out.push(t);
        }
        out
    }

    fn marker(tag: u8) -> [u8; 2] {
        [MARKER, tag]
    }

    #[test]
    fn plain_ascii_words_and_spaces() {
        let toks = drain(b"hello world");
        assert_eq!(
            toks,
            vec![
                Token::Word {
                    start: 0,
                    end: 5,
                    style: TextStyle::default()
                },
                Token::Space {
                    start: 5,
                    end: 6,
                    style: TextStyle::default()
                },
                Token::Word {
                    start: 6,
                    end: 11,
                    style: TextStyle::default()
                },
            ]
        );
    }

    #[test]
    fn hard_break_vs_paragraph_break() {
        let single = drain(b"a\nb");
        assert!(matches!(single[1], Token::HardBreak { .. }));

        let paragraph = drain(b"a\n\nb");
        assert!(matches!(paragraph[1], Token::ParagraphBreak { start: 1, end: 3 }));

        let triple = drain(b"a\n\n\nb");
        // three or more collapse into one ParagraphBreak
        match triple[1] {
            Token::ParagraphBreak { start, end } => {
                assert_eq!(start, 1);
                assert_eq!(end, 4);
            }
            _ => panic!("expected ParagraphBreak"),
        }
    }

    #[test]
    fn nbsp_splits_word() {
        // "a" + NBSP + "b" → Word, Nbsp, Word
        let bytes = b"a\xC2\xA0b";
        let toks = drain(bytes);
        assert!(matches!(toks[0], Token::Word { start: 0, end: 1, .. }));
        assert!(matches!(toks[1], Token::Nbsp { start: 1, end: 3, .. }));
        assert!(matches!(toks[2], Token::Word { start: 3, end: 4, .. }));
    }

    #[test]
    fn soft_hyphen_splits_word() {
        // "ab" + SHY + "cd"
        let bytes = b"ab\xC2\xADcd";
        let toks = drain(bytes);
        assert!(matches!(toks[0], Token::Word { start: 0, end: 2, .. }));
        assert!(matches!(toks[1], Token::SoftHyphen { start: 2, end: 4, .. }));
        assert!(matches!(toks[2], Token::Word { start: 4, end: 6, .. }));
    }

    #[test]
    fn bold_marker_splits_word_and_propagates_style() {
        // "h" + BOLD_ON + "i"
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"h");
        bytes.extend_from_slice(&marker(BOLD_ON));
        bytes.extend_from_slice(b"i");
        let toks = drain(&bytes);
        assert_eq!(toks.len(), 2);
        match toks[0] {
            Token::Word { style, .. } => assert!(!style.bold),
            _ => panic!(),
        }
        match toks[1] {
            Token::Word { style, .. } => assert!(style.bold),
            _ => panic!(),
        }
    }

    #[test]
    fn heading_levels() {
        let pairs = [
            (H1_ON, 1u8),
            (H2_ON, 2),
            (H3_ON, 3),
            (H4_ON, 4),
            (H5_ON, 5),
            (H6_ON, 6),
        ];
        for (on, level) in pairs {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&marker(on));
            bytes.extend_from_slice(b"x");
            let toks = drain(&bytes);
            match toks[0] {
                Token::Word { style, .. } => {
                    assert!(style.heading);
                    assert_eq!(style.hlevel, level);
                }
                _ => panic!(),
            }
        }
    }

    #[test]
    fn legacy_heading_is_h2_tier() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&marker(HEADING_ON));
        bytes.extend_from_slice(b"x");
        let toks = drain(&bytes);
        match toks[0] {
            Token::Word { style, .. } => {
                assert!(style.heading);
                assert_eq!(style.hlevel, 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn align_emits_block_changed_only_on_transition() {
        // ALIGN_CENTER then ALIGN_CENTER again: only first emits
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&marker(ALIGN_CENTER));
        bytes.extend_from_slice(b"a");
        bytes.extend_from_slice(&marker(ALIGN_CENTER));
        bytes.extend_from_slice(b"b");
        let toks = drain(&bytes);
        let bc_count = toks
            .iter()
            .filter(|t| matches!(t, Token::BlockChanged { .. }))
            .count();
        assert_eq!(bc_count, 1, "second identical align should not emit");
    }

    #[test]
    fn quote_indent_stack() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&marker(QUOTE_ON));
        bytes.extend_from_slice(&marker(QUOTE_ON));
        bytes.extend_from_slice(b"x");
        bytes.extend_from_slice(&marker(QUOTE_OFF));
        bytes.extend_from_slice(b"y");
        let toks = drain(&bytes);
        let depths: Vec<u8> = toks
            .iter()
            .filter_map(|t| match t {
                Token::BlockChanged { block, .. } => Some(block.indent),
                _ => None,
            })
            .collect();
        assert_eq!(depths, vec![1, 2, 1]);
    }

    #[test]
    fn page_and_thematic_break() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&marker(PAGE_BREAK));
        bytes.extend_from_slice(&marker(BREAK));
        let toks = drain(&bytes);
        assert!(matches!(toks[0], Token::PageBreak { .. }));
        assert!(matches!(toks[1], Token::ThematicBreak { .. }));
    }

    #[test]
    fn img_ref_payload_advances_correctly() {
        // [MARKER, IMG_REF, flags, w_lo, w_hi, h_lo, h_hi, alt_len, path_len, alt..., path...]
        let alt = b"alt-text";
        let path = b"images/cover.jpg";
        let mut bytes = Vec::new();
        bytes.push(MARKER);
        bytes.push(IMG_REF);
        bytes.push(0x42); // flags
        bytes.extend_from_slice(&100u16.to_le_bytes()); // attr_w
        bytes.extend_from_slice(&200u16.to_le_bytes()); // attr_h
        bytes.push(alt.len() as u8);
        bytes.push(path.len() as u8);
        bytes.extend_from_slice(alt);
        bytes.extend_from_slice(path);
        // trailing byte to verify advance lands correctly
        bytes.push(b'X');

        let toks = drain(&bytes);
        match toks[0] {
            Token::Image(img) => {
                assert_eq!(img.start, 0);
                assert_eq!(img.flags, 0x42);
                assert_eq!(img.attr_w, 100);
                assert_eq!(img.attr_h, 200);
                assert_eq!(img.alt_len as usize, alt.len());
                assert_eq!(img.path_len as usize, path.len());
                assert_eq!(img.alt_start as usize, IMG_HEADER_LEN);
                assert_eq!(img.path_start as usize, IMG_HEADER_LEN + alt.len());
                assert_eq!(img.end as usize, IMG_HEADER_LEN + alt.len() + path.len());
            }
            _ => panic!("expected Image token, got {:?}", toks[0]),
        }
        // next token should be Word("X")
        match toks[1] {
            Token::Word { start, end, .. } => {
                assert_eq!(start as usize, IMG_HEADER_LEN + alt.len() + path.len());
                assert_eq!(end as usize, IMG_HEADER_LEN + alt.len() + path.len() + 1);
            }
            _ => panic!("expected trailing Word, got {:?}", toks[1]),
        }
    }

    #[test]
    fn truncated_img_ref_yields_unknown_marker_and_does_not_panic() {
        // header truncated: only MARKER, IMG_REF, flags
        let bytes = vec![MARKER, IMG_REF, 0x00];
        let toks = drain(&bytes);
        assert_eq!(toks.len(), 1);
        match toks[0] {
            Token::UnknownMarker { tag, .. } => assert_eq!(tag, IMG_REF),
            _ => panic!(),
        }
    }

    #[test]
    fn img_ref_with_zero_path_len_is_unknown() {
        let mut bytes = vec![MARKER, IMG_REF, 0, 0, 0, 0, 0, 0, 0];
        bytes.push(b'X'); // trailing
        let toks = drain(&bytes);
        // path_len is 0 → invalid IMG_REF, but advance is still 2 bytes
        match toks[0] {
            Token::UnknownMarker { tag, end, .. } => {
                assert_eq!(tag, IMG_REF);
                assert_eq!(end, 2);
            }
            _ => panic!("expected UnknownMarker, got {:?}", toks[0]),
        }
    }

    #[test]
    fn unknown_marker_tag_consumes_two_bytes() {
        let bytes = [MARKER, b'?', b'a'];
        let toks = drain(&bytes);
        assert!(matches!(toks[0], Token::UnknownMarker { tag: b'?', start: 0, end: 2 }));
        assert!(matches!(toks[1], Token::Word { start: 2, end: 3, .. }));
    }

    #[test]
    fn dangling_marker_at_eof_does_not_panic() {
        let bytes = [b'a', MARKER];
        let toks = drain(&bytes);
        assert_eq!(toks.len(), 1);
        assert!(matches!(toks[0], Token::Word { start: 0, end: 1, .. }));
    }

    #[test]
    fn cr_is_silently_skipped() {
        let toks = drain(b"a\r\nb");
        // \r dropped, then \n emitted as HardBreak
        assert!(matches!(toks[0], Token::Word { start: 0, end: 1, .. }));
        assert!(matches!(toks[1], Token::HardBreak { start: 2, end: 3 }));
        assert!(matches!(toks[2], Token::Word { start: 3, end: 4, .. }));
    }

    #[test]
    fn empty_buffer_yields_nothing() {
        let toks = drain(b"");
        assert!(toks.is_empty());
    }

    #[test]
    fn next_returns_none_after_exhaustion() {
        let mut s = MarkupScanner::new(b"a");
        assert!(s.next().is_some());
        assert!(s.next().is_none());
        assert!(s.next().is_none());
    }

    #[test]
    fn align_left_then_reset_round_trips_block_state() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&marker(ALIGN_LEFT));
        bytes.extend_from_slice(b"x");
        bytes.extend_from_slice(&marker(ALIGN_RESET));
        bytes.extend_from_slice(b"y");
        let toks = drain(&bytes);
        let aligns: Vec<BlockAlign> = toks
            .iter()
            .filter_map(|t| match t {
                Token::BlockChanged { block, .. } => Some(block.align),
                _ => None,
            })
            .collect();
        assert_eq!(aligns, vec![BlockAlign::Left, BlockAlign::Default]);
    }

    #[test]
    fn figcaption_toggles_block_state() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&marker(FIGCAPTION_ON));
        bytes.extend_from_slice(b"x");
        bytes.extend_from_slice(&marker(FIGCAPTION_OFF));
        bytes.extend_from_slice(b"y");
        let toks = drain(&bytes);
        let figs: Vec<bool> = toks
            .iter()
            .filter_map(|t| match t {
                Token::BlockChanged { block, .. } => Some(block.figcaption),
                _ => None,
            })
            .collect();
        assert_eq!(figs, vec![true, false]);
    }
}
