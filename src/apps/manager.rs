// app lifecycle manager: nav stack, dispatch, font propagation, draw
//
// all dispatch is static (monomorphized via with_app!); no dyn, no vtable
// loading indicator is drawn between app content and overlays so it
// sits on top of page content but under quick menu and button bumps

use crate::apps::home::{self, HomeApp};
use crate::apps::library::LibraryApp;
use crate::apps::reader::ReaderApp;
use crate::apps::settings::SettingsApp;
use crate::apps::stats::StatsApp;
use crate::apps::{
    App, AppContext, AppId, AppNav, AppNavSlot, BgBudget, BgOutcome, DeferredPersistenceReason,
    HDir, HResult, Launcher, Modal, PendingSetting, Redraw, Tab, Transition,
};

use crate::apps::upload::UploadExit;
use crate::apps::widgets::quick_menu::{
    MAX_APP_ACTIONS, MenuContext, QuickAction, QuickMenuResult,
};
use crate::apps::widgets::{ButtonFeedback, QuickMenu, SleepCard};
use crate::board::action::{Action, ActionEvent, ButtonMapper};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::input::Event;
use crate::drivers::sdcard::SdStorage;
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::app::{AppIdType, AppLayer, ModeExit};
use crate::kernel::{KernelHandle, Screen};
use crate::kernel::bookmarks::BookmarkCache;
use crate::kernel::config::SystemSettings;
use crate::kernel::input_policy::SemanticInput;
use crate::ui::chrome::Chrome;
use crate::ui::{Painter, Region, Theme};

// monomorphized dispatch from AppId to concrete app type
macro_rules! with_app {
    ($id:expr, $mgr:expr, |$app:ident| $body:expr) => {
        match $id {
            AppId::Home => {
                let $app = &mut *$mgr.home;
                $body
            }
            AppId::Library => {
                let $app = &mut *$mgr.library;
                $body
            }
            AppId::Reader => {
                let $app = &mut *$mgr.reader;
                $body
            }
            AppId::Settings => {
                let $app = &mut *$mgr.settings;
                $body
            }
            AppId::Stats => {
                let $app = &mut *$mgr.stats;
                $body
            }
            AppId::Upload => {
                unreachable!("Upload mode is handled outside the app dispatch loop");
            }
        }
    };
}

// shared-ref variant for read-only dispatch (draw, quick_actions)
macro_rules! with_app_ref {
    ($id:expr, $mgr:expr, |$app:ident| $body:expr) => {
        match $id {
            AppId::Home => {
                let $app = &*$mgr.home;
                $body
            }
            AppId::Library => {
                let $app = &*$mgr.library;
                $body
            }
            AppId::Reader => {
                let $app = &*$mgr.reader;
                $body
            }
            AppId::Settings => {
                let $app = &*$mgr.settings;
                $body
            }
            AppId::Stats => {
                let $app = &*$mgr.stats;
                $body
            }
            AppId::Upload => {
                unreachable!("Upload mode is handled outside the app dispatch loop");
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use with_app;
#[allow(unused_imports)]
pub(crate) use with_app_ref;

pub struct AppManager {
    pub launcher: &'static mut Launcher,

    pub home: &'static mut HomeApp,
    pub library: &'static mut LibraryApp,
    pub reader: &'static mut ReaderApp,
    pub settings: &'static mut SettingsApp,
    pub stats: &'static mut StatsApp,

    pub quick_menu: &'static mut QuickMenu,
    pub bumps: &'static mut ButtonFeedback,

    // filled right before the active app drops its heap for sleep,
    // painted by the kernel's sleep passes through `draw_sleep_overlay`
    pub sleep_card: &'static mut SleepCard,

    pub mapper: ButtonMapper,

    /// nav state mirrored from `launcher` after every mutation. chunks
    /// C/D/E/G consume this; the underlying `Launcher` stays as the
    /// dispatch backend until chunk N retires both it and `with_app!`.
    pub nav: AppNav,

    /// persistent top status bar + bottom tab bar. drawn around tab
    /// screens (`App::show_chrome() == true`). reader opts out and
    /// keeps its legacy chrome until chunk G.
    pub chrome: Chrome,

    /// tab the upload screen was opened from, and the one Back returns
    /// to. upload is a tab, so it is entered by `Replace` at stack
    /// depth 1: there is nothing to pop, and the launcher is the only
    /// place that remembers where the user was.
    upload_return_tab: Tab,

    // set by the quick menu's Sleep row, drained by the scheduler
    sleep_requested: bool,
}

/// map a (legacy) `AppId` to the `Tab` it represents in the new model,
/// or `None` if the `AppId` is a modal (only `Reader`) or has been
/// orphaned from the tab bar (legacy `Files`, retired in chunk N).
fn appid_to_tab(id: AppId) -> Option<Tab> {
    match id {
        AppId::Home => Some(Tab::Home),
        AppId::Library => Some(Tab::Library),
        AppId::Stats => Some(Tab::Stats),
        AppId::Settings => Some(Tab::Settings),
        AppId::Upload => Some(Tab::Upload),
        AppId::Reader => None,
    }
}

#[inline]
fn appid_to_modal(id: AppId) -> Option<Modal> {
    match id {
        AppId::Reader => Some(Modal::Reader),
        _ => None,
    }
}

/// `Press(PrevJump)` / `Press(NextJump)` / their `Repeat` siblings
/// translate to a horizontal navigation direction. anything else
/// returns None and falls through to the regular event dispatch path.
fn horizontal_from_event(ev: ActionEvent) -> Option<HDir> {
    let action = match ev {
        ActionEvent::Press(a) | ActionEvent::Repeat(a) => a,
        _ => return None,
    };
    match action {
        Action::PrevJump => Some(HDir::Left),
        Action::NextJump => Some(HDir::Right),
        _ => None,
    }
}

/// inverse of `appid_to_tab`: which `AppId` backs each tab in the
/// current bridge layout. used to translate `Nav::SetTab(...)` style
/// commands back into legacy `Transition` values until chunk N
/// retires the launcher.
fn tab_to_appid(tab: Tab) -> AppId {
    match tab {
        Tab::Home => AppId::Home,
        Tab::Library => AppId::Library,
        Tab::Stats => AppId::Stats,
        Tab::Settings => AppId::Settings,
        Tab::Upload => AppId::Upload,
    }
}

/// derive `(active_tab, optional_modal)` from the current launcher
/// stack. used to keep `AppManager.nav` in sync after every launcher
/// mutation.
fn nav_state_from_launcher(launcher: &Launcher) -> (Tab, Option<Modal>) {
    let active = launcher.active();
    if let Some(modal) = appid_to_modal(active) {
        let mut tab = Tab::Home;
        let depth = launcher.depth();
        for i in (0..depth.saturating_sub(1)).rev() {
            if let Some(t) = appid_to_tab(launcher.stack_at(i)) {
                tab = t;
                break;
            }
        }
        (tab, Some(modal))
    } else {
        (appid_to_tab(active).unwrap_or(Tab::Home), None)
    }
}

impl AppManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        launcher: &'static mut Launcher,
        home: &'static mut HomeApp,
        library: &'static mut LibraryApp,
        reader: &'static mut ReaderApp,
        settings: &'static mut SettingsApp,
        stats: &'static mut StatsApp,
        quick_menu: &'static mut QuickMenu,
        bumps: &'static mut ButtonFeedback,
        sleep_card: &'static mut SleepCard,
        mapper: ButtonMapper,
    ) -> Self {
        Self {
            launcher,
            home,
            library,
            reader,
            settings,
            stats,
            quick_menu,
            bumps,
            sleep_card,
            mapper,
            nav: AppNav::new(Tab::Home),
            chrome: Chrome::new(),
            upload_return_tab: Tab::Home,
            sleep_requested: false,
        }
    }

    /// Re-derive `self.nav` from the launcher stack. Call after every
    /// `launcher.apply()` or `launcher.restore_stack()`.
    #[inline]
    fn sync_nav_from_launcher(&mut self) {
        let (tab, modal) = nav_state_from_launcher(self.launcher);
        self.nav.restore(tab, modal);
        // the top bar names the current tab and its two neighbours.
        self.chrome.top.active = tab;
    }

    /// Currently active tab in the new nav model. Chrome reads this
    /// to highlight the right tab in the bottom bar.
    #[inline]
    pub fn active_tab(&self) -> Tab {
        self.nav.active_tab()
    }

    /// Currently open modal, if any.
    #[inline]
    pub fn active_modal(&self) -> Option<Modal> {
        self.nav.modal()
    }

    /// Combined view: tab or modal-over-tab. Useful for chrome
    /// decisions ("show the chrome only when no modal is open"
    /// happens via `App::show_chrome` on the active app, not here).
    #[inline]
    pub fn active_nav_slot(&self) -> AppNavSlot {
        self.nav.active_slot()
    }

    #[inline]
    pub fn active(&self) -> AppId {
        self.launcher.active()
    }

    #[inline]
    pub fn ctx(&self) -> &AppContext {
        &self.launcher.ctx
    }

    #[inline]
    pub fn ctx_mut(&mut self) -> &mut AppContext {
        &mut self.launcher.ctx
    }

    #[inline]
    pub fn has_redraw(&self) -> bool {
        self.launcher.ctx.has_redraw()
    }

    #[inline]
    pub fn take_redraw(&mut self) -> Redraw {
        self.launcher.ctx.take_redraw()
    }

    #[inline]
    pub fn request_full_redraw(&mut self) {
        self.launcher.ctx.request_full_redraw();
    }

    #[inline]
    pub fn apply_nav(&mut self, transition: Transition) -> Option<crate::apps::NavEvent> {
        let result = self.launcher.apply(transition);
        if result.is_some() {
            self.sync_nav_from_launcher();
        }
        result
    }

    pub fn load_eager_settings(&mut self, k: &mut KernelHandle<'_>) {
        self.settings.load_eager(k);
        self.propagate_fonts();
        self.sync_button_config();
    }

    // sync button mapper and label widget from settings
    fn sync_button_config(&mut self) {
        let swap = self.settings.system_settings().swap_buttons;
        self.mapper.set_swap(swap);
        if self.bumps.set_swap(swap) {
            // labels changed, need to redraw the button bar
            self.launcher.ctx.mark_dirty(crate::ui::Region::new(
                0,
                crate::board::SCREEN_H - crate::ui::BUTTON_BAR_H,
                crate::board::SCREEN_W,
                crate::ui::BUTTON_BAR_H,
            ));
        }
    }

    pub fn load_home_recent(&mut self, k: &mut KernelHandle<'_>) {
        self.home.load_recent(k);
    }

    pub fn enter_initial(&mut self, k: &mut KernelHandle<'_>) {
        self.home.on_enter(&mut self.launcher.ctx, k);
    }

    // save active app state to bookmark cache before sleep
    pub fn save_active_state(&mut self, bm: &mut crate::kernel::bookmarks::BookmarkCache) {
        let active = self.launcher.active();
        with_app!(active, self, |app| app.save_state(bm));
    }

    // drop transient heap on the active app before sleep so the
    // wallpaper allocator has room. wake path rebuilds state from SD
    // via `restore_state`, so dropping here is correctness-safe.
    pub fn on_active_pre_sleep(&mut self, k: &mut KernelHandle<'_>) {
        // the sleep card copies what it needs while the reader still
        // holds its TOC and stats, and the cover read happens while
        // the SD card is still awake
        self.fill_sleep_card(k);
        let active = self.launcher.active();
        with_app!(active, self, |app| app.on_pre_sleep(k));
    }

    fn fill_sleep_card(&mut self, k: &mut KernelHandle<'_>) {
        let card = &mut *self.sleep_card;
        card.clear();
        let mut filename = plump_kernel::util::FixedStr::<32>::EMPTY;
        if self.launcher.contains(AppId::Reader) && self.reader.has_book() {
            self.reader.fill_sleep_card(card);
            filename.set(self.reader.filename_bytes());
        } else if self.home.fill_sleep_card(card) {
            filename.set(self.home.recent_filename());
        }
        if !card.is_set() {
            log::info!("sleep card: no book to show");
            return;
        }
        card.set_fonts(fonts::ReaderFont::from_idx(
            self.settings.system_settings().reader_font,
        ));
        if let Some(next) = self.home.next_recent_title() {
            card.set_next(next);
        }
        if !card.has_stats() {
            if let Some(s) = crate::apps::stats::ReadingStats::load(k, filename.as_str()) {
                card.set_stats(s.pages, s.time_secs);
            }
        }
        // the Card variant scaled down beats the Mini one blown up; the
        // loader hands back whichever variant the bundle has
        match crate::apps::cover_cache::load_cover_variant_for(
            k,
            filename.as_bytes(),
            plump_kernel::kernel::bundle::CoverKind::Card,
        ) {
            Some(img) => {
                let ok = card.set_cover(&img);
                log::info!(
                    "sleep card: cover {}x{} stride {} {}",
                    img.width,
                    img.height,
                    img.stride,
                    if ok { "copied" } else { "does not fit, placeholder" }
                );
            }
            None => log::info!("sleep card: no cover in bundle, placeholder"),
        }
        card.set_battery(crate::drivers::battery::battery_percentage(k.battery_mv()));
        log::info!("sleep card: filled for {}", filename.as_str());
    }

    // collect session state to RTC memory struct before sleep
    pub fn collect_session(&self, session: &mut crate::kernel::rtc_session::RtcSession) {
        use crate::kernel::rtc_session::MAX_NAV_STACK;

        // save navigation stack
        session.nav_depth = self.launcher.depth() as u8;
        for i in 0..MAX_NAV_STACK {
            session.nav_stack[i] = if i < self.launcher.depth() {
                self.launcher.stack_at(i) as u8
            } else {
                0
            };
        }

        // save reader state
        session.reader_filename_len = self.reader.filename_len() as u8;
        let len = session.reader_filename_len as usize;
        session.reader_filename[..len].copy_from_slice(self.reader.filename_bytes());
        session.reader_is_epub = self.reader.is_epub() as u8;
        session.reader_chapter = self.reader.chapter();
        session.reader_page = self.reader.page() as u16;
        session.reader_byte_offset = self.reader.byte_offset();
        session.reader_font_size = self.reader.font_size_idx();

        // (legacy `files_*` fields are now reserved padding — Files
        // app was removed in chunk N; the struct fields stay to keep
        // the on-RTC layout stable for old wake images that still
        // decode to this struct.)

        // save home state
        session.home_state = self.home.state_id();
        session.home_selected = self.home.selected() as u8;
        session.home_bm_selected = self.home.bm_selected() as u8;
        session.home_bm_scroll = self.home.bm_scroll() as u8;

        log::debug!(
            "session: collected nav_depth={} active={:?}",
            session.nav_depth,
            self.launcher.active()
        );
    }

    // restore session from RTC memory; returns true if successful
    //
    // IMPORTANT: this does NOT call on_enter() on any app. the previous
    // code called on_enter() after restore_state(), which clobbered the
    // restored values (home.selected, files.scroll, reader.chapter, etc).
    // instead, each app's restore_state() is now responsible for setting
    // up ALL state needed to resume, and the reader enters at NeedBookmark
    // with its chapter/offset pre-populated from the RTC session.
    pub fn apply_session(
        &mut self,
        session: &crate::kernel::rtc_session::RtcSession,
        k: &mut KernelHandle<'_>,
    ) -> bool {
        // validate session data
        if session.nav_depth == 0 || session.nav_depth > 4 {
            log::warn!("session: invalid nav_depth {}", session.nav_depth);
            return false;
        }

        // restore navigation stack
        self.launcher.restore_stack(
            session.nav_depth as usize,
            &session.nav_stack,
            AppId::from_raw,
        );
        self.sync_nav_from_launcher();

        log::debug!(
            "session: restored nav stack depth={} active={:?}",
            session.nav_depth,
            self.launcher.active()
        );

        // restore home state (always in stack)
        // restore_state sets state/selected/bm cursors without
        // resetting them like on_enter() would
        self.home.restore_state(
            session.home_state,
            session.home_selected as usize,
            session.home_bm_selected as usize,
            session.home_bm_scroll as usize,
        );
        // battery percentage for status display
        self.home.set_battery(k.battery_mv());

        // (no Files restore — app was removed in chunk N)

        // restore reader state if active or in stack
        if self.launcher.active() == AppId::Reader || self.launcher.contains(AppId::Reader) {
            let filename = &session.reader_filename[..session.reader_filename_len as usize];
            self.reader.restore_state(
                filename,
                session.reader_is_epub != 0,
                session.reader_chapter,
                session.reader_page as usize,
                session.reader_byte_offset,
                session.reader_font_size,
            );

            // Wake-to-reader should use the rich cover loading screen on
            // the very first frame instead of flashing a generic resume
            // screen before background restore reloads cached assets.
            if self.launcher.active() == AppId::Reader {
                self.reader.prepare_restore_loading_screen(k);
            }
        }

        // propagate fonts (uses settings already loaded)
        self.propagate_fonts();

        // the reader paints its own first frame after wake: a full
        // clear once the page, or its loading screen, is due. every
        // other app repaints now
        if self.launcher.active() != AppId::Reader {
            self.launcher.ctx.request_full_redraw();
        }

        log::debug!(
            "session: restore complete, active={:?}",
            self.launcher.active()
        );

        true
    }

    /// Open the quick menu for the active app. every main-screen
    /// tab borrows home's header and resume rows, so the last book
    /// is one press away from anywhere; the reader has its own.
    fn open_quick_menu(&mut self) {
        let active = self.launcher.active();
        let borrow_home =
            active != AppId::Home && active != AppId::Reader && self.home.has_recent();
        let mut meta = crate::ui::StackFmt::<64>::new();
        let mut combined = [QuickAction::trigger(0, "", ""); MAX_APP_ACTIONS];
        let mut n = 0;
        if borrow_home {
            self.home.menu_meta(&mut meta);
            for a in self.home.quick_actions() {
                combined[n] = *a;
                n += 1;
            }
        } else {
            with_app_ref!(active, self, |app| app.menu_meta(&mut meta));
        }
        let own: &[QuickAction] = with_app_ref!(active, self, |app| app.quick_actions());
        for a in own.iter().take(MAX_APP_ACTIONS - n) {
            combined[n] = *a;
            n += 1;
        }
        let title: &str = if borrow_home {
            self.home.menu_title()
        } else {
            with_app_ref!(active, self, |app| app.menu_title())
        };
        self.quick_menu.show(
            &combined[..n],
            MenuContext {
                title,
                meta: meta.as_str(),
                on_home: active == AppId::Home,
            },
        );
        self.launcher.ctx.mark_dirty(self.quick_menu.region());
    }

    /// Menu goes to the app while it owns an overlay of its own (the
    /// reader's contents sheet closes on Menu instead of opening the
    /// menu over it).
    fn forward_menu_to_app(&mut self) -> Option<Transition> {
        let active = self.launcher.active();
        let captures = with_app_ref!(active, self, |app| app.captures_menu());
        if !captures {
            return None;
        }
        Some(with_app!(active, self, |app| {
            app.on_event(ActionEvent::Press(Action::Menu), &mut self.launcher.ctx)
        }))
    }

    /// Close the quick menu, propagating any changed cycle values
    /// and pending settings to the active app.
    ///
    /// Idempotent: safe to call even if `hide()` was already called
    /// (e.g. from `QuickMenu::on_action`).
    fn close_quick_menu(&mut self) {
        let region = self.quick_menu.region();
        self.quick_menu.hide();
        self.sync_quick_menu();
        self.launcher.ctx.mark_dirty(region);
    }

    /// Handle a semantic input from the input policy layer.
    pub fn dispatch_semantic_input(&mut self, input: SemanticInput) -> Transition {
        match input {
            SemanticInput::MenuTap => {
                if self.quick_menu.open {
                    self.close_quick_menu();
                } else if let Some(t) = self.forward_menu_to_app() {
                    return t;
                } else {
                    self.open_quick_menu();
                }
                Transition::None
            }
        }
    }

    // power-button long-press must be intercepted by the scheduler
    // before calling this method
    pub fn dispatch_event(&mut self, hw_event: Event, bm_cache: &mut BookmarkCache) -> Transition {
        let event = self.mapper.map_event(hw_event);

        if self.quick_menu.open {
            return self.handle_quick_menu(event, bm_cache);
        }

        if matches!(event, ActionEvent::Press(Action::Menu)) {
            if let Some(t) = self.forward_menu_to_app() {
                return t;
            }
            self.open_quick_menu();
            return Transition::None;
        }

        // chunk E: physical Left / Right (PrevJump / NextJump) cycles
        // tabs at the edge of the current screen. only intercepted on
        // tab screens (active slot is Tab, not Modal). reader keeps
        // these for chapter jumps because the manager doesn't enter
        // this branch when active is a modal.
        if let Some(dir) = horizontal_from_event(event) {
            if let AppNavSlot::Tab(active_tab) = self.nav.active_slot() {
                let result = self.dispatch_horizontal(dir);
                if let HResult::AtEdge = result {
                    if let Some(next_tab) = match dir {
                        HDir::Left => active_tab.left(),
                        HDir::Right => active_tab.right(),
                    } {
                        return Transition::Replace(tab_to_appid(next_tab));
                    }
                }
                return Transition::None;
            }
        }

        let active = self.launcher.active();
        with_app!(active, self, |app| {
            app.on_event(event, &mut self.launcher.ctx)
        })
    }

    /// Route a horizontal navigation gesture to the currently active app.
    fn dispatch_horizontal(&mut self, dir: HDir) -> HResult {
        let active = self.launcher.active();
        with_app!(active, self, |app| {
            app.on_horizontal(dir, &mut self.launcher.ctx)
        })
    }

    fn handle_quick_menu(
        &mut self,
        event: ActionEvent,
        bm_cache: &mut BookmarkCache,
    ) -> Transition {
        let action = match event {
            ActionEvent::Press(a) | ActionEvent::Repeat(a) => a,
            _ => return Transition::None,
        };

        let result = self.quick_menu.on_action(action);

        match result {
            QuickMenuResult::Consumed => {
                if self.quick_menu.dirty {
                    self.launcher.ctx.mark_dirty(self.quick_menu.region());
                    self.quick_menu.dirty = false;
                }
                Transition::None
            }

            QuickMenuResult::Close => {
                // hide() already called by on_action; close_quick_menu
                // is idempotent and handles sync + dirty marking
                self.close_quick_menu();
                Transition::None
            }

            QuickMenuResult::RefreshScreen => {
                self.close_quick_menu();
                self.launcher.ctx.request_full_redraw();
                Transition::None
            }

            QuickMenuResult::GoHome => {
                self.close_quick_menu();
                Transition::Home
            }

            QuickMenuResult::Sleep => {
                self.close_quick_menu();
                self.sleep_requested = true;
                Transition::None
            }

            QuickMenuResult::AppTrigger(id) => {
                let active = self.launcher.active();
                self.close_quick_menu();

                // resume rows lent to a tab go back to home
                if active != AppId::Home && home::is_resume_action(id) {
                    return self.home.on_quick_trigger(id, &mut self.launcher.ctx);
                }

                with_app!(active, self, |app| {
                    let t = app.on_quick_trigger(id, &mut self.launcher.ctx);
                    // Save app state after trigger (e.g. font change
                    // may invalidate the reader's current page offset).
                    app.save_state(bm_cache);
                    t
                })
            }
        }
    }

    /// Flush deferred persistence for all app singletons.
    ///
    /// Dispatches to every concrete app so that failed flushes can
    /// retry even when the owning app is suspended (e.g. reader
    /// dirty state retries while Home is active).
    pub fn flush_deferred_persistence(
        &mut self,
        k: &mut KernelHandle<'_>,
        reason: DeferredPersistenceReason,
    ) -> crate::error::Result<()> {
        // today only ReaderApp does real work; others inherit the
        // default no-op. iterate all singletons for retry semantics.
        let mut first_error = None;
        for &id in &[
            AppId::Home,
            AppId::Library,
            AppId::Reader,
            AppId::Settings,
            AppId::Stats,
        ] {
            let result = with_app!(id, self, |app| app.flush_deferred_persistence(k, reason));
            if let Err(e) = result {
                log::warn!("flush_deferred_persistence({:?}): {}", id, e);
                first_error.get_or_insert(e);
            }
        }

        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(())
        }
    }

    pub fn apply_transition(&mut self, transition: Transition, k: &mut KernelHandle<'_>) {
        if let Some(nav) = self.launcher.apply(transition) {
            self.sync_nav_from_launcher();
            log::debug!("app: {:?} -> {:?}", nav.from, nav.to);

            // captured on the way in: once upload is active the tab it
            // replaced is gone from the stack
            if nav.to == AppId::Upload {
                self.upload_return_tab = appid_to_tab(nav.from).unwrap_or(Tab::Home);
            }

            if nav.from != AppId::Upload {
                with_app!(nav.from, self, |app| app.save_state(k.bookmark_cache_mut()));

                // force flush deferred persistence for all app singletons
                // before leaving the current app so inactive retries still
                // run and reader dirty state gets one last chance before the
                // singleton is potentially reused for another book.
                if let Err(e) =
                    self.flush_deferred_persistence(k, DeferredPersistenceReason::Transition)
                {
                    log::warn!("flush on leave {:?}: {}", nav.from, e);
                }

                with_app!(nav.from, self, |app| {
                    if nav.suspend {
                        app.on_suspend();
                    } else {
                        app.on_exit();
                    }
                });
            }

            self.propagate_fonts();
            self.launcher.ctx.clear_loading();

            if nav.to != AppId::Upload {
                if nav.resume {
                    with_app!(nav.to, self, |app| {
                        app.on_resume(&mut self.launcher.ctx, k)
                    });
                } else {
                    with_app!(nav.to, self, |app| {
                        app.on_enter(&mut self.launcher.ctx, k)
                    });
                }
            }

            if nav.resume {
                self.launcher
                    .ctx
                    .mark_dirty(Region::new(0, 0, SCREEN_W, SCREEN_H));
            } else if nav.to != AppId::Reader {
                // the reader holds its first frame until the page or
                // its loading screen is due (a full clear either way)
                self.launcher.ctx.request_full_redraw();
            }
        }
    }

    pub fn run_background_step(
        &mut self,
        k: &mut KernelHandle<'_>,
        budget: BgBudget,
    ) -> BgOutcome {
        let active = self.launcher.active();
        let active_outcome = with_app!(active, self, |app| {
            app.background_step(&mut self.launcher.ctx, k, budget)
        });

        let mut combined = active_outcome;
        for &id in &[
            AppId::Home,
            AppId::Library,
            AppId::Reader,
            AppId::Settings,
            AppId::Stats,
        ] {
            if id != active {
                let outcome = with_app!(id, self, |app| {
                    app.background_suspended_step(k, budget)
                });
                combined = combined.merge(outcome);
            }
        }

        self.sync_button_config();
        combined
    }

    pub fn draw(&self, strip: &mut StripBuffer) {
        let active = self.launcher.active();
        let show_top = with_app_ref!(active, self, |app| app.show_top_status());
        let show_tabs = with_app_ref!(active, self, |app| app.show_tab_bar());

        with_app_ref!(active, self, |app| app.draw(strip));

        // shared chrome is drawn on top of the app's content so the
        // bars always win the painter's-algorithm over any app pixel
        // that strays into the bar regions.
        // `show_tabs` now only says whether this screen is a tab
        // screen at all; there is one bar and it lives at the top
        let _ = show_tabs;
        if show_top {
            let theme = Theme::default_v1();
            let mut painter = Painter::new(strip, &theme);
            self.chrome.draw_top(&mut painter, &crate::ui::TopFonts {
                name: fonts::ui_heading_font(0),
                small: fonts::chrome_font(),
            });
        }

        // loading indicator: after app content, before overlays.
        // the reader draws its own centered loading screen while a
        // book is opening, so suppress the generic indicator there.
        let suppress_loading = active == AppId::Reader && self.reader.shows_loading_screen();
        if self.launcher.ctx.loading_active() && !suppress_loading {
            let region = self.launcher.ctx.loading_region();
            if region.intersects(strip.logical_window()) {
                crate::ui::LoadingIndicator::new(
                    region,
                    self.launcher.ctx.loading_msg(),
                    self.launcher.ctx.loading_pct(),
                )
                .draw(strip);
            }
        }

        // the settings cache sheet is an overlay like the quick menu:
        // drawn after the chrome so the bars do not slice its edges
        if active == AppId::Settings && self.settings.cache_sheet_open() {
            self.settings.draw_cache_sheet(strip);
        }

        // the settings cache sheet is an overlay like the quick menu:
        // drawn after the chrome so the bars do not slice its edges
        if active == AppId::Settings && self.settings.cache_sheet_open() {
            self.settings.draw_cache_sheet(strip);
        }

        if self.quick_menu.open {
            self.quick_menu.draw(strip);
        }

        // legacy button feedback bumps: only drawn when the active app
        // does NOT use the new chrome (today, only Reader). once
        // chunk G lands the new reader footer, button feedback gets
        // deleted entirely in chunk N.
        let hide = with_app_ref!(active, self, |app| app.hide_button_bar());
        if !hide && !show_tabs {
            self.bumps.draw(strip);
        }
    }

    pub fn propagate_fonts(&mut self) {
        let ss = self.settings.system_settings();
        let ui_idx = ss.ui_font_size_idx;
        let book_idx = ss.book_font_size_idx;
        let reader_font = fonts::ReaderFont::from_idx(ss.reader_font);
        let theme_idx = ss.reading_theme;
        let line_spacing = ss.line_spacing;
        let reader_status = ss.reader_status;
        let text_alignment = ss.text_alignment;

        self.home.set_ui_font_size(ui_idx);
        self.library.set_ui_font_size(ui_idx);
        self.settings.set_ui_font_size(ui_idx);
        self.stats.set_ui_font_size(ui_idx);
        // a book's title is set in the book's own face on every screen
        // that names one, not just inside the reader
        self.home.set_reader_font(reader_font);
        self.library.set_reader_font(reader_font);
        self.stats.set_reader_font(reader_font);
        // each setter flags the reader's layout_stale on change so
        // on_resume knows to re-index the chapter.
        self.reader.set_reader_font(reader_font);
        self.reader.set_book_font_size(book_idx);
        self.reader.set_reading_theme(theme_idx);
        self.reader.set_line_spacing(line_spacing);
        self.reader.set_show_chrome(reader_status);
        self.reader.set_text_alignment(text_alignment);

        let chrome = fonts::chrome_font();
        self.reader.set_chrome_font(chrome);
        self.reader.set_ui_font_size(ui_idx);
        self.quick_menu.set_ui_font_size(ui_idx);
        self.bumps.set_chrome_font(fonts::button_label_font());
    }

    fn sync_quick_menu(&mut self) {
        let active = self.launcher.active();

        for id in 0..MAX_APP_ACTIONS as u8 {
            if let Some(value) = self.quick_menu.app_cycle_value(id) {
                with_app!(active, self, |app| {
                    app.on_quick_cycle_update(id, value, &mut self.launcher.ctx);
                });
            }
        }

        let pending = with_app!(active, self, |app| app.pending_setting());
        if let Some(setting) = pending {
            match setting {
                PendingSetting::BookFontSize(idx) => {
                    let ss = self.settings.system_settings_mut();
                    if ss.book_font_size_idx != idx {
                        ss.book_font_size_idx = idx;
                        self.settings.mark_save_needed();
                    }
                }
            }
        }
    }

    #[inline]
    pub fn system_settings(&self) -> &crate::kernel::config::SystemSettings {
        self.settings.system_settings()
    }

    #[inline]
    pub fn settings_generation(&self) -> u32 {
        self.settings.generation()
    }

    pub fn ghost_clear_every(&self) -> u32 {
        if self.settings.is_loaded() {
            self.settings.system_settings().ghost_clear_every as u32
        } else {
            crate::kernel::DEFAULT_GHOST_CLEAR_EVERY
        }
    }
}

impl AppLayer for AppManager {
    type Id = AppId;

    fn take_sleep_request(&mut self) -> bool {
        core::mem::take(&mut self.sleep_requested)
    }

    #[inline]
    fn active(&self) -> AppId {
        self.launcher.active()
    }

    fn dispatch_event(&mut self, event: Event, bm: &mut BookmarkCache) -> Transition {
        AppManager::dispatch_event(self, event, bm)
    }

    fn dispatch_semantic(&mut self, input: SemanticInput) -> Transition {
        AppManager::dispatch_semantic_input(self, input)
    }

    fn apply_transition(&mut self, t: Transition, k: &mut KernelHandle<'_>) {
        AppManager::apply_transition(self, t, k);
    }

    fn run_background_step(
        &mut self,
        k: &mut KernelHandle<'_>,
        budget: BgBudget,
    ) -> BgOutcome {
        AppManager::run_background_step(self, k, budget)
    }

    fn draw(&self, strip: &mut StripBuffer) {
        AppManager::draw(self, strip);
    }

    #[inline]
    fn has_redraw(&self) -> bool {
        self.launcher.ctx.has_redraw()
    }

    #[inline]
    fn take_redraw(&mut self) -> Redraw {
        self.launcher.ctx.take_redraw()
    }

    #[inline]
    fn request_full_redraw(&mut self) {
        self.launcher.ctx.request_full_redraw();
    }

    #[inline]
    fn ctx_mut(&mut self) -> &mut AppContext {
        &mut self.launcher.ctx
    }

    fn system_settings(&self) -> &SystemSettings {
        self.settings.system_settings()
    }

    fn settings_generation(&self) -> u32 {
        self.settings.generation()
    }

    fn on_swap_buttons_changed(&mut self, swap: bool) {
        self.mapper.set_swap(swap);
        if self.bumps.set_swap(swap) {
            self.launcher.ctx.mark_dirty(crate::ui::Region::new(
                0,
                crate::board::SCREEN_H - crate::ui::BUTTON_BAR_H,
                crate::board::SCREEN_W,
                crate::ui::BUTTON_BAR_H,
            ));
        }
    }

    fn set_chrome_state(&mut self, battery_pct: u8, today_pages: u16, today_secs: u32) {
        // the bar carries the battery and the tab's name; the name only
        // changes on navigation, which repaints everything anyway
        let bar_changed = self.chrome.top.battery_pct != battery_pct;
        self.chrome.top.battery_pct = battery_pct;

        // today's reading is not chrome any more, and not Home's
        // either: the Stats screen reads it straight off the kernel
        // when it draws, so nothing here has to carry or invalidate it
        let _ = (today_pages, today_secs);

        let active = self.launcher.active();
        let show_top = with_app_ref!(active, self, |app| app.show_top_status());
        // in the reader (chrome hidden) the day-stat drain changes
        // today_secs after every page turn, and an unconditional mark
        // fired a ~400 ms DU repainting identical book pixels
        if show_top && bar_changed {
            plump_kernel::perf_event!("chrome", "bar_mark pct={}", battery_pct);
            let theme = Theme::default_v1();
            self.launcher.ctx.mark_dirty_coalesced(crate::ui::Region::new(
                0,
                0,
                crate::board::SCREEN_W,
                theme.top_bar_h,
            ));
        }
    }

    fn ghost_clear_every(&self) -> u32 {
        AppManager::ghost_clear_every(self)
    }

    fn grayscale_mode(&self) -> crate::kernel::app::GrayscaleMode {
        use crate::kernel::app::GrayscaleMode;
        if self.quick_menu.open {
            return GrayscaleMode::Disabled;
        }
        match self.launcher.active() {
            // reader page turns are deliberate + spaced; firing AA
            // back-to-back with the partial DU hides the latency well.
            AppId::Reader => {
                if self.reader.wants_grayscale() {
                    GrayscaleMode::Immediate
                } else {
                    GrayscaleMode::Disabled
                }
            }
            // home used Deferred AA, but AA is a whole-panel affair
            // (see kernel/screen.rs): every selection move would first
            // neutralize the codes (full revert + plane rewrite, ~450ms
            // before the row even moves) and then re-gray the whole
            // screen after the idle window. the windowed passes this
            // used to run are what darkened everything except the
            // selected row. plain BW keeps navigation at one DU
            AppId::Home => GrayscaleMode::Disabled,
            _ => GrayscaleMode::Disabled,
        }
    }

    fn load_eager_settings(&mut self, k: &mut KernelHandle<'_>) {
        AppManager::load_eager_settings(self, k);
    }

    fn load_initial_state(&mut self, k: &mut KernelHandle<'_>) {
        AppManager::load_home_recent(self, k);
    }

    fn enter_initial(&mut self, k: &mut KernelHandle<'_>) {
        AppManager::enter_initial(self, k);
    }

    fn save_active_state(&mut self, bm: &mut crate::kernel::bookmarks::BookmarkCache) {
        AppManager::save_active_state(self, bm);
    }

    fn on_active_pre_sleep(&mut self, k: &mut KernelHandle<'_>) {
        AppManager::on_active_pre_sleep(self, k);
    }

    fn has_sleep_overlay(&self) -> bool {
        self.sleep_card.is_set()
    }

    fn draw_sleep_overlay(&self, strip: &mut StripBuffer) {
        self.sleep_card.draw(strip);
    }

    fn flush_deferred_persistence(
        &mut self,
        k: &mut KernelHandle<'_>,
        reason: DeferredPersistenceReason,
    ) -> crate::error::Result<()> {
        AppManager::flush_deferred_persistence(self, k, reason)
    }

    fn collect_session(&self, session: &mut crate::kernel::rtc_session::RtcSession) {
        AppManager::collect_session(self, session);
    }

    fn apply_session(
        &mut self,
        session: &crate::kernel::rtc_session::RtcSession,
        k: &mut KernelHandle<'_>,
    ) -> bool {
        AppManager::apply_session(self, session, k)
    }

    fn needs_special_mode(&self) -> bool {
        self.launcher.active() == AppId::Upload
    }

    async fn run_special_mode(&mut self, screen: &mut Screen, sd: &SdStorage) -> ModeExit<AppId> {
        // Safety: WIFI is not owned by any other driver.  Upload mode
        // runs in isolation (the scheduler exits the main dispatch loop
        // first) and tears down the radio stack before returning.  The
        // peripheral is not accessed again until the next upload session.
        let wifi = unsafe { esp_hal::peripherals::WIFI::steal() };

        let exit = crate::apps::upload::run_upload_mode(
            wifi,
            screen,
            sd,
            self.settings.system_settings().ui_font_size_idx,
            &self.chrome,
            &self.mapper,
            self.settings.wifi_config(),
        )
        .await;

        let tab = match exit {
            UploadExit::Back => self.upload_return_tab,
            UploadExit::Tab(tab) => tab,
        };
        ModeExit::to(tab_to_appid(tab))
    }

    fn suppress_deferred_input(&self) -> bool {
        self.quick_menu.open
    }
}
