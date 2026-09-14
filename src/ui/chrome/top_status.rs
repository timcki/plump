// top bar: the navigation.
//
//     <- settings  home  library ->                    [###  ] 87%
//
// the five screens are a line with Home in its middle
// (`apps::tab`), and this bar is that line seen from where you are
// standing: a cluster in the left corner holding the screen each
// button gets you either side of the one you are on, and the battery
// in the far corner. at either end of the line one arm is simply
// absent, so running out reads as running out.
//
// the battery is the glyph the sleep card draws plus its percentage,
// not the percentage alone: the charge is read from the lock screen
// more often than from here, and two shapes for one measurement is
// one shape too many.
//
// all lowercase, and the current screen is bold rather than large:
// the whole cluster is one line of body text, which is as much bar as
// two bits of information is worth.
//
// it replaced a bottom bar of five icon slots, which was the touch
// idiom on a device with no touch: five equal tap targets for a
// control that cannot be pressed. the arms say the same thing in
// words, on a strip that was already being drawn.
//
// no per-screen figures live here. every one of them belongs to the
// screen below it -- today's reading to Home's own caption, the book
// count to the library's, the version to the About group -- and this
// bar is navigation, not status.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Alignment, BatteryIcon, Painter, Region, StackFmt};

use crate::apps::Tab;
use crate::fonts::bitmap::BitmapFont;

/// Gap between the three names in the cluster.
const GAP: u16 = 12;

/// The arms point with arrows, not the solid triangles they used to.
/// An arm is one lowercase word in the body face; a filled triangle
/// beside it carried more ink than the destination it pointed at, so
/// the bar read as two glyphs with some text between them. An arrow
/// is drawn in the same stroke weight as the word, which is what
/// makes the cluster read as one line.
const ARROW_LEFT: char = '\u{2190}';
const ARROW_RIGHT: char = '\u{2192}';

/// The two faces the bar draws with.
#[derive(Clone, Copy)]
pub struct TopFonts {
    /// the screen you are on: bold, same size as the rest
    pub name: &'static BitmapFont,
    /// the two arms and the battery
    pub small: &'static BitmapFont,
}

pub struct TopStatus {
    pub active: Tab,
    pub battery_pct: u8,
}

impl TopStatus {
    pub const fn new() -> Self {
        Self {
            active: Tab::Home,
            battery_pct: 0,
        }
    }

    pub fn draw(&self, p: &mut Painter<'_>, fonts: &TopFonts) {
        let theme = *p.theme();
        let parent = p.region();
        let bar = Region::new(parent.x, parent.y, parent.w, theme.top_bar_h);
        if !p.intersects(bar) {
            return;
        }

        p.fill_in(bar, BinaryColor::Off);

        // the cluster, left to right in the order the line runs: what
        // Left gets you, where you are, what Right gets you
        let mut x = bar.x + theme.margin_lg;
        if let Some(prev) = self.active.left() {
            let mut text = StackFmt::<24>::new();
            let _ = write!(text, "{} ", ARROW_LEFT);
            push_lower(&mut text, prev.label());
            x = self.draw_piece(p, fonts.small, bar, x, text.as_str()) + GAP;
        }
        {
            let mut text = StackFmt::<24>::new();
            push_lower(&mut text, self.active.label());
            x = self.draw_piece(p, fonts.name, bar, x, text.as_str()) + GAP;
        }
        if let Some(next) = self.active.right() {
            let mut text = StackFmt::<24>::new();
            push_lower(&mut text, next.label());
            let _ = write!(text, " {}", ARROW_RIGHT);
            x = self.draw_piece(p, fonts.small, bar, x, text.as_str());
        }
        let _ = x;

        // the same glyph the sleep card draws, so the charge reads as
        // one measurement across the two screens that name it. both
        // hang off the right margin and the glyph off the text's own
        // width, so the gap between them does not open up at 9%
        let pct = self.battery_pct.min(100);
        let mut bat = StackFmt::<8>::new();
        let _ = write!(bat, "{}%", pct);
        let bat_w = fonts.small.measure_str(bat.as_str());
        let text_x = (bar.x + bar.w).saturating_sub(theme.margin_lg + bat_w);
        let bat_r = Region::new(text_x, bar.y, bat_w, bar.h);
        // the glyph sits on the percentage's own midline; centring it
        // in the bar would put it above the text, which sits high in a
        // line box by the height of its descent and leading
        let icon_y = fonts
            .small
            .midline_in(bat_r)
            .saturating_sub(BatteryIcon::H / 2);
        fonts.small.draw_aligned(
            p.strip_mut(),
            bat_r,
            bat.as_str(),
            Alignment::CenterRight,
            BinaryColor::On,
        );
        BatteryIcon::new(
            text_x.saturating_sub(BatteryIcon::GAP + BatteryIcon::TOTAL_W),
            icon_y,
            pct,
        )
        .draw(p.strip_mut());

        // the edge neither bar has ever had
        p.hairline_h(bar.y + bar.h - 1, bar.x, bar.x + bar.w);
    }

    /// Draw one piece of the cluster at `x`, returning the x it ended
    /// at so the next piece can follow it.
    fn draw_piece(
        &self,
        p: &mut Painter<'_>,
        font: &BitmapFont,
        bar: Region,
        x: u16,
        text: &str,
    ) -> u16 {
        let w = font.measure_str(text);
        let r = Region::new(x, bar.y, w, bar.h);
        font.draw_aligned(p.strip_mut(), r, text, Alignment::CenterLeft, BinaryColor::On);
        x + w
    }
}

/// Append a label in lowercase. Tab labels are title case for menus
/// and logs; the bar is quieter than that.
fn push_lower(out: &mut StackFmt<24>, label: &str) {
    for ch in label.chars() {
        let _ = out.write_char(ch.to_ascii_lowercase());
    }
}

impl Default for TopStatus {
    fn default() -> Self {
        Self::new()
    }
}
