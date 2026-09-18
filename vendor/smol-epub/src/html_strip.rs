//! Single-pass HTML to styled-text converter for EPUB XHTML.
//!
//! [`HtmlStripStream`]: streaming `feed`/`finish` interface; emits the
//! [`crate::markup`] encoding: plain UTF-8 text with two-byte style
//! toggles, a block record per paragraph and image references.
//!
//! Styling is resolved here, once, at strip time. Every open tag goes
//! through the cascade `user-agent defaults < book stylesheet < inline
//! style`, and an element stack records what each open changed so its
//! close restores exactly that: nested `<b><span class="bold">` does
//! not emit a premature bold-off, `font-weight: normal` inside bold
//! turns bold off and back on, and an inline tag left open is unwound
//! by the block that closes over it.
//!
//! [`strip_html_inplace`]: in-place variant for `container.xml` / OPF /
//! TOC, plain text only.

use alloc::vec::Vec;

use crate::css::{self, CssRules, StyleProps};
pub use crate::markup::{
    is_marker, BLOCK, BLOCK_LEN, BOLD_OFF, BOLD_ON, BREAK, H1_OFF, H1_ON, H2_OFF, H2_ON, H3_OFF,
    H3_ON, H4_OFF, H4_ON, H5_OFF, H5_ON, H6_OFF, H6_ON, IMG_FLAG_BLOCK, IMG_FLAG_FIGURE,
    IMG_HEADER_LEN, IMG_REF, ITALIC_OFF, ITALIC_ON, MARKER, PAGE_BREAK, STRIKE_OFF, STRIKE_ON,
    UNDERLINE_OFF, UNDERLINE_ON,
};
use crate::markup::{Align, BlockProps};

const TAG_BUF_CAP: usize = 16;
const ENTITY_BUF_CAP: usize = 12;
const BANG_BUF_CAP: usize = 8;
// close-side bytes: unwinding a block over several open inline tags
// emits up to five off markers per element
const PENDING_CAP: usize = 64;
// holds queued deferred bytes flushed before the next text byte: a
// heading marker, up to four inline on/off markers, the `<li>` bullet
const DEFERRED_CAP: usize = 32;
const CLASS_CAP: usize = 64;
const STYLE_VAL_CAP: usize = 96;
const ELEM_STACK_CAP: usize = 16;

// img_src layout during <img> attribute capture:
//   [0..9)                 reserved for the marker header, filled at finish
//   [9..9+IMG_ALT_CAP)     alt-text buffer (img_alt_w bytes valid)
//   [9+IMG_ALT_CAP..end)   path buffer (img_w bytes valid)
// at finish, path is shifted left to follow alt and the header is written
// in front, producing a single drainable [MARKER, IMG_REF, flags, w_lo,
// w_hi, h_lo, h_hi, alt_len, path_len, alt..., path...] payload. The total
// post-compaction length is at most IMG_SRC_CAP, which keeps img_w (u8)
// usable as the drain cursor.
const IMG_SRC_CAP: usize = 192;
const IMG_ALT_CAP: usize = 48;
const IMG_PATH_OFFSET: usize = IMG_HEADER_LEN + IMG_ALT_CAP;
const IMG_PATH_CAP: usize = IMG_SRC_CAP - IMG_PATH_OFFSET;

// captured <img> attribute kinds
const IMG_ATTR_NONE: u8 = 0;
const IMG_ATTR_SRC: u8 = 1;
const IMG_ATTR_ALT: u8 = 2;
const IMG_ATTR_WIDTH: u8 = 3;
const IMG_ATTR_HEIGHT: u8 = 4;

// captured element attribute kinds
const ATTR_NONE: u8 = 0;
const ATTR_CLASS: u8 = 1;
const ATTR_STYLE: u8 = 2;

// inline style bits, shared by `inline_state` and `Elem::{changed, prev}`
const INLINE_BOLD: u8 = 1 << 0;
const INLINE_ITALIC: u8 = 1 << 1;
const INLINE_UNDERLINE: u8 = 1 << 2;
const INLINE_STRIKE: u8 = 1 << 3;

// `Elem::kind` bits
const ELEM_BLOCK: u8 = 1 << 0;
const ELEM_HIDDEN: u8 = 1 << 1;
const ELEM_FIGURE: u8 = 1 << 2;
const ELEM_CELL: u8 = 1 << 3;

// `pending_breaks` bits, flushed ahead of the block record
const PEND_THEMATIC: u8 = 1 << 0;
const PEND_PAGE: u8 = 1 << 1;

const H_ON: [u8; 7] = [0, H1_ON, H2_ON, H3_ON, H4_ON, H5_ON, H6_ON];
const H_OFF: [u8; 7] = [0, H1_OFF, H2_OFF, H3_OFF, H4_OFF, H5_OFF, H6_OFF];

// state machine phases
#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
enum Phase {
    Text,
    AfterLt,
    TagName,
    TagBody,
    Entity,
    SkipContent,
    SkipLt,
    SkipCloseName,
    SkipToGt,
    BangProbe,
    Comment,
    Cdata,
    Pi,
    BangOther,
    ImgBody,
    ImgAttrName,
    ImgAttrGap,
    ImgValStart,
    ImgSrcVal,
    ImgSkipVal,
    // attribute scan for every other open tag. same shape as the Img*
    // states; only `class` and `style` are captured, other attributes
    // are skipped.
    AttrLook,
    AttrName,
    AttrGap,
    ValStart,
    ValBody,
    ValSkip,
}

// one open element. records what its open changed so the close can
// restore it, and the block state in force before it opened.
#[derive(Clone, Copy)]
struct Elem {
    // css::class_hash of the lowercase tag name, for close matching
    tag: u16,
    // INLINE_* bits this element toggled, and their previous values
    changed: u8,
    prev: u8,
    // 1..=6 when this element emitted a heading marker
    heading: u8,
    kind: u8,
    // effective block state before this element opened
    prev_align: u8,
    prev_left: u8,
    prev_indent: u8,
    // bottom margin in quarter-em, collapsed into the next gap on close
    margin_bottom: u8,
    // deferred queue length and generation when this element opened
    dmark: u8,
    dgen: u8,
}

impl Elem {
    const EMPTY: Self = Self {
        tag: 0,
        changed: 0,
        prev: 0,
        heading: 0,
        kind: 0,
        prev_align: 0,
        prev_left: 0,
        prev_indent: 0,
        margin_bottom: 0,
        dmark: 0,
        dgen: 0,
    };
}

impl Default for HtmlStripStream {
    fn default() -> Self {
        Self::new()
    }
}

/// Stateful, streaming HTML-to-styled-text converter.
///
/// Feed chunks of EPUB XHTML via [`feed`](Self::feed), then call
/// [`finish`](Self::finish) to flush any trailing state. The output is
/// the [`crate::markup`] encoding. The stylesheet is passed to every
/// `feed` call rather than stored, so the stream carries no lifetime.
pub struct HtmlStripStream {
    phase: Phase,

    // tag name accumulation (also reused for attribute names)
    tag_buf: [u8; TAG_BUF_CAP],
    tag_len: u8,
    is_close_tag: bool,
    enter_skip: bool, // tag is skip-content; enter SkipContent on >

    // the element name being opened, kept while its attributes scan
    elem_buf: [u8; TAG_BUF_CAP],
    elem_len: u8,

    // entity accumulation
    entity_buf: [u8; ENTITY_BUF_CAP],
    entity_len: u8,

    // skip content
    skip_target: Option<SkipTag>,
    skip_match: bool, // in SkipToGt: did close tag name match?

    // bang construct probing
    bang_buf: [u8; BANG_BUF_CAP],
    bang_len: u8,

    // terminator matching (comment / CDATA / PI)
    match_pos: u8,

    // output state
    last_was_space: bool,
    trailing_nl: u8, // deferred newlines; flushed before next visible byte; capped at 2
    has_output: bool, // true once any visible char emitted; suppresses leading whitespace

    // deferred open-side bytes (heading marker, inline toggles, bullet);
    // they appear after the paragraph prefix and before the text.
    // close-side markers go to `pending` immediately.
    deferred: [u8; DEFERRED_CAP],
    deferred_len: u8,

    // pending output: bytes queued but not yet drained to the caller
    pending: [u8; PENDING_CAP],
    pend_w: u8,
    pend_r: u8,

    // image attribute capture (<img src=alt=width=height>)
    img_src: [u8; IMG_SRC_CAP],
    img_w: u8,           // path bytes written, relative to IMG_PATH_OFFSET
    img_alt_w: u8,       // alt bytes written, relative to IMG_HEADER_LEN
    img_r: u8,           // drain read cursor (after finish_img_tag)
    img_w_attr: u16,     // <img width="..."> parsed pixels, 0 if absent
    img_h_attr: u16,     // <img height="..."> parsed pixels, 0 if absent
    img_num: u16,        // accumulated digits for width/height value
    img_flags: u8,       // emitted as IMG_REF flags byte
    img_attr_kind: u8,   // IMG_ATTR_NONE / SRC / ALT / WIDTH / HEIGHT
    capture_img: bool,   // true while inside <img> tag body
    img_quote: u8,       // active quote char in attribute value
    in_figure: u8,       // open <figure> depth

    // class="..." / style="..." capture for every other open tag
    capture_attrs: bool,
    attr_kind: u8,
    attr_quote: u8,
    self_closing: bool, // saw `/` right before `>`
    class_buf: [u8; CLASS_CAP],
    class_w: u8,
    style_val_buf: [u8; STYLE_VAL_CAP],
    style_val_w: u8,

    // element stack
    stack: [Elem; ELEM_STACK_CAP],
    depth: u8,
    // INLINE_* bits currently in force in the output
    inline_state: u8,
    // effective block state (inherited through the stack)
    cur_align: u8,
    cur_left: u8,
    cur_indent: u8,
    // collapsed margin waiting for the next paragraph, quarter-em
    pending_space: u8,
    pending_breaks: u8,
    // bumped every time the deferred queue is flushed, so a close can
    // tell whether its open-side bytes went out yet
    deferred_gen: u8,
    // number of open elements inside a display:none subtree
    hidden_depth: u8,
}

impl HtmlStripStream {
    /// Create a new stream in its initial state.
    pub const fn new() -> Self {
        Self {
            phase: Phase::Text,
            tag_buf: [0u8; TAG_BUF_CAP],
            tag_len: 0,
            is_close_tag: false,
            enter_skip: false,
            elem_buf: [0u8; TAG_BUF_CAP],
            elem_len: 0,
            entity_buf: [0u8; ENTITY_BUF_CAP],
            entity_len: 0,
            skip_target: None,
            skip_match: false,
            bang_buf: [0u8; BANG_BUF_CAP],
            bang_len: 0,
            match_pos: 0,
            last_was_space: true,
            trailing_nl: 0,
            has_output: false,
            deferred: [0u8; DEFERRED_CAP],
            deferred_len: 0,
            pending: [0u8; PENDING_CAP],
            pend_w: 0,
            pend_r: 0,
            img_src: [0u8; IMG_SRC_CAP],
            img_w: 0,
            img_alt_w: 0,
            img_r: 0,
            img_w_attr: 0,
            img_h_attr: 0,
            img_num: 0,
            img_flags: 0,
            img_attr_kind: IMG_ATTR_NONE,
            capture_img: false,
            img_quote: 0,
            in_figure: 0,
            capture_attrs: false,
            attr_kind: ATTR_NONE,
            attr_quote: 0,
            self_closing: false,
            class_buf: [0u8; CLASS_CAP],
            class_w: 0,
            style_val_buf: [0u8; STYLE_VAL_CAP],
            style_val_w: 0,
            stack: [Elem::EMPTY; ELEM_STACK_CAP],
            depth: 0,
            inline_state: 0,
            cur_align: 0,
            cur_left: 0,
            cur_indent: 0,
            pending_space: 0,
            pending_breaks: 0,
            deferred_gen: 0,
            hidden_depth: 0,
        }
    }

    /// Process a chunk of HTML input against the book's stylesheet
    /// (pass [`CssRules::EMPTY`] when there is none).
    ///
    /// Returns `(consumed, written)`. If `consumed < input.len()`, call
    /// again with the remaining input (the output buffer was full).
    pub fn feed(&mut self, input: &[u8], output: &mut [u8], css: &CssRules) -> (usize, usize) {
        let ilen = input.len();
        let olen = output.len();
        let mut ip: usize = 0;
        let mut op: usize = 0;

        loop {
            // step 1: drain pending bytes to output
            while self.pend_r < self.pend_w {
                if op >= olen {
                    return (ip, op);
                }
                output[op] = self.pending[self.pend_r as usize];
                op += 1;
                self.pend_r += 1;
            }
            self.pend_r = 0;
            self.pend_w = 0;

            // step 1.5: drain image reference
            if !self.capture_img && self.img_r < self.img_w {
                while self.img_r < self.img_w {
                    if op >= olen {
                        return (ip, op);
                    }
                    output[op] = self.img_src[self.img_r as usize];
                    op += 1;
                    self.img_r += 1;
                }
                self.img_r = 0;
                self.img_w = 0;
            }

            // step 2: check for end of input
            if ip >= ilen {
                return (ip, op);
            }

            // step 3: process one input byte
            let b = input[ip];
            let mut advance = true;

            match self.phase {
                // normal text
                Phase::Text => {
                    if b == b'<' {
                        self.phase = Phase::AfterLt;
                    } else if self.hidden_depth > 0 || b == MARKER {
                        // inside display:none, or a literal 0x01 in the
                        // source (SOH never appears in real EPUBs): drop
                    } else if b == b'&' {
                        self.entity_len = 0;
                        self.phase = Phase::Entity;
                    } else if is_html_ws(b) {
                        self.queue_ws();
                    } else {
                        // ASCII printable and UTF-8 bytes (lead + continuation)
                        // pass through directly; multi-byte sequences are
                        // preserved in the output as-is
                        self.queue_text(b);
                    }
                }

                // after '<'
                Phase::AfterLt => match b {
                    b'!' => {
                        self.bang_len = 0;
                        self.phase = Phase::BangProbe;
                    }
                    b'?' => {
                        self.match_pos = 0;
                        self.phase = Phase::Pi;
                    }
                    b'/' => {
                        self.is_close_tag = true;
                        self.tag_len = 0;
                        self.enter_skip = false;
                        self.phase = Phase::TagName;
                    }
                    b'>' => {
                        // empty <>; ignore
                        self.phase = Phase::Text;
                    }
                    _ => {
                        self.is_close_tag = false;
                        self.tag_len = 0;
                        self.enter_skip = false;
                        self.phase = Phase::TagName;
                        advance = false; // reprocess in TagName
                    }
                },

                // accumulating tag name
                Phase::TagName => {
                    if is_tag_delim(b) {
                        self.classify_tag();

                        if b == b'>' {
                            if self.capture_img {
                                self.finish_img_tag();
                            } else if self.capture_attrs {
                                self.finish_elem_tag(css);
                            } else {
                                self.finish_tag();
                            }
                        } else if self.capture_img {
                            self.phase = Phase::ImgBody;
                        } else if self.capture_attrs {
                            if b == b'/' {
                                self.self_closing = true;
                            }
                            self.phase = Phase::AttrLook;
                        } else {
                            self.phase = Phase::TagBody;
                        }
                    } else if (self.tag_len as usize) < TAG_BUF_CAP {
                        self.tag_buf[self.tag_len as usize] = b.to_ascii_lowercase();
                        self.tag_len += 1;
                    }
                    // overflow: stop accumulating, keep scanning for delimiter
                }

                // past tag name; skip attributes to '>'
                Phase::TagBody => {
                    if b == b'>' {
                        self.finish_tag();
                    }
                }

                // <img> attribute parsing; capture src="..."
                Phase::ImgBody => {
                    if b == b'>' {
                        self.finish_img_tag();
                    } else if b.is_ascii_alphabetic() || b == b'_' {
                        self.tag_len = 0;
                        self.tag_buf[0] = b.to_ascii_lowercase();
                        self.tag_len = 1;
                        self.phase = Phase::ImgAttrName;
                    }
                    // whitespace, '/', etc: stay
                }

                Phase::ImgAttrName => {
                    if b == b'=' {
                        self.classify_img_attr();
                        self.phase = Phase::ImgValStart;
                    } else if b == b'>' {
                        self.finish_img_tag();
                    } else if is_html_ws(b) || b == b'/' {
                        self.phase = Phase::ImgAttrGap;
                    } else if (self.tag_len as usize) < TAG_BUF_CAP {
                        self.tag_buf[self.tag_len as usize] = b.to_ascii_lowercase();
                        self.tag_len += 1;
                    }
                }

                Phase::ImgAttrGap => {
                    if b == b'=' {
                        self.classify_img_attr();
                        self.phase = Phase::ImgValStart;
                    } else if b == b'>' {
                        self.finish_img_tag();
                    } else if b.is_ascii_alphabetic() || b == b'_' {
                        self.tag_len = 0;
                        self.tag_buf[0] = b.to_ascii_lowercase();
                        self.tag_len = 1;
                        self.phase = Phase::ImgAttrName;
                    }
                    // whitespace, '/': stay
                }

                Phase::ImgValStart => {
                    self.img_num = 0;
                    if b == b'"' || b == b'\'' {
                        self.img_quote = b;
                        self.phase = next_img_val_phase(self.img_attr_kind);
                    } else if b == b'>' {
                        self.finish_img_tag();
                    } else if !is_html_ws(b) {
                        // unquoted attribute value: capture the first byte directly
                        self.img_quote = 0;
                        self.phase = next_img_val_phase(self.img_attr_kind);
                        self.capture_img_val_byte(b);
                    }
                }

                Phase::ImgSrcVal => {
                    let done = if self.img_quote != 0 {
                        b == self.img_quote
                    } else {
                        is_html_ws(b) || b == b'>' || b == b'/'
                    };
                    if done {
                        self.commit_img_num();
                        self.img_attr_kind = IMG_ATTR_NONE;
                        self.phase = Phase::ImgBody;
                        if self.img_quote == 0 && b == b'>' {
                            self.finish_img_tag();
                        }
                    } else {
                        self.capture_img_val_byte(b);
                    }
                }

                Phase::ImgSkipVal => {
                    let done = if self.img_quote != 0 {
                        b == self.img_quote
                    } else {
                        is_html_ws(b) || b == b'>' || b == b'/'
                    };
                    if done {
                        self.phase = Phase::ImgBody;
                        if self.img_quote == 0 && b == b'>' {
                            self.finish_img_tag();
                        }
                    }
                }

                // attribute scan for every other open tag: name accumulator
                // -> '=' -> value capture (class / style) or skip. a `/`
                // right before `>` marks the element self-closing.
                Phase::AttrLook => {
                    if b == b'>' {
                        self.finish_elem_tag(css);
                    } else if b == b'/' {
                        self.self_closing = true;
                    } else if b.is_ascii_alphabetic() || b == b'_' {
                        self.self_closing = false;
                        self.tag_buf[0] = b.to_ascii_lowercase();
                        self.tag_len = 1;
                        self.phase = Phase::AttrName;
                    }
                    // whitespace: stay
                }

                Phase::AttrName => {
                    if b == b'=' {
                        self.classify_attr();
                        self.phase = Phase::ValStart;
                    } else if b == b'>' {
                        self.finish_elem_tag(css);
                    } else if b == b'/' {
                        self.self_closing = true;
                        self.phase = Phase::AttrGap;
                    } else if is_html_ws(b) {
                        self.phase = Phase::AttrGap;
                    } else if (self.tag_len as usize) < TAG_BUF_CAP {
                        self.tag_buf[self.tag_len as usize] = b.to_ascii_lowercase();
                        self.tag_len += 1;
                    }
                }

                Phase::AttrGap => {
                    if b == b'=' {
                        self.classify_attr();
                        self.phase = Phase::ValStart;
                    } else if b == b'>' {
                        self.finish_elem_tag(css);
                    } else if b == b'/' {
                        self.self_closing = true;
                    } else if b.is_ascii_alphabetic() || b == b'_' {
                        self.self_closing = false;
                        self.tag_buf[0] = b.to_ascii_lowercase();
                        self.tag_len = 1;
                        self.phase = Phase::AttrName;
                    }
                    // whitespace: stay
                }

                Phase::ValStart => {
                    if b == b'"' || b == b'\'' {
                        self.attr_quote = b;
                        self.phase = if self.attr_kind != ATTR_NONE {
                            Phase::ValBody
                        } else {
                            Phase::ValSkip
                        };
                    } else if b == b'>' {
                        self.finish_elem_tag(css);
                    } else if !is_html_ws(b) {
                        // unquoted value: this byte is already part of it
                        self.attr_quote = 0;
                        if self.attr_kind != ATTR_NONE {
                            self.capture_attr_byte(b);
                            self.phase = Phase::ValBody;
                        } else {
                            self.phase = Phase::ValSkip;
                        }
                    }
                }

                Phase::ValBody => {
                    let done = if self.attr_quote != 0 {
                        b == self.attr_quote
                    } else {
                        is_html_ws(b) || b == b'>'
                    };
                    if done {
                        self.attr_kind = ATTR_NONE;
                        self.phase = Phase::AttrLook;
                        if self.attr_quote == 0 && b == b'>' {
                            self.finish_elem_tag(css);
                        }
                    } else {
                        self.capture_attr_byte(b);
                    }
                }

                Phase::ValSkip => {
                    let done = if self.attr_quote != 0 {
                        b == self.attr_quote
                    } else {
                        is_html_ws(b) || b == b'>'
                    };
                    if done {
                        self.phase = Phase::AttrLook;
                        if self.attr_quote == 0 && b == b'>' {
                            self.finish_elem_tag(css);
                        }
                    }
                }

                // entity accumulation
                Phase::Entity => {
                    if b == b';' {
                        let name = &self.entity_buf[..self.entity_len as usize];
                        match resolve_entity(name) {
                            Some(cp) => self.queue_codepoint(cp),
                            None => {
                                // unrecognised entity; emit literal '&'
                                self.queue_text(b'&');
                            }
                        }
                        self.phase = Phase::Text;
                    } else if is_entity_char(b) && (self.entity_len as usize) < ENTITY_BUF_CAP {
                        self.entity_buf[self.entity_len as usize] = b;
                        self.entity_len += 1;
                    } else {
                        // invalid char or overflow; emit literal '&'
                        self.queue_text(b'&');
                        self.phase = Phase::Text;
                        advance = false; // reprocess this byte as text
                    }
                }

                // skip content (script / style / head)
                Phase::SkipContent => {
                    if b == b'<' {
                        self.phase = Phase::SkipLt;
                    }
                }

                Phase::SkipLt => {
                    if b == b'/' {
                        self.tag_len = 0;
                        self.phase = Phase::SkipCloseName;
                    } else {
                        self.phase = Phase::SkipContent;
                    }
                }

                Phase::SkipCloseName => {
                    if is_tag_delim(b) || b == b'>' {
                        let matched = if let Some(target) = self.skip_target {
                            let tgt = target.name();
                            let name = &self.tag_buf[..self.tag_len as usize];
                            name.len() == tgt.len()
                                && name.iter().zip(tgt.iter()).all(|(a, t)| *a == *t)
                        } else {
                            false
                        };

                        if b == b'>' {
                            if matched {
                                self.skip_target = None;
                                self.phase = Phase::Text;
                            } else {
                                self.phase = Phase::SkipContent;
                            }
                        } else {
                            self.skip_match = matched;
                            self.phase = Phase::SkipToGt;
                        }
                    } else if (self.tag_len as usize) < TAG_BUF_CAP {
                        self.tag_buf[self.tag_len as usize] = b.to_ascii_lowercase();
                        self.tag_len += 1;
                    }
                }

                Phase::SkipToGt => {
                    if b == b'>' {
                        if self.skip_match {
                            self.skip_target = None;
                            self.phase = Phase::Text;
                        } else {
                            self.phase = Phase::SkipContent;
                        }
                    }
                }

                // bang construct probing (after '<!')
                Phase::BangProbe => {
                    if b == b'>' {
                        self.phase = Phase::Text;
                    } else {
                        let pos = self.bang_len as usize;
                        if pos < BANG_BUF_CAP {
                            self.bang_buf[pos] = b;
                            self.bang_len += 1;
                        }
                        let n = self.bang_len as usize;

                        if n == 1 {
                            match b {
                                b'-' | b'[' => {}
                                _ => self.phase = Phase::BangOther,
                            }
                        } else if self.bang_buf[0] == b'-' {
                            if n == 2 && b == b'-' {
                                self.match_pos = 0;
                                self.phase = Phase::Comment;
                            } else {
                                self.phase = Phase::BangOther;
                            }
                        } else {
                            // bang_buf[0] == '[': check against "[CDATA["
                            const CDATA: &[u8] = b"[CDATA[";
                            if n <= CDATA.len() && b == CDATA[n - 1] {
                                if n == CDATA.len() {
                                    self.match_pos = 0;
                                    self.phase = Phase::Cdata;
                                }
                            } else {
                                self.phase = Phase::BangOther;
                            }
                        }
                    }
                }

                // comment: scanning for '-->'
                Phase::Comment => match self.match_pos {
                    0 => {
                        if b == b'-' {
                            self.match_pos = 1;
                        }
                    }
                    1 => {
                        if b == b'-' {
                            self.match_pos = 2;
                        } else {
                            self.match_pos = 0;
                        }
                    }
                    _ => {
                        if b == b'>' {
                            self.phase = Phase::Text;
                        } else if b != b'-' {
                            self.match_pos = 0;
                        }
                    }
                },

                // CDATA: scanning for ']]>'
                Phase::Cdata => match self.match_pos {
                    0 => {
                        if b == b']' {
                            self.match_pos = 1;
                        }
                    }
                    1 => {
                        if b == b']' {
                            self.match_pos = 2;
                        } else {
                            self.match_pos = 0;
                        }
                    }
                    _ => {
                        if b == b'>' {
                            self.phase = Phase::Text;
                        } else if b != b']' {
                            self.match_pos = 0;
                        }
                    }
                },

                // PI: scanning for '?>'
                Phase::Pi => match self.match_pos {
                    0 => {
                        if b == b'?' {
                            self.match_pos = 1;
                        }
                    }
                    _ => {
                        if b == b'>' {
                            self.phase = Phase::Text;
                        } else if b != b'?' {
                            self.match_pos = 0;
                        }
                    }
                },

                // other bang construct: scanning for '>'
                Phase::BangOther => {
                    if b == b'>' {
                        self.phase = Phase::Text;
                    }
                }
            }

            if advance {
                ip += 1;
            }
        }
    }

    /// Flush any pending state and append a terminal newline if content
    /// was produced. Returns the number of bytes written to `output`.
    pub fn finish(&mut self, output: &mut [u8]) -> usize {
        let mut op: usize = 0;

        // drain remaining pending bytes
        while self.pend_r < self.pend_w && op < output.len() {
            output[op] = self.pending[self.pend_r as usize];
            op += 1;
            self.pend_r += 1;
        }
        self.pend_r = 0;
        self.pend_w = 0;

        // terminal newline
        if self.has_output && op < output.len() {
            output[op] = b'\n';
            op += 1;
        }

        self.phase = Phase::Text;
        op
    }

    #[inline]
    fn push_pending(&mut self, byte: u8) {
        let w = self.pend_w as usize;
        if w < PENDING_CAP {
            self.pending[w] = byte;
            self.pend_w += 1;
        }
    }

    fn push_deferred_marker(&mut self, tag: u8) {
        let n = self.deferred_len as usize;
        if n + 2 <= DEFERRED_CAP {
            self.deferred[n] = MARKER;
            self.deferred[n + 1] = tag;
            self.deferred_len += 2;
        }
    }

    // append raw UTF-8 bytes to the deferred queue (no MARKER prefix). used
    // for tag-keyed text injection like the U+2022 bullet on `<li>`.
    fn push_deferred_bytes(&mut self, bytes: &[u8]) {
        let n = self.deferred_len as usize;
        if n + bytes.len() <= DEFERRED_CAP {
            self.deferred[n..n + bytes.len()].copy_from_slice(bytes);
            self.deferred_len += bytes.len() as u8;
        }
    }

    // queue visible text byte; the paragraph prefix goes out first
    fn queue_text(&mut self, b: u8) {
        self.flush_prefix();
        self.push_pending(b);
        self.last_was_space = false;
        self.has_output = true;
    }

    // everything that precedes a paragraph's first text byte, in stream
    // order: deferred newlines, thematic / page breaks, the block record,
    // then the deferred open-side bytes (heading marker, inline toggles,
    // bullet). the block record is skipped when the paragraph has default
    // properties, and after a single `\n` (a `<br>` continuation)
    fn flush_prefix(&mut self) {
        let para_start = !self.has_output || self.trailing_nl >= 2;
        if self.has_output {
            for _ in 0..self.trailing_nl {
                self.push_pending(b'\n');
            }
        }
        self.trailing_nl = 0;

        if self.pending_breaks & PEND_THEMATIC != 0 {
            self.push_pending(MARKER);
            self.push_pending(BREAK);
        }
        if self.pending_breaks & PEND_PAGE != 0 {
            self.push_pending(MARKER);
            self.push_pending(PAGE_BREAK);
        }
        self.pending_breaks = 0;

        if para_start {
            let block = BlockProps {
                align: Align::from_bits(self.cur_align),
                left: self.cur_left,
                text_indent_qem: self.cur_indent,
                // no space above the first paragraph of a chapter
                space_above_qem: if self.has_output { self.pending_space } else { 0 },
            };
            if !block.is_default() {
                for &x in block.encode().iter() {
                    self.push_pending(x);
                }
            }
        }
        self.pending_space = 0;

        let dlen = self.deferred_len as usize;
        for i in 0..dlen {
            self.push_pending(self.deferred[i]);
        }
        self.deferred_len = 0;
        self.deferred_gen = self.deferred_gen.wrapping_add(1);
    }

    // handle whitespace byte; collapse runs to a single space
    fn queue_ws(&mut self) {
        if self.last_was_space || !self.has_output {
            return;
        }
        self.last_was_space = true;

        // pending newlines already act as word separators
        if self.trailing_nl > 0 {
            return;
        }

        self.push_pending(b' ');
    }

    // queue a Unicode codepoint as UTF-8 bytes; handles whitespace codepoints
    fn queue_codepoint(&mut self, cp: u32) {
        if cp == 0 {
            return;
        }
        // newline
        if cp == 0x0A {
            self.trailing_nl = self.trailing_nl.saturating_add(1).min(2);
            self.last_was_space = true;
            return;
        }
        // whitespace (ASCII ws + NBSP + other Unicode spaces)
        if cp == 0x20 || cp == 0x09 || cp == 0x0D || cp == 0x0C || cp == 0xA0 {
            self.queue_ws();
            return;
        }
        // encode to UTF-8 and queue each byte
        let mut tmp = [0u8; 4];
        let n = encode_codepoint_utf8(cp, &mut tmp);
        for &byte in tmp.iter().take(n) {
            self.queue_text(byte);
        }
    }

    // classify the accumulated tag name: close tags unwind the element
    // stack, void tags act at once, every other open tag goes on to
    // attribute capture and is resolved in `finish_elem_tag`
    fn classify_tag(&mut self) {
        // copy tag name to a local to avoid borrowing self.tag_buf
        // while mutating self
        let mut tn = [0u8; TAG_BUF_CAP];
        let n = self.tag_len as usize;
        tn[..n].copy_from_slice(&self.tag_buf[..n]);
        let name = &tn[..n];

        if self.is_close_tag {
            self.close_elem(name);
            return;
        }

        // skip-content tags (script/style/head)
        if let Some(sk) = SkipTag::from_name(name) {
            self.skip_target = Some(sk);
            self.enter_skip = true;
            return;
        }

        match name {
            b"br" => {
                if self.hidden_depth == 0 {
                    self.trailing_nl = self.trailing_nl.saturating_add(1).min(2);
                    self.last_was_space = true;
                }
            }
            b"hr" => {
                if self.hidden_depth == 0 {
                    self.trailing_nl = 2;
                    self.last_was_space = true;
                    self.pending_breaks |= PEND_THEMATIC;
                    self.pending_space = self.pending_space.max(2);
                }
            }
            b"img" => {
                self.capture_img = true;
                self.img_w = 0;
                self.img_alt_w = 0;
                self.img_w_attr = 0;
                self.img_h_attr = 0;
                self.img_num = 0;
                self.img_attr_kind = IMG_ATTR_NONE;
                self.img_flags = if self.in_figure > 0 {
                    IMG_FLAG_FIGURE
                } else {
                    0
                };
            }
            _ if is_void_element(name) => {}
            _ => {
                self.elem_buf[..n].copy_from_slice(name);
                self.elem_len = n as u8;
                self.capture_attrs = true;
                self.attr_kind = ATTR_NONE;
                self.class_w = 0;
                self.style_val_w = 0;
                self.self_closing = false;
            }
        }
    }

    // transition out of TagName/TagBody on '>'
    fn finish_tag(&mut self) {
        if self.enter_skip {
            self.enter_skip = false;
            self.phase = Phase::SkipContent;
        } else {
            self.phase = Phase::Text;
        }
    }

    // the open tag's attributes are in; resolve and open the element
    fn finish_elem_tag(&mut self, css: &CssRules) {
        self.capture_attrs = false;
        self.attr_kind = ATTR_NONE;
        let self_closing = self.self_closing;
        self.self_closing = false;
        self.phase = Phase::Text;

        let mut name = [0u8; TAG_BUF_CAP];
        let n = self.elem_len as usize;
        name[..n].copy_from_slice(&self.elem_buf[..n]);
        self.open_elem(&name[..n], css);
        if self_closing {
            self.close_elem(&name[..n]);
        }
    }

    // identify the captured attribute name (in tag_buf)
    fn classify_attr(&mut self) {
        let name = &self.tag_buf[..self.tag_len as usize];
        self.attr_kind = match name {
            b"class" => ATTR_CLASS,
            b"style" => ATTR_STYLE,
            _ => ATTR_NONE,
        };
    }

    fn capture_attr_byte(&mut self, b: u8) {
        match self.attr_kind {
            ATTR_CLASS => {
                let w = self.class_w as usize;
                if w < CLASS_CAP {
                    self.class_buf[w] = b;
                    self.class_w += 1;
                }
            }
            ATTR_STYLE => {
                let w = self.style_val_w as usize;
                if w < STYLE_VAL_CAP {
                    self.style_val_buf[w] = b;
                    self.style_val_w += 1;
                }
            }
            _ => {}
        }
    }

    // resolve the cascade for an open tag and emit what changes
    fn open_elem(&mut self, name: &[u8], css: &CssRules) {
        let mut e = Elem {
            tag: css::class_hash(name),
            dmark: self.deferred_len,
            dgen: self.deferred_gen,
            ..Elem::EMPTY
        };

        // inside a hidden subtree nothing is emitted; the element is
        // pushed only so its close keeps the stack balanced
        if self.hidden_depth > 0 {
            e.kind = ELEM_HIDDEN;
            if self.push_elem(e) {
                self.hidden_depth += 1;
            }
            return;
        }

        let mut props = ua_defaults(name);
        if !css.is_empty() {
            let class = &self.class_buf[..self.class_w as usize];
            props.overlay(&css.resolve_classes(name, class));
        }
        if self.style_val_w > 0 {
            let inline = &self.style_val_buf[..self.style_val_w as usize];
            props.overlay(&StyleProps::parse_inline(inline));
        }

        if props.is_hidden() {
            e.kind = ELEM_HIDDEN;
            if self.push_elem(e) {
                self.hidden_depth += 1;
            }
            return;
        }

        let is_block = if props.set & css::PROP_DISPLAY == 0 {
            is_block_element(name)
        } else {
            match props.display {
                css::DISP_BLOCK => true,
                css::DISP_INLINE => false,
                _ => is_block_element(name),
            }
        };

        if is_block {
            e.kind |= ELEM_BLOCK;
            self.trailing_nl = self.trailing_nl.max(2);
            self.last_was_space = true;
            if props.set & css::PROP_MARGIN_TOP != 0 {
                self.pending_space = self.pending_space.max(qem_u8(props.margin_top));
            }
            if props.set & css::PROP_MARGIN_BOTTOM != 0 {
                e.margin_bottom = qem_u8(props.margin_bottom);
            }
            e.prev_align = self.cur_align;
            e.prev_left = self.cur_left;
            e.prev_indent = self.cur_indent;
            if props.set & css::PROP_TEXT_ALIGN != 0 {
                self.cur_align = match props.text_align {
                    css::TA_LEFT => Align::Left as u8,
                    css::TA_CENTER => Align::Center as u8,
                    css::TA_RIGHT => Align::Right as u8,
                    _ => Align::Default as u8,
                };
            }
            if props.set & css::PROP_MARGIN_LEFT != 0 && props.margin_left > 0 {
                // one indent level per whole em, rounded
                let levels = (props.margin_left as u8 + 2) / 4;
                self.cur_left = (self.cur_left + levels).min(BlockProps::LEFT_MAX);
            }
            if props.set & css::PROP_TEXT_INDENT != 0 {
                // hanging (negative) indents are not supported: flush left
                self.cur_indent = qem_u8(props.text_indent);
            }
            if props.is_page_break_before() {
                self.pending_breaks |= PEND_PAGE;
            }
        }

        match name {
            b"figure" => {
                self.in_figure = self.in_figure.saturating_add(1);
                e.kind |= ELEM_FIGURE;
            }
            b"td" | b"th" => e.kind |= ELEM_CELL,
            _ => {}
        }

        let lvl = heading_level(name);
        if lvl != 0 {
            e.heading = lvl;
            self.push_deferred_marker(H_ON[lvl as usize]);
        }

        // inline flags: emit only transitions, remember what to restore
        let td = props.set & css::PROP_TEXT_DECORATION != 0;
        let wants = [
            (INLINE_BOLD, props.set & css::PROP_FONT_WEIGHT != 0, props.is_bold()),
            (INLINE_ITALIC, props.set & css::PROP_FONT_STYLE != 0, props.is_italic()),
            (INLINE_UNDERLINE, td, props.is_underline()),
            (INLINE_STRIKE, td, props.is_line_through()),
        ];
        for (bit, specified, on) in wants {
            if !specified {
                continue;
            }
            let have = self.inline_state & bit != 0;
            if have == on {
                continue;
            }
            e.changed |= bit;
            if have {
                e.prev |= bit;
            }
            self.inline_state ^= bit;
            self.push_deferred_marker(inline_marker(bit, on));
        }

        if name == b"li" {
            self.push_deferred_bytes("\u{2022} ".as_bytes());
        }

        self.push_elem(e);
    }

    // returns false when the stack is full and the element was dropped;
    // a dropped element's close is then ignored by `close_elem`
    fn push_elem(&mut self, e: Elem) -> bool {
        let d = self.depth as usize;
        if d >= ELEM_STACK_CAP {
            return false;
        }
        self.stack[d] = e;
        self.depth += 1;
        true
    }

    // pop the nearest open element with this name, unwinding anything
    // left open above it (an unclosed inline tag ends with its block).
    // a close with no matching open is ignored
    fn close_elem(&mut self, name: &[u8]) {
        let tag = css::class_hash(name);
        let mut found = None;
        for i in (0..self.depth as usize).rev() {
            if self.stack[i].tag == tag {
                found = Some(i);
                break;
            }
        }
        let Some(target) = found else {
            return;
        };
        while self.depth as usize > target {
            self.depth -= 1;
            let e = self.stack[self.depth as usize];
            self.unwind(e);
        }
    }

    // emit the close side of one element and restore the state it changed
    fn unwind(&mut self, e: Elem) {
        if e.kind & ELEM_HIDDEN != 0 {
            self.hidden_depth = self.hidden_depth.saturating_sub(1);
            return;
        }

        // nothing was emitted since this element opened (empty or
        // self-closing): its open-side bytes are still queued, so drop
        // them rather than closing what never went out
        let unflushed = e.dgen == self.deferred_gen;
        if unflushed {
            self.deferred_len = e.dmark;
        }

        if e.heading != 0 && !unflushed {
            self.push_pending(MARKER);
            self.push_pending(H_OFF[e.heading as usize]);
        }

        for bit in [INLINE_BOLD, INLINE_ITALIC, INLINE_UNDERLINE, INLINE_STRIKE] {
            if e.changed & bit == 0 {
                continue;
            }
            let on = e.prev & bit != 0;
            if on {
                self.inline_state |= bit;
            } else {
                self.inline_state &= !bit;
            }
            if !unflushed {
                self.push_pending(MARKER);
                self.push_pending(inline_marker(bit, on));
            }
        }

        if e.kind & ELEM_FIGURE != 0 {
            self.in_figure = self.in_figure.saturating_sub(1);
        }

        if e.kind & ELEM_CELL != 0 {
            // cells of one row run on inline; keep a space between them
            if self.has_output && self.trailing_nl == 0 && !self.last_was_space {
                self.push_pending(b' ');
                self.last_was_space = true;
            }
        }

        if e.kind & ELEM_BLOCK != 0 {
            self.trailing_nl = self.trailing_nl.max(2);
            self.last_was_space = true;
            self.pending_space = self.pending_space.max(e.margin_bottom);
            self.cur_align = e.prev_align;
            self.cur_left = e.prev_left;
            self.cur_indent = e.prev_indent;
        }
    }

    // closes out any digit value pending from a previous attribute.
    fn classify_img_attr(&mut self) {
        // commit any in-flight numeric value before switching attribute
        self.commit_img_num();
        let name = &self.tag_buf[..self.tag_len as usize];
        self.img_attr_kind = match name {
            b"src" => IMG_ATTR_SRC,
            b"alt" => IMG_ATTR_ALT,
            b"width" => IMG_ATTR_WIDTH,
            b"height" => IMG_ATTR_HEIGHT,
            _ => IMG_ATTR_NONE,
        };
        self.img_num = 0;
    }

    // capture one byte of an attribute value. dispatches by `img_attr_kind`:
    //   src   -> append to img_src path region (offset IMG_PATH_OFFSET..)
    //   alt   -> append to img_src alt region (offset IMG_HEADER_LEN..)
    //   w/h   -> accumulate ASCII digits into img_num; ignore other chars
    //            so values like `width="80px"` keep the leading digits
    fn capture_img_val_byte(&mut self, b: u8) {
        match self.img_attr_kind {
            IMG_ATTR_SRC => {
                let pos = IMG_PATH_OFFSET + self.img_w as usize;
                if (self.img_w as usize) < IMG_PATH_CAP {
                    self.img_src[pos] = b;
                    self.img_w += 1;
                }
            }
            IMG_ATTR_ALT => {
                // collapse runs of whitespace, drop leading whitespace, and
                // skip the MARKER escape byte (cannot appear in payloads).
                let alt_pos = self.img_alt_w as usize;
                if b == MARKER {
                    return;
                }
                let is_ws = is_html_ws(b);
                if is_ws {
                    if alt_pos == 0 {
                        return;
                    }
                    if self.img_src[IMG_HEADER_LEN + alt_pos - 1] == b' ' {
                        return;
                    }
                    if alt_pos < IMG_ALT_CAP {
                        self.img_src[IMG_HEADER_LEN + alt_pos] = b' ';
                        self.img_alt_w += 1;
                    }
                } else if alt_pos < IMG_ALT_CAP {
                    self.img_src[IMG_HEADER_LEN + alt_pos] = b;
                    self.img_alt_w += 1;
                }
            }
            // ascii digits accumulate into img_num; non-digit bytes (units,
            // separators) are silently dropped so `width="80px"` parses as 80.
            IMG_ATTR_WIDTH | IMG_ATTR_HEIGHT if b.is_ascii_digit() => {
                let n = self.img_num.saturating_mul(10);
                self.img_num = n.saturating_add((b - b'0') as u16);
            }
            _ => {}
        }
    }

    // finalise the in-flight numeric attribute value by writing it to the
    // appropriate field. called when the attribute value ends (close quote
    // or whitespace) and on every attribute boundary.
    fn commit_img_num(&mut self) {
        match self.img_attr_kind {
            IMG_ATTR_WIDTH => self.img_w_attr = self.img_num,
            IMG_ATTR_HEIGHT => self.img_h_attr = self.img_num,
            _ => {}
        }
    }

    // trim trailing ASCII whitespace from the captured alt-text region.
    fn trim_img_alt_trailing_ws(&mut self) {
        while self.img_alt_w > 0 {
            let last_idx = IMG_HEADER_LEN + self.img_alt_w as usize - 1;
            if self.img_src[last_idx] == b' ' {
                self.img_alt_w -= 1;
            } else {
                break;
            }
        }
    }


    // finish <img> tag; emit image-ref marker if src was captured
    fn finish_img_tag(&mut self) {
        // commit any in-flight numeric value (e.g. unquoted width="123")
        self.commit_img_num();
        // tidy up captured alt
        self.trim_img_alt_trailing_ws();

        let path_len = self.img_w as usize;
        let alt_len = self.img_alt_w as usize;
        self.capture_img = false;

        if path_len > 0 && self.hidden_depth == 0 {
            // an image is its own block (the consumer demotes it to inline
            // based on dimensions); it takes the paragraph prefix like text
            if self.has_output {
                self.trailing_nl = self.trailing_nl.max(2);
            }
            self.flush_prefix();

            // compact: shift path bytes from IMG_PATH_OFFSET to IMG_HEADER_LEN
            // + alt_len so they sit immediately after the alt buffer. then
            // write the 9-byte header in front.
            let path_dst = IMG_HEADER_LEN + alt_len;
            if path_dst != IMG_PATH_OFFSET {
                // copy_within handles overlap; src and dst don't alias here
                // because path_dst < IMG_PATH_OFFSET (alt_len < IMG_ALT_CAP)
                self.img_src.copy_within(
                    IMG_PATH_OFFSET..IMG_PATH_OFFSET + path_len,
                    path_dst,
                );
            }
            let total_len = IMG_HEADER_LEN + alt_len + path_len;
            // build header
            self.img_src[0] = MARKER;
            self.img_src[1] = IMG_REF;
            self.img_src[2] = self.img_flags;
            self.img_src[3] = (self.img_w_attr & 0xFF) as u8;
            self.img_src[4] = (self.img_w_attr >> 8) as u8;
            self.img_src[5] = (self.img_h_attr & 0xFF) as u8;
            self.img_src[6] = (self.img_h_attr >> 8) as u8;
            self.img_src[7] = alt_len as u8;
            self.img_src[8] = path_len as u8;

            self.img_r = 0;
            self.img_w = total_len as u8;

            // block break after image
            self.trailing_nl = 2;
            self.last_was_space = true;
            self.has_output = true;
        } else {
            self.img_w = 0;
        }

        // reset capture state for the next image
        self.img_alt_w = 0;
        self.img_w_attr = 0;
        self.img_h_attr = 0;
        self.img_num = 0;
        self.img_flags = 0;
        self.img_attr_kind = IMG_ATTR_NONE;

        self.phase = Phase::Text;
    }
}
// pick the post-quote phase based on which attribute we captured. width and
// height share the same accumulation phase as src and alt (`ImgSrcVal`); the
// dispatch inside `capture_img_val_byte` decides what to do per byte.
fn next_img_val_phase(kind: u8) -> Phase {
    match kind {
        IMG_ATTR_NONE => Phase::ImgSkipVal,
        _ => Phase::ImgSrcVal,
    }
}


/// Strip HTML tags from a complete buffer **in place**, producing plain text
/// without style markers.
///
/// The write cursor never passes the read cursor, so no extra allocation
/// is needed.
pub fn strip_html_inplace(buf: &mut Vec<u8>) {
    let len = buf.len();
    if len == 0 {
        return;
    }

    let mut r: usize = 0;
    let mut w: usize = 0;
    let mut last_was_space = true;
    let mut trailing_nl: u8 = 1;
    let mut skip_until: Option<SkipTag> = None;

    while r < len {
        if let Some(skip) = skip_until {
            if let Some(end_pos) = find_close_tag(&buf[r..], skip.name()) {
                r += end_pos;
                skip_until = None;
            } else {
                break;
            }
            continue;
        }

        let b = buf[r];

        if b == b'<' {
            r += 1;
            if r >= len {
                break;
            }

            if buf[r] == b'!' {
                r = skip_bang_construct(buf, r);
                continue;
            }
            if buf[r] == b'?' {
                r = skip_pi(buf, r);
                continue;
            }

            let is_close = buf[r] == b'/';
            if is_close {
                r += 1;
            }

            let name_start = r;
            while r < len && !is_tag_delim(buf[r]) {
                r += 1;
            }
            let mut tn = [0u8; 16];
            let tn_len = (r - name_start).min(16);
            for i in 0..tn_len {
                tn[i] = buf[name_start + i].to_ascii_lowercase();
            }
            let tag = &tn[..tn_len];

            if !is_close && let Some(sk) = SkipTag::from_name(tag) {
                skip_until = Some(sk);
            }

            if is_block_element(tag) {
                while trailing_nl < 2 {
                    buf[w] = b'\n';
                    w += 1;
                    trailing_nl += 1;
                }
                last_was_space = true;
            }

            if tag == b"br" {
                buf[w] = b'\n';
                w += 1;
                trailing_nl = trailing_nl.saturating_add(1);
                last_was_space = true;
            }

            while r < len && buf[r] != b'>' {
                r += 1;
            }
            if r < len {
                r += 1;
            }
            continue;
        }

        if b == b'&' {
            let (decoded, advance) = decode_entity_inplace(buf, r);
            r += advance;

            match decoded {
                DecodedInplace::Codepoint(cp) => {
                    if cp == 0x0A {
                        buf[w] = b'\n';
                        w += 1;
                        trailing_nl = trailing_nl.saturating_add(1);
                        last_was_space = true;
                    } else if cp == 0x20 || cp == 0x09 || cp == 0x0D || cp == 0x0C || cp == 0xA0 {
                        if !last_was_space {
                            buf[w] = b' ';
                            w += 1;
                            last_was_space = true;
                            trailing_nl = 0;
                        }
                    } else {
                        let mut tmp = [0u8; 4];
                        let n = encode_codepoint_utf8(cp, &mut tmp);
                        buf[w..w + n].copy_from_slice(&tmp[..n]);
                        w += n;
                        last_was_space = false;
                        trailing_nl = 0;
                    }
                }

                DecodedInplace::None => {
                    buf[w] = b'&';
                    w += 1;
                    last_was_space = false;
                    trailing_nl = 0;
                }
            }
            continue;
        }

        if is_html_ws(b) {
            if !last_was_space {
                buf[w] = b' ';
                w += 1;
                last_was_space = true;
                trailing_nl = 0;
            }
        } else if b >= 0xC0 {
            // UTF-8 lead byte: check for NBSP (U+00A0 = 0xC2 0xA0),
            // otherwise pass through the full sequence unchanged
            let seq_len = utf8_seq_len(b);
            let end = (r + seq_len).min(len);
            if b == 0xC2 && r + 1 < len && buf[r + 1] == 0xA0 {
                // non-breaking space -> regular space
                if !last_was_space {
                    buf[w] = b' ';
                    w += 1;
                    last_was_space = true;
                    trailing_nl = 0;
                }
            } else {
                for idx in r..end {
                    buf[w] = buf[idx];
                    w += 1;
                }
                last_was_space = false;
                trailing_nl = 0;
            }
            r = end;
            continue;
        } else if b >= 0x80 {
            // UTF-8 continuation byte (possibly stray): pass through
            buf[w] = b;
            w += 1;
            last_was_space = false;
            trailing_nl = 0;
            r += 1;
            continue;
        } else {
            buf[w] = b;
            w += 1;
            last_was_space = false;
            trailing_nl = 0;
        }

        r += 1;
    }

    while w > 0 && (buf[w - 1] == b' ' || buf[w - 1] == b'\n') {
        w -= 1;
    }
    if w > 0 {
        buf[w] = b'\n';
        w += 1;
    }

    buf.truncate(w);
}

fn is_block_element(name: &[u8]) -> bool {
    matches!(
        name,
        b"p" | b"div"
            | b"h1"
            | b"h2"
            | b"h3"
            | b"h4"
            | b"h5"
            | b"h6"
            | b"li"
            | b"ul"
            | b"ol"
            | b"dl"
            | b"dt"
            | b"dd"
            | b"tr"
            | b"blockquote"
            | b"section"
            | b"article"
            | b"aside"
            | b"figure"
            | b"figcaption"
            | b"header"
            | b"footer"
            | b"nav"
            | b"pre"
            | b"hr"
            | b"table"
    )
}

// what a tag means before any stylesheet: the user-agent defaults of the
// cascade. margins are quarter-em and follow the browser defaults (p 1em,
// headings by level) so a book without CSS reads the way it does in a
// browser; a book with CSS overrides any of this per rule
fn ua_defaults(tag: &[u8]) -> StyleProps {
    use css::*;
    let mut p = StyleProps::EMPTY;
    fn margins(p: &mut StyleProps, top: i8, bottom: i8) {
        p.margin_top = top;
        p.margin_bottom = bottom;
        p.set |= PROP_MARGIN_TOP | PROP_MARGIN_BOTTOM;
    }
    fn italic(p: &mut StyleProps) {
        p.font_style = FS_ITALIC;
        p.set |= PROP_FONT_STYLE;
    }
    fn center(p: &mut StyleProps) {
        p.text_align = TA_CENTER;
        p.set |= PROP_TEXT_ALIGN;
    }
    match tag {
        b"b" | b"strong" => {
            p.font_weight = FW_BOLD;
            p.set |= PROP_FONT_WEIGHT;
        }
        b"i" | b"em" | b"cite" | b"dfn" | b"var" | b"address" => italic(&mut p),
        b"u" | b"ins" => {
            p.text_decoration = TD_UNDERLINE;
            p.set |= PROP_TEXT_DECORATION;
        }
        b"s" | b"strike" | b"del" => {
            p.text_decoration = TD_LINE_THROUGH;
            p.set |= PROP_TEXT_DECORATION;
        }
        b"p" => margins(&mut p, 4, 4),
        b"h1" => {
            margins(&mut p, 3, 3);
            center(&mut p);
            p.page_break_before = PB_ALWAYS;
            p.set |= PROP_PAGE_BREAK;
        }
        b"h2" => {
            margins(&mut p, 3, 3);
            center(&mut p);
        }
        b"h3" => margins(&mut p, 4, 4),
        b"h4" => margins(&mut p, 5, 5),
        b"h5" => margins(&mut p, 7, 7),
        b"h6" => margins(&mut p, 9, 9),
        b"blockquote" => {
            margins(&mut p, 4, 4);
            p.margin_left = 4;
            p.set |= PROP_MARGIN_LEFT;
        }
        b"ul" | b"ol" => {
            margins(&mut p, 4, 4);
            p.margin_left = 4;
            p.set |= PROP_MARGIN_LEFT;
        }
        b"figure" | b"pre" | b"dl" => margins(&mut p, 4, 4),
        b"figcaption" => {
            italic(&mut p);
            center(&mut p);
        }
        b"dd" => {
            p.margin_left = 4;
            p.set |= PROP_MARGIN_LEFT;
        }
        b"center" => center(&mut p),
        _ => {}
    }
    p
}

fn heading_level(tag: &[u8]) -> u8 {
    match tag {
        b"h1" => 1,
        b"h2" => 2,
        b"h3" => 3,
        b"h4" => 4,
        b"h5" => 5,
        b"h6" => 6,
        _ => 0,
    }
}

// elements that never have content or a close tag (br, hr and img have
// their own handling in classify_tag)
fn is_void_element(tag: &[u8]) -> bool {
    matches!(
        tag,
        b"wbr"
            | b"meta"
            | b"link"
            | b"input"
            | b"col"
            | b"area"
            | b"base"
            | b"embed"
            | b"source"
            | b"track"
            | b"param"
    )
}

// a CSS length in signed quarter-em to the stream's unsigned field
fn qem_u8(v: i8) -> u8 {
    v.clamp(0, BlockProps::QEM_MAX as i8) as u8
}

fn inline_marker(bit: u8, on: bool) -> u8 {
    match (bit, on) {
        (INLINE_BOLD, true) => BOLD_ON,
        (INLINE_BOLD, false) => BOLD_OFF,
        (INLINE_ITALIC, true) => ITALIC_ON,
        (INLINE_ITALIC, false) => ITALIC_OFF,
        (INLINE_UNDERLINE, true) => UNDERLINE_ON,
        (INLINE_UNDERLINE, false) => UNDERLINE_OFF,
        (_, true) => STRIKE_ON,
        (_, false) => STRIKE_OFF,
    }
}

#[derive(Clone, Copy)]
enum SkipTag {
    Script,
    Style,
    Head,
}

impl SkipTag {
    fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"script" => Some(Self::Script),
            b"style" => Some(Self::Style),
            b"head" => Some(Self::Head),
            _ => None,
        }
    }

    fn name(&self) -> &'static [u8] {
        match self {
            Self::Script => b"script",
            Self::Style => b"style",
            Self::Head => b"head",
        }
    }
}

fn find_close_tag(data: &[u8], name: &[u8]) -> Option<usize> {
    let mut pos = 0;
    while pos + 2 < data.len() {
        if data[pos] == b'<' && data[pos + 1] == b'/' {
            let tag_start = pos + 2;
            let mut tag_pos = tag_start;
            while tag_pos < data.len() && !is_tag_delim(data[tag_pos]) {
                tag_pos += 1;
            }
            let tag_name = &data[tag_start..tag_pos];
            if tag_name.len() == name.len()
                && tag_name
                    .iter()
                    .zip(name.iter())
                    .all(|(a, b)| a.to_ascii_lowercase() == *b)
            {
                while tag_pos < data.len() && data[tag_pos] != b'>' {
                    tag_pos += 1;
                }
                return Some(tag_pos + 1);
            }
        }
        pos += 1;
    }
    None
}

// resolve entity name to Unicode codepoint; None for unrecognised
fn resolve_entity(name: &[u8]) -> Option<u32> {
    match name {
        b"amp" => Some(0x26),                 // &
        b"lt" => Some(0x3C),                  // <
        b"gt" => Some(0x3E),                  // >
        b"quot" => Some(0x22),                // "
        b"apos" => Some(0x27),                // '
        b"nbsp" => Some(0xA0),                // non-breaking space
        b"mdash" | b"emdash" => Some(0x2014), // —
        b"ndash" | b"endash" => Some(0x2013), // –
        b"lsquo" => Some(0x2018),             // '
        b"rsquo" => Some(0x2019),             // '
        b"sbquo" => Some(0x201A),             // ‚
        b"ldquo" => Some(0x201C),             // "
        b"rdquo" => Some(0x201D),             // "
        b"bdquo" => Some(0x201E),             // „
        b"hellip" => Some(0x2026),            // …
        b"bull" | b"bullet" => Some(0x2022),  // •
        b"copy" => Some(0xA9),                // ©
        b"reg" => Some(0xAE),                 // ®
        b"trade" => Some(0x2122),             // ™
        b"times" => Some(0xD7),               // ×
        b"divide" => Some(0xF7),              // ÷
        b"deg" => Some(0xB0),                 // °
        b"plusmn" => Some(0xB1),              // ±
        b"frac14" => Some(0xBC),              // ¼
        b"frac12" => Some(0xBD),              // ½
        b"frac34" => Some(0xBE),              // ¾
        b"laquo" => Some(0xAB),               // «
        b"raquo" => Some(0xBB),               // »
        b"iexcl" => Some(0xA1),               // ¡
        b"iquest" => Some(0xBF),              // ¿
        b"cent" => Some(0xA2),                // ¢
        b"pound" => Some(0xA3),               // £
        b"yen" => Some(0xA5),                 // ¥
        b"euro" => Some(0x20AC),              // €
        b"sect" => Some(0xA7),                // §
        b"para" => Some(0xB6),                // ¶
        b"middot" => Some(0xB7),              // ·
        b"micro" => Some(0xB5),               // µ
        b"szlig" => Some(0xDF),               // ß
        // Latin uppercase accented
        b"Agrave" => Some(0xC0),
        b"Aacute" => Some(0xC1),
        b"Acirc" => Some(0xC2),
        b"Atilde" => Some(0xC3),
        b"Auml" => Some(0xC4),
        b"Aring" => Some(0xC5),
        b"AElig" => Some(0xC6),
        b"Ccedil" => Some(0xC7),
        b"Egrave" => Some(0xC8),
        b"Eacute" => Some(0xC9),
        b"Ecirc" => Some(0xCA),
        b"Euml" => Some(0xCB),
        b"Igrave" => Some(0xCC),
        b"Iacute" => Some(0xCD),
        b"Icirc" => Some(0xCE),
        b"Iuml" => Some(0xCF),
        b"ETH" => Some(0xD0),
        b"Ntilde" => Some(0xD1),
        b"Ograve" => Some(0xD2),
        b"Oacute" => Some(0xD3),
        b"Ocirc" => Some(0xD4),
        b"Otilde" => Some(0xD5),
        b"Ouml" => Some(0xD6),
        b"Oslash" => Some(0xD8),
        b"Ugrave" => Some(0xD9),
        b"Uacute" => Some(0xDA),
        b"Ucirc" => Some(0xDB),
        b"Uuml" => Some(0xDC),
        b"Yacute" => Some(0xDD),
        b"THORN" => Some(0xDE),
        // Latin lowercase accented
        b"agrave" => Some(0xE0),
        b"aacute" => Some(0xE1),
        b"acirc" => Some(0xE2),
        b"atilde" => Some(0xE3),
        b"auml" => Some(0xE4),
        b"aring" => Some(0xE5),
        b"aelig" => Some(0xE6),
        b"ccedil" => Some(0xE7),
        b"egrave" => Some(0xE8),
        b"eacute" => Some(0xE9),
        b"ecirc" => Some(0xEA),
        b"euml" => Some(0xEB),
        b"igrave" => Some(0xEC),
        b"iacute" => Some(0xED),
        b"icirc" => Some(0xEE),
        b"iuml" => Some(0xEF),
        b"eth" => Some(0xF0),
        b"ntilde" => Some(0xF1),
        b"ograve" => Some(0xF2),
        b"oacute" => Some(0xF3),
        b"ocirc" => Some(0xF4),
        b"otilde" => Some(0xF5),
        b"ouml" => Some(0xF6),
        b"oslash" => Some(0xF8),
        b"ugrave" => Some(0xF9),
        b"uacute" => Some(0xFA),
        b"ucirc" => Some(0xFB),
        b"uuml" => Some(0xFC),
        b"yacute" => Some(0xFD),
        b"thorn" => Some(0xFE),
        b"yuml" => Some(0xFF),
        // numeric references
        _ => {
            if name.starts_with(b"#x") || name.starts_with(b"#X") {
                let cp = parse_hex(&name[2..]);
                if cp > 0 && cp <= 0x10FFFF {
                    Some(cp)
                } else {
                    None
                }
            } else if name.starts_with(b"#") {
                let cp = parse_decimal(&name[1..]);
                if cp > 0 && cp <= 0x10FFFF {
                    Some(cp)
                } else {
                    None
                }
            } else {
                None
            }
        }
    }
}

/// Encode a Unicode codepoint as UTF-8 into `out`.  Returns the number
/// of bytes written (1–4), or 0 for codepoint 0 / out-of-range.
fn encode_codepoint_utf8(cp: u32, out: &mut [u8; 4]) -> usize {
    if cp == 0 || cp > 0x10FFFF {
        return 0;
    }
    if cp < 0x80 {
        out[0] = cp as u8;
        1
    } else if cp < 0x800 {
        out[0] = (0xC0 | (cp >> 6)) as u8;
        out[1] = (0x80 | (cp & 0x3F)) as u8;
        2
    } else if cp < 0x10000 {
        out[0] = (0xE0 | (cp >> 12)) as u8;
        out[1] = (0x80 | ((cp >> 6) & 0x3F)) as u8;
        out[2] = (0x80 | (cp & 0x3F)) as u8;
        3
    } else {
        out[0] = (0xF0 | (cp >> 18)) as u8;
        out[1] = (0x80 | ((cp >> 12) & 0x3F)) as u8;
        out[2] = (0x80 | ((cp >> 6) & 0x3F)) as u8;
        out[3] = (0x80 | (cp & 0x3F)) as u8;
        4
    }
}

/// Return the expected byte length of a UTF-8 sequence from its lead byte.
#[inline]
fn utf8_seq_len(lead: u8) -> usize {
    if lead < 0xC0 {
        1
    } else if lead < 0xE0 {
        2
    } else if lead < 0xF0 {
        3
    } else {
        4
    }
}

// in-place entity decoding; delegates to resolve_entity for the entity table
enum DecodedInplace {
    Codepoint(u32),
    None,
}

fn decode_entity_inplace(input: &[u8], pos: usize) -> (DecodedInplace, usize) {
    debug_assert!(input[pos] == b'&');

    let remaining = &input[pos + 1..];
    let max_scan = remaining.len().min(12);
    let semi = remaining[..max_scan].iter().position(|&b| b == b';');

    let Some(semi) = semi else {
        return (DecodedInplace::None, 1);
    };

    let entity = &remaining[..semi];
    let advance = 1 + semi + 1;

    let decoded = match resolve_entity(entity) {
        Some(cp) => DecodedInplace::Codepoint(cp),
        None => DecodedInplace::None,
    };

    (decoded, advance)
}

fn parse_hex(bytes: &[u8]) -> u32 {
    let mut val = 0u32;
    for &b in bytes {
        let nibble = match b {
            b'0'..=b'9' => (b - b'0') as u32,
            b'a'..=b'f' => (b - b'a' + 10) as u32,
            b'A'..=b'F' => (b - b'A' + 10) as u32,
            _ => return 0,
        };
        val = val.wrapping_mul(16).wrapping_add(nibble);
    }
    val
}

fn parse_decimal(bytes: &[u8]) -> u32 {
    let mut val = 0u32;
    for &b in bytes {
        if b.is_ascii_digit() {
            val = val.wrapping_mul(10).wrapping_add((b - b'0') as u32);
        } else {
            return 0;
        }
    }
    val
}

#[inline]
fn is_html_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0C)
}

#[inline]
fn is_tag_delim(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
}

#[inline]
fn is_entity_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'#'
}

fn skip_to_gt(data: &[u8], mut pos: usize) -> usize {
    while pos < data.len() {
        if data[pos] == b'>' {
            return pos + 1;
        }
        pos += 1;
    }
    data.len()
}

fn skip_bang_construct(data: &[u8], pos: usize) -> usize {
    let rest = &data[pos..];

    if rest.starts_with(b"!--") {
        let mut p = pos + 3;
        while p + 2 < data.len() {
            if data[p] == b'-' && data[p + 1] == b'-' && data[p + 2] == b'>' {
                return p + 3;
            }
            p += 1;
        }
        return data.len();
    }

    if rest.starts_with(b"![CDATA[") {
        let mut p = pos + 8;
        while p + 2 < data.len() {
            if data[p] == b']' && data[p + 1] == b']' && data[p + 2] == b'>' {
                return p + 3;
            }
            p += 1;
        }
        return data.len();
    }

    skip_to_gt(data, pos)
}

fn skip_pi(data: &[u8], pos: usize) -> usize {
    let mut p = pos + 1;
    while p + 1 < data.len() {
        if data[p] == b'?' && data[p + 1] == b'>' {
            return p + 2;
        }
        p += 1;
    }
    data.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markup::{Event, Events, SliceSource, Style};
    use alloc::vec::Vec;

    // run the stripper over `html` in `chunk`-byte input slices and a
    // deliberately small output buffer, so every resume path is exercised
    fn strip_chunked(html: &[u8], css: &CssRules, chunk: usize) -> Vec<u8> {
        let mut s = HtmlStripStream::new();
        let mut out = Vec::new();
        let mut tmp = [0u8; 7];
        let mut ip = 0;
        while ip < html.len() {
            let end = (ip + chunk).min(html.len());
            let (consumed, written) = s.feed(&html[ip..end], &mut tmp, css);
            out.extend_from_slice(&tmp[..written]);
            if consumed == 0 && written == 0 {
                panic!("no progress at {ip}");
            }
            ip += consumed;
        }
        loop {
            let n = s.finish(&mut tmp);
            out.extend_from_slice(&tmp[..n]);
            if n < tmp.len() {
                break;
            }
        }
        out
    }

    fn strip(html: &str, css: &str) -> Vec<u8> {
        let mut rules = CssRules::new();
        rules.parse(css.as_bytes());
        let whole = strip_chunked(html.as_bytes(), &rules, html.len().max(1));
        let bytewise = strip_chunked(html.as_bytes(), &rules, 1);
        assert_eq!(whole, bytewise, "chunked feed must not change output");
        whole
    }

    fn events(bytes: &[u8]) -> Vec<Event> {
        let mut src = SliceSource(bytes);
        Events::new(&mut src).collect()
    }

    // (word text, style) for every Word event
    fn words(bytes: &[u8]) -> Vec<(alloc::string::String, Style)> {
        events(bytes)
            .iter()
            .filter_map(|e| match e {
                Event::Word { start, end, style } => Some((
                    alloc::string::String::from_utf8_lossy(&bytes[*start as usize..*end as usize])
                        .into_owned(),
                    *style,
                )),
                _ => None,
            })
            .collect()
    }

    fn blocks(bytes: &[u8]) -> Vec<BlockProps> {
        events(bytes)
            .iter()
            .filter_map(|e| match e {
                Event::Block { block, .. } => Some(*block),
                _ => None,
            })
            .collect()
    }

    fn text(bytes: &[u8]) -> alloc::string::String {
        let plain: Vec<u8> = {
            let mut v = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == MARKER {
                    i += if i + 1 < bytes.len() && bytes[i + 1] == BLOCK { BLOCK_LEN } else { 2 };
                    continue;
                }
                v.push(bytes[i]);
                i += 1;
            }
            v
        };
        alloc::string::String::from_utf8_lossy(&plain).into_owned()
    }

    #[test]
    fn semantic_tags_style_words() {
        let out = strip("<p>a <b>b</b> <i>c</i> <u>d</u> <s>e</s></p>", "");
        let w = words(&out);
        assert_eq!(w.len(), 5);
        assert_eq!(w[0].1, Style::PLAIN);
        assert!(w[1].1.bold);
        assert!(w[2].1.italic);
        assert!(w[3].1.underline);
        assert!(w[4].1.strike);
        assert_eq!(text(&out), "a b c d e\n");
    }

    #[test]
    fn stylesheet_class_makes_italic() {
        let out = strip(
            "<p>a <span class=\"calibre4\">b</span> c</p>",
            ".calibre4 { font-style: italic }",
        );
        let w = words(&out);
        assert!(!w[0].1.italic);
        assert!(w[1].1.italic);
        assert!(!w[2].1.italic);
    }

    #[test]
    fn multiple_classes_and_tag_class_selectors() {
        let out = strip(
            "<p class=\"body first\">a</p><p class=\"body\">b</p>",
            "p.first { text-indent: 0 } .body { text-indent: 1.5em; margin: 0 }",
        );
        let b = blocks(&out);
        // first paragraph: indent 0 (default record, so none emitted);
        // second: 1.5em = 6 qem
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text_indent_qem, 6);
    }

    #[test]
    fn nested_bold_does_not_end_early() {
        let out = strip(
            "<p><b>a <span class=\"bold\">b</span> c</b> d</p>",
            ".bold { font-weight: bold }",
        );
        let w = words(&out);
        assert!(w[0].1.bold && w[1].1.bold && w[2].1.bold);
        assert!(!w[3].1.bold);
    }

    #[test]
    fn normal_inside_italic_restores() {
        let out = strip(
            "<p><em>a <span class=\"roman\">b</span> c</em></p>",
            ".roman { font-style: normal }",
        );
        let w = words(&out);
        assert!(w[0].1.italic);
        assert!(!w[1].1.italic);
        assert!(w[2].1.italic);
    }

    #[test]
    fn unclosed_inline_ends_with_its_block() {
        let out = strip("<p><b>a</p><p>b</p>", "");
        let w = words(&out);
        assert!(w[0].1.bold);
        assert!(!w[1].1.bold);
    }

    #[test]
    fn default_paragraph_gap_is_one_em_and_css_can_remove_it() {
        let out = strip("<p>a</p><p>b</p>", "");
        let b = blocks(&out);
        assert_eq!(b.len(), 1, "only the second paragraph carries a gap");
        assert_eq!(b[0].space_above_qem, 4);
        assert_eq!(text(&out), "a\n\nb\n");

        let out = strip("<p>a</p><p>b</p>", "p { margin: 0 }");
        assert!(blocks(&out).is_empty());
    }

    #[test]
    fn margins_collapse_to_the_larger() {
        let out = strip(
            "<h2>t</h2><p>x</p>",
            "h2 { margin-bottom: 2em; text-align: left } p { margin-top: 1em }",
        );
        let b = blocks(&out);
        assert_eq!(b.len(), 2, "the heading carries its own record for the left alignment");
        assert_eq!(b[0].align, Align::Left);
        assert_eq!(b[1].space_above_qem, 8);
        assert_eq!(b[1].align, Align::Default);
    }

    #[test]
    fn text_indent_is_inherited_and_overridable() {
        let out = strip(
            "<div class=\"c\"><p class=\"n\">a</p><p>b</p></div>",
            ".c { text-indent: 1.5em } .n { text-indent: 0 } p { margin: 0 }",
        );
        let b = blocks(&out);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text_indent_qem, 6);
    }

    #[test]
    fn hanging_indent_flushes_left() {
        let out = strip("<p>a</p>", "p { text-indent: -1em; margin: 0 }");
        assert!(blocks(&out).is_empty());
    }

    #[test]
    fn blockquote_and_margin_left_indent() {
        let out = strip(
            "<blockquote><p>q</p></blockquote><p class=\"e\">e</p>",
            ".e { margin-left: 2em }",
        );
        let b = blocks(&out);
        assert_eq!(b[0].left, 1);
        assert_eq!(b[1].left, 2);
    }

    #[test]
    fn display_none_hides_content_and_markers() {
        let out = strip(
            "<p>a</p><div style=\"display:none\"><p>h <b>x</b></p><img src=\"i.jpg\"/></div><p>b</p>",
            "",
        );
        let w = words(&out);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].0, "a");
        assert_eq!(w[1].0, "b");
        assert!(!w[1].1.bold);
        assert!(!out.contains(&IMG_REF));
    }

    #[test]
    fn display_none_from_stylesheet_class() {
        let out = strip(
            "<p>a</p><p class=\"hidden\">x</p><p>b</p>",
            ".hidden { display: none }",
        );
        assert_eq!(text(&out), "a\n\nb\n");
    }

    #[test]
    fn self_closing_element_closes_at_once() {
        let out = strip(
            "<p>a<span class=\"i\"/>b <span class=\"i\" />c</p>",
            ".i { font-style: italic }",
        );
        let w = words(&out);
        assert!(w.iter().all(|(_, s)| !s.italic), "{w:?}");
        assert_eq!(text(&out), "ab c\n");
    }

    #[test]
    fn h1_page_breaks_and_centers_unless_css_says_otherwise() {
        let out = strip("<p>a</p><h1>T</h1>", "");
        let ev = events(&out);
        let pb = ev.iter().position(|e| matches!(e, Event::PageBreak { .. })).unwrap();
        let bl = ev.iter().position(|e| matches!(e, Event::Block { .. })).unwrap();
        assert!(pb < bl, "page break must precede the block record");
        let b = blocks(&out);
        assert_eq!(b[0].align, Align::Center);
        let w = words(&out);
        assert_eq!(w[1].1.heading, 1);

        let out = strip("<p>a</p><h1>T</h1>", "h1 { text-align: left }");
        assert_eq!(blocks(&out)[0].align, Align::Left);
    }

    #[test]
    fn inline_style_attribute_wins() {
        let out = strip(
            "<p class=\"l\" style=\"text-align:center; font-weight:bold\">x</p>",
            ".l { text-align: left }",
        );
        assert_eq!(blocks(&out)[0].align, Align::Center);
        assert!(words(&out)[0].1.bold);
    }

    #[test]
    fn table_cells_are_separated() {
        let out = strip("<table><tr><td>a</td><td>b</td></tr></table>", "");
        let w = words(&out);
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn hr_is_a_thematic_break() {
        let out = strip("<p>a</p><hr/><p>b</p>", "");
        let ev = events(&out);
        assert!(ev.iter().any(|e| matches!(e, Event::ThematicBreak { .. })));
        assert_eq!(text(&out), "a\n\nb\n");
    }

    #[test]
    fn br_continues_the_block() {
        let out = strip("<p style=\"text-align:right\">a<br/>b</p>", "");
        let ev = events(&out);
        assert!(ev.iter().any(|e| matches!(e, Event::HardBreak { .. })));
        assert_eq!(blocks(&out).len(), 1);
        let mut src = SliceSource(&out);
        let mut it = Events::new(&mut src);
        for e in it.by_ref() {
            if matches!(e, Event::HardBreak { .. }) {
                break;
            }
        }
        assert_eq!(it.block().align, Align::Right);
    }

    #[test]
    fn percent_indent_and_font_weight_number() {
        let out = strip(
            "<p class=\"p\"><span class=\"h\">x</span></p>",
            ".p { text-indent: 3.8%; margin: 0 } .h { font-weight: 900 }",
        );
        assert_eq!(blocks(&out)[0].text_indent_qem, 3);
        assert!(words(&out)[0].1.bold);
    }

    #[test]
    fn display_block_span_breaks_lines() {
        let out = strip("<p>a<span style=\"display:block\">b</span>c</p>", "");
        assert_eq!(text(&out), "a\n\nb\n\nc\n");
    }

    #[test]
    fn image_takes_the_prefix_and_keeps_alt_and_path() {
        let out = strip(
            "<p>a</p><figure><img src=\"x/y.jpg\" alt=\"A  cat\" width=\"10\" height=\"20\"/></figure>",
            "",
        );
        let ev = events(&out);
        let img = ev
            .iter()
            .find_map(|e| match e {
                Event::Image(i) => Some(*i),
                _ => None,
            })
            .unwrap();
        assert_eq!(img.alt(&out), b"A cat");
        assert_eq!(img.path(&out), b"x/y.jpg");
        assert_eq!(img.attr_w, 10);
        assert_eq!(img.attr_h, 20);
        assert_eq!(img.flags & IMG_FLAG_FIGURE, IMG_FLAG_FIGURE);
    }

    #[test]
    fn li_gets_a_bullet_and_list_indent() {
        let out = strip("<ul><li>a</li></ul>", "");
        assert_eq!(text(&out), "\u{2022} a\n");
        assert_eq!(blocks(&out)[0].left, 1);
    }

    #[test]
    fn entities_and_whitespace_collapse() {
        let out = strip("<p>a&amp;b   c&nbsp;d\n\te</p>", "");
        assert_eq!(text(&out), "a&b c d e\n");
    }

    #[test]
    fn stray_close_tags_are_ignored() {
        let out = strip("</b><p>a</span></p></div>", "");
        assert_eq!(text(&out), "a\n");
        assert!(!words(&out)[0].1.bold);
    }

    #[test]
    fn deep_nesting_beyond_the_stack_degrades_gracefully() {
        let mut html = alloc::string::String::new();
        for _ in 0..40 {
            html.push_str("<span>");
        }
        html.push('x');
        for _ in 0..40 {
            html.push_str("</span>");
        }
        html.push_str("<b>y</b>");
        let out = strip(&html, "");
        let w = words(&out);
        assert_eq!(w[0].0, "x");
        assert!(w[1].1.bold);
    }
}
