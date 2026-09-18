//! Downscale fidelity: features narrower than the scale factor must
//! survive the reduction.
//!
//! The decoder used to point-sample one source column in `scale` when
//! downscaling, so a feature `n` px wide survived with probability
//! `n / scale` — at Project Hail Mary's 17:1 chapter banners that meant
//! 3 px strokes appeared roughly one time in six, and Floyd-Steinberg
//! could not diffuse error from samples it never read. Folding every
//! source pixel into the output column it lands in (a box filter) turns
//! those into grey the dither can render.
//!
//! The fixture is a 3744x48 greyscale JPEG of vertical bars at known
//! widths, six bars per width, downscaled 9:1 to a 416 px column.

use smol_epub::jpeg::{SliceReader, decode_jpeg};

const BARS: &[u8] = include_bytes!("fixtures/synthetic_bars_3744x48.jpg");
const SRC_W: usize = 3744;
const WIDTHS: [usize; 7] = [1, 2, 3, 4, 6, 9, 14];
const PER_WIDTH: usize = 6;

/// Rebuild the generator's bar layout: bars are laid left to right,
/// `PER_WIDTH` of each width, with a gap wide enough to stay resolvable.
fn bars() -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut x = 24;
    for wd in WIDTHS {
        for _ in 0..PER_WIDTH {
            out.push((wd, x));
            x += wd + 27;
        }
        x += 40;
    }
    out
}

#[test]
fn features_narrower_than_the_scale_factor_survive() {
    let img = decode_jpeg(SliceReader::new(BARS), 416, 700).expect("decode");
    assert_eq!((img.width, img.height), (416, 5));
    let (w, stride) = (img.width as usize, img.stride);
    let ink = |y: usize, x: usize| (img.data[y * stride + x / 8] >> (7 - (x & 7))) & 1 == 1;

    for (wd, sx) in bars() {
        // the output columns this bar maps into, plus a column either
        // side for dither spill
        let lo = (sx * w / SRC_W).saturating_sub(1);
        let hi = (((sx + wd - 1) * w / SRC_W) + 2).min(w);
        let kept = (0..img.height as usize).any(|y| (lo..hi).any(|x| ink(y, x)));

        // 1 px in 9 is 1.2% darkening over the box — genuinely below
        // what one bit can carry, and not what this test is about
        if wd >= 3 {
            assert!(
                kept,
                "{wd} px bar at x={sx} vanished; point sampling would keep \
                 about {}/{PER_WIDTH} of these",
                wd * PER_WIDTH / 9
            );
        }
    }
}

#[test]
fn downscale_preserves_overall_density() {
    // error diffusion should still land the page's ink within a point
    // or so of the source's mean darkness
    let img = decode_jpeg(SliceReader::new(BARS), 416, 700).expect("decode");
    let (w, h, stride) = (img.width as usize, img.height as usize, img.stride);
    let black: usize = (0..h)
        .map(|y| {
            (0..w)
                .filter(|x| (img.data[y * stride + x / 8] >> (7 - (x & 7))) & 1 == 1)
                .count()
        })
        .sum();
    let ink = black as f32 / (w * h) as f32;

    // the generator covers PER_WIDTH bars of each width
    let covered: usize = PER_WIDTH * WIDTHS.iter().sum::<usize>();
    let expected = covered as f32 / SRC_W as f32;
    assert!(
        (ink - expected).abs() < 0.01,
        "ink {ink:.3} should track the source's {expected:.3}"
    );
}
