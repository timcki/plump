// the per-book record: reading position and statistics in one file
//
// `_PLUMP/STATS/<filename>` is the book's irreplaceable state. it is
// keyed by the filename, not the bundle hash, so a rebuilt bundle keeps
// the place, and Forget book already deletes it. one writer (the
// reader's deferred flush), one reader per screen, one set of numbers
// everywhere.
//
// key=value text, one per line; the stats screen parses the first
// three keys and ignores the rest. `sum` closes the file with an fnv1a
// of everything before it so a torn write loses the position, never
// invents one.

use core::fmt::Write as _;

use crate::apps::stats::{ReadingStats, STATS_DIR};
use crate::kernel::KernelHandle;
use crate::ui::StackFmt;
use plump_kernel::util::hash::fnv1a;

/// `Position::para` / `word` when no anchor could be counted.
pub const NO_ANCHOR: u32 = u32::MAX;

/// Where the reader was, in every unit a restore or a screen needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// size of the source file the offsets belong to
    pub archive_size: u32,
    /// spine index (0 for a txt book)
    pub chapter: u16,
    /// byte offset into the stripped chapter stream
    pub byte_offset: u32,
    /// content format that produced `byte_offset`; 0 for txt
    pub content_fmt: u8,
    /// paragraph ordinal within the chapter, or `NO_ANCHOR`
    pub para: u32,
    /// word ordinal within the paragraph, or `NO_ANCHOR`
    pub word: u32,
    /// hash of the layout key `page` was counted under
    pub layout_key: u32,
    /// page within the chapter under that layout, 0-based
    pub page: u16,
    /// chapter number as the device names it (TOC order), 1-based
    pub chapter_no: u16,
    /// chapters as the device counts them, 0 when unknown
    pub chapter_count: u16,
    /// progress through the book, percent
    pub progress_pct: u8,
    /// book font size index at the time
    pub font_idx: u8,
}

impl Position {
    #[inline]
    pub fn anchor(&self) -> Option<smol_epub::markup::Anchor> {
        (self.para != NO_ANCHOR && self.word != NO_ANCHOR).then_some(smol_epub::markup::Anchor {
            para: self.para,
            word: self.word,
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BookRecord {
    pub stats: ReadingStats,
    pub pos: Option<Position>,
    /// bumped on every save; a tie-breaker for anyone comparing copies
    pub seq: u32,
}

const BUF_LEN: usize = 320;

impl BookRecord {
    pub const EMPTY: Self = Self {
        stats: ReadingStats::EMPTY,
        pos: None,
        seq: 0,
    };

    pub fn load(k: &mut KernelHandle<'_>, filename: &str) -> Option<Self> {
        let mut buf = [0u8; BUF_LEN];
        let n = k
            .sd()
            .read_chunk_in_plump_subdir(STATS_DIR, filename, 0, &mut buf)
            .ok()?;
        if n == 0 {
            return None;
        }
        Some(Self::parse(&buf[..n]))
    }

    pub fn save(&self, k: &mut KernelHandle<'_>, filename: &str) -> crate::error::Result<()> {
        let mut buf = [0u8; BUF_LEN];
        let len = self.encode(&mut buf);
        k.sd().ensure_plump_subdir(STATS_DIR)?;
        k.sd().write_in_plump_subdir(STATS_DIR, filename, &buf[..len])
    }

    pub fn encode(&self, out: &mut [u8; BUF_LEN]) -> usize {
        let mut fmt = StackFmt::<BUF_LEN>::new();
        let s = &self.stats;
        let _ = write!(
            fmt,
            "pages={}\ntime={}\nsessions={}\nseq={}\n",
            s.pages, s.time_secs, s.sessions, self.seq
        );
        if let Some(p) = self.pos {
            let _ = write!(
                fmt,
                "size={}\nch={}\noff={}\nfmt={}\n",
                p.archive_size, p.chapter, p.byte_offset, p.content_fmt
            );
            if p.anchor().is_some() {
                let _ = write!(fmt, "para={}\nword={}\n", p.para, p.word);
            }
            let _ = write!(
                fmt,
                "lkey={}\npage={}\nchno={}\nchn={}\npct={}\nfont={}\n",
                p.layout_key, p.page, p.chapter_no, p.chapter_count, p.progress_pct, p.font_idx
            );
        }
        let body = fmt.as_str().as_bytes();
        let sum = fnv1a(body);
        let mut tail = StackFmt::<24>::new();
        let _ = write!(tail, "sum={:08X}\n", sum);
        let n = body.len().min(out.len());
        out[..n].copy_from_slice(&body[..n]);
        let t = tail.as_str().as_bytes();
        let m = t.len().min(out.len() - n);
        out[n..n + m].copy_from_slice(&t[..m]);
        n + m
    }

    /// Stats always parse; the position only when the checksum line is
    /// present and matches, so a file cut short mid-write reopens the
    /// book at its start rather than somewhere the bytes happened to say.
    pub fn parse(data: &[u8]) -> Self {
        let mut rec = Self {
            stats: ReadingStats::parse(data),
            pos: None,
            seq: 0,
        };
        let mut p = Position::default();
        let mut have_pos = false;
        let mut summed = false;
        let mut p_para = NO_ANCHOR;
        let mut p_word = NO_ANCHOR;
        for (line_start, line) in split_lines(data) {
            let Some(eq) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let key = &line[..eq];
            let val = &line[eq + 1..];
            match key {
                b"seq" => rec.seq = parse_u32(val),
                b"size" => {
                    p.archive_size = parse_u32(val);
                    have_pos = true;
                }
                b"ch" => p.chapter = parse_u32(val) as u16,
                b"off" => p.byte_offset = parse_u32(val),
                b"fmt" => p.content_fmt = parse_u32(val) as u8,
                b"para" => p_para = parse_u32(val),
                b"word" => p_word = parse_u32(val),
                b"lkey" => p.layout_key = parse_u32(val),
                b"page" => p.page = parse_u32(val) as u16,
                b"chno" => p.chapter_no = parse_u32(val) as u16,
                b"chn" => p.chapter_count = parse_u32(val) as u16,
                b"pct" => p.progress_pct = parse_u32(val).min(100) as u8,
                b"font" => p.font_idx = parse_u32(val) as u8,
                b"sum" => {
                    summed = parse_hex(val) == fnv1a(&data[..line_start]);
                    break;
                }
                _ => {}
            }
        }
        if have_pos && summed {
            p.para = p_para;
            p.word = p_word;
            rec.pos = Some(p);
        } else if have_pos {
            log::warn!("book record: checksum mismatch, position dropped");
        }
        rec
    }
}

// lines with their start offsets, trailing '\r' and spaces trimmed
fn split_lines(data: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let mut start = 0usize;
    core::iter::from_fn(move || {
        if start >= data.len() {
            return None;
        }
        let rest = &data[start..];
        let len = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        let at = start;
        start += len + 1;
        let mut line = &rest[..len];
        while let Some((&last, head)) = line.split_last() {
            if last.is_ascii_whitespace() {
                line = head;
            } else {
                break;
            }
        }
        Some((at, line))
    })
}

fn parse_u32(s: &[u8]) -> u32 {
    let mut n: u32 = 0;
    for &b in s {
        if b.is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add((b - b'0') as u32);
        }
    }
    n
}

fn parse_hex(s: &[u8]) -> u32 {
    let mut n: u32 = 0;
    for &b in s {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => continue,
        };
        n = (n << 4) | d as u32;
    }
    n
}
