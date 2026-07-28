// screen: typestate refresh interface over the SSD1677 driver
//
// owns the EPD driver, the strip buffer, and all display-plane state
// (stale region, partial counter). a refresh is a linear session:
//
//   begin_partial / begin_full  ->  Wave<'_, M>   (waveform running)
//   wave.settle() / wave.wait() ->  Settled<'_, M>
//   settled.sync_red / .abandon / .grayscale / .finish
//
// Wave borrows the Screen mutably, so no other panel access can
// compile while a waveform is in flight; the scheduler exploits that
// window for SD I/O (the charge pump drives pixels with no SPI
// traffic). misordered phases are unrepresentable: phase 3 methods
// only exist on Settled, which only exists after the wave is consumed

use core::marker::PhantomData;

use embassy_time::TimeoutError;
use esp_hal::delay::Delay;

use crate::board::{Epd, SCREEN_H, SCREEN_W};
use crate::drivers::ssd1677::{HEIGHT, RenderState, WIDTH};
use crate::drivers::strip::StripBuffer;
use crate::kernel::plane_map::{AaQueue, PlaneMap, SessionOutcome};
use crate::ui::{AlignedRegion, Region};

pub struct Screen {
    epd: Epd,
    strip: &'static mut StripBuffer,
    delay: Delay,

    // which panel areas the RAM planes do not describe, and what
    // recovering each takes (re-drive, or revert-then-re-drive).
    // area absent from the map is in sync: delta DUs are safe there
    planes: PlaneMap,

    // rects driven to plain BW since their last AA pass; the deferred
    // fire pulses exactly these. only just-re-driven pixels may take
    // gray pulses: the LUT lightens pixels the BW waveform just drove
    // black, so re-pulsing an area already carrying its gray drives
    // it further off level
    pending_aa: AaQueue,

    // partial refreshes since the last full GC; the scheduler promotes
    // to a full clear once this reaches ghost_clear_every
    partials: u32,
}

/// Marker for a partial DU waveform session.
pub struct Du;
/// Marker for a full GC waveform session.
pub struct Gc;

/// A waveform in flight. Borrows the [`Screen`] until consumed, so no
/// other panel access can compile while the EPD is busy.
pub struct Wave<'s, M> {
    screen: &'s mut Screen,
    rs: RenderState,
    // logical counterpart of `rs`: the area this session drives, used
    // for stale-region bookkeeping
    region: AlignedRegion,
    hard_redrive: bool,
    _mode: PhantomData<M>,
}

/// A completed waveform awaiting its closing phase.
pub struct Settled<'s, M> {
    screen: &'s mut Screen,
    rs: RenderState,
    region: AlignedRegion,
    _mode: PhantomData<M>,
}

/// Why a partial refresh could not start.
pub enum PartialRejected {
    /// The region aligned to nothing; there is no work to do.
    Empty,
    /// The panel has never been fully refreshed; run a full GC instead.
    NeedsFull,
}

const FULL_RS: RenderState = RenderState {
    px: 0,
    py: 0,
    pw: WIDTH,
    ph: HEIGHT,
};

const FULL_REGION: AlignedRegion =
    AlignedRegion::from_aligned(Region::new(0, 0, SCREEN_W, SCREEN_H));

impl Screen {
    pub fn new(epd: Epd, strip: &'static mut StripBuffer, delay: Delay) -> Self {
        Self {
            epd,
            strip,
            delay,
            planes: PlaneMap::new(),
            pending_aa: AaQueue::new(),
            partials: 0,
        }
    }

    #[inline]
    pub fn partials_since_clear(&self) -> u32 {
        self.partials
    }

    /// Bounding box of everything a delta DU may not touch; debug view.
    #[inline]
    pub fn stale_region(&self) -> Option<Region> {
        self.planes.bbox().map(AlignedRegion::get)
    }

    /// Force the next partial request to promote to a full GC.
    #[inline]
    pub fn force_ghost_clear(&mut self) {
        self.partials = u32::MAX;
    }

    #[inline]
    pub fn needs_initial_refresh(&self) -> bool {
        self.epd.needs_initial_refresh()
    }

    #[inline]
    pub fn set_sunlight_mode(&mut self, enabled: bool) {
        self.epd.set_sunlight_mode(enabled);
    }

    pub fn enter_deep_sleep(&mut self) {
        self.epd.enter_deep_sleep();
    }

    /// Snap AA grays under `region` back to their rails before a
    /// partial re-drive. Runs one wave per gray-coded window under the
    /// region (usually one), so a small partial does not strip AA from
    /// the rest of the screen and never drives outside the tracked
    /// codes: the intersection of aligned regions is aligned, so the
    /// physical window cannot snap outward onto planes holding content,
    /// which the revert LUT would misdrive (white content reads {1,1},
    /// its drive-black state). Returns whether any wave ran. On timeout
    /// the next frame is promoted to a full GC; the caller may still
    /// proceed with its partial, since the inv_red re-drive keeps the
    /// content correct either way.
    pub async fn revert_gray_for(&mut self, region: Region) -> Result<bool, TimeoutError> {
        let region = AlignedRegion::snap(region);
        let mut ran = false;
        for w in self.planes.gray_windows(region).into_iter().flatten() {
            let w = w.get();
            let Some(rs) = self.epd.region_state(w.x, w.y, w.w, w.h) else {
                continue;
            };
            // the codes in RAM stay valid (the pass writes nothing),
            // so the map survives for the following session's closer
            if let Err(e) = self.epd.grayscale_revert_pass(&rs).await {
                self.force_ghost_clear();
                return Err(e);
            }
            ran = true;
        }
        Ok(ran)
    }

    /// Partial DU refresh, waiting inline on the busy pin. Falls back
    /// to a full GC when the panel has not been refreshed yet. For
    /// paths with no background work to overlap (wifi upload screens).
    pub async fn render_partial<F>(&mut self, region: Region, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        if self.revert_gray_for(region).await.is_err() {
            log::warn!("render_partial: revert pass timed out, forcing full GC next frame");
        }
        match self.begin_partial(region, draw) {
            Ok(wave) => {
                wave.wait().await?.sync_red(draw);
                Ok(())
            }
            Err(PartialRejected::Empty) => Ok(()),
            Err(PartialRejected::NeedsFull) => self.render_full(draw).await,
        }
    }

    /// Write BW RAM for `region` and kick the DU waveform.
    ///
    /// When the region overlaps panel area the RAM planes no longer
    /// describe (gray from an AA pass, a skipped phase 3), the write
    /// goes through inv_red so the waveform re-drives every pixel it
    /// covers from a known state instead of computing a delta against
    /// a stale plane. Recovery stays scoped to the requested region;
    /// the caller never tracks plane state.
    pub fn begin_partial<F>(
        &mut self,
        region: Region,
        draw: &F,
    ) -> Result<Wave<'_, Du>, PartialRejected>
    where
        F: Fn(&mut StripBuffer),
    {
        if self.epd.needs_initial_refresh() {
            return Err(PartialRejected::NeedsFull);
        }

        // snap on both axes: the physical window then carries no edge
        // slop, and the 0-7 extra rows repaint with real content
        // instead of the old masks parking fake white in both planes
        let r = AlignedRegion::snap(region);
        let hard_redrive = self.planes.needs_redrive(r);

        let (x, y, w, h) = {
            let r = r.get();
            (r.x, r.y, r.w, r.h)
        };
        let rs = if hard_redrive {
            self.epd
                .partial_phase1_bw_inv_red(self.strip, x, y, w, h, &mut self.delay, draw)
        } else {
            self.epd
                .partial_phase1_bw(self.strip, x, y, w, h, &mut self.delay, draw)
        }
        .ok_or(PartialRejected::Empty)?;

        self.epd.partial_start_du(&rs);
        self.partials = self.partials.saturating_add(1);
        self.pending_aa.push(r);

        Ok(Wave {
            screen: self,
            rs,
            region: r,
            hard_redrive,
            _mode: PhantomData,
        })
    }

    /// Write the full frame to both RAMs and kick the GC waveform.
    pub fn begin_full<F>(&mut self, draw: &F) -> Wave<'_, Gc>
    where
        F: Fn(&mut StripBuffer),
    {
        self.epd.write_full_frame(self.strip, &mut self.delay, draw);
        self.epd.start_full_update();
        // both planes hold content the panel does not show yet; if the
        // GC completes, finish() clears the map, and if it times out
        // the whole screen correctly stays marked for re-drive
        self.planes.apply(FULL_REGION, SessionOutcome::Abandoned);
        self.pending_aa.clear();
        self.pending_aa.push(FULL_REGION);
        Wave {
            screen: self,
            rs: FULL_RS,
            region: FULL_REGION,
            hard_redrive: true,
            _mode: PhantomData,
        }
    }

    /// Full GC refresh, waiting inline on the busy pin. For paths with
    /// no background work to overlap (boot console, sleep screens).
    pub async fn render_full<F>(&mut self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        crate::perf_begin!(_t0);
        let wave = self.begin_full(draw);
        crate::perf_event!(
            "render",
            "full_inline_write write_ms={}",
            _t0.elapsed().as_millis()
        );
        crate::perf_begin!(_t1);
        let res = wave.wait().await;
        crate::perf_event!(
            "render",
            "full_inline_wave wave_ms={}",
            _t1.elapsed().as_millis()
        );
        res?.finish();
        Ok(())
    }

    /// Full-screen grayscale AA pass. Leaves the panel holding gray
    /// levels only the codes now in RAM describe, so the whole screen
    /// is tracked as gray-coded; on timeout the next partial request
    /// additionally promotes to a full GC.
    pub async fn grayscale_full<F>(&mut self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let res = self.epd.grayscale_pass(self.strip, &FULL_RS, draw).await;
        self.planes.apply(FULL_REGION, SessionOutcome::Grayed);
        self.pending_aa.clear();
        if res.is_err() {
            self.force_ghost_clear();
        }
        res
    }

    /// Grayscale AA pass over everything driven to plain BW since the
    /// last pass. Used by the deferred fire: one pass per pending
    /// rect, never their bounding box, whose slop would re-pulse
    /// areas that already carry their gray and drive them off level.
    /// No-op when nothing was refreshed.
    pub async fn grayscale_fresh<F>(&mut self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        while let Some(region) = self.pending_aa.take_next() {
            let r = region.get();
            let Some(rs) = self.epd.region_state(r.x, r.y, r.w, r.h) else {
                continue;
            };
            let res = self.epd.grayscale_pass(self.strip, &rs, draw).await;
            // the planes were rewritten with codes before the wave
            // started, so the region is gray-coded even on timeout
            self.planes.apply(region, SessionOutcome::Grayed);
            if res.is_err() {
                self.force_ghost_clear();
                return res;
            }
        }
        Ok(())
    }
}

impl<'s, M> Wave<'s, M> {
    /// Sync GPIO read of the busy pin.
    #[inline]
    pub fn is_busy(&mut self) -> bool {
        self.screen.epd.is_busy()
    }

    /// True when this session re-drives every pixel in its region
    /// (inv_red recovery or a full GC) rather than a DU delta.
    #[inline]
    pub fn hard_redrive(&self) -> bool {
        self.hard_redrive
    }

    /// Resolves when the busy pin goes low. Unlike [`Wave::wait`] this
    /// borrows, so the scheduler can race it against input/background
    /// arms in a select and keep the session alive on the other arms.
    pub async fn until_idle(&mut self) {
        let _ = self.screen.epd.busy_pin().wait_for_low().await;
    }

    /// Consume the wave after the caller has observed busy-low (or
    /// given up via its own guard timeout).
    pub fn settle(self) -> Settled<'s, M> {
        Settled {
            screen: self.screen,
            rs: self.rs,
            region: self.region,
            _mode: PhantomData,
        }
    }

    /// Wait inline for the waveform with the driver timeout, then
    /// settle. For paths with nothing to overlap.
    pub async fn wait(self) -> Result<Settled<'s, M>, TimeoutError> {
        self.screen.epd.wait_busy_async("wave").await?;
        Ok(self.settle())
    }
}

impl Settled<'_, Du> {
    /// Phase 3: rewrite RED RAM with current content (BW already holds
    /// it from phase 1) so the next DU computes a minimal delta. Panel
    /// power stays latched for the next refresh.
    pub fn sync_red<F>(self, draw: &F)
    where
        F: Fn(&mut StripBuffer),
    {
        let s = self.screen;
        s.epd.partial_phase3_sync(s.strip, &self.rs, draw);
        // the panel matches both planes across this region again; the
        // carve is exact, so claims outside it (gray codes elsewhere,
        // an old skipped phase 3) survive as fragments instead of the
        // old whole-box invalidation that kept dropping revert
        // coverage after every sync
        s.planes.apply(self.region, SessionOutcome::Synced);
    }

    /// Skip phase 3 (content changed mid-waveform); RED RAM keeps the
    /// pre-waveform image while the panel shows the new one, so the
    /// region needs an inv_red re-drive before any delta DU.
    pub fn abandon(self) {
        self.screen
            .planes
            .apply(self.region, SessionOutcome::Abandoned);
    }

    /// Grayscale AA pass over this refresh's region instead of phase 3.
    ///
    /// The gray LUT states are short relative pulses that lighten
    /// pixels the BW frame just drove black, and `{0,0}` is literally
    /// "no change", so the pass leaves the panel holding intermediate
    /// levels only the codes now in RAM describe. The region is
    /// tracked as gray-coded: the next partial touching it reverts the
    /// grays to their rails and re-drives via inv_red, the starting
    /// state the DU transitions assume. On timeout the pass is
    /// additionally promoted to a full GC.
    pub async fn grayscale<F>(self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let s = self.screen;
        let res = s.epd.grayscale_pass(s.strip, &self.rs, draw).await;
        s.planes.apply(self.region, SessionOutcome::Grayed);
        // this region just took its gray; a later deferred fire over
        // a queued rect covering it would stack a second pulse
        s.pending_aa.subtract(self.region);
        if res.is_err() {
            s.force_ghost_clear();
        }
        res
    }
}

impl Settled<'_, Gc> {
    /// Close out a full GC: both planes are in sync and ghosting is
    /// cleared, so the partial counter and stale flag reset.
    pub fn finish(self) {
        let s = self.screen;
        s.epd.finish_full_update();
        s.partials = 0;
        s.planes.clear();
    }
}
