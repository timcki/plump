// loading sheet: the bottom sheet a book loads under (mockups/
// xteink_x4_reader_loading_sheet.html). the menu's and the contents'
// sheet with a single row: the header names what is loading, the row
// says which stage it is in and how far along, and the hint says Back
// cancels. it is drawn over whatever the manager put beneath it, the
// screen the book was opened from or plain paper, so it never owns
// the rest of the screen.

use crate::drivers::strip::StripBuffer;
use crate::fonts::bitmap::BitmapFont;

use super::row::RowLead;
use super::sheet::{self, HintSlot, RowSpec, SheetFonts, SheetGeom};

pub struct LoadingSheet<'a> {
    pub title: &'a str,
    /// a book's title is set in the book's face, a device label in
    /// the UI's
    pub title_font: &'static BitmapFont,
    pub caption: &'a str,
    pub meta: &'a str,
    pub row: RowSpec<'a>,
    /// what Back does from here
    pub back: &'a str,
}

impl LoadingSheet<'_> {
    fn geom(&self, fonts: &SheetFonts) -> SheetGeom {
        let text_h = fonts.row_h(
            self.row.text_font,
            !self.row.sub.is_empty(),
            self.row.progress.is_some(),
        );
        let lead_h = match self.row.lead {
            RowLead::Cover(_, size) => size.row_h(),
            _ => 0,
        };
        SheetGeom::anchored(1, None, text_h.max(lead_h))
    }

    pub fn draw(&self, strip: &mut StripBuffer, ui_font_idx: u8) {
        let fonts = SheetFonts {
            title: self.title_font,
            ..SheetFonts::for_ui(ui_font_idx)
        };
        let geom = self.geom(&fonts);
        if !geom.region.intersects(strip.logical_window()) {
            return;
        }
        sheet::draw_frame(strip, &geom);
        sheet::draw_header(strip, &geom, &fonts, self.title, self.caption, self.meta);
        sheet::draw_groups(strip, &geom);
        sheet::draw_row(strip, &geom, 0, &fonts, &self.row);
        sheet::draw_hints(strip, &geom, &fonts, &[(HintSlot::Back, self.back)]);
    }
}
