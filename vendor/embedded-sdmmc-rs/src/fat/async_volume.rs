//! Async FAT-specific volume support.
//!
//! This module provides async counterparts to the methods in [`super::volume`],
//! for use with [`AsyncBlockDevice`] and [`AsyncBlockCache`].

use core::convert::TryFrom;
use core::ops::ControlFlow;

use byteorder::{ByteOrder, LittleEndian};

use crate::{
    Attributes, Block, BlockCount, BlockIdx, ClusterId, DirEntry, DirectoryInfo, Error, LfnBuffer,
    ShortFileName, TimeSource, VolumeType,
    blockdevice::{AsyncBlockCache, AsyncBlockDevice},
    debug,
    fat::{
        Bpb, Fat16Info, Fat32Info, FatSpecificInfo, FatType, InfoSector, OnDiskDirEntry,
        RESERVED_ENTRIES,
    },
    trace, warn,
};

use super::volume::FatVolume;

/// Async methods on [`FatVolume`].
///
/// These are the async equivalents of the synchronous methods, using
/// [`AsyncBlockCache`] instead of [`crate::BlockCache`].
impl FatVolume {
    /// Write a new entry in the FAT info sector (async version).
    pub async fn async_update_info_sector<D>(
        &mut self,
        block_cache: &mut AsyncBlockCache<D>,
    ) -> Result<(), Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_) => {
                // FAT16 volumes don't have an info sector
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                if self.free_clusters_count.is_none() && self.next_free_cluster.is_none() {
                    return Ok(());
                }
                trace!("Reading info sector");
                let block = block_cache
                    .read_mut(fat32_info.info_location)
                    .await
                    .map_err(Error::DeviceError)?;
                if let Some(count) = self.free_clusters_count {
                    block[488..492].copy_from_slice(&count.to_le_bytes());
                }
                if let Some(next_free_cluster) = self.next_free_cluster {
                    block[492..496].copy_from_slice(&next_free_cluster.0.to_le_bytes());
                }
                trace!("Writing info sector");
                block_cache.write_back().await?;
            }
        }
        Ok(())
    }

    /// Write a new entry in the FAT (async version).
    async fn async_update_fat<D>(
        &mut self,
        block_cache: &mut AsyncBlockCache<D>,
        cluster: ClusterId,
        new_value: ClusterId,
    ) -> Result<(), Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        let mut second_fat_block_num = None;
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                let fat_offset = cluster.0 * 2;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                if let Some(second_fat_start) = self.second_fat_start {
                    second_fat_block_num =
                        Some(self.lba_start + second_fat_start.offset_bytes(fat_offset));
                }
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Reading FAT for update");
                let block = block_cache
                    .read_mut(this_fat_block_num)
                    .await
                    .map_err(Error::DeviceError)?;
                // See <https://en.wikipedia.org/wiki/Design_of_the_FAT_file_system>
                let entry = match new_value {
                    ClusterId::INVALID => 0xFFF6,
                    ClusterId::BAD => 0xFFF7,
                    ClusterId::EMPTY => 0x0000,
                    ClusterId::END_OF_FILE => 0xFFFF,
                    _ => new_value.0 as u16,
                };
                LittleEndian::write_u16(
                    &mut block[this_fat_ent_offset..=this_fat_ent_offset + 1],
                    entry,
                );
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                // FAT32 => 4 bytes per entry
                let fat_offset = cluster.0 * 4;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                if let Some(second_fat_start) = self.second_fat_start {
                    second_fat_block_num =
                        Some(self.lba_start + second_fat_start.offset_bytes(fat_offset));
                }
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Reading FAT for update");
                let block = block_cache
                    .read_mut(this_fat_block_num)
                    .await
                    .map_err(Error::DeviceError)?;
                let entry = match new_value {
                    ClusterId::INVALID => 0x0FFF_FFF6,
                    ClusterId::BAD => 0x0FFF_FFF7,
                    ClusterId::EMPTY => 0x0000_0000,
                    _ => new_value.0,
                };
                let existing =
                    LittleEndian::read_u32(&block[this_fat_ent_offset..=this_fat_ent_offset + 3]);
                let new = (existing & 0xF000_0000) | (entry & 0x0FFF_FFFF);
                LittleEndian::write_u32(
                    &mut block[this_fat_ent_offset..=this_fat_ent_offset + 3],
                    new,
                );
            }
        }
        trace!("Updating FAT");
        if let Some(duplicate) = second_fat_block_num {
            block_cache.write_back_with_duplicate(duplicate).await?;
        } else {
            block_cache.write_back().await?;
        }
        Ok(())
    }

    /// Look in the FAT to see which cluster comes next (async version).
    pub(crate) async fn async_next_cluster<D>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        cluster: ClusterId,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        if cluster.0 > (u32::MAX / 4) {
            panic!("next_cluster called on invalid cluster {:x?}", cluster);
        }
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                let fat_offset = cluster.0 * 2;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Walking FAT");
                let block = block_cache.read(this_fat_block_num).await?;
                let fat_entry =
                    LittleEndian::read_u16(&block[this_fat_ent_offset..=this_fat_ent_offset + 1]);
                match fat_entry {
                    0xFFF7 => {
                        // Bad cluster
                        Err(Error::BadCluster)
                    }
                    0xFFF8..=0xFFFF => {
                        // There is no next cluster
                        Err(Error::EndOfFile)
                    }
                    f => {
                        // Seems legit
                        Ok(ClusterId(u32::from(f)))
                    }
                }
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                let fat_offset = cluster.0 * 4;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Walking FAT");
                let block = block_cache.read(this_fat_block_num).await?;
                let fat_entry =
                    LittleEndian::read_u32(&block[this_fat_ent_offset..=this_fat_ent_offset + 3])
                        & 0x0FFF_FFFF;
                match fat_entry {
                    0x0000_0000 => {
                        // Jumped to free space
                        Err(Error::UnterminatedFatChain)
                    }
                    0x0FFF_FFF7 => {
                        // Bad cluster
                        Err(Error::BadCluster)
                    }
                    0x0000_0001 | 0x0FFF_FFF8..=0x0FFF_FFFF => {
                        // There is no next cluster
                        Err(Error::EndOfFile)
                    }
                    f => {
                        // Seems legit
                        Ok(ClusterId(f))
                    }
                }
            }
        }
    }

    /// Finds an empty entry space and writes the new entry to it, allocates a new cluster if
    /// needed (async version).
    pub(crate) async fn async_write_new_directory_entry<D, T>(
        &mut self,
        block_cache: &mut AsyncBlockCache<D>,
        time_source: &T,
        dir_cluster: ClusterId,
        name: ShortFileName,
        attributes: Attributes,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: AsyncBlockDevice,
        T: TimeSource,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                let mut current_cluster = Some(dir_cluster);
                let mut first_dir_block_num = match dir_cluster {
                    ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
                    _ => self.cluster_to_block(dir_cluster),
                };
                let dir_size = match dir_cluster {
                    ClusterId::ROOT_DIR => {
                        let len_bytes =
                            u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                        BlockCount::from_bytes(len_bytes)
                    }
                    _ => BlockCount(u32::from(self.blocks_per_cluster)),
                };

                while let Some(cluster) = current_cluster {
                    for block_idx in first_dir_block_num.range(dir_size) {
                        trace!("Reading directory");
                        let block = block_cache
                            .read_mut(block_idx)
                            .await
                            .map_err(Error::DeviceError)?;
                        for (i, dir_entry_bytes) in
                            block.chunks_exact_mut(OnDiskDirEntry::LEN).enumerate()
                        {
                            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                            if !dir_entry.is_valid() {
                                let ctime = time_source.get_timestamp();
                                let entry = DirEntry::new(
                                    name,
                                    attributes,
                                    ClusterId::EMPTY,
                                    ctime,
                                    block_idx,
                                    (i * OnDiskDirEntry::LEN) as u32,
                                );
                                dir_entry_bytes
                                    .copy_from_slice(&entry.serialize(FatType::Fat16)[..]);
                                trace!("Updating directory");
                                block_cache.write_back().await?;
                                return Ok(entry);
                            }
                        }
                    }
                    if cluster != ClusterId::ROOT_DIR {
                        current_cluster = match self.async_next_cluster(block_cache, cluster).await
                        {
                            Ok(n) => {
                                first_dir_block_num = self.cluster_to_block(n);
                                Some(n)
                            }
                            Err(Error::EndOfFile) => {
                                let c = self
                                    .async_alloc_cluster(block_cache, Some(cluster), true)
                                    .await?;
                                first_dir_block_num = self.cluster_to_block(c);
                                Some(c)
                            }
                            _ => None,
                        };
                    } else {
                        current_cluster = None;
                    }
                }
                Err(Error::NotEnoughSpace)
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                let mut current_cluster = match dir_cluster {
                    ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
                    _ => Some(dir_cluster),
                };
                let mut first_dir_block_num = self.cluster_to_block(dir_cluster);
                let dir_size = BlockCount(u32::from(self.blocks_per_cluster));

                while let Some(cluster) = current_cluster {
                    for block_idx in first_dir_block_num.range(dir_size) {
                        trace!("Reading directory");
                        let block = block_cache
                            .read_mut(block_idx)
                            .await
                            .map_err(Error::DeviceError)?;
                        for (i, dir_entry_bytes) in
                            block.chunks_exact_mut(OnDiskDirEntry::LEN).enumerate()
                        {
                            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                            if !dir_entry.is_valid() {
                                let ctime = time_source.get_timestamp();
                                let entry = DirEntry::new(
                                    name,
                                    attributes,
                                    ClusterId(0),
                                    ctime,
                                    block_idx,
                                    (i * OnDiskDirEntry::LEN) as u32,
                                );
                                dir_entry_bytes
                                    .copy_from_slice(&entry.serialize(FatType::Fat32)[..]);
                                trace!("Updating directory");
                                block_cache.write_back().await?;
                                return Ok(entry);
                            }
                        }
                    }
                    current_cluster = match self.async_next_cluster(block_cache, cluster).await {
                        Ok(n) => {
                            first_dir_block_num = self.cluster_to_block(n);
                            Some(n)
                        }
                        Err(Error::EndOfFile) => {
                            let c = self
                                .async_alloc_cluster(block_cache, Some(cluster), true)
                                .await?;
                            first_dir_block_num = self.cluster_to_block(c);
                            Some(c)
                        }
                        _ => None,
                    };
                }
                Err(Error::NotEnoughSpace)
            }
        }
    }

    /// Calls callback `func` with every valid entry in the given directory (async version).
    ///
    /// Long File Names will be ignored.
    pub(crate) async fn async_iterate_dir<D, F>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        dir_info: &DirectoryInfo,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry) -> ControlFlow<()>,
        D: AsyncBlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                self.async_iterate_fat16(dir_info, fat16_info, block_cache, |de, _| func(de))
                    .await
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                self.async_iterate_fat32(dir_info, fat32_info, block_cache, |de, _| func(de))
                    .await
            }
        }
    }

    /// Calls callback `func` with every valid entry in the given directory, plus its ODDE
    /// (async version).
    async fn async_iterate_dir_internal<D, F>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        dir_info: &DirectoryInfo,
        func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry, &OnDiskDirEntry) -> ControlFlow<()>,
        D: AsyncBlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                self.async_iterate_fat16(dir_info, fat16_info, block_cache, func)
                    .await
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                self.async_iterate_fat32(dir_info, fat32_info, block_cache, func)
                    .await
            }
        }
    }

    /// Calls callback `func` with every valid entry in the given directory,
    /// including the Long File Name (async version).
    pub(crate) async fn async_iterate_dir_lfn<D, F>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        lfn_buffer: &mut LfnBuffer<'_>,
        dir_info: &DirectoryInfo,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry, Option<&str>) -> ControlFlow<()>,
        D: AsyncBlockDevice,
    {
        #[derive(Clone, Copy)]
        enum SeqState {
            Waiting,
            Remaining { csum: u8, next: u8 },
            Complete { csum: u8 },
        }

        impl SeqState {
            fn update(
                self,
                lfn_buffer: &mut LfnBuffer<'_>,
                start: bool,
                sequence: u8,
                csum: u8,
                buffer: [u16; 13],
            ) -> Self {
                #[cfg(feature = "log")]
                debug!("LFN Contents {start} {sequence} {csum:02x} {buffer:04x?}");
                #[cfg(feature = "defmt-log")]
                debug!(
                    "LFN Contents {=bool} {=u8} {=u8:02x} {=[?; 13]:#04x}",
                    start, sequence, csum, buffer
                );
                match (start, sequence, self) {
                    (true, 0x01, _) => {
                        lfn_buffer.clear();
                        lfn_buffer.push(&buffer);
                        SeqState::Complete { csum }
                    }
                    (true, sequence, _) if (0x02..0x14).contains(&sequence) => {
                        lfn_buffer.clear();
                        lfn_buffer.push(&buffer);
                        SeqState::Remaining {
                            csum,
                            next: sequence - 1,
                        }
                    }
                    (false, 0x01, SeqState::Remaining { csum, next }) if next == sequence => {
                        lfn_buffer.push(&buffer);
                        SeqState::Complete { csum }
                    }
                    (false, sequence, SeqState::Remaining { csum, next })
                        if (0x01..0x13).contains(&sequence) && next == sequence =>
                    {
                        lfn_buffer.push(&buffer);
                        SeqState::Remaining {
                            csum,
                            next: sequence - 1,
                        }
                    }
                    _ => {
                        lfn_buffer.clear();
                        SeqState::Waiting
                    }
                }
            }
        }

        let mut seq_state = SeqState::Waiting;
        self.async_iterate_dir_internal(block_cache, dir_info, |de, odde| {
            if let Some((start, this_seqno, csum, buffer)) = odde.lfn_contents() {
                seq_state = seq_state.update(lfn_buffer, start, this_seqno, csum, buffer);
                ControlFlow::Continue(())
            } else if let SeqState::Complete { csum } = seq_state {
                if csum == de.name.csum() {
                    func(de, Some(lfn_buffer.as_str()))
                } else {
                    func(de, None)
                }
            } else {
                func(de, None)
            }
        })
        .await
    }

    /// Calls callback `func` with every valid entry in the given FAT16 directory (async version).
    async fn async_iterate_fat16<D, F>(
        &self,
        dir_info: &DirectoryInfo,
        fat16_info: &Fat16Info,
        block_cache: &mut AsyncBlockCache<D>,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: for<'odde> FnMut(&DirEntry, &OnDiskDirEntry<'odde>) -> ControlFlow<()>,
        D: AsyncBlockDevice,
    {
        let mut current_cluster = Some(dir_info.cluster);
        let mut first_dir_block_num = match dir_info.cluster {
            ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
            _ => self.cluster_to_block(dir_info.cluster),
        };
        let dir_size = match dir_info.cluster {
            ClusterId::ROOT_DIR => {
                let len_bytes = u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                BlockCount::from_bytes(len_bytes)
            }
            _ => BlockCount(u32::from(self.blocks_per_cluster)),
        };

        'outer: while let Some(cluster) = current_cluster {
            for block_idx in first_dir_block_num.range(dir_size) {
                trace!("Reading FAT");
                let block = block_cache.read(block_idx).await?;
                for (i, dir_entry_bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
                    let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                    if dir_entry.is_end() {
                        break 'outer;
                    } else if dir_entry.is_valid() {
                        let start = (i * OnDiskDirEntry::LEN) as u32;
                        let entry = dir_entry.get_entry(FatType::Fat16, block_idx, start);
                        if func(&entry, &dir_entry) == ControlFlow::Break(()) {
                            break 'outer;
                        }
                    }
                }
            }
            if cluster != ClusterId::ROOT_DIR {
                current_cluster = match self.async_next_cluster(block_cache, cluster).await {
                    Ok(n) => {
                        first_dir_block_num = self.cluster_to_block(n);
                        Some(n)
                    }
                    _ => None,
                };
            } else {
                current_cluster = None;
            }
        }
        Ok(())
    }

    /// Calls callback `func` with every valid entry in the given FAT32 directory (async version).
    async fn async_iterate_fat32<D, F>(
        &self,
        dir_info: &DirectoryInfo,
        fat32_info: &Fat32Info,
        block_cache: &mut AsyncBlockCache<D>,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: for<'odde> FnMut(&DirEntry, &OnDiskDirEntry<'odde>) -> ControlFlow<()>,
        D: AsyncBlockDevice,
    {
        let mut current_cluster = match dir_info.cluster {
            ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
            _ => Some(dir_info.cluster),
        };
        'outer: while let Some(cluster) = current_cluster {
            let start_block_idx = self.cluster_to_block(cluster);
            for block_idx in start_block_idx.range(BlockCount(u32::from(self.blocks_per_cluster))) {
                trace!("Reading FAT");
                let block = block_cache
                    .read(block_idx)
                    .await
                    .map_err(Error::DeviceError)?;
                for (i, dir_entry_bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
                    let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                    if dir_entry.is_end() {
                        break 'outer;
                    } else if dir_entry.is_valid() {
                        let start = (i * OnDiskDirEntry::LEN) as u32;
                        let entry = dir_entry.get_entry(FatType::Fat32, block_idx, start);
                        if let ControlFlow::Break(_) = func(&entry, &dir_entry) {
                            break 'outer;
                        }
                    }
                }
            }
            current_cluster = self.async_next_cluster(block_cache, cluster).await.ok();
        }
        Ok(())
    }

    /// Get an entry from the given directory (async version).
    pub(crate) async fn async_find_directory_entry<D>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &ShortFileName,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        let mut result = Err(Error::NotFound);
        self.async_iterate_dir(block_cache, dir_info, |de| {
            if de.name == *match_name {
                result = Ok(de.clone());
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await?;
        result
    }

    /// Get an entry from the given directory by long file name (async version).
    pub(crate) async fn async_find_directory_entry_by_lfn<D>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &str,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        let mut result = Err(Error::NotFound);
        enum SeqState<'a> {
            Waiting,
            Scanning {
                remaining: &'a str,
                sequence: u8,
                csum: u8,
            },
            Found {
                csum: u8,
            },
        }

        let mut state = SeqState::Waiting;
        self.async_iterate_dir_internal(block_cache, dir_info, |de, odde| {
            match state {
                SeqState::Waiting => {
                    debug!("Am waiting for LFN start");
                    let mut remaining = match_name;
                    if let Some((true, sequence, csum, buffer)) = odde.lfn_contents() {
                        #[cfg(feature = "defmt-log")]
                        debug!("{:02x} {:02x} {:04x}", sequence, csum, buffer);
                        #[cfg(feature = "log")]
                        debug!("{:02x} {:02x} {:04x?}", sequence, csum, buffer);
                        for word in buffer
                            .iter()
                            .rev()
                            .skip_while(|b| **b == 0xFFFF)
                            .skip_while(|b| **b == 0x0000)
                        {
                            debug!("Looking at word {:04x}", *word);
                            let Some(c) = char::from_u32(*word as u32) else {
                                return ControlFlow::Continue(());
                            };
                            debug!("Looking at char '{}'", c);
                            let Some(r) = remaining.strip_suffix(c) else {
                                debug!("No, didn't want that");
                                return ControlFlow::Continue(());
                            };
                            debug!("Liked it! {:?} is left", r);
                            remaining = r;
                        }
                        if sequence == 1 {
                            if remaining.is_empty() {
                                state = SeqState::Found { csum }
                            } else {
                                state = SeqState::Waiting
                            }
                        } else {
                            state = SeqState::Scanning {
                                remaining,
                                sequence: sequence - 1,
                                csum,
                            };
                        }
                    }
                }
                SeqState::Scanning {
                    remaining,
                    sequence,
                    csum,
                } => {
                    debug!(
                        "Am waiting for more LFN sequence={:02x}, csum={:02x}",
                        sequence, csum
                    );
                    let mut remaining = remaining;
                    if let Some((false, this_sequence, this_csum, buffer)) = odde.lfn_contents() {
                        #[cfg(feature = "defmt-log")]
                        debug!("{:02x} {:02x} {:04x}", sequence, csum, buffer);
                        #[cfg(feature = "log")]
                        debug!("{:02x} {:02x} {:04x?}", sequence, csum, buffer);
                        if (this_sequence != sequence) || (this_csum != csum) {
                            debug!(
                                "No! Got sequence={:02x}, csum={:02x}",
                                this_sequence, this_csum
                            );
                            state = SeqState::Waiting;
                            return ControlFlow::Continue(());
                        }
                        for word in buffer.iter().rev() {
                            debug!("Looking at word {:04x}", *word);
                            let Some(c) = char::from_u32(*word as u32) else {
                                return ControlFlow::Continue(());
                            };
                            debug!("Looking at char '{}'", c);
                            let Some(r) = remaining.strip_suffix(c) else {
                                debug!("No, didn't want that");
                                return ControlFlow::Continue(());
                            };
                            debug!("Liked it! {:?} is left", r);
                            remaining = r;
                        }
                        if sequence == 1 {
                            if remaining.is_empty() {
                                state = SeqState::Found { csum }
                            } else {
                                state = SeqState::Waiting
                            }
                        } else {
                            state = SeqState::Scanning {
                                remaining,
                                sequence: sequence - 1,
                                csum,
                            };
                        }
                    }
                }
                SeqState::Found { csum } => {
                    let calc_csum = de.name.csum();
                    if calc_csum == csum {
                        result = Ok(de.clone());
                        return ControlFlow::Break(());
                    } else {
                        debug!("Bad csum {:02x} != {:02x}", calc_csum, csum);
                    }
                }
            }
            ControlFlow::Continue(())
        })
        .await?;
        result
    }

    /// Delete an entry from the given directory (async version).
    pub(crate) async fn async_delete_directory_entry<D>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &ShortFileName,
    ) -> Result<(), Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                let mut current_cluster = Some(dir_info.cluster);
                let mut first_dir_block_num = match dir_info.cluster {
                    ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
                    _ => self.cluster_to_block(dir_info.cluster),
                };
                let dir_size = match dir_info.cluster {
                    ClusterId::ROOT_DIR => {
                        let len_bytes =
                            u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                        BlockCount::from_bytes(len_bytes)
                    }
                    _ => BlockCount(u32::from(self.blocks_per_cluster)),
                };

                while let Some(cluster) = current_cluster {
                    for block_idx in first_dir_block_num.range(dir_size) {
                        match self
                            .async_delete_entry_in_block(block_cache, match_name, block_idx)
                            .await
                        {
                            Err(Error::NotFound) => {
                                // Carry on
                            }
                            x => {
                                return x;
                            }
                        }
                    }
                    if cluster != ClusterId::ROOT_DIR {
                        current_cluster = match self.async_next_cluster(block_cache, cluster).await
                        {
                            Ok(n) => {
                                first_dir_block_num = self.cluster_to_block(n);
                                Some(n)
                            }
                            _ => None,
                        };
                    } else {
                        current_cluster = None;
                    }
                }
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                let mut current_cluster = match dir_info.cluster {
                    ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
                    _ => Some(dir_info.cluster),
                };
                while let Some(cluster) = current_cluster {
                    let start_block_idx = self.cluster_to_block(cluster);
                    for block_idx in
                        start_block_idx.range(BlockCount(u32::from(self.blocks_per_cluster)))
                    {
                        match self
                            .async_delete_entry_in_block(block_cache, match_name, block_idx)
                            .await
                        {
                            Err(Error::NotFound) => {
                                continue;
                            }
                            x => {
                                return x;
                            }
                        }
                    }
                    current_cluster = self.async_next_cluster(block_cache, cluster).await.ok();
                }
            }
        }
        Err(Error::NotFound)
    }

    /// Deletes a directory entry from a block of directory entries (async version).
    async fn async_delete_entry_in_block<D>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        match_name: &ShortFileName,
        block_idx: BlockIdx,
    ) -> Result<(), Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        trace!("Reading directory");
        let block = block_cache
            .read_mut(block_idx)
            .await
            .map_err(Error::DeviceError)?;
        for (i, dir_entry_bytes) in block.chunks_exact_mut(OnDiskDirEntry::LEN).enumerate() {
            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
            if dir_entry.is_end() {
                break;
            } else if dir_entry.matches(match_name) {
                let start = i * OnDiskDirEntry::LEN;
                block[start] = 0xE5;
                trace!("Updating directory");
                return block_cache.write_back().await.map_err(Error::DeviceError);
            }
        }
        Err(Error::NotFound)
    }

    /// Finds the next free cluster after the start_cluster and before end_cluster (async version).
    pub(crate) async fn async_find_next_free_cluster<D>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        start_cluster: ClusterId,
        end_cluster: ClusterId,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        let mut current_cluster = start_cluster;
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                while current_cluster.0 < end_cluster.0 {
                    trace!(
                        "current_cluster={:?}, end_cluster={:?}",
                        current_cluster, end_cluster
                    );
                    let fat_offset = current_cluster.0 * 2;
                    trace!("fat_offset = {:?}", fat_offset);
                    let this_fat_block_num =
                        self.lba_start + self.fat_start.offset_bytes(fat_offset);
                    trace!("this_fat_block_num = {:?}", this_fat_block_num);
                    let mut this_fat_ent_offset = usize::try_from(fat_offset % Block::LEN_U32)
                        .map_err(|_| Error::ConversionError)?;
                    trace!("Reading block {:?}", this_fat_block_num);
                    let block = block_cache
                        .read(this_fat_block_num)
                        .await
                        .map_err(Error::DeviceError)?;
                    while this_fat_ent_offset <= Block::LEN - 2 {
                        let fat_entry = LittleEndian::read_u16(
                            &block[this_fat_ent_offset..=this_fat_ent_offset + 1],
                        );
                        if fat_entry == 0 {
                            return Ok(current_cluster);
                        }
                        this_fat_ent_offset += 2;
                        current_cluster += 1;
                    }
                }
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                while current_cluster.0 < end_cluster.0 {
                    trace!(
                        "current_cluster={:?}, end_cluster={:?}",
                        current_cluster, end_cluster
                    );
                    let fat_offset = current_cluster.0 * 4;
                    trace!("fat_offset = {:?}", fat_offset);
                    let this_fat_block_num =
                        self.lba_start + self.fat_start.offset_bytes(fat_offset);
                    trace!("this_fat_block_num = {:?}", this_fat_block_num);
                    let mut this_fat_ent_offset = usize::try_from(fat_offset % Block::LEN_U32)
                        .map_err(|_| Error::ConversionError)?;
                    trace!("Reading block {:?}", this_fat_block_num);
                    let block = block_cache
                        .read(this_fat_block_num)
                        .await
                        .map_err(Error::DeviceError)?;
                    while this_fat_ent_offset <= Block::LEN - 4 {
                        let fat_entry = LittleEndian::read_u32(
                            &block[this_fat_ent_offset..=this_fat_ent_offset + 3],
                        ) & 0x0FFF_FFFF;
                        if fat_entry == 0 {
                            return Ok(current_cluster);
                        }
                        this_fat_ent_offset += 4;
                        current_cluster += 1;
                    }
                }
            }
        }
        warn!("Out of space...");
        Err(Error::NotEnoughSpace)
    }

    /// Tries to allocate a cluster (async version).
    pub(crate) async fn async_alloc_cluster<D>(
        &mut self,
        block_cache: &mut AsyncBlockCache<D>,
        prev_cluster: Option<ClusterId>,
        zero: bool,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        debug!("Allocating new cluster, prev_cluster={:?}", prev_cluster);
        let end_cluster = ClusterId(self.cluster_count + RESERVED_ENTRIES);
        let start_cluster = match self.next_free_cluster {
            Some(cluster) if cluster.0 < end_cluster.0 => cluster,
            _ => ClusterId(RESERVED_ENTRIES),
        };
        trace!(
            "Finding next free between {:?}..={:?}",
            start_cluster, end_cluster
        );
        let new_cluster = match self
            .async_find_next_free_cluster(block_cache, start_cluster, end_cluster)
            .await
        {
            Ok(cluster) => cluster,
            Err(_) if start_cluster.0 > RESERVED_ENTRIES => {
                debug!(
                    "Retrying, finding next free between {:?}..={:?}",
                    ClusterId(RESERVED_ENTRIES),
                    end_cluster
                );
                self.async_find_next_free_cluster(
                    block_cache,
                    ClusterId(RESERVED_ENTRIES),
                    end_cluster,
                )
                .await?
            }
            Err(e) => return Err(e),
        };
        self.async_update_fat(block_cache, new_cluster, ClusterId::END_OF_FILE)
            .await?;
        if let Some(cluster) = prev_cluster {
            trace!(
                "Updating old cluster {:?} to {:?} in FAT",
                cluster, new_cluster
            );
            self.async_update_fat(block_cache, cluster, new_cluster)
                .await?;
        }
        trace!(
            "Finding next free between {:?}..={:?}",
            new_cluster, end_cluster
        );
        self.next_free_cluster = match self
            .async_find_next_free_cluster(block_cache, new_cluster, end_cluster)
            .await
        {
            Ok(cluster) => Some(cluster),
            Err(_) if new_cluster.0 > RESERVED_ENTRIES => {
                match self
                    .async_find_next_free_cluster(
                        block_cache,
                        ClusterId(RESERVED_ENTRIES),
                        end_cluster,
                    )
                    .await
                {
                    Ok(cluster) => Some(cluster),
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        debug!("Next free cluster is {:?}", self.next_free_cluster);
        if let Some(ref mut number_free_cluster) = self.free_clusters_count {
            *number_free_cluster -= 1;
        };
        if zero {
            let start_block_idx = self.cluster_to_block(new_cluster);
            let num_blocks = BlockCount(u32::from(self.blocks_per_cluster));
            for block_idx in start_block_idx.range(num_blocks) {
                trace!("Zeroing cluster {:?}", block_idx);
                let _block = block_cache.blank_mut(block_idx);
                block_cache.write_back().await?;
            }
        }
        debug!("All done, returning {:?}", new_cluster);
        Ok(new_cluster)
    }

    /// Marks the input cluster as an EOF and all the subsequent clusters in the chain as free
    /// (async version).
    pub(crate) async fn async_truncate_cluster_chain<D>(
        &mut self,
        block_cache: &mut AsyncBlockCache<D>,
        cluster: ClusterId,
    ) -> Result<(), Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        if cluster.0 < RESERVED_ENTRIES {
            return Ok(());
        }
        let mut next = {
            match self.async_next_cluster(block_cache, cluster).await {
                Ok(n) => n,
                Err(Error::EndOfFile) => return Ok(()),
                Err(e) => return Err(e),
            }
        };
        if let Some(ref mut next_free_cluster) = self.next_free_cluster {
            if next_free_cluster.0 > next.0 {
                *next_free_cluster = next;
            }
        } else {
            self.next_free_cluster = Some(next);
        }
        self.async_update_fat(block_cache, cluster, ClusterId::END_OF_FILE)
            .await?;
        loop {
            match self.async_next_cluster(block_cache, next).await {
                Ok(n) => {
                    self.async_update_fat(block_cache, next, ClusterId::EMPTY)
                        .await?;
                    next = n;
                }
                Err(Error::EndOfFile) => {
                    self.async_update_fat(block_cache, next, ClusterId::EMPTY)
                        .await?;
                    break;
                }
                Err(e) => return Err(e),
            }
            if let Some(ref mut number_free_cluster) = self.free_clusters_count {
                *number_free_cluster += 1;
            };
        }
        Ok(())
    }

    /// Writes a Directory Entry to the disk (async version).
    pub(crate) async fn async_write_entry_to_disk<D>(
        &self,
        block_cache: &mut AsyncBlockCache<D>,
        entry: &DirEntry,
    ) -> Result<(), Error<D::Error>>
    where
        D: AsyncBlockDevice,
    {
        let fat_type = match self.fat_specific_info {
            FatSpecificInfo::Fat16(_) => FatType::Fat16,
            FatSpecificInfo::Fat32(_) => FatType::Fat32,
        };
        trace!("Reading directory for update");
        let block = block_cache
            .read_mut(entry.entry_block)
            .await
            .map_err(Error::DeviceError)?;

        let start = usize::try_from(entry.entry_offset).map_err(|_| Error::ConversionError)?;
        block[start..start + 32].copy_from_slice(&entry.serialize(fat_type)[..]);

        trace!("Updating directory");
        block_cache.write_back().await.map_err(Error::DeviceError)?;
        Ok(())
    }

    /// Create a new directory (async version).
    ///
    /// 1) Creates the directory entry in the parent
    /// 2) Allocates a new cluster to hold the new directory
    /// 3) Writes out the `.` and `..` entries in the new directory
    pub(crate) async fn async_make_dir<D, T>(
        &mut self,
        block_cache: &mut AsyncBlockCache<D>,
        time_source: &T,
        parent: ClusterId,
        sfn: ShortFileName,
        att: Attributes,
    ) -> Result<(), Error<D::Error>>
    where
        D: AsyncBlockDevice,
        T: TimeSource,
    {
        let mut new_dir_entry_in_parent = self
            .async_write_new_directory_entry(block_cache, time_source, parent, sfn, att)
            .await?;
        if new_dir_entry_in_parent.cluster == ClusterId::EMPTY {
            new_dir_entry_in_parent.cluster =
                self.async_alloc_cluster(block_cache, None, false).await?;
            self.async_write_entry_to_disk(block_cache, &new_dir_entry_in_parent)
                .await?;
        }
        let new_dir_start_block = self.cluster_to_block(new_dir_entry_in_parent.cluster);
        debug!("Made new dir entry {:?}", new_dir_entry_in_parent);
        let now = time_source.get_timestamp();
        let fat_type = self.get_fat_type();
        // A blank block
        let block = block_cache.blank_mut(new_dir_start_block);
        // make the "." entry
        let dot_entry_in_child = DirEntry {
            name: crate::ShortFileName::this_dir(),
            mtime: now,
            ctime: now,
            attributes: att,
            cluster: new_dir_entry_in_parent.cluster,
            size: 0,
            entry_block: new_dir_start_block,
            entry_offset: 0,
        };
        debug!("New dir has {:?}", dot_entry_in_child);
        let mut offset = 0;
        block[offset..offset + OnDiskDirEntry::LEN]
            .copy_from_slice(&dot_entry_in_child.serialize(fat_type)[..]);
        offset += OnDiskDirEntry::LEN;
        // make the ".." entry
        let dot_dot_entry_in_child = DirEntry {
            name: crate::ShortFileName::parent_dir(),
            mtime: now,
            ctime: now,
            attributes: att,
            cluster: if parent == ClusterId::ROOT_DIR {
                ClusterId::EMPTY
            } else {
                parent
            },
            size: 0,
            entry_block: new_dir_start_block,
            entry_offset: OnDiskDirEntry::LEN_U32,
        };
        debug!("New dir has {:?}", dot_dot_entry_in_child);
        block[offset..offset + OnDiskDirEntry::LEN]
            .copy_from_slice(&dot_dot_entry_in_child.serialize(fat_type)[..]);

        block_cache.write_back().await?;

        for block_idx in new_dir_start_block
            .range(BlockCount(u32::from(self.blocks_per_cluster)))
            .skip(1)
        {
            let _block = block_cache.blank_mut(block_idx);
            block_cache.write_back().await?;
        }

        Ok(())
    }
}

/// Load the boot parameter block from the start of the given partition and
/// determine if the partition contains a valid FAT16 or FAT32 file system
/// (async version).
pub async fn async_parse_volume<D>(
    block_cache: &mut AsyncBlockCache<D>,
    lba_start: BlockIdx,
    num_blocks: BlockCount,
) -> Result<VolumeType, Error<D::Error>>
where
    D: AsyncBlockDevice,
    D::Error: core::fmt::Debug,
{
    trace!("Reading BPB");
    let block = block_cache
        .read(lba_start)
        .await
        .map_err(Error::DeviceError)?;
    let bpb = Bpb::create_from_bytes(block).map_err(Error::FormatError)?;
    let fat_start = BlockCount(u32::from(bpb.reserved_block_count()));
    let second_fat_start = if bpb.num_fats() == 2 {
        Some(fat_start + BlockCount(bpb.fat_size()))
    } else {
        None
    };
    match bpb.fat_type {
        FatType::Fat16 => {
            if bpb.bytes_per_block() as usize != Block::LEN {
                return Err(Error::BadBlockSize(bpb.bytes_per_block()));
            }
            let root_dir_blocks = (u32::from(bpb.root_entries_count()) * OnDiskDirEntry::LEN_U32)
                .div_ceil(Block::LEN_U32);
            let first_root_dir_block =
                fat_start + BlockCount(u32::from(bpb.num_fats()) * bpb.fat_size());
            let first_data_block = first_root_dir_block + BlockCount(root_dir_blocks);
            let volume = FatVolume {
                lba_start,
                num_blocks,
                name: super::volume::VolumeName {
                    contents: bpb.volume_label(),
                },
                blocks_per_cluster: bpb.blocks_per_cluster(),
                first_data_block,
                fat_start,
                second_fat_start,
                free_clusters_count: None,
                next_free_cluster: None,
                cluster_count: bpb.total_clusters(),
                fat_specific_info: FatSpecificInfo::Fat16(Fat16Info {
                    root_entries_count: bpb.root_entries_count(),
                    first_root_dir_block,
                }),
            };
            Ok(VolumeType::Fat(volume))
        }
        FatType::Fat32 => {
            let first_data_block =
                fat_start + BlockCount(u32::from(bpb.num_fats()) * bpb.fat_size());
            let info_location = bpb.fs_info_block().unwrap();
            let mut volume = FatVolume {
                lba_start,
                num_blocks,
                name: super::volume::VolumeName {
                    contents: bpb.volume_label(),
                },
                blocks_per_cluster: bpb.blocks_per_cluster(),
                first_data_block,
                fat_start,
                second_fat_start,
                free_clusters_count: None,
                next_free_cluster: None,
                cluster_count: bpb.total_clusters(),
                fat_specific_info: FatSpecificInfo::Fat32(Fat32Info {
                    info_location: lba_start + info_location,
                    first_root_dir_cluster: ClusterId(bpb.first_root_dir_cluster()),
                }),
            };

            trace!("Reading info block");
            let info_block = block_cache
                .read(lba_start + info_location)
                .await
                .map_err(Error::DeviceError)?;
            let info_sector =
                InfoSector::create_from_bytes(info_block).map_err(Error::FormatError)?;
            volume.free_clusters_count = info_sector.free_clusters_count();
            volume.next_free_cluster = info_sector.next_free_cluster();

            Ok(VolumeType::Fat(volume))
        }
    }
}
