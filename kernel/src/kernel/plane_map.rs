// plane-state map: which panel areas the RAM planes do not describe,
// and what recovering each one takes
//
// the map replaces three bounding boxes (stale / fresh / gray) whose
// pairwise invariants lived in comments and in call-site discipline.
// entries are pairwise disjoint; area absent from the map is Content:
// planes match the panel and a delta DU is safe there
//
// precision degrades in the safe direction only. Stale may
// over-approximate (the cost is an extra re-drive); GrayCodes must
// never claim area whose planes hold content, because the revert LUT
// reads white content as {1,1}, its drive-black state, so gray demotes
// to Stale whenever exact tracking would overflow capacity. demotion
// is reported to the caller: the demoted area's panel gray would be
// re-driven open-loop (fog), so the caller promotes to a full GC

use crate::ui::{AlignedRegion, Region};

// carving a scroll bbox out of a full-screen gray claim already costs
// four fragments; sized so exact coalescing (not the lossy degrade
// path) is what normally reclaims slots
const CAP: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneState {
    /// Planes hold content the panel does not show (skipped phase 3,
    /// or a full refresh in flight): re-drive via inv_red, no delta.
    Stale,
    /// Planes hold the codes an AA pass wrote and the panel holds the
    /// matching grays: revert to the rails before any re-drive.
    GrayCodes,
}

/// How a refresh session left the planes across its region.
#[derive(Clone, Copy)]
pub enum SessionOutcome {
    /// Phase 3 ran: planes match the panel again.
    Synced,
    /// Phase 3 skipped: planes hold content the panel does not show.
    Abandoned,
    /// A gray pass replaced phase 3: planes hold AA codes.
    Grayed,
}

#[derive(Default)]
pub struct PlaneMap {
    entries: [Option<(AlignedRegion, PlaneState)>; CAP],
}

impl PlaneMap {
    pub const fn new() -> Self {
        Self {
            entries: [None; CAP],
        }
    }

    pub fn clear(&mut self) {
        self.entries = [None; CAP];
    }

    /// Any entry under `r` means a delta DU there would compute from
    /// planes the panel does not show.
    pub fn needs_redrive(&self, r: AlignedRegion) -> bool {
        self.entries.iter().flatten().any(|(e, _)| e.intersects(r))
    }

    /// Any gray codes anywhere. A windowed waveform scans the whole
    /// panel, so codes outside its window matter as much as those
    /// under it.
    pub fn has_gray(&self) -> bool {
        self.entries
            .iter()
            .flatten()
            .any(|(_, s)| *s == PlaneState::GrayCodes)
    }

    /// Gray-coded windows under `r`, each a valid revert target.
    pub fn gray_windows(&self, r: AlignedRegion) -> [Option<AlignedRegion>; CAP] {
        let mut out = [None; CAP];
        let mut n = 0;
        for (e, state) in self.entries.iter().flatten() {
            if *state == PlaneState::GrayCodes
                && let Some(w) = e.intersection(r)
            {
                out[n] = Some(w);
                n += 1;
            }
        }
        out
    }

    /// Debug view: bounding box of everything tracked.
    pub fn bbox(&self) -> Option<AlignedRegion> {
        self.entries
            .iter()
            .flatten()
            .map(|(e, _)| *e)
            .reduce(AlignedRegion::union)
    }

    /// Record what a refresh session did to the planes across `r`.
    /// Carving is exact: area the session rewrote stops being claimed
    /// by older entries, and their remainder survives as fragments
    /// (the old bounding boxes could only drop the whole claim, which
    /// is what kept losing revert coverage after every sync).
    ///
    /// Returns true when gray coverage was lost to capacity. The
    /// untracked panel gray would be re-driven open-loop on the next
    /// partial and accumulate as fog, so the caller should promote
    /// the next frame to a full GC instead.
    pub fn apply(&mut self, r: AlignedRegion, outcome: SessionOutcome) -> bool {
        let mut gray_lost = self.carve(r);
        gray_lost |= match outcome {
            SessionOutcome::Synced => false,
            SessionOutcome::Abandoned => self.insert(r, PlaneState::Stale),
            SessionOutcome::Grayed => self.insert(r, PlaneState::GrayCodes),
        };
        gray_lost
    }

    fn carve(&mut self, r: AlignedRegion) -> bool {
        let mut gray_lost = false;
        for i in 0..CAP {
            let Some((e, state)) = self.entries[i] else {
                continue;
            };
            if !e.intersects(r) {
                continue;
            }
            self.entries[i] = None;
            for frag in subtract_aligned(e, r).into_iter().flatten() {
                gray_lost |= self.insert(frag, state);
            }
        }
        gray_lost
    }

    fn insert(&mut self, r: AlignedRegion, state: PlaneState) -> bool {
        // already covered by a same-state entry: nothing new to track
        for (e, s) in self.entries.iter().flatten() {
            if *s == state && e.contains(r) {
                return false;
            }
        }
        // fold exactly-adjacent same-state entries into `r` first:
        // carving churns out band fragments whose unions are exact,
        // so coalescing (not the lossy degrade path) reclaims slots
        let mut r = r;
        loop {
            let mut merged = false;
            for slot in self.entries.iter_mut() {
                let Some((e, s)) = *slot else {
                    continue;
                };
                if s == state && let Some(u) = merge_exact(e, r) {
                    *slot = None;
                    r = u;
                    merged = true;
                }
            }
            if !merged {
                break;
            }
        }
        if let Some(slot) = self.entries.iter_mut().find(|s| s.is_none()) {
            *slot = Some((r, state));
            return false;
        }
        self.degrade_insert(r, state)
    }

    // capacity overflow: fold `r` into a Stale entry as an
    // over-approximation. if only gray entries exist, demote one:
    // its area falls back from revert to plain re-drive, which keeps
    // content correct; growing a gray entry never is safe. returns
    // whether any gray coverage was lost
    fn degrade_insert(&mut self, r: AlignedRegion, state: PlaneState) -> bool {
        let gray_lost = state == PlaneState::GrayCodes;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|(_, s)| *s == PlaneState::Stale)
        {
            entry.0 = entry.0.union(r);
            return gray_lost;
        }
        if let Some(entry) = self.entries.iter_mut().flatten().next() {
            *entry = (entry.0.union(r), PlaneState::Stale);
            return true;
        }
        gray_lost
    }
}

// union of two rects when it introduces no new area: identical span
// on one axis, touching or overlapping on the other
fn merge_exact(a: AlignedRegion, b: AlignedRegion) -> Option<AlignedRegion> {
    let (ar, br) = (a.get(), b.get());
    let spans_touch = |a0: u16, al: u16, b0: u16, bl: u16| a0 <= b0 + bl && b0 <= a0 + al;
    if ar.x == br.x && ar.w == br.w && spans_touch(ar.y, ar.h, br.y, br.h) {
        return Some(a.union(b));
    }
    if ar.y == br.y && ar.h == br.h && spans_touch(ar.x, ar.w, br.x, br.w) {
        return Some(a.union(b));
    }
    None
}

/// `a` minus `b` as up to four disjoint bands. Aligned inputs yield
/// aligned outputs: every edge is one of the inputs' edges.
fn subtract_aligned(a: AlignedRegion, b: AlignedRegion) -> [Option<AlignedRegion>; 4] {
    let Some(o) = a.intersection(b) else {
        return [Some(a), None, None, None];
    };
    let (a, o) = (a.get(), o.get());
    let mut out = [None; 4];
    let mut n = 0;
    let mut push = |x: u16, y: u16, w: u16, h: u16| {
        if w > 0 && h > 0 {
            out[n] = Some(AlignedRegion::from_aligned(Region::new(x, y, w, h)));
            n += 1;
        }
    };
    // full-width bands above and below the overlap, side bands beside it
    push(a.x, a.y, a.w, o.y - a.y);
    push(a.x, o.y + o.h, a.w, (a.y + a.h) - (o.y + o.h));
    push(a.x, o.y, o.x - a.x, o.h);
    push(o.x + o.w, o.y, (a.x + a.w) - (o.x + o.w), o.h);
    out
}
