// bottom tab bar: 5 evenly-spaced slots, active drawn as a filled
// square with knockout glyph. edge-hint chevrons render at the
// outermost side of the active slot whenever the next Left / Right
// press would switch tabs (chunk E).
//
// icons are placeholder ASCII letters today (H L S G U). when a
// Phosphor (or hand-traced) icon font lands, swap `slot_glyph` to
// return private-use codepoints instead.

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Alignment, Painter, Region};

use crate::apps::{HDir, Tab};
use crate::fonts::bitmap::BitmapFont;

const SLOTS: usize = 5;
const ACTIVE_PAD: u16 = 6;

pub struct TabBar {
    pub active: Tab,
    pub edge_hint: Option<HDir>,
}

impl TabBar {
    pub const fn new(active: Tab) -> Self {
        Self {
            active,
            edge_hint: None,
        }
    }

    pub fn draw(&self, p: &mut Painter<'_>, font: &BitmapFont) {
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
            self.draw_slot(p, font, tab, slot);
        }

        // edge hint chevrons sit just outside the active slot.
        if let Some(dir) = self.edge_hint {
            let active_idx = self.active.index();
            let active_slot = Region::new(
                bar.x + active_idx as u16 * slot_w,
                bar.y,
                slot_w,
                bar.h,
            );
            self.draw_edge_hint(p, font, dir, active_slot);
        }
    }

    fn draw_slot(&self, p: &mut Painter<'_>, font: &BitmapFont, tab: Tab, slot: Region) {
        let theme = *p.theme();
        let active = tab == self.active;
        let glyph = slot_glyph(tab);

        if active {
            // filled square centred in the slot, then knockout glyph.
            let size = (theme.bottom_bar_h.saturating_sub(2 * ACTIVE_PAD)).min(slot.w.saturating_sub(2 * ACTIVE_PAD));
            let sx = slot.x + (slot.w.saturating_sub(size)) / 2;
            let sy = slot.y + (slot.h.saturating_sub(size)) / 2;
            let square = Region::new(sx, sy, size, size);
            p.rounded_rect(square, 4, true);
            // knockout glyph (background colour over the filled rect)
            let mut buf = [0u8; 4];
            let s = glyph.encode_utf8(&mut buf);
            font.draw_aligned(
                p.strip_mut(),
                square,
                s,
                Alignment::Center,
                BinaryColor::Off,
            );
        } else {
            let mut buf = [0u8; 4];
            let s = glyph.encode_utf8(&mut buf);
            font.draw_aligned(
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

/// ASCII placeholder glyph for each tab. Swap to private-use
/// codepoints once an icon font is wired up; `Tab::icon()` already
/// reserves U+E000..U+E004 for that purpose.
#[inline]
fn slot_glyph(tab: Tab) -> char {
    match tab {
        Tab::Home => 'H',
        Tab::Library => 'L',
        Tab::Stats => 'S',
        Tab::Settings => 'G',
        Tab::Upload => 'U',
    }
}
