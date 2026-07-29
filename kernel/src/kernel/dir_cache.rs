// directory listing cache: sorted entries with title resolution
// loaded lazily from SD, held in RAM, invalidated on demand

use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage::{DirEntry, DirPage, MAX_TITLE_LINE, TITLES_FILE};
use crate::error::Result;

const MAX_DIR_ENTRIES: usize = 128;

// carry buffer for lines split across read chunks. save_title never
// writes more than MAX_TITLE_LINE; the slack absorbs foreign lines that
// are a little longer, and anything past it is dropped as poisoned.
const CARRY_CAP: usize = MAX_TITLE_LINE + 32;

// where the title reader stands between newlines
enum LineState {
    // nothing buffered; the next span is a whole line
    Fresh,
    // n bytes of a split line held in the carry buffer
    Carrying(usize),
    // a line too long for the carry buffer: drop through its newline
    Poisoned,
}

pub struct DirCache {
    entries: [DirEntry; MAX_DIR_ENTRIES],
    count: usize,
    valid: bool,
}

impl Default for DirCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DirCache {
    pub const fn new() -> Self {
        Self {
            entries: [DirEntry::EMPTY; MAX_DIR_ENTRIES],
            count: 0,
            valid: false,
        }
    }

    pub fn ensure_loaded(&mut self, sd: &SdStorage) -> Result<()> {
        if self.valid {
            return Ok(());
        }

        let count = sd.list_root_files(&mut self.entries)?;
        self.count = count;
        sort_entries(&mut self.entries, self.count);
        self.load_titles(sd);
        for i in 0..self.count {
            self.entries[i].humanize_sfn();
        }
        self.valid = true;
        Ok(())
    }

    fn load_titles(&mut self, sd: &SdStorage) {
        // TITLES.BIN is append-only, so it can exceed any single read
        // buffer (historic builds appended a line on every book open).
        // stream the whole file in chunks, carrying partial lines
        // across chunk boundaries; later lines overwrite earlier ones.
        const READ_CAP: u32 = 256 * 1024; // sanity bound for corrupt files
        let mut buf = [0u8; 2048];
        let mut carry = [0u8; CARRY_CAP];
        let mut state = LineState::Fresh;
        let mut offset = 0u32;

        while offset < READ_CAP {
            let n = match sd.read_chunk_in_plump(TITLES_FILE, offset, &mut buf) {
                Ok(n) => n,
                Err(_) => return,
            };
            if n == 0 {
                break;
            }
            offset += n as u32;

            let mut chunk = &buf[..n];
            while let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
                let (part, rest) = chunk.split_at(pos);
                match state {
                    // the rest of an oversized line: the newline ends it
                    LineState::Poisoned => {}
                    LineState::Carrying(len) if len + part.len() <= CARRY_CAP => {
                        carry[len..len + part.len()].copy_from_slice(part);
                        self.apply_title_line(&carry[..len + part.len()]);
                    }
                    LineState::Carrying(_) => {}
                    LineState::Fresh if !part.is_empty() => self.apply_title_line(part),
                    LineState::Fresh => {}
                }
                state = LineState::Fresh;
                chunk = &rest[1..];
            }

            // chunk tail with no newline: carry it into the next read
            let len = match state {
                LineState::Poisoned => continue,
                LineState::Carrying(len) => len,
                LineState::Fresh => 0,
            };
            state = if len + chunk.len() <= CARRY_CAP {
                carry[len..len + chunk.len()].copy_from_slice(chunk);
                LineState::Carrying(len + chunk.len())
            } else {
                LineState::Poisoned
            };
        }
    }

    fn apply_title_line(&mut self, line: &[u8]) {
        let tab_pos = match line.iter().position(|&b| b == b'\t') {
            Some(p) => p,
            None => return,
        };
        let file_part = &line[..tab_pos];
        let title_part = &line[tab_pos + 1..];
        if title_part.is_empty() {
            return;
        }

        let file_str = match core::str::from_utf8(file_part) {
            Ok(s) => s,
            Err(_) => return,
        };

        if let Some(entry) = self.entries[..self.count]
            .iter_mut()
            .find(|e| e.name_str().eq_ignore_ascii_case(file_str))
        {
            entry.set_title(title_part);
        }
    }

    pub fn page(&self, offset: usize, buf: &mut [DirEntry]) -> DirPage {
        let total = self.count;
        let start = offset.min(total);
        let end = (start + buf.len()).min(total);
        let count = end - start;
        buf[..count].clone_from_slice(&self.entries[start..end]);
        DirPage { total, count }
    }

    // look up the display title for a filename (case-insensitive)
    pub fn find_title(&self, filename: &[u8]) -> Option<&[u8]> {
        let name = core::str::from_utf8(filename).ok()?;
        self.entries[..self.count]
            .iter()
            .find(|e| e.name_str().eq_ignore_ascii_case(name))
            .and_then(|e| {
                if !e.title.is_empty() {
                    Some(e.title.as_bytes())
                } else {
                    None
                }
            })
    }

    // update the in-RAM title for a filename (case-insensitive), so a
    // freshly saved title shows without an invalidate + SD reload
    pub fn update_title(&mut self, filename: &[u8], title: &[u8]) {
        let Ok(name) = core::str::from_utf8(filename) else {
            return;
        };
        if let Some(entry) = self.entries[..self.count]
            .iter_mut()
            .find(|e| e.name_str().eq_ignore_ascii_case(name))
        {
            entry.set_title(title);
        }
    }
}

// insertion sort; count <= 128
fn sort_entries(entries: &mut [DirEntry], count: usize) {
    entries[..count].sort_unstable();
}
