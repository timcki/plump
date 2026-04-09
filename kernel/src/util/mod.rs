// utility modules: small, reusable components without hardware dependencies

mod fixed_str;
mod utf8;

pub use fixed_str::FixedStr;
pub use utf8::{Utf8Iter, decode_utf8_char};
