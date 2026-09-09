// tracked uppercase section caption ("CONTINUE READING", "RECENT").
//
// the mockup spaces letters by a few extra px (tracked typography).
// since BitmapFont measures glyph advances in bytes we render the
// label character-by-character with an added tracking offset between
// glyphs. for short strings (caption labels are <=20 chars) the
// per-char draw cost is negligible.

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Painter, Region};

use crate::fonts::bitmap::BitmapFont;

pub struct SectionLabel<'a> {
    pub region: Region,
    pub text: &'a str,
    // anchor the caption to the right edge of the region instead of
    // the left (library uses this for the scroll position)
    right_aligned: bool,
}

impl<'a> SectionLabel<'a> {
    pub const fn new(region: Region, text: &'a str) -> Self {
        Self {
            region,
            text,
            right_aligned: false,
        }
    }

    pub const fn right_aligned(mut self) -> Self {
        self.right_aligned = true;
        self
    }

    /// Tracked width of the uppercased caption in px.
    fn measure(&self, font: &BitmapFont, tracking: u16) -> u16 {
        let mut w = 0u16;
        let mut utf8 = [0u8; 4];
        for (i, ch) in self.text.chars().enumerate() {
            let upper = ch.to_ascii_uppercase();
            w += font.measure_str(upper.encode_utf8(&mut utf8));
            if i > 0 {
                w += tracking;
            }
        }
        w
    }

    pub fn draw(&self, p: &mut Painter<'_>, font: &BitmapFont) {
        if !p.intersects(self.region) || self.text.is_empty() {
            return;
        }
        let theme = *p.theme();
        let tracking = theme.tracked_caption_px.max(0) as u16;
        let baseline = self.region.y as i32 + font.ascent as i32;
        let right = (self.region.x + self.region.w) as i32;
        let mut x = if self.right_aligned {
            right - self.measure(font, tracking).min(self.region.w) as i32
        } else {
            self.region.x as i32
        };
        for ch in self.text.chars() {
            // uppercase ASCII; non-ASCII passes through unchanged.
            let upper = ch.to_ascii_uppercase();
            let advance = font.draw_char_fg(p.strip_mut(), upper, BinaryColor::On, x, baseline);
            x += advance as i32 + tracking as i32;
            if x >= right {
                break;
            }
        }
    }
}
