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

pub mod layout;
pub mod model;

use embedded_graphics::pixelcolor::BinaryColor;

use crate::apps::{App, AppContext, AppId, BgBudget, BgOutcome, HDir, HResult, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::kernel::KernelHandle;
use crate::kernel::config::{self, SystemSettings, WifiConfig};
use crate::ui::{Alignment, CONTENT_TOP, Painter, Region, SectionLabel, SelectableRow, Theme};

use layout::{Damage, GUTTER_W, SettingsList, VALUE_W};
use model::{Activation, ROWS, Row, SettingId, Step, ValueFmt};

/// which layer owns Left/Right.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// the manager cycles tabs on Left/Right
    Browsing,
    /// the focused row consumes Left/Right until the session closes
    Editing(SettingId),
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
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
        self.list.set_line_height(self.ui_fonts.body.line_height);
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
        };
        damage.mark(ctx);
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

    fn draw_item(&self, p: &mut Painter<'_>, id: SettingId, row: Region, selected: bool, editing: bool) {
        let theme = *p.theme();
        let font = self.ui_fonts.body;
        let fg = SelectableRow::new(row, selected).draw_if_visible(p.strip_mut());

        let label_w = row.w.saturating_sub(VALUE_W + 2 * theme.margin_md);
        let label_r = Region::new(row.x + theme.margin_md, row.y, label_w, row.h);
        font.draw_aligned(p.strip_mut(), label_r, id.label(), Alignment::CenterLeft, fg);

        let mut value = ValueFmt::new();
        id.format(&self.settings, &mut value);
        let value_r = Region::new(
            row.x + row.w - VALUE_W - theme.margin_md,
            row.y,
            VALUE_W,
            row.h,
        );

        if !selected {
            font.draw_aligned(
                p.strip_mut(),
                value_r,
                value.as_str(),
                Alignment::CenterRight,
                fg,
            );
            return;
        }

        // the value on the focused row wears a chip: outlined under the
        // cursor (this is what Select acts on), filled while the edit
        // session is open (this is what Left/Right are changing). with
        // no chevrons anywhere it is the only cue for the mode, so the
        // two states have to look clearly different
        let text_w = font.measure_str(value.as_str());
        let chip_w = (text_w + 2 * theme.margin_md).min(value_r.w);
        let chip_h = row.h.saturating_sub(2 * theme.margin_sm);
        let chip = Region::new(
            value_r.x + value_r.w - chip_w,
            row.y + row.h.saturating_sub(chip_h) / 2,
            chip_w,
            chip_h,
        );
        p.with_fg(BinaryColor::Off).rounded_rect(chip, 4, editing);
        let text_fg = if editing { BinaryColor::On } else { fg };
        font.draw_aligned(p.strip_mut(), chip, value.as_str(), Alignment::Center, text_fg);
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
    fn on_enter(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        self.list.reset();
        self.focus = Focus::Browsing;
        ctx.mark_dirty(Region::new(0, CONTENT_TOP, SCREEN_W, SCREEN_H - CONTENT_TOP));
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
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
            return BgOutcome::Progress { more: false };
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

    fn draw(&self, strip: &mut StripBuffer) {
        let theme = Theme::default_v1();
        let font = self.ui_fonts.body;
        let metrics = self.list.metrics();

        if !self.loaded {
            let r = Region::new(layout::LIST_X, metrics.top, layout::LIST_W, metrics.row_h);
            font.draw_aligned(strip, r, "Loading...", Alignment::CenterLeft, BinaryColor::On);
            return;
        }

        let mut p = Painter::new(strip, &theme);
        let cursor = self.list.cursor();
        let editing = matches!(self.focus, Focus::Editing(_));
        let mut prev_was_item = false;

        for (idx, row, region) in self.list.rows() {
            match row {
                Row::Section(caption) => {
                    // the box carries the gap above the caption, so the
                    // text sits at its bottom
                    let text_r = Region::new(
                        region.x + theme.margin_md,
                        region.y + region.h.saturating_sub(metrics.caption_h),
                        region.w,
                        metrics.caption_h,
                    );
                    SectionLabel::new(text_r, caption).draw(&mut p, font);
                    prev_was_item = false;
                }
                Row::Item(id) => {
                    let selected = idx == cursor;
                    // hairline between neighbouring rows; an inverted
                    // row draws its own edge
                    if prev_was_item && !selected && idx.saturating_sub(1) != cursor {
                        p.hairline_h(
                            region.y,
                            region.x + theme.margin_md,
                            region.x + region.w - theme.margin_md,
                        );
                    }
                    self.draw_item(&mut p, *id, region, selected, editing && selected);
                    prev_was_item = true;
                }
            }
        }

        self.draw_scroll_thumb(&mut p);
    }
}
