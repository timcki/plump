// app protocol: trait, context, transitions, redraw types, coalescing,
// and loading indicator state
//
// these types define the contract between the kernel scheduler and
// the app layer. concrete apps implement the App trait; the kernel
// drives lifecycle, input dispatch, and rendering through it.
//
// the kernel is generic over an app identity type (AppIdType).
// distros define their own AppId enum and implement AppIdType for
// it. the kernel never knows which specific apps exist.
//
// QuickAction types also live here - they are pure data describing
// what actions an app exposes; the renderer (QuickMenu widget) is
// app-side, but the protocol is kernel-side

use embassy_time::Instant;

use crate::board::action::ActionEvent;
use crate::drivers::input::Event;
use crate::drivers::sdcard::SdStorage;
use crate::drivers::strip::StripBuffer;
use crate::kernel::input_policy::SemanticInput;
use crate::kernel::nav::{HDir, HResult};
use crate::ui::Region;
use crate::ui::stack_fmt::StackFmt;
use crate::util::FixedStr;

use super::KernelHandle;
use super::bookmarks::BookmarkCache;
use super::config::SystemSettings;

pub const MAX_APP_ACTIONS: usize = 6;

// ── background-step contract ────────────────────────────────────────

/// Scheduler-owned budget for a single background step.
///
/// Steps that run inside a waveform window get a quiet budget: any
/// drawable-state change there means the closing phase would write
/// planes the panel does not show, forcing an abandon and a wasted
/// full re-drive on the next frame. Apps must gate progress-indicator
/// updates (and any other dirty marks driven purely by background
/// work) on [`BgBudget::allows_repaint`] and defer them to the next
/// permissive step; the scheduler always runs one after the render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BgBudget {
    repaint_ok: bool,
}

impl BgBudget {
    /// Budget for a normal step; repaints are fine (scheduler-side only).
    pub const fn new() -> Self {
        Self { repaint_ok: true }
    }

    /// Budget for a step inside a waveform window (scheduler-side only).
    pub const fn quiet() -> Self {
        Self { repaint_ok: false }
    }

    /// Whether this step may change drawable state / mark regions dirty.
    #[inline]
    pub fn allows_repaint(&self) -> bool {
        self.repaint_ok
    }
}

/// Outcome of a single background step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgOutcome {
    /// No useful work to do right now.
    Idle,
    /// Did useful work this call; `more: true` means "call me again soon".
    Progress { more: bool },
    /// Background work exists conceptually but cannot advance locally
    /// (e.g. waiting on a worker task to finish image decoding).
    WaitingExternal,
}

impl BgOutcome {
    /// Merge two outcomes — "most active" wins.
    ///
    /// Used by `AppManager` to combine active + suspended app outcomes.
    pub fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Progress { more: true }, _) | (_, Self::Progress { more: true }) => {
                Self::Progress { more: true }
            }
            (Self::Progress { more: false }, _) | (_, Self::Progress { more: false }) => {
                Self::Progress { more: false }
            }
            (Self::WaitingExternal, _) | (_, Self::WaitingExternal) => Self::WaitingExternal,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum QuickActionKind {
    Cycle {
        value: u8,
        options: &'static [&'static str],
    },
    Trigger {
        display: &'static str,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct QuickAction {
    pub id: u8,
    pub label: &'static str,
    pub kind: QuickActionKind,
    /// lead glyph in the sheet's first column (Phosphor codepoint)
    pub icon: Option<char>,
    /// short right-aligned text for trigger rows ("5 / 11", "62%")
    pub value: FixedStr<20>,
}

impl QuickAction {
    pub const fn cycle(
        id: u8,
        label: &'static str,
        value: u8,
        options: &'static [&'static str],
    ) -> Self {
        Self {
            id,
            label,
            kind: QuickActionKind::Cycle { value, options },
            icon: None,
            value: FixedStr::EMPTY,
        }
    }

    pub const fn trigger(id: u8, label: &'static str, display: &'static str) -> Self {
        Self {
            id,
            label,
            kind: QuickActionKind::Trigger { display },
            icon: None,
            value: FixedStr::EMPTY,
        }
    }

    pub const fn with_icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn with_value(mut self, value: &str) -> Self {
        self.value.set(value.as_bytes());
        self
    }
}

pub const RECENT_FILE: &str = "RECENT";

// distros define their own AppId enum and implement this trait
// the kernel uses HOME to initialise the nav stack, reset on
// Transition::Home, and read back a persisted nav stack; nothing else
// about the concrete variants is known to the kernel

pub trait AppIdType: Copy + Eq + core::fmt::Debug {
    const HOME: Self;

    /// Decode one byte of a persisted nav stack. The distro owns the
    /// encoding (`collect_session` writes it); unrecognised values must
    /// map to [`Self::HOME`]. This is the kernel's only way to reason
    /// about a saved stack without hardcoding the distro's numbering.
    fn from_raw(raw: u8) -> Self;
}

#[derive(Clone, Copy, Debug)]
pub enum PendingSetting {
    BookFontSize(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredPersistenceReason {
    Opportunistic,
    Transition,
    Sleep,
}

impl DeferredPersistenceReason {
    #[inline]
    pub const fn is_forced(self) -> bool {
        !matches!(self, Self::Opportunistic)
    }

    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opportunistic => "opportunistic",
            Self::Transition => "transition",
            Self::Sleep => "sleep",
        }
    }
}

/// Whether and when the scheduler should run a 4-level grayscale AA
/// pass on top of the BW image.
///
/// AA renders glyph edges with 4 levels of grey via the SSD1677's
/// dual-plane BW + RED RAM and a custom waveform LUT. The pass costs
/// roughly +300 ms of waveform time per fire, so the *when* matters as
/// much as the *whether*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrayscaleMode {
    /// No AA. The BW image is the final image.
    #[default]
    Disabled,
    /// Fire the grayscale pass immediately after every partial DU /
    /// post-GC refresh. Best for screens with rare, deliberate redraws
    /// (Reader page turns) where the latency hides naturally.
    Immediate,
    /// Hold the grayscale pass until the screen has been redraw-idle
    /// for `DEFERRED_GRAYSCALE_DELAY`. The pass is always full-screen
    /// and a windowed partial over the coded panel first neutralizes
    /// it (full revert plus plane rewrite), so this suits screens
    /// whose redraws are full-screen anyway; a list screen pays the
    /// neutralize on every cursor move.
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition<Id> {
    None,
    Push(Id),
    Pop,
    Replace(Id),
    Home,
}

/// Where the app layer wants to be once a special mode returns.
///
/// A special mode owns the screen and the input channel for its whole
/// run, so the scheduler cannot derive the next screen itself: the
/// mode reports it. No constructor spells "stay here" and none spells
/// `Pop` (a no-op at stack depth 1, which is where a tab-hosted mode
/// lives), so a mode that returns cannot be immediately re-entered.
#[must_use = "the scheduler applies a mode's exit; dropping it strands the mode"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeExit<Id>(Transition<Id>);

impl<Id: AppIdType> ModeExit<Id> {
    /// Leave for `id`.
    pub const fn to(id: Id) -> Self {
        Self(Transition::Replace(id))
    }

    /// Leave for the home screen.
    pub const fn home() -> Self {
        Self(Transition::Home)
    }

    pub(crate) const fn transition(self) -> Transition<Id> {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redraw {
    None,
    Partial(Region),
    Full,
}

const MSG_BUF_SIZE: usize = 64;
const LOADING_BUF_SIZE: usize = 32;

/// The pending redraw, if any.
///
/// One value for one state: a coalescing window only exists while a
/// partial redraw is being held back, and "render me now" carries no
/// window at all, so the scheduler reads the state instead of
/// re-deriving it from a flag and an `Option` that could disagree.
///
/// `Now` never holds [`Redraw::None`]; that state is spelled `None`.
#[derive(Debug)]
enum PendingRedraw {
    None,
    /// Render on the next tick.
    Now(Redraw),
    /// Background batch marks, held until `until` so a burst of them
    /// costs one refresh instead of one each.
    Coalescing { region: Region, until: Instant },
}

/// Loading indicator state; present only while one is shown.
struct Loading {
    buf: [u8; LOADING_BUF_SIZE],
    len: u8,
    pct: u8,
    region: Region,
}

pub struct AppContext {
    // the pending full redraw wants the real-temperature clear
    clean_refresh: bool,
    // display probe step to run, and whether the panel is held still
    // for the user to read a probe result
    display_probe: Option<u8>,
    probe_hold: bool,
    msg_buf: [u8; MSG_BUF_SIZE],
    msg_len: usize,
    msg_tag: u8,
    redraw: PendingRedraw,

    // loading indicator; kernel-level so any app can use it.
    // drawn by the app manager after app content, before overlays.
    // uses the built-in mono font so it works even with no bitmap
    // fonts loaded.
    loading: Option<Loading>,
}

impl Default for AppContext {
    fn default() -> Self {
        Self::new()
    }
}

impl AppContext {
    pub const fn new() -> Self {
        Self {
            msg_buf: [0u8; MSG_BUF_SIZE],
            msg_len: 0,
            msg_tag: 0,
            redraw: PendingRedraw::None,
            clean_refresh: false,
            display_probe: None,
            probe_hold: false,
            loading: None,
        }
    }

    pub fn set_message(&mut self, data: &[u8]) {
        let len = data.len().min(MSG_BUF_SIZE);
        self.msg_buf[..len].copy_from_slice(&data[..len]);
        self.msg_len = len;
        self.msg_tag = 0;
    }

    pub fn message(&self) -> &[u8] {
        &self.msg_buf[..self.msg_len]
    }

    /// Small out-of-band hint travelling with the message (set after
    /// `set_message`, cleared with it); distros define the values.
    pub fn set_message_tag(&mut self, tag: u8) {
        self.msg_tag = tag;
    }

    pub fn message_tag(&self) -> u8 {
        self.msg_tag
    }

    pub fn clear_message(&mut self) {
        self.msg_len = 0;
        self.msg_tag = 0;
    }

    pub fn request_full_redraw(&mut self) {
        self.redraw = PendingRedraw::Now(Redraw::Full);
    }

    /// A full redraw with the real-temperature clear waveform: the
    /// user asked for ghosting to go, so the quick fake-90C waveform
    /// (which does not reset the pigment) is the wrong tool.
    pub fn request_clean_refresh(&mut self) {
        self.redraw = PendingRedraw::Now(Redraw::Full);
        self.clean_refresh = true;
    }

    /// Whether the pending full redraw asked for the clean waveform;
    /// consumed by the render.
    pub fn take_clean_refresh(&mut self) -> bool {
        core::mem::take(&mut self.clean_refresh)
    }

    /// Run step `step` of the display probe (see `Screen::display_probe`)
    /// and hold the panel still afterwards so the result can be read:
    /// redraws are dropped until `end_display_probe`.
    pub fn request_display_probe(&mut self, step: u8) {
        self.display_probe = Some(step);
        self.probe_hold = true;
    }

    /// Release the probe hold and repaint; the screen promotes the
    /// repaint to a clean clear since the planes hold probe patterns.
    pub fn end_display_probe(&mut self) {
        self.probe_hold = false;
        self.display_probe = None;
        self.request_full_redraw();
    }

    pub fn take_display_probe(&mut self) -> Option<u8> {
        self.display_probe.take()
    }

    #[inline]
    pub fn display_probe_hold(&self) -> bool {
        self.probe_hold
    }

    // union `region` into whatever is pending, preserving its urgency:
    // a full redraw already covers it, and a coalescing window keeps
    // its original deadline
    fn union_partial(&mut self, region: Region) {
        self.redraw = match self.redraw {
            PendingRedraw::Now(Redraw::Full) => return,
            PendingRedraw::Now(Redraw::Partial(existing)) => {
                PendingRedraw::Now(Redraw::Partial(existing.union(region)))
            }
            PendingRedraw::Coalescing {
                region: existing,
                until,
            } => PendingRedraw::Coalescing {
                region: existing.union(region),
                until,
            },
            PendingRedraw::None | PendingRedraw::Now(Redraw::None) => {
                PendingRedraw::Now(Redraw::Partial(region))
            }
        };
    }

    // mark dirty and render on next tick; the default for all callers
    #[inline]
    pub fn mark_dirty(&mut self, region: Region) {
        self.union_partial(region);
        // a direct mark outranks a batch window: promote rather than
        // wait out a deadline armed by background work
        if let PendingRedraw::Coalescing { region, .. } = self.redraw {
            self.redraw = PendingRedraw::Now(Redraw::Partial(region));
        }
    }

    // mark dirty with coalescing window; use only for background
    // batch updates (title scanner) where many rapid dirty marks
    // should coalesce into a single refresh
    #[inline]
    pub fn mark_dirty_coalesced(&mut self, region: Region) {
        use super::timing;
        match self.redraw {
            // clean slate: hold this mark for the batch window
            PendingRedraw::None => {
                self.redraw = PendingRedraw::Coalescing {
                    region,
                    until: Instant::now()
                        + embassy_time::Duration::from_millis(timing::COALESCE_WINDOW_MS),
                };
            }
            // already pending: fold in without changing its urgency. an
            // armed window keeps its original deadline, so a steady
            // drip of marks cannot postpone the refresh forever, and a
            // render-now mark is never demoted to waiting
            _ => self.union_partial(region),
        }
    }

    pub fn has_redraw(&self) -> bool {
        !matches!(self.redraw, PendingRedraw::None)
    }

    // true when a pending redraw is ready to render
    pub fn render_ready(&self) -> bool {
        match self.redraw {
            PendingRedraw::None => false,
            PendingRedraw::Now(_) => true,
            PendingRedraw::Coalescing { until, .. } => Instant::now() >= until,
        }
    }

    /// Instant at which a pending coalesced redraw becomes render-ready,
    /// if one is armed. None when nothing is pending or the redraw is
    /// already renderable (the scheduler renders it before parking, so
    /// only a future coalesce window needs a timed wake).
    pub fn next_render_deadline(&self) -> Option<Instant> {
        match self.redraw {
            PendingRedraw::Coalescing { until, .. } => Some(until),
            _ => None,
        }
    }

    pub fn take_redraw(&mut self) -> Redraw {
        match core::mem::replace(&mut self.redraw, PendingRedraw::None) {
            PendingRedraw::None => Redraw::None,
            PendingRedraw::Now(r) => r,
            PendingRedraw::Coalescing { region, .. } => Redraw::Partial(region),
        }
    }

    // loading indicator: set text and percentage.
    // draws "msg...pct%" using the built-in mono font.
    // region defines where it renders; typically just below the
    // app header in the content area.
    // auto-marks the region dirty so the next render shows it.
    pub fn set_loading(&mut self, region: Region, msg: &str, pct: u8) {
        let pct = pct.min(100);
        log::debug!(
            "ui: set_loading msg='{}' pct={} region={:?} redraw_before={:?}",
            msg,
            pct,
            region,
            self.redraw
        );

        let n = msg.len().min(LOADING_BUF_SIZE);
        let mut buf = [0u8; LOADING_BUF_SIZE];
        buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
        self.loading = Some(Loading {
            buf,
            len: n as u8,
            pct,
            region,
        });
        self.mark_dirty(region);
    }

    // clear the loading indicator and mark its region dirty so
    // the underlying content repaints
    pub fn clear_loading(&mut self) {
        if let Some(l) = self.loading.take() {
            log::debug!(
                "ui: clear_loading region={:?} redraw_before={:?}",
                l.region,
                self.redraw
            );
            self.mark_dirty(l.region);
        }
    }

    #[inline]
    pub fn loading_active(&self) -> bool {
        self.loading.is_some()
    }

    #[inline]
    pub fn loading_msg(&self) -> &str {
        self.loading
            .as_ref()
            .and_then(|l| core::str::from_utf8(&l.buf[..l.len as usize]).ok())
            .unwrap_or("")
    }

    #[inline]
    pub fn loading_pct(&self) -> u8 {
        self.loading.as_ref().map_or(0, |l| l.pct)
    }

    #[inline]
    pub fn loading_region(&self) -> Region {
        self.loading
            .as_ref()
            .map_or(Region::new(0, 0, 0, 0), |l| l.region)
    }
}

pub trait App<Id> {
    fn on_enter(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>);

    fn on_exit(&mut self) {}

    fn on_suspend(&mut self) {
        self.on_exit();
    }

    fn on_resume(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        self.on_enter(ctx, k);
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition<Id>;

    fn quick_actions(&self) -> &[QuickAction] {
        &[]
    }

    /// Run a quick-menu trigger. May navigate (home's Continue
    /// reading pushes the reader).
    fn on_quick_trigger(&mut self, _id: u8, _ctx: &mut AppContext) -> Transition<Id> {
        Transition::None
    }

    fn on_quick_cycle_update(&mut self, _id: u8, _value: u8, _ctx: &mut AppContext) {}

    /// Title the quick menu sheet shows for this app (book title in
    /// the reader and on home). Empty falls back to "Menu".
    fn menu_title(&self) -> &str {
        ""
    }

    /// Meta line under the sheet title (position in the book).
    fn menu_meta(&self, _out: &mut StackFmt<64>) {}

    /// True while the app owns the Menu button (an open overlay of
    /// its own that Menu should close instead of opening the menu).
    fn captures_menu(&self) -> bool {
        false
    }

    fn draw(&self, strip: &mut StripBuffer);

    /// Run one bounded step of background work for the active app.
    ///
    /// Called by the scheduler once per main-loop iteration and
    /// repeatedly during EPD waveform waits. Each call should do
    /// a bounded amount of work and return promptly (target: well
    /// under 200 ms).
    fn background_step(
        &mut self,
        _ctx: &mut AppContext,
        _k: &mut KernelHandle<'_>,
        _budget: BgBudget,
    ) -> BgOutcome {
        BgOutcome::Idle
    }

    /// Run one bounded step of background work while suspended.
    ///
    /// Called for apps that are in the navigation stack but not
    /// currently active. No `AppContext` is provided since suspended
    /// apps should not affect the UI.
    fn background_suspended_step(
        &mut self,
        _k: &mut KernelHandle<'_>,
        _budget: BgBudget,
    ) -> BgOutcome {
        BgOutcome::Idle
    }

    fn pending_setting(&self) -> Option<PendingSetting> {
        None
    }

    fn save_state(&self, _bm: &mut BookmarkCache) {}

    /// Called on the active app right before the scheduler hands
    /// control to `enter_sleep`. Apps should drop transient heap
    /// (caches, decoded images, scratch buffers) that can be rebuilt
    /// from SD/bookmark on wake. The MCU performs a full reset on
    /// wake so any retained heap is lost anyway; freeing it before
    /// `load_sleep_image` gives the wallpaper allocator room to
    /// succeed.
    fn on_pre_sleep(&mut self, _k: &mut KernelHandle<'_>) {}

    /// Flush deferred persistence (e.g. RECENT, reading stats).
    ///
    /// Called periodically in safe no-redraw windows (`Opportunistic`)
    /// and on app transitions / before sleep (`Transition` / `Sleep`).
    /// Implementations should return early when nothing is dirty or
    /// when the debounce deadline hasn't passed (non-forced reasons).
    /// Dirty flags must only be cleared on success so failures retry.
    fn flush_deferred_persistence(
        &mut self,
        _k: &mut KernelHandle<'_>,
        _reason: DeferredPersistenceReason,
    ) -> crate::error::Result<()> {
        Ok(())
    }

    fn hide_button_bar(&self) -> bool {
        false
    }

    /// True if the manager should paint the shared top status bar
    /// above this app's content. Reader keeps this on (same today /
    /// battery line as other tabs) but draws its own page footer in
    /// place of the tab bar, so it overrides `show_tab_bar`.
    fn show_top_status(&self) -> bool {
        true
    }

    /// True if the manager should paint the shared bottom tab bar
    /// below this app's content. Reader returns false because it
    /// paints its own progress footer instead.
    fn show_tab_bar(&self) -> bool {
        true
    }

    /// Handle a horizontal navigation gesture (physical Left / Right,
    /// semantic `PrevJump` / `NextJump`).
    ///
    /// Default returns `HResult::AtEdge` so screens without horizontal
    /// content get free tab switching. Library overrides in chunk J
    /// to consume the gesture for its filter chips and grid.
    fn on_horizontal(&mut self, _dir: HDir, _ctx: &mut AppContext) -> HResult {
        HResult::AtEdge
    }
}

const MAX_STACK_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct NavEvent<Id> {
    pub from: Id,
    pub to: Id,
    pub suspend: bool,
    pub resume: bool,
}

// 4-deep navigation stack with shared AppContext
pub struct Launcher<Id: AppIdType> {
    stack: [Id; MAX_STACK_DEPTH],
    depth: usize,
    pub ctx: AppContext,
}

impl<Id: AppIdType> Default for Launcher<Id> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Id: AppIdType> Launcher<Id> {
    pub const fn new() -> Self {
        Self {
            stack: [Id::HOME; MAX_STACK_DEPTH],
            depth: 1,
            ctx: AppContext::new(),
        }
    }

    #[inline]
    pub fn active(&self) -> Id {
        self.stack[self.depth - 1]
    }

    #[inline]
    pub fn depth(&self) -> usize {
        self.depth
    }

    #[inline]
    pub fn stack_at(&self, index: usize) -> Id {
        self.stack[index]
    }

    // check if an app ID is anywhere in the stack
    pub fn contains(&self, id: Id) -> bool {
        self.stack[..self.depth].contains(&id)
    }

    // restore stack from saved session data
    // the `convert` function maps u8 values to Id.
    pub fn restore_stack<F>(&mut self, depth: usize, stack: &[u8], convert: F)
    where
        F: Fn(u8) -> Id,
    {
        self.depth = depth.clamp(1, MAX_STACK_DEPTH);
        for (i, &raw) in stack.iter().enumerate().take(self.depth) {
            self.stack[i] = convert(raw);
        }
    }

    pub fn apply(&mut self, transition: Transition<Id>) -> Option<NavEvent<Id>> {
        let old = self.active();

        let (suspend, resume) = match transition {
            Transition::None => return None,

            Transition::Push(id) => {
                if self.depth >= MAX_STACK_DEPTH {
                    log::warn!(
                        "nav stack full (depth {}), Push({:?}) degraded to Replace",
                        self.depth,
                        id
                    );
                    self.stack[self.depth - 1] = id;
                    (false, false)
                } else {
                    self.stack[self.depth] = id;
                    self.depth += 1;
                    (true, false)
                }
            }

            Transition::Pop => {
                if self.depth > 1 {
                    self.depth -= 1;
                    (false, true)
                } else {
                    return None;
                }
            }

            Transition::Replace(id) => {
                self.stack[self.depth - 1] = id;
                (false, false)
            }

            Transition::Home => {
                self.depth = 1;
                self.stack[0] = Id::HOME;
                (false, true)
            }
        };

        let new = self.active();
        if new != old {
            Some(NavEvent {
                from: old,
                to: new,
                suspend,
                resume,
            })
        } else {
            None
        }
    }
}

// aggregate interface the kernel scheduler calls on the app layer
// a distro implements this (typically via an AppManager struct that
// holds concrete app types and a with_app! dispatch macro); the
// scheduler is generic over AppLayer without importing any concrete
// app types

// run_special_mode is genuinely async (wifi radio); the rest is sync
#[allow(async_fn_in_trait)]
pub trait AppLayer {
    type Id: AppIdType;

    // active app and event dispatch
    fn active(&self) -> Self::Id;
    fn dispatch_event(&mut self, event: Event, bm: &mut BookmarkCache) -> Transition<Self::Id>;

    /// Handle a semantic input produced by the input policy layer.
    ///
    /// Semantic inputs bypass the ButtonMapper → ActionEvent path and
    /// carry higher-level intent (e.g. `MenuTap` = toggle quick menu).
    /// Default implementation is a no-op returning `Transition::None`.
    fn dispatch_semantic(&mut self, _input: SemanticInput) -> Transition<Self::Id> {
        Transition::None
    }

    /// True once when the app layer asked for sleep (quick menu Sleep
    /// row); the scheduler treats it like the power long-press.
    fn take_sleep_request(&mut self) -> bool {
        false
    }

    fn apply_transition(&mut self, t: Transition<Self::Id>, k: &mut KernelHandle<'_>);

    /// Run one round of sync background steps for all apps.
    ///
    /// Dispatches `background_step` to the active app and
    /// `background_suspended_step` to suspended apps, returning the
    /// merged outcome.
    fn run_background_step(
        &mut self,
        k: &mut KernelHandle<'_>,
        budget: BgBudget,
    ) -> BgOutcome;

    // rendering
    fn draw(&self, strip: &mut StripBuffer);
    fn has_redraw(&self) -> bool;
    fn take_redraw(&mut self) -> Redraw;
    fn request_full_redraw(&mut self);
    fn ctx_mut(&mut self) -> &mut AppContext;

    // system configuration
    fn system_settings(&self) -> &SystemSettings;

    /// Generation counter bumped whenever any `SystemSettings` field
    /// changes.  The kernel compares this against its last-seen value
    /// to decide whether hardware state needs updating.
    fn settings_generation(&self) -> u32;

    /// Called by the kernel when `swap_buttons` changes.  The app
    /// layer owns `ButtonMapper` and button-feedback labels, so it
    /// must propagate the new value itself.
    fn on_swap_buttons_changed(&mut self, swap: bool);

    /// Push live chrome state (battery percentage + today's reading
    /// stats) into the app layer so chunk D's persistent top status
    /// bar has up-to-date numbers without consulting `KernelHandle`
    /// per render. The default is a no-op so distros that don't yet
    /// render chrome aren't forced to plumb anything.
    fn set_chrome_state(&mut self, _battery_pct: u8, _today_pages: u16, _today_secs: u32) {}

    fn ghost_clear_every(&self) -> u32;
    /// Decide whether to run the grayscale AA pass for the current
    /// frame and, if so, whether to fire it immediately or defer it
    /// to the next redraw-idle window. See `GrayscaleMode`.
    fn grayscale_mode(&self) -> GrayscaleMode;

    // boot-time init: load settings, populate caches, enter first app
    fn load_eager_settings(&mut self, k: &mut KernelHandle<'_>);
    fn load_initial_state(&mut self, k: &mut KernelHandle<'_>);
    fn enter_initial(&mut self, k: &mut KernelHandle<'_>);

    // save active app's ephemeral state (e.g. reader position) to the
    // bookmark cache; called before collect_session during sleep so
    // bookmarks and session stay in sync
    fn save_active_state(&mut self, bm: &mut BookmarkCache);

    /// Dispatch `on_pre_sleep` to the active app. Called from the
    /// scheduler right before `enter_sleep` so apps can drop transient
    /// heap to make room for the sleep wallpaper allocator.
    fn on_active_pre_sleep(&mut self, k: &mut KernelHandle<'_>);

    /// True when `draw_sleep_overlay` has something to paint. With no
    /// wallpaper and no overlay the kernel falls back to its mono
    /// sleep text.
    fn has_sleep_overlay(&self) -> bool {
        false
    }

    /// Paint over the sleep screen. Runs inside both sleep passes (the
    /// BW base and the grayscale overlay) after the wallpaper blit, so
    /// 2bpp glyphs get anti-aliased edges for free; anything drawn
    /// over gray content must `fill_flat` its region first. Called
    /// after `on_active_pre_sleep`, so implementations draw from state
    /// that survives the app's heap drop.
    fn draw_sleep_overlay(&self, _strip: &mut StripBuffer) {}

    /// Flush deferred persistence for all app singletons.
    ///
    /// Dispatches to every app (not just the active one) so that
    /// failed flushes can retry even when the owning app is suspended.
    /// The reason identifies whether this is an opportunistic,
    /// transition-time, or sleep-time flush.
    fn flush_deferred_persistence(
        &mut self,
        k: &mut KernelHandle<'_>,
        reason: DeferredPersistenceReason,
    ) -> crate::error::Result<()>;

    // session persistence: save/restore active app across sleep/wake
    // using RTC FAST memory (survives deep sleep, zeroed on power-on)
    //
    // collect_session writes app state to the provided RtcSession struct
    // apply_session restores app state from RtcSession, returns true if successful
    fn collect_session(&self, session: &mut super::rtc_session::RtcSession);
    fn apply_session(
        &mut self,
        session: &super::rtc_session::RtcSession,
        k: &mut KernelHandle<'_>,
    ) -> bool;

    // true when the active app wants to take over the main loop
    // (e.g. wifi upload mode bypasses the normal event dispatch)
    fn needs_special_mode(&self) -> bool {
        false
    }

    // run the special mode; scheduler calls this when
    // needs_special_mode() returns true. the screen half and SD are
    // passed from the kernel since special modes drive the EPD
    // and SD directly (e.g. wifi upload mode).
    //
    // the returned ModeExit is what moves the app layer off the mode;
    // the scheduler applies it and would otherwise re-enter on the
    // next pass, since needs_special_mode() is a query over state the
    // mode itself does not change.
    async fn run_special_mode(
        &mut self,
        _screen: &mut super::Screen,
        _sd: &SdStorage,
    ) -> ModeExit<Self::Id> {
        ModeExit::home()
    }

    // true when deferred input during EPD refresh should be
    // suppressed (e.g. quick menu overlay is open)
    fn suppress_deferred_input(&self) -> bool {
        false
    }
}
