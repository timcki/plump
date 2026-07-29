// qr symbol widget: encode once, blit scaled modules per strip.
//
// font-independent (it only needs Region, Painter and BinaryColor), so
// it lives kernel-side by the same rule as the progress bar.
//
// encoding and drawing are deliberately split. the draw closure runs
// once per strip -- twelve times for a full-screen refresh -- and the
// encoder's automatic mask selection scores eight candidate masks, so
// encoding inside the closure would pay for the symbol twelve times
// over. `QrSymbol::encode` is called once by the caller and the owned
// module bitmap is what gets blitted.

use embedded_graphics::pixelcolor::BinaryColor;
use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};

use crate::ui::{Painter, Region};

/// Largest symbol this widget emits.
///
/// Version 4 is 33x33 modules and holds 62 bytes at ECC low, which
/// covers both payloads upload mode needs (a `http://<ip>/` URL and a
/// `WIFI:` join string) with room to spare. Capping it bounds the two
/// scratch buffers at 138 bytes each; version 40 would need 3 KB.
const MAX_VERSION: Version = Version::new(4);

/// Scratch length the encoder requires for `MAX_VERSION`.
const SCRATCH_LEN: usize = MAX_VERSION.buffer_len();

/// Side of the largest symbol, in modules.
const MAX_SIDE: usize = MAX_VERSION.value() as usize * 4 + 17;

/// Owned module bitmap, one bit per module, row-major.
const MODULE_BYTES: usize = (MAX_SIDE * MAX_SIDE).div_ceil(8);

/// Quiet zone the standard requires, in modules per side.
const QUIET: u16 = 4;

/// An encoded QR symbol, owning its modules.
///
/// Copied out of the encoder's borrowed buffers so the symbol can
/// outlive them: `QrCode` borrows the scratch it was built in, which
/// would make this struct self-referential.
pub struct QrSymbol {
    modules: [u8; MODULE_BYTES],
    /// Side length in modules; 0 is not representable for a real
    /// symbol, so `encode` returning `None` is the only empty case.
    side: u8,
}

impl QrSymbol {
    /// Encode `text`, or `None` if it does not fit in [`MAX_VERSION`].
    ///
    /// ECC low: an e-paper panel is a high-contrast, undamaged,
    /// perfectly flat target, so the error budget is better spent on
    /// fewer, larger modules.
    pub fn encode(text: &str) -> Option<Self> {
        let mut scratch_a = [0u8; SCRATCH_LEN];
        let mut scratch_b = [0u8; SCRATCH_LEN];

        let code = QrCode::encode_text(
            text,
            &mut scratch_a,
            &mut scratch_b,
            QrCodeEcc::Low,
            Version::MIN,
            MAX_VERSION,
            None,
            true,
        )
        .ok()?;

        let side = code.size();
        if side <= 0 || side as usize > MAX_SIDE {
            return None;
        }

        let mut modules = [0u8; MODULE_BYTES];
        for y in 0..side {
            for x in 0..side {
                if code.get_module(x, y) {
                    let bit = y as usize * side as usize + x as usize;
                    modules[bit / 8] |= 1 << (bit % 8);
                }
            }
        }

        Some(Self {
            modules,
            side: side as u8,
        })
    }

    /// Side length in modules, excluding the quiet zone.
    #[inline]
    pub const fn side(&self) -> u8 {
        self.side
    }

    /// Largest whole-pixel module size that fits this symbol, quiet
    /// zone included, inside `region`.
    ///
    /// Whole pixels only: a fractional scale would round module edges
    /// inconsistently across the symbol, which is exactly what a
    /// decoder's grid sampling cannot tolerate.
    fn scale_for(&self, region: Region) -> u16 {
        let modules = self.side as u16 + QUIET * 2;
        region.w.min(region.h) / modules
    }

    /// Pixel side of the drawn symbol, quiet zone included, or 0 if
    /// `region` cannot hold even one pixel per module.
    pub fn drawn_size(&self, region: Region) -> u16 {
        (self.side as u16 + QUIET * 2) * self.scale_for(region)
    }

    /// Draw centred in `region`: white quiet zone, black modules.
    ///
    /// Nothing is drawn if the region is too small for a whole-pixel
    /// scale; a half-size symbol would not scan anyway.
    pub fn draw(&self, p: &mut Painter<'_>, region: Region) {
        let scale = self.scale_for(region);
        if scale == 0 {
            return;
        }

        let size = self.drawn_size(region);
        let x0 = region.x + (region.w - size) / 2;
        let y0 = region.y + (region.h - size) / 2;
        let frame = Region::new(x0, y0, size, size);
        if !p.intersects(frame) {
            return;
        }

        // the quiet zone is part of the symbol: without it a decoder
        // cannot find the finder patterns against page content
        p.fill_in(frame, BinaryColor::Off);

        let origin_x = x0 + QUIET * scale;
        let origin_y = y0 + QUIET * scale;
        for y in 0..self.side as u16 {
            for x in 0..self.side as u16 {
                let bit = y as usize * self.side as usize + x as usize;
                if self.modules[bit / 8] & (1 << (bit % 8)) == 0 {
                    continue;
                }
                let module = Region::new(origin_x + x * scale, origin_y + y * scale, scale, scale);
                p.fill_in(module, BinaryColor::On);
            }
        }
    }
}
