// scheduler: main event loop, render pipeline, housekeeping, sleep
//
// EPD and SD share a single SPI bus via CriticalSectionDevice;
// during normal operation, all SD I/O completes before render()
// touches the EPD; during the DU/GC waveform (~400ms), the EPD
// charge pump drives pixels with no SPI commands, so the bus is
// free for SD I/O - busy_wait_with_background exploits this
// window to run background caching and housekeeping
//
// handle_input and poll_housekeeping are synchronous; they return
// a bool flag when the caller should enter_sleep (which is async
// because it renders a sleep screen via the EPD)
//
// sd_card_sleep sends cmd0 before deep sleep to reduce sd card
// idle current from ~150 uA to ~10 uA

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Ticker, with_timeout};
use log::{debug, info};

use super::app::{AppLayer, Redraw, Transition};
use super::input_policy::{ResolvedInput, SemanticInput};
use crate::drivers::battery;
use crate::drivers::input::Event;
use crate::drivers::strip::StripBuffer;
use crate::kernel::tasks;

use crate::ui::{free_stack_bytes, stack_high_water_mark};

use super::timing;

/// Outcome of resolving a hardware event through the input policy.
enum InputResult<Id> {
    /// Event was ignored or handled with no state change.
    Nothing,
    /// Forwarded raw input produced a transition.
    Transition(Transition<Id>),
    /// Forwarded raw input changed overlay state with no transition.
    OverlayChanged,
    /// Semantic input should be handled by the app layer.
    Semantic(SemanticInput),
    /// Sleep was requested.
    Sleep,
}

/// Action deferred until the EPD waveform completes.
enum DeferredAction<Id> {
    Transition(Transition<Id>),
    Semantic(SemanticInput),
}

impl super::Kernel {
    // render boot console to EPD; call before boot() to show
    // hardware init progress in the built-in mono font
    pub async fn show_boot_console(&mut self, console: &super::BootConsole) {
        let draw = |s: &mut StripBuffer| console.draw(s);
        if self
            .epd
            .full_refresh_async(self.strip, &mut self.delay, &draw)
            .await
            .is_err()
        {
            log::warn!("show_boot_console: EPD refresh timed out");
        }
    }

    // check for valid session early (before boot console) so we can
    // skip the console render on fast wake. checks RTC first (free),
    // then SD fallback (one file read).
    pub fn has_valid_session(&self) -> bool {
        use super::rtc_session::RtcSession;
        // RTC check is a single volatile read — essentially free
        if RtcSession::rtc_peek_valid() {
            return true;
        }
        // SD fallback: check if session file exists and is valid.
        // this costs one SD read (~20ms) but saves ~1.6s of EPD refresh
        // when the session is valid.
        RtcSession::load_from_sd(&self.sd).is_some()
    }

    // one-time boot: load caches, settings, render the home screen
    // if waking from deep sleep with valid RTC session, restore it
    pub async fn boot<A: AppLayer>(&mut self, app_mgr: &mut A) {
        use super::rtc_session::RtcSession;
        use embassy_time::Instant;

        let boot_start = Instant::now();

        // log reset reason for debugging RTC session persistence
        {
            use esp_hal::rtc_cntl::{SocResetReason, reset_reason};
            use esp_hal::system::Cpu;
            let reason = reset_reason(Cpu::ProCpu);
            info!("boot: reset reason = {:?}", reason);
            // on battery wake, brownout can produce SysBrownOut instead
            // of CoreDeepSleep — we disable the detector before sleep to
            // prevent this (see enter_sleep)
            if matches!(reason, Some(SocResetReason::SysBrownOut)) {
                info!("boot: WARNING brownout reset detected (RTC memory may be lost)");
            }
        }

        let t0 = Instant::now();
        self.bm_cache.ensure_loaded(&self.sd);
        let bm_ms = t0.elapsed().as_millis();
        info!("boot: bookmark cache loaded ({}ms)", bm_ms);

        // check for valid session: try RTC first (fast), then SD fallback.
        //
        // on battery wake, the brownout detector fires during the voltage
        // sag, causing a full system reset that wipes RTC FAST memory.
        // the SD-backed session survives this and provides reliable resume.
        let t0 = Instant::now();
        let has_rtc_session = RtcSession::rtc_consume();
        let rtc_session_data = if has_rtc_session {
            let session = RtcSession::rtc_load();
            info!(
                "boot: RTC session valid (wake count {}) ({}ms)",
                session.wake_count(),
                t0.elapsed().as_millis()
            );
            Some(session)
        } else {
            // RTC invalid — try SD fallback (typical on battery wake)
            let t1 = Instant::now();
            match RtcSession::load_from_sd(&self.sd) {
                Some(session) => {
                    info!(
                        "boot: SD session valid (wake count {}) ({}ms)",
                        session.wake_count(),
                        t1.elapsed().as_millis()
                    );
                    Some(session)
                }
                None => {
                    info!("boot: no session (power-on or first boot)");
                    None
                }
            }
        };

        // load settings from SD
        let t0 = Instant::now();
        {
            let mut handle = self.handle();
            app_mgr.load_eager_settings(&mut handle);
        }
        info!("boot: settings loaded ({}ms)", t0.elapsed().as_millis());

        // only load home recent data if we're not restoring into a
        // different app — saves SD I/O when waking directly to reader
        let skip_home_load = rtc_session_data.as_ref().map_or(false, |s| {
            // active app is the top of the stack
            s.nav_depth > 0 && s.nav_stack[(s.nav_depth - 1) as usize] != 0 // 0 = Home
        });

        if !skip_home_load {
            let t0 = Instant::now();
            {
                let mut handle = self.handle();
                app_mgr.load_initial_state(&mut handle);
            }
            info!("boot: home recent loaded ({}ms)", t0.elapsed().as_millis());
        } else {
            info!("boot: skipped home recent load (not waking to home)");
        }

        // apply initial settings to hardware and record them so the
        // generation-based check in run() starts from a known baseline
        tasks::set_idle_timeout(app_mgr.system_settings().sleep_timeout);
        self.epd
            .set_sunlight_mode(app_mgr.system_settings().sunlight_fix);
        self.applied
            .init_from(app_mgr.settings_generation(), app_mgr.system_settings());
        self.log_stats();

        // try to restore session from RTC memory
        let t0 = Instant::now();
        let restored = if let Some(session) = rtc_session_data {
            let ok = app_mgr.apply_session(&session, &mut self.handle());
            info!(
                "boot: apply_session {} ({}ms)",
                if ok { "ok" } else { "failed" },
                t0.elapsed().as_millis()
            );
            ok
        } else {
            false
        };

        if !restored {
            app_mgr.enter_initial(&mut self.handle());
        }

        {
            let active = app_mgr.active();
            {
                let ctx = app_mgr.ctx_mut();
                info!(
                    "boot: first frame active={:?} loading_active={} loading='{}' pct={}",
                    active,
                    ctx.loading_active(),
                    ctx.loading_msg(),
                    ctx.loading_pct()
                );
            }

            let t0 = Instant::now();
            let draw = |s: &mut StripBuffer| app_mgr.draw(s);
            if self
                .epd
                .full_refresh_async(self.strip, &mut self.delay, &draw)
                .await
                .is_err()
            {
                log::warn!("boot: first EPD refresh timed out");
            }
            info!("boot: first EPD refresh ({}ms)", t0.elapsed().as_millis());
        }
        let _ = app_mgr.take_redraw();

        info!(
            "boot: ui ready (total {}ms, path={})",
            boot_start.elapsed().as_millis(),
            if restored { "rtc-restore" } else { "cold-boot" }
        );
        crate::perf_event!(
            "boot",
            "ready path={} elapsed_ms={}",
            if restored { "rtc-restore" } else { "cold-boot" },
            boot_start.elapsed().as_millis()
        );
    }

    // event-driven main loop; never returns
    //
    // two genuine async suspension points in steady state:
    //   1. select(INPUT_EVENTS.receive(), work_ticker.next())
    //   2. EPD busy pin wait inside render()
    // everything between them is synchronous function calls
    pub async fn run<A: AppLayer>(&mut self, app_mgr: &mut A) -> ! {
        let mut work_ticker = Ticker::every(Duration::from_millis(timing::TICK_MS));

        loop {
            if app_mgr.needs_special_mode() {
                self.handle_special_mode(app_mgr).await;
                continue;
            }

            // async point 1: wait for input or tick
            let hw_event = match select(tasks::INPUT_EVENTS.receive(), work_ticker.next()).await {
                Either::First(ev) => Some(ev),
                Either::Second(_) => None,
            };

            if let Some(ev) = hw_event {
                if matches!(ev, Event::LongPress(_)) {
                    debug!("scheduler: received {:?}", ev);
                }
                if self.handle_input(ev, app_mgr) {
                    self.sleep_with_session(app_mgr, "power held").await;
                    continue;
                }
            }

            if app_mgr.needs_special_mode() {
                continue;
            }

            // SPI bus sharing invariant
            //
            // the EPD and SD card share a single SPI2 bus via
            // CriticalSectionDevice (RefCell under the hood).
            //
            //   1. background SD I/O runs here, before EPD access;
            //      interruptible by input so the user can navigate
            //      away during long-running operations
            //   2. poll_housekeeping may do SD I/O, also before render
            //   3. render() touches the EPD; during the waveform window
            //      busy_wait_with_background runs SD I/O because the
            //      EPD charge pump is driving pixels with no SPI commands
            //   4. no SD I/O outside these three sites
            //
            // background steps are bounded and sync; between steps we
            // poll for input so the user can interrupt long-running
            // multi-step operations (e.g. chapter caching)
            'bg: loop {
                let outcome = {
                    let mut handle = self.handle();
                    app_mgr.run_background_step(&mut handle, super::app::BgBudget::new())
                };

                // check for pending input between steps
                if let Ok(ev) = tasks::INPUT_EVENTS.try_receive() {
                    if self.handle_input(ev, app_mgr) {
                        self.sleep_with_session(app_mgr, "power held").await;
                        // sleep returns; restart main loop
                        break 'bg;
                    }
                    if app_mgr.needs_special_mode() {
                        break 'bg;
                    }
                }

                match outcome {
                    super::app::BgOutcome::Progress { more: true } => continue 'bg,
                    _ => break 'bg,
                }
            }

            if app_mgr.needs_special_mode() {
                continue;
            }

            if self.poll_housekeeping() {
                self.sleep_with_session(app_mgr, "idle timeout").await;
                continue;
            }

            // generation-based settings propagation: only re-apply
            // hardware state when the app layer signals a change
            let settings_gen = app_mgr.settings_generation();
            if settings_gen != self.applied.generation {
                let swap_changed =
                    self.applied
                        .sync(settings_gen, app_mgr.system_settings(), &mut self.epd);
                if swap_changed {
                    app_mgr.on_swap_buttons_changed(self.applied.swap_buttons);
                }
            }

            // push live chrome state into the app layer so the top
            // status bar shows up-to-date numbers.
            let pct = crate::drivers::battery::battery_percentage(self.cached_battery_mv);
            let day_pages = self.day_stats.pages();
            let day_secs = self.day_stats.secs_today();
            app_mgr.set_chrome_state(pct, day_pages, day_secs);

            // opportunistic flush of deferred app persistence (RECENT,
            // reading stats) in safe no-redraw windows. apps own their
            // debounce logic so most calls are cheap no-ops.
            if !app_mgr.has_redraw() {
                if let Err(e) = app_mgr.flush_deferred_persistence(
                    &mut self.handle(),
                    super::app::DeferredPersistenceReason::Opportunistic,
                ) {
                    debug!("scheduler: opportunistic flush error: {}", e);
                }
            }

            if app_mgr.ctx_mut().render_ready() {
                let redraw = app_mgr.take_redraw();
                if self.render(app_mgr, redraw).await {
                    self.sleep_with_session(app_mgr, "power held").await;
                    continue;
                }
            }
        }
    }

    // delegate to app layer for modes that bypass normal dispatch
    // (e.g. wifi upload); kernel passes hardware resources through
    async fn handle_special_mode<A: AppLayer>(&mut self, app_mgr: &mut A) {
        app_mgr
            .run_special_mode(&mut self.epd, self.strip, &mut self.delay, &self.sd)
            .await;

        app_mgr.apply_transition(Transition::Pop, &mut self.handle());
        app_mgr.request_full_redraw();
    }

    /// Shared helper: run a hardware event through the input policy and
    /// dispatch forwarded raw events to the app layer.
    ///
    /// Both `handle_input` (normal path) and `busy_wait_with_background`
    /// (waveform path) call this so the policy resolution logic cannot
    /// drift between the two sites. Semantic inputs are returned to the
    /// caller so the waveform path can defer them until refresh completes.
    ///
    /// `suppress_forward`: when true, forwarded raw events are dropped
    /// (used during EPD waveform when the quick-menu overlay is open).
    fn resolve_input<A: AppLayer>(
        &mut self,
        hw_event: Event,
        app_mgr: &mut A,
        suppress_forward: bool,
    ) -> InputResult<A::Id> {
        match self.input_policy.resolve(hw_event) {
            ResolvedInput::RequestSleep => {
                info!("input_policy: RequestSleep");
                InputResult::Sleep
            }
            ResolvedInput::Semantic(s) => InputResult::Semantic(s),
            ResolvedInput::Forward(ev) => {
                if suppress_forward {
                    return InputResult::Nothing;
                }
                let suppressed_before = app_mgr.suppress_deferred_input();
                let t = app_mgr.dispatch_event(ev, &mut *self.bm_cache);
                if t != Transition::None {
                    InputResult::Transition(t)
                } else if app_mgr.suppress_deferred_input() != suppressed_before {
                    InputResult::OverlayChanged
                } else {
                    InputResult::Nothing
                }
            }
            ResolvedInput::Ignore => InputResult::Nothing,
        }
    }

    fn apply_deferred_action<A: AppLayer>(
        &mut self,
        action: DeferredAction<A::Id>,
        app_mgr: &mut A,
    ) {
        match action {
            DeferredAction::Transition(t) => {
                app_mgr.apply_transition(t, &mut self.handle());
            }
            DeferredAction::Semantic(input) => {
                let t = app_mgr.dispatch_semantic(input);
                if t != Transition::None {
                    app_mgr.apply_transition(t, &mut self.handle());
                }
            }
        }
    }

    // returns true if caller should call enter_sleep
    fn handle_input<A: AppLayer>(&mut self, hw_event: Event, app_mgr: &mut A) -> bool {
        let _ = tasks::IDLE_SLEEP_DUE.try_take();

        match self.resolve_input(hw_event, app_mgr, false) {
            InputResult::Sleep => true,
            InputResult::Transition(t) => {
                app_mgr.apply_transition(t, &mut self.handle());
                tasks::request_hold_reset();
                false
            }
            InputResult::OverlayChanged => {
                tasks::request_hold_reset();
                false
            }
            InputResult::Semantic(input) => {
                let t = app_mgr.dispatch_semantic(input);
                if t != Transition::None {
                    app_mgr.apply_transition(t, &mut self.handle());
                }
                false
            }
            InputResult::Nothing => false,
        }
    }

    // shared housekeeping body: battery, sd probe, bookmark flush, stats
    fn poll_housekeeping_inner(&mut self) {
        if let Some(mv) = tasks::BATTERY_MV.try_take() {
            self.cached_battery_mv = mv;
        }

        if tasks::SD_CHECK_DUE.try_take().is_some() {
            self.sd_ok = self.sd.probe_ok();
        }

        if tasks::BOOKMARK_FLUSH_DUE.try_take().is_some() && self.bm_cache.is_dirty() {
            self.bm_cache.flush(&self.sd);
        }

        // flush today's reading stats opportunistically when dirty.
        // piggybacks on the bookmark cadence so we don't add another
        // background task; cost is one ~16-byte write per minute or so
        // while the user is actively reading.
        if self.day_stats.is_dirty() && self.sd_ok {
            if let Err(e) = self.day_stats.flush(&self.sd) {
                log::warn!("daystats flush: {}", e);
            } else {
                // mtime of DAYSTATS.BIN just advanced; refresh
                // today_key so a same-session rollover is detected
                // before the next boot.
                if let Some(k) = self
                    .sd
                    .file_mtime_day_key_in_plump(super::daystats::DAYSTATS_FILE)
                {
                    self.today_key = k;
                }
            }
        }

        if tasks::STATUS_DUE.try_take().is_some() {
            self.log_stats();
        }
    }

    // returns true if idle sleep is due
    fn poll_housekeeping(&mut self) -> bool {
        self.poll_housekeeping_inner();
        tasks::IDLE_SLEEP_DUE.try_take().is_some()
    }

    // housekeeping without idle-sleep check; never sleep mid-refresh
    fn poll_housekeeping_waveform(&mut self) {
        self.poll_housekeeping_inner();
    }

    // partial refreshes use DU waveform (~400 ms); after ghost_clear_every
    // partials, a full GC refresh (~1.6 s) clears ghosting
    //
    // returns true if power-long-press arrived during the waveform and
    // the caller should enter sleep
    async fn render<A: AppLayer>(&mut self, app_mgr: &mut A, redraw: Redraw) -> bool {
        use embassy_time::Instant;
        crate::perf_begin!(_render_t0);

        #[cfg(feature = "perf")]
        let requested_mode = match redraw {
            Redraw::None => "none",
            Redraw::Partial(_) => "partial",
            Redraw::Full => "full",
        };
        #[cfg(feature = "perf")]
        let mut actual_mode = "none";
        let mut sleep_requested = false;
        let active = app_mgr.active();
        {
            let ctx = app_mgr.ctx_mut();
            debug!(
                "render: begin active={:?} redraw={:?} loading_active={} loading='{}' pct={}",
                active,
                redraw,
                ctx.loading_active(),
                ctx.loading_msg(),
                ctx.loading_pct()
            );
        }

        'render: {
            if let Redraw::Partial(r) = redraw {
                let ghost_clear_every = app_mgr.ghost_clear_every();

                if self.partial_refreshes < ghost_clear_every {
                    let r = r.align8();

                    let t_write = Instant::now();
                    let rs = {
                        let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                        if self.red_stale {
                            self.epd.partial_phase1_bw_inv_red(
                                self.strip,
                                r.x,
                                r.y,
                                r.w,
                                r.h,
                                &mut self.delay,
                                &draw,
                            )
                        } else {
                            self.epd.partial_phase1_bw(
                                self.strip,
                                r.x,
                                r.y,
                                r.w,
                                r.h,
                                &mut self.delay,
                                &draw,
                            )
                        }
                    };

                    if let Some(rs) = rs {
                        #[cfg(feature = "perf")]
                        {
                            actual_mode = "partial";
                        }
                        let write_ms = t_write.elapsed().as_millis();
                        debug!(
                            "render: partial phase1 region={:?} red_stale={} ({}ms)",
                            r, self.red_stale, write_ms
                        );
                        let t_wave = Instant::now();
                        self.epd.partial_start_du(&rs);
                        let (deferred, sleep) = self.busy_wait_with_background(app_mgr).await;
                        sleep_requested = sleep;
                        let wave_ms = t_wave.elapsed().as_millis();
                        debug!(
                            "render: partial waveform done region={:?} pending_redraw={} deferred={} sleep={} ({}ms)",
                            r,
                            app_mgr.has_redraw(),
                            deferred.is_some(),
                            sleep,
                            wave_ms
                        );
                        crate::perf_event!(
                            "render",
                            "partial write_ms={} wave_ms={} region_x={} region_y={} region_w={} region_h={}",
                            write_ms,
                            wave_ms,
                            r.x,
                            r.y,
                            r.w,
                            r.h
                        );

                        // skip phase 3 when content changed mid-DU or
                        // a deferred action is queued (the screen
                        // will be redrawn immediately after); the next
                        // partial will use inv_red to compensate for
                        // the desynchronised RED RAM
                        if app_mgr.has_redraw() || deferred.is_some() {
                            app_mgr.ctx_mut().mark_dirty(r);
                            self.red_stale = true;
                            self.partial_refreshes += 1;
                        } else {
                            self.partial_refreshes += 1;

                            if app_mgr.system_settings().text_aa
                                && app_mgr.wants_grayscale()
                                && !app_mgr.has_redraw()
                            {
                                // grayscale AA: skip phase3_sync (gray overwrites
                                // both RAMs) and skip post-gray restore (next page
                                // turn uses inv_red to resync)
                                let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                                if self.epd.grayscale_pass(self.strip, &rs, &draw).await.is_err() {
                                    log::warn!("render: grayscale_pass timed out, forcing full GC next frame");
                                    // post-wait BW restore was skipped; next partial would
                                    // see stale BW delta. force a full GC on the next refresh
                                    self.partial_refreshes = app_mgr.ghost_clear_every();
                                }
                                self.red_stale = true;
                            } else {
                                let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                                self.epd.partial_phase3_sync(self.strip, &rs, &draw);
                                // don't clear red_stale here: phase3_sync only
                                // covers the dirty region, so RED RAM outside it
                                // may still be desynchronised (e.g. after a
                                // grayscale pass). only full GC clears red_stale.
                                if self.epd.power_off_async().await.is_err() {
                                    log::warn!("render: power_off_async timed out after partial DU");
                                }
                            }
                        }

                        if let Some(action) = deferred {
                            self.apply_deferred_action(action, app_mgr);
                        }

                        break 'render;
                    }

                    if !self.epd.needs_initial_refresh() {
                        break 'render;
                    }
                    info!("display: partial failed (initial refresh), promoting to full");
                } else {
                    info!("display: promoted partial to full (ghosting clear)");
                }
            }

            if matches!(redraw, Redraw::Full | Redraw::Partial(_)) {
                self.log_stats();

                let t_write = Instant::now();
                {
                    let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                    self.epd
                        .write_full_frame(self.strip, &mut self.delay, &draw);
                }
                #[cfg(feature = "perf")]
                {
                    actual_mode = "full";
                }
                let write_ms = t_write.elapsed().as_millis();
                debug!("render: full frame written ({}ms)", write_ms);

                let t_wave = Instant::now();
                self.epd.start_full_update();

                let (deferred, sleep) = self.busy_wait_with_background(app_mgr).await;
                sleep_requested = sleep;
                let wave_ms = t_wave.elapsed().as_millis();
                debug!(
                    "render: full waveform done pending_redraw={} deferred={} sleep={} ({}ms)",
                    app_mgr.has_redraw(),
                    deferred.is_some(),
                    sleep,
                    wave_ms
                );
                crate::perf_event!("render", "full write_ms={} wave_ms={}", write_ms, wave_ms);

                self.epd.finish_full_update();
                self.partial_refreshes = 0;
                self.red_stale = false;

                // After a full GC refresh the panel is left in plain BW.
                // re-apply grayscale AA for reader text; next partial
                // will use inv_red to resync both RAM planes
                if app_mgr.system_settings().text_aa
                    && app_mgr.wants_grayscale()
                    && !app_mgr.has_redraw()
                    && deferred.is_none()
                {
                    let rs = crate::drivers::ssd1677::RenderState {
                        px: 0,
                        py: 0,
                        pw: crate::drivers::ssd1677::WIDTH,
                        ph: crate::drivers::ssd1677::HEIGHT,
                        left_mask: 0,
                        right_mask: 0,
                    };
                    let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                    if self.epd.grayscale_pass(self.strip, &rs, &draw).await.is_err() {
                        log::warn!(
                            "render: post-GC grayscale_pass timed out, forcing full GC next frame"
                        );
                        self.partial_refreshes = app_mgr.ghost_clear_every();
                    }
                    self.red_stale = true;
                }

                if let Some(action) = deferred {
                    self.apply_deferred_action(action, app_mgr);
                }
            }
        } // 'render

        debug!(
            "render: end active={:?} pending_redraw={} sleep_requested={}",
            app_mgr.active(),
            app_mgr.has_redraw(),
            sleep_requested
        );
        crate::perf_event!(
            "render",
            "complete requested={} actual={} elapsed_ms={}",
            requested_mode,
            actual_mode,
            _render_t0.elapsed().as_millis()
        );

        sleep_requested
    }

    // collect input and run background work while EPD is busy refreshing
    //
    // during the DU/GC waveform the EPD charge pump drives pixels;
    // no SPI commands are sent, so the bus is free for SD I/O.
    // is_busy() is a sync GPIO read; no epd borrow is held across
    // any .await point, so self is fully available for handle() etc.
    //
    // background work runs as bounded sync steps via
    // run_background_step; between steps we poll for input via
    // with_timeout. the TICK_MS timeout ensures is_busy is
    // re-checked regularly even when no input arrives.
    //
    // first deferred action wins; hold reset prevents the held
    // button from re-firing LongPress/Repeat for the waveform
    //
    // returns (deferred_action, sleep_requested) so the caller
    // can enter sleep after the EPD finishes if power-long-press
    // arrived during the waveform
    async fn busy_wait_with_background<A: AppLayer>(
        &mut self,
        app_mgr: &mut A,
    ) -> (Option<DeferredAction<A::Id>>, bool) {
        let mut deferred: Option<DeferredAction<A::Id>> = None;
        let mut sleep_requested = false;

        loop {
            if !self.epd.is_busy() {
                break;
            }

            // run one bounded background step, then poll for input
            {
                let mut handle = self.handle();
                app_mgr.run_background_step(&mut handle, super::app::BgBudget::new());
            }

            // check for input; tick timeout ensures busy loop doesn't spin
            // too tightly when no input and no background work remain
            let ev = match with_timeout(
                Duration::from_millis(timing::TICK_MS),
                tasks::INPUT_EVENTS.receive(),
            )
            .await
            {
                Ok(ev) => Some(ev),
                Err(_) => None,
            };

            if let Some(hw_event) = ev {
                let _ = tasks::IDLE_SLEEP_DUE.try_take();
                let suppress = app_mgr.suppress_deferred_input();

                match self.resolve_input(hw_event, app_mgr, suppress) {
                    InputResult::Sleep => {
                        info!("busy_wait: sleep requested during waveform, will sleep after");
                        sleep_requested = true;
                    }
                    InputResult::Transition(t) => {
                        if deferred.is_none() {
                            deferred = Some(DeferredAction::Transition(t));
                            tasks::request_hold_reset();
                        }
                    }
                    InputResult::OverlayChanged => {
                        tasks::request_hold_reset();
                    }
                    InputResult::Semantic(input) => {
                        if !suppress && deferred.is_none() {
                            deferred = Some(DeferredAction::Semantic(input));
                        }
                    }
                    InputResult::Nothing => {}
                }
            }

            self.poll_housekeeping_waveform();
        }

        (deferred, sleep_requested)
    }

    // save session to RTC memory + SD card and enter deep sleep.
    //
    // RTC FAST memory is the fast path but is lost on battery wake
    // due to brownout resets (voltage sag wipes the RTC power domain).
    // the SD copy is the reliable fallback (~20ms extra).
    async fn sleep_with_session<A: AppLayer>(&mut self, app_mgr: &mut A, reason: &str) {
        use super::rtc_session::RtcSession;
        use embassy_time::Instant;

        let sleep_start = Instant::now();

        // save active app state (reader position) to bookmark cache
        // before collecting session, so bookmarks stay in sync
        app_mgr.save_active_state(&mut *self.bm_cache);

        // force flush deferred app persistence (RECENT, reading stats)
        // before deep sleep so no dirty state is lost
        // TODO: decide policy on force-flush failure here: keep the
        // current best-effort behavior, retry, or abort/defer sleep to
        // preserve durability guarantees more strictly.
        if let Err(e) = app_mgr.flush_deferred_persistence(
            &mut self.handle(),
            super::app::DeferredPersistenceReason::Sleep,
        ) {
            info!("sleep: flush_deferred_persistence error: {}", e);
        }

        // collect session state from app layer
        let t0 = Instant::now();
        let mut session = RtcSession::zeroed();
        app_mgr.collect_session(&mut session);

        // increment wake count for debugging
        session.increment_wake_count();

        // mark valid BEFORE saving so both RTC and SD get the magic
        session.mark_valid();

        // save to RTC memory (fast path, works on USB / stable power)
        session.rtc_save();

        // save to SD card (reliable fallback for battery wake)
        session.save_to_sd(&self.sd);
        info!(
            "sleep: session saved to RTC + SD ({}ms)",
            t0.elapsed().as_millis()
        );

        // let the active app drop transient heap (reader chapter cache,
        // decoded images, etc.) BEFORE the wallpaper allocator runs in
        // enter_sleep. session capture must happen first because
        // collect_session reads reader state we're about to free.
        info!("sleep: freeing active app transient heap...");
        let t0 = Instant::now();
        app_mgr.on_active_pre_sleep(&mut self.handle());
        info!(
            "sleep: active app cleanup ({}ms)",
            t0.elapsed().as_millis()
        );

        self.enter_sleep(reason, sleep_start).await;
    }

    // flush bookmarks, render sleep screen, enter MCU deep sleep;
    // on real hardware this never returns (wake = full MCU reset)
    //
    // uses a custom sleep config that keeps RTC FAST memory powered
    // so session state survives the sleep cycle (~1-2µA extra)
    async fn enter_sleep(&mut self, reason: &str, sleep_start: embassy_time::Instant) {
        use embassy_time::Instant;
        use embedded_graphics::mono_font::MonoTextStyle;
        use embedded_graphics::mono_font::ascii::FONT_9X18;
        use embedded_graphics::pixelcolor::BinaryColor;
        use embedded_graphics::prelude::*;
        use embedded_graphics::text::Text;
        use esp_hal::gpio::RtcPinWithResistors;
        use esp_hal::rtc_cntl::Rtc;
        use esp_hal::rtc_cntl::sleep::{RtcSleepConfig, RtcioWakeupSource, WakeupLevel};

        info!("{}: entering sleep...", reason);

        info!("sleep: flushing bookmarks...");
        let t0 = Instant::now();
        if self.bm_cache.is_dirty() {
            self.bm_cache.flush(&self.sd);
        }
        info!("sleep: bookmark flush ({}ms)", t0.elapsed().as_millis());

        // load sleep wallpaper from SD before putting the card to sleep
        info!("sleep: loading wallpaper from SD...");
        let t0 = Instant::now();
        let sleep_img = super::sleep_image::load_sleep_image(&self.sd);
        if sleep_img.is_some() {
            info!("sleep: wallpaper loaded ({}ms)", t0.elapsed().as_millis());
        } else {
            info!("sleep: no wallpaper found ({}ms)", t0.elapsed().as_millis());
        }

        info!("sleep: putting SD card to sleep...");
        let t0 = Instant::now();
        self.sd_card_sleep();
        info!("sleep: SD card sleep ({}ms)", t0.elapsed().as_millis());

        let t0 = Instant::now();
        if let Some(ref img) = sleep_img {
            // render 4-level grayscale wallpaper via dual-plane grayscale pass
            use super::sleep_image::CHUNK_COUNT;
            use crate::drivers::ssd1677::{HEIGHT, RenderState, WIDTH};

            let rs = RenderState {
                px: 0,
                py: 0,
                pw: WIDTH,
                ph: HEIGHT,
                left_mask: 0,
                right_mask: 0,
            };

            // blit all 6 chunks each strip call; blit_2bpp clips to the
            // current strip window so only the overlapping chunk draws pixels
            let draw = |s: &mut StripBuffer| {
                for i in 0..CHUNK_COUNT {
                    s.blit_2bpp(
                        &img.chunks[i],
                        0,
                        img.width as usize,
                        img.chunk_rows(i),
                        img.stride as usize,
                        0,
                        img.chunk_start_row(i) as i32,
                        true,
                    );
                }
            };

            // Establish the base black/white image first, then overlay the
            // intermediate gray levels with the grayscale LUT. This matches
            // the normal grayscale text AA flow more closely than a white clear.
            info!("sleep: rendering wallpaper base BW pass...");
            let t1 = Instant::now();
            if self
                .epd
                .full_refresh_async(self.strip, &mut self.delay, &draw)
                .await
                .is_err()
            {
                log::warn!("sleep: wallpaper base refresh timed out, continuing");
            }
            info!("sleep: wallpaper base BW ({}ms)", t1.elapsed().as_millis());

            info!("sleep: rendering wallpaper grayscale overlay...");
            let t1 = Instant::now();
            // grayscale_pass writes LSB plane to BW RAM and MSB plane to
            // RED RAM, then triggers a single refresh with the grayscale LUT.
            if self.epd.grayscale_pass(self.strip, &rs, &draw).await.is_err() {
                log::warn!("sleep: wallpaper grayscale overlay timed out, continuing");
            }
            info!(
                "sleep: wallpaper grayscale overlay ({}ms)",
                t1.elapsed().as_millis()
            );
        } else {
            // fallback: simple text sleep screen
            info!("sleep: rendering fallback sleep text screen...");
            if self
                .epd
                .full_refresh_async(self.strip, &mut self.delay, &|s: &mut StripBuffer| {
                    let style = MonoTextStyle::new(&FONT_9X18, BinaryColor::On);
                    let _ = Text::new("(sleep)", Point::new(210, 400), style).draw(s);
                })
                .await
                .is_err()
            {
                log::warn!("sleep: fallback text refresh timed out, continuing");
            }
        }
        info!("sleep: screen rendered ({}ms)", t0.elapsed().as_millis());

        info!("sleep: EPD entering deep sleep...");
        self.epd.enter_deep_sleep();
        info!(
            "sleep: EPD deep sleep, total sleep entry {}ms",
            sleep_start.elapsed().as_millis()
        );

        // safety: deep sleep never returns, the MCU resets on wake, so
        // these stolen peripherals cannot alias with their original
        // owners. LPWR is not used elsewhere; GPIO3 was previously
        // cloned into InputHw but we are about to halt the CPU
        let mut rtc = Rtc::new(unsafe { esp_hal::peripherals::LPWR::steal() });
        let mut gpio3 = unsafe { esp_hal::peripherals::GPIO3::steal() };
        let wakeup_pins: &mut [(&mut dyn RtcPinWithResistors, WakeupLevel)] =
            &mut [(&mut gpio3, WakeupLevel::Low)];
        let rtcio = RtcioWakeupSource::new(wakeup_pins);

        // custom sleep config: keep RTC FAST memory powered for session
        // persistence. this adds ~1-2µA to deep sleep current but enables
        // instant wake restoration without SD card I/O.
        let mut sleep_config = RtcSleepConfig::deep();
        sleep_config.set_rtc_fastmem_pd_en(false); // keep RTC FAST powered

        info!("mcu: entering deep sleep (power button to wake)");
        rtc.sleep(&sleep_config, &[&rtcio]);

        // deep sleep resets the MCU; backstop if sleep returns
        #[allow(unreachable_code)]
        loop {
            core::hint::spin_loop();
        }
    }

    // send cmd0 to put sd card into idle/sleep state;
    // reduces sd current from ~150 µa to ~10 µa during deep sleep.
    // call after all sd i/o is done and before epd sleep-screen render
    fn sd_card_sleep(&self) {
        use embedded_hal::digital::OutputPin;

        self.sd.flush_and_close();

        critical_section::with(|cs| {
            let bus_ref = crate::board::SPI_BUS_REF.borrow(cs).get();
            let mut cs_pin = crate::board::SD_CS_SLEEP.borrow_ref_mut(cs);

            if let (Some(bus_ref), Some(pin)) = (bus_ref, cs_pin.as_mut()) {
                let mut bus: core::cell::RefMut<'_, _> = bus_ref.borrow(cs).borrow_mut();
                // 80 clocks cs high (sd spec: card ready for command)
                let _ = bus.write(&[0xFF; 10]);
                let _ = pin.set_low();
                // cmd0 (GO_IDLE_STATE) with valid crc
                let _ = bus.write(&[0x40, 0x00, 0x00, 0x00, 0x00, 0x95]);
                let _ = bus.write(&[0xFF]);
                let _ = pin.set_high();
            }
        });
    }

    pub fn log_stats(&self) {
        let stats = esp_alloc::HEAP.stats();
        let bat_pct = battery::battery_percentage(self.cached_battery_mv);
        let uptime = super::uptime_secs();
        let mins = (uptime / 60) % 60;
        let hrs = uptime / 3600;

        info!(
            "stats: heap {}/{}K peak {}K | stack free {}K hwm {}K | bat {}% {}.{}V | up {}:{:02} | SD:{}",
            stats.current_usage / 1024,
            stats.size / 1024,
            stats.max_usage / 1024,
            free_stack_bytes() / 1024,
            stack_high_water_mark() / 1024,
            bat_pct,
            self.cached_battery_mv / 1000,
            (self.cached_battery_mv % 1000) / 100,
            hrs,
            mins,
            if self.sd_ok { "ok" } else { "--" },
        );
    }
}
