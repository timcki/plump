//! The async Volume Manager implementation.
//!
//! The async volume manager handles partitions and open files on an async block device.
//! Unlike the synchronous [`crate::VolumeManager`], this uses `&mut self` instead of
//! interior mutability via `RefCell`, which is the natural fit for single-executor
//! async runtimes like Embassy.

use core::convert::TryFrom;
use core::ops::ControlFlow;

use byteorder::{ByteOrder, LittleEndian};
use heapless::Vec;

use crate::{
    Block, BlockCount, BlockIdx, Error, PARTITION_ID_FAT16, PARTITION_ID_FAT16_LBA,
    PARTITION_ID_FAT16_SMALL, PARTITION_ID_FAT32_CHS_LBA, PARTITION_ID_FAT32_LBA, RawVolume,
    ShortFileName, VolumeIdx, VolumeInfo, VolumeType,
    blockdevice::{AsyncBlockCache, AsyncBlockDevice},
    debug,
    fat::{self, RESERVED_ENTRIES},
    filesystem::{
        Attributes, ClusterId, DirEntry, DirectoryInfo, FileInfo, HandleGenerator, LfnBuffer,
        MAX_FILE_SIZE, Mode, RawDirectory, RawFile, TimeSource, ToShortFileName,
    },
    trace,
};

/// Wraps an async block device and gives access to the FAT-formatted volumes
/// within it.
///
/// This is the async counterpart to [`crate::VolumeManager`]. It uses `&mut self`
/// instead of interior mutability, which is the natural fit for single-executor
/// async runtimes like Embassy.
///
/// Tracks which files and directories are open, to prevent you from deleting
/// a file or directory you currently have open.
#[derive(Debug)]
pub struct AsyncVolumeManager<
    D,
    T,
    const MAX_DIRS: usize = 4,
    const MAX_FILES: usize = 4,
    const MAX_VOLUMES: usize = 1,
> where
    D: AsyncBlockDevice,
    T: TimeSource,
{
    time_source: T,
    id_generator: HandleGenerator,
    block_cache: AsyncBlockCache<D>,
    open_volumes: Vec<VolumeInfo, MAX_VOLUMES>,
    open_dirs: Vec<DirectoryInfo, MAX_DIRS>,
    open_files: Vec<FileInfo, MAX_FILES>,
}

impl<D, T> AsyncVolumeManager<D, T, 4, 4>
where
    D: AsyncBlockDevice,
    T: TimeSource,
{
    /// Create a new async Volume Manager using a generic `AsyncBlockDevice`.
    /// From this object we can open volumes (partitions) and with those we can
    /// open files.
    ///
    /// This creates an `AsyncVolumeManager` with default values
    /// MAX_DIRS = 4, MAX_FILES = 4, MAX_VOLUMES = 1. Call
    /// `AsyncVolumeManager::new_with_limits(block_device, time_source)` if you
    /// need different limits.
    pub fn new(block_device: D, time_source: T) -> AsyncVolumeManager<D, T, 4, 4, 1> {
        Self::new_with_limits(block_device, time_source, 5000)
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    AsyncVolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: AsyncBlockDevice,
    T: TimeSource,
{
    /// Create a new async Volume Manager using a generic `AsyncBlockDevice`.
    /// From this object we can open volumes (partitions) and with those we can
    /// open files.
    ///
    /// You can also give an offset for all the IDs this volume manager
    /// generates, which might help you find the IDs in your logs when
    /// debugging.
    pub fn new_with_limits(
        block_device: D,
        time_source: T,
        id_offset: u32,
    ) -> AsyncVolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES> {
        debug!("Creating new embedded-sdmmc::AsyncVolumeManager");
        AsyncVolumeManager {
            time_source,
            block_cache: AsyncBlockCache::new(block_device),
            id_generator: HandleGenerator::new(id_offset),
            open_volumes: Vec::new(),
            open_dirs: Vec::new(),
            open_files: Vec::new(),
        }
    }

    /// Temporarily get access to the underlying block device.
    pub fn device(&mut self) -> &mut D {
        self.block_cache.block_device()
    }

    /// Get a volume (or partition) based on entries in the Master Boot Record.
    ///
    /// We do not support GUID Partition Table disks. Nor do we support any
    /// concept of drive letters - that is for a higher layer to handle.
    pub async fn open_raw_volume(
        &mut self,
        volume_idx: VolumeIdx,
    ) -> Result<RawVolume, Error<D::Error>> {
        const PARTITION1_START: usize = 446;
        const PARTITION2_START: usize = PARTITION1_START + PARTITION_INFO_LENGTH;
        const PARTITION3_START: usize = PARTITION2_START + PARTITION_INFO_LENGTH;
        const PARTITION4_START: usize = PARTITION3_START + PARTITION_INFO_LENGTH;
        const FOOTER_START: usize = 510;
        const FOOTER_VALUE: u16 = 0xAA55;
        const PARTITION_INFO_LENGTH: usize = 16;
        const PARTITION_INFO_STATUS_INDEX: usize = 0;
        const PARTITION_INFO_TYPE_INDEX: usize = 4;
        const PARTITION_INFO_LBA_START_INDEX: usize = 8;
        const PARTITION_INFO_NUM_BLOCKS_INDEX: usize = 12;

        if self.open_volumes.is_full() {
            return Err(Error::TooManyOpenVolumes);
        }

        for v in self.open_volumes.iter() {
            if v.idx == volume_idx {
                return Err(Error::VolumeAlreadyOpen);
            }
        }

        let (part_type, lba_start, num_blocks) = {
            trace!("Reading partition table");
            let block = self
                .block_cache
                .read(BlockIdx(0))
                .await
                .map_err(Error::DeviceError)?;
            // We only support Master Boot Record (MBR) partitioned cards, not
            // GUID Partition Table (GPT)
            if LittleEndian::read_u16(&block[FOOTER_START..FOOTER_START + 2]) != FOOTER_VALUE {
                return Err(Error::FormatError("Invalid MBR signature"));
            }
            let partition = match volume_idx {
                VolumeIdx(0) => {
                    &block[PARTITION1_START..(PARTITION1_START + PARTITION_INFO_LENGTH)]
                }
                VolumeIdx(1) => {
                    &block[PARTITION2_START..(PARTITION2_START + PARTITION_INFO_LENGTH)]
                }
                VolumeIdx(2) => {
                    &block[PARTITION3_START..(PARTITION3_START + PARTITION_INFO_LENGTH)]
                }
                VolumeIdx(3) => {
                    &block[PARTITION4_START..(PARTITION4_START + PARTITION_INFO_LENGTH)]
                }
                _ => {
                    return Err(Error::NoSuchVolume);
                }
            };
            // Only 0x80 and 0x00 are valid (bootable, and non-bootable)
            if (partition[PARTITION_INFO_STATUS_INDEX] & 0x7F) != 0x00 {
                return Err(Error::FormatError("Invalid partition status"));
            }
            let lba_start = LittleEndian::read_u32(
                &partition[PARTITION_INFO_LBA_START_INDEX..(PARTITION_INFO_LBA_START_INDEX + 4)],
            );
            let num_blocks = LittleEndian::read_u32(
                &partition[PARTITION_INFO_NUM_BLOCKS_INDEX..(PARTITION_INFO_NUM_BLOCKS_INDEX + 4)],
            );
            (
                partition[PARTITION_INFO_TYPE_INDEX],
                BlockIdx(lba_start),
                BlockCount(num_blocks),
            )
        };
        match part_type {
            PARTITION_ID_FAT32_CHS_LBA
            | PARTITION_ID_FAT32_LBA
            | PARTITION_ID_FAT16_LBA
            | PARTITION_ID_FAT16
            | PARTITION_ID_FAT16_SMALL => {
                let volume = fat::async_volume::async_parse_volume(
                    &mut self.block_cache,
                    lba_start,
                    num_blocks,
                )
                .await?;
                let id = RawVolume(self.id_generator.generate());
                let info = VolumeInfo {
                    raw_volume: id,
                    idx: volume_idx,
                    volume_type: volume,
                };
                // We already checked for space
                self.open_volumes.push(info).unwrap();
                Ok(id)
            }
            _ => Err(Error::FormatError("Partition type not supported")),
        }
    }

    /// Open the volume's root directory.
    ///
    /// You can then read the directory entries with `iterate_dir`, or you can
    /// use `open_file_in_dir`.
    pub fn open_root_dir(&mut self, volume: RawVolume) -> Result<RawDirectory, Error<D::Error>> {
        debug!("Opening root on {:?}", volume);

        // Opening a root directory twice is OK

        let directory_id = RawDirectory(self.id_generator.generate());
        let dir_info = DirectoryInfo {
            raw_volume: volume,
            cluster: ClusterId::ROOT_DIR,
            raw_directory: directory_id,
        };

        self.open_dirs
            .push(dir_info)
            .map_err(|_| Error::TooManyOpenDirs)?;

        debug!("Opened root on {:?}, got {:?}", volume, directory_id);

        Ok(directory_id)
    }

    /// Open a directory.
    ///
    /// You can then read the directory entries with `iterate_dir` and `open_file_in_dir`.
    ///
    /// Passing "." as the name results in opening the `parent_dir` a second time.
    pub async fn open_dir<N>(
        &mut self,
        parent_dir: RawDirectory,
        name: N,
    ) -> Result<RawDirectory, Error<D::Error>>
    where
        N: ToShortFileName,
    {
        if self.open_dirs.is_full() {
            return Err(Error::TooManyOpenDirs);
        }

        // Find dir by ID
        let parent_dir_idx = self.get_dir_by_id(parent_dir)?;
        let volume_idx = self.get_volume_by_id(self.open_dirs[parent_dir_idx].raw_volume)?;
        let short_file_name = name.to_short_filename().map_err(Error::FilenameError)?;

        // Should we short-cut? (root dir doesn't have ".")
        if short_file_name == ShortFileName::this_dir() {
            let directory_id = RawDirectory(self.id_generator.generate());
            let dir_info = DirectoryInfo {
                raw_directory: directory_id,
                raw_volume: self.open_volumes[volume_idx].raw_volume,
                cluster: self.open_dirs[parent_dir_idx].cluster,
            };

            self.open_dirs
                .push(dir_info)
                .map_err(|_| Error::TooManyOpenDirs)?;

            return Ok(directory_id);
        }

        // ok we'll actually look for the directory then
        let dir_entry = match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                fat.async_find_directory_entry(
                    &mut self.block_cache,
                    &self.open_dirs[parent_dir_idx],
                    &short_file_name,
                )
                .await?
            }
        };

        debug!("Found dir entry: {:?}", dir_entry);

        if !dir_entry.attributes.is_directory() {
            return Err(Error::OpenedFileAsDir);
        }

        // We don't check if the directory is already open - directories hold
        // no cached state and so opening a directory twice is allowable.

        // Remember this open directory.
        let directory_id = RawDirectory(self.id_generator.generate());
        let dir_info = DirectoryInfo {
            raw_directory: directory_id,
            raw_volume: self.open_volumes[volume_idx].raw_volume,
            cluster: dir_entry.cluster,
        };

        self.open_dirs
            .push(dir_info)
            .map_err(|_| Error::TooManyOpenDirs)?;

        Ok(directory_id)
    }

    /// Close a directory.
    ///
    /// This releases internal resources and renders the given
    /// [`RawDirectory`] unusable.
    pub fn close_dir(&mut self, directory: RawDirectory) -> Result<(), Error<D::Error>> {
        debug!("Closing {:?}", directory);

        for (idx, info) in self.open_dirs.iter().enumerate() {
            if directory == info.raw_directory {
                self.open_dirs.swap_remove(idx);
                return Ok(());
            }
        }
        Err(Error::BadHandle)
    }

    /// Close a volume.
    ///
    /// You can't close it if there are any files or directories open on it.
    ///
    /// If the info sector update (which is non-critical) fails, the volume is
    /// closed anyway and the resulting error is returned.
    pub async fn close_volume(&mut self, volume: RawVolume) -> Result<(), Error<D::Error>> {
        for f in self.open_files.iter() {
            if f.raw_volume == volume {
                return Err(Error::VolumeStillInUse);
            }
        }

        for d in self.open_dirs.iter() {
            if d.raw_volume == volume {
                return Err(Error::VolumeStillInUse);
            }
        }

        let volume_idx = self.get_volume_by_id(volume)?;

        let update_result = match &mut self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => fat.async_update_info_sector(&mut self.block_cache).await,
        };

        self.open_volumes.swap_remove(volume_idx);

        update_result
    }

    /// Look in a directory for a named file.
    pub async fn find_directory_entry<N>(
        &mut self,
        directory: RawDirectory,
        name: N,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        N: ToShortFileName,
    {
        let directory_idx = self.get_dir_by_id(directory)?;
        let volume_idx = self.get_volume_by_id(self.open_dirs[directory_idx].raw_volume)?;
        match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                let sfn = name.to_short_filename().map_err(Error::FilenameError)?;
                fat.async_find_directory_entry(
                    &mut self.block_cache,
                    &self.open_dirs[directory_idx],
                    &sfn,
                )
                .await
            }
        }
    }

    /// Call a callback function for each directory entry in a directory.
    ///
    /// Long File Names will be ignored.
    pub async fn iterate_dir<F>(
        &mut self,
        directory: RawDirectory,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry) -> ControlFlow<()>,
    {
        let directory_idx = self.get_dir_by_id(directory)?;
        let volume_idx = self.get_volume_by_id(self.open_dirs[directory_idx].raw_volume)?;
        match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                fat.async_iterate_dir(
                    &mut self.block_cache,
                    &self.open_dirs[directory_idx],
                    |de| {
                        // Hide all the LFN directory entries
                        if !de.attributes.is_lfn() {
                            func(de)
                        } else {
                            ControlFlow::Continue(())
                        }
                    },
                )
                .await
            }
        }
    }

    /// Call a callback function for each directory entry in a directory, and
    /// process Long File Names.
    ///
    /// You must supply a [`LfnBuffer`] this API can use to temporarily hold the
    /// Long File Name. If you pass one that isn't large enough, any Long File
    /// Names that don't fit will be ignored and presented as if they only had a
    /// Short File Name.
    pub async fn iterate_dir_lfn<F>(
        &mut self,
        directory: RawDirectory,
        lfn_buffer: &mut LfnBuffer<'_>,
        func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry, Option<&str>) -> ControlFlow<()>,
    {
        let directory_idx = self.get_dir_by_id(directory)?;
        let volume_idx = self.get_volume_by_id(self.open_dirs[directory_idx].raw_volume)?;

        match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                fat.async_iterate_dir_lfn(
                    &mut self.block_cache,
                    lfn_buffer,
                    &self.open_dirs[directory_idx],
                    func,
                )
                .await
            }
        }
    }

    /// Open a file with the given short file name, in the given directory.
    pub async fn open_file_in_dir<N>(
        &mut self,
        directory: RawDirectory,
        name: N,
        mode: Mode,
    ) -> Result<RawFile, Error<D::Error>>
    where
        N: ToShortFileName,
    {
        // This check is load-bearing - we do an unchecked push later.
        if self.open_files.is_full() {
            return Err(Error::TooManyOpenFiles);
        }

        let directory_idx = self.get_dir_by_id(directory)?;
        let volume_id = self.open_dirs[directory_idx].raw_volume;
        let volume_idx = self.get_volume_by_id(volume_id)?;
        let sfn = name.to_short_filename().map_err(Error::FilenameError)?;

        let dir_entry = match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                fat.async_find_directory_entry(
                    &mut self.block_cache,
                    &self.open_dirs[directory_idx],
                    &sfn,
                )
                .await
            }
        };

        let dir_entry = match dir_entry {
            Ok(entry) => {
                // we are opening an existing file
                Some(entry)
            }
            Err(_)
                if (mode == Mode::ReadWriteCreate)
                    | (mode == Mode::ReadWriteCreateOrTruncate)
                    | (mode == Mode::ReadWriteCreateOrAppend) =>
            {
                // We are opening a non-existant file, but that's OK because they
                // asked us to create it
                None
            }
            _ => {
                // We are opening a non-existant file, and that's not OK.
                return Err(Error::NotFound);
            }
        };

        // Check if it's open already
        if let Some(dir_entry) = &dir_entry {
            if self.file_is_open(volume_id, dir_entry) {
                return Err(Error::FileAlreadyOpen);
            }
        }

        let mode = solve_mode_variant(mode, dir_entry.is_some());

        match mode {
            Mode::ReadWriteCreate => {
                if dir_entry.is_some() {
                    return Err(Error::FileAlreadyExists);
                }
                let cluster = self.open_dirs[directory_idx].cluster;
                let att = Attributes::create_from_fat(0);
                let volume_idx = self.get_volume_by_id(volume_id)?;
                let entry = match &mut self.open_volumes[volume_idx].volume_type {
                    VolumeType::Fat(fat) => {
                        fat.async_write_new_directory_entry(
                            &mut self.block_cache,
                            &self.time_source,
                            cluster,
                            sfn,
                            att,
                        )
                        .await?
                    }
                };

                let file_id = RawFile(self.id_generator.generate());

                let file = FileInfo {
                    raw_file: file_id,
                    raw_volume: volume_id,
                    current_cluster: (0, entry.cluster),
                    current_offset: 0,
                    mode,
                    entry,
                    dirty: false,
                };

                // Remember this open file - can't be full as we checked already
                unsafe {
                    self.open_files.push_unchecked(file);
                }

                Ok(file_id)
            }
            _ => {
                // Safe to unwrap, since we actually have an entry if we got here
                let dir_entry = dir_entry.unwrap();

                if dir_entry.attributes.is_read_only() && mode != Mode::ReadOnly {
                    return Err(Error::ReadOnly);
                }

                if dir_entry.attributes.is_directory() {
                    return Err(Error::OpenedDirAsFile);
                }

                // Check it's not already open
                if self.file_is_open(volume_id, &dir_entry) {
                    return Err(Error::FileAlreadyOpen);
                }

                let mode = solve_mode_variant(mode, true);
                let raw_file = RawFile(self.id_generator.generate());

                let file = match mode {
                    Mode::ReadOnly => FileInfo {
                        raw_file,
                        raw_volume: volume_id,
                        current_cluster: (0, dir_entry.cluster),
                        current_offset: 0,
                        mode,
                        entry: dir_entry,
                        dirty: false,
                    },
                    Mode::ReadWriteAppend => {
                        let mut file = FileInfo {
                            raw_file,
                            raw_volume: volume_id,
                            current_cluster: (0, dir_entry.cluster),
                            current_offset: 0,
                            mode,
                            entry: dir_entry,
                            dirty: false,
                        };
                        // seek_from_end with 0 can't fail
                        file.seek_from_end(0).ok();
                        file
                    }
                    Mode::ReadWriteTruncate => {
                        let mut file = FileInfo {
                            raw_file,
                            raw_volume: volume_id,
                            current_cluster: (0, dir_entry.cluster),
                            current_offset: 0,
                            mode,
                            entry: dir_entry,
                            dirty: false,
                        };
                        let volume_idx = self.get_volume_by_id(volume_id)?;
                        match &mut self.open_volumes[volume_idx].volume_type {
                            VolumeType::Fat(fat) => {
                                fat.async_truncate_cluster_chain(
                                    &mut self.block_cache,
                                    file.entry.cluster,
                                )
                                .await?
                            }
                        };
                        file.update_length(0);
                        let volume_idx = self.get_volume_by_id(volume_id)?;
                        match &self.open_volumes[volume_idx].volume_type {
                            VolumeType::Fat(fat) => {
                                file.entry.mtime = self.time_source.get_timestamp();
                                fat.async_write_entry_to_disk(&mut self.block_cache, &file.entry)
                                    .await?;
                            }
                        };

                        file
                    }
                    _ => return Err(Error::Unsupported),
                };

                // Remember this open file - can't be full as we checked already
                unsafe {
                    self.open_files.push_unchecked(file);
                }

                Ok(raw_file)
            }
        }
    }

    /// Open a file with the given Unicode long file name, in the given directory.
    ///
    /// You can only open existing long-file-name files - you cannot create them.
    pub async fn open_long_name_file_in_dir(
        &mut self,
        directory: RawDirectory,
        name: &str,
        mode: Mode,
    ) -> Result<RawFile, Error<D::Error>> {
        // This check is load-bearing - we do an unchecked push later.
        if self.open_files.is_full() {
            return Err(Error::TooManyOpenFiles);
        }

        let directory_idx = self.get_dir_by_id(directory)?;
        let volume_id = self.open_dirs[directory_idx].raw_volume;
        let volume_idx = self.get_volume_by_id(volume_id)?;

        let dir_entry = match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                fat.async_find_directory_entry_by_lfn(
                    &mut self.block_cache,
                    &self.open_dirs[directory_idx],
                    name,
                )
                .await
            }
        };

        let dir_entry = match dir_entry {
            Ok(entry) => {
                // we are opening an existing file
                entry
            }
            Err(_)
                if (mode == Mode::ReadWriteCreate)
                    | (mode == Mode::ReadWriteCreateOrTruncate)
                    | (mode == Mode::ReadWriteCreateOrAppend) =>
            {
                // We are opening a non-existant file and we cannot do that with LFNs
                return Err(Error::NotFound);
            }
            _ => {
                // We are opening a non-existant file, and that's not OK.
                return Err(Error::NotFound);
            }
        };

        // Check if it's open already
        if self.file_is_open(volume_id, &dir_entry) {
            return Err(Error::FileAlreadyOpen);
        }

        let mode = solve_mode_variant(mode, true);

        match mode {
            Mode::ReadWriteCreate => Err(Error::FileAlreadyExists),
            _ => {
                if dir_entry.attributes.is_read_only() && mode != Mode::ReadOnly {
                    return Err(Error::ReadOnly);
                }

                if dir_entry.attributes.is_directory() {
                    return Err(Error::OpenedDirAsFile);
                }

                // Check it's not already open
                if self.file_is_open(volume_id, &dir_entry) {
                    return Err(Error::FileAlreadyOpen);
                }

                let mode = solve_mode_variant(mode, true);
                let raw_file = RawFile(self.id_generator.generate());

                let file = match mode {
                    Mode::ReadOnly => FileInfo {
                        raw_file,
                        raw_volume: volume_id,
                        current_cluster: (0, dir_entry.cluster),
                        current_offset: 0,
                        mode,
                        entry: dir_entry,
                        dirty: false,
                    },
                    Mode::ReadWriteAppend => {
                        let mut file = FileInfo {
                            raw_file,
                            raw_volume: volume_id,
                            current_cluster: (0, dir_entry.cluster),
                            current_offset: 0,
                            mode,
                            entry: dir_entry,
                            dirty: false,
                        };
                        // seek_from_end with 0 can't fail
                        file.seek_from_end(0).ok();
                        file
                    }
                    Mode::ReadWriteTruncate => {
                        let mut file = FileInfo {
                            raw_file,
                            raw_volume: volume_id,
                            current_cluster: (0, dir_entry.cluster),
                            current_offset: 0,
                            mode,
                            entry: dir_entry,
                            dirty: false,
                        };
                        let volume_idx = self.get_volume_by_id(volume_id)?;
                        match &mut self.open_volumes[volume_idx].volume_type {
                            VolumeType::Fat(fat) => {
                                fat.async_truncate_cluster_chain(
                                    &mut self.block_cache,
                                    file.entry.cluster,
                                )
                                .await?
                            }
                        };
                        file.update_length(0);
                        let volume_idx = self.get_volume_by_id(volume_id)?;
                        match &self.open_volumes[volume_idx].volume_type {
                            VolumeType::Fat(fat) => {
                                file.entry.mtime = self.time_source.get_timestamp();
                                fat.async_write_entry_to_disk(&mut self.block_cache, &file.entry)
                                    .await?;
                            }
                        };

                        file
                    }
                    _ => return Err(Error::Unsupported),
                };

                // Remember this open file - can't be full as we checked already
                unsafe {
                    self.open_files.push_unchecked(file);
                }

                Ok(raw_file)
            }
        }
    }

    /// Delete a closed file or empty directory with the given filename, if it exists.
    pub async fn delete_entry_in_dir<N>(
        &mut self,
        directory: RawDirectory,
        name: N,
    ) -> Result<(), Error<D::Error>>
    where
        N: ToShortFileName,
    {
        let dir_idx = self.get_dir_by_id(directory)?;
        let volume_idx = self.get_volume_by_id(self.open_dirs[dir_idx].raw_volume)?;
        let sfn = name.to_short_filename().map_err(Error::FilenameError)?;

        let dir_entry = match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                fat.async_find_directory_entry(
                    &mut self.block_cache,
                    &self.open_dirs[dir_idx],
                    &sfn,
                )
                .await
            }
        }?;

        let raw_volume = self.open_dirs[dir_idx].raw_volume;

        if dir_entry.attributes.is_directory() {
            // Find the directory to be deleted, so that we can check its contents.
            if self
                .open_dirs
                .iter()
                .any(|dir_info| dir_info.cluster == dir_entry.cluster)
            {
                // Subdirectory is already open.
                return Err(Error::DirAlreadyOpen);
            }
            // The subdirectory isn't yet open. Open it in order to be able to list it.
            let raw_directory = RawDirectory(self.id_generator.generate());
            let dir_info = DirectoryInfo {
                raw_directory,
                raw_volume: self.open_volumes[volume_idx].raw_volume,
                cluster: dir_entry.cluster,
            };
            // Can only delete directories that are already empty.
            let mut count = 0;
            match &self.open_volumes[volume_idx].volume_type {
                VolumeType::Fat(fat) => {
                    fat.async_iterate_dir(&mut self.block_cache, &dir_info, |de| {
                        if !de.attributes.is_lfn()
                            && de.name != ShortFileName::this_dir()
                            && de.name != ShortFileName::parent_dir()
                        {
                            count += 1;
                        }
                        ControlFlow::Continue(())
                    })
                    .await?;
                }
            }
            if count != 0 {
                return Err(Error::DeleteNonEmptyDir);
            }
        } else if self.file_is_open(raw_volume, &dir_entry) {
            return Err(Error::FileAlreadyOpen);
        }

        let parent_dir_info = &self.open_dirs[dir_idx];
        let volume_idx = self.get_volume_by_id(parent_dir_info.raw_volume)?;
        match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                fat.async_delete_directory_entry(&mut self.block_cache, parent_dir_info, &sfn)
                    .await?
            }
        }

        Ok(())
    }

    /// Get the volume label
    ///
    /// Will look in the filesystem metadata for a volume label, and if
    /// nothing is found, will search the root directory for a volume label.
    pub async fn get_root_volume_label(
        &mut self,
        raw_volume: RawVolume,
    ) -> Result<Option<crate::VolumeName>, Error<D::Error>> {
        debug!("Reading volume label for {:?}", raw_volume);
        // prefer the one in the BPB - it's easier to get
        let volume_idx = self.get_volume_by_id(raw_volume)?;
        match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                if !fat.name.name().is_empty() {
                    debug!(
                        "Got volume label {:?} for {:?} from BPB",
                        fat.name, raw_volume
                    );
                    return Ok(Some(fat.name.clone()));
                }
            }
        }

        // Nothing in the BPB, let's do it the slow way
        let root_dir = self.open_root_dir(raw_volume)?;
        let mut maybe_volume_name = None;
        self.iterate_dir(root_dir, |de| {
            if maybe_volume_name.is_none()
                && de.attributes == Attributes::create_from_fat(Attributes::VOLUME)
            {
                maybe_volume_name = Some(unsafe { de.name.to_volume_label() });
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await?;
        self.close_dir(root_dir)?;

        debug!(
            "Got volume label {:?} for {:?} from root",
            maybe_volume_name, raw_volume
        );

        Ok(maybe_volume_name)
    }

    /// Read from an open file.
    ///
    /// We read as many bytes as we can, stopping at either the length of
    /// `buffer`, or the end of the file.
    ///
    /// The number of bytes written to `buffer` is returned on success,
    /// otherwise you get an error and you should not rely on either the
    /// current seek position or the contents of `buffer`.
    pub async fn read(
        &mut self,
        file: RawFile,
        buffer: &mut [u8],
    ) -> Result<usize, Error<D::Error>> {
        let file_idx = self.get_file_by_id(file)?;
        let volume_idx = self.get_volume_by_id(self.open_files[file_idx].raw_volume)?;

        // Calculate which file block the current offset lies within
        // While there is more to read, read the block and copy in to the buffer.
        // If we need to find the next cluster, walk the FAT.
        let mut space = buffer.len();
        let mut read = 0;
        while space > 0 && !self.open_files[file_idx].eof() {
            let mut current_cluster = self.open_files[file_idx].current_cluster;
            let (block_idx, block_offset, block_avail) = self
                .find_data_on_disk(
                    volume_idx,
                    &mut current_cluster,
                    self.open_files[file_idx].entry.cluster,
                    self.open_files[file_idx].current_offset,
                )
                .await?;
            self.open_files[file_idx].current_cluster = current_cluster;
            trace!("Reading file ID {:?}", file);
            let block = self
                .block_cache
                .read(block_idx)
                .await
                .map_err(Error::DeviceError)?;
            let to_copy = block_avail
                .min(space)
                .min(self.open_files[file_idx].left() as usize);
            assert!(to_copy != 0);
            buffer[read..read + to_copy]
                .copy_from_slice(&block[block_offset..block_offset + to_copy]);
            read += to_copy;
            space -= to_copy;
            self.open_files[file_idx]
                .seek_from_current(to_copy as i32)
                .unwrap();
        }
        Ok(read)
    }

    /// Write to a open file.
    ///
    /// Endeavours to write the entire contents of the slice, stopping only if
    /// there is an error reading from or writing to the disk, or if the
    /// volume runs out of space.
    ///
    /// If you get an error, then you cannot be sure how much of `buffer` was
    /// successfully written, nor can you rely on the current seek position.
    pub async fn write(&mut self, file: RawFile, buffer: &[u8]) -> Result<(), Error<D::Error>> {
        #[cfg(feature = "defmt-log")]
        debug!("write(file={:?}, buffer={:x}", file, buffer);

        #[cfg(feature = "log")]
        debug!("write(file={:?}, buffer={:x?}", file, buffer);

        let file_idx = self.get_file_by_id(file)?;
        let volume_idx = self.get_volume_by_id(self.open_files[file_idx].raw_volume)?;

        if self.open_files[file_idx].mode == Mode::ReadOnly {
            return Err(Error::ReadOnly);
        }

        self.open_files[file_idx].dirty = true;

        if self.open_files[file_idx].entry.cluster.0 < RESERVED_ENTRIES {
            // file doesn't have a valid allocated cluster (possible zero-length file), allocate one
            self.open_files[file_idx].entry.cluster =
                match self.open_volumes[volume_idx].volume_type {
                    VolumeType::Fat(ref mut fat) => {
                        fat.async_alloc_cluster(&mut self.block_cache, None, false)
                            .await?
                    }
                };
            debug!(
                "Alloc first cluster {:?}",
                self.open_files[file_idx].entry.cluster
            );
        }

        let volume_idx = self.get_volume_by_id(self.open_files[file_idx].raw_volume)?;

        if (self.open_files[file_idx].current_cluster.1) < self.open_files[file_idx].entry.cluster {
            debug!("Rewinding to start");
            self.open_files[file_idx].current_cluster =
                (0, self.open_files[file_idx].entry.cluster);
        }
        let bytes_until_max =
            usize::try_from(MAX_FILE_SIZE - self.open_files[file_idx].current_offset)
                .map_err(|_| Error::ConversionError)?;
        let bytes_to_write = core::cmp::min(buffer.len(), bytes_until_max);
        let mut written = 0;

        while written < bytes_to_write {
            let mut current_cluster = self.open_files[file_idx].current_cluster;
            debug!(
                "Have written bytes {}/{}, finding cluster {:?}",
                written, bytes_to_write, current_cluster
            );
            let current_offset = self.open_files[file_idx].current_offset;
            let (block_idx, block_offset, block_avail) = match self
                .find_data_on_disk(
                    volume_idx,
                    &mut current_cluster,
                    self.open_files[file_idx].entry.cluster,
                    current_offset,
                )
                .await
            {
                Ok(vars) => {
                    debug!(
                        "Found block_idx={:?}, block_offset={:?}, block_avail={}",
                        vars.0, vars.1, vars.2
                    );
                    vars
                }
                Err(Error::EndOfFile) => {
                    debug!("Extending file");
                    match self.open_volumes[volume_idx].volume_type {
                        VolumeType::Fat(ref mut fat) => {
                            if fat
                                .async_alloc_cluster(
                                    &mut self.block_cache,
                                    Some(current_cluster.1),
                                    false,
                                )
                                .await
                                .is_err()
                            {
                                return Err(Error::DiskFull);
                            }
                            debug!("Allocated new FAT cluster, finding offsets...");
                            let new_offset = self
                                .find_data_on_disk(
                                    volume_idx,
                                    &mut current_cluster,
                                    self.open_files[file_idx].entry.cluster,
                                    self.open_files[file_idx].current_offset,
                                )
                                .await
                                .map_err(|_| Error::AllocationError)?;
                            debug!("New offset {:?}", new_offset);
                            new_offset
                        }
                    }
                }
                Err(e) => return Err(e),
            };
            let to_copy = core::cmp::min(block_avail, bytes_to_write - written);
            let block = if (block_offset == 0) && (to_copy == block_avail) {
                // we're replacing the whole Block, so the previous contents
                // are irrelevant
                self.block_cache.blank_mut(block_idx)
            } else {
                debug!("Reading for partial block write");
                self.block_cache
                    .read_mut(block_idx)
                    .await
                    .map_err(Error::DeviceError)?
            };
            block[block_offset..block_offset + to_copy]
                .copy_from_slice(&buffer[written..written + to_copy]);
            debug!("Writing block {:?}", block_idx);
            self.block_cache.write_back().await?;
            written += to_copy;
            self.open_files[file_idx].current_cluster = current_cluster;

            let to_copy = to_copy as u32;
            let new_offset = self.open_files[file_idx].current_offset + to_copy;
            if new_offset > self.open_files[file_idx].entry.size {
                // We made it longer
                self.open_files[file_idx].update_length(new_offset);
            }
            self.open_files[file_idx]
                .seek_from_start(new_offset)
                .unwrap();
            // Entry update deferred to file close, for performance.
        }
        self.open_files[file_idx].entry.attributes.set_archive(true);
        self.open_files[file_idx].entry.mtime = self.time_source.get_timestamp();
        Ok(())
    }

    /// Close a file with the given raw file handle.
    ///
    /// Attempts to flush the file before closing, if necessary. If the flush
    /// fails, the file is closed anyway and the resulting error is returned.
    pub async fn close_file(&mut self, file: RawFile) -> Result<(), Error<D::Error>> {
        let flush_result = self.flush_file(file).await;
        let file_idx = self.get_file_by_id(file)?;
        self.open_files.swap_remove(file_idx);
        flush_result
    }

    /// Flush (update the entry) for a file with the given raw file handle.
    pub async fn flush_file(&mut self, file: RawFile) -> Result<(), Error<D::Error>> {
        let file_id = self.get_file_by_id(file)?;

        if self.open_files[file_id].dirty {
            let volume_idx = self.get_volume_by_id(self.open_files[file_id].raw_volume)?;
            match &mut self.open_volumes[volume_idx].volume_type {
                VolumeType::Fat(fat) => {
                    debug!("Updating FAT info sector");
                    fat.async_update_info_sector(&mut self.block_cache).await?;
                    debug!("Updating dir entry {:?}", self.open_files[file_id].entry);
                    if self.open_files[file_id].entry.size != 0 {
                        // If you have a length, you must have a cluster
                        assert!(self.open_files[file_id].entry.cluster.0 != 0);
                    }
                    fat.async_write_entry_to_disk(
                        &mut self.block_cache,
                        &self.open_files[file_id].entry,
                    )
                    .await?;
                }
            };
        }
        Ok(())
    }

    /// Check if any files or folders are open.
    pub fn has_open_handles(&self) -> bool {
        !(self.open_dirs.is_empty() || self.open_files.is_empty())
    }

    /// Consume self and return BlockDevice and TimeSource
    pub fn free(self) -> (D, T) {
        (self.block_cache.free(), self.time_source)
    }

    /// Check if a file is at End Of File.
    pub fn file_eof(&self, file: RawFile) -> Result<bool, Error<D::Error>> {
        let file_idx = self.get_file_by_id(file)?;
        Ok(self.open_files[file_idx].eof())
    }

    /// Seek a file with an offset from the start of the file.
    pub fn file_seek_from_start(
        &mut self,
        file: RawFile,
        offset: u32,
    ) -> Result<(), Error<D::Error>> {
        let file_idx = self.get_file_by_id(file)?;
        self.open_files[file_idx]
            .seek_from_start(offset)
            .map_err(|_| Error::InvalidOffset)?;
        Ok(())
    }

    /// Seek a file with an offset from the current position.
    pub fn file_seek_from_current(
        &mut self,
        file: RawFile,
        offset: i32,
    ) -> Result<(), Error<D::Error>> {
        let file_idx = self.get_file_by_id(file)?;
        self.open_files[file_idx]
            .seek_from_current(offset)
            .map_err(|_| Error::InvalidOffset)?;
        Ok(())
    }

    /// Seek a file with an offset back from the end of the file.
    pub fn file_seek_from_end(
        &mut self,
        file: RawFile,
        offset: u32,
    ) -> Result<(), Error<D::Error>> {
        let file_idx = self.get_file_by_id(file)?;
        self.open_files[file_idx]
            .seek_from_end(offset)
            .map_err(|_| Error::InvalidOffset)?;
        Ok(())
    }

    /// Get the length of a file
    pub fn file_length(&self, file: RawFile) -> Result<u32, Error<D::Error>> {
        let file_idx = self.get_file_by_id(file)?;
        Ok(self.open_files[file_idx].length())
    }

    /// Get the current offset of a file
    pub fn file_offset(&self, file: RawFile) -> Result<u32, Error<D::Error>> {
        let file_idx = self.get_file_by_id(file)?;
        Ok(self.open_files[file_idx].current_offset)
    }

    /// Create a directory in a given directory, with the given short name.
    pub async fn make_dir_in_dir<N>(
        &mut self,
        directory: RawDirectory,
        name: N,
    ) -> Result<(), Error<D::Error>>
    where
        N: ToShortFileName,
    {
        if self.open_dirs.is_full() {
            return Err(Error::TooManyOpenDirs);
        }

        let parent_directory_idx = self.get_dir_by_id(directory)?;
        let volume_id = self.open_dirs[parent_directory_idx].raw_volume;
        let volume_idx = self.get_volume_by_id(volume_id)?;
        let sfn = name.to_short_filename().map_err(Error::FilenameError)?;

        debug!("Creating directory '{}'", sfn);
        debug!(
            "Parent dir is in cluster {:?}",
            self.open_dirs[parent_directory_idx].cluster
        );

        // Does an entry exist with this name?
        let parent_dir_info = &self.open_dirs[parent_directory_idx];
        let maybe_dir_entry = match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                fat.async_find_directory_entry(&mut self.block_cache, parent_dir_info, &sfn)
                    .await
            }
        };

        match maybe_dir_entry {
            Ok(entry) if entry.attributes.is_directory() => {
                return Err(Error::DirAlreadyExists);
            }
            Ok(_entry) => {
                return Err(Error::FileAlreadyExists);
            }
            Err(Error::NotFound) => {
                // perfect, let's make it
            }
            Err(e) => {
                // Some other error - tell them about it
                return Err(e);
            }
        };

        let att = Attributes::create_from_fat(Attributes::DIRECTORY);
        let parent_cluster = self.open_dirs[parent_directory_idx].cluster;

        // Need mutable access for this
        match &mut self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => {
                debug!("Making dir entry");
                fat.async_make_dir(
                    &mut self.block_cache,
                    &self.time_source,
                    parent_cluster,
                    sfn,
                    att,
                )
                .await?;
            }
        };

        Ok(())
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Check if a file is open
    fn file_is_open(&self, raw_volume: RawVolume, dir_entry: &DirEntry) -> bool {
        for f in self.open_files.iter() {
            if f.raw_volume == raw_volume
                && f.entry.entry_block == dir_entry.entry_block
                && f.entry.entry_offset == dir_entry.entry_offset
            {
                return true;
            }
        }
        false
    }

    fn get_volume_by_id(&self, raw_volume: RawVolume) -> Result<usize, Error<D::Error>> {
        for (idx, v) in self.open_volumes.iter().enumerate() {
            if v.raw_volume == raw_volume {
                return Ok(idx);
            }
        }
        Err(Error::BadHandle)
    }

    fn get_dir_by_id(&self, raw_directory: RawDirectory) -> Result<usize, Error<D::Error>> {
        for (idx, d) in self.open_dirs.iter().enumerate() {
            if d.raw_directory == raw_directory {
                return Ok(idx);
            }
        }
        Err(Error::BadHandle)
    }

    fn get_file_by_id(&self, raw_file: RawFile) -> Result<usize, Error<D::Error>> {
        for (idx, f) in self.open_files.iter().enumerate() {
            if f.raw_file == raw_file {
                return Ok(idx);
            }
        }
        Err(Error::BadHandle)
    }

    /// This function turns `desired_offset` into an appropriate block to be
    /// read. It either calculates this based on the start of the file, or
    /// from the given start point - whichever is better.
    ///
    /// Returns:
    ///
    /// * the index for the block on the disk that contains the data we want,
    /// * the byte offset into that block for the data we want, and
    /// * how many bytes remain in that block.
    async fn find_data_on_disk(
        &mut self,
        volume_idx: usize,
        start: &mut (u32, ClusterId),
        file_start: ClusterId,
        desired_offset: u32,
    ) -> Result<(BlockIdx, usize, usize), Error<D::Error>> {
        let bytes_per_cluster = match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => fat.bytes_per_cluster(),
        };
        // do we need to be before our start point?
        if desired_offset < start.0 {
            // user wants to go backwards - start from the beginning of the file
            // because the FAT is only a singly-linked list.
            start.0 = 0;
            start.1 = file_start;
        }
        // How many clusters forward do we need to go?
        let offset_from_cluster = desired_offset - start.0;
        // walk through the FAT chain
        let num_clusters = offset_from_cluster / bytes_per_cluster;
        for _ in 0..num_clusters {
            start.1 = match &self.open_volumes[volume_idx].volume_type {
                VolumeType::Fat(fat) => {
                    fat.async_next_cluster(&mut self.block_cache, start.1)
                        .await?
                }
            };
            start.0 += bytes_per_cluster;
        }
        // How many blocks in are we now?
        let offset_from_cluster = desired_offset - start.0;
        assert!(offset_from_cluster < bytes_per_cluster);
        let num_blocks = BlockCount(offset_from_cluster / Block::LEN_U32);
        let block_idx = match &self.open_volumes[volume_idx].volume_type {
            VolumeType::Fat(fat) => fat.cluster_to_block(start.1),
        } + num_blocks;
        let block_offset = (desired_offset % Block::LEN_U32) as usize;
        let available = Block::LEN - block_offset;
        Ok((block_idx, block_offset, available))
    }
}

/// Transforms the mode variant into the concrete one that the file should be opened in.
fn solve_mode_variant(mode: Mode, file_exists: bool) -> Mode {
    match mode {
        Mode::ReadWriteCreateOrTruncate if file_exists => Mode::ReadWriteTruncate,
        Mode::ReadWriteCreateOrTruncate => Mode::ReadWriteCreate,
        Mode::ReadWriteCreateOrAppend if file_exists => Mode::ReadWriteAppend,
        Mode::ReadWriteCreateOrAppend => Mode::ReadWriteCreate,
        mode => mode,
    }
}
