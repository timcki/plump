// rounded pill: "{label} · {count}".  used by the Library tab (chunk J)
// for All / Reading / Done.  active variant is inverted.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;

use plump_kernel::ui::{Alignment, Painter, Region, StackFmt};

use crate::fonts::bitmap::BitmapFont;

pub struct FilterChip<'a> {
    pub region: Region,
    pub label: &'a str,
    pub count: u16,
    pub active: bool,
}

impl<'a> FilterChip<'a> {
    pub const fn new(region: Region, label: &'a str, count: u16, active: bool) -> Self {
        Self {
            region,
            label,
            count,
            active,
        }
    }

    pub fn draw(&self, p: &mut Painter<'_>, font: &BitmapFont) {
        if !p.intersects(self.region) {
            return;
        }
        let radius = self.region.h / 2;

        if self.active {
            // filled pill, knockout text
            p.rounded_rect(self.region, radius, true);
            let mut fmt = StackFmt::<24>::new();
            let _ = write!(fmt, "{} \u{00B7} {}", self.label, self.count);
            font.draw_aligned(
                p.strip_mut(),
                self.region,
                fmt.as_str(),
                Alignment::Center,
                BinaryColor::Off,
            );
        } else {
            // stroked pill, on-state text
            p.rounded_rect(self.region, radius, false);
            let mut fmt = StackFmt::<24>::new();
            let _ = write!(fmt, "{} \u{00B7} {}", self.label, self.count);
            font.draw_aligned(
                p.strip_mut(),
                self.region,
                fmt.as_str(),
                Alignment::Center,
                BinaryColor::On,
            );
        }
    }
}
