// top status bar: today's reading stats on the left, battery on the right.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Alignment, Painter, Region, StackFmt};

use crate::fonts::bitmap::BitmapFont;

const BAT_W: u16 = 56;

pub struct TopStatus {
    pub today_pages: u16,
    pub today_secs: u32,
    pub battery_pct: u8,
}

impl TopStatus {
    pub const fn new() -> Self {
        Self {
            today_pages: 0,
            today_secs: 0,
            battery_pct: 0,
        }
    }

    pub fn draw(&self, p: &mut Painter<'_>, font: &BitmapFont) {
        let theme = *p.theme();
        let parent = p.region();
        let bar = Region::new(parent.x, parent.y, parent.w, theme.top_bar_h);
        if !p.intersects(bar) {
            return;
        }

        // background clear (no border, the mockup has bare bar bleed)
        p.fill_in(bar, BinaryColor::Off);

        // left: "today · N pages · Hh Mm"
        let mut left = StackFmt::<48>::new();
        let (h, m) = split_hm(self.today_secs);
        let _ = if h > 0 {
            write!(left, "today \u{00B7} {} pages \u{00B7} {}h {}m", self.today_pages, h, m)
        } else {
            write!(left, "today \u{00B7} {} pages \u{00B7} {}m", self.today_pages, m)
        };
        let left_x = bar.x + theme.margin_lg;
        let left_w = bar.w.saturating_sub(2 * theme.margin_lg + BAT_W + theme.margin_md);
        let left_region = Region::new(left_x, bar.y, left_w, bar.h);
        font.draw_aligned(
            p.strip_mut(),
            left_region,
            left.as_str(),
            Alignment::CenterLeft,
            BinaryColor::On,
        );

        // right: "NN%"
        let mut right = StackFmt::<8>::new();
        let _ = write!(right, "{}%", self.battery_pct.min(100));
        let right_x = bar.x + bar.w - theme.margin_lg - BAT_W;
        let right_region = Region::new(right_x, bar.y, BAT_W, bar.h);
        font.draw_aligned(
            p.strip_mut(),
            right_region,
            right.as_str(),
            Alignment::CenterRight,
            BinaryColor::On,
        );
    }
}

impl Default for TopStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn split_hm(secs: u32) -> (u32, u32) {
    (secs / 3600, (secs % 3600) / 60)
}
