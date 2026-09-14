// design tokens for the unified UI.
//
// lives in the kernel because the scheduler-driven chrome pass and the
// distro chrome widgets both need to see the same values. apps read
// the shared instance through `Painter::theme()`.
//
// values are mockup-derived (mockups/*.html). v1 is the only variant
// shipping today; the struct is sized for future runtime theming
// (light/dark, font scaling) without bumping disc layouts.

use crate::board::SCREEN_H;

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
            // the top bar is the navigation now: a screen nameplate
            // flanked by its two neighbours. all three are body text
            // (the current one bold), so the height is margin around
            // one line rather than room for a heading
            top_bar_h: 40,
            // nothing at the bottom but a margin. the tab bar was 64 px
            // of a 800 px display drawing a control that could not be
            // pressed; its job moved up top
            bottom_bar_h: 8,
            tracked_caption_px: 3,
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

}

impl Default for Theme {
    fn default() -> Self {
        Self::default_v1()
    }
}
