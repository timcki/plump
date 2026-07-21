// widget primitives for 1-bit e-paper displays
//
// font-independent: Region, Alignment, stack measurement, StackFmt
// font-dependent widgets (BitmapLabel, QuickMenu, ButtonFeedback)
// live in the distro's apps::widgets module

pub mod layout;
pub mod painter;
pub mod stack_fmt;
pub mod statusbar;
pub mod theme;
mod widget;

pub use layout::{
    CONTENT_TOP, FULL_CONTENT_W, HEADER_W, LARGE_MARGIN, SECTION_GAP, TITLE_Y, TITLE_Y_OFFSET,
};
pub use painter::Painter;
pub use stack_fmt::{StackFmt, stack_fmt};
pub use statusbar::{
    BAR_HEIGHT, StackHwmDetail, free_stack_bytes, paint_stack, stack_high_water_mark,
    stack_hwm_detail,
};
pub use theme::Theme;
pub use widget::{
    Alignment, LoadingIndicator, ProgressBar, Region, wrap_next, wrap_prev,
};

pub use crate::board::{SCREEN_H, SCREEN_W};
