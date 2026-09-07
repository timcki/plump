// scheduler: main event loop, render pipeline, housekeeping, sleep
//
// EPD and SD share a single SPI bus via CriticalSectionDevice;
// during normal operation, all SD I/O completes before render()
// touches the EPD; during the DU/GC waveform (~400ms), the EPD
// charge pump drives pixels with no SPI commands, so the bus is
// free for SD I/O - Services::wave_window exploits this window to
// run background caching and housekeeping while a screen.rs Wave
// session holds the display half of the kernel
//
// handle_input is synchronous and reports AfterInput::Sleep when the
// caller should sleep (sleeping is async because it renders a sleep
// screen via the EPD)
//
// sd_card_sleep sends cmd0 before deep sleep to reduce sd card
// idle current from ~150 uA to ~10 uA

use embassy_futures::select::{Either3, Either4, select3, select4};
use embassy_time::{Duration, Instant, Timer};
use log::{debug, info};

use super::app::{AppIdType, AppLayer, GrayscaleMode, Redraw, Transition};

/// Idle window before a `GrayscaleMode::Deferred` AA pass fires. Sized
/// to feel "settled" without making the user wait noticeably: shorter
/// than a normal reading pause, longer than a single keypress
/// repeat-rate.
const DEFERRED_GRAYSCALE_DELAY: Duration = Duration::from_millis(800);
use super::input_policy::{ResolvedInput, SemanticInput};
use crate::drivers::battery;
use crate::drivers::input::Event;
use crate::drivers::strip::StripBuffer;
use crate::kernel::tasks;

use crate::ui::free_stack_bytes;

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

/// What the caller should do after an input pass or a render.
///
/// Sleep entry is async (it renders a sleep screen), so the sync
/// halves report it instead of doing it; naming the two outcomes keeps
/// the obligation out of comments.
#[must_use]
#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterInput {
    Continue,
    Sleep,
}

impl AfterInput {
    #[inline]
    const fn sleep_if(cond: bool) -> Self {
        if cond { Self::Sleep } else { Self::Continue }
    }

    #[inline]
    const fn wants_sleep(self) -> bool {
        matches!(self, Self::Sleep)
    }
}

/// What a waveform window collected while the EPD was busy.
struct WaveOutcome<Id> {
    /// First input that needs applying once the refresh completes.
    deferred: Option<DeferredAction<Id>>,
    /// A power-long-press arrived during the waveform.
    sleep: bool,
}

/// What closes a refresh session once its waveform has settled.
///
/// Both render paths derive this from the same three inputs (whether
/// the frame was overtaken mid-waveform, the `text_aa` setting, and
/// the app's `GrayscaleMode`) and then interpret it with their own
/// closers, so the partial and full paths cannot drift apart. Each
/// path writes the deferred-AA arm exactly once, from the plan.
enum ClosePlan {
    /// The frame was overtaken: skip phase 3 (partial) or the AA pass
    /// (full), and cancel any armed deferred fire.
    Abandon,
    /// Plain phase 3, nothing more.
    SyncRed,
    /// Grayscale AA pass in place of phase 3.
    GrayNow,
    /// Phase 3 now, deferred AA fire at the given instant.
    SyncThenArm(Instant),
}

impl ClosePlan {
    fn decide(interrupted: bool, aa_enabled: bool, mode: GrayscaleMode) -> Self {
        if interrupted {
            return Self::Abandon;
        }
        match mode {
            GrayscaleMode::Immediate if aa_enabled => Self::GrayNow,
            GrayscaleMode::Deferred if aa_enabled => {
                Self::SyncThenArm(Instant::now() + DEFERRED_GRAYSCALE_DELAY)
            }
            _ => Self::SyncRed,
        }
    }

    /// The deferred-AA arm this plan implies; every other plan cancels.
    #[inline]
    const fn aa_arm(&self) -> Option<Instant> {
        match self {
            Self::SyncThenArm(at) => Some(*at),
            _ => None,
        }
    }
}

/// Select arm for the background worker's result channel.
///
/// Armed only while the app layer reported `WaitingExternal`: armed
/// while idle, a stale-generation result nobody consumes would keep
/// the select permanently ready. Parking on a never-ready future
/// otherwise lets both park sites keep a single select instead of two
/// spellings of the same one; the branch monomorphizes away.
async fn worker_arm(waiting: bool) {
    if waiting {
        crate::kernel::work_queue::result_ready().await
    } else {
        core::future::pending::<()>().await
    }
}

/// Where a restored session was read from.
enum SessionSource {
    Rtc,
    Sd,
}

impl SessionSource {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Rtc => "RTC",
            Self::Sd => "SD",
        }
    }
}

impl super::Kernel {
    // render boot console to EPD; call before boot() to show
    // hardware init progress in the built-in mono font
    pub async fn show_boot_console(&mut self, console: &super::BootConsole) {
        let draw = |s: &mut StripBuffer| console.draw(s);
        if self.screen.render_full(&draw).await.is_err() {
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
        RtcSession::load_from_sd(&self.svc.sd).is_some()
    }

    // load the saved session: RTC first (a volatile read), SD second.
    //
    // on battery wake the brownout detector fires during the voltage
    // sag, causing a full system reset that wipes RTC FAST memory; the
    // SD-backed copy survives that and provides reliable resume.
    fn load_session(&self) -> Option<(super::rtc_session::RtcSession, SessionSource)> {
        use super::rtc_session::RtcSession;

        if let Some(session) = RtcSession::rtc_take() {
            return Some((session, SessionSource::Rtc));
        }
        RtcSession::load_from_sd(&self.svc.sd).map(|s| (s, SessionSource::Sd))
    }

    // one-time boot: load caches, settings, render the home screen
    // if waking from deep sleep with valid RTC session, restore it
    pub async fn boot<A: AppLayer>(&mut self, app_mgr: &mut A) {
        let boot_start = Instant::now();

        // log reset reason for debugging RTC session persistence
        let reset_reason = {
            use esp_hal::rtc_cntl::{SocResetReason, reset_reason};
            use esp_hal::system::Cpu;
            let reason = reset_reason(Cpu::ProCpu);
            info!("boot: reset reason = {:?}", reason);
            // on battery wake the power button feeds the MCU directly
            // and the sag trips the brownout detector, so a battery
            // wake reads SysBrownOut (or PowerOn once the latch is
            // released in sleep) rather than CoreDeepSleep; the SD
            // session copy covers both
            if matches!(reason, Some(SocResetReason::SysBrownOut)) {
                info!("boot: WARNING brownout reset detected (RTC memory may be lost)");
            }
            reason
        };

        let t0 = Instant::now();
        self.svc.bm_cache.ensure_loaded(&self.svc.sd);
        let bm_ms = t0.elapsed().as_millis();
        info!("boot: bookmark cache loaded ({}ms)", bm_ms);

        let t0 = Instant::now();
        let loaded_session = self.load_session();
        match &loaded_session {
            Some((session, src)) => info!(
                "boot: {} session valid (wake count {}) ({}ms)",
                src.as_str(),
                session.wake_count(),
                t0.elapsed().as_millis()
            ),
            None => info!("boot: no session (power-on or first boot)"),
        }
        super::power_log::boot(
            &self.svc.sd,
            self.svc.sd_ok,
            &reset_reason,
            loaded_session
                .as_ref()
                .map_or("none", |(_, src)| src.as_str()),
            loaded_session.as_ref().map(|(s, _)| s.wake_count()),
            self.svc.cached_battery_mv,
        );

        // load settings from SD
        let t0 = Instant::now();
        {
            let mut handle = self.handle();
            app_mgr.load_eager_settings(&mut handle);
        }
        info!("boot: settings loaded ({}ms)", t0.elapsed().as_millis());

        // only load home recent data if we're not restoring into a
        // different app — saves SD I/O when waking directly to reader.
        // the raw stack byte goes through the app layer's own decoder,
        // so the kernel never spells the distro's Home variant itself
        let skip_home_load = loaded_session.as_ref().is_some_and(|(s, _)| {
            // active app is the top of the stack
            s.nav_depth > 0
                && A::Id::from_raw(s.nav_stack[(s.nav_depth - 1) as usize]) != A::Id::HOME
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
        self.svc.idle_timeout_mins = app_mgr.system_settings().sleep_timeout;
        self.screen
            .set_sunlight_mode(app_mgr.system_settings().sunlight_fix);
        self.svc
            .applied
            .init_from(app_mgr.settings_generation(), app_mgr.system_settings());
        self.svc.log_stats();

        // try to restore session from RTC memory
        let t0 = Instant::now();
        let restored = if let Some((session, _)) = loaded_session {
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
            if self.screen.render_full(&draw).await.is_err() {
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

    // deadline-driven main loop; never returns
    //
    // steady-state suspension points:
    //   1. the park select at the bottom (input / worker result /
    //      earliest deadline); truly idle, the loop wakes at most
    //      every STATUS_INTERVAL_SECS for the stats log
    //   2. EPD busy pin wait inside render()
    //   3. yield_now between background steps (executor fairness)
    // everything between them is synchronous function calls
    pub async fn run<A: AppLayer>(&mut self, app_mgr: &mut A) -> ! {
        // re-arm from here rather than Kernel::new so the initial
        // housekeeping delay counts from UI-ready, not from before the
        // multi-second boot sequence
        self.svc.hk = super::HousekeepingDeadlines::starting_now();
        self.svc.last_activity = Instant::now();

        loop {
            if app_mgr.needs_special_mode() {
                self.handle_special_mode(app_mgr).await;
                continue;
            }

            // drain queued input; a burst of keypresses coalesces into
            // one pass over the loop body instead of one render each
            while let Ok(ev) = tasks::INPUT_EVENTS.try_receive() {
                if matches!(ev, Event::LongPress(_)) {
                    debug!("scheduler: received {:?}", ev);
                }
                let _ = self.dispatch_input(ev, app_mgr).await;
                if app_mgr.needs_special_mode() {
                    break;
                }
            }

            if app_mgr.needs_special_mode() {
                continue;
            }

            // idle sleep by deadline (checked only here, never during a
            // waveform)
            if self.svc.idle_deadline().is_some_and(|d| Instant::now() >= d) {
                self.sleep_with_session(app_mgr, "idle timeout").await;
                self.svc.last_activity = Instant::now();
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
            //      wave_window runs SD I/O because the
            //      EPD charge pump is driving pixels with no SPI commands
            //   4. no SD I/O outside these three sites
            //
            // background steps are bounded and sync; between steps we
            // poll for input so the user can interrupt long-running
            // multi-step operations (e.g. chapter caching). the
            // yield_now lets input_task/worker_task actually run during
            // a long Progress chain; without an await point they starve
            // on the cooperative executor
            let mut outcome;
            'bg: loop {
                outcome = {
                    let mut handle = self.handle();
                    app_mgr.run_background_step(&mut handle, super::app::BgBudget::new())
                };

                // check for pending input between steps
                if let Ok(ev) = tasks::INPUT_EVENTS.try_receive() {
                    if self.dispatch_input(ev, app_mgr).await.wants_sleep() {
                        // sleep returns; restart main loop
                        break 'bg;
                    }
                    if app_mgr.needs_special_mode() {
                        break 'bg;
                    }
                }

                match outcome {
                    super::app::BgOutcome::Progress { more: true } => {
                        // paint before draining the rest of the chain:
                        // on a cold book open the first page (and the
                        // loading percentages before it) would otherwise
                        // wait on caching work unrelated to showing it.
                        // no throughput loss: the render's wave_window
                        // keeps stepping this chain during the waveform,
                        // and the post-render Progress check re-enters
                        // this loop
                        if app_mgr.ctx_mut().render_ready() {
                            break 'bg;
                        }
                        embassy_futures::yield_now().await;
                        continue 'bg;
                    }
                    _ => break 'bg,
                }
            }

            if app_mgr.needs_special_mode() {
                continue;
            }

            self.svc.run_due_housekeeping();

            // generation-based settings propagation: only re-apply
            // hardware state when the app layer signals a change
            let settings_gen = app_mgr.settings_generation();
            if settings_gen != self.svc.applied.generation {
                let swap_changed = self.svc.applied.sync(
                    settings_gen,
                    app_mgr.system_settings(),
                    &mut self.screen,
                );
                if swap_changed {
                    app_mgr.on_swap_buttons_changed(self.svc.applied.swap_buttons);
                }
                self.svc.idle_timeout_mins = self.svc.applied.sleep_timeout;
            }

            // push live chrome state into the app layer so the top
            // status bar shows up-to-date numbers.
            let pct = crate::drivers::battery::battery_percentage(self.svc.cached_battery_mv);
            let day_pages = self.svc.day_stats.pages();
            let day_secs = self.svc.day_stats.secs_today();
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
                if self.render(app_mgr, redraw).await.wants_sleep() {
                    self.sleep_with_session(app_mgr, "power held").await;
                    continue;
                }
            }

            // deferred grayscale fire: when the active app uses
            // `GrayscaleMode::Deferred` the previous render armed
            // `svc.aa`. fire the AA pass once the idle window has
            // elapsed and the screen is still settled (no pending
            // redraw, AA setting still on, no power-down in progress).
            // the park below wakes at the armed instant, so fire
            // latency is near zero.
            if !app_mgr.system_settings().text_aa
                || !matches!(app_mgr.grayscale_mode(), GrayscaleMode::Deferred)
            {
                self.svc.aa.cancel();
            } else if !app_mgr.has_redraw() && self.svc.aa.take_due(Instant::now()) {
                self.fire_deferred_grayscale(app_mgr).await;
            }

            // re-run the loop instead of parking when work is already
            // pending: an interrupted Progress chain (sleep/special
            // exit above) or a redraw made ready by a deferred action
            // applied during the render's waveform
            if matches!(outcome, super::app::BgOutcome::Progress { more: true })
                || app_mgr.ctx_mut().render_ready()
            {
                continue;
            }

            // nothing to draw: this is the only place the panel rails
            // are dropped, so a pending redraw or armed AA fire (both
            // handled above) never pays a booster start
            self.power_off_panel_if_idle();

            // park until input, a worker result (only while the app
            // layer is waiting on one), or the earliest deadline. the
            // status-log cadence bounds the park at STATUS_INTERVAL_SECS,
            // which doubles as the backstop for the worker's silent
            // stale-generation skip (no result is posted for those)
            let mut deadline = self.svc.hk.earliest();
            if let Some(d) = self.svc.idle_deadline() {
                deadline = deadline.min(d);
            }
            if let Some(d) = app_mgr.ctx_mut().next_render_deadline() {
                deadline = deadline.min(d);
            }
            if let Some(d) = self.svc.aa.deadline() {
                deadline = deadline.min(d);
            }
            if let Some(d) = self.panel_idle_off_due() {
                deadline = deadline.min(d);
            }

            let ev = select3(
                tasks::INPUT_EVENTS.receive(),
                worker_arm(matches!(outcome, super::app::BgOutcome::WaitingExternal)),
                Timer::at(deadline),
            )
            .await;
            if let Either3::First(ev) = ev {
                let _ = self.dispatch_input(ev, app_mgr).await;
            }
        }
    }

    // run one hardware event through the input policy and take the
    // sleep it may ask for. the outcome is reported back because the
    // background-chain caller restarts the main loop afterwards while
    // the other two carry on
    async fn dispatch_input<A: AppLayer>(&mut self, ev: Event, app_mgr: &mut A) -> AfterInput {
        let after = self.svc.handle_input(ev, app_mgr);
        if after.wants_sleep() {
            self.sleep_with_session(app_mgr, "power held").await;
        }
        after
    }

    // delegate to app layer for modes that bypass normal dispatch
    // (e.g. wifi upload); kernel passes hardware resources through
    async fn handle_special_mode<A: AppLayer>(&mut self, app_mgr: &mut A) {
        let exit = app_mgr
            .run_special_mode(&mut self.screen, &self.svc.sd)
            .await;

        app_mgr.apply_transition(exit.transition(), &mut self.handle());

        // post-condition: needs_special_mode() is a query over app-layer
        // state the mode does not own, so a refused transition would put
        // the loop straight back into the mode. ModeExit cannot spell a
        // refusable one, but the launcher is free to decline any
        // transition, so prove the mode is really over.
        if app_mgr.needs_special_mode() {
            log::warn!("special mode still active after its exit; forcing home");
            app_mgr.apply_transition(Transition::Home, &mut self.handle());
        }
        app_mgr.request_full_redraw();
        // a long special mode (wifi upload) must not be followed by an
        // immediate idle sleep computed from pre-upload activity
        self.svc.last_activity = Instant::now();
        self.svc.last_refresh = Instant::now();
    }

    // drop the panel rails once nothing has been refreshed for
    // PANEL_IDLE_OFF_SECS; the latch that makes consecutive page turns
    // fast otherwise keeps the booster running for the whole awake
    // session. runs only from the parked main loop, never mid-session
    fn panel_idle_off_due(&self) -> Option<Instant> {
        self.screen.panel_powered().then(|| {
            self.svc.last_refresh + Duration::from_secs(super::timing::PANEL_IDLE_OFF_SECS)
        })
    }

    fn power_off_panel_if_idle(&mut self) {
        if self.panel_idle_off_due().is_some_and(|d| Instant::now() >= d) {
            let t0 = Instant::now();
            if self.screen.power_off_idle() {
                info!(
                    "display: panel rails off after {}s idle ({}ms)",
                    super::timing::PANEL_IDLE_OFF_SECS,
                    t0.elapsed().as_millis()
                );
            }
        }
    }
}

impl super::Services {
    /// Shared helper: run a hardware event through the input policy and
    /// dispatch forwarded raw events to the app layer.
    ///
    /// Both `handle_input` (normal path) and `wave_window`
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

    fn handle_input<A: AppLayer>(&mut self, hw_event: Event, app_mgr: &mut A) -> AfterInput {
        self.last_activity = Instant::now();
        self.input_events = self.input_events.wrapping_add(1);

        match self.resolve_input(hw_event, app_mgr, false) {
            InputResult::Sleep => AfterInput::Sleep,
            InputResult::Transition(t) => {
                app_mgr.apply_transition(t, &mut self.handle());
                tasks::request_hold_reset();
                AfterInput::Continue
            }
            InputResult::OverlayChanged => {
                tasks::request_hold_reset();
                AfterInput::Continue
            }
            InputResult::Semantic(input) => {
                let t = app_mgr.dispatch_semantic(input);
                if t != Transition::None {
                    app_mgr.apply_transition(t, &mut self.handle());
                }
                AfterInput::Continue
            }
            InputResult::Nothing => AfterInput::Continue,
        }
    }

    // shared housekeeping body: battery, sd probe, bookmark flush,
    // stats. each slot re-arms itself when due (see Periodic) so a
    // 1.6s GC waveform cannot queue catch-up runs
    fn run_due_housekeeping(&mut self) {
        if let Some(mv) = tasks::BATTERY_MV.try_take() {
            self.cached_battery_mv = mv;
        }

        let now = Instant::now();

        if self.hk.sd_check.due(now) {
            self.sd_ok = self.sd.probe_ok();
        }

        if self.hk.bm_flush.due(now) {
            if self.bm_cache.is_dirty() {
                self.bm_cache.flush(&self.sd);
            }
            // day stats piggyback the bookmark cadence; an ungated
            // check here would run an SD write plus a FAT mtime lookup
            // on every page turn's keypress-to-render path (each
            // add_pages/add_secs sets the dirty bit)
            self.flush_day_stats();
        }

        if self.hk.status.due(now) {
            self.log_stats();
        }
    }

    // flush today's reading stats and re-derive today_key from the
    // file's now-advanced FAT mtime, so a same-session calendar
    // rollover is detected before the next boot. no-op when nothing is
    // dirty or the card is unusable. shared by the housekeeping
    // cadence and the sleep path, which would otherwise drift
    fn flush_day_stats(&mut self) {
        if !self.day_stats.is_dirty() || !self.sd_ok {
            return;
        }
        if let Err(e) = self.day_stats.flush(&self.sd) {
            log::warn!("daystats flush: {}", e);
            return;
        }
        if let Some(k) = self
            .sd
            .file_mtime_day_key_in_plump(super::daystats::DAYSTATS_FILE)
        {
            self.today_key = Some(k);
        }
    }

    // deadline for idle sleep, None when disabled. re-derived from
    // last_activity on demand, so re-applying an unchanged timeout
    // value cannot restart the countdown
    fn idle_deadline(&self) -> Option<Instant> {
        (self.idle_timeout_mins > 0)
            .then(|| self.last_activity + Duration::from_secs(self.idle_timeout_mins as u64 * 60))
    }
}

impl super::Kernel {
    // partial refreshes use DU waveform (~400 ms); after ghost_clear_every
    // partials, a full GC refresh (~600 ms at faked temp) clears ghosting
    //
    // reports Sleep if a power-long-press arrived during the waveform
    async fn render<A: AppLayer>(&mut self, app_mgr: &mut A, redraw: Redraw) -> AfterInput {
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

        let super::Kernel { screen, svc } = self;

        'render: {
            if let Redraw::Partial(r) = redraw {
                if !screen.ghost_clear_due(app_mgr.ghost_clear_every()) {
                    // the plan picks bw vs inv_red from its own plane
                    // state: a region overlapping gray left by an AA
                    // pass comes back RevertFirst, whose only path to
                    // a wave snaps the grays to their rails before the
                    // re-drive; everything else is Ready with the DU
                    // already running
                    let stale = screen.stale_region();
                    let t_write = Instant::now();
                    let wave = {
                        let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                        match screen.plan_partial(r, &draw) {
                            Ok(super::screen::PartialPlan::Ready(wave)) => Ok(wave),
                            Ok(super::screen::PartialPlan::RevertFirst(pending)) => {
                                pending.proceed(&draw).await
                            }
                            Err(e) => Err(e),
                        }
                    };

                    match wave {
                        Ok(mut wave) => {
                            #[cfg(feature = "perf")]
                            {
                                actual_mode = "partial";
                            }
                            let write_ms = t_write.elapsed().as_millis();
                            let redrive = wave.hard_redrive();
                            debug!(
                                "render: partial phase1 region={:?} redrive={} stale={:?} ({}ms)",
                                r, redrive, stale, write_ms
                            );
                            let t_wave = Instant::now();
                            let WaveOutcome { deferred, sleep } =
                                svc.wave_window(&mut wave, app_mgr).await;
                            sleep_requested = sleep;
                            let settled = wave.settle();
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
                                "partial write_ms={} wave_ms={} redrive={} region_x={} region_y={} region_w={} region_h={}",
                                write_ms,
                                wave_ms,
                                redrive,
                                r.x,
                                r.y,
                                r.w,
                                r.h
                            );

                            let plan = ClosePlan::decide(
                                app_mgr.has_redraw() || deferred.is_some(),
                                app_mgr.system_settings().text_aa,
                                app_mgr.grayscale_mode(),
                            );
                            // the plan is the only writer of the arm on
                            // this path; nothing below reads it
                            svc.aa.set(plan.aa_arm());

                            match plan {
                                ClosePlan::Abandon => {
                                    // skip phase 3 when content changed
                                    // mid-DU or a deferred action is queued
                                    // (the screen will be redrawn right
                                    // after); the next partial recovers the
                                    // desynchronised RED RAM via inv_red.
                                    //
                                    // discriminator for the page-turn AA
                                    // hunt: any turn logging this line
                                    // skipped its AA pass because a mark
                                    // landed mid-waveform
                                    crate::perf_event!(
                                        "render",
                                        "partial_abandon pending_redraw={} deferred={} region_x={} region_y={} region_w={} region_h={}",
                                        app_mgr.has_redraw(),
                                        deferred.is_some(),
                                        r.x,
                                        r.y,
                                        r.w,
                                        r.h
                                    );
                                    app_mgr.ctx_mut().mark_dirty(r);
                                    settled.abandon();
                                }
                                ClosePlan::GrayNow => {
                                    // grayscale AA replaces phase 3; the
                                    // region stays marked stale so the next
                                    // partial touching it re-drives via
                                    // inv_red, which is the black starting
                                    // state the gray pulses assume
                                    let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                                    if settled.grayscale(&draw).await.is_err() {
                                        log::warn!(
                                            "render: grayscale_pass timed out, forcing full GC next frame"
                                        );
                                    }
                                }
                                ClosePlan::SyncRed | ClosePlan::SyncThenArm(_) => {
                                    let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                                    settled.sync_red(&draw);
                                }
                            }

                            if let Some(action) = deferred {
                                svc.apply_deferred_action(action, app_mgr);
                            }

                            break 'render;
                        }
                        Err(super::screen::PartialRejected::Empty) => break 'render,
                        Err(super::screen::PartialRejected::NeedsFull) => {
                            info!("display: partial failed (initial refresh), promoting to full");
                        }
                    }
                } else {
                    info!("display: promoted partial to full (ghosting clear)");
                }
            }

            if matches!(redraw, Redraw::Full | Redraw::Partial(_)) {
                svc.log_stats();

                let t_write = Instant::now();
                let mut wave = {
                    let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                    screen.begin_full(&draw)
                };
                #[cfg(feature = "perf")]
                {
                    actual_mode = "full";
                }
                let write_ms = t_write.elapsed().as_millis();
                debug!("render: full frame written ({}ms)", write_ms);

                let t_wave = Instant::now();
                let WaveOutcome { deferred, sleep } = svc.wave_window(&mut wave, app_mgr).await;
                sleep_requested = sleep;
                wave.settle().finish();
                let wave_ms = t_wave.elapsed().as_millis();
                debug!(
                    "render: full waveform done pending_redraw={} deferred={} sleep={} ({}ms)",
                    app_mgr.has_redraw(),
                    deferred.is_some(),
                    sleep,
                    wave_ms
                );
                crate::perf_event!("render", "full write_ms={} wave_ms={}", write_ms, wave_ms);

                // after a full GC refresh the panel is left in plain BW.
                // re-apply grayscale AA per the current `GrayscaleMode`
                // (or arm the deferred timer). the pass marks the screen
                // stale, so later partials re-drive the area they touch
                // via inv_red.
                //
                // finish() already closed the session, so the plan's
                // sync arms carry no work here: only GrayNow has a
                // closer, and Abandon just means "skip the AA pass"
                let plan = ClosePlan::decide(
                    app_mgr.has_redraw() || deferred.is_some(),
                    app_mgr.system_settings().text_aa,
                    app_mgr.grayscale_mode(),
                );
                svc.aa.set(plan.aa_arm());

                if matches!(plan, ClosePlan::GrayNow) {
                    let draw = |s: &mut StripBuffer| app_mgr.draw(s);
                    if screen.grayscale_full(&draw).await.is_err() {
                        log::warn!(
                            "render: post-GC grayscale_pass timed out, forcing full GC next frame"
                        );
                    }
                }

                if let Some(action) = deferred {
                    svc.apply_deferred_action(action, app_mgr);
                }
            }
        } // 'render

        svc.last_refresh = Instant::now();
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

        AfterInput::sleep_if(sleep_requested)
    }

    // run a grayscale_pass over everything refreshed since the last
    // one, on top of an already-rendered BW image. fires only from the
    // main loop after the deferred-AA timer expires; panel power is
    // normally still latched on from the last partial DU, so the pass
    // skips the booster start entirely.
    async fn fire_deferred_grayscale<A: AppLayer>(&mut self, app_mgr: &mut A) {
        let draw = |s: &mut StripBuffer| app_mgr.draw(s);
        let t0 = Instant::now();
        let res = self.screen.grayscale_fresh(&draw).await;
        self.svc.last_refresh = Instant::now();
        if res.is_err() {
            log::warn!(
                "render: deferred grayscale_pass timed out, forcing full GC next frame"
            );
            return;
        }
        // the screen is now marked stale: the panel holds gray levels
        // the RAM planes cannot express, so the next partial re-drives
        // the region it touches instead of computing a delta. recovery
        // is scoped to that region, so a small mark stays small
        debug!(
            "render: deferred grayscale_pass complete ({}ms)",
            t0.elapsed().as_millis()
        );
    }

}

impl super::Services {
    // collect input and run background work while the EPD waveform is
    // in flight
    //
    // during the DU/GC waveform the EPD charge pump drives pixels;
    // no SPI commands are sent, so the bus is free for SD I/O. the
    // Wave session holds the screen half of the kernel, this method
    // holds the services half; the borrow checker enforces the bus
    // invariant.
    //
    // background work runs as bounded sync steps via
    // run_background_step; while it reports Progress the steps run
    // back-to-back (yield between them for executor fairness). once
    // the app is out of work, park on the busy-pin edge instead of
    // polling, so waveform completion wakes us immediately.
    //
    // first deferred action wins; hold reset prevents the held
    // button from re-firing LongPress/Repeat for the waveform
    //
    // the outcome carries the deferred action and whether a
    // power-long-press arrived, so the caller can sleep after the EPD
    // finishes
    async fn wave_window<A: AppLayer, M>(
        &mut self,
        wave: &mut super::screen::Wave<'_, M>,
        app_mgr: &mut A,
    ) -> WaveOutcome<A::Id> {
        // derived from the driver's own busy-pin bound; without a timed
        // arm in the parks, a stuck-high busy pin would park the whole
        // loop forever
        const WAVEFORM_GUARD: Duration =
            Duration::from_millis(crate::drivers::ssd1677::BUSY_TIMEOUT_MS);
        let guard_at = Instant::now() + WAVEFORM_GUARD;

        let mut deferred: Option<DeferredAction<A::Id>> = None;
        let mut sleep_requested = false;

        loop {
            if !wave.is_busy() {
                break;
            }
            if Instant::now() >= guard_at {
                log::error!("wave_window: waveform guard timeout (busy pin stuck high)");
                break;
            }

            // run one bounded background step, then poll for input.
            // quiet budget: a drawable-state change during the wave
            // would force the closing phase to abandon
            let outcome = {
                let mut handle = self.handle();
                app_mgr.run_background_step(&mut handle, super::app::BgBudget::quiet())
            };

            let ev = if let Ok(ev) = tasks::INPUT_EVENTS.try_receive() {
                Some(ev)
            } else {
                match outcome {
                    super::app::BgOutcome::Progress { more: true } => {
                        // more work queued; just let other tasks run
                        embassy_futures::yield_now().await;
                        None
                    }
                    _ => {
                        match select4(
                            wave.until_idle(),
                            tasks::INPUT_EVENTS.receive(),
                            worker_arm(matches!(
                                outcome,
                                super::app::BgOutcome::WaitingExternal
                            )),
                            Timer::at(guard_at),
                        )
                        .await
                        {
                            Either4::Second(ev) => Some(ev),
                            _ => None,
                        }
                    }
                }
            };

            if let Some(hw_event) = ev {
                self.last_activity = Instant::now();
                self.input_events = self.input_events.wrapping_add(1);
                let suppress = app_mgr.suppress_deferred_input();

                match self.resolve_input(hw_event, app_mgr, suppress) {
                    InputResult::Sleep => {
                        info!("wave_window: sleep requested during waveform, will sleep after");
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

            // SD I/O is legal here: the charge pump is driving pixels
            // with no SPI traffic. idle sleep is never taken mid-waveform
            // (the deadline check lives in the main loop only)
            self.run_due_housekeeping();
        }

        WaveOutcome {
            deferred,
            sleep: sleep_requested,
        }
    }
}

impl super::Kernel {
    // save session to RTC memory + SD card and enter deep sleep.
    //
    // RTC FAST memory is the fast path but is lost on battery wake
    // due to brownout resets (voltage sag wipes the RTC power domain).
    // the SD copy is the reliable fallback (~20ms extra).
    async fn sleep_with_session<A: AppLayer>(&mut self, app_mgr: &mut A, reason: &str) {
        use super::rtc_session::RtcSession;

        let sleep_start = Instant::now();

        // about to deep-sleep, cancel any pending grayscale-AA fire so
        // it doesn't run with stale state on wake.
        self.svc.aa.cancel();

        // save active app state (reader position) to bookmark cache
        // before collecting session, so bookmarks stay in sync
        app_mgr.save_active_state(&mut *self.svc.bm_cache);

        // day-stats only flushes on the 30s bookmark cadence now, so
        // up to 30s of reading stats would be lost without this
        self.svc.flush_day_stats();

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
        session.save_to_sd(&self.svc.sd);
        info!(
            "sleep: session saved to RTC + SD ({}ms)",
            t0.elapsed().as_millis()
        );
        super::power_log::sleep(
            &self.svc.sd,
            self.svc.sd_ok,
            reason,
            &app_mgr.active(),
            super::uptime_secs() as u64,
            self.svc.input_events,
            self.svc.cached_battery_mv,
            session.wake_count(),
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
    async fn enter_sleep(&mut self, reason: &str, sleep_start: Instant) {
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
        if self.svc.bm_cache.is_dirty() {
            self.svc.bm_cache.flush(&self.svc.sd);
        }
        info!("sleep: bookmark flush ({}ms)", t0.elapsed().as_millis());

        // load sleep wallpaper from SD before putting the card to sleep
        info!("sleep: loading wallpaper from SD...");
        let t0 = Instant::now();
        let sleep_img = super::sleep_image::load_sleep_image(&self.svc.sd);
        if sleep_img.is_some() {
            info!("sleep: wallpaper loaded ({}ms)", t0.elapsed().as_millis());
        } else {
            info!("sleep: no wallpaper found ({}ms)", t0.elapsed().as_millis());
        }

        info!("sleep: putting SD card to sleep...");
        let t0 = Instant::now();
        self.svc.sd_card_sleep();
        info!("sleep: SD card sleep ({}ms)", t0.elapsed().as_millis());

        let t0 = Instant::now();
        if let Some(ref img) = sleep_img {
            // render 4-level grayscale wallpaper via dual-plane grayscale pass
            use super::sleep_image::CHUNK_COUNT;

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
            if self.screen.render_full(&draw).await.is_err() {
                log::warn!("sleep: wallpaper base refresh timed out, continuing");
            }
            info!("sleep: wallpaper base BW ({}ms)", t1.elapsed().as_millis());

            info!("sleep: rendering wallpaper grayscale overlay...");
            let t1 = Instant::now();
            // grayscale_full writes LSB plane to BW RAM and MSB plane to
            // RED RAM, then triggers a single refresh with the grayscale LUT.
            if self.screen.grayscale_full(&draw).await.is_err() {
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
                .screen
                .render_full(&|s: &mut StripBuffer| {
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
        self.screen.enter_deep_sleep();
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

        // last step before the MCU goes down: on battery the board
        // loses power right here (the vendor firmware's sleep does the
        // same), so every SD write and the panel sleep above must be
        // done; on USB the deep sleep below still runs and GPIO3 wakes
        // it as before
        info!("mcu: releasing battery latch (GPIO13 low), entering deep sleep (power button to wake)");
        crate::board::battery_latch_off();
        rtc.sleep(&sleep_config, &[&rtcio]);

        // deep sleep resets the MCU; backstop if sleep returns
        #[allow(unreachable_code)]
        loop {
            core::hint::spin_loop();
        }
    }

}

impl super::Services {
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
        let hwm = crate::ui::stack_hwm_detail();
        // idle seconds and the countdown to idle sleep make a phantom
        // input source (a noisy ladder resetting the timer) visible
        // as a countdown that never reaches zero
        let now = Instant::now();
        let idle_secs = now.saturating_duration_since(self.last_activity).as_secs();
        let sleep_in = self
            .idle_deadline()
            .map(|d| d.saturating_duration_since(now).as_secs());

        info!(
            "stats: heap {}/{}K peak {}K | stack free {}K hwm {}K | bat {}% {}.{}V | up {}:{:02} | SD:{} | idle {}s sleep_in {}s inputs {}",
            stats.current_usage / 1024,
            stats.size / 1024,
            stats.max_usage / 1024,
            free_stack_bytes() / 1024,
            hwm.hwm / 1024,
            bat_pct,
            self.cached_battery_mv / 1000,
            (self.cached_battery_mv % 1000) / 100,
            hrs,
            mins,
            if self.sd_ok { "ok" } else { "--" },
            idle_secs,
            sleep_in.unwrap_or(0),
            self.input_events,
        );

        // one extra line each time the water mark grows: a real deep
        // call chain leaves no intact canary above the lowest break,
        // while a large intact span means a stray write is inflating
        // the reading (see the stack spike investigation)
        static LAST_HWM: critical_section::Mutex<core::cell::Cell<usize>> =
            critical_section::Mutex::new(core::cell::Cell::new(0));
        let grew = critical_section::with(|cs| {
            let last = LAST_HWM.borrow(cs);
            let grew = hwm.hwm > last.get();
            if grew {
                last.set(hwm.hwm);
            }
            grew
        });
        if grew {
            info!(
                "stack: hwm {}B break@{:#010x} intact-above {}B{}",
                hwm.hwm,
                hwm.break_addr,
                hwm.intact_above,
                if hwm.intact_above > 4096 {
                    " (stray write suspected)"
                } else {
                    ""
                },
            );
        }
    }
}
