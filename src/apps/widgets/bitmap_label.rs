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
    inverted: bool,
}

impl<'a> BitmapLabel<'a> {
    pub fn new(region: Region, text: &'a str, font: &'static BitmapFont) -> Self {
        Self {
            region,
            text,
            font,
            alignment: Alignment::CenterLeft,
            inverted: false,
        }
    }

    pub const fn alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    pub const fn inverted(mut self, inverted: bool) -> Self {
        self.inverted = inverted;
        self
    }

    pub fn draw(&self, strip: &mut StripBuffer) -> Result<(), Infallible> {
        draw_bitmap_text(
            strip,
            self.region,
            self.text,
            self.font,
            self.alignment,
            self.inverted,
        )
    }
}

pub struct BitmapDynLabel<const N: usize> {
    region: Region,
    buffer: plump_kernel::util::FixedStr<N>,
    font: &'static BitmapFont,
    alignment: Alignment,
    inverted: bool,
}

impl<const N: usize> BitmapDynLabel<N> {
    pub fn new(region: Region, font: &'static BitmapFont) -> Self {
        Self {
            region,
            buffer: plump_kernel::util::FixedStr::EMPTY,
            font,
            alignment: Alignment::CenterLeft,
            inverted: false,
        }
    }

    pub const fn alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    pub const fn inverted(mut self, inverted: bool) -> Self {
        self.inverted = inverted;
        self
    }

    pub fn set_text(&mut self, text: &str) {
        self.buffer.set(text.as_bytes());
    }

    pub fn clear_text(&mut self) {
        self.buffer.clear();
    }

    pub fn text(&self) -> &str {
        self.buffer.as_str()
    }

    pub fn draw(&self, strip: &mut StripBuffer) -> Result<(), Infallible> {
        draw_bitmap_text(
            strip,
            self.region,
            self.text(),
            self.font,
            self.alignment,
            self.inverted,
        )
    }
}

impl<const N: usize> core::fmt::Write for BitmapDynLabel<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.buffer.append(s.as_bytes());
        Ok(())
    }
}

fn draw_bitmap_text(
    strip: &mut StripBuffer,
    region: Region,
    text: &str,
    font: &'static BitmapFont,
    alignment: Alignment,
    inverted: bool,
) -> Result<(), Infallible> {
    if !region.intersects(strip.logical_window()) {
        return Ok(());
    }

    let (bg, fg) = if inverted {
        (BinaryColor::On, BinaryColor::Off)
    } else {
        (BinaryColor::Off, BinaryColor::On)
    };

    region
        .to_rect()
        .into_styled(PrimitiveStyle::with_fill(bg))
        .draw(strip)?;

    font.draw_aligned(strip, region, text, alignment, fg);

    Ok(())
}
