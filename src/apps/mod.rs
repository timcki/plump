// app modules, AppId definition, and re-exports from kernel::app
//
// AppId is defined here (the distro side); the kernel attempts to be
// generic. Modal + the Tab / Nav type aliases are reserved for the
// rebuild: chunks B.3+ migrate `on_event` away from `Transition<AppId>`
// to `NavCmd<Tab, Modal>`.

pub mod cover_cache;
pub mod cover_placeholder;
pub mod files;
pub mod home;
pub mod library;
pub mod manager;
pub mod reader;
pub mod stats;
pub mod tab;
pub mod widgets;

pub mod settings;
pub mod upload;

use crate::kernel::app::AppIdType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppId {
    Home,
    /// Library tab (chunk J). The flat file list lives on `Files`
    /// until chunk N deletes it.
    Library,
    /// Legacy flat file list; no longer reachable from the tab bar
    /// but kept in the enum until chunk N for binary compat with
    /// older RTC sessions.
    Files,
    Reader,
    Settings,
    Stats,
    // upload bypasses the App trait; AppManager::needs_special_mode
    // returns true for this variant and run_special_mode handles it
    Upload,
}

impl AppIdType for AppId {
    const HOME: Self = Self::Home;
}

pub type Transition = crate::kernel::app::Transition<AppId>;
pub type NavEvent = crate::kernel::app::NavEvent<AppId>;
pub type Launcher = crate::kernel::app::Launcher<AppId>;

pub use crate::kernel::app::{
    App, AppContext, BgBudget, BgOutcome, DeferredPersistenceReason, PendingSetting, RECENT_FILE,
    Redraw,
};

// unified error types
pub use crate::kernel::{Error, ErrorKind, Result, ResultExt};

// backward-compatible alias
pub use crate::kernel::StorageError;

// ── nav rebuild types (unused until chunk B.3+) ───────────────────────

pub use tab::Tab;

/// Modal slot kinds: only Reader for now. Files is being retired
/// (deleted in chunk N); Library replaces it as a tab.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Modal {
    Reader,
}

pub type AppNavCmd = plump_kernel::kernel::nav::NavCmd<Tab, Modal>;
pub type AppNavEvent = plump_kernel::kernel::nav::NavEvent<Tab, Modal>;
pub type AppNavSlot = plump_kernel::kernel::nav::NavSlot<Tab, Modal>;
pub type AppNav = plump_kernel::kernel::nav::Nav<Tab, Modal>;

pub use plump_kernel::kernel::nav::{HDir, HResult};
