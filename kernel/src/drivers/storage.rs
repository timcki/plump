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

// file-operation macros; each evaluates to Result<T, Error>
// none use ? internally so caller cleanup is never bypassed

macro_rules! op_file_size {
    ($inner:expr, $dir:expr, $name:expr) => {
        $inner
            .mgr
            .find_directory_entry($dir, $name)
            .await
            .map(|e| e.size)
            .map_err(|_| Error::new(ErrorKind::OpenFile, "file_size"))
    };
}

macro_rules! op_read_chunk {
    ($inner:expr, $dir:expr, $name:expr, $offset:expr, $buf:expr) => {
        match $inner
            .mgr
            .open_file_in_dir($dir, $name, Mode::ReadOnly)
            .await
        {
            Err(_) => Err(Error::new(ErrorKind::OpenFile, "read_chunk")),
            Ok(file) => {
                let result = match $inner.mgr.file_seek_from_start(file, $offset) {
                    Ok(()) => $inner
                        .mgr
                        .read(file, $buf)
                        .await
                        .map_err(|_| Error::new(ErrorKind::ReadFailed, "read_chunk")),
                    Err(_) => Err(Error::new(ErrorKind::SeekFailed, "read_chunk")),
                };
                let _ = $inner.mgr.close_file(file).await;
                if let Ok(n) = &result {
                    $crate::perf::counters::inc_sd_reads();
                    $crate::perf::counters::add_sd_bytes_read(*n as u32);
                }
                result
            }
        }
    };
}

macro_rules! op_read_start {
    ($inner:expr, $dir:expr, $name:expr, $buf:expr) => {
        match $inner
            .mgr
            .open_file_in_dir($dir, $name, Mode::ReadOnly)
            .await
        {
            Err(_) => Err(Error::new(ErrorKind::OpenFile, "read_start")),
            Ok(file) => {
                let size = $inner.mgr.file_length(file).unwrap_or(0);
                let result = $inner
                    .mgr
                    .read(file, $buf)
                    .await
                    .map_err(|_| Error::new(ErrorKind::ReadFailed, "read_start"));
                let _ = $inner.mgr.close_file(file).await;
                let mapped = result.map(|n| {
                    $crate::perf::counters::inc_sd_reads();
                    $crate::perf::counters::add_sd_bytes_read(n as u32);
                    (size, n)
                });
                mapped
            }
        }
    };
}

macro_rules! op_write {
    ($inner:expr, $dir:expr, $name:expr, $data:expr) => {
        match $inner
            .mgr
            .open_file_in_dir($dir, $name, Mode::ReadWriteCreateOrTruncate)
            .await
        {
            Err(_) => Err(Error::new(ErrorKind::OpenFile, "write")),
            Ok(file) => {
                let data_ref = $data;
                let result = if data_ref.is_empty() {
                    Ok(())
                } else {
                    $inner
                        .mgr
                        .write(file, data_ref)
                        .await
                        .map_err(|_| Error::new(ErrorKind::WriteFailed, "write"))
                };
                let _ = $inner.mgr.close_file(file).await;
                if result.is_ok() {
                    $crate::perf::counters::inc_sd_writes();
                    $crate::perf::counters::add_sd_bytes_written(data_ref.len() as u32);
                }
                result
            }
        }
    };
}

macro_rules! op_append {
    ($inner:expr, $dir:expr, $name:expr, $data:expr) => {
        match $inner
            .mgr
            .open_file_in_dir($dir, $name, Mode::ReadWriteCreateOrAppend)
            .await
        {
            Err(_) => Err(Error::new(ErrorKind::OpenFile, "append")),
            Ok(file) => {
                let data_ref = $data;
                let result = if data_ref.is_empty() {
                    Ok(())
                } else {
                    $inner
                        .mgr
                        .write(file, data_ref)
                        .await
                        .map_err(|_| Error::new(ErrorKind::WriteFailed, "append"))
                };
                let _ = $inner.mgr.close_file(file).await;
                if result.is_ok() {
                    $crate::perf::counters::inc_sd_writes();
                    $crate::perf::counters::add_sd_bytes_written(data_ref.len() as u32);
                }
                result
            }
        }
    };
}

macro_rules! op_delete {
    ($inner:expr, $dir:expr, $name:expr) => {{
        $inner
            .mgr
            .delete_entry_in_dir($dir, $name)
            .await
            .map_err(|_| Error::new(ErrorKind::DeleteFailed, "delete"))
    }};
}

// dir-scoping macros; open subdir, execute body, close handle

macro_rules! in_dir {
    ($inner:expr, $dirname:expr, |$dir:ident| $body:expr) => {
        match $inner.mgr.open_dir($inner.root, $dirname).await {
            Err(_) => Err(Error::new(ErrorKind::OpenDir, "in_dir")),
            Ok($dir) => {
                let _r = $body;
                let _ = $inner.mgr.close_dir($dir);
                _r
            }
        }
    };
}

macro_rules! in_subdir {
    ($inner:expr, $d1:expr, $d2:expr, |$dir:ident| $body:expr) => {
        match $inner.mgr.open_dir($inner.root, $d1).await {
            Err(_) => Err(Error::new(ErrorKind::OpenDir, "in_subdir")),
            Ok(_mid) => match $inner.mgr.open_dir(_mid, $d2).await {
                Err(_) => {
                    let _ = $inner.mgr.close_dir(_mid);
                    Err(Error::new(ErrorKind::OpenDir, "in_subdir"))
                }
                Ok($dir) => {
                    let _r = $body;
                    let _ = $inner.mgr.close_dir($dir);
                    let _ = $inner.mgr.close_dir(_mid);
                    _r
                }
            },
        }
    };
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
            let inner = &mut *guard;
            op_write!(inner, inner.root, name, data)
        })
    }

    /// Append data to an existing file (open + seek-to-end + write + close).
    pub fn append_root_file(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            op_append!(inner, inner.root, name, data)
        })
    }

    /// Delete a file from the root directory.
    pub fn delete_file(&self, name: &str) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            op_delete!(inner, inner.root, name)
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
            let inner = &mut *guard;
            op_file_size!(inner, inner.root, name)
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
            let inner = &mut *guard;
            op_read_chunk!(inner, inner.root, name, offset, buf)
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
            let inner = &mut *guard;
            op_read_start!(inner, inner.root, name, buf)
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
            let inner = &mut *guard;
            in_dir!(inner, dir, |dir_h| op_write!(inner, dir_h, name, data))
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
            let inner = &mut *guard;
            in_dir!(inner, dir, |dir_h| op_read_start!(inner, dir_h, name, buf))
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
            let inner = &mut *guard;
            let dir = inner.data_dir;
            in_dir!(inner, dir, |plump_h| {
                match inner.mgr.open_dir(plump_h, name).await {
                    Ok(sub) => {
                        let _ = inner.mgr.close_dir(sub);
                        Ok::<_, Error>(true)
                    }
                    Err(_) => Ok(false),
                }
            })
        })?;

        if exists {
            return Ok(());
        }

        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            let dir = inner.data_dir;
            in_dir!(inner, dir, |plump_h| {
                match inner.mgr.make_dir_in_dir(plump_h, name).await {
                    Ok(()) => Ok::<_, Error>(()),
                    Err(embedded_sdmmc::Error::DirAlreadyExists) => Ok(()),
                    Err(_) => Err(Error::new(ErrorKind::WriteFailed, "ensure_plump_subdir")),
                }
            })
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
            let inner = &mut *guard;
            let dir = inner.data_dir;
            in_dir!(inner, dir, |dir_h| op_read_chunk!(
                inner, dir_h, name, offset, buf
            ))
        })
    }

    /// Write (create/truncate) a file in the data directory.
    pub fn write_in_plump(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            let dir = inner.data_dir;
            in_dir!(inner, dir, |dir_h| op_write!(inner, dir_h, name, data))
        })
    }

    /// Append data to a file in the data directory.
    pub fn append_in_plump(&self, name: &str, data: &[u8]) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            let dir = inner.data_dir;
            in_dir!(inner, dir, |dir_h| op_append!(
                inner, dir_h, name, data
            ))
        })
    }

    /// Get the size of a file in the data directory.
    pub fn file_size_in_plump(&self, name: &str) -> crate::error::Result<u32> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            let dir = inner.data_dir;
            in_dir!(inner, dir, |dir_h| op_file_size!(inner, dir_h, name))
        })
    }

    /// Delete a file in the data directory.
    pub fn delete_in_plump(&self, name: &str) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            let dir = inner.data_dir;
            in_dir!(inner, dir, |dir_h| op_delete!(inner, dir_h, name))
        })
    }

    /// Seek to offset and write data in a file in the data directory.
    /// Used to update the chapter offset table after all chapters are appended.
    pub fn write_at_in_plump(
        &self,
        name: &str,
        offset: u32,
        data: &[u8],
    ) -> crate::error::Result<()> {
        poll_once(async {
            let mut guard = borrow(self)?;
            let inner = &mut *guard;
            let dir = inner.data_dir;
            in_dir!(inner, dir, |dir_h| {
                match inner
                    .mgr
                    .open_file_in_dir(dir_h, name, Mode::ReadWriteCreateOrAppend)
                    .await
                {
                    Err(_) => Err(Error::new(ErrorKind::OpenFile, "write_at")),
                    Ok(file) => {
                        let result = match inner.mgr.file_seek_from_start(file, offset) {
                            Ok(()) => inner
                                .mgr
                                .write(file, data)
                                .await
                                .map_err(|_| Error::new(ErrorKind::WriteFailed, "write_at")),
                            Err(_) => Err(Error::new(ErrorKind::SeekFailed, "write_at")),
                        };
                        let _ = inner.mgr.close_file(file).await;
                        if result.is_ok() {
                            crate::perf::counters::inc_sd_writes();
                            crate::perf::counters::add_sd_bytes_written(data.len() as u32);
                        }
                        result
                    }
                }
            })
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
            let inner = &mut *guard;
            let dd = inner.data_dir;
            in_subdir!(inner, dd, dir, |sub_h| op_write!(
                inner, sub_h, name, data
            ))
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
            let inner = &mut *guard;
            let dd = inner.data_dir;
            in_subdir!(inner, dd, dir, |sub_h| op_append!(
                inner, sub_h, name, data
            ))
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
            let inner = &mut *guard;
            let dd = inner.data_dir;
            in_subdir!(inner, dd, dir, |sub_h| op_read_chunk!(
                inner, sub_h, name, offset, buf
            ))
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
            let inner = &mut *guard;
            let dd = inner.data_dir;
            in_subdir!(inner, dd, dir, |sub_h| op_file_size!(
                inner, sub_h, name
            ))
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
            let inner = &mut *guard;
            let dd = inner.data_dir;
            in_subdir!(inner, dd, dir, |sub_h| op_delete!(inner, sub_h, name))
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

