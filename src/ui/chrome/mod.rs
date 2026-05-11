// chrome: persistent top status bar + bottom tab bar plus reusable
// surface widgets (panel, section label, filter chip).
//
// chrome widgets live in the distro because they need bitmap fonts
// (UI text, eventual icon font). they compose `Painter` (clipping +
// raw drawing, kernel-side) with a `BitmapFont` reference. nothing in
// this module is currently rendered; chunk D wires the scheduler to
// call `Chrome::draw` before delegating to the active app.

pub mod filter_chip;
pub mod panel;
pub mod section_label;
pub mod tab_bar;
pub mod top_status;

pub use filter_chip::FilterChip;
pub use panel::{Panel, PanelRow, RowAccessory};
pub use section_label::SectionLabel;
pub use tab_bar::TabBar;
pub use top_status::TopStatus;

use plump_kernel::ui::Painter;

use crate::apps::Tab;
use crate::fonts::bitmap::BitmapFont;

/// Convenience wrapper that draws both the top status bar and the
/// bottom tab bar in one call. Reader opts out via `App::show_chrome`.
pub struct Chrome {
    pub top: TopStatus,
    pub tabs: TabBar,
}

impl Chrome {
    pub const fn new() -> Self {
        Self {
            top: TopStatus::new(),
            tabs: TabBar::new(Tab::Home),
        }
    }

    /// Update the live state from the kernel + nav before drawing.
    pub fn refresh(
        &mut self,
        today_pages: u16,
        today_secs: u32,
        battery_pct: u8,
        active_tab: Tab,
    ) {
        self.top.today_pages = today_pages;
        self.top.today_secs = today_secs;
        self.top.battery_pct = battery_pct;
        self.tabs.active = active_tab;
    }

    /// Paint just the top status bar (today's stats + battery).
    /// Reader uses this and skips `draw_tabs` because it paints its
    /// own progress footer instead.
    pub fn draw_top(&self, p: &mut Painter<'_>, text_font: &BitmapFont) {
        self.top.draw(p, text_font);
    }

    /// Paint just the bottom tab bar (5 Phosphor icons).
    pub fn draw_tabs(&self, p: &mut Painter<'_>, text_font: &BitmapFont, icon_font: &BitmapFont) {
        self.tabs.draw(p, text_font, icon_font);
    }

    /// Convenience: draw top and tabs in one call.
    pub fn draw(&self, p: &mut Painter<'_>, text_font: &BitmapFont, icon_font: &BitmapFont) {
        self.draw_top(p, text_font);
        self.draw_tabs(p, text_font, icon_font);
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self::new()
    }
}
