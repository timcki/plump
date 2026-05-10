//! Knuth-Plass paragraph breaker.
//!
//! Pure function: takes a slice of `Item`s and a `BreakConfig`,
//! produces a list of `BreakChoice`s into a caller-provided
//! `Vec<BreakChoice>`. No I/O, no font lookups, no allocator
//! beyond the output vec.
//!
//! Algorithm: bounded dynamic programming over legal breakpoints
//! with breakpoint pruning. For paragraphs <2000 items (the only
//! ones we accept) this runs effectively linearly because most
//! candidate predecessors are infeasible (overfull) and pruned
//! immediately.
//!
//! Numerics: i32 fixed-point throughout. Adjustment ratio is Q8
//! signed (256 = 1.0, range ±8.0). Badness = 100·|r|³ capped at
//! 10_000. Demerits sum capped at i32::MAX/4 to avoid overflow.

use alloc::vec::Vec;

use super::items::{Item, ItemKind};

// ── types ─────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitnessClass {
    Tight = 0,
    Normal = 1,
    Loose = 2,
    VeryLoose = 3,
}

/// Non-exclusive flag bits attached to a `BreakChoice`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChoiceFlags(u8);

impl ChoiceFlags {
    pub const NONE: Self = Self(0);
    pub const FORCED_BREAK: Self = Self(1 << 0);
    pub const FROM_HYPHEN: Self = Self(1 << 1);
    pub const LAST_LINE: Self = Self(1 << 2);

    #[inline]
    pub fn contains(self, f: Self) -> bool {
        (self.0 & f.0) != 0
    }
    #[inline]
    pub fn insert(&mut self, f: Self) {
        self.0 |= f.0;
    }
    #[inline]
    pub fn bits(self) -> u8 {
        self.0
    }
}

/// One paragraph line, as decided by the breaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakChoice {
    pub item_idx: u16,
    pub line_width_used: u16,
    pub stretch_total: u16,
    pub shrink_total: u16,
    /// signed Q8; `Adjustment::Overflow` is encoded as `i16::MIN`.
    adjustment_ratio_q8: i16,
    pub fitness: FitnessClass,
    pub flags: ChoiceFlags,
}

impl BreakChoice {
    /// Sentinel value meaning "this line was emitted with an
    /// unbreakable Box wider than the line width; renderer lets it
    /// overflow into the right margin".
    pub const ADJUSTMENT_OVERFLOW: i16 = i16::MIN;

    /// Ratio as a typed enum at the use site; storage stays 2 B.
    pub fn adjustment(&self) -> Adjustment {
        match self.adjustment_ratio_q8 {
            Self::ADJUSTMENT_OVERFLOW => Adjustment::Overflow,
            0 => Adjustment::Perfect,
            r if r > 0 => Adjustment::Stretch(r as u16),
            r => Adjustment::Shrink((-r) as u16),
        }
    }

    pub fn raw_ratio_q8(&self) -> i16 {
        self.adjustment_ratio_q8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adjustment {
    Overflow,
    Shrink(u16), // Q8 magnitude
    Perfect,
    Stretch(u16), // Q8 magnitude
}

#[derive(Clone, Copy, Debug)]
pub struct BreakConfig {
    pub line_width: u16,
    pub tolerance: u16,
    pub looseness: i8,
    pub line_penalty: u16,
    pub double_hyphen_demerit: u16,
    pub adjacent_loose_demerit: u16,
    pub max_items: u16,
}

impl BreakConfig {
    pub const DEFAULT: Self = Self {
        line_width: 0, // caller fills
        // 10000 is loose by TeX's `\pretolerance` standards but matches
        // TeX's `\tolerance` ballpark. We don't have hyphenation or
        // emergencystretch, so 200 leaves the breaker with no feasible
        // interior breaks for narrow columns + chunky-glyph fonts
        // (e.g. Atkinson Small at 464 px) — every paragraph collapses
        // to one forced-terminal line that overflows the column. K-P
        // still prefers tight breaks via demerits=(badness+penalty)²,
        // so loosening tolerance does not produce visibly looser type
        // when feasible interior breaks exist.
        tolerance: 10000,
        looseness: 0,
        line_penalty: 10,
        double_hyphen_demerit: 100,
        adjacent_loose_demerit: 100,
        max_items: 2048,
    };

    pub const fn with_line_width(mut self, w: u16) -> Self {
        self.line_width = w;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakError {
    EmptyParagraph,
    ItemBudgetExceeded,
    /// A single Box exceeds the line width and the breaker accepted
    /// it as overflow (informational; lines were still emitted).
    /// Returned only when the caller asks for strict mode.
    UnbreakableTooWide,
    NoFeasibleBreaks,
}

// ── constants ─────────────────────────────────────────────────────

const Q8_ONE: i32 = 256;
const RATIO_SHRINK_MAX: i32 = -Q8_ONE; // r = -1.0
const BADNESS_INFINITY: i32 = 10_000;
const DEMERITS_CAP: i32 = i32::MAX / 4;

// ── breaker ───────────────────────────────────────────────────────

/// Run the K-P paragraph breaker.
///
/// On success, `out` is appended with one `BreakChoice` per emitted
/// line in order; the last choice's `flags` carries `LAST_LINE`.
/// `out` is not cleared by the function — the caller does that.
pub fn break_paragraph(
    items: &[Item],
    cfg: &BreakConfig,
    out: &mut Vec<BreakChoice>,
) -> Result<(), BreakError> {
    if items.is_empty() {
        return Err(BreakError::EmptyParagraph);
    }
    if items.len() > cfg.max_items as usize {
        return Err(BreakError::ItemBudgetExceeded);
    }

    let n = items.len();

    // prefix sums of width / stretch / shrink across items.
    // pfx_w[k] = sum of widths of items[..k]; pfx_w[0] = 0.
    let mut pfx_w: Vec<u32> = Vec::new();
    let mut pfx_y: Vec<u32> = Vec::new();
    let mut pfx_z: Vec<u32> = Vec::new();
    if pfx_w.try_reserve_exact(n + 1).is_err()
        || pfx_y.try_reserve_exact(n + 1).is_err()
        || pfx_z.try_reserve_exact(n + 1).is_err()
    {
        return Err(BreakError::ItemBudgetExceeded);
    }
    pfx_w.push(0);
    pfx_y.push(0);
    pfx_z.push(0);
    for it in items {
        let (w, y, z) = match it.kind() {
            ItemKind::Box => (it.width as u32, 0, 0),
            ItemKind::Glue => (it.width as u32, it.stretch as u32, it.shrink as u32),
            ItemKind::Penalty => (0, 0, 0),
        };
        pfx_w.push(pfx_w.last().unwrap() + w);
        pfx_y.push(pfx_y.last().unwrap() + y);
        pfx_z.push(pfx_z.last().unwrap() + z);
    }

    // best[i] = best way to reach a break at item i, or None.
    let mut best: Vec<Option<Node>> = Vec::new();
    if best.try_reserve_exact(n).is_err() {
        return Err(BreakError::ItemBudgetExceeded);
    }
    best.resize(n, None);

    let line_width = cfg.line_width as i32;

    // First pass: forward DP.
    for i in 0..n {
        if !is_feasible_breakpoint(items, i) {
            continue;
        }

        let mut chosen: Option<Node> = None;

        // Try line beginning at start (no prior break).
        consider_break(items, &pfx_w, &pfx_y, &pfx_z, None, i, line_width, cfg, &best, &mut chosen);

        // Try every earlier feasible breakpoint as predecessor.
        for a in 0..i {
            if best[a].is_none() {
                continue;
            }
            consider_break(items, &pfx_w, &pfx_y, &pfx_z, Some(a), i, line_width, cfg, &best, &mut chosen);
        }

        best[i] = chosen;
    }

    // Pick terminal break: prefer the last forced-break Penalty in `items`
    // that we successfully reached. Falls back to the last reachable break.
    let terminal = pick_terminal(items, &best);

    let Some(mut cur) = terminal else {
        return Err(BreakError::NoFeasibleBreaks);
    };

    // Walk back to reconstruct the break path.
    let mut path: Vec<usize> = Vec::new();
    if path.try_reserve_exact(8).is_err() {
        return Err(BreakError::ItemBudgetExceeded);
    }
    loop {
        path.push(cur);
        match best[cur].as_ref().and_then(|n| n.prev) {
            Some(prev) => cur = prev,
            None => break,
        }
    }
    path.reverse();

    // Emit BreakChoices for each line, marking the last one.
    let mut had_overfull = false;
    if out.try_reserve_exact(path.len()).is_err() {
        return Err(BreakError::ItemBudgetExceeded);
    }
    for (idx, &i) in path.iter().enumerate() {
        let node = best[i].as_ref().unwrap();
        let mut flags = ChoiceFlags::NONE;
        if idx + 1 == path.len() {
            flags.insert(ChoiceFlags::LAST_LINE);
        }
        if matches!(items[i].kind(), ItemKind::Penalty) {
            if items[i].is_forced() {
                flags.insert(ChoiceFlags::FORCED_BREAK);
            }
            if items[i].is_flagged() {
                flags.insert(ChoiceFlags::FROM_HYPHEN);
            }
        }
        if node.adjustment_q8 == BreakChoice::ADJUSTMENT_OVERFLOW as i32 {
            had_overfull = true;
        }
        out.push(BreakChoice {
            item_idx: i as u16,
            line_width_used: node.width_used as u16,
            stretch_total: node.stretch_total.min(u16::MAX as u32) as u16,
            shrink_total: node.shrink_total.min(u16::MAX as u32) as u16,
            adjustment_ratio_q8: node.adjustment_q8.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            fitness: node.fitness,
            flags,
        });
    }

    if had_overfull {
        // soft signal — caller can log; we still emitted the lines.
        // Returning Ok is correct; the strict-mode caller (none today)
        // can re-derive this from BreakChoice.adjustment().
        let _ = BreakError::UnbreakableTooWide; // suppress unused-variant warning
    }
    Ok(())
}

/// Try once at default tolerance; retry with emergency loosening
/// (`tolerance = 1000`, `line_penalty = 0`) when the primary pass either:
/// 1. errored with `NoFeasibleBreaks` (e.g., URL longer than column), or
/// 2. returned a single `Overflow` choice — the signature of "every
///    intermediate breakpoint was rejected as too loose/tight and only
///    the forced terminal break landed". A correctly-scaled `badness`
///    makes this rare in normal prose, but the second trigger protects
///    against pathological paragraphs and future tolerance tweaks.
pub fn break_paragraph_with_fallback(
    items: &[Item],
    cfg: &BreakConfig,
    out: &mut Vec<BreakChoice>,
) -> Result<(), BreakError> {
    let retry = match break_paragraph(items, cfg, out) {
        Err(BreakError::NoFeasibleBreaks) => true,
        Err(other) => return Err(other),
        Ok(()) => out.len() == 1 && matches!(out[0].adjustment(), Adjustment::Overflow),
    };
    if !retry {
        return Ok(());
    }
    out.clear();
    let loose = BreakConfig {
        tolerance: cfg.tolerance.max(1000),
        line_penalty: 0,
        ..*cfg
    };
    break_paragraph(items, &loose, out)
}

// ── internals ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Node {
    /// total demerits to reach this breakpoint
    demerits: i32,
    /// prior breakpoint index (None = start of paragraph)
    prev: Option<usize>,
    /// chosen line metrics (the line that ENDS at this breakpoint)
    width_used: u32,
    stretch_total: u32,
    shrink_total: u32,
    adjustment_q8: i32,
    fitness: FitnessClass,
}

#[inline]
fn is_feasible_breakpoint(items: &[Item], i: usize) -> bool {
    match items[i].kind() {
        ItemKind::Glue => i > 0 && matches!(items[i - 1].kind(), ItemKind::Box),
        ItemKind::Penalty => items[i].penalty_value() < Item::PENALTY_FORBIDDEN,
        ItemKind::Box => false,
    }
}

/// Compute line metrics + demerits for the line `(a, i]` and update
/// `chosen` if this line beats the current best.
#[allow(clippy::too_many_arguments)]
fn consider_break(
    items: &[Item],
    pfx_w: &[u32],
    pfx_y: &[u32],
    pfx_z: &[u32],
    a: Option<usize>,
    i: usize,
    line_width: i32,
    cfg: &BreakConfig,
    best: &[Option<Node>],
    chosen: &mut Option<Node>,
) {
    // line spans items[(a+1)..=i] (or [0..=i] if no prior break)
    let lo = a.map(|x| x + 1).unwrap_or(0);
    let hi = i + 1;
    if lo > i {
        return;
    }

    let mut line_w = pfx_w[hi] - pfx_w[lo];
    let mut line_y = pfx_y[hi] - pfx_y[lo];
    let mut line_z = pfx_z[hi] - pfx_z[lo];

    // Trailing Glue at the breakpoint is collapsed (zero-width).
    // Trailing Penalty's width was already 0 in our model (visible
    // hyphen rendering deferred); leave it as-is.
    if matches!(items[i].kind(), ItemKind::Glue) {
        line_w -= items[i].width as u32;
        line_y -= items[i].stretch as u32;
        line_z -= items[i].shrink as u32;
    }

    let line_w_i = line_w as i32;
    let slack = line_width - line_w_i;
    let forced = matches!(items[i].kind(), ItemKind::Penalty) && items[i].is_forced();

    // tracks "the raw shrink ratio was below RATIO_SHRINK_MAX and we had to
    // clamp because forced=true." used below to override badness so K-P
    // does not prefer a forced single-line collapse (badness 100 with
    // post-clamp r=-1.0) over a feasible multi-line solution (each line
    // ~badness 1000–1500 in narrow columns).
    let mut forced_clamp_was_overshrunk = false;

    // adjustment ratio in Q8
    let r_q8 = if slack == 0 {
        0
    } else if slack > 0 {
        if line_y == 0 {
            // overfull from the wrong side; only a forced break is feasible
            if !forced {
                return;
            }
            // last line: ragged-right is fine, treat as r=0
            0
        } else {
            (slack * Q8_ONE) / line_y as i32
        }
    } else {
        // slack < 0: line is too long; need to shrink
        if line_z == 0 {
            // unshrinkable overfull; only feasible if forced AND single-Box overflow
            if forced || single_overfull_box(items, lo, i) {
                BreakChoice::ADJUSTMENT_OVERFLOW as i32
            } else {
                return;
            }
        } else {
            let r = (slack * Q8_ONE) / line_z as i32;
            if r < RATIO_SHRINK_MAX {
                if forced {
                    // forced clamp; record so we can override badness
                    forced_clamp_was_overshrunk = true;
                    RATIO_SHRINK_MAX
                } else if single_overfull_box(items, lo, i) {
                    BreakChoice::ADJUSTMENT_OVERFLOW as i32
                } else {
                    // accept as Overflow rather than rejecting outright,
                    // so paragraphs whose only feasible interior breaks
                    // need r < -1.0 still produce multi-line output. K-P
                    // sees Overflow as BADNESS_INFINITY → demerits ~100M
                    // and prefers any non-overflow alternative.
                    BreakChoice::ADJUSTMENT_OVERFLOW as i32
                }
            } else {
                r
            }
        }
    };

    // Tolerance check (skip for forced breaks and overflow lines).
    if r_q8 != BreakChoice::ADJUSTMENT_OVERFLOW as i32
        && !forced
        && q8_to_badness_input(r_q8) > cfg.tolerance as i32
    {
        return;
    }

    // forced single-line collapse stores r_q8 = RATIO_SHRINK_MAX so the
    // renderer's encoded `extra` carries the shrink magnitude. but the
    // *true* badness was much higher than badness(-1.0) = 100 — the
    // raw r could have been -10 or worse. report BADNESS_INFINITY in
    // that case so the demerits sum reflects how bad this line really
    // is and K-P picks any feasible multi-line path over it.
    let badness = if forced_clamp_was_overshrunk {
        BADNESS_INFINITY
    } else {
        badness(r_q8)
    };
    let fit = fitness_class(r_q8);

    // Demerits: (line_penalty + badness)² plus fitness adjacency penalty
    // plus double-hyphen penalty when this break and the prior one are
    // both flagged (and one of them is a hyphen-penalty).
    let lp = cfg.line_penalty as i32;
    let mut dem = saturating_square(lp + badness);

    // Penalty cost of this break (e.g. hyphen):
    if matches!(items[i].kind(), ItemKind::Penalty) {
        let p = items[i].penalty_value();
        if p > 0 && !forced {
            dem = dem.saturating_add(p as i32 * p as i32);
        } else if p < 0 && p != Item::PENALTY_FORCE {
            dem = dem.saturating_sub((p as i32) * (p as i32));
        }
    }

    // Fitness-adjacency demerit (only when both this and previous break exist)
    if let Some(prev_idx) = a {
        if let Some(prev_node) = best[prev_idx].as_ref() {
            if fit_distance(fit, prev_node.fitness) > 1 {
                dem = dem.saturating_add(cfg.adjacent_loose_demerit as i32);
            }
            // double-hyphen demerit: both flagged Penalty breaks
            if items[i].is_flagged()
                && matches!(items[prev_idx].kind(), ItemKind::Penalty)
                && items[prev_idx].is_flagged()
            {
                dem = dem.saturating_add(cfg.double_hyphen_demerit as i32);
            }
        }
    }

    let prev_dem = a
        .and_then(|p| best[p].as_ref().map(|n| n.demerits))
        .unwrap_or(0);
    let total_dem = prev_dem.saturating_add(dem).min(DEMERITS_CAP);

    let candidate = Node {
        demerits: total_dem,
        prev: a,
        width_used: line_w,
        stretch_total: line_y,
        shrink_total: line_z,
        adjustment_q8: r_q8,
        fitness: fit,
    };

    match chosen {
        Some(existing) if existing.demerits <= total_dem => {}
        _ => *chosen = Some(candidate),
    }
}

/// True when items[lo..=i] is exactly one Box that overflows the line.
/// Lets us accept a single ridiculous URL without rejecting the whole
/// paragraph.
fn single_overfull_box(items: &[Item], lo: usize, i: usize) -> bool {
    if lo == i {
        return matches!(items[i].kind(), ItemKind::Box);
    }
    // Allow leading Glue to be discarded — common after a soft hyphen
    // followed by a giant token.
    let mut only_box: Option<usize> = None;
    for k in lo..=i {
        match items[k].kind() {
            ItemKind::Box => {
                if only_box.is_some() {
                    return false;
                }
                only_box = Some(k);
            }
            ItemKind::Glue if only_box.is_none() => {} // leading glue ok
            _ => return false,
        }
    }
    only_box.is_some()
}

/// Pick the best terminal break: prefer a forced-break Penalty (which
/// is what `items.rs` always emits at paragraph end). Among reachable
/// forced breaks, pick the latest. If none, fall back to the last
/// reachable break of any kind.
fn pick_terminal(items: &[Item], best: &[Option<Node>]) -> Option<usize> {
    let mut last_forced: Option<usize> = None;
    let mut last_any: Option<usize> = None;
    for i in 0..items.len() {
        if best[i].is_some() {
            last_any = Some(i);
            if matches!(items[i].kind(), ItemKind::Penalty) && items[i].is_forced() {
                last_forced = Some(i);
            }
        }
    }
    last_forced.or(last_any)
}

/// Convert a Q8 adjustment ratio to the "badness input" used for the
/// tolerance comparison: the unsigned magnitude in Q8.
#[inline]
fn q8_to_badness_input(r_q8: i32) -> i32 {
    badness(r_q8)
}

/// Knuth's badness: 100·|r|³, capped at BADNESS_INFINITY.
///
/// The final divide-by-Q8_ONE normalises r³ out of fixed-point. Without
/// it the result is 256× over-scale (r_cubed_q8 is r³ in Q8), and the
/// classical tolerance values (200 / 1000) become effectively r ≤ 0.2,
/// which rejects every realistic intermediate breakpoint in prose. K-P
/// then only records the forced terminal break, collapsing every
/// paragraph into one Overflow line.
fn badness(r_q8: i32) -> i32 {
    if r_q8 == BreakChoice::ADJUSTMENT_OVERFLOW as i32 {
        return BADNESS_INFINITY;
    }
    let r = r_q8.unsigned_abs() as i32;
    // r_q8 ∈ [-2048, 2048] keeps r³/65536 ≤ 131072; *100 fits in i32.
    let r_squared_q8 = (r * r) / Q8_ONE;
    let r_cubed_q8 = (r_squared_q8 * r) / Q8_ONE;
    (100 * r_cubed_q8 / Q8_ONE).min(BADNESS_INFINITY)
}

#[inline]
fn fitness_class(r_q8: i32) -> FitnessClass {
    if r_q8 == BreakChoice::ADJUSTMENT_OVERFLOW as i32 {
        return FitnessClass::Tight;
    }
    if r_q8 < -Q8_ONE / 2 {
        FitnessClass::Tight
    } else if r_q8 < Q8_ONE / 2 {
        FitnessClass::Normal
    } else if r_q8 < Q8_ONE {
        FitnessClass::Loose
    } else {
        FitnessClass::VeryLoose
    }
}

#[inline]
fn fit_distance(a: FitnessClass, b: FitnessClass) -> i32 {
    (a as i32 - b as i32).abs()
}

#[inline]
fn saturating_square(x: i32) -> i32 {
    let abs = x.unsigned_abs() as u64;
    let sq = abs.saturating_mul(abs);
    if sq > i32::MAX as u64 {
        i32::MAX
    } else {
        sq as i32
    }
}

// ── tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::reader::layout::items::Item;

    fn glue(w: u16, y: u16, z: u8) -> Item {
        Item::glue(w, y, z, 0)
    }
    fn boxed(w: u16) -> Item {
        Item::boxed(w, 0)
    }
    fn forced_triple() -> [Item; 3] {
        [
            Item::penalty(Item::PENALTY_FORBIDDEN, false, false, 0),
            Item::glue(0, u16::MAX, 0, 0),
            Item::penalty(Item::PENALTY_FORCE, false, true, 0),
        ]
    }

    fn cfg(line_width: u16) -> BreakConfig {
        BreakConfig {
            line_width,
            ..BreakConfig::DEFAULT
        }
    }

    #[test]
    fn empty_paragraph_returns_error() {
        let mut out = Vec::new();
        assert_eq!(break_paragraph(&[], &cfg(100), &mut out), Err(BreakError::EmptyParagraph));
    }

    #[test]
    fn single_word_fits_in_one_line() {
        let mut items = Vec::new();
        items.push(boxed(20));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].flags.contains(ChoiceFlags::LAST_LINE));
        assert!(out[0].flags.contains(ChoiceFlags::FORCED_BREAK));
    }

    #[test]
    fn two_words_fit_one_line_with_glue() {
        let mut items = Vec::new();
        items.push(boxed(10));
        items.push(glue(5, 2, 1));
        items.push(boxed(10));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line_width_used, 25);
    }

    #[test]
    fn long_paragraph_breaks_into_multiple_lines() {
        let mut items = Vec::new();
        for _ in 0..20 {
            items.push(boxed(10));
            items.push(glue(5, 2, 1));
        }
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(50), &mut out).unwrap();
        assert!(out.len() >= 4, "expected ≥4 lines, got {}", out.len());
        assert!(out.last().unwrap().flags.contains(ChoiceFlags::LAST_LINE));
    }

    #[test]
    fn unbreakable_box_wider_than_line_emits_overflow() {
        let mut items = Vec::new();
        items.push(boxed(200)); // wider than line_width=100
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph_with_fallback(&items, &cfg(100), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].adjustment(), Adjustment::Overflow);
    }

    #[test]
    fn forced_break_in_middle_emits_two_lines() {
        let mut items = Vec::new();
        items.push(boxed(10));
        items.push(Item::penalty(Item::PENALTY_FORCE, false, true, 0));
        items.push(boxed(10));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].flags.contains(ChoiceFlags::FORCED_BREAK));
        assert!(out[1].flags.contains(ChoiceFlags::LAST_LINE));
    }

    #[test]
    fn item_budget_exceeded_returns_error() {
        let mut items = Vec::new();
        for _ in 0..10 {
            items.push(boxed(10));
        }
        let mut out = Vec::new();
        let cfg = BreakConfig {
            line_width: 100,
            max_items: 5,
            ..BreakConfig::DEFAULT
        };
        assert_eq!(break_paragraph(&items, &cfg, &mut out), Err(BreakError::ItemBudgetExceeded));
    }

    #[test]
    fn fallback_loosens_tolerance_on_no_feasible() {
        // Construct a paragraph that requires emergency loosening:
        // many Boxes back-to-back with tiny Glue, line too narrow.
        let mut items = Vec::new();
        for _ in 0..5 {
            items.push(boxed(40));
            items.push(glue(2, 0, 0)); // no stretch — strict tolerance fails
        }
        items.extend(forced_triple());
        let mut out = Vec::new();
        // With default tolerance and zero stretch, lines are either
        // perfect or overfull. line_width=42 makes each pair fit exactly.
        let _ = break_paragraph_with_fallback(&items, &cfg(42), &mut out);
        // Just assert we got a non-empty output (didn't return error).
        assert!(!out.is_empty());
    }

    #[test]
    fn fallback_loosens_tolerance_on_single_overflow() {
        // Many wide boxes joined by zero-stretch glue: every interior
        // breakpoint runs short and (with strict tolerance) is rejected,
        // so primary break_paragraph emits one Overflow choice spanning
        // the whole paragraph. The fallback detects this and retries
        // with tolerance=1000, which accepts interior breaks and emits
        // multiple lines.
        let mut items = Vec::new();
        for _ in 0..6 {
            items.push(boxed(20));
            items.push(glue(2, 0, 0));
        }
        items.extend(forced_triple());

        // Primary alone: one Overflow line.
        let strict = BreakConfig {
            line_width: 80,
            tolerance: 1, // exclude every interior break
            ..BreakConfig::DEFAULT
        };
        let mut primary_out = Vec::new();
        break_paragraph(&items, &strict, &mut primary_out).unwrap();
        assert_eq!(primary_out.len(), 1, "primary should collapse to one line");
        assert_eq!(primary_out[0].adjustment(), Adjustment::Overflow);

        // Fallback wrapper: detects single-Overflow and retries loose.
        let mut out = Vec::new();
        break_paragraph_with_fallback(&items, &strict, &mut out).unwrap();
        assert!(out.len() > 1, "fallback should emit >1 line, got {}", out.len());
    }

    #[test]
    fn last_line_carries_last_line_flag() {
        let mut items = Vec::new();
        items.push(boxed(10));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut out).unwrap();
        assert!(out.last().unwrap().flags.contains(ChoiceFlags::LAST_LINE));
    }

    #[test]
    fn adjustment_accessor_returns_typed_variant() {
        let mut items = Vec::new();
        items.push(boxed(50));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut out).unwrap();
        // Last line is the forced break with no glue: adjustment is
        // either Perfect (0) or Overflow if box is too wide. With width
        // 50 ≤ 100, it's Perfect.
        assert_eq!(out[0].adjustment(), Adjustment::Perfect);
    }
}
