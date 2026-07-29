// shared RECENT record: the reader writes it, the home
// continue-reading card reads it. one layout, one encoder, one decoder.
//
// on-disk layout (see RECENT_FILE in the kernel for the name):
//
//     filename \0 title \0 author \0 progress
//
// the three text fields are raw UTF-8 bytes, each NUL-terminated and
// each capped at the capacity of the FixedStr both sides hold it in.
// progress is a single trailing byte (0..=100), not decimal text, and
// has no terminator.

/// scratch/read buffer size both sides use for the whole record.
pub const BUF_LEN: usize = 196;

pub const FILENAME_CAP: usize = 32;
pub const TITLE_CAP: usize = 64;
pub const AUTHOR_CAP: usize = 64;

/// borrowed view of the record: `encode` reads out of one, `decode`
/// hands one back pointing into the caller's buffer.
#[derive(Clone, Copy)]
pub struct RecentRecord<'a> {
    pub filename: &'a [u8],
    pub title: &'a [u8],
    pub author: &'a [u8],
    pub progress: u8,
}

impl<'a> RecentRecord<'a> {
    pub const EMPTY: Self = Self {
        filename: &[],
        title: &[],
        author: &[],
        progress: 0,
    };

    /// serialize into `out`, returning the number of bytes written.
    pub const fn encode(&self, out: &mut [u8; BUF_LEN]) -> usize {
        let mut pos = 0;
        pos = put_field(out, pos, self.filename, FILENAME_CAP);
        pos = put_field(out, pos, self.title, TITLE_CAP);
        pos = put_field(out, pos, self.author, AUTHOR_CAP);
        out[pos] = self.progress;
        pos + 1
    }

    /// parse bytes previously written by `encode`. tolerates a
    /// truncated tail: fields past the end decode empty, and a missing
    /// progress byte decodes as 0.
    pub fn decode(data: &'a [u8]) -> Self {
        let mut fields = data.splitn(4, |&b| b == 0);
        Self {
            filename: fields.next().unwrap_or(&[]),
            title: fields.next().unwrap_or(&[]),
            author: fields.next().unwrap_or(&[]),
            progress: fields.next().and_then(|r| r.first().copied()).unwrap_or(0),
        }
    }
}

// copy at most `cap` bytes of `src`, then the NUL terminator
const fn put_field(out: &mut [u8; BUF_LEN], mut pos: usize, src: &[u8], cap: usize) -> usize {
    let n = if src.len() < cap { src.len() } else { cap };
    let mut i = 0;
    while i < n {
        out[pos] = src[i];
        pos += 1;
        i += 1;
    }
    out[pos] = 0;
    pos + 1
}

// golden bytes: encode is const-evaluable, so the on-disk layout is
// pinned at compile time rather than by a host test (esp-hal is
// riscv-only, this crate has no host test target).
const _: () = {
    const fn check(rec: &RecentRecord<'_>, expect: &[u8]) {
        let mut buf = [0u8; BUF_LEN];
        let n = rec.encode(&mut buf);
        assert!(n == expect.len());
        let mut i = 0;
        while i < n {
            assert!(buf[i] == expect[i]);
            i += 1;
        }
    }

    // populated record: 0x2a is progress 42, emitted raw
    check(
        &RecentRecord {
            filename: b"BOOK.EPB",
            title: b"Moby Dick",
            author: b"Melville",
            progress: 42,
        },
        b"BOOK.EPB\0Moby Dick\0Melville\0\x2a",
    );

    // every field empty: three bare terminators plus a zero progress
    check(&RecentRecord::EMPTY, b"\0\0\0\0");

    // no author (the non-epub case) still emits its terminator
    check(
        &RecentRecord {
            filename: b"A.TXT",
            title: b"A",
            author: b"",
            progress: 100,
        },
        b"A.TXT\0A\0\0\x64",
    );

    // over-long fields truncate at their cap instead of overrunning
    check(
        &RecentRecord {
            filename: b"0123456789012345678901234567890123456789",
            title: b"",
            author: b"",
            progress: 0,
        },
        b"01234567890123456789012345678901\0\0\0\0",
    );
};
