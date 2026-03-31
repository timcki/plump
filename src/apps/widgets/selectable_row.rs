// selectable row widget: inverted selection highlight for list items
//
// consolidates the repeated pattern of drawing a selected/unselected row
// with inverted colors. used by home, files, settings, reader TOC, and
// quick menu for consistent selection rendering.
// the widget draws the selection background and returns the foreground
// color to use for text, letting the caller handle the actual content.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::PrimitiveStyle;

use crate::drivers::strip::StripBuffer;
use crate::ui::Region;

/// A row that renders with inverted colors when selected.
///
/// Construct with region + selected state, then call `draw()` to fill
/// the background and get the foreground color for text.
pub struct SelectableRow {
    pub region: Region,
    pub selected: bool,
}

impl SelectableRow {
    #[inline]
    pub const fn new(region: Region, selected: bool) -> Self {
        Self { region, selected }
    }

    /// Returns the foreground color without drawing anything.
    #[inline]
    pub const fn fg(&self) -> BinaryColor {
        if self.selected {
            BinaryColor::Off
        } else {
            BinaryColor::On
        }
    }

    /// Fill the background if selected, return foreground color for text.
    #[inline]
    pub fn draw(&self, strip: &mut StripBuffer) -> BinaryColor {
        if self.selected {
            self.region
                .to_rect()
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(strip)
                .unwrap();
            BinaryColor::Off
        } else {
            BinaryColor::On
        }
    }

    /// Same as `draw()` but skips the fill if the region doesn't
    /// intersect the current strip window (avoids wasted draw calls).
    #[inline]
    pub fn draw_if_visible(&self, strip: &mut StripBuffer) -> BinaryColor {
        if self.selected && self.region.intersects(strip.logical_window()) {
            self.region
                .to_rect()
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(strip)
                .unwrap();
            BinaryColor::Off
        } else if self.selected {
            BinaryColor::Off
        } else {
            BinaryColor::On
        }
    }
}
