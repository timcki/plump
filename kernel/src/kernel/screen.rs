// screen: typestate refresh interface over the SSD1677 driver
//
// owns the EPD driver, the strip buffer, and all display-plane state
// (stale region, partial counter, forced-clear debt). a refresh is a
// linear session:
//
//   plan_partial -> PartialPlan (Ready(Wave) | RevertFirst)
//   begin_full   -> Wave<'_, M>   (waveform running)
//   wave.settle() / wave.wait() ->  Settled<'_, M>
//   settled.sync_red / .abandon / .grayscale / .finish
//
// Wave borrows the Screen mutably, so no other panel access can
// compile while a waveform is in flight; the scheduler exploits that
// window for SD I/O (the charge pump drives pixels with no SPI
// traffic). misordered phases are unrepresentable: phase 3 methods
// only exist on Settled, which only exists after the wave is consumed,
// and the driver tokens the session carries (see drivers/ssd1677.rs)
// make the same order unrepresentable one layer down
//
// whole-panel rule for custom LUTs: the RAM window set before a
// waveform only scopes the RAM writes. the controller scans every
// gate and drives every source from whatever the planes hold, so a
// waveform "over a window" is really a waveform over the whole panel
// in which the area outside the window is expected to sit in a
// no-change LUT state. the OTP DU has two of those ({0,0} and {1,1},
// plain content), the gray and revert LUTs only one ({0,0}). AA codes
// therefore never coexist with a windowed waveform: gray passes are
// always full-screen, and a windowed partial over a gray-coded panel
// first neutralizes the codes (full revert, planes rewritten with
// content). crosspoint enforces the same rule (grayscaleRevert before
// displayWindow); the windowed gray/revert passes this file used to
// run re-pulsed every AA edge on the panel per session, which is the
// progressive darkening of home and the quick menu surroundings

use core::marker::PhantomData;

use embassy_time::{Instant, TimeoutError};
use esp_hal::delay::Delay;

use crate::board::{Epd, SCREEN_H, SCREEN_W};
use crate::drivers::ssd1677::{Driven, Phase1, Window};
use crate::drivers::strip::StripBuffer;
use crate::kernel::plane_map::{PlaneMap, SessionOutcome};
use crate::ui::{AlignedRegion, Region};

pub struct Screen {
    epd: Epd,
    strip: &'static mut StripBuffer,
    delay: Delay,

    // which panel areas the RAM planes do not describe, and what
    // recovering each takes (re-drive, or revert-then-re-drive).
    // area absent from the map is in sync: delta DUs are safe there
    planes: PlaneMap,

    // something was driven to plain BW since the last AA pass, so a
    // deferred fire has work. the fire is always full-screen (see the
    // whole-panel rule above), so no rect bookkeeping is needed
    aa_pending: bool,

    // partial refreshes since the last full GC; the scheduler promotes
    // to a full clear once this reaches ghost_clear_every
    partials: u32,

    // set when a session left panel and planes out of sync (a timeout,
    // or lost gray coverage): the next partial request promotes to a
    // full GC whatever the counter says
    force_gc: bool,
}

/// Marker for a partial DU waveform session.
pub struct Du;
/// Marker for a full GC waveform session.
pub struct Gc;

/// A waveform in flight. Borrows the [`Screen`] until consumed, so no
/// other panel access can compile while the EPD is busy.
pub struct Wave<'s, M> {
    screen: &'s mut Screen,
    // driver token proving the waveform was kicked over a window the
    // driver itself wrote; the closing phases will not compile without
    // it, so this session type cannot skip ahead
    driven: Driven,
    // logical counterpart of `driven`: the area this session drives,
    // used for stale-region bookkeeping
    region: AlignedRegion,
    hard_redrive: bool,
    _mode: PhantomData<M>,
}

/// A completed waveform awaiting its closing phase.
pub struct Settled<'s, M> {
    screen: &'s mut Screen,
    driven: Driven,
    region: AlignedRegion,
    _mode: PhantomData<M>,
}

/// A planned partial refresh.
///
/// The revert obligation is part of the type: when AA gray codes are
/// on the panel, the only way to obtain a [`Wave`] is through
/// [`PendingRevert::proceed`], which runs the revert wave first. A
/// call site cannot forget the revert and re-drive straight over
/// intermediate grays (the page-turn mottle) or start phase 1 over
/// the codes the revert reads.
pub enum PartialPlan<'s> {
    /// No gray codes on the panel; phase 1 is written and the DU
    /// waveform is already running.
    Ready(Wave<'s, Du>),
    /// Gray codes are on the panel; call [`PendingRevert::proceed`]
    /// to revert them and start the DU.
    RevertFirst(PendingRevert<'s>),
}

/// Proof-of-obligation stage of [`PartialPlan`]: holds the screen
/// until the revert runs.
pub struct PendingRevert<'s> {
    screen: &'s mut Screen,
    region: AlignedRegion,
}

impl<'s> PendingRevert<'s> {
    /// Revert the panel's grays to their rails, then write phase 1
    /// and start the DU.
    ///
    /// A full-screen partial reverts in place and re-drives via
    /// inv_red (the reader page turn). A windowed partial cannot run
    /// while codes sit anywhere in RAM (whole-panel rule), so it
    /// neutralizes first: full revert, then both planes rewritten
    /// with content, after which the window is re-driven like any
    /// stale area. A revert timeout degrades gracefully: the next
    /// frame is promoted to a full GC and the partial still runs,
    /// since its inv_red re-drive keeps the content correct.
    pub async fn proceed<F>(self, draw: &F) -> Result<Wave<'s, Du>, PartialRejected>
    where
        F: Fn(&mut StripBuffer),
    {
        let PendingRevert { screen, region } = self;
        crate::perf_begin!(_t0);
        let res = if region == FULL_REGION {
            screen.revert_windows(region).await
        } else {
            screen.neutralize_gray(draw).await
        };
        match res {
            Ok(true) => {
                crate::perf_event!("render", "revert wave_ms={}", _t0.elapsed().as_millis());
            }
            Ok(false) => {}
            Err(_) => {
                log::warn!("partial: revert pass timed out, forcing full GC next frame");
            }
        }
        screen.start_partial_wave(region, draw)
    }
}

/// Why a partial refresh could not start.
pub enum PartialRejected {
    /// The region aligned to nothing; there is no work to do.
    Empty,
    /// The panel has never been fully refreshed; run a full GC instead.
    NeedsFull,
}

const FULL_REGION: AlignedRegion =
    AlignedRegion::from_aligned(Region::new(0, 0, SCREEN_W, SCREEN_H));

impl Screen {
    pub fn new(epd: Epd, strip: &'static mut StripBuffer, delay: Delay) -> Self {
        Self {
            epd,
            strip,
            delay,
            planes: PlaneMap::new(),
            aa_pending: false,
            partials: 0,
            force_gc: false,
        }
    }

    /// True while the panel holds AA gray levels (codes in RAM).
    #[inline]
    pub fn gray_coded(&self) -> bool {
        self.planes.has_gray()
    }

    /// Drop the panel's analog rails after an idle stretch. The next
    /// refresh pays the ~100ms booster start inside its waveform; a
    /// panel left powered draws the booster's quiescent current for
    /// the whole awake session. No-op when already off.
    pub fn power_off_idle(&mut self) -> bool {
        self.epd.power_off()
    }

    #[inline]
    pub fn panel_powered(&self) -> bool {
        self.epd.is_powered()
    }

    /// True when the next refresh should be a full GC: either enough
    /// partials have piled up since the last clear, or a session left
    /// panel and planes out of sync.
    #[inline]
    pub fn ghost_clear_due(&self, every: u32) -> bool {
        self.force_gc || self.partials >= every
    }

    /// Bounding box of everything a delta DU may not touch; debug view.
    #[inline]
    pub fn stale_region(&self) -> Option<Region> {
        self.planes.bbox().map(AlignedRegion::get)
    }

    /// Force the next partial request to promote to a full GC.
    #[inline]
    pub fn force_ghost_clear(&mut self) {
        self.force_gc = true;
    }

    #[inline]
    pub fn set_sunlight_mode(&mut self, enabled: bool) {
        self.epd.set_sunlight_mode(enabled);
    }

    pub fn enter_deep_sleep(&mut self) {
        self.epd.enter_deep_sleep();
    }

    // snap AA grays under `region` back to their rails, one wave per
    // gray-coded window. gray entries only ever cover the full screen
    // now (gray passes are full-screen), so this runs at most one
    // full revert; the intersection is kept so a stale fragment map
    // cannot widen the window onto planes holding content, which the
    // revert LUT would misdrive (white content reads {1,1}, its
    // drive-black state). the codes in RAM stay valid (the pass
    // writes nothing), so the map survives for the following
    // session's closer
    async fn revert_windows(&mut self, region: AlignedRegion) -> Result<bool, TimeoutError> {
        let mut ran = false;
        for w in self.planes.gray_windows(region).into_iter().flatten() {
            let w = w.get();
            let Some(win) = self.epd.region_state(w.x, w.y, w.w, w.h) else {
                continue;
            };
            if let Err(e) = self.epd.grayscale_revert_pass(&win).await {
                self.force_ghost_clear();
                return Err(e);
            }
            ran = true;
        }
        Ok(ran)
    }

    // leave the gray-coded state before a windowed waveform: revert
    // the whole panel to its rails, then rewrite both planes with the
    // current content so nothing in RAM reads as a drive state to the
    // OTP DU outside the window that follows. the panel then shows
    // the previous frame at its rails while the planes hold the new
    // one, which is exactly the Stale contract, so the full screen is
    // marked stale and the following partial re-drives its window via
    // inv_red instead of a delta against planes equal to itself
    async fn neutralize_gray<F>(&mut self, draw: &F) -> Result<bool, TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let t0 = Instant::now();
        let res = self.revert_windows(FULL_REGION).await;
        let revert_ms = t0.elapsed().as_millis();
        // the FrameWritten token is deliberately not kicked: this is
        // a RAM state change, not a refresh
        let _ = self.epd.write_full_frame(self.strip, &mut self.delay, draw);
        self.planes.clear();
        let _ = self.planes.apply(FULL_REGION, SessionOutcome::Abandoned);
        log::info!(
            "screen: neutralize gray (windowed partial over AA) revert_ms={} total_ms={} ok={}",
            revert_ms,
            t0.elapsed().as_millis(),
            res.is_ok()
        );
        res
    }

    /// Partial DU refresh, waiting inline on the busy pin. Falls back
    /// to a full GC when the panel has not been refreshed yet. For
    /// paths with no background work to overlap (wifi upload screens).
    pub async fn render_partial<F>(&mut self, region: Region, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        match self.plan_partial(region, draw) {
            Ok(PartialPlan::Ready(wave)) => {
                wave.wait().await?.sync_red(draw);
                Ok(())
            }
            Ok(PartialPlan::RevertFirst(pending)) => match pending.proceed(draw).await {
                Ok(wave) => {
                    wave.wait().await?.sync_red(draw);
                    Ok(())
                }
                Err(_) => Ok(()),
            },
            Err(PartialRejected::Empty) => Ok(()),
            Err(PartialRejected::NeedsFull) => self.render_full(draw).await,
        }
    }

    /// Plan a partial DU refresh of `region`.
    ///
    /// When the region overlaps panel area the RAM planes no longer
    /// describe, the write goes through inv_red so the waveform
    /// re-drives every pixel it covers from a known state instead of
    /// computing a delta against a stale plane; and when the planes
    /// hold AA gray codes anywhere, the plan comes back as
    /// [`PartialPlan::RevertFirst`], whose only path to a [`Wave`]
    /// runs the revert pass (or, for a windowed region, the full
    /// neutralize). Callers cannot skip the revert or start phase 1
    /// over codes, and never track plane state themselves.
    pub fn plan_partial<F>(
        &mut self,
        region: Region,
        draw: &F,
    ) -> Result<PartialPlan<'_>, PartialRejected>
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

        // any codes anywhere, not just under r: a windowed DU scans
        // the whole panel, so codes outside the window would be
        // driven too (whole-panel rule). phase 1 must not run yet
        // either way: it would overwrite the codes the revert wave
        // reads as its index
        if self.planes.has_gray() {
            return Ok(PartialPlan::RevertFirst(PendingRevert {
                screen: self,
                region: r,
            }));
        }

        self.start_partial_wave(r, draw).map(PartialPlan::Ready)
    }

    // phase 1 + DU kick, shared by the two plan arms
    fn start_partial_wave<F>(
        &mut self,
        r: AlignedRegion,
        draw: &F,
    ) -> Result<Wave<'_, Du>, PartialRejected>
    where
        F: Fn(&mut StripBuffer),
    {
        let hard_redrive = self.planes.needs_redrive(r);

        let (x, y, w, h) = {
            let r = r.get();
            (r.x, r.y, r.w, r.h)
        };
        let phase1 = if hard_redrive {
            self.epd
                .partial_phase1_bw_inv_red(self.strip, x, y, w, h, &mut self.delay, draw)
        } else {
            self.epd
                .partial_phase1_bw(self.strip, x, y, w, h, &mut self.delay, draw)
        };

        // the driver reports the two failure modes separately, so the
        // rejection the caller sees is the driver's own verdict rather
        // than a re-derivation of it
        let written = match phase1 {
            Phase1::Written(w) => w,
            Phase1::EmptyWindow => return Err(PartialRejected::Empty),
            Phase1::NeedsFullFirst => return Err(PartialRejected::NeedsFull),
        };

        let driven = self.epd.partial_start_du(written);
        self.partials = self.partials.saturating_add(1);
        self.aa_pending = true;

        Ok(Wave {
            screen: self,
            driven,
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
        let written = self.epd.write_full_frame(self.strip, &mut self.delay, draw);
        let driven = self.epd.start_full_update(written);
        // both planes hold content the panel does not show yet; if the
        // GC completes, finish() clears the map, and if it times out
        // the whole screen correctly stays marked for re-drive. gray
        // loss is moot: the running GC wipes the panel gray anyway
        let _ = self.planes.apply(FULL_REGION, SessionOutcome::Abandoned);
        self.aa_pending = true;
        Wave {
            screen: self,
            driven,
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
        let res = self
            .epd
            .grayscale_pass(self.strip, &Window::FULL, draw)
            .await;
        if self.planes.apply(FULL_REGION, SessionOutcome::Grayed) {
            self.force_ghost_clear();
        }
        self.aa_pending = false;
        if res.is_err() {
            self.force_ghost_clear();
        }
        res
    }

    /// Deferred grayscale AA pass: full-screen when anything was
    /// driven to plain BW since the last pass, no-op otherwise. Always
    /// full-screen because the gray LUT has a single no-change state
    /// ({0,0}); any plain content left in RAM outside a smaller window
    /// would take a gray pulse along with it (whole-panel rule).
    pub async fn grayscale_fresh<F>(&mut self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        if !self.aa_pending {
            return Ok(());
        }
        self.grayscale_full(draw).await
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
            driven: self.driven,
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
        s.epd.partial_phase3_sync(s.strip, &self.driven, draw);
        // the panel matches both planes across this region again; the
        // carve is exact, so claims outside it (gray codes elsewhere,
        // an old skipped phase 3) survive as fragments instead of the
        // old whole-box invalidation that kept dropping revert
        // coverage after every sync
        if s.planes.apply(self.region, SessionOutcome::Synced) {
            s.force_ghost_clear();
        }
    }

    /// Skip phase 3 (content changed mid-waveform); RED RAM keeps the
    /// pre-waveform image while the panel shows the new one, so the
    /// region needs an inv_red re-drive before any delta DU.
    pub fn abandon(self) {
        if self
            .screen
            .planes
            .apply(self.region, SessionOutcome::Abandoned)
        {
            self.screen.force_ghost_clear();
        }
    }

    /// Grayscale AA pass over this refresh instead of phase 3.
    ///
    /// The gray LUT states are short relative pulses that lighten
    /// pixels the BW frame just drove black, and `{0,0}` is literally
    /// "no change", so the pass leaves the panel holding intermediate
    /// levels only the codes now in RAM describe. The screen is
    /// tracked as gray-coded: the next partial reverts the grays to
    /// their rails and re-drives via inv_red, the starting state the
    /// DU transitions assume. On timeout the pass is additionally
    /// promoted to a full GC.
    ///
    /// Only a full-screen session takes the pass (whole-panel rule);
    /// a windowed one closes with a plain phase 3 and stays BW.
    pub async fn grayscale<F>(self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        if self.region != FULL_REGION {
            log::debug!(
                "screen: AA skipped for windowed session {:?}",
                self.region.get()
            );
            self.sync_red(draw);
            return Ok(());
        }
        let s = self.screen;
        let res = s
            .epd
            .grayscale_pass(s.strip, &self.driven.window(), draw)
            .await;
        if s.planes.apply(self.region, SessionOutcome::Grayed) {
            s.force_ghost_clear();
        }
        s.aa_pending = false;
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
        s.epd.finish_full_update(self.driven);
        s.partials = 0;
        s.force_gc = false;
        s.planes.clear();
    }
}
