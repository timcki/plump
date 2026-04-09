// directory listing cache: sorted entries with title resolution
// loaded lazily from SD, held in RAM, invalidated on demand

use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage::{DirEntry, DirPage, TITLES_FILE};
use crate::error::Result;

const MAX_DIR_ENTRIES: usize = 128;

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
        let mut buf = [0u8; 4096];
        let n = match sd.read_file_start_in_dir(sd.data_dir(), TITLES_FILE, &mut buf) {
            Ok((_, n)) => n,
            Err(_) => return,
        };

        for line in buf[..n].split(|&b| b == b'\n') {
            if !line.is_empty() {
                self.apply_title_line(line);
            }
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

    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    pub fn next_untitled_epub(&self, from: usize) -> Option<(usize, crate::util::FixedStr<13>)> {
        for i in from..self.count {
            let e = &self.entries[i];
            if e.has_real_title() || e.is_dir {
                continue;
            }
            let name = e.name.as_bytes();
            if name.len() >= 5
                && name[name.len() - 5] == b'.'
                && name[name.len() - 4..].eq_ignore_ascii_case(b"EPUB")
            {
                return Some((i, e.name));
            }
        }
        None
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

    pub fn set_entry_title(&mut self, index: usize, title: &[u8]) {
        if index < self.count {
            self.entries[index].set_title(title);
        }
    }
}

// insertion sort; count <= 128
fn sort_entries(entries: &mut [DirEntry], count: usize) {
    entries[..count].sort_unstable();
}
