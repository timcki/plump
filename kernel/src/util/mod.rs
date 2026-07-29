// utility modules: small, reusable components without hardware dependencies

mod fixed_str;
pub mod hash;
mod record;
mod utf8;

pub use fixed_str::FixedStr;
pub use record::{Field, mark_span, read_fixed_str, write_fixed_str};
pub use utf8::{Utf8Iter, decode_utf8_char};
