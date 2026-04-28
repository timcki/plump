//! Paginator: turns a chapter line table into pages of at most
//! `max_lines`, honoring forced `PAGE_BREAK` boundaries and
//! image-block fitting rules.
//!
//! Phase 4 will populate this. Sketch:
//!
//! ```ignore
//! pub fn paginate(
//!     lines: &[LineLayout],
//!     max_lines: u8,
//!     out_pages: &mut Vec<PageLayout>,
//! ) -> Result<(), PaginateError>
//! ```
//!
//! Rules:
//!
//! - emit a new page after every `max_lines` lines.
//! - honor `PAGE_BREAK` flags by forcing the next line onto a
//!   fresh page.
//! - an image block that doesn't fit on the current page is moved
//!   to the next page entirely (one image origin line + N filler
//!   lines must stay together).
//! - a single image larger than `max_lines` is emitted on its own
//!   page anyway, clipped at the bottom by the renderer.
