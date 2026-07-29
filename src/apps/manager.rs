// app lifecycle manager: nav stack, dispatch, font propagation, draw
//
// all dispatch is static (monomorphized via with_app!); no dyn, no vtable
// loading indicator is drawn between app content and overlays so it
// sits on top of page content but under quick menu and button bumps

use crate::apps::home::HomeApp;
use crate::apps::library::LibraryApp;
use crate::apps::reader::ReaderApp;
use crate::apps::settings::SettingsApp;
use crate::apps::stats::StatsApp;
use crate::apps::{
    App, AppContext, AppId, AppNav, AppNavSlot, BgBudget, BgOutcome, DeferredPersistenceReason,
    HDir, HResult, Launcher, Modal, PendingSetting, Redraw, Tab, Transition,
};

use crate::apps::upload::UploadExit;
use crate::apps::widgets::quick_menu::{MAX_APP_ACTIONS, QuickMenuResult};
use crate::apps::widgets::{ButtonFeedback, QuickMenu};
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
            mapper,
            nav: AppNav::new(Tab::Home),
            chrome: Chrome::new(),
            upload_return_tab: Tab::Home,
        }
    }

    /// Re-derive `self.nav` from the launcher stack. Call after every
    /// `launcher.apply()` or `launcher.restore_stack()`.
    #[inline]
    fn sync_nav_from_launcher(&mut self) {
        let (tab, modal) = nav_state_from_launcher(self.launcher);
        self.nav.restore(tab, modal);
        // chrome's tab bar always reflects the current tab.
        self.chrome.tabs.active = tab;
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
        let active = self.launcher.active();
        with_app!(active, self, |app| app.on_pre_sleep(k));
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

        // set loading indicator for reader if it's the active app,
        // so the first frame shows "Opening" instead of blank content
        if self.launcher.active() == AppId::Reader {
            self.launcher
                .ctx
                .set_loading(crate::apps::reader::LOADING_REGION, "Resuming", 0);
        }

        // mark full redraw needed — the next render will draw the
        // active app's content using the restored state
        self.launcher.ctx.request_full_redraw();

        log::debug!(
            "session: restore complete, active={:?}",
            self.launcher.active()
        );

        true
    }

    /// Open the quick menu for the active app.
    fn open_quick_menu(&mut self) {
        let active = self.launcher.active();
        let actions: &[_] = with_app!(active, self, |app| app.quick_actions());
        self.quick_menu.show(actions);
        self.launcher.ctx.mark_dirty(self.quick_menu.region());
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

            QuickMenuResult::AppTrigger(id) => {
                let active = self.launcher.active();
                self.close_quick_menu();

                with_app!(active, self, |app| {
                    app.on_quick_trigger(id, &mut self.launcher.ctx);
                    // Save app state after trigger (e.g. font change
                    // may invalidate the reader's current page offset).
                    app.save_state(bm_cache);
                });

                Transition::None
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
            } else {
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
        if show_top || show_tabs {
            let theme = Theme::default_v1();
            let text_font = fonts::chrome_font();
            let icon_font = fonts::icon_font(2);
            let mut painter = Painter::new(strip, &theme);
            if show_top {
                self.chrome.draw_top(&mut painter, text_font);
            }
            if show_tabs {
                self.chrome.draw_tabs(&mut painter, icon_font);
            }
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
        self.quick_menu.set_chrome_font(chrome);
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
        let prev_pct = self.chrome.top.battery_pct;
        let prev_pages = self.chrome.top.today_pages;
        let prev_secs = self.chrome.top.today_secs;
        self.chrome.top.battery_pct = battery_pct;
        self.chrome.top.today_pages = today_pages;
        self.chrome.top.today_secs = today_secs;
        // only invalidate when the active app actually shows the bar;
        // in the reader (chrome hidden) the day-stat drain changes
        // today_secs after every page turn, and an unconditional mark
        // fired a ~400ms DU partial repainting identical book pixels
        let active = self.launcher.active();
        let show_top = with_app_ref!(active, self, |app| app.show_top_status());
        // compare at displayed granularity: the bar renders time as
        // h:mm, so raw-second changes (which follow every page turn)
        // would repaint pixel-identical content
        if show_top
            && (prev_pct != battery_pct
                || prev_pages != today_pages
                || prev_secs / 60 != today_secs / 60)
        {
            // chrome top bar changed; queue a coalesced redraw so the
            // next paintable window picks it up. width spans the full
            // bar; height is the top chrome region.
            //
            // discriminator for the post-turn extra refresh: if this
            // fires right after a page turn's render, the day-stat
            // drain missed that render and the bar repaint rides its
            // own follow-up refresh (item 15 regression via item 8's
            // break-to-render)
            plump_kernel::perf_event!(
                "chrome",
                "bar_mark pct={} pages={} mins={}",
                battery_pct,
                today_pages,
                today_secs / 60
            );
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
            // home has frequent up/down navigation; defer AA so each
            // keypress stays snappy and the final settled view picks
            // up the AA pass.
            AppId::Home => GrayscaleMode::Deferred,
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
