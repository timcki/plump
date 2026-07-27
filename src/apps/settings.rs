// settings app UI; configuration types live in kernel::config
//
// settings items (12 total):
//   0: Sleep After    : power management
//   1: Ghost Clear    : e-paper refresh interval
//   2: Book Font      : reading font size
//   3: Reader Font    : Bookerly / Atkinson Hyperlegible
//   4: UI Font        : chrome font size
//   5: Reading Theme  : Compact / Default / Relaxed / Spacious (margins)
//   6: Swap Buttons   : swap Back/OK with Left/Right for left-handed use
//   7: Sunlight Fix   : power off analog after partial refresh (prevents fading)
//   8: Text AA        : antialiased text via 4-level grayscale LUT
//   9: Reader Status  : show book title + page info bar
//  10: Text Align     : Left or Justify
//  11: Line Spacing   : 1.30x to 2.00x of the em size

use core::fmt::Write as _;

use crate::apps::{App, AppContext, AppId, Transition};
use crate::board::action::{Action, ActionEvent};
use crate::board::{SCREEN_H, SCREEN_W};

use crate::drivers::strip::StripBuffer;
use crate::fonts;
use crate::fonts::max_size_idx;
use crate::kernel::KernelHandle;
use crate::kernel::config::{
    self, GHOST_CLEAR_STEP, MAX_GHOST_CLEAR, MAX_SLEEP_TIMEOUT, MIN_GHOST_CLEAR, NUM_LINE_SPACINGS,
    NUM_READER_FONTS, NUM_READING_THEMES, NUM_TEXT_ALIGNMENTS, SLEEP_TIMEOUT_STEP, SystemSettings,
    WifiConfig,
};
use crate::ui::{
    Alignment, BUTTON_BAR_H, BitmapLabel, CONTENT_TOP, FULL_CONTENT_W, LARGE_MARGIN, Region,
    SECTION_GAP, StackFmt, TITLE_Y, wrap_next, wrap_prev,
};

// layout constants
const ROW_H: u16 = 40;
const ROW_GAP: u16 = 6;
const ROW_STRIDE: u16 = ROW_H + ROW_GAP;

const LABEL_X: u16 = LARGE_MARGIN;
const LABEL_W: u16 = 160;
const COL_GAP: u16 = 8;
const VALUE_X: u16 = LABEL_X + LABEL_W + COL_GAP;
const VALUE_W: u16 = FULL_CONTENT_W - LABEL_W - COL_GAP;

const NUM_ITEMS: usize = 12;
const HEADING_ITEMS_GAP: u16 = SECTION_GAP;

// reorder rows to match the mockup grouping (READING / DISPLAY / SYSTEM).
// visual position -> internal logical id (used by item_label /
// format_value / increment / decrement match arms).
const VISUAL_TO_LOGICAL: [usize; NUM_ITEMS] = [
    3,  // Reader Font          | READING
    2,  // Book Font
    11, // Line Spacing
    5,  // Theme
    4,  // UI Font
    9,  // Reader Status
    10, // Text Align
    1,  // Ghost Clear          | DISPLAY
    7,  // Sunlight Fix
    8,  // Text AA
    0,  // Sleep After          | SYSTEM
    6,  // Swap Buttons
];

// visual indices at which a new section caption is rendered.
const SECTION_AT: &[(usize, &str)] = &[
    (0, "READING"),
    (7, "DISPLAY"),
    (10, "SYSTEM"),
];

const CAPTION_H: u16 = 14;

impl Default for SettingsApp {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SettingsApp {
    settings: SystemSettings,
    wifi: WifiConfig,
    selected: usize,
    scroll: usize,
    loaded: bool,
    save_needed: bool,
    generation: u32,
    ui_fonts: fonts::UiFonts,
    items_top: u16,
}

impl SettingsApp {
    pub fn new() -> Self {
        let uf = fonts::UiFonts::for_size(0);
        Self {
            settings: SystemSettings::defaults(),
            wifi: WifiConfig::empty(),
            selected: 0,
            scroll: 0,
            loaded: false,
            save_needed: false,
            generation: 0,
            ui_fonts: uf,
            items_top: TITLE_Y + uf.heading.line_height + HEADING_ITEMS_GAP,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_fonts = fonts::UiFonts::for_size(idx);
        self.items_top = TITLE_Y + self.ui_fonts.heading.line_height + HEADING_ITEMS_GAP;
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

        match k.sd().read_file_start_in_dir(k.sd().data_dir(), config::SETTINGS_FILE, &mut buf) {
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
        match k.sd().write_file_in_dir(k.sd().data_dir(), config::SETTINGS_FILE, &buf[..len]) {
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

    // visible items: how many rows fit between items_top and BUTTON_BAR_H
    fn visible_items(&self) -> usize {
        let avail = SCREEN_H.saturating_sub(self.items_top + BUTTON_BAR_H);
        let count = (avail / ROW_STRIDE) as usize;
        count.clamp(1, NUM_ITEMS)
    }

    // item labels and values:

    fn item_label(i: usize) -> &'static str {
        match i {
            0 => "Sleep After",
            1 => "Ghost Clear",
            2 => "Book Font",
            3 => "Reader Font",
            4 => "UI Font",
            5 => "Theme",
            6 => "Swap Buttons",
            7 => "Sunlight Fix",
            8 => "Text AA",
            9 => "Reader Status",
            10 => "Text Align",
            11 => "Line Spacing",
            _ => "",
        }
    }

    fn format_value(&self, i: usize, buf: &mut StackFmt<20>) {
        buf.clear();
        match i {
            0 => {
                if self.settings.sleep_timeout == 0 {
                    let _ = write!(buf, "Never");
                } else {
                    let _ = write!(buf, "{} min", self.settings.sleep_timeout);
                }
            }
            1 => {
                let _ = write!(buf, "Every {}", self.settings.ghost_clear_every);
            }
            2 => {
                let _ = write!(
                    buf,
                    "{}",
                    fonts::font_size_name(self.settings.book_font_size_idx)
                );
            }
            3 => {
                let idx = (self.settings.reader_font as usize)
                    .min(fonts::READER_FONT_NAMES.len() - 1);
                let _ = write!(buf, "{}", fonts::READER_FONT_NAMES[idx]);
            }
            4 => {
                let _ = write!(
                    buf,
                    "{}",
                    fonts::font_size_name(self.settings.ui_font_size_idx)
                );
            }
            5 => {
                let theme = self.settings.reading_theme();
                let _ = write!(buf, "{}", theme.name);
            }
            6 => {
                let _ = write!(
                    buf,
                    "{}",
                    if self.settings.swap_buttons {
                        "Yes"
                    } else {
                        "No"
                    }
                );
            }
            7 => {
                let _ = write!(
                    buf,
                    "{}",
                    if self.settings.sunlight_fix {
                        "On"
                    } else {
                        "Off"
                    }
                );
            }
            8 => {
                let _ = write!(buf, "{}", if self.settings.text_aa { "On" } else { "Off" });
            }
            9 => {
                let _ = write!(
                    buf,
                    "{}",
                    if self.settings.reader_status {
                        "Show"
                    } else {
                        "Hide"
                    }
                );
            }
            10 => {
                let _ = write!(buf, "{}", self.settings.text_alignment_name());
            }
            11 => {
                let pct = config::line_spacing_pct(self.settings.line_spacing);
                let _ = write!(buf, "{}.{:02}x", pct / 100, pct % 100);
            }
            _ => {}
        }
    }

    // increment/decrement:

    fn increment(&mut self) {
        match VISUAL_TO_LOGICAL[self.selected] {
            0 => {
                self.settings.sleep_timeout = match self.settings.sleep_timeout {
                    0 => SLEEP_TIMEOUT_STEP,
                    t if t >= MAX_SLEEP_TIMEOUT => MAX_SLEEP_TIMEOUT,
                    t => t + SLEEP_TIMEOUT_STEP,
                };
            }
            1 => {
                self.settings.ghost_clear_every = self
                    .settings
                    .ghost_clear_every
                    .saturating_add(GHOST_CLEAR_STEP)
                    .min(MAX_GHOST_CLEAR);
            }
            2 => {
                if self.settings.book_font_size_idx < max_size_idx() {
                    self.settings.book_font_size_idx += 1;
                }
            }
            3 => {
                if self.settings.reader_font < NUM_READER_FONTS - 1 {
                    self.settings.reader_font += 1;
                }
            }
            4 => {
                if self.settings.ui_font_size_idx < max_size_idx() {
                    self.settings.ui_font_size_idx += 1;
                }
            }
            5 => {
                if self.settings.reading_theme < NUM_READING_THEMES - 1 {
                    self.settings.reading_theme += 1;
                }
            }
            6 => {
                self.settings.swap_buttons = !self.settings.swap_buttons;
            }
            7 => {
                self.settings.sunlight_fix = !self.settings.sunlight_fix;
            }
            8 => {
                self.settings.text_aa = !self.settings.text_aa;
            }
            9 => {
                self.settings.reader_status = !self.settings.reader_status;
            }
            10 => {
                if self.settings.text_alignment < NUM_TEXT_ALIGNMENTS - 1 {
                    self.settings.text_alignment += 1;
                }
            }
            11 => {
                if self.settings.line_spacing < NUM_LINE_SPACINGS - 1 {
                    self.settings.line_spacing += 1;
                }
            }
            _ => return,
        }
        self.mark_save_needed();
    }

    fn decrement(&mut self) {
        match VISUAL_TO_LOGICAL[self.selected] {
            0 => {
                self.settings.sleep_timeout = match self.settings.sleep_timeout {
                    t if t <= SLEEP_TIMEOUT_STEP => 0,
                    t => t - SLEEP_TIMEOUT_STEP,
                };
            }
            1 => {
                self.settings.ghost_clear_every = self
                    .settings
                    .ghost_clear_every
                    .saturating_sub(GHOST_CLEAR_STEP)
                    .max(MIN_GHOST_CLEAR);
            }
            2 => {
                if self.settings.book_font_size_idx > 0 {
                    self.settings.book_font_size_idx -= 1;
                }
            }
            3 => {
                if self.settings.reader_font > 0 {
                    self.settings.reader_font -= 1;
                }
            }
            4 => {
                if self.settings.ui_font_size_idx > 0 {
                    self.settings.ui_font_size_idx -= 1;
                }
            }
            5 => {
                if self.settings.reading_theme > 0 {
                    self.settings.reading_theme -= 1;
                }
            }
            6 => {
                self.settings.swap_buttons = !self.settings.swap_buttons;
            }
            7 => {
                self.settings.sunlight_fix = !self.settings.sunlight_fix;
            }
            8 => {
                self.settings.text_aa = !self.settings.text_aa;
            }
            9 => {
                self.settings.reader_status = !self.settings.reader_status;
            }
            10 => {
                if self.settings.text_alignment > 0 {
                    self.settings.text_alignment -= 1;
                }
            }
            11 => {
                if self.settings.line_spacing > 0 {
                    self.settings.line_spacing -= 1;
                }
            }
            _ => return,
        }
        self.mark_save_needed();
    }

    // scroll management:

    fn scroll_into_view(&mut self) {
        let vis = self.visible_items();
        crate::apps::widgets::list::ensure_visible(self.selected, &mut self.scroll, vis);
    }

    // row region helpers (visible_idx = position on screen, 0 = first visible):

    #[inline]
    fn value_region(&self, visible_idx: usize) -> Region {
        Region::new(
            VALUE_X,
            self.items_top + visible_idx as u16 * ROW_STRIDE,
            VALUE_W,
            ROW_H,
        )
    }

    #[inline]
    fn row_region(&self, visible_idx: usize) -> Region {
        Region::new(
            LABEL_X,
            self.items_top + visible_idx as u16 * ROW_STRIDE,
            LABEL_W + COL_GAP + VALUE_W,
            ROW_H,
        )
    }

    fn list_region(&self) -> Region {
        let vis = self.visible_items();
        Region::new(
            LABEL_X,
            self.items_top,
            LABEL_W + COL_GAP + VALUE_W,
            vis as u16 * ROW_STRIDE,
        )
    }
}

impl App<AppId> for SettingsApp {
    fn on_enter(&mut self, ctx: &mut AppContext, _k: &mut KernelHandle<'_>) {
        self.selected = 0;
        self.scroll = 0;
        self.save_needed = false;
        ctx.mark_dirty(Region::new(
            0,
            CONTENT_TOP,
            SCREEN_W,
            SCREEN_H - CONTENT_TOP,
        ));
    }

    fn on_event(&mut self, event: ActionEvent, ctx: &mut AppContext) -> Transition {
        let vis = self.visible_items();

        match event {
            ActionEvent::Press(Action::Back) => Transition::Pop,
            ActionEvent::LongPress(Action::Back) => Transition::Home,

            ActionEvent::Press(Action::Next) => {
                let old_selected = self.selected;
                let old_scroll = self.scroll;
                self.selected = wrap_next(self.selected, NUM_ITEMS);
                if self.selected < old_selected {
                    self.scroll = 0;
                } else {
                    self.scroll_into_view();
                }
                if self.scroll != old_scroll {
                    ctx.mark_dirty(self.list_region());
                } else if self.selected != old_selected {
                    let old_vis = old_selected - old_scroll;
                    let new_vis = self.selected - self.scroll;
                    ctx.mark_dirty(self.row_region(old_vis));
                    ctx.mark_dirty(self.row_region(new_vis));
                }
                Transition::None
            }

            ActionEvent::Press(Action::Prev) => {
                let old_selected = self.selected;
                let old_scroll = self.scroll;
                self.selected = wrap_prev(self.selected, NUM_ITEMS);
                if self.selected > old_selected {
                    self.scroll = NUM_ITEMS.saturating_sub(vis);
                } else {
                    self.scroll_into_view();
                }
                if self.scroll != old_scroll {
                    ctx.mark_dirty(self.list_region());
                } else if self.selected != old_selected {
                    let old_vis = old_selected - old_scroll;
                    let new_vis = self.selected - self.scroll;
                    ctx.mark_dirty(self.row_region(old_vis));
                    ctx.mark_dirty(self.row_region(new_vis));
                }
                Transition::None
            }

            ActionEvent::Press(Action::NextJump) | ActionEvent::Repeat(Action::NextJump) => {
                self.increment();
                let v = self.selected - self.scroll;
                ctx.mark_dirty(self.value_region(v));
                Transition::None
            }

            ActionEvent::Press(Action::PrevJump) | ActionEvent::Repeat(Action::PrevJump) => {
                self.decrement();
                let v = self.selected - self.scroll;
                ctx.mark_dirty(self.value_region(v));
                Transition::None
            }

            _ => Transition::None,
        }
    }

    fn background_step(
        &mut self,
        ctx: &mut AppContext,
        k: &mut KernelHandle<'_>,
        _budget: crate::apps::BgBudget,
    ) -> crate::apps::BgOutcome {
        if !self.loaded {
            self.load(k);
            ctx.request_full_redraw();
            return crate::apps::BgOutcome::Progress {
                more: self.save_needed,
            };
        }

        if self.save_needed && self.save(k) {
            self.save_needed = false;
            return crate::apps::BgOutcome::Progress { more: false };
        }

        crate::apps::BgOutcome::Idle
    }

    fn draw(&self, strip: &mut StripBuffer) {
        if !self.loaded {
            let r = Region::new(LABEL_X, self.items_top, 200, ROW_H);
            BitmapLabel::new(r, "Loading...", self.ui_fonts.body)
                .alignment(Alignment::CenterLeft)
                .draw(strip)
                .unwrap();
            return;
        }

        // draw visible settings rows. each visible row at visual index
        // `item_idx` dispatches through VISUAL_TO_LOGICAL to look up
        // the underlying setting. captions are interleaved when a
        // section starts at the current visual position.
        let vis = self.visible_items();
        let visible_count = vis.min(NUM_ITEMS - self.scroll);
        let mut val_buf = StackFmt::<20>::new();

        let mut row_y = self.items_top;
        for vi in 0..visible_count {
            let item_idx = self.scroll + vi;
            let selected = item_idx == self.selected;
            let logical = VISUAL_TO_LOGICAL[item_idx];

            // section caption above the first item of each section.
            for (sec_at, caption) in SECTION_AT.iter().copied() {
                if sec_at == item_idx {
                    let cap_r = Region::new(LABEL_X, row_y, FULL_CONTENT_W, CAPTION_H);
                    BitmapLabel::new(cap_r, caption, self.ui_fonts.body)
                        .alignment(Alignment::CenterLeft)
                        .draw(strip)
                        .unwrap();
                    row_y += CAPTION_H + 2;
                }
            }

            let label_r = Region::new(LABEL_X, row_y, LABEL_W, ROW_H);
            let value_r = Region::new(VALUE_X, row_y, VALUE_W, ROW_H);
            BitmapLabel::new(label_r, Self::item_label(logical), self.ui_fonts.body)
                .alignment(Alignment::CenterLeft)
                .inverted(selected)
                .draw(strip)
                .unwrap();

            self.format_value(logical, &mut val_buf);
            BitmapLabel::new(value_r, val_buf.as_str(), self.ui_fonts.body)
                .alignment(Alignment::Center)
                .inverted(selected)
                .draw(strip)
                .unwrap();

            row_y += ROW_STRIDE;
        }
    }
}
