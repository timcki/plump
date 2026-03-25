// RTC FAST memory session persistence
//
// stores session state in RTC FAST memory (8KB on ESP32-C3) which
// survives deep sleep but is zeroed on power-on reset. this enables
// instant wake restoration without SD card I/O.
//
// the session struct is placed in .rtc_fast.persistent via link_section;
// esp-hal's linker scripts ensure this memory is only zeroed on power-on,
// not on deep sleep wake.

use core::sync::atomic::{AtomicU32, Ordering};

// magic value to validate RTC session data: "PLPS" (PuLP Session)
const RTC_SESSION_MAGIC: u32 = 0x504C5053;

// max navigation stack depth (must match app::MAX_STACK_DEPTH)
pub const MAX_NAV_STACK: usize = 4;

// max filename length for reader state
pub const MAX_FILENAME_LEN: usize = 32;

// RTC-persistent session state
// stored in RTC FAST memory; survives deep sleep
// all fields use fixed-size types for stable memory layout
#[derive(Clone, Copy)]
#[repr(C, align(4))]
pub struct RtcSession {
    // header (16 bytes)
    magic: u32,      // must equal RTC_SESSION_MAGIC for valid data
    wake_count: u32, // incremented each successful wake
    flags: u32,      // reserved
    _header_pad: u32,

    // navigation stack (8 bytes)
    pub nav_depth: u8,                  // stack depth (1-4)
    pub nav_stack: [u8; MAX_NAV_STACK], // app ids: Home=0, Files=1, Reader=2, Settings=3, Upload=4
    _nav_pad: [u8; 3],

    // reader state (48 bytes)
    pub reader_filename: [u8; MAX_FILENAME_LEN],
    pub reader_filename_len: u8,
    pub reader_is_epub: u8,
    pub reader_chapter: u16,
    pub reader_page: u16,
    pub reader_byte_offset: u32,
    pub reader_font_size: u8,
    _reader_pad: [u8; 5],

    // files state (8 bytes)
    pub files_scroll: u16,
    pub files_selected: u8,
    pub files_total: u16,
    _files_pad: [u8; 3],

    // home state (8 bytes)
    pub home_state: u8, // 0=Menu, 1=ShowBookmarks
    pub home_selected: u8,
    pub home_bm_selected: u8,
    pub home_bm_scroll: u8,
    _home_pad: [u8; 4],

    // settings cache (16 bytes) - avoid SD reads on wake
    pub settings_sleep_timeout: u16,
    pub settings_ghost_clear: u8,
    pub settings_book_font: u8,
    pub settings_ui_font: u8,
    pub settings_valid: u8,
    _settings_pad: [u8; 10],

    // reserved (24 bytes)
    _reserved: [u8; 24],
}

// Compile-time size check: ensure struct fits comfortably in RTC FAST (8KB)
// and doesn't grow unexpectedly
const _: () = assert!(core::mem::size_of::<RtcSession>() <= 256);

impl RtcSession {
    // create zeroed session (invalid until populated)
    //
    // Safety: RtcSession is #[repr(C)] with only primitive types and
    // fixed-size arrays of primitives - all valid when zero-initialized
    pub const fn zeroed() -> Self {
        // SAFETY: All fields are primitives or arrays of primitives,
        // and zero is a valid value for each (magic=0 means invalid session)
        unsafe { core::mem::zeroed() }
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == RTC_SESSION_MAGIC
    }

    #[inline]
    pub fn mark_valid(&mut self) {
        self.magic = RTC_SESSION_MAGIC;
    }

    pub fn clear(&mut self) {
        self.magic = 0;
    }

    pub fn increment_wake_count(&mut self) {
        self.wake_count = self.wake_count.wrapping_add(1);
    }

    #[inline]
    pub fn wake_count(&self) -> u32 {
        self.wake_count
    }
}

// RTC FAST persistent storage
//
// This static is placed in .rtc_fast.persistent section which:
// - Survives deep sleep (RTC domain stays powered)
// - Is zeroed only on power-on reset (not deep sleep wake)
// - Requires RTC FAST memory to remain powered during sleep
//
// Safety: Access is through save()/load() which use volatile operations
// and are only called from single-threaded boot/sleep contexts.
#[unsafe(link_section = ".rtc_fast.persistent")]
static mut RTC_SESSION: RtcSession = RtcSession::zeroed();

// Atomic flag to track if we've detected a valid session this boot
// (prevents re-reading stale data after initial restore)
static SESSION_CONSUMED: AtomicU32 = AtomicU32::new(0);

// peek at RTC session validity without consuming it.
// safe to call multiple times (e.g. from main before boot console,
// then again during boot()). does NOT prevent subsequent restore.
pub fn peek_valid() -> bool {
    // Safety: single-threaded boot context, volatile read via raw pointer
    let magic = unsafe {
        let ptr = core::ptr::addr_of!(RTC_SESSION);
        core::ptr::read_volatile(core::ptr::addr_of!((*ptr).magic))
    };
    magic == RTC_SESSION_MAGIC
}

// check if RTC session data is valid and available for restore
// returns true only once per boot (subsequent calls return false)
// must be called from main thread during boot, before async tasks
pub fn is_valid_session() -> bool {
    // Only allow one restore per boot
    if SESSION_CONSUMED.load(Ordering::Relaxed) != 0 {
        return false;
    }

    // Safety: single-threaded boot context, volatile read via raw pointer
    let valid = unsafe {
        let ptr = core::ptr::addr_of!(RTC_SESSION);
        core::ptr::read_volatile(core::ptr::addr_of!((*ptr).magic))
    } == RTC_SESSION_MAGIC;

    if valid {
        SESSION_CONSUMED.store(1, Ordering::Relaxed);
    }

    valid
}

// load session data (caller should check is_valid_session() first)
// must be called from main thread during boot
pub fn load() -> RtcSession {
    // Safety: single-threaded context, volatile read via raw pointer
    unsafe {
        let ptr = core::ptr::addr_of!(RTC_SESSION);
        core::ptr::read_volatile(ptr)
    }
}

// save session data before entering deep sleep
// must be called from main thread before sleep, after tasks stopped
pub fn save(session: &RtcSession) {
    unsafe {
        let ptr = core::ptr::addr_of_mut!(RTC_SESSION);
        // Copy all fields
        core::ptr::write_volatile(ptr, *session);
        // Ensure magic is set (caller may have forgotten)
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).magic), RTC_SESSION_MAGIC);
    }
}

// clear RTC session data
pub fn clear() {
    unsafe {
        let ptr = core::ptr::addr_of_mut!(RTC_SESSION);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).magic), 0);
    }
}

// get wake count for debugging (doesn't consume session)
pub fn wake_count() -> u32 {
    unsafe {
        let ptr = core::ptr::addr_of!(RTC_SESSION);
        core::ptr::read_volatile(core::ptr::addr_of!((*ptr).wake_count))
    }
}

// --- SD-based session persistence (fallback for battery wake) ---
//
// on battery, the brownout detector fires during the deep sleep wake
// voltage sag, causing a full system reset that wipes RTC FAST memory.
// we save the session to SD as a fallback so it can be restored even
// when RTC data is lost.

pub const SESSION_FILE: &str = "SESSION.BIN";

// size of the raw session data on SD (must match struct size)
const SESSION_SIZE: usize = core::mem::size_of::<RtcSession>();

// save session to SD card as raw bytes
pub fn save_to_sd(session: &RtcSession, sd: &crate::drivers::sdcard::SdStorage) {
    // safety: RtcSession is #[repr(C)] with only primitive types;
    // reinterpreting as bytes is well-defined
    let bytes: &[u8] = unsafe {
        core::slice::from_raw_parts(session as *const RtcSession as *const u8, SESSION_SIZE)
    };
    match crate::drivers::storage::write_in_pulp(sd, SESSION_FILE, bytes) {
        Ok(()) => log::info!("session: saved to SD ({} bytes)", SESSION_SIZE),
        Err(e) => log::warn!("session: SD save failed: {}", e),
    }
}

// load session from SD card; returns Some if valid
pub fn load_from_sd(sd: &crate::drivers::sdcard::SdStorage) -> Option<RtcSession> {
    // align the read buffer to match RtcSession's align(4) requirement;
    // a plain [u8] has align 1 which causes UB with ptr::read on RISC-V
    #[repr(C, align(4))]
    struct AlignedBuf([u8; SESSION_SIZE]);
    let mut buf = AlignedBuf([0u8; SESSION_SIZE]);

    match crate::drivers::storage::read_chunk_in_pulp(sd, SESSION_FILE, 0, &mut buf.0) {
        Ok(n) if n >= SESSION_SIZE => {
            // safety: buf is properly aligned (align 4) and contains
            // SESSION_SIZE bytes with the same layout as RtcSession
            let session: RtcSession =
                unsafe { core::ptr::read(buf.0.as_ptr() as *const RtcSession) };
            if session.is_valid() {
                log::info!(
                    "session: loaded from SD (wake count {})",
                    session.wake_count()
                );
                Some(session)
            } else {
                log::warn!(
                    "session: SD file magic {:08x} != {:08x} (read {} bytes, first 8: {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x})",
                    session.magic,
                    RTC_SESSION_MAGIC,
                    n,
                    buf.0[0], buf.0[1], buf.0[2], buf.0[3],
                    buf.0[4], buf.0[5], buf.0[6], buf.0[7],
                );
                None
            }
        }
        Ok(n) => {
            log::info!("session: SD file too small ({} < {})", n, SESSION_SIZE);
            None
        }
        Err(_) => None,
    }
}

// delete session file from SD (e.g. after successful cold boot)
pub fn clear_sd(sd: &crate::drivers::sdcard::SdStorage) {
    let _ = crate::drivers::storage::delete_in_pulp(sd, SESSION_FILE);
}
