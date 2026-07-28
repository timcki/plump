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
// to Stale whenever exact tracking would overflow capacity

use crate::ui::{AlignedRegion, Region};

const CAP: usize = 8;

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
    pub fn apply(&mut self, r: AlignedRegion, outcome: SessionOutcome) {
        self.carve(r);
        match outcome {
            SessionOutcome::Synced => {}
            SessionOutcome::Abandoned => self.insert(r, PlaneState::Stale),
            SessionOutcome::Grayed => self.insert(r, PlaneState::GrayCodes),
        }
    }

    fn carve(&mut self, r: AlignedRegion) {
        for i in 0..CAP {
            let Some((e, state)) = self.entries[i] else {
                continue;
            };
            if !e.intersects(r) {
                continue;
            }
            self.entries[i] = None;
            for frag in subtract_aligned(e, r).into_iter().flatten() {
                self.insert(frag, state);
            }
        }
    }

    fn insert(&mut self, r: AlignedRegion, state: PlaneState) {
        // already covered by a same-state entry: nothing new to track
        for (e, s) in self.entries.iter().flatten() {
            if *s == state && e.contains(r) {
                return;
            }
        }
        if let Some(slot) = self.entries.iter_mut().find(|s| s.is_none()) {
            *slot = Some((r, state));
            return;
        }
        self.degrade_insert(r);
    }

    // capacity overflow: fold `r` into a Stale entry as an
    // over-approximation. if only gray entries exist, demote one:
    // its area falls back from revert to plain re-drive (one mottled
    // frame), which is safe; growing a gray entry never is
    fn degrade_insert(&mut self, r: AlignedRegion) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|(_, s)| *s == PlaneState::Stale)
        {
            entry.0 = entry.0.union(r);
            return;
        }
        if let Some(entry) = self.entries.iter_mut().flatten().next() {
            *entry = (entry.0.union(r), PlaneState::Stale);
        }
    }
}

/// Pending deferred-AA work: rects driven to plain BW since their
/// last gray pass. Kept as individual rects, never a bounding box:
/// pulsing area that was not just re-driven stacks a second gray
/// pulse on pixels already carrying theirs, which drifts them off
/// level a little more on every pass (the darkening mist). On
/// overflow an entry is dropped instead of widened; the cost is a
/// patch of un-antialiased text until the next full pass.
#[derive(Default)]
pub struct AaQueue {
    entries: [Option<AlignedRegion>; Self::CAP],
}

impl AaQueue {
    const CAP: usize = 6;

    pub const fn new() -> Self {
        Self {
            entries: [None; Self::CAP],
        }
    }

    pub fn clear(&mut self) {
        self.entries = [None; Self::CAP];
    }

    pub fn push(&mut self, r: AlignedRegion) {
        for e in self.entries.iter().flatten() {
            if e.contains(r) {
                return;
            }
        }
        // absorb entries the new rect covers
        for slot in self.entries.iter_mut() {
            if slot.is_some_and(|e| r.contains(e)) {
                *slot = None;
            }
        }
        if let Some(slot) = self.entries.iter_mut().find(|s| s.is_none()) {
            *slot = Some(r);
            return;
        }
        // full: sacrifice one entry's AA rather than widening any rect
        self.entries[0] = Some(r);
    }

    /// Remove `r` from the queue: a gray pass just covered it, so
    /// pulsing it again would stack.
    pub fn subtract(&mut self, r: AlignedRegion) {
        for i in 0..Self::CAP {
            let Some(e) = self.entries[i] else {
                continue;
            };
            if !e.intersects(r) {
                continue;
            }
            self.entries[i] = None;
            for frag in subtract_aligned(e, r).into_iter().flatten() {
                // overflow drops the fragment: losing AA is safe,
                // re-pulsing is not
                if let Some(slot) = self.entries.iter_mut().find(|s| s.is_none()) {
                    *slot = Some(frag);
                }
            }
        }
    }

    pub fn take_next(&mut self) -> Option<AlignedRegion> {
        self.entries.iter_mut().find_map(Option::take)
    }
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
