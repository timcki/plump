// screen: typestate refresh interface over the SSD1677 driver
//
// owns the EPD driver, the strip buffer, and all display-plane state
// (red_stale, partial counter). a refresh is a linear session:
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

    // RED RAM out of sync with BW somewhere on screen (grayscale pass
    // or a skipped phase 3); the next partial expands to full screen
    // and re-drives every pixel via inv_red
    red_stale: bool,

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
    entered_stale: bool,
    _mode: PhantomData<M>,
}

/// A completed waveform awaiting its closing phase.
pub struct Settled<'s, M> {
    screen: &'s mut Screen,
    rs: RenderState,
    entered_stale: bool,
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

impl Screen {
    pub fn new(epd: Epd, strip: &'static mut StripBuffer, delay: Delay) -> Self {
        Self {
            epd,
            strip,
            delay,
            red_stale: false,
            partials: 0,
        }
    }

    #[inline]
    pub fn partials_since_clear(&self) -> u32 {
        self.partials
    }

    #[inline]
    pub fn red_stale(&self) -> bool {
        self.red_stale
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

    /// Partial DU refresh, waiting inline on the busy pin. Falls back
    /// to a full GC when the panel has not been refreshed yet. For
    /// paths with no background work to overlap (wifi upload screens).
    pub async fn render_partial<F>(&mut self, region: Region, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        match self.begin_partial(region, draw) {
            Ok(wave) => wave.wait().await?.sync_red(draw).await,
            Err(PartialRejected::Empty) => Ok(()),
            Err(PartialRejected::NeedsFull) => self.render_full(draw).await,
        }
    }

    /// Write BW RAM for `region` and kick the DU waveform.
    ///
    /// When RED RAM is stale the region silently expands to the full
    /// screen and the write re-drives every pixel via inv_red; the
    /// caller never tracks plane state.
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

        let entered_stale = self.red_stale;
        let r = if entered_stale {
            Region::new(0, 0, SCREEN_W, SCREEN_H)
        } else {
            region.align8()
        };

        let rs = if entered_stale {
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

        Ok(Wave {
            screen: self,
            rs,
            entered_stale,
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
        Wave {
            screen: self,
            rs: FULL_RS,
            entered_stale: false,
            _mode: PhantomData,
        }
    }

    /// Full GC refresh, waiting inline on the busy pin. For paths with
    /// no background work to overlap (boot console, sleep screens).
    pub async fn render_full<F>(&mut self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let wave = self.begin_full(draw);
        wave.wait().await?.finish();
        Ok(())
    }

    /// Full-screen grayscale AA pass. Leaves both RAM planes holding
    /// gray data, so `red_stale` is set; on timeout the next partial
    /// request additionally promotes to a full GC.
    pub async fn grayscale_full<F>(&mut self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let res = self.epd.grayscale_pass(self.strip, &FULL_RS, draw).await;
        self.red_stale = true;
        if res.is_err() {
            self.force_ghost_clear();
        }
        res
    }

    /// Rewrite both RAM planes with current content over the whole
    /// screen (no waveform), clearing `red_stale`. Used after a
    /// deferred grayscale pass while the device is idle.
    pub fn resync_red_full<F>(&mut self, draw: &F)
    where
        F: Fn(&mut StripBuffer),
    {
        self.epd.partial_phase3_sync(self.strip, &FULL_RS, draw);
        self.red_stale = false;
    }
}

impl<'s, M> Wave<'s, M> {
    /// Sync GPIO read of the busy pin.
    #[inline]
    pub fn is_busy(&mut self) -> bool {
        self.screen.epd.is_busy()
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
            entered_stale: self.entered_stale,
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
    /// Phase 3: rewrite RED+BW with current content so the next DU
    /// computes a minimal delta, then power off the analog drivers.
    pub async fn sync_red<F>(self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let s = self.screen;
        s.epd.partial_phase3_sync(s.strip, &self.rs, draw);
        // a full-screen inv_red re-drive plus this sync has resynced
        // RED everywhere; region-limited frames may still be stale
        // outside the region
        if self.entered_stale {
            s.red_stale = false;
        }
        s.epd.power_off_async().await
    }

    /// Skip phase 3 (content changed mid-waveform); RED RAM is now
    /// desynchronised and the next partial recovers via inv_red.
    pub fn abandon(self) {
        self.screen.red_stale = true;
    }

    /// Grayscale AA pass over this refresh's region instead of phase 3.
    /// Both RAM planes end up holding gray data, so `red_stale` is set;
    /// on timeout the next partial request promotes to a full GC.
    pub async fn grayscale<F>(self, draw: &F) -> Result<(), TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        let s = self.screen;
        let res = s.epd.grayscale_pass(s.strip, &self.rs, draw).await;
        s.red_stale = true;
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
        s.red_stale = false;
    }
}
