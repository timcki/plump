// today's reading stats: pages turned + seconds spent since the
// current calendar day rolled over.
//
// the day is identified by a coarse `DayKey` derived from the FAT
// mtime of `_PLUMP/DAYSTATS.BIN` (the file we write into). on every
// session, the kernel reads the file's mtime, computes today's key,
// and compares it to the key stored inside the file. when they
// differ, the counters reset.
//
// the device has no battery-backed RTC; the SD card's RTC (when
// present) is our only source of wall-clock truth. cards without an
// RTC stamp every file at the FAT epoch (1980); for those, key
// derivation yields None and counters degrade to "pages this boot"
// (rollover never triggers automatically). on disk a missing key is
// the zero word, which is why `DayKey` wraps a NonZeroU32.

use core::num::NonZeroU32;

use crate::drivers::sdcard::SdStorage;
use crate::error::Result;
use crate::util::Field;

pub const DAYSTATS_FILE: &str = "DAYSTATS.BIN";
pub const DAYSTATS_LEN: usize = 16;
const MAGIC: u32 = 0x44_53_54_31; // "DST1"

/// Coarse day-of-year-since-1970 key. Monotonically increasing across
/// calendar days, so two keys compare for inequality to detect a
/// rollover without real calendar math. Never zero: the zero word is
/// the on-disk spelling of "no usable wall clock".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DayKey(NonZeroU32);

impl DayKey {
    /// Build a key from a raw day count; zero means "no wall clock".
    #[inline]
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

// on disk the key is a plain u32 whose zero value means "absent", so
// the Option is what the four bytes actually encode
impl Field for Option<DayKey> {
    const WIDTH: usize = 4;
    const ZERO: Self = None;

    #[inline]
    fn read(src: &[u8]) -> Option<Self> {
        Some(DayKey::new(u32::read(src)?))
    }

    #[inline]
    fn write(self, dst: &mut [u8]) {
        self.map_or(0, DayKey::get).write(dst);
    }
}

crate::record! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct DayStats [DAYSTATS_LEN] {
        day_key: {Option<DayKey>} @ 4,
        pages:   u16 @ 8,
        secs_today: u32 @ 10,
        // 14..16 reserved
    }
    fixed {
        magic: u32 @ 0 = MAGIC,
    }
    extra {
        dirty: bool = false,
    }
}

impl DayStats {
    pub const EMPTY: Self = Self {
        day_key: None,
        pages: 0,
        secs_today: 0,
        dirty: false,
    };

    #[inline]
    pub const fn pages(&self) -> u16 {
        self.pages
    }

    #[inline]
    pub const fn secs_today(&self) -> u32 {
        self.secs_today
    }

    #[inline]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Reset counters when the calendar day has changed. `None` means
    /// "no usable wall clock"; we don't roll over in that case (the
    /// counters just keep accumulating since boot).
    pub fn rollover_if_new_day(&mut self, today: Option<DayKey>) {
        let Some(today) = today else { return };
        if self.day_key == Some(today) {
            return;
        }
        log::info!(
            "daystats: rollover key {} -> {} (was pages={} secs={})",
            self.day_key.map_or(0, DayKey::get),
            today.get(),
            self.pages,
            self.secs_today
        );
        self.day_key = Some(today);
        self.pages = 0;
        self.secs_today = 0;
        self.dirty = true;
    }

    pub fn add_pages(&mut self, today: Option<DayKey>, n: u16) {
        if n == 0 {
            return;
        }
        self.rollover_if_new_day(today);
        self.pages = self.pages.saturating_add(n);
        self.dirty = true;
    }

    pub fn add_secs(&mut self, today: Option<DayKey>, secs: u32) {
        if secs == 0 {
            return;
        }
        self.rollover_if_new_day(today);
        self.secs_today = self.secs_today.saturating_add(secs);
        self.dirty = true;
    }

    /// Load from SD. Returns EMPTY on any error or invalid header.
    pub fn load(sd: &SdStorage) -> Self {
        let mut buf = [0u8; DAYSTATS_LEN];
        let Ok(n) = sd.read_chunk_in_plump(DAYSTATS_FILE, 0, &mut buf) else {
            return Self::EMPTY;
        };
        if n < DAYSTATS_LEN {
            return Self::EMPTY;
        }
        Self::decode(&buf).unwrap_or(Self::EMPTY)
    }

    /// Write to SD. Clears the dirty flag on success.
    pub fn flush(&mut self, sd: &SdStorage) -> Result<()> {
        sd.write_in_plump(DAYSTATS_FILE, &self.encode())?;
        self.dirty = false;
        Ok(())
    }
}

impl Default for DayStats {
    fn default() -> Self {
        Self::EMPTY
    }
}
