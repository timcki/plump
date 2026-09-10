// bookmark cache: 16 slots, RAM-resident, flushed to SD on dirty

use crate::drivers::sdcard::SdStorage;
use crate::util::FixedStr;
use crate::util::hash::fnv1a_icase;

pub const BOOKMARK_FILE: &str = "BKMK.BIN";
pub const SLOTS: usize = 16;
pub const RECORD_LEN: usize = 48;
pub const FILE_LEN: usize = SLOTS * RECORD_LEN; // 768B
pub const FILENAME_CAP: usize = 32;

crate::record! {
    #[derive(Clone, Copy)]
    pub struct BookmarkSlot [RECORD_LEN] {
        name_hash:   u32 @ 0,
        byte_offset: u32 @ 4,
        chapter:     u16 @ 8,
        /// bit 0 of the flags word
        valid:   bool16 @ 10,
        generation:  u16 @ 12,
        // 15 pad
        filename: {str FILENAME_CAP} @ (14, 16),
    }
}

impl BookmarkSlot {
    pub const EMPTY: Self = Self {
        name_hash: 0,
        byte_offset: 0,
        chapter: 0,
        valid: false,
        generation: 0,
        filename: FixedStr::EMPTY,
    };

    pub fn filename_str(&self) -> &str {
        self.filename.as_str()
    }

    fn matches_name(&self, name: &[u8]) -> bool {
        self.filename.eq_ignore_ascii_case(name)
    }
}

#[derive(Clone, Copy)]
pub struct BmListEntry {
    pub filename: FixedStr<FILENAME_CAP>,
}

impl BmListEntry {
    pub const EMPTY: Self = Self {
        filename: FixedStr::EMPTY,
    };

    pub fn filename_str(&self) -> &str {
        self.filename.as_str()
    }
}

// 16-slot LRU bookmark cache; flushed to _PLUMP/BKMK.BIN periodically
pub struct BookmarkCache {
    slots: [BookmarkSlot; SLOTS],
    count: usize, // slots present in file; new saves past this extend count
    dirty: bool,
    loaded: bool,
}

impl Default for BookmarkCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BookmarkCache {
    pub const fn new() -> Self {
        Self {
            slots: [BookmarkSlot::EMPTY; SLOTS],
            count: 0,
            dirty: false,
            loaded: false,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn ensure_loaded(&mut self, sd: &SdStorage) {
        if self.loaded {
            return;
        }
        let mut buf = [0u8; FILE_LEN];
        let slot_count =
            match sd.read_file_start_in_dir(sd.data_dir(), BOOKMARK_FILE, &mut buf) {
                Ok((_, n)) => (n / RECORD_LEN).min(SLOTS),
                Err(_) => 0,
            };

        for i in 0..slot_count {
            let base = i * RECORD_LEN;
            self.slots[i] = BookmarkSlot::decode(&buf[base..base + RECORD_LEN])
                .unwrap_or(BookmarkSlot::EMPTY);
        }
        for i in slot_count..SLOTS {
            self.slots[i] = BookmarkSlot::EMPTY;
        }

        self.count = slot_count;
        self.dirty = false;
        self.loaded = true;

        log::debug!("bookmarks: loaded {} slots from SD", slot_count);
    }

    /// Load if needed and hand back a view whose reads need no
    /// "did anyone load this yet?" check. Mint this wherever an
    /// `&SdStorage` is in reach; the guarded `save` below stays for the
    /// app-trait path, which only ever receives `&mut BookmarkCache`.
    pub fn loaded(&mut self, sd: &SdStorage) -> Loaded<'_> {
        self.ensure_loaded(sd);
        Loaded { cache: self }
    }

    fn find_slot(&self, filename: &[u8]) -> Option<BookmarkSlot> {
        let key = fnv1a_icase(filename);
        self.slots[..self.count]
            .iter()
            .find(|s| s.valid && s.name_hash == key && s.matches_name(filename))
            .copied()
    }

    fn list_all(&self, out: &mut [BmListEntry]) -> usize {
        let mut gens = [0u16; SLOTS];
        let mut count = 0usize;

        for i in 0..self.count {
            if count >= out.len() {
                break;
            }
            let slot = &self.slots[i];
            if slot.valid && !slot.filename.is_empty() {
                gens[count] = slot.generation;
                out[count] = BmListEntry {
                    filename: slot.filename,
                };
                count += 1;
            }
        }

        for i in 1..count {
            let key_gen = gens[i];
            let key_entry = out[i];
            let mut j = i;
            while j > 0 && gens[j - 1] < key_gen {
                gens[j] = gens[j - 1];
                out[j] = out[j - 1];
                j -= 1;
            }
            gens[j] = key_gen;
            out[j] = key_entry;
        }

        count
    }

    /// Guarded fallback for the app-trait path (`App::save_state` only
    /// ever gets `&mut BookmarkCache`, with no `&SdStorage` in reach to
    /// mint a [`Loaded`] from). Prefer `Loaded::save`.
    pub fn save(&mut self, filename: &[u8], byte_offset: u32, chapter: u16) {
        if !self.loaded {
            log::warn!("bookmarks: save called before load, ignoring");
            return;
        }
        self.write_slot(filename, byte_offset, chapter);
    }

    /// Invalidate this book's slot. The record stays in the file as a
    /// dead slot, which is what `find_slot` already skips, so the
    /// layout of the other 15 is untouched. True when something was
    /// cleared; the flush rides the usual housekeeping tick.
    fn forget_slot(&mut self, filename: &[u8]) -> bool {
        let key = fnv1a_icase(filename);
        let mut cleared = false;
        for slot in self.slots[..self.count].iter_mut() {
            if slot.valid && slot.name_hash == key && slot.matches_name(filename) {
                *slot = BookmarkSlot::EMPTY;
                cleared = true;
            }
        }
        if cleared {
            self.dirty = true;
        }
        cleared
    }

    fn write_slot(&mut self, filename: &[u8], byte_offset: u32, chapter: u16) {
        let key = fnv1a_icase(filename);

        let mut max_gen: u16 = 0;
        let mut target: Option<usize> = None;
        let mut first_free: Option<usize> = None;
        let mut lru_slot: Option<usize> = None;
        let mut lru_gen: u16 = u16::MAX;

        for i in 0..self.count {
            let slot = &self.slots[i];

            if !slot.valid {
                if first_free.is_none() {
                    first_free = Some(i);
                }
                continue;
            }

            if slot.generation > max_gen {
                max_gen = slot.generation;
            }
            if slot.generation < lru_gen {
                lru_gen = slot.generation;
                lru_slot = Some(i);
            }

            if slot.name_hash == key && slot.matches_name(filename) {
                target = Some(i);
                break;
            }
        }

        let write_slot = target.or(first_free).unwrap_or_else(|| {
            if self.count >= SLOTS {
                // evict the least-recently-used valid slot. if no valid
                // LRU candidate was found (every slot was invalid), they
                // would all have been captured by first_free above, so
                // this path is unreachable; fall back to 0 as a safe
                // default rather than panicking
                lru_slot.unwrap_or(0)
            } else {
                self.count
            }
        });

        let generation = max_gen.wrapping_add(1);

        let new_slot = BookmarkSlot {
            name_hash: key,
            byte_offset,
            chapter,
            valid: true,
            generation,
            filename: FixedStr::from_bytes(filename),
        };

        self.slots[write_slot] = new_slot;

        if write_slot >= self.count {
            self.count = write_slot + 1;
        }
        debug_assert!(self.count <= SLOTS, "bookmark count exceeds slot limit");

        self.dirty = true;

        log::debug!(
            "bookmark: cached off={} ch={} gen={} for {:?}",
            byte_offset,
            chapter,
            generation,
            core::str::from_utf8(filename).unwrap_or("?"),
        );
    }

    pub fn flush(&mut self, sd: &SdStorage) {
        // an unloaded cache is also never dirty, so one test covers both
        if !self.dirty {
            return;
        }
        debug_assert!(self.loaded, "dirty bookmark cache was never loaded");

        let file_len = self.count * RECORD_LEN;
        let mut buf = [0u8; FILE_LEN];

        for i in 0..self.count {
            let base = i * RECORD_LEN;
            let rec = self.slots[i].encode();
            buf[base..base + RECORD_LEN].copy_from_slice(&rec);
        }

        match sd.write_file_in_dir(sd.data_dir(), BOOKMARK_FILE, &buf[..file_len]) {
            Ok(_) => {
                self.dirty = false;
                log::debug!("bookmarks: flushed {} slots to SD", self.count);
            }
            Err(e) => {
                log::warn!("bookmarks: flush failed: {}", e);
            }
        }
    }
}

/// A [`BookmarkCache`] that is known to be loaded, because the only way
/// to get one is [`BookmarkCache::loaded`]. Its methods carry no
/// "loaded?" test: the borrow is the proof.
pub struct Loaded<'a> {
    cache: &'a mut BookmarkCache,
}

impl Loaded<'_> {
    #[inline]
    pub fn find(&self, filename: &[u8]) -> Option<BookmarkSlot> {
        self.cache.find_slot(filename)
    }

    #[inline]
    pub fn load_all(&self, out: &mut [BmListEntry]) -> usize {
        self.cache.list_all(out)
    }

    #[inline]
    pub fn save(&mut self, filename: &[u8], byte_offset: u32, chapter: u16) {
        self.cache.write_slot(filename, byte_offset, chapter);
    }

    /// Drop this book's reading position. Nothing rebuilds it: the
    /// book reopens at page 1.
    #[inline]
    pub fn forget(&mut self, filename: &[u8]) -> bool {
        self.cache.forget_slot(filename)
    }
}
