// Settings tab.
//
// navigation:
//   Up / Down: move between settings (captions are skipped, the list
//     wraps at both ends).
//   Select: booleans flip in place; every other setting opens an edit
//     session on that row.
//   Left / Right: cycle tabs while browsing, change the open value
//     while editing. this split is the point of the edit session: the
//     manager intercepts Left/Right on every tab screen and only hands
//     them to the app when `on_horizontal` claims them, so before the
//     session existed no multi-valued setting could be changed at all.
//   Back: closes the session, or leaves the tab when browsing.
//
// the screen itself is described in `model` (what the settings are) and
// measured in `layout` (where the rows land). this file owns the input
// state machine, persistence, and drawing.

pub mod cache;
pub mod layout;
pub mod model;

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;

use crate::apps::{App, AppContext, AppId, BgBudget, BgOutcome, HDir, HResult, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::kernel::config::{self, SystemSettings, WifiConfig};
use crate::ui::{Alignment, CONTENT_TOP, Painter, Region, SectionLabel, Theme};

use crate::apps::widgets::row::{self, RowFonts, RowLead, RowSpec, ValueChip};

use plump_kernel::kernel::panel_lab::{self, Ask};

use cache::{CacheSheet, SheetResult};
use layout::{Damage, GUTTER_W, SettingsList};
use model::{Activation, Domain, InfoId, ROWS, Row, SettingId, Step, ValueFmt};

/// A group outline sits one pixel outside its rows, so the stroke
/// lands where a separator would and every row is framed at the same
/// distance above and below.
const fn grow_1(r: Region) -> Region {
    Region::new(r.x, r.y, r.w, r.h + 1)
}

/// which layer owns Left/Right.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// the manager cycles tabs on Left/Right
    Browsing,
    /// the focused row consumes Left/Right until the session closes
    Editing(SettingId),
}

/// Panel Lab input state.
///
/// The panel is held while a session runs, so there is no on-screen
/// UI: the log carries the experiment, the step, the question and the
/// candidate answers, and the operator picks with Up/Down and records
/// with Select.
#[derive(Clone, Copy)]
struct LabState {
    /// false outside a session
    active: bool,
    exp: u8,
    step: u8,
    /// index of the question being answered; equal to the step's ask
    /// count once every question is answered
    ask: u8,
    /// highlighted answer for the pending question
    answer: u8,
    /// every step of the experiment has run and been answered
    done: bool,
}

impl LabState {
    const fn new() -> Self {
        Self {
            active: false,
            exp: 0,
            step: 0,
            ask: 0,
            answer: 0,
            done: false,
        }
    }
}

pub struct SettingsApp {
    settings: SystemSettings,
    wifi: WifiConfig,
    list: SettingsList,
    focus: Focus,
    loaded: bool,
    save_needed: bool,
    generation: u32,
    ui_fonts: fonts::UiFonts,
    /// the Book Cache row's own surface; owns the picker, the two
    /// scopes and the clear in flight
    cache: CacheSheet,
    /// the Panel Lab's input state; inactive until its row opens
    lab: LabState,
    /// About group figures, sampled on the background budget: the
    /// battery is an ADC read the status bar already pays for, and
    /// the uptime is the monotonic clock
    bat_pct: u8,
    bat_mv: u16,
    uptime_secs: u32,
}

impl Default for SettingsApp {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsApp {
    pub fn new() -> Self {
        let ui_fonts = fonts::UiFonts::for_size(0);
        Self {
            settings: SystemSettings::defaults(),
            wifi: WifiConfig::empty(),
            list: SettingsList::new(ui_fonts.body.line_height),
            focus: Focus::Browsing,
            loaded: false,
            save_needed: false,
            generation: 0,
            ui_fonts,
            cache: CacheSheet::new(),
            lab: LabState::new(),
            bat_pct: 0,
            bat_mv: 0,
            uptime_secs: 0,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
        self.list.set_line_height(self.ui_fonts.body.line_height);
        self.cache.set_ui_font_size(idx);
    }

    pub fn system_settings(&self) -> &SystemSettings {
        &self.settings
    }

    pub fn system_settings_mut(&mut self) -> &mut SystemSettings {
        &mut self.settings
    }

    pub fn wifi_config(&self) -> &WifiConfig {
        &self.wifi
    }

    /// True while the Book Cache sheet owns the screen. The manager
    /// draws it after the shared chrome, the way it draws the quick
    /// menu: the status bar and tab bar win the painter's algorithm
    /// over app content, and would slice the sheet's edges.
    #[inline]
    pub fn cache_sheet_open(&self) -> bool {
        self.cache.is_open()
    }

    pub fn draw_cache_sheet(&self, strip: &mut StripBuffer) {
        self.cache.draw(strip);
    }

    pub fn mark_save_needed(&mut self) {
        self.save_needed = true;
        self.generation = self.generation.wrapping_add(1);
    }

    #[inline]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn load_eager(&mut self, k: &mut KernelHandle<'_>) {
        self.load(k);
        self.set_ui_font_size(self.settings.ui_font_size_idx);
    }

    fn load(&mut self, k: &mut KernelHandle<'_>) {
        let mut buf = [0u8; config::SETTINGS_BUF_CAP];

        self.settings = SystemSettings::defaults();
        self.wifi = WifiConfig::empty();

        match k
            .sd()
            .read_file_start_in_dir(k.sd().data_dir(), config::SETTINGS_FILE, &mut buf)
        {
            Ok((_size, n)) if n > 0 => {
                self.settings.parse_txt(&buf[..n], &mut self.wifi);
                self.settings.sanitize();
                log::debug!("settings: loaded from {}", config::SETTINGS_FILE);
            }
            _ => {
                log::debug!("settings: no file found, using defaults");
            }
        }

        self.loaded = true;
        self.generation = self.generation.wrapping_add(1);
    }

    fn save(&self, k: &mut KernelHandle<'_>) -> bool {
        let mut buf = [0u8; config::SETTINGS_BUF_CAP];
        let len = self.settings.write_txt(&self.wifi, &mut buf);
        match k
            .sd()
            .write_file_in_dir(k.sd().data_dir(), config::SETTINGS_FILE, &buf[..len])
        {
            Ok(_) => {
                log::info!("settings: saved to {}", config::SETTINGS_FILE);
                true
            }
            Err(e) => {
                log::error!("settings: save failed: {}", e);
                false
            }
        }
    }

    /// The open edit session, if there is one. Values are only
    /// reachable through this handle, so nothing can step a setting
    /// while the screen is in browsing mode.
    fn editor(&mut self) -> Option<Editor<'_>> {
        match self.focus {
            Focus::Editing(id) => Some(Editor { app: self, id }),
            Focus::Browsing => None,
        }
    }

    /// Persist and work out what a changed value invalidated. A UI font
    /// change re-measures every row, so it repaints the whole window
    /// rather than the value cell it was made in.
    fn on_value_changed(&mut self, id: SettingId) -> Damage {
        self.mark_save_needed();
        if id.resizes_ui() {
            self.set_ui_font_size(self.settings.ui_font_size_idx);
            Damage::Viewport(self.list.viewport())
        } else {
            Damage::Rows([self.list.selected_value_region(), None])
        }
    }

    // Select: flip booleans where they stand, open a session otherwise.
    fn activate(&mut self, ctx: &mut AppContext) {
        if let Some(editor) = self.editor() {
            editor.close().mark(ctx);
            return;
        }

        let id = self.list.selected();
        let damage = match id.activation() {
            Activation::Flip => {
                if id.step(&mut self.settings, Step::Up) {
                    self.on_value_changed(id)
                } else {
                    Damage::None
                }
            }
            Activation::Edit => {
                self.focus = Focus::Editing(id);
                Damage::Rows([self.list.selected_row_region(), None])
            }
            Activation::Open if id == SettingId::PanelLab => {
                self.lab_enter(ctx);
                Damage::None
            }
            Activation::Open => {
                self.cache.open();
                // the sheet covers all but the margins and replaces
                // every pixel under it. a DU that wide is a delta
                // against the whole settings screen, which the panel
                // then holds as a ghost showing through the paper
                ctx.request_full_redraw();
                Damage::None
            }
        };
        damage.mark(ctx);
    }

    /// While the Book Cache sheet is up it owns every press: Up/Down
    /// walk its rows, Select goes forward, Back comes back one stage
    /// (and out of the sheet from the first), Menu closes it. The tab
    /// bar is unreachable until it does, which is what `captures_menu`
    /// and the `on_horizontal` short-circuit are for.
    fn dispatch_to_cache(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        let result = match event {
            ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                self.cache.on_vertical(true, ctx)
            }
            ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                self.cache.on_vertical(false, ctx)
            }
            ActionEvent::Press(Action::Select) => self.cache.on_select(ctx),
            ActionEvent::Press(Action::Back) => self.cache.on_back(ctx),
            ActionEvent::Press(Action::Menu) => self.cache.on_menu(ctx),
            // a long Back leaves the tab outright, sheet and all
            ActionEvent::LongPress(Action::Back) => {
                self.cache.close();
                return Transition::Home;
            }
            _ => SheetResult::Consumed,
        };
        debug_assert!(
            !matches!(result, SheetResult::Closed) || !self.cache.is_open(),
            "sheet reported Closed while still open"
        );
        Transition::None
    }

    // ── panel lab ───────────────────────────────────────────────────
    //
    // the panel is held for the whole session, so every result and
    // every prompt goes to the log. the ops themselves live in the
    // kernel (`kernel::panel_lab`); this is only the input state. the
    // prompts are emitted through `panel_lab::dialogue` rather than
    // `log::info!` so they land under a log target a dev ESP_LOG
    // actually enables (see that function).

    fn lab_enter(&mut self, ctx: &mut AppContext) {
        self.lab = LabState {
            active: true,
            ..LabState::new()
        };
        panel_lab::dialogue(format_args!(
            "panel: lab started, {} experiments; Up/Down picks one, Select runs the next step, Back exits",
            panel_lab::EXPERIMENTS.len()
        ));
        self.lab_run(ctx);
    }

    fn lab_run(&mut self, ctx: &mut AppContext) {
        let Some(e) = panel_lab::EXPERIMENTS.get(self.lab.exp as usize) else {
            return;
        };
        let Some(s) = e.steps.get(self.lab.step as usize) else {
            return;
        };
        self.lab.ask = 0;
        self.lab.answer = 0;
        self.lab.done = false;
        panel_lab::dialogue(format_args!(
            "panel: experiment {} '{}' ({})",
            self.lab.exp + 1,
            e.name,
            e.sub
        ));
        panel_lab::dialogue(format_args!(
            "panel: step {}/{}: {}",
            self.lab.step + 1,
            e.steps.len(),
            s.op.name()
        ));
        if s.asks.is_empty() {
            panel_lab::dialogue(format_args!(
                "panel: no question; Select runs the next step"
            ));
        } else {
            self.lab_log_ask(&s.asks[0]);
        }
        ctx.request_panel_lab(self.lab.exp, self.lab.step);
    }

    fn lab_log_ask(&self, ask: &Ask) {
        panel_lab::dialogue(format_args!("panel: Q {}", ask.question));
        for (i, a) in ask.answers.iter().enumerate() {
            let mark = if i as u8 == self.lab.answer { ">" } else { " " };
            panel_lab::dialogue(format_args!("panel:  {} [{}] {}", mark, i + 1, a));
        }
        panel_lab::dialogue(format_args!("panel: Up/Down chooses, Select records"));
    }

    fn lab_asks(&self) -> &'static [Ask] {
        panel_lab::EXPERIMENTS
            .get(self.lab.exp as usize)
            .and_then(|e| e.steps.get(self.lab.step as usize))
            .map(|s| s.asks)
            .unwrap_or(&[])
    }

    fn lab_select(&mut self, ctx: &mut AppContext) {
        if self.lab.done {
            panel_lab::dialogue(format_args!(
                "panel: experiment done; Up/Down picks another, Back exits"
            ));
            return;
        }
        let asks = self.lab_asks();
        if (self.lab.ask as usize) < asks.len() {
            let ans = asks[self.lab.ask as usize]
                .answers
                .get(self.lab.answer as usize)
                .copied()
                .unwrap_or("?");
            panel_lab::dialogue(format_args!("panel: answer {} = {}", self.lab.ask + 1, ans));
            self.lab.ask += 1;
            if (self.lab.ask as usize) < asks.len() {
                self.lab.answer = 0;
                self.lab_log_ask(&asks[self.lab.ask as usize]);
                return;
            }
        }
        self.lab_advance(ctx);
    }

    fn lab_advance(&mut self, ctx: &mut AppContext) {
        let Some(e) = panel_lab::EXPERIMENTS.get(self.lab.exp as usize) else {
            return;
        };
        if (self.lab.step as usize) + 1 < e.steps.len() {
            self.lab.step += 1;
            self.lab_run(ctx);
        } else {
            self.lab.done = true;
            panel_lab::dialogue(format_args!(
                "panel: experiment {} '{}' complete; Up/Down picks another, Back exits",
                self.lab.exp + 1,
                e.name
            ));
        }
    }

    fn lab_vertical(&mut self, down: bool, ctx: &mut AppContext) {
        let asks = self.lab_asks();
        if !self.lab.done && (self.lab.ask as usize) < asks.len() {
            let ask = &asks[self.lab.ask as usize];
            let n = ask.answers.len() as u8;
            if n > 0 {
                self.lab.answer = if down {
                    (self.lab.answer + 1) % n
                } else {
                    (self.lab.answer + n - 1) % n
                };
            }
            self.lab_log_ask(ask);
            return;
        }
        let n = panel_lab::EXPERIMENTS.len() as u8;
        if n > 0 {
            self.lab.exp = if down {
                (self.lab.exp + 1) % n
            } else {
                (self.lab.exp + n - 1) % n
            };
        }
        self.lab.step = 0;
        self.lab_run(ctx);
    }

    fn lab_exit(&mut self, ctx: &mut AppContext) {
        self.lab.active = false;
        panel_lab::dialogue(format_args!("panel: lab ended"));
        ctx.end_panel_lab();
    }

    // Up / Down: move the cursor, or step the open value. leaving an
    // edited row is what Select and Back are for.
    fn vertical(&mut self, dir: Step, ctx: &mut AppContext) {
        let damage = match self.focus {
            Focus::Editing(_) => self
                .editor()
                .map(|mut editor| editor.step(dir))
                .unwrap_or(Damage::None),
            Focus::Browsing => self.list.move_cursor(dir),
        };
        damage.mark(ctx);
    }

    /// One setting. The chip on the focused row is the cue for what
    /// Select acts on, and it fills once the edit session has Left and
    /// Right; everything else about the row is the shared anatomy.
    #[allow(clippy::too_many_arguments)]
    fn draw_item(
        &self,
        strip: &mut StripBuffer,
        fonts: &RowFonts,
        id: SettingId,
        region: Region,
        edges: row::RowEdges,
        selected: bool,
        editing: bool,
    ) {
        let mut value = ValueFmt::new();
        let opens_surface = matches!(id.domain(), Domain::Action);
        if opens_surface {
            // the lab has no figure of its own, and the cache row's
            // byte count would be nonsense on it
            match id {
                SettingId::PanelLab => {
                    let _ = write!(value, "run \u{2192}");
                }
                _ => self.cache.row_value(&mut value),
            }
        } else {
            id.format(&self.settings, &mut value);
        }

        // a row with no value to edit never wears the chip: the arrow
        // in its value already says the press opens something
        let chip = match (selected, opens_surface, editing) {
            (false, _, _) | (_, true, _) => ValueChip::None,
            (true, false, false) => ValueChip::Cursor,
            (true, false, true) => ValueChip::Editing,
        };

        row::draw(
            strip,
            region,
            edges,
            fonts,
            &RowSpec {
                lead: RowLead::None,
                text: id.label(),
                text_font: fonts.text,
                value: value.as_str(),
                selected,
                sub: id.sub(),
                progress: None,
                chip,
            },
        );
    }

    /// Read the two live figures the About group shows. Both are
    /// already computed elsewhere every few seconds (the status bar's
    /// battery, the stats line's uptime), so this is a lookup.
    fn sample_device_figures(&mut self, k: &mut KernelHandle<'_>) {
        self.bat_mv = k.battery_mv();
        self.bat_pct = crate::drivers::battery::battery_percentage(self.bat_mv);
        self.uptime_secs = plump_kernel::kernel::wake::uptime_secs();
    }

    /// The About group's figures: what the device knows about itself
    /// without reading anything new off the card.
    fn info_value(&self, id: InfoId, out: &mut ValueFmt) {
        out.clear();
        match id {
            // the one thing a settings screen is always asked for,
            // free from the manifest at compile time
            InfoId::Version => {
                let _ = out.write_str(env!("CARGO_PKG_VERSION"));
            }
            InfoId::Storage => self.cache.row_value_plain(out),
            InfoId::Battery => {
                let _ = write!(
                    out,
                    "{}% \u{00B7} {}.{:02} V",
                    self.bat_pct,
                    self.bat_mv / 1000,
                    (self.bat_mv % 1000) / 10
                );
            }
            InfoId::Uptime => {
                let secs = self.uptime_secs;
                let hours = secs / 3600;
                let mins = (secs % 3600) / 60;
                if hours > 0 {
                    let _ = write!(out, "{}h {}m", hours, mins);
                } else {
                    let _ = write!(out, "{}m", mins);
                }
            }
        }
    }

    // thin thumb in the reserved right gutter; only drawn when the list
    // is taller than the window
    fn draw_scroll_thumb(&self, p: &mut Painter<'_>) {
        let Some((first, last)) = self.list.visible_span() else {
            return;
        };
        let viewport = self.list.viewport();
        let total = ROWS.len() as u32;
        let track_h = viewport.h as u32;
        let top = (first as u32 * track_h / total) as u16;
        let bottom = ((last as u32 + 1) * track_h / total) as u16;
        let thumb = Region::new(
            viewport.x + viewport.w - GUTTER_W + 2,
            viewport.y + top,
            2,
            bottom.saturating_sub(top).max(8),
        );
        p.fill_in(thumb, BinaryColor::On);
    }
}

/// An open edit session over one setting.
///
/// Only obtainable from [`SettingsApp::editor`], which yields `None`
/// while browsing, so the value-stepping methods do not exist outside a
/// session. Same idea as the screen's `Wave`: the borrow marks a mode
/// the rest of the code cannot accidentally be in.
struct Editor<'a> {
    app: &'a mut SettingsApp,
    id: SettingId,
}

impl Editor<'_> {
    /// Step the focused value one notch.
    fn step(&mut self, dir: Step) -> Damage {
        if !self.id.step(&mut self.app.settings, dir) {
            return Damage::None;
        }
        self.app.on_value_changed(self.id)
    }

    /// Close the session, handing Left/Right back to the tab bar.
    fn close(self) -> Damage {
        self.app.focus = Focus::Browsing;
        Damage::Rows([self.app.list.selected_row_region(), None])
    }
}

impl App<AppId> for SettingsApp {
    fn on_enter(&mut self, ctx: &mut AppContext, k: &mut KernelHandle<'_>) {
        self.list.reset();
        self.focus = Focus::Browsing;
        self.cache.close();
        self.cache.request_scan();
        self.lab.active = false;
        // the About figures are sampled here rather than tracked: a
        // clock that repaints itself costs a DU a minute and tells
        // nobody anything they cannot get by leaving and coming back
        self.sample_device_figures(k);
        ctx.mark_dirty(Region::new(0, CONTENT_TOP, SCREEN_W, SCREEN_H - CONTENT_TOP));
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        if self.cache.is_open() {
            return self.dispatch_to_cache(event, ctx);
        }
        if self.lab.active {
            match event {
                ActionEvent::Press(Action::Select) => self.lab_select(ctx),
                ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                    self.lab_vertical(true, ctx)
                }
                ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                    self.lab_vertical(false, ctx)
                }
                ActionEvent::Press(Action::Back) | ActionEvent::LongPress(Action::Back) => {
                    self.lab_exit(ctx)
                }
                _ => {}
            }
            return Transition::None;
        }

        match event {
            ActionEvent::LongPress(Action::Back) => Transition::Home,

            ActionEvent::Press(Action::Back) => match self.editor() {
                Some(editor) => {
                    editor.close().mark(ctx);
                    Transition::None
                }
                None => Transition::Pop,
            },

            ActionEvent::Press(Action::Select) => {
                self.activate(ctx);
                Transition::None
            }

            ActionEvent::Press(Action::Next) | ActionEvent::Repeat(Action::Next) => {
                self.vertical(Step::Down, ctx);
                Transition::None
            }

            ActionEvent::Press(Action::Prev) | ActionEvent::Repeat(Action::Prev) => {
                self.vertical(Step::Up, ctx);
                Transition::None
            }

            _ => Transition::None,
        }
    }

    fn on_horizontal(&mut self, dir: HDir, ctx: &mut AppContext) -> HResult {
        // the sheet navigates on Up/Down and OK alone, but it must not
        // let a stray Left/Right cycle the tab out from under it; the
        // panel lab owns the panel outright while it runs
        if self.cache.is_open() || self.lab.active {
            return HResult::Consumed;
        }
        match self.editor() {
            Some(mut editor) => {
                editor.step(Step::from_hdir(dir)).mark(ctx);
                HResult::Consumed
            }
            // no session open: let the manager cycle tabs
            None => HResult::AtEdge,
        }
    }

    fn background_step(
        &mut self,
        ctx: &mut AppContext,
        k: &mut KernelHandle<'_>,
        _budget: BgBudget,
    ) -> BgOutcome {
        if !self.loaded {
            self.load(k);
            self.set_ui_font_size(self.settings.ui_font_size_idx);
            ctx.request_full_redraw();
            return BgOutcome::Progress {
                more: self.save_needed,
            };
        }

        if self.save_needed && self.save(k) {
            self.save_needed = false;
            return BgOutcome::Progress {
                more: self.cache.has_work(),
            };
        }

        if self.cache.background_step(ctx, k) {
            // the row's total lands in one repaint when the scan
            // finishes, rather than once per book sized
            if self.cache.take_row_dirty()
                && let Some(idx) = model::row_index(SettingId::BookCache)
                && let Some(r) = self.list.row_region(idx)
            {
                ctx.mark_dirty(r);
            }
            return BgOutcome::Progress {
                more: self.cache.has_work(),
            };
        }

        BgOutcome::Idle
    }

    /// Retry a pending write after the user has navigated away; without
    /// this a change made just before leaving the tab waits for the next
    /// visit to reach the card.
    fn background_suspended_step(
        &mut self,
        k: &mut KernelHandle<'_>,
        _budget: BgBudget,
    ) -> BgOutcome {
        if self.loaded && self.save_needed && self.save(k) {
            self.save_needed = false;
            return BgOutcome::Progress { more: false };
        }
        BgOutcome::Idle
    }

    fn captures_menu(&self) -> bool {
        self.cache.is_open() || self.lab.active
    }

    fn draw(&self, strip: &mut StripBuffer) {
        let theme = Theme::default_v1();
        let font = self.ui_fonts.body;
        let metrics = self.list.metrics();

        if !self.loaded {
            let r = Region::new(layout::LIST_X, metrics.top, layout::LIST_W, metrics.row_h);
            font.draw_aligned(strip, r, "Loading...", Alignment::CenterLeft, BinaryColor::On);
            return;
        }

        let cursor = self.list.cursor();
        let editing = matches!(self.focus, Focus::Editing(_));
        let row_fonts = RowFonts {
            text: font,
            small: fonts::chrome_font(),
            icon: fonts::icon_font(1),
        };

        // one outline per run of rows between captions, drawn from the
        // union of the run's visible rows: a run scrolled off the top
        // or bottom is framed by what is on screen, and its stroke
        // falls outside the window rather than cutting a row in half
        let mut run: Option<(usize, Region)> = None;
        for (idx, row, region) in self.list.rows() {
            if matches!(row, Row::Section(_)) {
                continue;
            }
            let (first, _) = layout::SettingsList::group_run(idx);
            match run.as_mut() {
                Some((run_first, acc)) if *run_first == first => *acc = acc.union(region),
                _ => {
                    if let Some((_, acc)) = run {
                        row::draw_group_outline(strip, grow_1(acc));
                    }
                    run = Some((first, region));
                }
            }
        }
        if let Some((_, acc)) = run {
            row::draw_group_outline(strip, grow_1(acc));
        }

        {
            let mut p = Painter::new(strip, &theme);
            for (idx, row, region) in self.list.rows() {
                let Row::Section(caption) = row else {
                    continue;
                };
                // the box carries the gap above the caption and the
                // pad below it, so the text sits between the two
                let text_r = Region::new(
                    region.x + theme.margin_md,
                    region.y.saturating_add(
                        region
                            .h
                            .saturating_sub(metrics.caption_h + metrics.caption_pad),
                    ),
                    region.w,
                    metrics.caption_h,
                );
                SectionLabel::new(text_r, caption).draw(&mut p, font);
                let _ = idx;
            }
            self.draw_scroll_thumb(&mut p);
        }

        for (idx, row, region) in self.list.rows() {
            let (first, last) = match row {
                Row::Section(_) => continue,
                _ => layout::SettingsList::group_run(idx),
            };
            let edges = row::RowEdges {
                first: idx == first,
                last: idx == last,
            };
            match row {
                Row::Section(_) => {}
                Row::Item(id) => {
                    let selected = idx == cursor;
                    self.draw_item(strip, &row_fonts, *id, region, edges, selected, editing);
                }
                Row::Info(id) => {
                    let mut value = ValueFmt::new();
                    self.info_value(*id, &mut value);
                    row::draw(
                        strip,
                        region,
                        edges,
                        &row_fonts,
                        &RowSpec::label(id.label(), value.as_str(), font),
                    );
                }
            }
        }
    }
}
