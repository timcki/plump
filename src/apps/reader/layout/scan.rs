//! Styled byte stream scanner for the layout pass.
//!
//! Phase 2 will populate this. The scanner walks a smol-epub
//! chapter byte stream and emits `Event`s tagged by byte offset
//! and current style state, so the breaker can build Knuth-Plass
//! item lists a paragraph at a time without re-parsing markers.
//!
//! Shape (subject to refinement when Phase 2 lands):
//!
//! ```ignore
//! pub enum Event {
//!     Char { start: u32, end: u32, ch: char, style: StyleState },
//!     Space { start: u32, end: u32, style: StyleState },
//!     Nbsp { start: u32, end: u32, style: StyleState },
//!     SoftHyphen { start: u32, end: u32, style: StyleState },
//!     Newline { start: u32, paragraph: bool },
//!     Marker { start: u32, tag: u8 },
//!     Image { ... full IMG_REF payload ... },
//!     PageBreak { start: u32 },
//!     ThematicBreak { start: u32 },
//! }
//! ```
//!
//! Implementation notes for the next phase:
//!
//! - honor `BOLD_ON/OFF`, `ITALIC_ON/OFF`, `H1_ON/OFF` through
//!   `H6_ON/OFF`, `HEADING_ON/OFF`, `UNDERLINE_ON/OFF`,
//!   `STRIKE_ON/OFF`, `QUOTE_ON/OFF`, `ALIGN_*`, `PAGE_BREAK`,
//!   `BREAK`, `FIGCAPTION_ON/OFF`, `IMG_REF`.
//! - parse the full `IMG_REF` header (`IMG_HEADER_LEN` = 9) plus
//!   the `alt_len` and `path_len` payload.
//! - unknown markers consume two bytes and are otherwise ignored.
//! - `\n\n` ends a paragraph; a single `\n` is a hard line break.
//! - NBSP (U+00A0) renders as a fixed-width non-breaking space.
//! - soft hyphen (U+00AD) is a discretionary break opportunity.
