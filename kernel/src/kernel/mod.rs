// kernel: owns hardware resources, caches, and system state
//
// constructed once during boot in main(), lives for the lifetime of
// the program; not a separate Embassy task -- a struct held by main
//
// apps interact exclusively through KernelHandle, which borrows the
// kernel for the duration of an async lifecycle method

pub mod app;
pub mod bookmarks;
pub mod config;
pub mod console;
pub mod dir_cache;
pub mod handle;
pub mod input_policy;
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
        epd: &mut crate::board::Epd,
    ) -> bool {
        let mut swap_changed = false;

        if self.sleep_timeout != ss.sleep_timeout {
            log::info!(
                "settings: sleep_timeout {} -> {}",
                self.sleep_timeout,
                ss.sleep_timeout
            );
            tasks::set_idle_timeout(ss.sleep_timeout);
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
    /// the initial `set_idle_timeout` / `set_sunlight_mode` have
    /// already been called and we just need to record what was applied.
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

    // power-button policy state machine; resolves raw power events
    // into semantic inputs (MenuTap) or sleep requests
    pub(crate) input_policy: input_policy::InputPolicyState,

    // last settings values pushed to hardware; used for
    // generation-based diffing in the scheduler main loop
    pub(crate) applied: AppliedSettings,
}

impl Kernel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sd: SdStorage,
        epd: Epd,
        strip: &'static mut StripBuffer,
        dir_cache: &'static mut DirCache,
        bm_cache: &'static mut BookmarkCache,
        delay: Delay,
        sd_ok: bool,
        battery_mv: u16,
    ) -> Self {
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
            input_policy: input_policy::InputPolicyState::new(),
            applied: AppliedSettings::new(),
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
