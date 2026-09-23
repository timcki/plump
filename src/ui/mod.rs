// ui re-exports: kernel primitives + app-side font-dependent widgets
//
// kernel ui (Region, Alignment, StackFmt, statusbar constants) is
// re-exported from plump-kernel; font-dependent widgets (BitmapLabel,
// QuickMenu, the new Chrome) live here in the distro.

pub mod chrome;

// kernel-side primitives
pub use plump_kernel::ui::stack_fmt;
pub use plump_kernel::ui::*;

// app-side font-dependent widgets
pub use crate::apps::widgets::QuickMenu;
pub use crate::apps::widgets::bitmap_label::BitmapLabel;
pub use crate::apps::widgets::list::ListSelection;
pub use crate::apps::widgets::quick_menu;
pub use chrome::{Chrome, SectionLabel, TopFonts, TopStatus};
