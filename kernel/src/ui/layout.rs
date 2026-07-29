// shared layout constants for UI rendering
//
// CONTENT_TOP / HEADER_W etc. now match the v1 theme so existing apps
// shift below the new chrome top bar automatically (no per-app
// migration required for chunk D). the theme struct in ui::theme is
// authoritative; these constants must stay in sync.
//
// only constants used by 2+ apps belong here. single-use layout
// values should be defined locally.

use super::theme::Theme;
use crate::board::SCREEN_W;

// theme-derived constants. const evaluation: Theme::default_v1 is
// const fn and Theme is Copy, so this resolves at compile time.
const THEME: Theme = Theme::default_v1();

/// y where app content starts, below the new chrome top status bar.
pub const CONTENT_TOP: u16 = THEME.top_bar_h + THEME.margin_sm;
pub const LARGE_MARGIN: u16 = 16;
pub const FULL_CONTENT_W: u16 = SCREEN_W - 2 * LARGE_MARGIN;
pub const HEADER_W: u16 = 300;
