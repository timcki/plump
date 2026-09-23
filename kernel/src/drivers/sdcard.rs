// sd card over SPI: sync SdCard + sync volume manager
//
// SdCard handles the SD protocol (CMD0, init, sector I/O) over the
// shared blocking SPI bus; VolumeManager keeps its state behind its
// own RefCell and takes &self, so an open file handle holds no borrow
// and several can be in use at once

use core::cell::{Cell, RefCell};
use embedded_hal::delay::DelayNs;

use embedded_sdmmc::{
    RawDirectory, RawVolume, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager,
};
use log::info;

use crate::board::SdSpiDevice;
use crate::util::FixedStr;

// no RTC on this board

pub(crate) struct NullTimeSource;

impl TimeSource for NullTimeSource {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 0,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

// type aliases

pub type SyncSdCard = SdCard<SdSpiDevice, esp_hal::delay::Delay>;
// directory budget: root + data dir + cached subdirs + one transient
// open for Scope::Named or an uncached subdir
pub(crate) type VolMgr = VolumeManager<SyncSdCard, NullTimeSource, 8, 4, 1>;

// subdirectory handles kept open alongside the data dir (BOOKS,
// STATS, ...); names longer than the FixedStr are never cached
pub(crate) const SUB_HANDLES: usize = 4;

// persistent volume manager state; every field is borrowed only for
// the length of one call, never across a caller's closure

pub(crate) struct SdStorageInner {
    pub(crate) mgr: VolMgr,
    pub(crate) vol: RawVolume,
    pub(crate) root: RawDirectory,
    /// Resolved data directory name ("_PLUMP" or legacy "_PULP").
    pub(crate) data_dir: Cell<&'static str>,
    /// data directory handle, opened on first use and kept for the
    /// device lifetime: every directory open scans its parent on SD
    pub(crate) data_handle: Cell<Option<RawDirectory>>,
    /// subdirectories of the data dir, same lifetime, keyed by name
    pub(crate) sub_handles: RefCell<[Option<(FixedStr<12>, RawDirectory)>; SUB_HANDLES]>,
}

// holds a persistently-mounted VolumeManager with volume 0 and the
// root directory kept open for the device lifetime

pub struct SdStorage {
    inner: Option<SdStorageInner>,
}

impl SdStorage {
    pub fn empty() -> Self {
        Self { inner: None }
    }

    // init SD card at 400 kHz (SD spec init frequency)
    //
    // sync SdCard auto-initialises on first method call; we call
    // num_bytes() to force init and verify the card responds
    //
    // pub so Board::init can run this before other SPI peripherals
    // touch the bus - SD spec requires a clean 400 kHz bus for CMD0
    pub fn init_card(spi_device: SdSpiDevice) -> Option<SyncSdCard> {
        let sd = SdCard::new(spi_device, esp_hal::delay::Delay::new());

        for attempt in 1..=5 {
            match sd.num_bytes() {
                Ok(size) => {
                    info!("SD card: initialised (attempt {})", attempt);
                    info!("SD card: {} bytes ({} MB)", size, size / 1024 / 1024);
                    return Some(sd);
                }
                Err(e) => {
                    info!("SD card: init attempt {} failed: {:?}", attempt, e);
                    sd.mark_card_uninit();
                    esp_hal::delay::Delay::new().delay_ms(50);
                }
            }
        }

        info!("SD card: all init attempts failed");
        None
    }

    // mount FAT filesystem on an already-initialised SD card
    //
    // opens volume 0 (first MBR partition) and keeps the root
    // directory open for the device lifetime
    pub fn mount(sd: SyncSdCard) -> Self {
        let mgr: VolMgr = VolumeManager::new_with_limits(sd, NullTimeSource, 5000);

        let vol = match mgr.open_raw_volume(VolumeIdx(0)) {
            Ok(v) => v,
            Err(e) => {
                info!("SD card: open volume failed: {}", e);
                return Self { inner: None };
            }
        };

        let root = match mgr.open_root_dir(vol) {
            Ok(d) => d,
            Err(e) => {
                info!("SD card: open root dir failed: {}", e);
                let _ = mgr.close_volume(vol);
                return Self { inner: None };
            }
        };

        info!("SD card: filesystem mounted");
        Self {
            inner: Some(SdStorageInner {
                mgr,
                vol,
                root,
                data_dir: Cell::new(crate::drivers::storage::PLUMP_DIR),
                data_handle: Cell::new(None),
                sub_handles: RefCell::new([None; SUB_HANDLES]),
            }),
        }
    }

    #[inline]
    pub fn probe_ok(&self) -> bool {
        self.inner.is_some()
    }

    /// Return the resolved data directory name (`"_PLUMP"` or legacy `"_PULP"`).
    #[inline]
    pub fn data_dir(&self) -> &'static str {
        match &self.inner {
            Some(inner) => inner.data_dir.get(),
            None => crate::drivers::storage::PLUMP_DIR,
        }
    }

    #[inline]
    pub(crate) fn inner(&self) -> Option<&SdStorageInner> {
        self.inner.as_ref()
    }

    // flush pending writes and close fat handles; best-effort before halt.
    // after this call no further sd i/o is possible until mcu reset
    pub fn flush_and_close(&self) {
        if let Some(inner) = &self.inner {
            inner.drop_dir_handles();
            let _ = inner.mgr.close_dir(inner.root);
            let _ = inner.mgr.close_volume(inner.vol);
        }
    }
}
