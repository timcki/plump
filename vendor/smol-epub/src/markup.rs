//! The styled-text encoding shared by the stripper and every consumer.
//!
//! A stripped chapter is one flat UTF-8 byte stream. Three things are
//! mixed into the text:
//!
//! * `\n` is a hard line break (`<br>`); a run of two or more is a
//!   paragraph boundary (any block element).
//! * two-byte markers `[MARKER, tag]` toggle inline style (bold,
//!   italic, underline, strike, heading level) or mark a page or
//!   thematic break.
//! * two records with a payload: [`BLOCK`] carries a paragraph's
//!   block properties (alignment, left indent, first-line indent and
//!   the space above it, see [`BlockProps`]) and [`IMG_REF`] carries
//!   an inline image reference (see [`ImageRef`]).
//!
//! Inline style is stateful: a toggle stays in force until its
//! counterpart. Block properties are not: a [`BLOCK`] record applies
//! to the paragraph it prefixes and a paragraph without one has
//! [`BlockProps::DEFAULT`]. Lengths are quarter-em so the stream is
//! independent of the font the consumer renders with.
//!
//! [`Events`] is the one decoder. Typesetters and renderers alike
//! walk the stream through it so the encoding has a single owner:
//! extending the format means a new tag here, a new arm in
//! [`Events::next`], and the stripper emitting it.

// ── marker tags ─────────────────────────────────────────────────────

/// Escape byte that introduces a marker in the output stream.
pub const MARKER: u8 = 0x01;

/// Bold on.
pub const BOLD_ON: u8 = b'B';
/// Bold off.
pub const BOLD_OFF: u8 = b'b';
/// Italic on.
pub const ITALIC_ON: u8 = b'I';
/// Italic off.
pub const ITALIC_OFF: u8 = b'i';
/// Underline on (`<u>`, `<ins>`, `text-decoration: underline`).
pub const UNDERLINE_ON: u8 = b'U';
/// Underline off.
pub const UNDERLINE_OFF: u8 = b'u';
/// Strike-through on (`<s>`, `<del>`, `text-decoration: line-through`).
pub const STRIKE_ON: u8 = b'K';
/// Strike-through off.
pub const STRIKE_OFF: u8 = b'k';

/// `<h1>` open.
pub const H1_ON: u8 = b'1';
/// `<h1>` close.
pub const H1_OFF: u8 = b'!';
/// `<h2>` open.
pub const H2_ON: u8 = b'2';
/// `<h2>` close.
pub const H2_OFF: u8 = b'@';
/// `<h3>` open.
pub const H3_ON: u8 = b'3';
/// `<h3>` close.
pub const H3_OFF: u8 = b'#';
/// `<h4>` open.
pub const H4_ON: u8 = b'4';
/// `<h4>` close.
pub const H4_OFF: u8 = b'$';
/// `<h5>` open.
pub const H5_ON: u8 = b'5';
/// `<h5>` close.
pub const H5_OFF: u8 = b'%';
/// `<h6>` open.
pub const H6_ON: u8 = b'6';
/// `<h6>` close.
pub const H6_OFF: u8 = b'^';

/// Thematic break / horizontal rule (`<hr>`).
pub const BREAK: u8 = b'S';
/// End the current page; resume on a fresh page. Emitted before
/// `<h1>` and for blocks carrying `page-break-before: always`.
pub const PAGE_BREAK: u8 = b'F';

/// Block record: `[MARKER, BLOCK, align_left, text_indent, space_above]`.
/// See [`BlockProps::encode`].
pub const BLOCK: u8 = b'{';
/// Total length of a block record including the marker pair.
pub const BLOCK_LEN: usize = 5;

/// Inline image reference:
/// `[MARKER, IMG_REF, flags, w_lo, w_hi, h_lo, h_hi, alt_len, path_len, alt…, path…]`.
pub const IMG_REF: u8 = b'P';
/// Length of the IMG_REF header (the bytes preceding alt + path).
pub const IMG_HEADER_LEN: usize = 9;
/// IMG_REF flag: the image is the canonical content of a `<figure>`.
pub const IMG_FLAG_FIGURE: u8 = 1 << 0;
/// IMG_REF flag: reserved for an explicit "force block" hint.
pub const IMG_FLAG_BLOCK: u8 = 1 << 1;

/// Returns `true` if `b` is the [`MARKER`] escape byte.
#[inline]
pub const fn is_marker(b: u8) -> bool {
    b == MARKER
}

// ── inline style ────────────────────────────────────────────────────

/// Inline text style in force at a point in the stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Underlined (a draw-time decoration; no metric impact).
    pub underline: bool,
    /// Struck through (a draw-time decoration; no metric impact).
    pub strike: bool,
    /// Heading level `1..=6`, or `0` outside headings.
    pub heading: u8,
}

impl Style {
    /// No style at all.
    pub const PLAIN: Self = Self {
        bold: false,
        italic: false,
        underline: false,
        strike: false,
        heading: 0,
    };

    /// `true` inside any heading.
    #[inline]
    pub const fn is_heading(&self) -> bool {
        self.heading != 0
    }

    /// Pack into one byte: bit 0 bold, bit 1 italic, bit 2 underline,
    /// bit 3 strike, bits 4-6 heading level.
    #[inline]
    pub const fn pack(&self) -> u8 {
        (self.bold as u8)
            | ((self.italic as u8) << 1)
            | ((self.underline as u8) << 2)
            | ((self.strike as u8) << 3)
            | ((self.heading & 0x7) << 4)
    }

    /// Inverse of [`pack`](Self::pack).
    #[inline]
    pub const fn unpack(b: u8) -> Self {
        Self {
            bold: b & 1 != 0,
            italic: b & 2 != 0,
            underline: b & 4 != 0,
            strike: b & 8 != 0,
            heading: (b >> 4) & 0x7,
        }
    }
}

// ── block properties ────────────────────────────────────────────────

/// Horizontal alignment of a paragraph's lines.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// The reader's own default (typically the user's justify setting).
    #[default]
    Default = 0,
    /// Ragged right.
    Left = 1,
    /// Centered.
    Center = 2,
    /// Ragged left.
    Right = 3,
}

impl Align {
    /// Decode the two-bit alignment field.
    #[inline]
    pub const fn from_bits(b: u8) -> Self {
        match b & 0x3 {
            1 => Align::Left,
            2 => Align::Center,
            3 => Align::Right,
            _ => Align::Default,
        }
    }
}

/// Block-level properties of one paragraph, carried by a [`BLOCK`]
/// record that prefixes the paragraph's first text byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockProps {
    /// Line alignment.
    pub align: Align,
    /// Left indent in consumer-defined levels (block quotes, CSS
    /// `margin-left` in whole em), `0..=15`.
    pub left: u8,
    /// First-line indent in quarter-em, `0..=63`.
    pub text_indent_qem: u8,
    /// Vertical space above the paragraph in quarter-em, `0..=63`,
    /// already collapsed the CSS way with the previous block's
    /// bottom margin.
    pub space_above_qem: u8,
}

impl BlockProps {
    /// A paragraph without a block record.
    pub const DEFAULT: Self = Self {
        align: Align::Default,
        left: 0,
        text_indent_qem: 0,
        space_above_qem: 0,
    };

    /// Largest value carried by the indent and space fields.
    pub const QEM_MAX: u8 = 63;
    /// Largest left indent level.
    pub const LEFT_MAX: u8 = 15;

    /// `true` when nothing differs from [`DEFAULT`](Self::DEFAULT), in
    /// which case the stripper emits no record.
    #[inline]
    pub fn is_default(&self) -> bool {
        *self == Self::DEFAULT
    }

    /// Encode as a full record including the marker pair.
    pub fn encode(&self) -> [u8; BLOCK_LEN] {
        [
            MARKER,
            BLOCK,
            ((self.align as u8) << 4) | (self.left & Self::LEFT_MAX),
            self.text_indent_qem.min(Self::QEM_MAX),
            self.space_above_qem.min(Self::QEM_MAX),
        ]
    }

    /// Decode the three payload bytes that follow `[MARKER, BLOCK]`.
    pub const fn decode(payload: [u8; 3]) -> Self {
        Self {
            align: Align::from_bits(payload[0] >> 4),
            left: payload[0] & Self::LEFT_MAX,
            text_indent_qem: payload[1],
            space_above_qem: payload[2],
        }
    }

    /// The properties a paragraph inherits when a hard line break
    /// (`<br>`) splits its block: alignment and left indent carry on,
    /// the first-line indent and the space above do not.
    #[inline]
    pub const fn continued(&self) -> Self {
        Self {
            align: self.align,
            left: self.left,
            text_indent_qem: 0,
            space_above_qem: 0,
        }
    }
}

// ── image reference ─────────────────────────────────────────────────

/// A decoded [`IMG_REF`] record. Byte offsets are absolute into the
/// stream the record was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRef {
    /// Offset of the leading [`MARKER`] byte.
    pub start: u32,
    /// Offset of the first alt-text byte.
    pub alt_start: u32,
    /// Offset of the first path byte.
    pub path_start: u32,
    /// One past the last path byte.
    pub end: u32,
    /// `IMG_FLAG_*` bits.
    pub flags: u8,
    /// `<img width>` in pixels, `0` if absent.
    pub attr_w: u16,
    /// `<img height>` in pixels, `0` if absent.
    pub attr_h: u16,
    /// Alt-text length in bytes.
    pub alt_len: u8,
    /// Path length in bytes; never `0` in a valid record.
    pub path_len: u8,
}

impl ImageRef {
    /// Parse a record whose header sits at `bytes[at..]`; the header
    /// must include the marker pair. Returns `None` when the record is
    /// truncated or has an empty path.
    pub fn parse(bytes: &[u8], at: usize) -> Option<Self> {
        if at + IMG_HEADER_LEN > bytes.len() {
            return None;
        }
        let h = &bytes[at..at + IMG_HEADER_LEN];
        Self::from_header(h, at, bytes.len())
    }

    fn from_header(h: &[u8], at: usize, total: usize) -> Option<Self> {
        if h.len() < IMG_HEADER_LEN || h[0] != MARKER || h[1] != IMG_REF {
            return None;
        }
        let alt_len = h[7];
        let path_len = h[8];
        let alt_start = at + IMG_HEADER_LEN;
        let path_start = alt_start + alt_len as usize;
        let end = path_start + path_len as usize;
        if path_len == 0 || end > total {
            return None;
        }
        Some(Self {
            start: at as u32,
            alt_start: alt_start as u32,
            path_start: path_start as u32,
            end: end as u32,
            flags: h[2],
            attr_w: u16::from_le_bytes([h[3], h[4]]),
            attr_h: u16::from_le_bytes([h[5], h[6]]),
            alt_len,
            path_len,
        })
    }

    /// Find the next complete record at or after `from`, walking the
    /// stream record by record so a [`BLOCK`] payload byte that happens
    /// to equal [`MARKER`] is never mistaken for a record start (a
    /// paragraph with a quarter-em gap whose text begins with `P` looks
    /// exactly like an IMG_REF header to a raw byte scan). `Err` carries
    /// the offset the walk stopped at: the end of `bytes`, or the start
    /// of a record that runs past it, which a caller reading the stream
    /// in windows resumes from.
    pub fn find_next(bytes: &[u8], from: usize) -> Result<Self, usize> {
        let mut i = from;
        while i < bytes.len() {
            if bytes[i] != MARKER {
                i += 1;
                continue;
            }
            if i + 1 >= bytes.len() {
                return Err(i);
            }
            match bytes[i + 1] {
                BLOCK => {
                    if i + BLOCK_LEN > bytes.len() {
                        return Err(i);
                    }
                    i += BLOCK_LEN;
                }
                IMG_REF => return Self::parse(bytes, i).ok_or(i),
                _ => i += 2,
            }
        }
        Err(bytes.len())
    }

    /// The alt-text slice of the stream `bytes` this record was parsed from.
    #[inline]
    pub fn alt<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        &bytes[self.alt_start as usize..self.path_start as usize]
    }

    /// The path slice of the stream `bytes` this record was parsed from.
    #[inline]
    pub fn path<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        &bytes[self.path_start as usize..self.end as usize]
    }
}

// ── byte source ─────────────────────────────────────────────────────

/// Random-access, mostly sequential byte source for [`Events`].
///
/// A streaming implementation over a file is fastest when accesses
/// cluster: the iterator only ever looks a few bytes ahead of its
/// position, and [`Events::read_into`] is called for one word at a
/// time.
pub trait ByteSource {
    /// Total bytes addressable.
    fn len(&self) -> usize;
    /// `true` when nothing is addressable.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The byte at `pos`, or `None` past the end.
    fn byte_at(&mut self, pos: usize) -> Option<u8>;
    /// Copy bytes starting at `pos` into `dst`; returns how many were
    /// copied (short at the end of the source).
    fn read_into(&mut self, pos: usize, dst: &mut [u8]) -> usize;
}

/// [`ByteSource`] over an in-memory slice.
pub struct SliceSource<'a>(pub &'a [u8]);

impl ByteSource for SliceSource<'_> {
    #[inline]
    fn len(&self) -> usize {
        self.0.len()
    }
    #[inline]
    fn byte_at(&mut self, pos: usize) -> Option<u8> {
        self.0.get(pos).copied()
    }
    #[inline]
    fn read_into(&mut self, pos: usize, dst: &mut [u8]) -> usize {
        if pos >= self.0.len() {
            return 0;
        }
        let n = dst.len().min(self.0.len() - pos);
        dst[..n].copy_from_slice(&self.0[pos..pos + n]);
        n
    }
}

// ── events ──────────────────────────────────────────────────────────

/// One decoded item of the stream. Byte ranges never include marker
/// bytes, so a line's `start..end` still covers the markers that fall
/// between its words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Run of printable bytes under one [`Style`]: no spaces, no
    /// markers, no NBSP, no soft hyphen.
    Word {
        /// First byte.
        start: u32,
        /// One past the last byte.
        end: u32,
        /// Style in force.
        style: Style,
    },
    /// ASCII 0x20: a breakable, stretchable inter-word gap (one byte).
    Space {
        /// First byte.
        start: u32,
        /// One past the last byte.
        end: u32,
        /// Style in force.
        style: Style,
    },
    /// U+00A0: a fixed, unbreakable space (two bytes).
    Nbsp {
        /// First byte.
        start: u32,
        /// One past the last byte.
        end: u32,
        /// Style in force.
        style: Style,
    },
    /// U+00AD: a zero-width discretionary break (two bytes).
    SoftHyphen {
        /// First byte.
        start: u32,
        /// One past the last byte.
        end: u32,
        /// Style in force.
        style: Style,
    },
    /// A single `\n`: hard line break inside the current block.
    HardBreak {
        /// First byte.
        start: u32,
        /// One past the last byte.
        end: u32,
    },
    /// Two or more `\n`: end of paragraph.
    ParagraphBreak {
        /// First byte.
        start: u32,
        /// One past the last byte.
        end: u32,
    },
    /// [`PAGE_BREAK`]: forced page boundary.
    PageBreak {
        /// Marker offset.
        start: u32,
        /// One past the marker.
        end: u32,
    },
    /// [`BREAK`]: thematic break.
    ThematicBreak {
        /// Marker offset.
        start: u32,
        /// One past the marker.
        end: u32,
    },
    /// An inline image.
    Image(ImageRef),
    /// A [`BLOCK`] record; the properties apply to the paragraph that
    /// follows and are also readable through [`Events::block`].
    Block {
        /// One past the record.
        at: u32,
        /// The decoded properties.
        block: BlockProps,
    },
    /// A marker tag this decoder doesn't know; two bytes consumed.
    Unknown {
        /// Marker offset.
        start: u32,
        /// One past the marker.
        end: u32,
        /// The tag byte.
        tag: u8,
    },
}

/// Decoder over a stripped chapter stream.
///
/// Inline style toggles are folded into [`style`](Self::style) and
/// produce no event. Block records are folded into
/// [`block`](Self::block) *and* surfaced as [`Event::Block`]. The
/// block state resets to [`BlockProps::DEFAULT`] at every paragraph,
/// page, thematic or image boundary, and to
/// [`BlockProps::continued`] at a hard line break.
pub struct Events<'s> {
    src: &'s mut dyn ByteSource,
    pos: usize,
    style: Style,
    block: BlockProps,
}

impl<'s> Events<'s> {
    /// Start decoding `src` from its first byte with no style in force.
    pub fn new(src: &'s mut dyn ByteSource) -> Self {
        Self {
            src,
            pos: 0,
            style: Style::PLAIN,
            block: BlockProps::DEFAULT,
        }
    }

    /// Start decoding at `pos` with `style` already in force and
    /// `block` as the current paragraph's properties. Renderers use
    /// this to decode one line whose style at its first byte the
    /// typesetter recorded.
    pub fn resume(
        src: &'s mut dyn ByteSource,
        pos: usize,
        style: Style,
        block: BlockProps,
    ) -> Self {
        Self {
            src,
            pos,
            style,
            block,
        }
    }

    /// Current byte offset (the start of the next event).
    #[inline]
    pub fn offset(&self) -> u32 {
        self.pos as u32
    }

    /// Total length of the underlying source.
    #[inline]
    pub fn source_len(&self) -> u32 {
        self.src.len() as u32
    }

    /// `true` once every byte has been consumed.
    #[inline]
    pub fn is_eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    /// Inline style in force at [`position`](Self::position).
    #[inline]
    pub fn style(&self) -> Style {
        self.style
    }

    /// Block properties of the paragraph being decoded.
    #[inline]
    pub fn block(&self) -> BlockProps {
        self.block
    }

    /// Copy stream bytes at `start` into `dst`; returns bytes copied.
    /// Consumers use this to read back a [`Event::Word`]'s bytes.
    #[inline]
    pub fn read_into(&mut self, start: u32, dst: &mut [u8]) -> usize {
        self.src.read_into(start as usize, dst)
    }

    /// Decode the next event, or `None` at the end of the stream.
    pub fn next_event(&mut self) -> Option<Event> {
        loop {
            let b = self.src.byte_at(self.pos)?;

            if b == MARKER {
                if self.pos + 1 >= self.src.len() {
                    // dangling MARKER at the end: drop it
                    self.pos += 1;
                    continue;
                }
                if let Some(ev) = self.consume_marker() {
                    return Some(ev);
                }
                continue;
            }

            if b == b'\n' {
                let start = self.pos;
                self.pos += 1;
                if self.src.byte_at(self.pos) == Some(b'\n') {
                    while self.src.byte_at(self.pos) == Some(b'\n') {
                        self.pos += 1;
                    }
                    self.block = BlockProps::DEFAULT;
                    return Some(Event::ParagraphBreak {
                        start: start as u32,
                        end: self.pos as u32,
                    });
                }
                self.block = self.block.continued();
                return Some(Event::HardBreak {
                    start: start as u32,
                    end: self.pos as u32,
                });
            }

            // \r, \t and other control bytes are silently skipped
            if b < 0x20 {
                self.pos += 1;
                continue;
            }

            if b == b' ' {
                let start = self.pos;
                self.pos += 1;
                return Some(Event::Space {
                    start: start as u32,
                    end: self.pos as u32,
                    style: self.style,
                });
            }

            if b >= 0xC0 {
                let (ch, len) = self.decode_utf8_at(self.pos);
                let start = self.pos;
                match ch {
                    '\u{00A0}' => {
                        self.pos += len;
                        return Some(Event::Nbsp {
                            start: start as u32,
                            end: self.pos as u32,
                            style: self.style,
                        });
                    }
                    '\u{00AD}' => {
                        self.pos += len;
                        return Some(Event::SoftHyphen {
                            start: start as u32,
                            end: self.pos as u32,
                            style: self.style,
                        });
                    }
                    _ => return Some(self.consume_word(start, len)),
                }
            }

            // stray UTF-8 continuation byte
            if b >= 0x80 {
                self.pos += 1;
                continue;
            }

            return Some(self.consume_word(self.pos, 1));
        }
    }

    #[inline]
    fn decode_utf8_at(&mut self, pos: usize) -> (char, usize) {
        let mut scratch = [0u8; 4];
        let n = self.src.read_into(pos, &mut scratch);
        if n == 0 {
            return ('\u{FFFD}', 1);
        }
        decode_utf8(&scratch[..n])
    }

    // at `self.pos` is MARKER with at least one byte after it. returns
    // an event, or None when the marker only changed the inline style
    fn consume_marker(&mut self) -> Option<Event> {
        let start = self.pos;
        let tag = self.src.byte_at(start + 1)?;
        let s = &mut self.style;
        match tag {
            BOLD_ON => s.bold = true,
            BOLD_OFF => s.bold = false,
            ITALIC_ON => s.italic = true,
            ITALIC_OFF => s.italic = false,
            UNDERLINE_ON => s.underline = true,
            UNDERLINE_OFF => s.underline = false,
            STRIKE_ON => s.strike = true,
            STRIKE_OFF => s.strike = false,
            H1_ON => s.heading = 1,
            H2_ON => s.heading = 2,
            H3_ON => s.heading = 3,
            H4_ON => s.heading = 4,
            H5_ON => s.heading = 5,
            H6_ON => s.heading = 6,
            H1_OFF | H2_OFF | H3_OFF | H4_OFF | H5_OFF | H6_OFF => s.heading = 0,
            PAGE_BREAK => {
                self.pos += 2;
                self.block = BlockProps::DEFAULT;
                return Some(Event::PageBreak {
                    start: start as u32,
                    end: self.pos as u32,
                });
            }
            BREAK => {
                self.pos += 2;
                self.block = BlockProps::DEFAULT;
                return Some(Event::ThematicBreak {
                    start: start as u32,
                    end: self.pos as u32,
                });
            }
            BLOCK => {
                let mut payload = [0u8; 3];
                if start + BLOCK_LEN > self.src.len()
                    || self.src.read_into(start + 2, &mut payload) < 3
                {
                    self.pos += 2;
                    return Some(Event::Unknown {
                        start: start as u32,
                        end: self.pos as u32,
                        tag,
                    });
                }
                self.pos += BLOCK_LEN;
                self.block = BlockProps::decode(payload);
                return Some(Event::Block {
                    at: self.pos as u32,
                    block: self.block,
                });
            }
            IMG_REF => {
                let mut header = [0u8; IMG_HEADER_LEN];
                let n = self.src.read_into(start, &mut header);
                match ImageRef::from_header(&header[..n], start, self.src.len()) {
                    Some(img) => {
                        self.pos = img.end as usize;
                        self.block = BlockProps::DEFAULT;
                        return Some(Event::Image(img));
                    }
                    None => {
                        self.pos += 2;
                        return Some(Event::Unknown {
                            start: start as u32,
                            end: self.pos as u32,
                            tag,
                        });
                    }
                }
            }
            _ => {
                self.pos += 2;
                return Some(Event::Unknown {
                    start: start as u32,
                    end: self.pos as u32,
                    tag,
                });
            }
        }
        self.pos += 2;
        None
    }

    // the word starts at `start` with a first code point of `first_len`
    // bytes; extend to the end of the run
    fn consume_word(&mut self, start: usize, first_len: usize) -> Event {
        let style = self.style;
        let total = self.src.len();
        let mut p = start + first_len;
        while p < total {
            let Some(c) = self.src.byte_at(p) else { break };
            if c == MARKER || c <= b' ' {
                break;
            }
            if c >= 0xC0 {
                let (ch, len) = self.decode_utf8_at(p);
                if ch == '\u{00A0}' || ch == '\u{00AD}' {
                    break;
                }
                p += len;
                continue;
            }
            // stray continuation byte: keep scanning
            p += 1;
        }
        self.pos = p;
        Event::Word {
            start: start as u32,
            end: p as u32,
            style,
        }
    }
}

impl Iterator for Events<'_> {
    type Item = Event;

    #[inline]
    fn next(&mut self) -> Option<Event> {
        self.next_event()
    }
}

/// Decode one UTF-8 code point at the start of `buf`. Malformed input
/// yields U+FFFD and the number of bytes to skip.
pub fn decode_utf8(buf: &[u8]) -> (char, usize) {
    let b0 = buf[0];
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
    if expected > buf.len() {
        return ('\u{FFFD}', buf.len());
    }
    for (i, &cont) in buf.iter().enumerate().take(expected).skip(1) {
        if cont & 0xC0 != 0x80 {
            return ('\u{FFFD}', i);
        }
        cp = (cp << 6) | (cont as u32 & 0x3F);
    }
    (char::from_u32(cp).unwrap_or('\u{FFFD}'), expected)
}

// ── tests ───────────────────────────────────────────────────────────

// ── anchors ─────────────────────────────────────────────────────────

/// A position in a chapter that survives re-stripping: the ordinal of
/// the paragraph and of the word within it. Byte offsets move when the
/// stripper changes what it emits between words (a block record, a
/// style toggle); the count of words does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anchor {
    /// Paragraph ordinal, counting [`Event::ParagraphBreak`]s before it.
    pub para: u32,
    /// Ordinal of the word (or image) within the paragraph.
    pub word: u32,
}

/// The anchor of the first word or image that ends after `offset`.
/// Past the last word this is one beyond the last word of the last
/// paragraph, which [`offset_of`] maps back to the chapter's end.
pub fn anchor_at(src: &mut dyn ByteSource, offset: usize) -> Anchor {
    let mut a = Anchor::default();
    let mut ev = Events::new(src);
    while let Some(e) = ev.next_event() {
        match e {
            Event::Word { end, .. } if end as usize > offset => return a,
            Event::Image(img) if img.end as usize > offset => return a,
            Event::Word { .. } | Event::Image(_) => a.word += 1,
            Event::ParagraphBreak { .. } => {
                a.para += 1;
                a.word = 0;
            }
            _ => {}
        }
    }
    a
}

/// The byte offset `anchor` names: the start of that word, or the end
/// of the paragraph when it has fewer words than `anchor.word`, or the
/// chapter's end when it has fewer paragraphs.
pub fn offset_of(src: &mut dyn ByteSource, anchor: Anchor) -> u32 {
    let mut para = 0u32;
    let mut word = 0u32;
    let mut ev = Events::new(src);
    while let Some(e) = ev.next_event() {
        match e {
            Event::Word { start, .. } | Event::Image(ImageRef { start, .. }) => {
                if para == anchor.para && word == anchor.word {
                    return start;
                }
                word += 1;
            }
            Event::ParagraphBreak { start, .. } => {
                if para == anchor.para {
                    return start;
                }
                para += 1;
                word = 0;
            }
            _ => {}
        }
    }
    ev.offset()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_round_trip_and_survive_inserted_records() {
        let mut a = b"one two\n\nthree four five\n\nsix".to_vec();
        let off_four = a.windows(4).position(|w| w == b"four").unwrap();
        let anchor = anchor_at(&mut SliceSource(&a), off_four);
        assert_eq!(anchor, Anchor { para: 1, word: 1 });
        assert_eq!(offset_of(&mut SliceSource(&a), anchor) as usize, off_four);
        // an offset inside a word anchors to that word
        assert_eq!(anchor_at(&mut SliceSource(&a), off_four + 2), anchor);

        // the same text with a block record ahead of every paragraph
        // and an italic toggle: every byte offset moved, no anchor did
        let mut b = alloc::vec![MARKER, BLOCK, 0, 6, 0];
        b.extend_from_slice(b"one two\n\n");
        b.extend_from_slice(&[MARKER, BLOCK, 0, 6, 0, MARKER, ITALIC_ON]);
        b.extend_from_slice(b"three four five");
        b.extend_from_slice(&[MARKER, ITALIC_OFF]);
        b.extend_from_slice(b"\n\nsix");
        let off_four_b = b.windows(4).position(|w| w == b"four").unwrap();
        assert_ne!(off_four, off_four_b);
        assert_eq!(offset_of(&mut SliceSource(&b), anchor) as usize, off_four_b);

        // past the paragraph's words: its end; past the chapter: its end
        a.push(b' ');
        let end_p1 = a.windows(2).rposition(|w| w == b"\n\n").unwrap();
        assert_eq!(
            offset_of(&mut SliceSource(&a), Anchor { para: 1, word: 9 }) as usize,
            end_p1
        );
        assert_eq!(
            offset_of(&mut SliceSource(&a), Anchor { para: 7, word: 0 }) as usize,
            a.len()
        );
        assert_eq!(anchor_at(&mut SliceSource(&a), a.len()), Anchor { para: 2, word: 1 });
    }

    fn img_record(alt: &[u8], path: &[u8]) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec![MARKER, IMG_REF, 0, 0, 0, 0, 0, alt.len() as u8, path.len() as u8];
        v.extend_from_slice(alt);
        v.extend_from_slice(path);
        v
    }

    #[test]
    fn find_next_skips_block_payload_that_looks_like_a_header() {
        // gap of one quarter-em then a paragraph starting with "P": the
        // payload byte 0x01 followed by 'P' is not an IMG_REF
        let mut s = alloc::vec![MARKER, BLOCK, 0, 6, 1];
        s.extend_from_slice(b"Pretty soon the whole crew knew. ");
        assert_eq!(ImageRef::find_next(&s, 0), Err(s.len()));

        let at = s.len();
        s.extend_from_slice(&img_record(b"Chapter 7", b"../images/a.jpg"));
        let img = ImageRef::find_next(&s, 0).unwrap();
        assert_eq!(img.start as usize, at);
        assert_eq!(img.path(&s), b"../images/a.jpg");
        assert_eq!(ImageRef::find_next(&s, img.end as usize), Err(s.len()));
    }

    #[test]
    fn find_next_stops_at_a_record_cut_by_the_window() {
        let mut s = alloc::vec![MARKER, ITALIC_ON];
        s.extend_from_slice(b"ab ");
        let at = s.len();
        s.extend_from_slice(&img_record(b"", b"x.png"));
        let cut = &s[..s.len() - 2];
        assert_eq!(ImageRef::find_next(cut, 0), Err(at));
        let mut b = alloc::vec![MARKER, BLOCK, 2];
        assert_eq!(ImageRef::find_next(&b, 0), Err(0));
        b.push(0);
        b.push(0);
        assert_eq!(ImageRef::find_next(&b, 0), Err(5));
    }
    use alloc::vec::Vec;

    fn drain(bytes: &[u8]) -> Vec<Event> {
        let mut src = SliceSource(bytes);
        Events::new(&mut src).collect()
    }

    fn m(tag: u8) -> [u8; 2] {
        [MARKER, tag]
    }

    #[test]
    fn words_and_spaces() {
        let ev = drain(b"ab cd");
        assert_eq!(ev.len(), 3);
        assert!(matches!(ev[0], Event::Word { start: 0, end: 2, .. }));
        assert!(matches!(ev[1], Event::Space { start: 2, end: 3, .. }));
        assert!(matches!(ev[2], Event::Word { start: 3, end: 5, .. }));
    }

    #[test]
    fn hard_and_paragraph_breaks() {
        let ev = drain(b"a\nb\n\n\nc");
        assert!(matches!(ev[1], Event::HardBreak { start: 1, end: 2 }));
        assert!(matches!(ev[3], Event::ParagraphBreak { start: 3, end: 6 }));
        assert!(matches!(ev[4], Event::Word { start: 6, end: 7, .. }));
    }

    #[test]
    fn nbsp_and_soft_hyphen_split_words() {
        let ev = drain(b"a\xC2\xA0b\xC2\xADc");
        assert!(matches!(ev[0], Event::Word { start: 0, end: 1, .. }));
        assert!(matches!(ev[1], Event::Nbsp { start: 1, end: 3, .. }));
        assert!(matches!(ev[2], Event::Word { start: 3, end: 4, .. }));
        assert!(matches!(ev[3], Event::SoftHyphen { start: 4, end: 6, .. }));
        assert!(matches!(ev[4], Event::Word { start: 6, end: 7, .. }));
    }

    #[test]
    fn style_toggles_carry_onto_words() {
        let mut b = Vec::new();
        b.extend_from_slice(b"a");
        b.extend_from_slice(&m(BOLD_ON));
        b.extend_from_slice(&m(ITALIC_ON));
        b.extend_from_slice(b"b");
        b.extend_from_slice(&m(ITALIC_OFF));
        b.extend_from_slice(b"c");
        b.extend_from_slice(&m(BOLD_OFF));
        b.extend_from_slice(b"d");
        let ev = drain(&b);
        let styles: Vec<Style> = ev
            .iter()
            .filter_map(|e| match e {
                Event::Word { style, .. } => Some(*style),
                _ => None,
            })
            .collect();
        assert_eq!(styles.len(), 4);
        assert_eq!(styles[0], Style::PLAIN);
        assert!(styles[1].bold && styles[1].italic);
        assert!(styles[2].bold && !styles[2].italic);
        assert_eq!(styles[3], Style::PLAIN);
    }

    #[test]
    fn heading_levels() {
        for (on, off, lvl) in [
            (H1_ON, H1_OFF, 1u8),
            (H2_ON, H2_OFF, 2),
            (H3_ON, H3_OFF, 3),
            (H4_ON, H4_OFF, 4),
            (H5_ON, H5_OFF, 5),
            (H6_ON, H6_OFF, 6),
        ] {
            let mut b = Vec::new();
            b.extend_from_slice(&m(on));
            b.extend_from_slice(b"h");
            b.extend_from_slice(&m(off));
            b.extend_from_slice(b"t");
            let ev = drain(&b);
            match ev[0] {
                Event::Word { style, .. } => assert_eq!(style.heading, lvl),
                _ => panic!(),
            }
            match ev[1] {
                Event::Word { style, .. } => assert_eq!(style.heading, 0),
                _ => panic!(),
            }
        }
    }

    #[test]
    fn block_record_round_trips_and_resets_at_paragraph_end() {
        let props = BlockProps {
            align: Align::Center,
            left: 2,
            text_indent_qem: 6,
            space_above_qem: 4,
        };
        let mut b = Vec::new();
        b.extend_from_slice(&props.encode());
        b.extend_from_slice(b"x\n\ny");
        let mut src = SliceSource(&b);
        let mut ev = Events::new(&mut src);
        assert!(matches!(ev.next(), Some(Event::Block { at: 5, block }) if block == props));
        assert_eq!(ev.block(), props);
        assert!(matches!(ev.next(), Some(Event::Word { .. })));
        assert!(matches!(ev.next(), Some(Event::ParagraphBreak { .. })));
        assert_eq!(ev.block(), BlockProps::DEFAULT);
    }

    #[test]
    fn hard_break_keeps_alignment_but_drops_indent_and_space() {
        let props = BlockProps {
            align: Align::Right,
            left: 1,
            text_indent_qem: 6,
            space_above_qem: 4,
        };
        let mut b = Vec::new();
        b.extend_from_slice(&props.encode());
        b.extend_from_slice(b"x\ny");
        let mut src = SliceSource(&b);
        let mut ev = Events::new(&mut src);
        ev.next();
        ev.next();
        assert!(matches!(ev.next(), Some(Event::HardBreak { .. })));
        assert_eq!(ev.block(), props.continued());
        assert_eq!(ev.block().align, Align::Right);
        assert_eq!(ev.block().left, 1);
        assert_eq!(ev.block().text_indent_qem, 0);
    }

    #[test]
    fn truncated_block_record_is_unknown() {
        let ev = drain(&[MARKER, BLOCK, 0x20]);
        assert!(matches!(ev[0], Event::Unknown { tag: BLOCK, end: 2, .. }));
    }

    #[test]
    fn page_and_thematic_break() {
        let mut b = Vec::new();
        b.extend_from_slice(&m(PAGE_BREAK));
        b.extend_from_slice(&m(BREAK));
        let ev = drain(&b);
        assert!(matches!(ev[0], Event::PageBreak { start: 0, end: 2 }));
        assert!(matches!(ev[1], Event::ThematicBreak { start: 2, end: 4 }));
    }

    #[test]
    fn image_record_payload() {
        let mut b = Vec::new();
        b.push(MARKER);
        b.push(IMG_REF);
        b.push(IMG_FLAG_FIGURE);
        b.extend_from_slice(&300u16.to_le_bytes());
        b.extend_from_slice(&200u16.to_le_bytes());
        b.push(3); // alt_len
        b.push(5); // path_len
        b.extend_from_slice(b"alt");
        b.extend_from_slice(b"a.jpg");
        b.extend_from_slice(b"t");
        let ev = drain(&b);
        match ev[0] {
            Event::Image(img) => {
                assert_eq!(img.start, 0);
                assert_eq!(img.alt_start, 9);
                assert_eq!(img.path_start, 12);
                assert_eq!(img.end, 17);
                assert_eq!(img.attr_w, 300);
                assert_eq!(img.attr_h, 200);
                assert_eq!(img.flags, IMG_FLAG_FIGURE);
                assert_eq!(img.alt(&b), b"alt");
                assert_eq!(img.path(&b), b"a.jpg");
                assert_eq!(ImageRef::parse(&b, 0), Some(img));
            }
            _ => panic!("expected image, got {:?}", ev[0]),
        }
        assert!(matches!(ev[1], Event::Word { start: 17, end: 18, .. }));
    }

    #[test]
    fn truncated_image_is_unknown_and_does_not_panic() {
        let ev = drain(&[MARKER, IMG_REF, 0]);
        assert!(matches!(ev[0], Event::Unknown { tag: IMG_REF, end: 2, .. }));
        let ev = drain(&[MARKER, IMG_REF, 0, 0, 0, 0, 0, 0, 0, b'X']);
        assert!(matches!(ev[0], Event::Unknown { tag: IMG_REF, .. }));
    }

    #[test]
    fn unknown_marker_and_dangling_marker() {
        let ev = drain(&[MARKER, b'?', b'a', MARKER]);
        assert!(matches!(ev[0], Event::Unknown { tag: b'?', start: 0, end: 2 }));
        assert!(matches!(ev[1], Event::Word { start: 2, end: 3, .. }));
        assert_eq!(ev.len(), 2);
    }

    #[test]
    fn resume_seeds_style() {
        let b = b"x";
        let mut src = SliceSource(b);
        let seed = Style {
            bold: true,
            ..Style::PLAIN
        };
        let mut ev = Events::resume(&mut src, 0, seed, BlockProps::DEFAULT);
        match ev.next() {
            Some(Event::Word { style, .. }) => assert!(style.bold),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn style_pack_round_trip() {
        let s = Style {
            bold: true,
            italic: false,
            underline: true,
            strike: false,
            heading: 5,
        };
        assert_eq!(Style::unpack(s.pack()), s);
        assert_eq!(Style::unpack(0), Style::PLAIN);
    }

    #[test]
    fn control_bytes_are_skipped() {
        let ev = drain(b"a\r\tb");
        assert!(matches!(ev[0], Event::Word { start: 0, end: 1, .. }));
        assert!(matches!(ev[1], Event::Word { start: 3, end: 4, .. }));
    }
}
