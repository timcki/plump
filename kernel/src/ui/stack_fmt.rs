// no-alloc fmt::Write buffers; silently truncate on overflow

pub struct StackFmt<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Default for StackFmt<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> StackFmt<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; N],
            len: 0,
        }
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl<const N: usize> core::fmt::Write for StackFmt<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // borrow the owned buffer and reuse the one truncating append
        let mut w = BorrowedFmt {
            buf: &mut self.buf,
            pos: self.len,
        };
        w.append(s);
        self.len = w.pos;
        Ok(())
    }
}

pub struct BorrowedFmt<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> BorrowedFmt<'a> {
    #[inline]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.pos]).unwrap_or("")
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pos == 0
    }

    // append as much of `s` as fits, silently dropping the rest
    #[inline]
    fn append(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let n = bytes.len().min(self.buf.len() - self.pos);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
    }
}

impl core::fmt::Write for BorrowedFmt<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.append(s);
        Ok(())
    }
}

#[inline]
pub fn stack_fmt(buf: &mut [u8], f: impl FnOnce(&mut BorrowedFmt<'_>)) -> usize {
    let mut w = BorrowedFmt::new(buf);
    f(&mut w);
    w.pos
}
