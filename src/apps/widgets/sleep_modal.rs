// sleep modal (mockups/xteink_x4_sleep_modal.html): a centred card
// painted the moment a sleep the user asked for starts (power held,
// the menu's Sleep row), over whatever is on screen, until the sleep
// screen takes the panel. deliberately not the bottom sheet: the sheet
// is something you act in, this is the device telling you something,
// so it has no rows and no hints, and an offset shadow lifts it off
// the page where the sheet sits flush.
//
// it is drawn once, in the partial whose waveform hosts the sleep
// prep, and never again: the app drops its heap during that prep, so
// the widget is stateless and draws from what the caller hands it.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};

use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::StripBuffer;
use crate::fonts::{self, bitmap::BitmapFont};
use crate::ui::{Alignment, Region};

use super::row::draw_ellipsized;
use super::sheet::ICON_MOON;

const W: u16 = 336;
const PAD: u16 = 20;
const RADIUS: u32 = 12;
const BORDER: u32 = 2;
const SHADOW: u16 = 4;
const ICON_GAP: u16 = 10;
// the hairline between the title and the book: Home's rule
const RULE_W: u16 = 24;
const RULE_ABOVE: u16 = 14;
const RULE_BELOW: u16 = 12;
const PLACE_GAP: u16 = 4;

/// The book in front of the user when the sleep started: its title in
/// the book's own face, and where they stopped.
pub struct SleepBook<'a> {
    pub title: &'a str,
    pub title_font: &'static BitmapFont,
    pub place: &'a str,
}

pub fn draw(strip: &mut StripBuffer, ui_font_idx: u8, book: Option<&SleepBook<'_>>) {
    let icon = fonts::icon_font(ui_font_idx.saturating_add(1));
    let title = fonts::ui_heading_font(ui_font_idx);
    let small = fonts::chrome_font();

    let mut h = PAD + icon.line_height + ICON_GAP + title.line_height + PAD;
    if let Some(b) = book {
        h += RULE_ABOVE + 1 + RULE_BELOW + b.title_font.line_height + PLACE_GAP + small.line_height;
    }
    let card = Region::new((SCREEN_W - W) / 2, (SCREEN_H - h) / 2, W, h);
    let reach = Region::new(card.x, card.y, card.w + SHADOW, card.h + SHADOW);
    if !reach.intersects(strip.logical_window()) {
        return;
    }

    let radii = Size::new(RADIUS, RADIUS);
    let shadow = Rectangle::new(
        Point::new((card.x + SHADOW) as i32, (card.y + SHADOW) as i32),
        Size::new(card.w as u32, card.h as u32),
    );
    RoundedRectangle::with_equal_corners(shadow, radii)
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(strip)
        .ok();
    RoundedRectangle::with_equal_corners(card.to_rect(), radii)
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
        .draw(strip)
        .ok();
    RoundedRectangle::with_equal_corners(card.to_rect(), radii)
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, BORDER))
        .draw(strip)
        .ok();

    let inner_x = card.x + PAD;
    let inner_w = card.w - 2 * PAD;
    let mut y = card.y + PAD;

    let mut moon = [0u8; 4];
    centred(strip, icon, inner_x, y, inner_w, ICON_MOON.encode_utf8(&mut moon));
    y += icon.line_height + ICON_GAP;
    centred(strip, title, inner_x, y, inner_w, "Going to sleep");
    y += title.line_height;

    let Some(b) = book else {
        return;
    };
    y += RULE_ABOVE;
    Rectangle::new(
        Point::new((card.x + (card.w - RULE_W) / 2) as i32, y as i32),
        Size::new(RULE_W as u32, 1),
    )
    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
    .draw(strip)
    .ok();
    y += 1 + RULE_BELOW;
    centred(strip, b.title_font, inner_x, y, inner_w, b.title);
    y += b.title_font.line_height + PLACE_GAP;
    centred(strip, small, inner_x, y, inner_w, b.place);
}

// one line centred in the card; a line too long for it is cut with
// an ellipsis from the left edge instead
fn centred(strip: &mut StripBuffer, font: &BitmapFont, x: u16, y: u16, w: u16, text: &str) {
    let r = Region::new(x, y, w, font.line_height);
    if font.measure_str(text) > w {
        draw_ellipsized(strip, font, r, text, BinaryColor::On);
    } else {
        font.draw_aligned(strip, r, text, Alignment::Center, BinaryColor::On);
    }
}
