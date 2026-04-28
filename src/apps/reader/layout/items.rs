//! Knuth-Plass item builder: turns a paragraph's `scan::Event`s
//! into a list of boxes / glue / penalties for the breaker.
//!
//! Phase 3 will populate this. Sketch:
//!
//! ```ignore
//! pub struct Item {
//!     pub kind: ItemKind,
//!     pub width: u16,
//!     pub stretch: u16,
//!     pub shrink: u16,
//!     pub penalty: i16,
//!     pub flagged: bool,
//!     pub byte_offset: u32,
//! }
//!
//! pub enum ItemKind { Box, Glue, Penalty }
//! ```
//!
//! Mapping rules (sketch):
//!
//! - text runs become `Box` items measured via `FontSet::advance`.
//! - ASCII spaces become `Glue` with stretch ~ 1/2 space width and
//!   shrink ~ 1/3 space width.
//! - NBSP becomes a zero-stretch glue or a fixed-width box.
//! - soft hyphen becomes a `Penalty` with width 0 (or the hyphen's
//!   width when visible hyphenation is enabled).
//! - hard breaks become forced negative-infinity penalties.
