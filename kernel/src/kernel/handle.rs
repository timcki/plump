// kernel handle: app-facing API surface
//
// provides sd() for direct storage access, dir-cache coordination,
// system info, and cache accessors

use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage::{DirEntry, DirPage};
use crate::error::Result;
use crate::kernel::bookmarks::{self, BookmarkCache};
use crate::kernel::daystats::{DayKey, DayStats};
use crate::kernel::dir_cache::DirCache;

// synchronous API surface for apps
//
// borrows the kernel's services half for the duration of an app
// lifecycle method; no SPI, no generics, no driver types visible to
// apps. the screen half stays free, so background work can run
// through a handle while a waveform session holds the display
pub struct KernelHandle<'k> {
    pub(crate) svc: &'k mut super::Services,
}

impl<'k> KernelHandle<'k> {
    pub(crate) fn new(svc: &'k mut super::Services) -> Self {
        Self { svc }
    }

    /// Direct access to the SD storage for file I/O.
    #[inline]
    pub fn sd(&self) -> &SdStorage {
        &self.svc.sd
    }

    pub fn dir_page(&mut self, offset: usize, buf: &mut [DirEntry]) -> Result<DirPage> {
        let k = &mut *self.svc;
        k.dir_cache.ensure_loaded(&k.sd)?;
        Ok(k.dir_cache.page(offset, buf))
    }

    // system info (sync, no I/O)

    #[inline]
    pub fn battery_mv(&self) -> u16 {
        self.svc.cached_battery_mv
    }

    #[inline]
    pub fn sd_ok(&self) -> bool {
        self.svc.sd_ok
    }

    pub fn ensure_dir_cache_loaded(&mut self) -> Result<()> {
        let k = &mut *self.svc;
        k.dir_cache.ensure_loaded(&k.sd)
    }

    // direct cache accessors

    /// Bookmark cache, loaded if it wasn't already. The returned view
    /// needs no per-call "is it loaded" test.
    #[inline]
    pub fn bookmarks(&mut self) -> bookmarks::Loaded<'_> {
        let k = &mut *self.svc;
        k.bm_cache.loaded(&k.sd)
    }

    #[inline]
    pub fn bookmark_cache_mut(&mut self) -> &mut BookmarkCache {
        &mut *self.svc.bm_cache
    }

    #[inline]
    pub fn dir_cache_mut(&mut self) -> &mut DirCache {
        &mut *self.svc.dir_cache
    }

    /// Today's reading stats (pages + seconds since the last calendar
    /// rollover). Chrome reads this; the active reader mutates it
    /// during page turns and on session-time accumulation.
    #[inline]
    pub fn day_stats(&self) -> &DayStats {
        &*self.svc.day_stats
    }

    #[inline]
    pub fn day_stats_mut(&mut self) -> &mut DayStats {
        &mut *self.svc.day_stats
    }

    /// Current day key (derived from `_PLUMP/DAYSTATS.BIN` mtime).
    /// None when the SD card has no usable wall clock, in which case
    /// counters accumulate from boot without ever rolling over.
    #[inline]
    pub fn today_key(&self) -> Option<DayKey> {
        self.svc.today_key
    }
}
