//! Knuth-Plass paragraph breaker.
//!
//! Phase 3 will populate this. Pure function with no I/O so it can
//! be host-tested:
//!
//! ```ignore
//! pub fn break_paragraph(
//!     items: &[Item],
//!     line_width: u16,
//!     cfg: &BreakConfig,
//!     out: &mut Vec<BreakChoice>,
//! ) -> Result<(), BreakError>
//! ```
//!
//! Implementation strategy:
//!
//! - bounded dynamic programming over legal breakpoints, with
//!   cumulative width / stretch / shrink prefix sums.
//! - fixed-point integer math for adjustment ratios and badness.
//! - demerits + fitness class + previous breakpoint per node.
//! - reject overfull lines unless the line contains an
//!   unbreakable word that exceeds the width.
//! - return an explicit `BreakError` so callers can fall back to
//!   greedy without panicking.
//!
//! If profiling shows a problem this can be replaced internally
//! with a TeX-style active-list implementation without changing
//! the scanner or cache interfaces.
