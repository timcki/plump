// sd card file operations
//
// all I/O through embedded-sdmmc AsyncVolumeManager; functions are
// synchronous, wrapping async ops with poll_once (SPI bus is blocking
// so every .await resolves immediately)
//
// returns the unified Error type (re-exported as StorageError for
// backward compat); apps receive it through KernelHandle

use core::cmp::Ordering;
use core::ops::ControlFlow;

use embedded_sdmmc::{Mode, RawFile};

use crate::drivers::sdcard::{SdStorage, SdStorageInner, poll_once};
use crate::error::{Error, ErrorKind};
use crate::kernel::daystats::DayKey;
use crate::util::FixedStr;

// TODO: rename _PULP to _PLUMP on-disk and drop legacy fallback
pub const PLUMP_DIR: &str = "_PLUMP";
pub const LEGACY_DIR: &str = "_PULP";
pub const TITLES_FILE: &str = "TITLES.BIN";
pub const TITLE_CAP: usize = 64;

/// Longest line `save_title` will write into TITLES.BIN, including the
/// tab and the trailing newline. The reader in `dir_cache` sizes its
/// carry buffer from this, so the two cannot drift apart.
pub const MAX_TITLE_LINE: usize = 128;

// backward-compatible alias
pub type StorageError = Error;

#[derive(Clone, Copy)]
pub struct DirEntry {
    pub name: FixedStr<13>,
    pub is_dir: bool,
    pub size: u32,
    pub title: FixedStr<TITLE_CAP>,
    // true when title is a humanized SFN fallback (not a real resolved title)
    pub title_humanized: bool,
}

impl DirEntry {
    pub const EMPTY: Self = Self {
        name: FixedStr::EMPTY,
        is_dir: false,
        size: 0,
        title: FixedStr::EMPTY,
        title_humanized: false,
    };

    pub fn name_str(&self) -> &str {
        self.name.as_str()
    }

    pub fn display_name(&self) -> &str {
        if !self.title.is_empty() {
            self.title.as_str()
        } else {
            self.name.as_str()
        }
    }

    fn has_real_title(&self) -> bool {
        !self.title.is_empty() && !self.title_humanized
    }

    pub fn set_title(&mut self, s: &[u8]) {
        self.title.set(s);
        self.title_humanized = false;
    }

    // write a humanized SFN into the title buffer as a soft fallback;
    // does not prevent the title scanner from resolving a real title
    pub fn humanize_sfn(&mut self) {
        if self.name.is_empty() || self.has_real_title() {
            return;
        }
        let src = self.name.as_bytes();
        let all_upper = src.iter().all(|&b| !b.is_ascii_lowercase());
        if !all_upper {
            return; // mixed case: user-supplied LFN, leave as-is
        }
        let n = src.len().min(TITLE_CAP);
        let buf = self.title.buf_mut();
        for i in 0..n {
            buf[i] = if i == 0 {
                src[i] // keep first char uppercase
            } else {
                src[i].to_ascii_lowercase()
            };
        }
        self.title.set_len(n as u8);
        self.title_humanized = true;
    }
}

// directories before files, then case-insensitive name order
impl PartialEq for DirEntry {
    fn eq(&self, other: &Self) -> bool {
        self.is_dir == other.is_dir && self.name == other.name
    }
}

impl Eq for DirEntry {}

impl PartialOrd for DirEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DirEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // directories sort before files
        match (self.is_dir, other.is_dir) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        // case-insensitive name comparison
        let an = self.name.as_bytes();
        let bn = other.name.as_bytes();
        for (a, b) in an.iter().zip(bn.iter()) {
            let ord = a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase());
            if ord != Ordering::Equal {
                return ord;
            }
        }
        an.len().cmp(&bn.len())
    }
}

pub struct DirPage {
    pub total: usize,
    pub count: usize,
}

fn ext_eq(name: &[u8], target: &[u8]) -> bool {
    let dot = match name.iter().rposition(|&b| b == b'.') {
        Some(p) => p,
        None => return false,
    };
    let ext = &name[dot + 1..];
    ext.len() == target.len() && ext.eq_ignore_ascii_case(target)
}

fn has_supported_ext(name: &[u8]) -> bool {
    ext_eq(name, b"TXT") || ext_eq(name, b"EPUB") || ext_eq(name, b"EPU") || ext_eq(name, b"MD")
}

/// Convert an embedded-sdmmc Timestamp into a coarse day-of-year-
/// since-1970 key. Strictly monotonic across calendar days; not a
/// real day count (uses 31 days/month, 372 days/year). Returns None
/// when the timestamp is stuck at the FAT epoch (1980-01-00) which
/// indicates the SD card has no battery-backed RTC.
fn timestamp_to_day_key(t: embedded_sdmmc::Timestamp) -> Option<DayKey> {
    // FAT epoch: year_since_1970 = 10, month / day both zero.
    if t.year_since_1970 <= 10 && t.zero_indexed_month == 0 && t.zero_indexed_day == 0 {
        return None;
    }
    DayKey::new(
        (t.year_since_1970 as u32) * 372
            + (t.zero_indexed_month as u32) * 31
            + (t.zero_indexed_day as u32),
    )
}

// build "NAME.EXT" bytes from a ShortFileName

/// How much a subdirectory holds, from one directory walk.
#[derive(Clone, Copy, Debug)]
pub struct SubdirUsage {
    pub files: u16,
    pub bytes: u32,
}

impl SubdirUsage {
    pub const EMPTY: Self = Self { files: 0, bytes: 0 };
}

/// Files unlinked in one [`SdStorage::purge_plump_subdir`] call.
#[derive(Clone, Copy, Debug)]
pub struct PurgeStep {
    pub deleted: u16,
    pub bytes: u32,
    /// entries were still there when the batch filled up
    pub more: bool,
}

/// Files unlinked per purge call. Each unlink walks the directory, so
/// this is the trade between clearing promptly and holding the event
/// loop; 8 keeps a batch near the cost of one page turn.
pub const PURGE_BATCH: usize = 8;

/// Directory entries that are not files we can unlink: the volume
/// label, subdirectories (`.` and `..` among them), long-name shards.
fn skip_entry(entry: &embedded_sdmmc::DirEntry) -> bool {
    entry.attributes.is_volume() || entry.attributes.is_directory() || entry.attributes.is_lfn()
}

fn sfn_to_bytes(name: &embedded_sdmmc::ShortFileName, out: &mut [u8; 13]) -> u8 {
    let base = name.base_name();
    let ext = name.extension();
    let mut pos = 0usize;
    let blen = base.len().min(8);
    out[..blen].copy_from_slice(&base[..blen]);
    pos += blen;
    if !ext.is_empty() {
        out[pos] = b'.';
        pos += 1;
        let elen = ext.len().min(3);
        out[pos..pos + elen].copy_from_slice(&ext[..elen]);
        pos += elen;
    }
    pos as u8
}

// async file operations on SdStorageInner — replaces the old op_* macros.
// none use ? internally so caller cleanup is never bypassed.

use embedded_sdmmc::RawDirectory;

impl SdStorageInner {
    async fn file_size(&mut self, dir: RawDirectory, name: &str) -> crate::error::Result<u32> {
        self.mgr
            .find_directory_entry(dir, name)
            .await
            .map(|e| e.size)
            .map_err(|_| Error::new(ErrorKind::OpenFile, "file_size"))
    }

    async fn file_mtime(
        &mut self,
        dir: RawDirectory,
        name: &str,
    ) -> crate::error::Result<embedded_sdmmc::Timestamp> {
        self.mgr
            .find_directory_entry(dir, name)
            .await
            .map(|e| e.mtime)
            .map_err(|_| Error::new(ErrorKind::OpenFile, "file_mtime"))
    }

    async fn read_chunk(
        &mut self,
        dir: RawDirectory,
        name: &str,
        offset: u32,
        buf: &mut [u8],
    ) -> crate::error::Result<usize> {
        let file = self
            .mgr
            .open_file_in_dir(dir, name, Mode::ReadOnly)
            .await
            .map_err(|_| Error::new(ErrorKind::OpenFile, "read_chunk"))?;

        let result = match self.mgr.file_seek_from_start(file, offset) {
            Ok(()) => self
                .mgr
                .read(file, buf)
                .await
                .map_err(|_| Error::new(ErrorKind::ReadFailed, "read_chunk")),
            Err(_) => Err(Error::new(ErrorKind::SeekFailed, "read_chunk")),
        };
        let _ = self.mgr.close_file(file).await;
        if let Ok(n) = &result {
            crate::perf::counters::inc_sd_reads();
            crate::perf::counters::add_sd_bytes_read(*n as u32);
        }
        result
    }

    async fn read_start(
        &mut self,
        dir: RawDirectory,
        name: &str,
        buf: &mut [u8],
    ) -> crate::error::Result<(u32, usize)> {
        let file = self
            .mgr
            .open_file_in_dir(dir, name, Mode::ReadOnly)
            .await
            .map_err(|_| Error::new(ErrorKind::OpenFile, "read_start"))?;

        let size = self.mgr.file_length(file).unwrap_or(0);
        let result = self
            .mgr
            .read(file, buf)
            .await
            .map_err(|_| Error::new(ErrorKind::ReadFailed, "read_start"));
        let _ = self.mgr.close_file(file).await;
        result.map(|n| {
            crate::perf::counters::inc_sd_reads();
            crate::perf::counters::add_sd_bytes_read(n as u32);
            (size, n)
        })
    }

    async fn write_file(
        &mut self,
        dir: RawDirectory,
        name: &str,
        data: &[u8],
    ) -> crate::error::Result<()> {
        let file = self
            .mgr
            .open_file_in_dir(dir, name, Mode::ReadWriteCreateOrTruncate)
            .await
            .map_err(|_| Error::new(ErrorKind::OpenFile, "write"))?;

        let result = if data.is_empty() {
            Ok(())
        } else {
            self.mgr
                .write(file, data)
                .await
                .map_err(|_| Error::new(ErrorKind::WriteFailed, "write"))
        };
        let _ = self.mgr.close_file(file).await;
        if result.is_ok() {
            crate::perf::counters::inc_sd_writes();
            crate::perf::counters::add_sd_bytes_written(data.len() as u32);
        }
        result
    }

    async fn append(
        &mut self,
        dir: RawDirectory,
        name: &str,
        data: &[u8],
    ) -> crate::error::Result<()> {
        let file = self
            .mgr
            .open_file_in_dir(dir, name, Mode::ReadWriteCreateOrAppend)
            .await
            .map_err(|_| Error::new(ErrorKind::OpenFile, "append"))?;

        let result = if data.is_empty() {
            Ok(())
        } else {
            self.mgr
                .write(file, data)
                .await
                .map_err(|_| Error::new(ErrorKind::WriteFailed, "append"))
        };
        let _ = self.mgr.close_file(file).await;
        if result.is_ok() {
            crate::perf::counters::inc_sd_writes();
            crate::perf::counters::add_sd_bytes_written(data.len() as u32);
        }
        result
    }

    // seek-then-write, shared by the data-dir and data-subdir entry
    // points; `tag` is their distinct error source string
    async fn write_at(
        &mut self,
        dir: RawDirectory,
        name: &str,
        offset: u32,
        data: &[u8],
        tag: &'static str,
    ) -> crate::error::Result<()> {
        let file = match self
            .mgr
            .open_file_in_dir(dir, name, Mode::ReadWriteCreateOrAppend)
            .await
        {
            Ok(f) => f,
            Err(_) => return Err(Error::new(ErrorKind::OpenFile, tag)),
        };
        let result = match self.mgr.file_seek_from_start(file, offset) {
            Ok(()) => self
                .mgr
                .write(file, data)
                .await
                .map_err(|_| Error::new(ErrorKind::WriteFailed, tag)),
            Err(_) => Err(Error::new(ErrorKind::SeekFailed, tag)),
        };
        let _ = self.mgr.close_file(file).await;
        if result.is_ok() {
            crate::perf::counters::inc_sd_writes();
            crate::perf::counters::add_sd_bytes_written(data.len() as u32);
        }
        result
    }

    async fn delete(&mut self, dir: RawDirectory, name: &str) -> crate::error::Result<()> {
        self.mgr
            .delete_entry_in_dir(dir, name)
            .await
            .map_err(|_| Error::new(ErrorKind::DeleteFailed, "delete"))
    }

    async fn open_dir(&mut self, name: &str) -> crate::error::Result<RawDirectory> {
        self.mgr
            .open_dir(self.root, name)
            .await
            .map_err(|_| Error::new(ErrorKind::OpenDir, "open_dir"))
    }

    /// The data directory, opened once and kept for the device
    /// lifetime.
    async fn data_handle(&mut self) -> crate::error::Result<RawDirectory> {
        if let Some(dir) = self.data_handle {
            return Ok(dir);
        }
        let dir = self.open_dir(self.data_dir).await?;
        self.data_handle = Some(dir);
        Ok(dir)
    }

    /// A subdirectory of the data dir. The bool says whether the
    /// handle is cached; an uncached one (table full, name too long)
    /// belongs to the caller, who closes it after use.
    async fn sub_handle(&mut self, name: &str) -> crate::error::Result<(RawDirectory, bool)> {
        for (cached, dir) in self.sub_handles.iter().flatten() {
            if cached.as_str() == name {
                return Ok((*dir, true));
            }
        }
        let parent = self.data_handle().await?;
        let dir = self
            .mgr
            .open_dir(parent, name)
            .await
            .map_err(|_| Error::new(ErrorKind::OpenDir, "sub_handle"))?;
        if name.len() <= 12
            && let Some(free) = self.sub_handles.iter_mut().find(|s| s.is_none())
        {
            *free = Some((FixedStr::from_bytes(name.as_bytes()), dir));
            return Ok((dir, true));
        }
        Ok((dir, false))
    }

    /// Close and forget one cached subdirectory handle, if it is
    /// cached. Removing a directory goes through here first: FAT will
    /// not unlink an entry that is still open.
    pub(crate) fn close_sub_handle(&mut self, name: &str) {
        for slot in self.sub_handles.iter_mut() {
            let Some((cached, dir)) = *slot else { continue };
            if cached.as_str() == name {
                *slot = None;
                let _ = self.mgr.close_dir(dir);
                return;
            }
        }
    }

    /// Close every cached directory handle (data dir change, halt).
    pub(crate) fn drop_dir_handles(&mut self) {
        if let Some(dir) = self.data_handle.take() {
            let _ = self.mgr.close_dir(dir);
        }
        for slot in self.sub_handles.iter_mut() {
            if let Some((_, dir)) = slot.take() {
                let _ = self.mgr.close_dir(dir);
            }
        }
    }
}

/// Read-only handle to an open file, valid inside
/// [`SdStorage::with_file_in_plump_subdir`]. Each `read_at` is one seek
/// plus one read on the already-open file, so a caller that needs
/// several pieces of one file pays the directory lookup once.
pub struct FileReader<'a> {
    inner: &'a mut SdStorageInner,
    raw: RawFile,
}

impl FileReader<'_> {
    /// File size in bytes.
    pub fn len(&self) -> u32 {
        self.inner.mgr.file_length(self.raw).unwrap_or(0)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read `buf.len()` bytes at `offset`; returns the count actually
    /// read (short at end of file).
    pub fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> crate::error::Result<usize> {
        self.inner
            .mgr
            .file_seek_from_start(self.raw, offset)
            .map_err(|_| Error::new(ErrorKind::SeekFailed, "FileReader::read_at"))?;
        let n = poll_once(self.inner.mgr.read(self.raw, buf))
            .map_err(|_| Error::new(ErrorKind::ReadFailed, "FileReader::read_at"))?;
        crate::perf::counters::inc_sd_reads();
        crate::perf::counters::add_sd_bytes_read(n as u32);
        Ok(n)
    }
}

fn borrow(sd: &SdStorage) -> core::result::Result<core::cell::RefMut<'_, SdStorageInner>, Error> {
    sd.borrow_inner()
        .ok_or(Error::new(ErrorKind::NoCard, "storage::borrow"))
}

/// Which directory a file operation resolves against.
///
/// Each scope names a chain of directories to open; [`SdStorage::with_scope`]
/// opens the chain, runs the operation, and closes what it opened on
/// every exit path.
#[derive(Clone, Copy)]
enum Scope<'a> {
    /// The volume root, held open for the device lifetime.
    Root,
    /// A named directory directly under the root.
    Named(&'a str),
    /// The resolved data directory (`_PLUMP`, or legacy `_PULP`).
    Data,
    /// A subdirectory of the data directory.
    DataSub(&'a str),
}

// streaming file handle: keeps one file open across multiple writes.
//
// the handle borrows the storage it came from, so it cannot outlive
// it, and closes the file when dropped, so no path leaves the volume
// manager holding an open handle it will never see again

/// Handle to an open file on the SD card.
///
/// Created via [`SdStorage::create_file`]. Closing is automatic on
/// drop; call [`close()`](FileWriter::close) instead when the close
/// error matters (it is the only way to observe it).
#[must_use = "a FileWriter closes its file when dropped; bind it to write"]
pub struct FileWriter<'sd> {
    sd: &'sd SdStorage,
    // None only between close() taking it and self being dropped
    raw: Option<RawFile>,
}

impl FileWriter<'_> {
    /// Write a chunk of data to the open file.
    pub fn write(&self, data: &[u8]) -> crate::error::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let raw = self.raw.expect("FileWriter::write after close");
        poll_once(async {
            let mut guard = borrow(self.sd)?;
            guard
                .mgr
                .write(raw, data)
                .await
                .map_err(|_| Error::new(ErrorKind::WriteFailed, "OpenFile::write"))?;
            crate::perf::counters::inc_sd_writes();
            crate::perf::counters::add_sd_bytes_written(data.len() as u32);
            Ok(())
        })
    }

    /// Close the file, flushing metadata to SD, and report the result.
    ///
    /// Dropping does the same close but discards the error, so callers
    /// that must know the data landed close explicitly.
    pub fn close(mut self) -> crate::error::Result<()> {
        // take() first: Drop must not close a second time
        let raw = self.raw.take().expect("FileWriter::close called twice");
        Self::close_raw(self.sd, raw)
    }

    fn close_raw(sd: &SdStorage, raw: RawFile) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(sd)?;
            guard
                .mgr
                .close_file(raw)
                .await
                .map_err(|_| Error::new(ErrorKind::WriteFailed, "OpenFile::close"))
        })
    }
}

impl Drop for FileWriter<'_> {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take()
            && let Err(e) = Self::close_raw(self.sd, raw)
        {
            log::error!("FileWriter: close on drop failed: {}", e);
        }
    }
}

impl SdStorage {
    /// Open `scope`'s directory chain, run `op` against the innermost
    /// directory, then close every directory this call opened.
    ///
    /// The chain is closed on all three exits: `op` succeeding, `op`
    /// failing, and the chain itself failing to open part-way. `op` is
    /// an async closure so it can borrow the volume manager across its
    /// own awaits; monomorphised per call site, so the abstraction
    /// costs nothing over the hand-written open/run/close.
    async fn with_scope<F, T>(&self, scope: Scope<'_>, op: F) -> crate::error::Result<T>
    where
        F: AsyncFnOnce(&mut SdStorageInner, RawDirectory) -> crate::error::Result<T>,
    {
        let mut guard = borrow(self)?;
        let inner = &mut *guard;

        // root, the data dir and its cached subdirs stay open for the
        // device lifetime, so only a directory opened here is closed
        // here
        let (dir, close_dir) = match scope {
            Scope::Root => (inner.root, false),
            Scope::Named(d) => (inner.open_dir(d).await?, true),
            Scope::Data => (inner.data_handle().await?, false),
            Scope::DataSub(sub) => {
                let (dir, cached) = inner.sub_handle(sub).await?;
                (dir, !cached)
            }
        };

        let result = op(inner, dir).await;

        if close_dir {
            let _ = inner.mgr.close_dir(dir);
        }
        result
    }

    /// Create (or truncate) a file in the root directory and return
    /// a [`FileWriter`] handle for streaming writes.
    pub fn create_file(&self, name: &str) -> crate::error::Result<FileWriter<'_>> {
        let raw = poll_once(self.with_scope(Scope::Root, async |inner, dir| {
            inner
                .mgr
                .open_file_in_dir(dir, name, Mode::ReadWriteCreateOrTruncate)
                .await
                .map_err(|_| Error::new(ErrorKind::OpenFile, "SdStorage::create_file"))
        }))?;
        Ok(FileWriter {
            sd: self,
            raw: Some(raw),
        })
    }

    /// Write an entire file atomically (create/truncate + write + close).
    pub fn write_file(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::Root, async |inner, dir| {
            inner.write_file(dir, name, data).await
        }))
    }

    /// Delete a file from the root directory.
    pub fn delete_file(&self, name: &str) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::Root, async |inner, dir| {
            inner.delete(dir, name).await
        }))
    }

    /// List supported files in the root directory.
    pub fn list_root_files(&self, buf: &mut [DirEntry]) -> crate::error::Result<usize> {
        poll_once(self.with_scope(Scope::Root, async |inner, dir| {
            let mut count = 0usize;
            let mut total = 0usize;

            inner
                .mgr
                .iterate_dir(dir, |entry| {
                    if entry.attributes.is_volume() || entry.attributes.is_directory() {
                        return ControlFlow::Continue(());
                    }

                    let mut name_buf = [0u8; 13];
                    let name_len = sfn_to_bytes(&entry.name, &mut name_buf);
                    let sfn = &name_buf[..name_len as usize];

                    if sfn.is_empty() || sfn[0] == b'.' || sfn[0] == b'_' {
                        return ControlFlow::Continue(());
                    }
                    if !has_supported_ext(sfn) {
                        return ControlFlow::Continue(());
                    }

                    total += 1;

                    if count < buf.len() {
                        buf[count] = DirEntry {
                            name: FixedStr::from_raw(name_buf, name_len),
                            is_dir: false,
                            size: entry.size,
                            title: FixedStr::EMPTY,
                            title_humanized: false,
                        };
                        count += 1;
                    }
                    ControlFlow::Continue(())
                })
                .await
                .map_err(|_| Error::new(ErrorKind::ReadFailed, "list_root_files"))?;

            if total > count {
                log::warn!(
                    "dir: {} supported files on SD, only {} fit in buffer (max {})",
                    total,
                    count,
                    buf.len(),
                );
            }
            Ok(count)
        }))
    }

    // root file reads

    /// Get the size of a file in the root directory.
    pub fn file_size(&self, name: &str) -> crate::error::Result<u32> {
        poll_once(self.with_scope(Scope::Root, async |inner, dir| {
            inner.file_size(dir, name).await
        }))
    }

    /// Read a chunk from a file in the root directory at the given offset.
    pub fn read_file_chunk(
        &self,
        name: &str,
        offset: u32,
        buf: &mut [u8],
    ) -> crate::error::Result<usize> {
        poll_once(self.with_scope(Scope::Root, async |inner, dir| {
            inner.read_chunk(dir, name, offset, buf).await
        }))
    }

    /// Read from the start of a file in the root directory.
    /// Returns (file_size, bytes_read).
    pub fn read_file_start(
        &self,
        name: &str,
        buf: &mut [u8],
    ) -> crate::error::Result<(u32, usize)> {
        poll_once(self.with_scope(Scope::Root, async |inner, dir| {
            inner.read_start(dir, name, buf).await
        }))
    }

    // named-directory file operations

    /// Write a file in a named subdirectory of root.
    pub fn write_file_in_dir(
        &self,
        dir: &str,
        name: &str,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::Named(dir), async |inner, dir_h| {
            inner.write_file(dir_h, name, data).await
        }))
    }

    /// Read from the start of a file in a named subdirectory of root.
    /// Returns (file_size, bytes_read).
    pub fn read_file_start_in_dir(
        &self,
        dir: &str,
        name: &str,
        buf: &mut [u8],
    ) -> crate::error::Result<(u32, usize)> {
        poll_once(self.with_scope(Scope::Named(dir), async |inner, dir_h| {
            inner.read_start(dir_h, name, buf).await
        }))
    }

    // _PLUMP/ directory management

    /// Ensure the data directory exists (async, for boot path).
    ///
    /// Probes for `_PLUMP` first; if absent, falls back to legacy `_PULP`
    /// so existing SD cards keep working. Creates `_PLUMP` only when
    /// neither directory is found.
    // TODO: rename _PULP to _PLUMP on-disk and drop legacy fallback
    pub async fn ensure_plump_dir_async(&self) -> crate::error::Result<()> {
        let mut guard = borrow(self)?;
        let inner = &mut *guard;
        inner.drop_dir_handles();

        // Try the current name first.
        if let Ok(dir) = inner.mgr.open_dir(inner.root, PLUMP_DIR).await {
            let _ = inner.mgr.close_dir(dir);
            inner.data_dir = PLUMP_DIR;
            return Ok(());
        }

        // Fall back to the legacy directory if it exists.
        if let Ok(dir) = inner.mgr.open_dir(inner.root, LEGACY_DIR).await {
            let _ = inner.mgr.close_dir(dir);
            inner.data_dir = LEGACY_DIR;
            log::info!("data dir: using legacy {}", LEGACY_DIR);
            return Ok(());
        }

        // Neither exists — create the new one.
        inner.data_dir = PLUMP_DIR;
        match inner.mgr.make_dir_in_dir(inner.root, PLUMP_DIR).await {
            Ok(()) => Ok(()),
            Err(embedded_sdmmc::Error::DirAlreadyExists) => Ok(()),
            Err(_) => Err(Error::new(ErrorKind::WriteFailed, "ensure_plump_dir_async")),
        }
    }

    /// Ensure a subdirectory exists under the data directory.
    pub fn ensure_plump_subdir(&self, name: &str) -> crate::error::Result<()> {
        let exists = poll_once(self.with_scope(Scope::Data, async |inner, dir| {
            match inner.mgr.open_dir(dir, name).await {
                Ok(sub) => {
                    let _ = inner.mgr.close_dir(sub);
                    Ok(true)
                }
                Err(_) => Ok(false),
            }
        }))?;

        if exists {
            return Ok(());
        }

        poll_once(self.with_scope(Scope::Data, async |inner, dir| {
            match inner.mgr.make_dir_in_dir(dir, name).await {
                Ok(()) => Ok(()),
                Err(embedded_sdmmc::Error::DirAlreadyExists) => Ok(()),
                Err(_) => Err(Error::new(ErrorKind::WriteFailed, "ensure_plump_subdir")),
            }
        }))
    }

    // data dir direct file operations (cache files live directly in the data dir)

    /// Read a chunk from a file in the data directory.
    pub fn read_chunk_in_plump(
        &self,
        name: &str,
        offset: u32,
        buf: &mut [u8],
    ) -> crate::error::Result<usize> {
        poll_once(self.with_scope(Scope::Data, async |inner, dir| {
            inner.read_chunk(dir, name, offset, buf).await
        }))
    }

    /// Write (create/truncate) a file in the data directory.
    pub fn write_in_plump(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::Data, async |inner, dir| {
            inner.write_file(dir, name, data).await
        }))
    }

    /// Append data to a file in the data directory.
    pub fn append_in_plump(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::Data, async |inner, dir| {
            inner.append(dir, name, data).await
        }))
    }

    /// Day-of-year-since-1970 key derived from a file's FAT mtime in
    /// `_PLUMP/`. Returns `None` when the file is missing, when mtime
    /// is unreadable, or when the timestamp is stuck at the FAT epoch
    /// (1980) — the case where the SD card has no battery-backed RTC.
    ///
    /// The key is monotonically increasing across calendar days so
    /// callers can compare two keys for inequality to detect a
    /// rollover without doing real calendar math.
    pub fn file_mtime_day_key_in_plump(&self, name: &str) -> Option<DayKey> {
        poll_once(self.with_scope(Scope::Data, async |inner, dir| {
            inner.file_mtime(dir, name).await
        }))
        .ok()
        .and_then(timestamp_to_day_key)
    }

    /// Delete a file in the data directory.
    pub fn delete_in_plump(&self, name: &str) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::Data, async |inner, dir| {
            inner.delete(dir, name).await
        }))
    }

    /// Seek to offset and write data in a file in the data directory.
    pub fn write_at_in_plump(
        &self,
        name: &str,
        offset: u32,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::Data, async |inner, dir| {
            inner.write_at(dir, name, offset, data, "write_at").await
        }))
    }

    // data dir subdirectory file operations

    /// Write (create/truncate) a file in <data_dir>/<dir>/.
    pub fn write_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::DataSub(dir), async |inner, sub| {
            inner.write_file(sub, name, data).await
        }))
    }

    /// Append data to a file in <data_dir>/<dir>/.
    pub fn append_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::DataSub(dir), async |inner, sub| {
            inner.append(sub, name, data).await
        }))
    }

    /// Read a chunk from a file in <data_dir>/<dir>/.
    pub fn read_chunk_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
        offset: u32,
        buf: &mut [u8],
    ) -> crate::error::Result<usize> {
        poll_once(self.with_scope(Scope::DataSub(dir), async |inner, sub| {
            inner.read_chunk(sub, name, offset, buf).await
        }))
    }

    /// Open a file in <data_dir>/<dir>/ once and run `f` against it;
    /// the file closes on every exit path. Use this over repeated
    /// `read_chunk_in_plump_subdir` calls when several reads target one
    /// file: each of those pays the directory lookup again.
    pub fn with_file_in_plump_subdir<T>(
        &self,
        dir: &str,
        name: &str,
        f: impl FnOnce(&mut FileReader<'_>) -> crate::error::Result<T>,
    ) -> crate::error::Result<T> {
        poll_once(
            self.with_scope(Scope::DataSub(dir), async move |inner, sub| {
                let raw = inner
                    .mgr
                    .open_file_in_dir(sub, name, Mode::ReadOnly)
                    .await
                    .map_err(|_| Error::new(ErrorKind::OpenFile, "with_file_in_plump_subdir"))?;
                let result = {
                    let mut reader = FileReader {
                        inner: &mut *inner,
                        raw,
                    };
                    f(&mut reader)
                };
                let _ = inner.mgr.close_file(raw).await;
                result
            }),
        )
    }

    /// Get the size of a file in <data_dir>/<dir>/.
    pub fn file_size_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
    ) -> crate::error::Result<u32> {
        poll_once(self.with_scope(Scope::DataSub(dir), async |inner, sub| {
            inner.file_size(sub, name).await
        }))
    }

    /// Seek to offset and write data in a file in <data_dir>/<dir>/.
    ///
    /// Creates the file if missing. Does not truncate trailing bytes
    /// past `offset + data.len()` — callers that need logical truncation
    /// should record the authoritative size elsewhere (e.g. a header
    /// field) and ignore bytes beyond it.
    pub fn write_at_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
        offset: u32,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::DataSub(dir), async |inner, sub| {
            inner
                .write_at(sub, name, offset, data, "write_at_sub")
                .await
        }))
    }

    /// Delete a file in <data_dir>/<dir>/.
    pub fn delete_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
    ) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::DataSub(dir), async |inner, sub| {
            inner.delete(sub, name).await
        }))
    }

    // whole-subdirectory operations (per-book cache clearing)

    /// Count the files in <data_dir>/<dir>/ and total their bytes.
    ///
    /// One directory walk, no file opens. A missing directory is an
    /// `OpenDir` error, which callers reading a cache size treat as
    /// zero rather than a failure.
    pub fn measure_plump_subdir(&self, dir: &str) -> crate::error::Result<SubdirUsage> {
        poll_once(self.with_scope(Scope::DataSub(dir), async |inner, sub| {
            let mut usage = SubdirUsage::EMPTY;
            inner
                .mgr
                .iterate_dir(sub, |entry| {
                    if !skip_entry(entry) {
                        usage.files = usage.files.saturating_add(1);
                        usage.bytes = usage.bytes.saturating_add(entry.size);
                    }
                    ControlFlow::Continue(())
                })
                .await
                .map_err(|_| Error::new(ErrorKind::ReadFailed, "measure_plump_subdir"))?;
            Ok(usage)
        }))
    }

    /// Delete up to [`PURGE_BATCH`] files from <data_dir>/<dir>/.
    ///
    /// Bounded so a directory with hundreds of cached images clears
    /// across several background steps instead of blocking the event
    /// loop. Call until `more` comes back false; the directory itself
    /// survives, so finish with [`Self::remove_plump_subdir`].
    ///
    /// The walk and the unlinks are separate passes: deleting entries
    /// while iterating the same directory would invalidate the walk.
    pub fn purge_plump_subdir(&self, dir: &str) -> crate::error::Result<PurgeStep> {
        poll_once(self.with_scope(Scope::DataSub(dir), async |inner, sub| {
            let mut names = [([0u8; 13], 0u8, 0u32); PURGE_BATCH];
            let mut found = 0usize;
            let mut more = false;

            inner
                .mgr
                .iterate_dir(sub, |entry| {
                    if skip_entry(entry) {
                        return ControlFlow::Continue(());
                    }
                    if found >= PURGE_BATCH {
                        more = true;
                        return ControlFlow::Break(());
                    }
                    let mut buf = [0u8; 13];
                    let len = sfn_to_bytes(&entry.name, &mut buf);
                    names[found] = (buf, len, entry.size);
                    found += 1;
                    ControlFlow::Continue(())
                })
                .await
                .map_err(|_| Error::new(ErrorKind::ReadFailed, "purge_plump_subdir"))?;

            let mut step = PurgeStep {
                deleted: 0,
                bytes: 0,
                more,
            };
            for (buf, len, size) in names.iter().take(found) {
                let name = match core::str::from_utf8(&buf[..*len as usize]) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                match inner.delete(sub, name).await {
                    Ok(()) => {
                        step.deleted = step.deleted.saturating_add(1);
                        step.bytes = step.bytes.saturating_add(*size);
                    }
                    // an entry we cannot unlink would otherwise be
                    // walked again forever; report no progress and let
                    // the caller stop
                    Err(e) => {
                        log::warn!("purge {}/{}: {}", dir, name, e);
                        step.more = false;
                        return Ok(step);
                    }
                }
            }
            Ok(step)
        }))
    }

    /// Remove the (empty) directory <data_dir>/<dir>/.
    ///
    /// FAT refuses to unlink a directory that is still open, and this
    /// one may well be holding one of the cached `sub_handles`, so the
    /// handle is closed before the entry goes.
    pub fn remove_plump_subdir(&self, dir: &str) -> crate::error::Result<()> {
        poll_once(self.with_scope(Scope::Data, async |inner, data| {
            inner.close_sub_handle(dir);
            inner.delete(data, dir).await
        }))
    }

    // title mapping

    /// Append a title line to TITLES.BIN in the data directory.
    pub fn save_title(&self, filename: &str, title: &str) -> crate::error::Result<()> {
        let name_bytes = filename.as_bytes();
        let title_bytes = title.as_bytes();
        let title_len = title_bytes.len().min(TITLE_CAP);
        let line_len = name_bytes.len() + 1 + title_len + 1;
        if line_len > MAX_TITLE_LINE {
            return Err(Error::new(
                ErrorKind::WriteFailed,
                "save_title: line too long",
            ));
        }
        let mut line = [0u8; MAX_TITLE_LINE];
        line[..name_bytes.len()].copy_from_slice(name_bytes);
        line[name_bytes.len()] = b'\t';
        line[name_bytes.len() + 1..name_bytes.len() + 1 + title_len]
            .copy_from_slice(&title_bytes[..title_len]);
        line[name_bytes.len() + 1 + title_len] = b'\n';

        self.append_in_plump(TITLES_FILE, &line[..line_len])
    }
}

