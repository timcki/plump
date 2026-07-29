// kernel: owns hardware resources, caches, and system state
//
// constructed once during boot in main(), lives for the lifetime of
// the program; not a separate Embassy task -- a struct held by main
//
// apps interact exclusively through KernelHandle, which borrows the
// kernel for the duration of an async lifecycle method

pub mod app;
pub mod bookmarks;
pub mod bundle;
pub mod config;
pub mod console;
pub mod daystats;
pub mod dir_cache;
pub mod handle;
pub mod input_policy;
pub mod nav;
pub mod plane_map;
pub mod rtc_session;
pub mod scheduler;
pub mod screen;
pub mod sleep_image;
pub mod tasks;
pub mod timing;
pub mod wake;
pub mod work_queue;

// Unified error types (primary home: crate::error)
pub use crate::error::{Error, ErrorKind, Result, ResultExt};

// backward-compatible alias
pub use crate::drivers::storage::StorageError;

pub use app::{
    App, AppContext, AppIdType, AppLayer, BgBudget, BgOutcome, Launcher, NavEvent, PendingSetting,
    QuickAction, QuickActionKind, RECENT_FILE, Redraw, Transition,
};
pub use bookmarks::BookmarkCache;
pub use console::BootConsole;
pub use handle::KernelHandle;
pub use input_policy::SemanticInput;
pub use screen::Screen;
pub use wake::uptime_secs;

use esp_hal::delay::Delay;

use crate::board::Epd;
use crate::drivers::sdcard::SdStorage;
use crate::drivers::strip::StripBuffer;
use crate::kernel::daystats::DayStats;
use crate::kernel::dir_cache::DirCache;

// default ghost-clear interval (overridden by settings once loaded)
pub const DEFAULT_GHOST_CLEAR_EVERY: u32 = 10;

/// Tracks the last settings values pushed to hardware so the
/// scheduler only re-applies when something actually changed.
///
/// Inspired by the SdCard pattern: encapsulate mutable state behind
/// a struct with explicit methods instead of scattering raw Signal
/// calls and ad-hoc comparisons across the scheduler.
pub(crate) struct AppliedSettings {
    pub generation: u32,
    pub sleep_timeout: u16,
    pub sunlight_fix: bool,
    pub swap_buttons: bool,
}

impl AppliedSettings {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            sleep_timeout: 0,
            sunlight_fix: false,
            swap_buttons: false,
        }
    }

    /// Compare against current settings and apply only changed fields
    /// to hardware.  Updates self and returns whether `swap_buttons`
    /// changed (the caller must propagate that to the app layer since
    /// `ButtonMapper` lives in the distro, not the kernel).
    pub fn sync(
        &mut self,
        generation: u32,
        ss: &config::SystemSettings,
        screen: &mut Screen,
    ) -> bool {
        let mut swap_changed = false;

        if self.sleep_timeout != ss.sleep_timeout {
            log::info!(
                "settings: sleep_timeout {} -> {}",
                self.sleep_timeout,
                ss.sleep_timeout
            );
            // the scheduler mirrors this into idle_timeout_mins after
            // sync; the idle deadline re-derives from last_activity so
            // a same-value write cannot restart the countdown
            self.sleep_timeout = ss.sleep_timeout;
        }

        if self.sunlight_fix != ss.sunlight_fix {
            log::info!(
                "settings: sunlight_fix {} -> {}",
                self.sunlight_fix,
                ss.sunlight_fix
            );
            screen.set_sunlight_mode(ss.sunlight_fix);
            self.sunlight_fix = ss.sunlight_fix;
        }

        if self.swap_buttons != ss.swap_buttons {
            log::info!(
                "settings: swap_buttons {} -> {}",
                self.swap_buttons,
                ss.swap_buttons
            );
            self.swap_buttons = ss.swap_buttons;
            swap_changed = true;
        }

        self.generation = generation;
        swap_changed
    }

    /// Snapshot current settings without diffing; used at boot when
    /// the initial idle-timeout / `set_sunlight_mode` values have
    /// already been applied and we just need to record them.
    pub fn init_from(&mut self, generation: u32, ss: &config::SystemSettings) {
        self.generation = generation;
        self.sleep_timeout = ss.sleep_timeout;
        self.sunlight_fix = ss.sunlight_fix;
        self.swap_buttons = ss.swap_buttons;
    }
}

// split so the scheduler can borrow both halves disjointly: a Wave
// (screen.rs) holds the screen half for the duration of a waveform
// while background work runs through a KernelHandle over the services
// half. the SPI bus-sharing invariant falls out of the borrow checker
pub struct Kernel {
    pub(crate) screen: Screen,
    pub(crate) svc: Services,
}

// everything the app-facing KernelHandle and background/housekeeping
// paths need; no display hardware in here
pub struct Services {
    pub(crate) sd: SdStorage,
    pub(crate) dir_cache: &'static mut DirCache,
    pub(crate) bm_cache: &'static mut BookmarkCache,
    pub(crate) sd_ok: bool,
    pub(crate) cached_battery_mv: u16,

    // deferred grayscale-AA fire; armed by the render path when the
    // active app uses `GrayscaleMode::Deferred`
    pub(crate) aa: DeferredAa,

    // power-button policy state machine; resolves raw power events
    // into semantic inputs (MenuTap) or sleep requests
    pub(crate) input_policy: input_policy::InputPolicyState,

    // last settings values pushed to hardware; used for
    // generation-based diffing in the scheduler main loop
    pub(crate) applied: AppliedSettings,

    // today's reading stats (pages + secs since calendar rollover).
    // owned static, loaded from `_PLUMP/DAYSTATS.BIN` at boot. mutated
    // by the active reader; flushed by housekeeping when dirty.
    pub(crate) day_stats: &'static mut DayStats,

    // current day key (derived from FAT mtime of DAYSTATS.BIN). 0 when
    // the SD card has no usable wall clock (no battery-backed RTC).
    pub(crate) today_key: u32,

    // housekeeping deadlines, re-armed as now + interval when due (no
    // ticker catch-up bursts after a long EPD waveform)
    pub(crate) hk: HousekeepingDeadlines,

    // instant of the last received input event (plus special-mode
    // exit); the idle-sleep deadline derives from it on demand
    pub(crate) last_activity: embassy_time::Instant,

    // mirrored from applied.sleep_timeout; 0 disables idle sleep
    pub(crate) idle_timeout_mins: u16,
}

/// Armed deferred grayscale-AA fire.
///
/// The main loop fires the AA pass once the armed instant has passed
/// AND no new redraw is pending. Every render decides the arm exactly
/// once (see the scheduler's `ClosePlan`); sleep and a `text_aa`
/// toggle cancel it.
#[derive(Default)]
pub(crate) struct DeferredAa {
    at: Option<embassy_time::Instant>,
}

impl DeferredAa {
    pub const fn new() -> Self {
        Self { at: None }
    }

    /// Arm for `at`, or cancel when the close plan carries no arm.
    #[inline]
    pub fn set(&mut self, at: Option<embassy_time::Instant>) {
        self.at = at;
    }

    #[inline]
    pub fn cancel(&mut self) {
        self.at = None;
    }

    /// Instant the park should wake at, so fire latency is near zero.
    #[inline]
    pub fn deadline(&self) -> Option<embassy_time::Instant> {
        self.at
    }

    /// Disarm and report true when the idle window has elapsed. Leaves
    /// the arm in place while it has not.
    #[inline]
    pub fn take_due(&mut self, now: embassy_time::Instant) -> bool {
        match self.at {
            Some(at) if now >= at => {
                self.at = None;
                true
            }
            _ => false,
        }
    }
}

/// A deadline that re-arms itself when it fires.
///
/// `due` pushes the next fire to `now + period` rather than
/// `at + period`, so a 1.6s GC waveform cannot leave a burst of
/// catch-up runs queued the way a ticker would.
#[derive(Clone, Copy)]
pub(crate) struct Periodic {
    at: embassy_time::Instant,
    period: embassy_time::Duration,
}

impl Periodic {
    pub fn new(first: embassy_time::Instant, period: embassy_time::Duration) -> Self {
        Self { at: first, period }
    }

    #[inline]
    pub fn due(&mut self, now: embassy_time::Instant) -> bool {
        if now < self.at {
            return false;
        }
        self.at = now + self.period;
        true
    }
}

pub(crate) struct HousekeepingDeadlines {
    pub status: Periodic,
    pub sd_check: Periodic,
    pub bm_flush: Periodic,
}

impl HousekeepingDeadlines {
    // initial delay lets boot settle; the bookmark flush keeps its 2s
    // stagger off the SD check so the two never coincide
    pub fn starting_now() -> Self {
        use embassy_time::{Duration, Instant};
        let now = Instant::now();
        let initial = Duration::from_secs(timing::HOUSEKEEPING_INITIAL_DELAY_SECS);
        Self {
            status: Periodic::new(
                now + initial,
                Duration::from_secs(timing::STATUS_INTERVAL_SECS),
            ),
            sd_check: Periodic::new(
                now + initial,
                Duration::from_secs(timing::SD_CHECK_INTERVAL_SECS),
            ),
            bm_flush: Periodic::new(
                now + initial + Duration::from_secs(timing::BOOKMARK_FLUSH_STAGGER_SECS),
                Duration::from_secs(timing::BOOKMARK_FLUSH_INTERVAL_SECS),
            ),
        }
    }

    /// Earliest of every slot. The park chain folds over this instead
    /// of listing the slots itself, so a new slot cannot silently miss
    /// the wake-up.
    pub fn earliest(&self) -> embassy_time::Instant {
        let mut e = self.status.at;
        for at in [self.sd_check.at, self.bm_flush.at] {
            e = e.min(at);
        }
        e
    }
}

impl Kernel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sd: SdStorage,
        epd: Epd,
        strip: &'static mut StripBuffer,
        dir_cache: &'static mut DirCache,
        bm_cache: &'static mut BookmarkCache,
        day_stats: &'static mut DayStats,
        delay: Delay,
        sd_ok: bool,
        battery_mv: u16,
    ) -> Self {
        // load today's stats from disk and derive today's key from the
        // file's FAT mtime. on a fresh SD or a card without an RTC, we
        // fall back to EMPTY + today_key=0 (counters accumulate from
        // boot, no rollover).
        let today_key = if sd_ok {
            sd.file_mtime_day_key_in_plump(daystats::DAYSTATS_FILE)
                .unwrap_or(0)
        } else {
            0
        };
        *day_stats = if sd_ok {
            DayStats::load(&sd)
        } else {
            DayStats::EMPTY
        };
        day_stats.rollover_if_new_day(today_key);

        Self {
            screen: Screen::new(epd, strip, delay),
            svc: Services {
                sd,
                dir_cache,
                bm_cache,
                sd_ok,
                cached_battery_mv: battery_mv,
                aa: DeferredAa::new(),
                input_policy: input_policy::InputPolicyState::new(),
                applied: AppliedSettings::new(),
                day_stats,
                today_key,
                hk: HousekeepingDeadlines::starting_now(),
                last_activity: embassy_time::Instant::now(),
                idle_timeout_mins: 0,
            },
        }
    }

    #[inline]
    pub fn handle(&mut self) -> KernelHandle<'_> {
        self.svc.handle()
    }
}

impl Services {
    #[inline]
    pub fn handle(&mut self) -> KernelHandle<'_> {
        KernelHandle::new(self)
    }
}
