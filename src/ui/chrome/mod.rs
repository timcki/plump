// chrome: the persistent top bar, which is the navigation, plus the
// section label.
//
// there is no bottom bar any more: five icon slots that could not be
// pressed became two named arms either side of a nameplate, in the
// strip that was already being drawn. see `top_status`.
//
// chrome widgets live in the distro because they need bitmap fonts
// (UI text, eventual icon font). they compose `Painter` (clipping +
// raw drawing, kernel-side) with a `BitmapFont` reference.

pub mod section_label;
pub mod top_status;

pub use section_label::SectionLabel;
pub use top_status::{TopFonts, TopStatus};

use plump_kernel::ui::Painter;

/// The persistent chrome. One bar now, at the top; the reader opts
/// out of it entirely and paints its own footer instead.
pub struct Chrome {
    pub top: TopStatus,
}

impl Chrome {
    pub const fn new() -> Self {
        Self {
            top: TopStatus::new(),
        }
    }

    pub fn draw_top(&self, p: &mut Painter<'_>, fonts: &TopFonts) {
        self.top.draw(p, fonts);
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self::new()
    }
}
