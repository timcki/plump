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
pub mod rtc_session;
pub mod scheduler;
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
pub use wake::uptime_secs;

use esp_hal::delay::Delay;

use crate::board::Epd;
use crate::drivers::sdcard::SdStorage;
use crate::drivers::strip::StripBuffer;
use crate::kernel::daystats::DayStats;
use crate::kernel::dir_cache::DirCache;
use crate::ui::Theme;

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
        epd: &mut crate::board::Epd,
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
            epd.set_sunlight_mode(ss.sunlight_fix);
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

pub struct Kernel {
    pub(crate) sd: SdStorage,
    pub(crate) dir_cache: &'static mut DirCache,
    pub(crate) bm_cache: &'static mut BookmarkCache,
    pub(crate) epd: Epd,
    pub(crate) strip: &'static mut StripBuffer,
    pub(crate) delay: Delay,
    pub(crate) sd_ok: bool,
    pub(crate) cached_battery_mv: u16,
    pub(crate) partial_refreshes: u32,

    // true when RED RAM is out of sync with BW after a skipped
    // phase3_sync (rapid navigation); next partial uses inv_red
    pub(crate) red_stale: bool,

    // armed by the render path when the active app uses
    // `GrayscaleMode::Deferred`. the main loop fires the AA pass once
    // this instant has passed AND no new redraw is pending. cleared on
    // every render (re-armed if the new frame still wants Deferred),
    // on sleep, and when `text_aa` is toggled off.
    pub(crate) aa_deferred_at: Option<embassy_time::Instant>,

    // power-button policy state machine; resolves raw power events
    // into semantic inputs (MenuTap) or sleep requests
    pub(crate) input_policy: input_policy::InputPolicyState,

    // last settings values pushed to hardware; used for
    // generation-based diffing in the scheduler main loop
    pub(crate) applied: AppliedSettings,

    // design tokens shared by chrome + apps; one instance per kernel
    pub(crate) theme: Theme,

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

pub(crate) struct HousekeepingDeadlines {
    pub status_at: embassy_time::Instant,
    pub sd_check_at: embassy_time::Instant,
    pub bm_flush_at: embassy_time::Instant,
}

impl HousekeepingDeadlines {
    // initial delay lets boot settle; the bookmark flush keeps its 2s
    // stagger off the SD check so the two never coincide
    pub fn starting_now() -> Self {
        let now = embassy_time::Instant::now();
        let initial = embassy_time::Duration::from_secs(timing::HOUSEKEEPING_INITIAL_DELAY_SECS);
        Self {
            status_at: now + initial,
            sd_check_at: now + initial,
            bm_flush_at: now
                + initial
                + embassy_time::Duration::from_secs(timing::BOOKMARK_FLUSH_STAGGER_SECS),
        }
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
            sd,
            dir_cache,
            bm_cache,
            epd,
            strip,
            delay,
            sd_ok,
            cached_battery_mv: battery_mv,
            partial_refreshes: 0,
            red_stale: false,
            aa_deferred_at: None,
            input_policy: input_policy::InputPolicyState::new(),
            applied: AppliedSettings::new(),
            theme: Theme::default_v1(),
            day_stats,
            today_key,
            hk: HousekeepingDeadlines::starting_now(),
            last_activity: embassy_time::Instant::now(),
            idle_timeout_mins: 0,
        }
    }

    #[inline]
    pub fn handle(&mut self) -> KernelHandle<'_> {
        KernelHandle::new(self)
    }

    #[inline]
    pub fn set_battery_mv(&mut self, mv: u16) {
        self.cached_battery_mv = mv;
    }

    #[inline]
    pub fn reset_partial_count(&mut self) {
        self.partial_refreshes = 0;
        self.red_stale = false;
    }

    #[inline]
    pub fn bump_partial_count(&mut self) {
        self.partial_refreshes += 1;
    }
}
