// kernel handle: app-facing API surface
//
// provides sd() for direct storage access, sync-reader bridges for
// smol-epub, dir-cache coordination, system info, and cache accessors

use crate::drivers::sdcard::SdStorage;
use crate::drivers::storage::{DirEntry, DirPage};
use crate::error::{Error, Result};
use crate::kernel::bookmarks::BookmarkCache;
use crate::kernel::daystats::DayStats;
use crate::kernel::dir_cache::DirCache;
use crate::kernel::wake::uptime_secs;
use crate::ui::Theme;

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

    // smol-epub sync reader bridge
    //
    // smol-epub performs I/O through closures that return
    // Result<usize, &'static str>; these adapters convert
    // Error → &'static str at the boundary via the From impl.

    pub fn with_sync_reader<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(
            &mut dyn FnMut(&str, u32, &mut [u8]) -> core::result::Result<usize, &'static str>,
        ) -> R,
    {
        let sd = self.sd();
        let mut reader = |name: &str, offset: u32, buf: &mut [u8]| {
            sd.read_file_chunk(name, offset, buf)
                .map_err(|e: Error| -> &'static str { e.into() })
        };
        f(&mut reader)
    }

    pub fn with_sync_reader_app_subdir<F, R>(&mut self, dir: &str, f: F) -> R
    where
        F: FnOnce(
            &mut dyn FnMut(&str, u32, &mut [u8]) -> core::result::Result<usize, &'static str>,
        ) -> R,
    {
        let sd = self.sd();
        let mut reader = |name: &str, offset: u32, buf: &mut [u8]| {
            sd.read_chunk_in_plump_subdir(dir, name, offset, buf)
                .map_err(|e: Error| -> &'static str { e.into() })
        };
        f(&mut reader)
    }

    pub fn dir_page(&mut self, offset: usize, buf: &mut [DirEntry]) -> Result<DirPage> {
        let k = &mut *self.svc;
        k.dir_cache.ensure_loaded(&k.sd)?;
        Ok(k.dir_cache.page(offset, buf))
    }

    pub fn invalidate_dir_cache(&mut self) {
        self.svc.dir_cache.invalidate();
    }

    // system info (sync, no I/O)

    #[inline]
    pub fn battery_mv(&self) -> u16 {
        self.svc.cached_battery_mv
    }

    #[inline]
    pub fn uptime_secs(&self) -> u32 {
        uptime_secs()
    }

    #[inline]
    pub fn sd_ok(&self) -> bool {
        self.svc.sd_ok
    }

    pub fn ensure_dir_cache_loaded(&mut self) -> Result<()> {
        let k = &mut *self.svc;
        k.dir_cache.ensure_loaded(&k.sd)
    }

    /// Largest single allocation the heap can satisfy right now,
    /// found by binary-searching probe allocations (freed
    /// immediately; each probe is O(1) under TLSF). 1 KB resolution.
    /// Decoders should size big transient buffers against this
    /// instead of asking for a fixed worst case.
    pub fn largest_free_block(&self) -> usize {
        largest_free_block()
    }

    // direct cache accessors

    #[inline]
    pub fn bookmark_cache(&self) -> &BookmarkCache {
        &*self.svc.bm_cache
    }

    #[inline]
    pub fn bookmark_cache_mut(&mut self) -> &mut BookmarkCache {
        &mut *self.svc.bm_cache
    }

    #[inline]
    pub fn dir_cache_mut(&mut self) -> &mut DirCache {
        &mut *self.svc.dir_cache
    }

    /// Borrow the shared design tokens. Chrome widgets and any app
    /// that needs panel radii, margins, or chrome bar heights should
    /// thread this reference through rather than redeclaring constants.
    #[inline]
    pub fn theme(&self) -> &Theme {
        &self.svc.theme
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
    /// Zero when the SD card has no usable wall clock, in which case
    /// counters accumulate from boot without ever rolling over.
    #[inline]
    pub fn today_key(&self) -> u32 {
        self.svc.today_key
    }
}

/// See [`KernelHandle::largest_free_block`].
pub fn largest_free_block() -> usize {
    use core::alloc::Layout;

    let mut lo = 0usize;
    let mut hi = esp_alloc::HEAP.free();
    // probe allocations are transient and single-threaded (apps run on
    // the cooperative executor; ISRs never allocate), so alloc+dealloc
    // pairs cannot race another allocation mid-probe
    while hi.saturating_sub(lo) > 1024 {
        let mid = lo + (hi - lo).div_ceil(2);
        let Ok(layout) = Layout::from_size_align(mid, 4) else {
            break;
        };
        let ptr = unsafe { alloc::alloc::alloc(layout) };
        if ptr.is_null() {
            hi = mid - 1;
        } else {
            unsafe { alloc::alloc::dealloc(ptr, layout) };
            lo = mid;
        }
    }
    lo
}
