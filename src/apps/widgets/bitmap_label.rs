use core::convert::Infallible;

use embedded_graphics::{pixelcolor::BinaryColor, prelude::*, primitives::PrimitiveStyle};

use crate::drivers::strip::StripBuffer;
use crate::fonts::bitmap::BitmapFont;
use crate::ui::{Alignment, Region};

pub struct BitmapLabel<'a> {
    region: Region,
    text: &'a str,
    font: &'static BitmapFont,
    alignment: Alignment,
}

impl<'a> BitmapLabel<'a> {
    pub fn new(region: Region, text: &'a str, font: &'static BitmapFont) -> Self {
        Self {
            region,
            text,
            font,
            alignment: Alignment::CenterLeft,
        }
    }

    pub const fn alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    pub fn draw(&self, strip: &mut StripBuffer) -> Result<(), Infallible> {
        if !self.region.intersects(strip.logical_window()) {
            return Ok(());
        }

        self.region
            .to_rect()
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
            .draw(strip)?;

        self.font
            .draw_aligned(strip, self.region, self.text, self.alignment, BinaryColor::On);

        Ok(())
    }
}
