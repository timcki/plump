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

use crate::apps::{HDir, Tab};
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
        edge_hint: Option<HDir>,
    ) {
        self.top.today_pages = today_pages;
        self.top.today_secs = today_secs;
        self.top.battery_pct = battery_pct;
        self.tabs.active = active_tab;
        self.tabs.edge_hint = edge_hint;
    }

    /// `text_font` is used for top-status text and tab-bar edge hints;
    /// `icon_font` is the Phosphor bitmap font for tab icons.
    pub fn draw(&self, p: &mut Painter<'_>, text_font: &BitmapFont, icon_font: &BitmapFont) {
        self.top.draw(p, text_font);
        self.tabs.draw(p, text_font, icon_font);
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self::new()
    }
}
