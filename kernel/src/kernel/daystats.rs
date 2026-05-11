// today's reading stats: pages turned + seconds spent since the
// current calendar day rolled over.
//
// the day is identified by a coarse "day key" derived from the FAT
// mtime of `_PLUMP/DAYSTATS.BIN` (the file we write into). on every
// session, the kernel reads the file's mtime, computes today_key, and
// compares it to the day_key stored inside the file. when they
// differ, the counters reset.
//
// the device has no battery-backed RTC; the SD card's RTC (when
// present) is our only source of wall-clock truth. cards without an
// RTC stamp every file at the FAT epoch (1980); for those, day_key
// derivation returns None and counters degrade to "pages this boot"
// (rollover never triggers automatically).
//
// on-disk format at `_PLUMP/DAYSTATS.BIN` (16 bytes, magic "DST1"):
//   0..4   u32 magic = 0x44_53_54_31
//   4..8   u32 day_key
//   8..10  u16 pages
//   10..14 u32 secs_today
//   14..16 reserved (zero)

use crate::drivers::sdcard::SdStorage;
use crate::error::Result;

pub const DAYSTATS_FILE: &str = "DAYSTATS.BIN";
pub const DAYSTATS_LEN: usize = 16;
const MAGIC: u32 = 0x44_53_54_31; // "DST1"

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DayStats {
    pub day_key: u32,
    pub pages: u16,
    pub secs_today: u32,
    dirty: bool,
}

impl DayStats {
    pub const EMPTY: Self = Self {
        day_key: 0,
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

    /// Reset counters when the calendar day has changed. `today_key`
    /// of 0 means "no usable wall clock"; we don't roll over in that
    /// case (the counters just keep accumulating since boot).
    pub fn rollover_if_new_day(&mut self, today_key: u32) {
        if today_key == 0 {
            return;
        }
        if today_key == self.day_key {
            return;
        }
        log::info!(
            "daystats: rollover key {} -> {} (was pages={} secs={})",
            self.day_key,
            today_key,
            self.pages,
            self.secs_today
        );
        self.day_key = today_key;
        self.pages = 0;
        self.secs_today = 0;
        self.dirty = true;
    }

    pub fn add_pages(&mut self, today_key: u32, n: u16) {
        if n == 0 {
            return;
        }
        self.rollover_if_new_day(today_key);
        self.pages = self.pages.saturating_add(n);
        self.dirty = true;
    }

    pub fn add_secs(&mut self, today_key: u32, secs: u32) {
        if secs == 0 {
            return;
        }
        self.rollover_if_new_day(today_key);
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
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC {
            return Self::EMPTY;
        }
        Self {
            day_key: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            pages: u16::from_le_bytes([buf[8], buf[9]]),
            secs_today: u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]),
            dirty: false,
        }
    }

    /// Write to SD. Clears the dirty flag on success.
    pub fn flush(&mut self, sd: &SdStorage) -> Result<()> {
        let mut buf = [0u8; DAYSTATS_LEN];
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&self.day_key.to_le_bytes());
        buf[8..10].copy_from_slice(&self.pages.to_le_bytes());
        buf[10..14].copy_from_slice(&self.secs_today.to_le_bytes());
        // bytes 14..16 reserved (zero)
        sd.write_in_plump(DAYSTATS_FILE, &buf)?;
        self.dirty = false;
        Ok(())
    }
}

impl Default for DayStats {
    fn default() -> Self {
        Self::EMPTY
    }
}
