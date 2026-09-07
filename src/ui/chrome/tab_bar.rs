// bottom tab bar: 5 evenly-spaced slots, active drawn as a filled
// square with knockout glyph.
//
// the bar carries no left/right edge hints: Left/Right only cycle tabs
// when the active screen declines them (`App::on_horizontal` returning
// `AtEdge`), so a permanent chevron would be lying on every screen that
// consumes them (library paging, a settings row being edited).
//
// icons are drawn from the Phosphor icon font (PUA codepoints in
// U+E0EA..U+E758). the `icon_font` parameter on `draw` is passed by
// the chrome owner; chrome size constants pick a single Phosphor
// rasterisation tier.

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Alignment, Painter, Region};

use crate::apps::Tab;
use crate::fonts::bitmap::BitmapFont;

const SLOTS: usize = 5;
const ACTIVE_PAD: u16 = 6;

pub struct TabBar {
    pub active: Tab,
}

impl TabBar {
    pub const fn new(active: Tab) -> Self {
        Self { active }
    }

    /// `icon_font`: Phosphor font for tab slot glyphs.
    pub fn draw(&self, p: &mut Painter<'_>, icon_font: &BitmapFont) {
        let theme = *p.theme();
        let parent = p.region();
        let bar = Region::new(
            parent.x,
            parent.y + parent.h - theme.bottom_bar_h,
            parent.w,
            theme.bottom_bar_h,
        );
        if !p.intersects(bar) {
            return;
        }

        // clear the bar behind the icons
        p.fill_in(bar, BinaryColor::Off);

        let slot_w = bar.w / SLOTS as u16;
        for (i, tab) in Tab::ORDER.iter().copied().enumerate() {
            let slot = Region::new(bar.x + i as u16 * slot_w, bar.y, slot_w, bar.h);
            self.draw_slot(p, icon_font, tab, slot);
        }
    }

    fn draw_slot(&self, p: &mut Painter<'_>, icon_font: &BitmapFont, tab: Tab, slot: Region) {
        let theme = *p.theme();
        let active = tab == self.active;
        let glyph = tab.icon();

        let mut buf = [0u8; 4];
        let s = glyph.encode_utf8(&mut buf);

        if active {
            // filled square centred in the slot, then knockout glyph.
            let size = (theme.bottom_bar_h.saturating_sub(2 * ACTIVE_PAD))
                .min(slot.w.saturating_sub(2 * ACTIVE_PAD));
            let sx = slot.x + (slot.w.saturating_sub(size)) / 2;
            let sy = slot.y + (slot.h.saturating_sub(size)) / 2;
            let square = Region::new(sx, sy, size, size);
            p.rounded_rect(square, 4, true);
            icon_font.draw_aligned(
                p.strip_mut(),
                square,
                s,
                Alignment::Center,
                BinaryColor::Off,
            );
        } else {
            icon_font.draw_aligned(
                p.strip_mut(),
                slot,
                s,
                Alignment::Center,
                BinaryColor::On,
            );
        }
    }

}
