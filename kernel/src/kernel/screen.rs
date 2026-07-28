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
use crate::ui::Region;

pub struct Screen {
    epd: Epd,
    strip: &'static mut StripBuffer,
    delay: Delay,

    // bounding box of the area whose panel content is not represented
    // by the RAM planes: intermediate gray left by an AA pass, or the
    // pre-waveform content left by a skipped phase 3. a DU delta
    // against those planes would compute from an image the panel is
    // not showing, so a partial overlapping this box re-drives its own
    // region via inv_red instead
    stale: Option<Region>,

    // bounding box of the area driven to plain BW since the last AA
    // pass. only this area needs (and may take) gray pulses: the gray
    // LUT lightens pixels the BW waveform just drove black, so pulsing
    // an area that already carries its gray drives it further off
    fresh: Option<Region>,

    // area whose RAM planes still hold the gray codes an AA pass
    // wrote (a subset of `stale`). those codes index the revert LUT,
    // so a partial overlapping this box first snaps the grays back to
    // their rails; re-driving straight over intermediate gray levels
    // gave every DU transition a starting state its waveform does not
    // expect, which is what broke AA on page turns. any plane write
    // that does not end in a gray pass invalidates the box
    gray: Option<Region>,

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
    region: Region,
    hard_redrive: bool,
    _mode: PhantomData<M>,
}

/// A completed waveform awaiting its closing phase.
pub struct Settled<'s, M> {
    screen: &'s mut Screen,
    rs: RenderState,
    region: Region,
    hard_redrive: bool,
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
    left_mask: 0,
    right_mask: 0,
};

const FULL_REGION: Region = Region::new(0, 0, SCREEN_W, SCREEN_H);

impl Screen {
    pub fn new(epd: Epd, strip: &'static mut StripBuffer, delay: Delay) -> Self {
        Self {
            epd,
            strip,
            delay,
            stale: None,
            fresh: None,
            gray: None,
            partials: 0,
        }
    }

    #[inline]
    pub fn partials_since_clear(&self) -> u32 {
        self.partials
    }

    #[inline]
    pub fn stale_region(&self) -> Option<Region> {
        self.stale
    }

    // grow the stale box to cover `r`
    fn mark_stale(&mut self, r: Region) {
        self.stale = Some(match self.stale {
            Some(s) => s.union(r),
            None => r,
        });
    }

    // an inv_red pass over `r` re-drove every pixel it covers and
    // rewrote both planes there, so the box clears once `r` swallows
    // it. a partial overlap leaves the box alone: it is a bounding
    // box, not a pixel set, and shrinking it by guesswork would strand
    // gray outside the next delta DU
    fn clear_stale_within(&mut self, r: Region) {
        if self.stale.is_some_and(|s| r.contains(s)) {
            self.stale = None;
        }
    }

    // a gray pass over `r` left codes in the planes there. keep the
    // old box only when it strictly contains `r` (this session's
    // phase 1 wrote content planes inside `r` alone, so codes outside
    // survive); a bounding-box union could cover never-grayed area
    // whose content planes would misindex the revert LUT
    fn set_gray_region(&mut self, r: Region) {
        self.gray = Some(match self.gray {
            Some(g) if g.contains(r) => g,
            _ => r,
        });
    }

    // a plane write over `r` that did not end in a gray pass replaced
    // codes with content; the box can no longer be vouched for. the
    // areas outside `r` merely lose revert coverage: they stay inside
    // `stale`, so correctness falls back to the inv_red re-drive
    fn invalidate_gray_within(&mut self, r: Region) {
        if self.gray.is_some_and(|g| g.intersects(r)) {
            self.gray = None;
        }
    }

    fn mark_fresh(&mut self, r: Region) {
        self.fresh = Some(match self.fresh {
            Some(f) => f.union(r),
            None => r,
        });
    }

    fn clear_fresh_within(&mut self, r: Region) {
        if self.fresh.is_some_and(|f| r.contains(f)) {
            self.fresh = None;
        }
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
    /// partial re-drive. No-op unless the region overlaps the
    /// gray-coded box; the wave runs only over the overlap, so a small
    /// partial does not strip AA from the rest of the screen. Returns
    /// whether a wave ran. On timeout the next frame is promoted to a
    /// full GC; the caller may still proceed with its partial, since
    /// the inv_red re-drive keeps the content correct either way.
    pub async fn revert_gray_for(&mut self, region: Region) -> Result<bool, TimeoutError> {
        let Some(g) = self.gray else {
            return Ok(false);
        };
        // both boxes are 8-aligned, so the overlap needs no edge
        // masks; masked slop bits would hand drive states to the
        // revert LUT
        let Some(w) = g.intersection(region.align8()) else {
            return Ok(false);
        };
        let Some(rs) = self.epd.region_state(w.x, w.y, w.w, w.h) else {
            return Ok(false);
        };
        let res = self.epd.grayscale_revert_pass(&rs).await;
        // the codes in RAM stay valid (the pass writes nothing), so
        // the box survives for the closers of the following session
        if res.is_err() {
            self.force_ghost_clear();
        }
        res.map(|()| true)
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

        let r = region.align8();
        let hard_redrive = self.stale.is_some_and(|s| s.intersects(r));

        let rs = if hard_redrive {
            self.epd.partial_phase1_bw_inv_red(
                self.strip,
                r.x,
                r.y,
                r.w,
                r.h,
                &mut self.delay,
                draw,
            )
        } else {
            self.epd
                .partial_phase1_bw(self.strip, r.x, r.y, r.w, r.h, &mut self.delay, draw)
        }
        .ok_or(PartialRejected::Empty)?;

        self.epd.partial_start_du(&rs);
        self.partials = self.partials.saturating_add(1);
        self.mark_fresh(r);

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
        self.fresh = Some(FULL_REGION);
        // both planes now hold content, not gray codes
        self.gray = None;
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
    /// levels the RAM planes cannot express, so the whole screen is
    /// marked stale; on timeout the next partial request additionally
    /// promotes to a full GC.
    pub async fn grayscale_full<F>(&mut self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let res = self.epd.grayscale_pass(self.strip, &FULL_RS, draw).await;
        self.mark_stale(FULL_REGION);
        self.gray = Some(FULL_REGION);
        self.fresh = None;
        if res.is_err() {
            self.force_ghost_clear();
        }
        res
    }

    /// Grayscale AA pass over everything driven to plain BW since the
    /// last pass. Used by the deferred fire: a full-screen pass would
    /// re-pulse areas that already carry their gray and drive them
    /// off level, while areas nobody redrew still show the AA the
    /// previous pass gave them. No-op when nothing was refreshed.
    pub async fn grayscale_fresh<F>(&mut self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let Some(region) = self.fresh else {
            return Ok(());
        };
        // snap outward so the physical window carries no edge masks:
        // masked bits land in both planes, which the gray LUT reads as
        // a drive-dark state
        let region = region.align8_xy();
        let Some(rs) = self
            .epd
            .region_state(region.x, region.y, region.w, region.h)
        else {
            self.fresh = None;
            return Ok(());
        };

        let res = self.epd.grayscale_pass(self.strip, &rs, draw).await;
        self.fresh = None;
        self.mark_stale(region);
        self.set_gray_region(region);
        if res.is_err() {
            self.force_ghost_clear();
        }
        res
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
            hard_redrive: self.hard_redrive,
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
        // the panel now matches both planes across this region; if the
        // re-drive swallowed the stale box, nothing is outstanding
        if self.hard_redrive {
            s.clear_stale_within(self.region);
        }
        // planes over this region hold content now, not gray codes
        s.invalidate_gray_within(self.region);
    }

    /// Skip phase 3 (content changed mid-waveform); RED RAM keeps the
    /// pre-waveform image while the panel shows the new one, so the
    /// region needs an inv_red re-drive before any delta DU.
    pub fn abandon(self) {
        let region = self.region;
        self.screen.mark_stale(region);
        // phase 1 replaced any gray codes here with content planes
        self.screen.invalidate_gray_within(region);
    }

    /// Grayscale AA pass over this refresh's region instead of phase 3.
    ///
    /// The gray LUT states are short relative pulses that lighten
    /// pixels the BW frame just drove black, and `{0,0}` is literally
    /// "no change", so the pass leaves the panel holding intermediate
    /// levels neither RAM plane can express. The region is therefore
    /// marked stale: the next partial touching it re-drives from black
    /// via inv_red, which is the starting state the pulses assume.
    /// Clearing the mark instead (RED := BW) makes the following DU
    /// skip those pixels and the next pass stack another pulse on an
    /// already-gray pixel, washing the AA ramp out over a few turns.
    /// On timeout the pass is additionally promoted to a full GC.
    pub async fn grayscale<F>(self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let s = self.screen;
        let res = s.epd.grayscale_pass(s.strip, &self.rs, draw).await;
        // the DU that preceded this pass re-drove the region when it
        // took the inv_red path, so retire the old box before marking
        // the fresh gray; otherwise the box only ever grows
        if self.hard_redrive {
            s.clear_stale_within(self.region);
        }
        s.mark_stale(self.region);
        s.set_gray_region(self.region);
        s.clear_fresh_within(self.region);
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
        s.stale = None;
        s.gray = None;
    }
}
