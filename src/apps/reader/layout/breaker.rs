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
    /// per-line extra stretchability (pixels) added to `line_y` when
    /// computing `r` on stretched lines. TeX's `\emergencystretch`.
    /// Pass 1 leaves this at 0; the fallback pass sets it to a fraction
    /// of `line_width` so previously-infeasible loose lines become
    /// acceptable.
    pub emergency_stretch: u16,
}

impl BreakConfig {
    pub const DEFAULT: Self = Self {
        line_width: 0, // caller fills
        // TeX's classic `\tolerance`. Pass 1 stays tight; the fallback
        // in `break_paragraph_with_fallback` widens tolerance and adds
        // emergency_stretch so pathological narrow-column paragraphs
        // still find a solution.
        tolerance: 200,
        looseness: 0,
        line_penalty: 10,
        double_hyphen_demerit: 100,
        adjacent_loose_demerit: 100,
        max_items: 2048,
        emergency_stretch: 0,
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

/// Reusable DP scratch. A chapter typeset runs the breaker once per
/// paragraph; allocating the prefix sums, node table and path per call
/// produced thousands of same-size TLSF alloc/free cycles per chapter
/// and a ~90 KB transient peak. The pipeline owns one of these for the
/// whole typeset and releases the memory in its `Drop`.
pub struct BreakScratch {
    pfx_w: Vec<u32>,
    pfx_y: Vec<u32>,
    pfx_z: Vec<u32>,
    best: Vec<Node>,
    path: Vec<u16>,
}

impl Default for BreakScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl BreakScratch {
    pub const fn new() -> Self {
        Self {
            pfx_w: Vec::new(),
            pfx_y: Vec::new(),
            pfx_z: Vec::new(),
            best: Vec::new(),
            path: Vec::new(),
        }
    }

    /// Drop all retained capacity (end of a chapter typeset).
    pub fn release(&mut self) {
        self.pfx_w = Vec::new();
        self.pfx_y = Vec::new();
        self.pfx_z = Vec::new();
        self.best = Vec::new();
        self.path = Vec::new();
    }

    // prefix sums of width / stretch / shrink across items.
    // pfx_w[k] = sum of widths of items[..k]; pfx_w[0] = 0.
    fn build_prefixes(&mut self, items: &[Item]) -> Result<(), BreakError> {
        let n = items.len();
        self.pfx_w.clear();
        self.pfx_y.clear();
        self.pfx_z.clear();
        if self.pfx_w.try_reserve_exact(n + 1).is_err()
            || self.pfx_y.try_reserve_exact(n + 1).is_err()
            || self.pfx_z.try_reserve_exact(n + 1).is_err()
        {
            return Err(BreakError::ItemBudgetExceeded);
        }
        self.pfx_w.push(0);
        self.pfx_y.push(0);
        self.pfx_z.push(0);
        for it in items {
            let (w, y, z) = match it.kind() {
                ItemKind::Box => (it.width as u32, 0, 0),
                ItemKind::Glue => (it.width as u32, it.stretch as u32, it.shrink as u32),
                ItemKind::Penalty => (0, 0, 0),
            };
            self.pfx_w.push(self.pfx_w.last().unwrap() + w);
            self.pfx_y.push(self.pfx_y.last().unwrap() + y);
            self.pfx_z.push(self.pfx_z.last().unwrap() + z);
        }
        Ok(())
    }

    fn prepare_dp(&mut self, n: usize) -> Result<(), BreakError> {
        self.best.clear();
        if self.best.try_reserve_exact(n).is_err() {
            return Err(BreakError::ItemBudgetExceeded);
        }
        self.best.resize(n, Node::UNREACHABLE_NODE);
        self.path.clear();
        // path length is bounded by the number of lines <= n
        if self.path.try_reserve_exact(n).is_err() {
            return Err(BreakError::ItemBudgetExceeded);
        }
        Ok(())
    }
}

/// Run the K-P paragraph breaker.
///
/// On success, `out` is appended with one `BreakChoice` per emitted
/// line in order; the last choice's `flags` carries `LAST_LINE`.
/// `out` is not cleared by the function — the caller does that.
pub fn break_paragraph(
    items: &[Item],
    cfg: &BreakConfig,
    scratch: &mut BreakScratch,
    out: &mut Vec<BreakChoice>,
) -> Result<(), BreakError> {
    break_paragraph_inner(items, cfg, scratch, out, false, true)
}

/// Test oracle: identical to `break_paragraph` but with the DP window
/// disabled (every predecessor scanned, original order). Property tests
/// assert windowed == full-scan on representative paragraphs.
#[cfg(test)]
pub(crate) fn break_paragraph_full_scan(
    items: &[Item],
    cfg: &BreakConfig,
    scratch: &mut BreakScratch,
    out: &mut Vec<BreakChoice>,
) -> Result<(), BreakError> {
    break_paragraph_inner(items, cfg, scratch, out, false, false)
}

fn break_paragraph_inner(
    items: &[Item],
    cfg: &BreakConfig,
    scratch: &mut BreakScratch,
    out: &mut Vec<BreakChoice>,
    reuse_prefixes: bool,
    windowed: bool,
) -> Result<(), BreakError> {
    if items.is_empty() {
        return Err(BreakError::EmptyParagraph);
    }
    if items.len() > cfg.max_items as usize {
        return Err(BreakError::ItemBudgetExceeded);
    }

    let n = items.len();

    // the fallback pass re-runs with the same items, so its prefix
    // sums are still valid and only the DP state needs a reset
    if !reuse_prefixes {
        scratch.build_prefixes(items)?;
    }
    debug_assert_eq!(scratch.pfx_w.len(), n + 1);
    scratch.prepare_dp(n)?;

    let BreakScratch {
        pfx_w,
        pfx_y,
        pfx_z,
        best,
        path,
    } = scratch;

    let line_width = cfg.line_width as i32;

    // effective span of the line (a, i] is pfx[i] - pfx[a+1] for every
    // trailing-item case (trailing glue is subtracted, a trailing
    // penalty contributes zero to the prefixes), so "can fit after
    // maximal shrink" is s(i) - s(a+1) <= line_width with
    // s(k) = pfx_w[k] - pfx_z[k]. per-item w >= z (boxes have z = 0,
    // glue shrink <= its width, penalties are 0/0), so s is
    // nondecreasing and the window start only ever moves forward.
    let s = |k: usize| pfx_w[k] - pfx_z[k];

    // First pass: forward DP.
    let mut win_lo: usize = 0;
    for i in 0..n {
        if !is_feasible_breakpoint(items, i) {
            continue;
        }

        if windowed {
            while win_lo < i && s(i) - s(win_lo + 1) > line_width as u32 {
                win_lo += 1;
            }
        }

        let mut chosen: Option<Node> = None;

        // Try line beginning at start (no prior break).
        consider_break(items, pfx_w, pfx_y, pfx_z, None, i, line_width, cfg, best, &mut chosen);

        // Predecessors inside the feasible window; anything earlier
        // spans wider than line_width even after maximal shrink.
        let lo = if windowed { win_lo } else { 0 };
        for a in lo..i {
            if best[a].demerits == Node::UNREACHABLE {
                continue;
            }
            consider_break(items, pfx_w, pfx_y, pfx_z, Some(a), i, line_width, cfg, best, &mut chosen);
        }

        // escape hatch, REQUIRED for behavior parity: this breaker
        // accepts overfull spans (Overflow with BADNESS_INFINITY,
        // single-overfull-box URLs, forced clamps), and those
        // predecessors lie outside the window by construction. when
        // the window produced nothing, run the legacy full scan so
        // degenerate paragraphs break exactly as before.
        if windowed && chosen.is_none() {
            for a in 0..win_lo {
                if best[a].demerits == Node::UNREACHABLE {
                    continue;
                }
                consider_break(items, pfx_w, pfx_y, pfx_z, Some(a), i, line_width, cfg, best, &mut chosen);
            }
        }

        best[i] = chosen.unwrap_or(Node::UNREACHABLE_NODE);
    }

    // Pick terminal break: prefer the last forced-break Penalty in `items`
    // that we successfully reached. Falls back to the last reachable break.
    let terminal = pick_terminal(items, best);

    let Some(mut cur) = terminal else {
        return Err(BreakError::NoFeasibleBreaks);
    };

    // Walk back to reconstruct the break path.
    loop {
        path.push(cur as u16);
        let prev = best[cur].prev;
        if prev == Node::NO_PREV {
            break;
        }
        cur = prev as usize;
    }
    path.reverse();

    // Emit BreakChoices for each line, marking the last one.
    let mut had_overfull = false;
    if out.try_reserve_exact(path.len()).is_err() {
        return Err(BreakError::ItemBudgetExceeded);
    }
    for (idx, &i16idx) in path.iter().enumerate() {
        let i = i16idx as usize;
        let node = &best[i];
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
        if node.adjustment_q8 == BreakChoice::ADJUSTMENT_OVERFLOW {
            had_overfull = true;
        }
        out.push(BreakChoice {
            item_idx: i as u16,
            line_width_used: node.width_used,
            stretch_total: node.stretch_total,
            shrink_total: node.shrink_total,
            adjustment_ratio_q8: node.adjustment_q8,
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

/// Two-pass paragraph breaker.
///
/// Pass 1 runs with the caller's `cfg` (default `tolerance = 200`,
/// `emergency_stretch = 0`) — TeX-classical tight justification.
///
/// Pass 2 (fallback) runs with `tolerance = 10000`, `line_penalty = 0`,
/// and `emergency_stretch = line_width / 5` (SILE's 20%lw default).
/// The extra per-line stretch budget makes previously-infeasible loose
/// lines acceptable, so this pass always finds a solution short of
/// `NoFeasibleBreaks` on degenerate input.
///
/// Trigger for pass 2: pass 1 errored with `NoFeasibleBreaks`, or it
/// produced a single line whose natural width exceeds `cfg.line_width`.
/// That single-line case has two encodings depending on whether the
/// paragraph has any shrinkable glue: `Adjustment::Overflow` when
/// `line_z == 0` (no shrink at all) and `Adjustment::Shrink(256)` when
/// `line_z > 0` but the forced terminal was clamped to r=-1.0. Both mean
/// the same thing — every interior break was rejected and only the
/// forced terminal landed — and both need pass 2 to find a multi-line
/// solution. Comparing `line_width_used` to `cfg.line_width` catches
/// both encodings in one check.
///
/// Once hyphenation lands, an intermediate pass (TeX `\tolerance` with
/// discretionary hyphenation breakpoints) slots in between these two.
pub fn break_paragraph_with_fallback(
    items: &[Item],
    cfg: &BreakConfig,
    scratch: &mut BreakScratch,
    out: &mut Vec<BreakChoice>,
) -> Result<(), BreakError> {
    let retry = match break_paragraph_inner(items, cfg, scratch, out, false, true) {
        Err(BreakError::NoFeasibleBreaks) => true,
        Err(other) => return Err(other),
        Ok(()) => out.len() == 1 && out[0].line_width_used > cfg.line_width,
    };
    if !retry {
        return Ok(());
    }
    out.clear();
    let fallback = BreakConfig {
        tolerance: 10_000,
        line_penalty: 0,
        emergency_stretch: cfg.line_width / 5,
        ..*cfg
    };
    // items are unchanged between passes, so the prefix sums are
    // reused; only the DP state resets
    break_paragraph_inner(items, &fallback, scratch, out, true, true)
}

// ── internals ─────────────────────────────────────────────────────

/// 16 bytes; `best` holds `max_items` of these, so the old 32-byte
/// `Option<Node>` layout cost 64 KB of scratch at the item budget.
/// Metrics are stored saturated at u16 exactly as the emit path
/// already clamped them; demerits math runs on full-precision locals
/// in `consider_break` before anything is stored.
#[derive(Clone, Copy, Debug)]
struct Node {
    /// total demerits to reach this breakpoint; `UNREACHABLE` marks a
    /// slot with no feasible path (real values cap at `DEMERITS_CAP`)
    demerits: i32,
    /// chosen line metrics (the line that ENDS at this breakpoint)
    width_used: u16,
    stretch_total: u16,
    shrink_total: u16,
    /// signed Q8, clamped to i16; i16::MIN is the Overflow sentinel
    /// (same encoding as `BreakChoice::ADJUSTMENT_OVERFLOW`)
    adjustment_q8: i16,
    /// prior breakpoint index; `NO_PREV` = start of paragraph
    prev: u16,
    fitness: FitnessClass,
}

impl Node {
    const UNREACHABLE: i32 = i32::MAX;
    const NO_PREV: u16 = u16::MAX;
    const UNREACHABLE_NODE: Node = Node {
        demerits: Self::UNREACHABLE,
        width_used: 0,
        stretch_total: 0,
        shrink_total: 0,
        adjustment_q8: 0,
        prev: Self::NO_PREV,
        fitness: FitnessClass::Normal,
    };
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
    best: &[Node],
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

    // Trailing Glue at the breakpoint is collapsed (zero-width). A
    // Penalty broken at adds its pre-break width: the hyphen glyph of a
    // discretionary, nothing for a plain penalty.
    match items[i].kind() {
        ItemKind::Glue => {
            line_w -= items[i].width as u32;
            line_y -= items[i].stretch as u32;
            line_z -= items[i].shrink as u32;
        }
        ItemKind::Penalty => line_w += items[i].width as u32,
        ItemKind::Box => {}
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
        // emergency_stretch (per-line budget, pixels) is added to the
        // line's natural stretchability. lets pass 2 of the fallback
        // accept lines that pass 1 would reject as too loose.
        let budget = line_y as i32 + cfg.emergency_stretch as i32;
        if budget == 0 {
            // unstretchable underfull line; only feasible if forced
            if !forced {
                return;
            }
            // last line: ragged-right is fine, treat as r=0
            0
        } else {
            (slack * Q8_ONE) / budget
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

    // Fitness-adjacency demerit (only when both this and previous break
    // exist; callers only pass reachable predecessors)
    if let Some(prev_idx) = a {
        let prev_node = &best[prev_idx];
        if prev_node.demerits != Node::UNREACHABLE {
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

    let prev_dem = a.map(|p| best[p].demerits).unwrap_or(0);
    let total_dem = prev_dem.saturating_add(dem).min(DEMERITS_CAP);

    let candidate = Node {
        demerits: total_dem,
        prev: a.map(|p| p as u16).unwrap_or(Node::NO_PREV),
        // saturate rather than the emit path's old truncation; only
        // observable for lines wider than 65535 px, where saturation
        // also keeps the fallback's natural-width trigger correct
        width_used: line_w.min(u16::MAX as u32) as u16,
        stretch_total: line_y.min(u16::MAX as u32) as u16,
        shrink_total: line_z.min(u16::MAX as u32) as u16,
        // same clamp the emit path applied; i16::MIN only arrives via
        // the Overflow sentinel (real shrink ratios stop at -256)
        adjustment_q8: r_q8.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
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
fn pick_terminal(items: &[Item], best: &[Node]) -> Option<usize> {
    let mut last_forced: Option<usize> = None;
    let mut last_any: Option<usize> = None;
    for i in 0..items.len() {
        if best[i].demerits != Node::UNREACHABLE {
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
    // clamp |r| to 8.0 before cubing: badness(2048) = 51200 already
    // saturates the 10_000 cap, and unclamped loose ratios (tiny
    // stretch budgets produce r in the tens of thousands) overflowed
    // the i32 cube, wrapping to garbage badness in release builds
    let r = (r_q8.unsigned_abs() as i32).min(8 * Q8_ONE);
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
        assert_eq!(break_paragraph(&[], &cfg(100), &mut BreakScratch::new(), &mut out), Err(BreakError::EmptyParagraph));
    }

    #[test]
    fn single_word_fits_in_one_line() {
        let mut items = Vec::new();
        items.push(boxed(20));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut BreakScratch::new(), &mut out).unwrap();
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
        break_paragraph(&items, &cfg(100), &mut BreakScratch::new(), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line_width_used, 25);
    }

    #[test]
    fn long_paragraph_breaks_into_multiple_lines() {
        // stretchy glue keeps interior breaks inside pass-1 tolerance
        // (with stretch 2 every break had badness > 200 and the forced
        // single-line collapse won on demerits; this test never ran
        // before the host harness existed and encoded that wrong)
        let mut items = Vec::new();
        for _ in 0..20 {
            items.push(boxed(10));
            items.push(glue(5, 12, 1));
        }
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(50), &mut BreakScratch::new(), &mut out).unwrap();
        assert!(out.len() >= 4, "expected ≥4 lines, got {}", out.len());
        assert!(out.last().unwrap().flags.contains(ChoiceFlags::LAST_LINE));
    }

    #[test]
    fn unbreakable_box_wider_than_line_emits_overflow() {
        let mut items = Vec::new();
        items.push(boxed(200)); // wider than line_width=100
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph_with_fallback(&items, &cfg(100), &mut BreakScratch::new(), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].adjustment(), Adjustment::Overflow);
    }

    #[test]
    fn mid_forced_penalty_reaches_terminal() {
        // production only emits forced penalties at the end of an item
        // stream (every push_forced_break_triple is followed by
        // `return finish`), so the breaker has no forced-break barrier:
        // a cheap line spanning a mid-stream forced penalty can win on
        // demerits. this test documents that; if mid-stream forced
        // breaks ever become reachable, a barrier must be added and
        // this becomes a two-line assertion
        let mut items = Vec::new();
        items.push(boxed(10));
        items.push(Item::penalty(Item::PENALTY_FORCE, false, true, 0));
        items.push(boxed(10));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut BreakScratch::new(), &mut out).unwrap();
        assert!(!out.is_empty());
        let last = out.last().unwrap();
        assert!(last.flags.contains(ChoiceFlags::LAST_LINE));
        assert!(last.flags.contains(ChoiceFlags::FORCED_BREAK));
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
        assert_eq!(break_paragraph(&items, &cfg, &mut BreakScratch::new(), &mut out), Err(BreakError::ItemBudgetExceeded));
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
        let _ = break_paragraph_with_fallback(&items, &cfg(42), &mut BreakScratch::new(), &mut out);
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
        break_paragraph(&items, &strict, &mut BreakScratch::new(), &mut primary_out).unwrap();
        assert_eq!(primary_out.len(), 1, "primary should collapse to one line");
        assert_eq!(primary_out[0].adjustment(), Adjustment::Overflow);

        // Fallback wrapper: detects single-Overflow and retries loose.
        let mut out = Vec::new();
        break_paragraph_with_fallback(&items, &strict, &mut BreakScratch::new(), &mut out).unwrap();
        assert!(out.len() > 1, "fallback should emit >1 line, got {}", out.len());
    }

    #[test]
    fn last_line_carries_last_line_flag() {
        let mut items = Vec::new();
        items.push(boxed(10));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut BreakScratch::new(), &mut out).unwrap();
        assert!(out.last().unwrap().flags.contains(ChoiceFlags::LAST_LINE));
    }

    #[test]
    fn adjustment_accessor_returns_typed_variant() {
        let mut items = Vec::new();
        items.push(boxed(50));
        items.extend(forced_triple());
        let mut out = Vec::new();
        break_paragraph(&items, &cfg(100), &mut BreakScratch::new(), &mut out).unwrap();
        // Last line is the forced break with no glue: adjustment is
        // either Perfect (0) or Overflow if box is too wide. With width
        // 50 ≤ 100, it's Perfect.
        assert_eq!(out[0].adjustment(), Adjustment::Perfect);
    }

    #[test]
    fn emergency_stretch_helps_loose_line() {
        // 10× (box=10, glue=2 with tiny stretch=1) at line_width=40.
        // Pass 1 (tolerance=200, emergency_stretch=0): every interior
        // break has line_y too small to absorb the slack (badness
        // saturates at 10000); only the forced terminal break remains,
        // and its natural width exceeds line_width so it emits a single
        // Overflow line.
        let mut items = Vec::new();
        for _ in 0..10 {
            items.push(boxed(10));
            items.push(glue(2, 1, 0));
        }
        items.extend(forced_triple());

        // Pass 1 alone collapses to single Overflow.
        let mut pass1 = Vec::new();
        break_paragraph(&items, &cfg(40), &mut BreakScratch::new(), &mut pass1).unwrap();
        assert_eq!(pass1.len(), 1);
        assert_eq!(pass1[0].adjustment(), Adjustment::Overflow);

        // Pass-2 fallback adds emergency_stretch = line_width/5 = 8;
        // interior breaks become acceptable and the paragraph emits
        // multiple lines.
        let mut out = Vec::new();
        break_paragraph_with_fallback(&items, &cfg(40), &mut BreakScratch::new(), &mut out).unwrap();
        assert!(out.len() > 1, "expected multi-line, got {}", out.len());
    }

    #[test]
    fn emergency_stretch_zero_is_no_op() {
        // For a paragraph that fits comfortably at tolerance=200,
        // explicitly setting emergency_stretch=0 must produce identical
        // output to the default (also 0). Guards against accidentally
        // mixing the field into the stretch budget when it's zero.
        let mut items = Vec::new();
        for _ in 0..5 {
            items.push(boxed(10));
            items.push(glue(5, 3, 1));
        }
        items.extend(forced_triple());

        let cfg_default = cfg(60);
        let cfg_explicit_zero = BreakConfig {
            emergency_stretch: 0,
            ..cfg(60)
        };

        let mut a = Vec::new();
        let mut b = Vec::new();
        break_paragraph(&items, &cfg_default, &mut BreakScratch::new(), &mut a).unwrap();
        break_paragraph(&items, &cfg_explicit_zero, &mut BreakScratch::new(), &mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn fallback_fires_on_forced_clamp_shrink() {
        // 5× (box=20, glue=2 with stretch=1, shrink=1) at line_width=40.
        // Natural width 108 > 40. Interior breaks all reject under
        // tolerance=200. Forced terminal: line_z > 0 so encoded as
        // Adjustment::Shrink(256) (forced clamp), NOT Overflow. This is
        // the exact shape of the on-device bug fixed by the natural-
        // width trigger.
        let mut items = Vec::new();
        for _ in 0..5 {
            items.push(boxed(20));
            items.push(glue(2, 1, 1));
        }
        items.extend(forced_triple());

        // Pass 1 alone: single line, Shrink (NOT Overflow), wider than column.
        let mut pass1 = Vec::new();
        break_paragraph(&items, &cfg(40), &mut BreakScratch::new(), &mut pass1).unwrap();
        assert_eq!(pass1.len(), 1);
        assert!(matches!(pass1[0].adjustment(), Adjustment::Shrink(_)));
        assert!(pass1[0].line_width_used > 40);

        // with_fallback must fire pass 2 on natural-width overflow and
        // produce multi-line output.
        let mut out = Vec::new();
        break_paragraph_with_fallback(&items, &cfg(40), &mut BreakScratch::new(), &mut out).unwrap();
        assert!(out.len() > 1, "expected multi-line, got {}", out.len());
    }

    // deterministic LCG so property tests need no rand dependency
    fn lcg(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        *state >> 16
    }

    #[test]
    fn windowed_matches_full_scan_on_prose() {
        // pseudo-random prose-like paragraphs: the DP window must be a
        // pure optimization for every feasible input
        let mut seed = 0xC0FFEE;
        for case in 0..40 {
            let words = 5 + (lcg(&mut seed) % 60) as usize;
            let mut items = Vec::new();
            for _ in 0..words {
                items.push(boxed(8 + (lcg(&mut seed) % 32) as u16));
                items.push(glue(
                    4 + (lcg(&mut seed) % 4) as u16,
                    2 + (lcg(&mut seed) % 3) as u16,
                    1 + (lcg(&mut seed) % 2) as u8,
                ));
            }
            items.extend(forced_triple());
            let line_width = 60 + (lcg(&mut seed) % 300) as u16;

            let mut windowed = Vec::new();
            let mut full = Vec::new();
            let r1 = break_paragraph(&items, &cfg(line_width), &mut BreakScratch::new(), &mut windowed);
            let r2 = break_paragraph_full_scan(&items, &cfg(line_width), &mut BreakScratch::new(), &mut full);
            assert_eq!(r1, r2, "case {} result mismatch", case);
            assert_eq!(windowed, full, "case {} (lw={}) output mismatch", case, line_width);
        }
    }

    #[test]
    fn windowed_matches_full_scan_giant_url_mid_paragraph() {
        // a single unbreakable box wider than the line, surrounded by
        // normal prose: predecessors of the span containing it lie
        // outside the DP window, so this exercises the escape hatch.
        // assert exact equality with the full scan under both the
        // pass-1 config and the fallback config
        let mut items = Vec::new();
        for _ in 0..6 {
            items.push(boxed(20));
            items.push(glue(5, 12, 1));
        }
        items.push(boxed(400)); // the URL
        items.push(glue(5, 12, 1));
        for _ in 0..6 {
            items.push(boxed(20));
            items.push(glue(5, 12, 1));
        }
        items.extend(forced_triple());

        let configs = [
            cfg(100),
            BreakConfig {
                tolerance: 10_000,
                line_penalty: 0,
                emergency_stretch: 100 / 5,
                ..cfg(100)
            },
        ];
        for (ci, c) in configs.iter().enumerate() {
            let mut windowed = Vec::new();
            let mut full = Vec::new();
            let r1 = break_paragraph(&items, c, &mut BreakScratch::new(), &mut windowed);
            let r2 = break_paragraph_full_scan(&items, c, &mut BreakScratch::new(), &mut full);
            assert_eq!(r1, r2, "cfg {} result mismatch", ci);
            assert_eq!(windowed, full, "cfg {} output mismatch", ci);
        }
    }

    #[test]
    fn scratch_reuse_is_equivalent_to_fresh() {
        // running two different paragraphs through ONE scratch must
        // give the same output as fresh scratches (stale-state guard)
        let mut items_a = Vec::new();
        for _ in 0..10 {
            items_a.push(boxed(12));
            items_a.push(glue(5, 3, 1));
        }
        items_a.extend(forced_triple());
        let mut items_b = Vec::new();
        for _ in 0..4 {
            items_b.push(boxed(30));
            items_b.push(glue(6, 2, 2));
        }
        items_b.extend(forced_triple());

        let mut shared = BreakScratch::new();
        let mut out_a1 = Vec::new();
        let mut out_b1 = Vec::new();
        break_paragraph(&items_a, &cfg(70), &mut shared, &mut out_a1).unwrap();
        break_paragraph(&items_b, &cfg(70), &mut shared, &mut out_b1).unwrap();

        let mut out_a2 = Vec::new();
        let mut out_b2 = Vec::new();
        break_paragraph(&items_a, &cfg(70), &mut BreakScratch::new(), &mut out_a2).unwrap();
        break_paragraph(&items_b, &cfg(70), &mut BreakScratch::new(), &mut out_b2).unwrap();

        assert_eq!(out_a1, out_a2);
        assert_eq!(out_b1, out_b2);
    }

    #[test]
    fn pass_one_used_when_feasible() {
        // A paragraph that fits at pass 1 (tolerance=200) must produce
        // output byte-identical to a direct break_paragraph call with
        // the same cfg — proving the fallback didn't fire and re-do the
        // work with line_penalty=0 + emergency_stretch>0.
        let mut items = Vec::new();
        for _ in 0..6 {
            items.push(boxed(10));
            items.push(glue(5, 12, 1));
        }
        items.extend(forced_triple());

        let mut direct = Vec::new();
        break_paragraph(&items, &cfg(50), &mut BreakScratch::new(), &mut direct).unwrap();
        assert!(direct.len() > 1, "test paragraph should span multiple lines");

        let mut via_fallback = Vec::new();
        break_paragraph_with_fallback(&items, &cfg(50), &mut BreakScratch::new(), &mut via_fallback).unwrap();
        assert_eq!(direct, via_fallback);
    }
}
