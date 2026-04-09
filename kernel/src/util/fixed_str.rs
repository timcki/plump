// fixed-capacity string: [u8; N] + length byte
// replaces the repeated (array, len) pattern across the kernel

use core::fmt;

#[derive(Clone, Copy, Eq)]
pub struct FixedStr<const N: usize> {
    buf: [u8; N],
    len: u8,
}

impl<const N: usize> FixedStr<N> {
    pub const EMPTY: Self = Self {
        buf: [0u8; N],
        len: 0,
    };

    pub fn from_bytes(src: &[u8]) -> Self {
        let n = src.len().min(N);
        let mut s = Self::EMPTY;
        s.buf[..n].copy_from_slice(&src[..n]);
        s.len = n as u8;
        s
    }

    pub fn set(&mut self, src: &[u8]) {
        let n = src.len().min(N);
        self.buf[..n].copy_from_slice(&src[..n]);
        self.len = n as u8;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("?")
    }

    /// access the raw backing array (for binary serialization)
    #[inline]
    pub fn raw_buf(&self) -> &[u8; N] {
        &self.buf
    }

    /// access the raw length byte (for binary serialization)
    #[inline]
    pub fn raw_len(&self) -> u8 {
        self.len
    }

    /// build from pre-filled array + length (for binary deserialization)
    #[inline]
    pub fn from_raw(buf: [u8; N], len: u8) -> Self {
        Self {
            buf,
            len: len.min(N as u8),
        }
    }

    /// case-insensitive ASCII comparison of content
    pub fn eq_ignore_ascii_case(&self, other: &[u8]) -> bool {
        self.len() == other.len() && self.as_bytes().eq_ignore_ascii_case(other)
    }

    /// mutable access to backing buffer for in-place transforms
    #[inline]
    pub fn buf_mut(&mut self) -> &mut [u8; N] {
        &mut self.buf
    }

    /// set length directly (for in-place transforms); clamped to N
    #[inline]
    pub fn set_len(&mut self, len: u8) {
        self.len = len.min(N as u8);
    }
}

impl<const N: usize> Default for FixedStr<N> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<const N: usize> PartialEq for FixedStr<N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl<const N: usize> AsRef<str> for FixedStr<N> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> AsRef<[u8]> for FixedStr<N> {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<const N: usize> fmt::Display for FixedStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> fmt::Debug for FixedStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{}\"", self.as_str())
    }
}
