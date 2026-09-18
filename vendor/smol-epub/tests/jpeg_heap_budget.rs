//! Heap-budget regression test for the progressive JPEG decoder.
//!
//! On device the reader holds tens of KB (chapter buffer, ZIP index)
//! while the worker task decodes, so a decode has far less than the
//! full heap to work with. `decode_progressive` allocates one buffer
//! that can shrink (the coefficient band, sized from MAX_BAND_BYTES)
//! and three that cannot (y_row, output, dither rows). Taking the
//! shrinkable one first let it swallow the whole budget and starve
//! the fixed-size output — "jpeg: OOM for output" on a 418x269 image
//! with ~50 KB free. The fixed buffers must be reserved first so the
//! band adapts to what is left.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;

use smol_epub::jpeg::{SliceReader, decode_jpeg};

thread_local! {
    // 0 = unarmed: allocations pass through unaccounted
    static CAP: Cell<usize> = const { Cell::new(0) };
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

struct CapAlloc;

unsafe impl GlobalAlloc for CapAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let refused = CAP.with(|cap| {
            let cap = cap.get();
            if cap == 0 {
                return false;
            }
            LIVE.with(|live| {
                let next = live.get() + layout.size();
                if next > cap {
                    return true;
                }
                live.set(next);
                PEAK.with(|peak| peak.set(peak.get().max(next)));
                false
            })
        });
        if refused {
            return core::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CAP.with(|cap| {
            if cap.get() != 0 {
                LIVE.with(|live| live.set(live.get().saturating_sub(layout.size())));
            }
        });
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: CapAlloc = CapAlloc;

// decode under a live-bytes cap; returns the result and the peak
fn decode_capped_at(
    bytes: &[u8],
    cap: usize,
    max_w: u16,
    max_h: u16,
) -> (Result<(u16, u16), &'static str>, usize) {
    LIVE.with(|l| l.set(0));
    PEAK.with(|p| p.set(0));
    CAP.with(|c| c.set(cap));
    let res =
        decode_jpeg(SliceReader::new(bytes), max_w, max_h).map(|img| (img.width, img.height));
    CAP.with(|c| c.set(0));
    let peak = PEAK.with(|p| p.get());
    (res, peak)
}

fn decode_capped(bytes: &[u8], cap: usize) -> (Result<(u16, u16), &'static str>, usize) {
    decode_capped_at(bytes, cap, 600, 800)
}

// 418x269 progressive, 2x2 luma subsampling: 27 MCUs across, so the
// 48 KB band target lands on K=3 (~40 KB) while the output plane needs
// a further 14 KB. With ~50 KB the band has to fall back to K=1
const INLINE_0007: &[u8] = include_bytes!("fixtures/exhalation_inline_0007.jpg");

#[test]
fn progressive_inline_decodes_within_50k() {
    let (res, peak) = decode_capped(INLINE_0007, 50 * 1024);
    let dims = res.unwrap_or_else(|e| panic!("decode failed under a 50 KB cap: {e} (peak {peak})"));
    assert_eq!(dims, (418, 269));
}

#[test]
fn progressive_cover_decodes_within_80k() {
    // 720x960 progressive; scaled 2:1 to fit 600x800
    let (res, peak) = decode_capped(
        include_bytes!("fixtures/exhalation_cover.jpg"),
        80 * 1024,
    );
    let dims = res.unwrap_or_else(|e| panic!("decode failed under an 80 KB cap: {e} (peak {peak})"));
    assert_eq!(dims, (360, 480));
}

#[test]
fn progressive_decode_reports_oom_rather_than_wedging() {
    // a cap below any workable footprint must fail cleanly
    let (res, _) = decode_capped(INLINE_0007, 8 * 1024);
    assert!(res.is_err(), "8 KB cap should not produce an image");
}

// 904x1388 baseline, 2x2 luma subsampling. At the reader's full page
// budget (464x758) it downscales 2:1, so the output plane alone is
// ~39 KB; one step down the budget it scales 4:1 and needs ~8 KB.
// This is the premise ReaderApp::oom_retry's ladder rests on: a decode
// the heap cannot hold at full size fits comfortably one step coarser
const FULLPAGE_1Q84: &[u8] = include_bytes!("fixtures/1q84_fullpage_00001.jpg");

#[test]
fn full_page_baseline_needs_more_than_40k_at_full_budget() {
    let (res, _) = decode_capped_at(FULLPAGE_1Q84, 40 * 1024, 464, 758);
    let err = res.expect_err("should not fit a 40 KB cap at the full page budget");
    assert!(err.contains("OOM"), "expected a budget failure, got {err}");
}

#[test]
fn full_page_baseline_fits_40k_one_budget_step_down() {
    let (res, peak) = decode_capped_at(FULLPAGE_1Q84, 40 * 1024, 232, 379);
    let dims = res.unwrap_or_else(|e| panic!("half budget should fit 40 KB: {e} (peak {peak})"));
    assert_eq!(dims, (226, 347));
}

// 3744x48 baseline greyscale: 468 MCUs across, downscaled 9:1 to fit a
// 416 px text column. The MCU-row band used to be materialised at full
// source width (3744 * 8 = 29 KB) and point-sampled; binning it to the
// 416 output columns costs 6.5 KB instead. Project Hail Mary's chapter
// banners are the same shape at 6776 px wide, where the full-width band
// was 53 KB — more than the reader has free — and no step of
// ReaderApp::oom_retry's ladder could shrink it, since it was sized
// from the source rather than the budget
const WIDE_BARS: &[u8] = include_bytes!("fixtures/synthetic_bars_3744x48.jpg");

#[test]
fn wide_image_band_scales_with_the_budget_not_the_source() {
    let (res, peak) = decode_capped_at(WIDE_BARS, 24 * 1024, 416, 700);
    let dims =
        res.unwrap_or_else(|e| panic!("wide image should fit a 24 KB cap: {e} (peak {peak})"));
    assert_eq!(dims, (416, 5));
    // a full-width band alone would have overrun the cap
    assert!(peak < 24 * 1024, "peak {peak} should stay under the cap");
}
