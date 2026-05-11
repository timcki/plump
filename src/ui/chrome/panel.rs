// grouped rounded panel with internal hairline separators.
//
// the mockups use this for: settings sections (Reading / Display /
// System), the home continue-reading card, the quick-settings overlay
// row groups. each row has a label on the left and an accessory on
// the right (value text, arrow glyph, or nothing).
//
// optional selected-row index inverts that row (background on,
// foreground off). drawing the row inversion delegates to the
// existing `SelectableRow` widget so the inversion logic stays in
// one place.

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Alignment, Painter, Region};

use crate::apps::widgets::selectable_row::SelectableRow;
use crate::fonts::bitmap::BitmapFont;

/// what to render on the right side of a panel row.
#[derive(Clone, Copy)]
pub enum RowAccessory<'a> {
    /// no accessory (full-width label).
    None,
    /// muted value text (settings rows like "Font  Bookerly").
    Value(&'a str),
    /// terminal arrow (rows that open a sub-view).
    Arrow,
}

pub struct PanelRow<'a> {
    pub label: &'a str,
    pub accessory: RowAccessory<'a>,
    /// optional leading icon glyph (Phosphor codepoint or ASCII).
    pub icon: Option<char>,
}

impl<'a> PanelRow<'a> {
    pub const fn new(label: &'a str) -> Self {
        Self {
            label,
            accessory: RowAccessory::None,
            icon: None,
        }
    }

    pub const fn with_value(mut self, value: &'a str) -> Self {
        self.accessory = RowAccessory::Value(value);
        self
    }

    pub const fn with_arrow(mut self) -> Self {
        self.accessory = RowAccessory::Arrow;
        self
    }

    pub const fn with_icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }
}

pub struct Panel {
    pub region: Region,
    /// 0-based selected row, or None when the panel is non-interactive.
    pub selected: Option<usize>,
}

impl Panel {
    pub const fn new(region: Region) -> Self {
        Self {
            region,
            selected: None,
        }
    }

    pub fn with_selected(mut self, selected: usize) -> Self {
        self.selected = Some(selected);
        self
    }

    /// Draw the panel container and its rows. `rows` is borrowed for
    /// the duration of the call; nothing is kept.
    pub fn draw(&self, p: &mut Painter<'_>, font: &BitmapFont, rows: &[PanelRow<'_>]) {
        if !p.intersects(self.region) {
            return;
        }
        let theme = *p.theme();

        // outline
        p.rounded_rect(self.region, theme.panel_radius, false);

        if rows.is_empty() {
            return;
        }

        // distribute rows evenly; each row is theme.row_h tall, padded
        // slightly so the rounded corners aren't bisected by separators.
        let inner_x = self.region.x + theme.panel_stroke;
        let inner_w = self.region.w.saturating_sub(2 * theme.panel_stroke);
        let row_h = theme.row_h;

        for (i, row) in rows.iter().enumerate() {
            let ry = self.region.y + theme.panel_stroke + i as u16 * row_h;
            if ry + row_h > self.region.y + self.region.h {
                break;
            }
            let row_region = Region::new(inner_x, ry, inner_w, row_h);

            // separator hairline between consecutive rows.
            if i > 0 {
                p.hairline_h(
                    ry,
                    row_region.x + theme.margin_md,
                    row_region.x + row_region.w - theme.margin_md,
                );
            }

            let selected = self.selected == Some(i);
            let fg = if selected {
                // SelectableRow returns the foreground colour to use
                // for text after painting the inversion rect.
                SelectableRow::new(row_region, true).draw(p.strip_mut())
            } else {
                BinaryColor::On
            };

            draw_row_label(p, font, row_region, row.label, row.icon, fg);
            draw_row_accessory(p, font, row_region, row.accessory, fg);
        }
    }
}

fn draw_row_label(
    p: &mut Painter<'_>,
    font: &BitmapFont,
    row: Region,
    label: &str,
    icon: Option<char>,
    fg: BinaryColor,
) {
    let theme = *p.theme();
    let mut x = row.x + theme.margin_md;
    if let Some(ch) = icon {
        // draw icon glyph, then push the label past it
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        let icon_w: u16 = font.measure_str(s);
        let icon_region = Region::new(x, row.y, icon_w + 4, row.h);
        font.draw_aligned(p.strip_mut(), icon_region, s, Alignment::CenterLeft, fg);
        x += icon_w + theme.margin_sm;
    }
    let label_w = row.x + row.w - x - theme.margin_md;
    let label_region = Region::new(x, row.y, label_w, row.h);
    font.draw_aligned(p.strip_mut(), label_region, label, Alignment::CenterLeft, fg);
}

fn draw_row_accessory(
    p: &mut Painter<'_>,
    font: &BitmapFont,
    row: Region,
    accessory: RowAccessory<'_>,
    fg: BinaryColor,
) {
    let theme = *p.theme();
    let acc_w: u16 = 80;
    let acc_region = Region::new(
        row.x + row.w - acc_w - theme.margin_md,
        row.y,
        acc_w,
        row.h,
    );
    match accessory {
        RowAccessory::None => {}
        RowAccessory::Value(v) => {
            font.draw_aligned(p.strip_mut(), acc_region, v, Alignment::CenterRight, fg);
        }
        RowAccessory::Arrow => {
            font.draw_aligned(p.strip_mut(), acc_region, "\u{2192}", Alignment::CenterRight, fg);
        }
    }
}
