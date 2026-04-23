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
use crate::util::FixedStr;

// TODO: rename _PULP to _PLUMP on-disk and drop legacy fallback
pub const PLUMP_DIR: &str = "_PLUMP";
pub const LEGACY_DIR: &str = "_PULP";
pub const TITLES_FILE: &str = "TITLES.BIN";
pub const TITLE_CAP: usize = 64;

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

    pub fn has_real_title(&self) -> bool {
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

// build "NAME.EXT" bytes from a ShortFileName

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

    async fn open_subdir(
        &mut self,
        d1: &str,
        d2: &str,
    ) -> crate::error::Result<(RawDirectory, RawDirectory)> {
        let mid = self.open_dir(d1).await?;
        match self.mgr.open_dir(mid, d2).await {
            Ok(sub) => Ok((mid, sub)),
            Err(_) => {
                let _ = self.mgr.close_dir(mid);
                Err(Error::new(ErrorKind::OpenDir, "open_subdir"))
            }
        }
    }
}

fn borrow(sd: &SdStorage) -> core::result::Result<core::cell::RefMut<'_, SdStorageInner>, Error> {
    sd.borrow_inner()
        .ok_or(Error::new(ErrorKind::NoCard, "storage::borrow"))
}

// streaming file handle — keeps one file open across multiple writes
//
// must be closed via close(); dropping without closing leaks the
// handle in the volume manager (it will refuse to open the file again).
// debug builds panic on leak; release builds log an error.

/// Handle to an open file on the SD card.
///
/// Created via [`SdStorage::create_file`]. Must be consumed via
/// [`close()`](OpenFile::close) — dropping without closing leaks the
/// handle inside the volume manager.
pub struct OpenFile {
    raw: Option<RawFile>,
}

impl OpenFile {
    /// Write a chunk of data to the open file.
    pub fn write(&self, sd: &SdStorage, data: &[u8]) -> crate::error::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let raw = self.raw.expect("OpenFile::write after close");
        poll_once(async {
            let mut guard = borrow(sd)?;
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

    /// Close the file, flushing metadata to SD. Consumes self.
    pub fn close(mut self, sd: &SdStorage) -> crate::error::Result<()> {
        let raw = self.raw.take().expect("OpenFile::close called twice");
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

impl Drop for OpenFile {
    fn drop(&mut self) {
        if self.raw.is_some() {
            // file handle leaked — volume manager still thinks it is open
            log::error!("OpenFile dropped without close()! Handle leaked.");
            debug_assert!(false, "OpenFile dropped without close()");
        }
    }
}

impl SdStorage {
    /// Create (or truncate) a file in the root directory and return
    /// an [`OpenFile`] handle for streaming writes.
    pub fn create_file(&self, name: &str) -> crate::error::Result<OpenFile> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            let raw = inner
                .mgr
                .open_file_in_dir(inner.root, name, Mode::ReadWriteCreateOrTruncate)
                .await
                .map_err(|_| Error::new(ErrorKind::OpenFile, "SdStorage::create_file"))?;
            Ok(OpenFile { raw: Some(raw) })
        })
    }

    /// Write an entire file atomically (create/truncate + write + close).
    pub fn write_file(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let root = guard.root;
            guard.write_file(root, name, data).await
        })
    }

    /// Append data to an existing file (open + seek-to-end + write + close).
    pub fn append_root_file(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let root = guard.root;
            guard.append(root, name, data).await
        })
    }

    /// Delete a file from the root directory.
    pub fn delete_file(&self, name: &str) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let root = guard.root;
            guard.delete(root, name).await
        })
    }

    /// List supported files in the root directory.
    pub fn list_root_files(&self, buf: &mut [DirEntry]) -> crate::error::Result<usize> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;

            let mut count = 0usize;
            let mut total = 0usize;

            inner
                .mgr
                .iterate_dir(inner.root, |entry| {
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
        })
    }

    // root file reads

    /// Get the size of a file in the root directory.
    pub fn file_size(&self, name: &str) -> crate::error::Result<u32> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let root = guard.root;
            guard.file_size(root, name).await
        })
    }

    /// Read a chunk from a file in the root directory at the given offset.
    pub fn read_file_chunk(
        &self,
        name: &str,
        offset: u32,
        buf: &mut [u8],
    ) -> crate::error::Result<usize> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let root = guard.root;
            guard.read_chunk(root, name, offset, buf).await
        })
    }

    /// Read from the start of a file in the root directory.
    /// Returns (file_size, bytes_read).
    pub fn read_file_start(
        &self,
        name: &str,
        buf: &mut [u8],
    ) -> crate::error::Result<(u32, usize)> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let root = guard.root;
            guard.read_start(root, name, buf).await
        })
    }

    // named-directory file operations

    /// Write a file in a named subdirectory of root.
    pub fn write_file_in_dir(
        &self,
        dir: &str,
        name: &str,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dir_h = guard.open_dir(dir).await?;
            let r = guard.write_file(dir_h, name, data).await;
            let _ = guard.mgr.close_dir(dir_h);
            r
        })
    }

    /// Read from the start of a file in a named subdirectory of root.
    /// Returns (file_size, bytes_read).
    pub fn read_file_start_in_dir(
        &self,
        dir: &str,
        name: &str,
        buf: &mut [u8],
    ) -> crate::error::Result<(u32, usize)> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dir_h = guard.open_dir(dir).await?;
            let r = guard.read_start(dir_h, name, buf).await;
            let _ = guard.mgr.close_dir(dir_h);
            r
        })
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
        let exists = poll_once(async {
            let mut guard = borrow(self)?;
            let dir = guard.data_dir;
            let plump_h = guard.open_dir(dir).await?;
            let r: crate::error::Result<bool> = match guard.mgr.open_dir(plump_h, name).await {
                Ok(sub) => {
                    let _ = guard.mgr.close_dir(sub);
                    Ok(true)
                }
                Err(_) => Ok(false),
            };
            let _ = guard.mgr.close_dir(plump_h);
            r
        })?;

        if exists {
            return Ok(());
        }

        poll_once(async {
            let mut guard = borrow(self)?;
            let dir = guard.data_dir;
            let plump_h = guard.open_dir(dir).await?;
            let r = match guard.mgr.make_dir_in_dir(plump_h, name).await {
                Ok(()) => Ok(()),
                Err(embedded_sdmmc::Error::DirAlreadyExists) => Ok(()),
                Err(_) => Err(Error::new(ErrorKind::WriteFailed, "ensure_plump_subdir")),
            };
            let _ = guard.mgr.close_dir(plump_h);
            r
        })
    }

    // data dir direct file operations (cache files live directly in the data dir)

    /// Read a chunk from a file in the data directory.
    pub fn read_chunk_in_plump(
        &self,
        name: &str,
        offset: u32,
        buf: &mut [u8],
    ) -> crate::error::Result<usize> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dir = guard.data_dir;
            let dir_h = guard.open_dir(dir).await?;
            let r = guard.read_chunk(dir_h, name, offset, buf).await;
            let _ = guard.mgr.close_dir(dir_h);
            r
        })
    }

    /// Write (create/truncate) a file in the data directory.
    pub fn write_in_plump(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dir = guard.data_dir;
            let dir_h = guard.open_dir(dir).await?;
            let r = guard.write_file(dir_h, name, data).await;
            let _ = guard.mgr.close_dir(dir_h);
            r
        })
    }

    /// Append data to a file in the data directory.
    pub fn append_in_plump(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dir = guard.data_dir;
            let dir_h = guard.open_dir(dir).await?;
            let r = guard.append(dir_h, name, data).await;
            let _ = guard.mgr.close_dir(dir_h);
            r
        })
    }

    /// Get the size of a file in the data directory.
    pub fn file_size_in_plump(&self, name: &str) -> crate::error::Result<u32> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dir = guard.data_dir;
            let dir_h = guard.open_dir(dir).await?;
            let r = guard.file_size(dir_h, name).await;
            let _ = guard.mgr.close_dir(dir_h);
            r
        })
    }

    /// Delete a file in the data directory.
    pub fn delete_in_plump(&self, name: &str) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dir = guard.data_dir;
            let dir_h = guard.open_dir(dir).await?;
            let r = guard.delete(dir_h, name).await;
            let _ = guard.mgr.close_dir(dir_h);
            r
        })
    }

    /// Seek to offset and write data in a file in the data directory.
    pub fn write_at_in_plump(
        &self,
        name: &str,
        offset: u32,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dir = guard.data_dir;
            let dir_h = guard.open_dir(dir).await?;
            let file = match guard
                .mgr
                .open_file_in_dir(dir_h, name, Mode::ReadWriteCreateOrAppend)
                .await
            {
                Ok(f) => f,
                Err(_) => {
                    let _ = guard.mgr.close_dir(dir_h);
                    return Err(Error::new(ErrorKind::OpenFile, "write_at"));
                }
            };
            let result = match guard.mgr.file_seek_from_start(file, offset) {
                Ok(()) => guard
                    .mgr
                    .write(file, data)
                    .await
                    .map_err(|_| Error::new(ErrorKind::WriteFailed, "write_at")),
                Err(_) => Err(Error::new(ErrorKind::SeekFailed, "write_at")),
            };
            let _ = guard.mgr.close_file(file).await;
            let _ = guard.mgr.close_dir(dir_h);
            if result.is_ok() {
                crate::perf::counters::inc_sd_writes();
                crate::perf::counters::add_sd_bytes_written(data.len() as u32);
            }
            result
        })
    }

    // data dir subdirectory file operations

    /// Write (create/truncate) a file in <data_dir>/<dir>/.
    pub fn write_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dd = guard.data_dir;
            let (mid, sub) = guard.open_subdir(dd, dir).await?;
            let r = guard.write_file(sub, name, data).await;
            let _ = guard.mgr.close_dir(sub);
            let _ = guard.mgr.close_dir(mid);
            r
        })
    }

    /// Append data to a file in <data_dir>/<dir>/.
    pub fn append_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dd = guard.data_dir;
            let (mid, sub) = guard.open_subdir(dd, dir).await?;
            let r = guard.append(sub, name, data).await;
            let _ = guard.mgr.close_dir(sub);
            let _ = guard.mgr.close_dir(mid);
            r
        })
    }

    /// Read a chunk from a file in <data_dir>/<dir>/.
    pub fn read_chunk_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
        offset: u32,
        buf: &mut [u8],
    ) -> crate::error::Result<usize> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dd = guard.data_dir;
            let (mid, sub) = guard.open_subdir(dd, dir).await?;
            let r = guard.read_chunk(sub, name, offset, buf).await;
            let _ = guard.mgr.close_dir(sub);
            let _ = guard.mgr.close_dir(mid);
            r
        })
    }

    /// Get the size of a file in <data_dir>/<dir>/.
    pub fn file_size_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
    ) -> crate::error::Result<u32> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dd = guard.data_dir;
            let (mid, sub) = guard.open_subdir(dd, dir).await?;
            let r = guard.file_size(sub, name).await;
            let _ = guard.mgr.close_dir(sub);
            let _ = guard.mgr.close_dir(mid);
            r
        })
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
        poll_once(async {
            let mut guard = borrow(self)?;
            let dd = guard.data_dir;
            let (mid, sub) = guard.open_subdir(dd, dir).await?;
            let file = match guard
                .mgr
                .open_file_in_dir(sub, name, Mode::ReadWriteCreateOrAppend)
                .await
            {
                Ok(f) => f,
                Err(_) => {
                    let _ = guard.mgr.close_dir(sub);
                    let _ = guard.mgr.close_dir(mid);
                    return Err(Error::new(ErrorKind::OpenFile, "write_at_sub"));
                }
            };
            let result = match guard.mgr.file_seek_from_start(file, offset) {
                Ok(()) => guard
                    .mgr
                    .write(file, data)
                    .await
                    .map_err(|_| Error::new(ErrorKind::WriteFailed, "write_at_sub")),
                Err(_) => Err(Error::new(ErrorKind::SeekFailed, "write_at_sub")),
            };
            let _ = guard.mgr.close_file(file).await;
            let _ = guard.mgr.close_dir(sub);
            let _ = guard.mgr.close_dir(mid);
            if result.is_ok() {
                crate::perf::counters::inc_sd_writes();
                crate::perf::counters::add_sd_bytes_written(data.len() as u32);
            }
            result
        })
    }

    /// Delete a file in <data_dir>/<dir>/.
    pub fn delete_in_plump_subdir(
        &self,
        dir: &str,
        name: &str,
    ) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let dd = guard.data_dir;
            let (mid, sub) = guard.open_subdir(dd, dir).await?;
            let r = guard.delete(sub, name).await;
            let _ = guard.mgr.close_dir(sub);
            let _ = guard.mgr.close_dir(mid);
            r
        })
    }

    // title mapping

    /// Append a title line to TITLES.BIN in the data directory.
    pub fn save_title(&self, filename: &str, title: &str) -> crate::error::Result<()> {
        let name_bytes = filename.as_bytes();
        let title_bytes = title.as_bytes();
        let title_len = title_bytes.len().min(TITLE_CAP);
        let line_len = name_bytes.len() + 1 + title_len + 1;
        if line_len > 128 {
            return Err(Error::new(
                ErrorKind::WriteFailed,
                "save_title: line too long",
            ));
        }
        let mut line = [0u8; 128];
        line[..name_bytes.len()].copy_from_slice(name_bytes);
        line[name_bytes.len()] = b'\t';
        line[name_bytes.len() + 1..name_bytes.len() + 1 + title_len]
            .copy_from_slice(&title_bytes[..title_len]);
        line[name_bytes.len() + 1 + title_len] = b'\n';

        self.append_in_plump(TITLES_FILE, &line[..line_len])
    }
}

