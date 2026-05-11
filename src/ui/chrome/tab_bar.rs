// bottom tab bar: 5 evenly-spaced slots, active drawn as a filled
// square with knockout glyph. chevrons (< / >) render next to the
// active slot on every side where a neighbour tab exists, as a
// persistent visual affordance for the physical Left / Right buttons.
//
// icons are drawn from the Phosphor icon font (PUA codepoints in
// U+E0EA..U+E758). the `icon_font` parameter on `draw` is passed by
// the chrome owner; chrome size constants pick a single Phosphor
// rasterisation tier.

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Alignment, Painter, Region};

use crate::apps::{HDir, Tab};
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

    /// `text_font`: UI font for chevrons (ASCII `<` `>` today).
    /// `icon_font`: Phosphor font for tab slot glyphs.
    pub fn draw(&self, p: &mut Painter<'_>, text_font: &BitmapFont, icon_font: &BitmapFont) {
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

        // clear + hairline along the top edge
        p.fill_in(bar, BinaryColor::Off);
        p.hairline_h(bar.y, bar.x + theme.margin_lg, bar.x + bar.w - theme.margin_lg);

        let slot_w = bar.w / SLOTS as u16;
        for (i, tab) in Tab::ORDER.iter().copied().enumerate() {
            let slot = Region::new(bar.x + i as u16 * slot_w, bar.y, slot_w, bar.h);
            self.draw_slot(p, icon_font, tab, slot);
        }

        // chevrons next to the active slot on each side where a
        // neighbour tab exists. drawn unconditionally because every
        // tab screen takes Left / Right as tab cycle; Library (chunk J)
        // overrides this by drawing its own internal edge cue inside
        // its content area.
        let active_idx = self.active.index();
        let active_slot = Region::new(
            bar.x + active_idx as u16 * slot_w,
            bar.y,
            slot_w,
            bar.h,
        );
        if self.active.left().is_some() {
            self.draw_edge_hint(p, text_font, HDir::Left, active_slot);
        }
        if self.active.right().is_some() {
            self.draw_edge_hint(p, text_font, HDir::Right, active_slot);
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

    fn draw_edge_hint(
        &self,
        p: &mut Painter<'_>,
        font: &BitmapFont,
        dir: HDir,
        active_slot: Region,
    ) {
        let arrow = match dir {
            HDir::Left => '<',
            HDir::Right => '>',
        };
        let mut buf = [0u8; 4];
        let s = arrow.encode_utf8(&mut buf);
        // place the arrow just outside the active slot on the
        // appropriate side, vertically centred.
        let region = match dir {
            HDir::Left => Region::new(
                active_slot.x.saturating_sub(active_slot.w / 4),
                active_slot.y,
                active_slot.w / 4,
                active_slot.h,
            ),
            HDir::Right => Region::new(
                active_slot.x + active_slot.w,
                active_slot.y,
                active_slot.w / 4,
                active_slot.h,
            ),
        };
        font.draw_aligned(
            p.strip_mut(),
            region,
            s,
            Alignment::Center,
            BinaryColor::On,
        );
    }
}

