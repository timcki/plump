// design tokens for the unified UI.
//
// lives in the kernel because the scheduler-driven chrome pass and the
// distro chrome widgets both need to see the same values. apps access
// the live instance via `KernelHandle::theme()`.
//
// values are mockup-derived (mockups/*.html). v1 is the only variant
// shipping today; the struct is sized for future runtime theming
// (light/dark, font scaling) without bumping disc layouts.

use crate::board::{SCREEN_H, SCREEN_W};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    // margins / gaps
    pub margin_lg: u16,
    pub margin_md: u16,
    pub margin_sm: u16,
    pub section_gap: u16,

    // panel
    pub panel_radius: u16,
    pub panel_stroke: u16,
    pub row_h: u16,
    pub row_h_compact: u16,

    // chrome bars
    pub top_bar_h: u16,
    pub bottom_bar_h: u16,

    // tracked uppercase letter spacing for section captions, in px.
    // positive = expand; signed because future themes may compress.
    pub tracked_caption_px: i8,

    // 1 to 2 px progress bar height in the reader footer
    pub progress_bar_h: u16,
}

impl Theme {
    pub const fn default_v1() -> Self {
        Self {
            margin_lg: 16,
            margin_md: 12,
            margin_sm: 8,
            section_gap: 14,
            panel_radius: 6,
            panel_stroke: 2,
            row_h: 44,
            row_h_compact: 32,
            top_bar_h: 28,
            bottom_bar_h: 64,
            tracked_caption_px: 3,
            progress_bar_h: 2,
        }
    }

    /// y of the first content row, below the top status bar.
    #[inline]
    pub const fn content_top(&self) -> u16 {
        self.top_bar_h + self.margin_sm
    }

    /// y of the first row of the bottom tab bar.
    #[inline]
    pub const fn content_bottom(&self) -> u16 {
        SCREEN_H - self.bottom_bar_h
    }

    /// usable content height between top status and tab bar.
    #[inline]
    pub const fn content_h(&self) -> u16 {
        self.content_bottom() - self.content_top()
    }

    /// usable content width with `margin_lg` left and right.
    #[inline]
    pub const fn content_w(&self) -> u16 {
        SCREEN_W - 2 * self.margin_lg
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_v1()
    }
}
