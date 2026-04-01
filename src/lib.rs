// plump - e-reader firmware for the XTEink X4

#![no_std]

extern crate alloc;

pub use plump_kernel::board;
pub use plump_kernel::drivers;
pub use plump_kernel::error;
pub use plump_kernel::kernel;

pub mod apps;
pub mod fonts;
pub mod ui;
