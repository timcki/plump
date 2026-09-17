// SSD1677 e-paper driver (board-independent)
// tested on GDEQ0426T82 (800x480), no framebuffer, strip-streamed
//
// a refresh is a linear sequence, enforced by driver-minted tokens
// whose fields are private to this module:
//
//   phase1_bw / phase1_bw_inv_red / write_full_frame -> FrameWritten
//   partial_start_du / start_full_update(FrameWritten) -> Driven
//   partial_phase3_sync(&Driven) / finish_full_update(Driven)
//   grayscale_pass(&Window)  -- Window from region_state or Driven
//
// no caller can kick a waveform over a window it never wrote, or run
// a closing phase for a waveform that never started. which closer a
// given session is allowed (phase 3 vs finish vs a gray pass) is the
// session layer's business, see kernel/screen.rs
//
// when phase3 is skipped, phase1_bw_inv_red writes RED=!BW so DU
// drives every pixel to the correct BW target without a full GC
//
// panel power is latched on across refreshes (crosspoint-style):
// `kick` adds CLOCK_ON + ANALOG_ON only when power is actually off,
// and nothing powers down until deep sleep. the OffAfterRefresh power
// policy (sunlight mode) overrides this with ANALOG_OFF + CLOCK_OFF
// on every waveform

use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::spi::SpiDevice;
use esp_hal::delay::Delay;

use super::strip::StripBuffer;

pub const WIDTH: u16 = 800;
pub const HEIGHT: u16 = 480;

pub const SPI_FREQ_MHZ: u32 = 20;

const POWER_OFF_TIME_MS: u32 = 200; // analog shutdown timeout

/// How long any wait on the busy pin may last before the driver gives
/// up. ~2x worst case (full GC ~1.6s, grayscale ~1-2s, partial DU
/// ~400ms). Callers that watch the pin themselves (the scheduler's
/// waveform window) derive their own guard from this so the two
/// cannot drift.
pub(crate) const BUSY_TIMEOUT_MS: u64 = 5_000;

#[allow(dead_code)]
mod cmd {
    pub const DRIVER_OUTPUT_CONTROL: u8 = 0x01;
    pub const GATE_VOLTAGE: u8 = 0x03;
    pub const SOURCE_VOLTAGE: u8 = 0x04;
    pub const BOOSTER_SOFT_START: u8 = 0x0C;
    pub const DEEP_SLEEP: u8 = 0x10;
    pub const DATA_ENTRY_MODE: u8 = 0x11;
    pub const SW_RESET: u8 = 0x12;
    pub const TEMPERATURE_SENSOR: u8 = 0x18;
    pub const WRITE_TEMP_REGISTER: u8 = 0x1A;
    pub const MASTER_ACTIVATION: u8 = 0x20;
    pub const DISPLAY_UPDATE_CONTROL_1: u8 = 0x21;
    pub const DISPLAY_UPDATE_CONTROL_2: u8 = 0x22;
    pub const WRITE_RAM_BW: u8 = 0x24;
    pub const WRITE_RAM_RED: u8 = 0x26;
    pub const WRITE_VCOM: u8 = 0x2C;
    pub const WRITE_LUT: u8 = 0x32;
    pub const BORDER_WAVEFORM: u8 = 0x3C;
    pub const SET_RAM_X_RANGE: u8 = 0x44;
    pub const SET_RAM_Y_RANGE: u8 = 0x45;
    pub const SET_RAM_X_COUNTER: u8 = 0x4E;
    pub const SET_RAM_Y_COUNTER: u8 = 0x4F;
}

// DISPLAY_UPDATE_CONTROL_2 bits. the base byte of a waveform selects
// what the activation runs; `kick` ors in the power bits
mod ctrl2 {
    /// Power-up bits, added only when the panel is actually off.
    pub const POWER_ON: u8 = 0xC0; // CLOCK_ON + ANALOG_ON
    /// Power-down-after-refresh bits, added under the sunlight policy.
    pub const POWER_OFF: u8 = 0x03; // ANALOG_OFF + CLOCK_OFF

    /// TEMP_LOAD + LUT_LOAD + mode + DISPLAY_START: the partial DU.
    ///
    /// This was briefly 0x1C, on the theory that TEMP_LOAD was behind
    /// the windowed-DU inversion and that CrossPoint's FAST_REFRESH
    /// was the reference. Both halves were wrong: the inversion is the
    /// window itself (`screen::plan_partial`), and CrossPoint never
    /// calls `displayWindow` at all -- it is commented out of its
    /// GfxRenderer, so every CrossPoint refresh is full-panel and its
    /// fast path was never a windowed reference to match.
    ///
    /// Reverted rather than kept: TEMP_LOAD re-reads the sensor before
    /// the OTP search (datasheet 6.9), which is what corrects the
    /// register `start_full_update` leaves faked at 90C.
    pub const DU: u8 = 0x3C;
    /// LUT_LOAD + DISPLAY_START, no TEMP_LOAD so the faked temperature
    /// written just before survives into the OTP LUT pick: the fast
    /// clear (CrossPoint's HALF refresh).
    pub const FULL: u8 = 0x14;
    /// TEMP_LOAD + LUT_LOAD + DISPLAY_START: the OTP full-clear
    /// waveform at the panel's real temperature (CrossPoint's FULL
    /// refresh). Slower, and the only one that actually resets the
    /// pigment; the faked-90C waveform is under-driven at room
    /// temperature and leaves a haze the ghost clear was meant to
    /// remove.
    pub const FULL_CLEAN: u8 = 0x34;
    /// DISPLAY_START only: run the custom LUT already loaded.
    pub const CUSTOM_LUT: u8 = 0x0C;
}

/// Custom waveform LUT for 4-level grayscale rendering.
///
/// The SSD1677 combines BW RAM (LSB) and RED RAM (MSB) into a 2-bit index
/// per pixel. This LUT defines a waveform for each of the 4 states:
///   {0,0} = no change (white/black pixels stay as-is from BW refresh)
///   {0,1} = light gray
///   {1,0} = medium gray
///   {1,1} = dark gray
///
/// Waveform data from CrossPoint Reader (open-source, tuned for X4 display).
#[rustfmt::skip]
static LUT_GRAYSCALE: [u8; 112] = [
    // VS[0..4] waveform entries (5 × 10 bytes = 50 bytes)
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 00 no change
    0x54, 0x54, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 01 light gray
    0xAA, 0xA0, 0xA8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 10 medium gray
    0xA2, 0x22, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 11 dark gray
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // VCOM
    // TP/RP timing groups (10 × 5 bytes = 50 bytes)
    0x01, 0x01, 0x01, 0x01, 0x00,  // G0
    0x01, 0x01, 0x01, 0x01, 0x00,  // G1
    0x01, 0x01, 0x01, 0x01, 0x00,  // G2
    0x00, 0x00, 0x00, 0x00, 0x00,  // G3
    0x00, 0x00, 0x00, 0x00, 0x00,  // G4
    0x00, 0x00, 0x00, 0x00, 0x00,  // G5
    0x00, 0x00, 0x00, 0x00, 0x00,  // G6
    0x00, 0x00, 0x00, 0x00, 0x00,  // G7
    0x00, 0x00, 0x00, 0x00, 0x00,  // G8
    0x00, 0x00, 0x00, 0x00, 0x00,  // G9
    // Frame rate (5 bytes)
    0x8F, 0x8F, 0x8F, 0x8F, 0x8F,
    // Voltages: VGH, VSH1, VSH2, VSL, VCOM (5 bytes) + reserved (2)
    0x17, 0x41, 0xA8, 0x32, 0x30, 0x00, 0x00,
];

/// Revert LUT: snaps each AA gray state back to its nearest rail.
///
/// Indexed by the same {RED, BW} pair as [`LUT_GRAYSCALE`], and driven
/// with the gray planes still resident in controller RAM, so the pass
/// needs no RAM writes. After it the panel is bimodal (pure black /
/// white), which is the starting state the OTP DU transitions assume;
/// re-driving straight over intermediate grays is what produced the
/// mottled, non-uniform AA on page turns.
///
/// Waveform bytes from CrossPoint Reader's lut_grayscale_revert (X4):
/// medium gray ({1,0}, the strong-lift state) snaps white-ward, dark
/// gray ({1,1}) snaps black-ward. {0,1} is unused by our glyph
/// encoding and stays a no-op.
#[rustfmt::skip]
static LUT_GRAYSCALE_REVERT: [u8; 112] = [
    // VS[0..4] waveform entries (5 x 10 bytes = 50 bytes)
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 00 no change
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 01 unused
    0xA8, 0xA8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 10 medium -> white
    0xFC, 0xFC, 0xFC, 0xFC, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 11 dark -> black
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // VCOM
    // TP/RP timing groups (10 x 5 bytes = 50 bytes)
    0x01, 0x01, 0x01, 0x01, 0x01,  // G0
    0x01, 0x01, 0x01, 0x01, 0x01,  // G1
    0x01, 0x01, 0x01, 0x01, 0x00,  // G2
    0x01, 0x01, 0x01, 0x01, 0x00,  // G3
    0x00, 0x00, 0x00, 0x00, 0x00,  // G4
    0x00, 0x00, 0x00, 0x00, 0x00,  // G5
    0x00, 0x00, 0x00, 0x00, 0x00,  // G6
    0x00, 0x00, 0x00, 0x00, 0x00,  // G7
    0x00, 0x00, 0x00, 0x00, 0x00,  // G8
    0x00, 0x00, 0x00, 0x00, 0x00,  // G9
    // Frame rate (5 bytes)
    0x8F, 0x8F, 0x8F, 0x8F, 0x8F,
    // Voltages: VGH, VSH1, VSH2, VSL, VCOM (5 bytes) + reserved (2)
    0x17, 0x41, 0xA8, 0x32, 0x30, 0x00, 0x00,
];

/// Physical window of a refresh. Callers hand the driver logical
/// regions snapped to byte boundaries on both axes (see
/// `AlignedRegion`), so the window never carries edge masks: every
/// pixel inside it is drawn with real content, and the old practice
/// of parking mask slop bits at 1 in both planes (which erased up to
/// 7 rows of the neighbouring content on every unaligned partial) is
/// gone with the masks.
///
/// The fields are private: a window only comes from
/// [`DisplayDriver::region_state`] or out of a session token, so it
/// always describes an area the driver itself aligned.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Window {
    px: u16,
    py: u16,
    pw: u16,
    ph: u16,
}

impl Window {
    /// The whole panel.
    pub(crate) const FULL: Window = Window {
        px: 0,
        py: 0,
        pw: WIDTH,
        ph: HEIGHT,
    };
}

/// Proof that RAM holds this window's new content.
///
/// Minted only by the phase-1 writers and
/// [`DisplayDriver::write_full_frame`], and moved into a waveform
/// starter, so no waveform can be kicked over a window nobody wrote.
pub(crate) struct FrameWritten(Window);

/// Proof that a waveform was activated over this window.
///
/// Minted only by the starters, and required by every closing phase,
/// so phase 3 (or a gray pass standing in for it) cannot run before
/// phase 1 and its kick.
pub(crate) struct Driven(Window);

impl Driven {
    /// The window this waveform drove, for a pass that follows it.
    #[inline]
    pub(crate) fn window(&self) -> Window {
        self.0
    }
}

/// Outcome of a phase-1 write.
pub(crate) enum Phase1 {
    /// RAM holds the new content; kick the waveform.
    Written(FrameWritten),
    /// The region aligned to an empty window; nothing to refresh.
    EmptyWindow,
    /// The panel has never taken a full GC, so a DU has no defined
    /// starting state; run a full refresh instead.
    NeedsFullFirst,
}

/// Which full-clear waveform a GC runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FullKind {
    /// The faked-90C waveform: quick, for entering a book and waking.
    Fast,
    /// The real-temperature waveform: the one that resets the pigment,
    /// for the periodic and the manual ghost clear.
    Clean,
}

/// Which controller planes a strip feeds.
#[derive(Clone, Copy)]
enum PlaneWrite {
    /// One plane, content as drawn. Streams the whole window as a
    /// single RAM command instead of re-addressing per strip.
    Single(u8),
    /// The same strip into both planes (full frame).
    DualSame,
    /// BW takes the content, RED its inverse (delta-free re-drive).
    BwInvRed,
    /// Gray dual-plane: LSB -> BW RAM, MSB -> RED RAM.
    GrayDual,
}

/// Whether the controller registers are programmed, and whether the
/// panel has ever taken a full GC (a DU has no defined starting state
/// until it has). Deep sleep drops the registers but not the image,
/// so `ever_gc` survives it.
#[derive(Clone, Copy)]
enum PanelLife {
    Asleep { ever_gc: bool },
    Inited { ever_gc: bool },
}

impl PanelLife {
    #[inline]
    fn ever_gc(self) -> bool {
        match self {
            PanelLife::Asleep { ever_gc } | PanelLife::Inited { ever_gc } => ever_gc,
        }
    }

    #[inline]
    fn is_inited(self) -> bool {
        matches!(self, PanelLife::Inited { .. })
    }

    #[inline]
    fn inited(&mut self) {
        *self = PanelLife::Inited {
            ever_gc: self.ever_gc(),
        };
    }

    #[inline]
    fn slept(&mut self) {
        *self = PanelLife::Asleep {
            ever_gc: self.ever_gc(),
        };
    }

    #[inline]
    fn gc_done(&mut self) {
        *self = PanelLife::Inited { ever_gc: true };
    }
}

/// What the panel does with its analog rails after a waveform.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PowerPolicy {
    /// Leave them up: the next refresh skips the ~100ms booster start.
    Latch,
    /// Drop them every refresh; prevents UV-induced fading on
    /// white-bezel X4 models.
    OffAfterRefresh,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelPower {
    Off,
    On,
}

pub struct DisplayDriver<SPI, DC, RST, BUSY> {
    spi: SPI,
    dc: DC,
    rst: RST,
    busy: BUSY,
    power: PanelPower,
    policy: PowerPolicy,
    life: PanelLife,
}

impl<SPI, DC, RST, BUSY, E> DisplayDriver<SPI, DC, RST, BUSY>
where
    SPI: SpiDevice<Error = E>,
    DC: OutputPin,
    RST: OutputPin,
    BUSY: InputPin,
{
    pub fn new(spi: SPI, dc: DC, rst: RST, busy: BUSY) -> Self {
        Self {
            spi,
            dc,
            rst,
            busy,
            power: PanelPower::Off,
            policy: PowerPolicy::Latch,
            life: PanelLife::Asleep { ever_gc: false },
        }
    }

    fn reset(&mut self, delay: &mut Delay) {
        let _ = self.rst.set_high();
        delay.delay_millis(20);
        let _ = self.rst.set_low();
        delay.delay_millis(2);
        let _ = self.rst.set_high();
        delay.delay_millis(20);
    }

    pub fn init(&mut self, delay: &mut Delay) {
        self.reset(delay);
        self.init_display(delay);
    }

    // one strip loop for every plane layout: the shell (window walk,
    // begin_window, draw callback) is identical, only the per-strip
    // plane sends differ. the match monomorphizes into the loop
    fn write_region_strips<F>(
        &mut self,
        strip: &mut StripBuffer,
        win: &Window,
        planes: PlaneWrite,
        draw: &F,
    ) where
        F: Fn(&mut StripBuffer),
    {
        let Window { px, py, pw, ph } = *win;
        let max_rows = StripBuffer::max_rows_for_width(pw);

        // a single plane streams as one continuous window: address the
        // RAM and send the write command once, then push every strip
        // back to back
        if let PlaneWrite::Single(ram_cmd) = planes {
            self.set_partial_ram_area(px, py, pw, ph);
            self.send_command(ram_cmd);
        }

        let mut y = py;
        while y < py + ph {
            let rows = max_rows.min(py + ph - y);
            strip.begin_window(px, y, pw, rows);
            draw(strip);

            match planes {
                PlaneWrite::Single(_) => self.send_data(strip.data()),
                PlaneWrite::DualSame => {
                    // send the same rendered strip to both RAMs directly;
                    // no replay copy needed since send_data only reads
                    // the buffer
                    for &ram_cmd in &[cmd::WRITE_RAM_RED, cmd::WRITE_RAM_BW] {
                        self.set_partial_ram_area(px, y, pw, rows);
                        self.send_command(ram_cmd);
                        self.send_data(strip.data());
                    }
                }
                PlaneWrite::BwInvRed => {
                    self.set_partial_ram_area(px, y, pw, rows);
                    self.send_command(cmd::WRITE_RAM_BW);
                    self.send_data(strip.data());

                    // invert in place for the RED plane and send it as
                    // one DMA transfer; safe because this strip's
                    // contents are dead after the RED send (the next
                    // iteration's begin_window refills the buffer).
                    // replaces ~62 64-byte transactions with per-byte
                    // row math per strip
                    for b in strip.data_mut().iter_mut() {
                        *b = !*b;
                    }
                    self.set_partial_ram_area(px, y, pw, rows);
                    self.send_command(cmd::WRITE_RAM_RED);
                    self.send_data(strip.data());
                }
                PlaneWrite::GrayDual => {
                    // LSB plane → BW RAM
                    self.set_partial_ram_area(px, y, pw, rows);
                    self.send_command(cmd::WRITE_RAM_BW);
                    self.send_data(strip.data());

                    // MSB plane → RED RAM
                    self.set_partial_ram_area(px, y, pw, rows);
                    self.send_command(cmd::WRITE_RAM_RED);
                    self.send_data(strip.gray_data());
                }
            }

            y += rows;
        }
    }

    fn init_display(&mut self, delay: &mut Delay) {
        self.send_command(cmd::SW_RESET);
        delay.delay_millis(10);

        self.send_command(cmd::TEMPERATURE_SENSOR);
        self.send_data(&[0x80]);

        self.send_command(cmd::BOOSTER_SOFT_START);
        self.send_data(&[0xAE, 0xC7, 0xC3, 0xC0, 0x40]);

        self.send_command(cmd::DRIVER_OUTPUT_CONTROL);
        self.send_data(&[((HEIGHT - 1) & 0xFF) as u8, ((HEIGHT - 1) >> 8) as u8, 0x02]);

        self.send_command(cmd::BORDER_WAVEFORM);
        self.send_data(&[0x01]);

        // entry mode never changes after init; sending it per RAM-area
        // setup cost two SPI transactions per strip per plane
        self.send_command(cmd::DATA_ENTRY_MODE);
        self.send_data(&[0x01]);

        self.set_partial_ram_area(0, 0, WIDTH, HEIGHT);

        self.life.inited();
    }

    // every write path starts here: the controller loses its registers
    // over deep sleep, so the first access after a wake reprograms them
    #[inline]
    fn ensure_inited(&mut self, delay: &mut Delay) {
        if !self.life.is_inited() {
            self.init_display(delay);
        }
    }

    fn transform_region(&self, x: u16, y: u16, w: u16, h: u16) -> (u16, u16, u16, u16) {
        (y, HEIGHT - x - w, h, w)
    }

    // callers pass logical regions snapped to byte boundaries on both
    // axes, so after the rotation transform the physical window is
    // already byte-aligned; the outward snap here is a no-op kept as a
    // guard against an unsnapped caller
    fn align_partial_region(&self, x: u16, y: u16, w: u16, h: u16) -> Option<Window> {
        let (tx, ty, tw, th) = self.transform_region(x, y, w, h);

        let px = (tx & !7).min(WIDTH);
        let py = ty.min(HEIGHT);
        let pw = ((tw + (tx & 7) + 7) & !7).min(WIDTH - px);
        let ph = th.min(HEIGHT - py);

        if pw == 0 || ph == 0 {
            return None;
        }

        Some(Window { px, py, pw, ph })
    }

    // gates wired in reverse; Y flipped, X inc / Y dec.
    // DATA_ENTRY_MODE is programmed once in init_display
    fn set_partial_ram_area(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let y_flipped = HEIGHT - y - h;

        self.send_command(cmd::SET_RAM_X_RANGE);
        self.send_data(&[
            (x & 0xFF) as u8,
            (x >> 8) as u8,
            ((x + w - 1) & 0xFF) as u8,
            ((x + w - 1) >> 8) as u8,
        ]);

        self.send_command(cmd::SET_RAM_Y_RANGE);
        self.send_data(&[
            ((y_flipped + h - 1) & 0xFF) as u8,
            ((y_flipped + h - 1) >> 8) as u8,
            (y_flipped & 0xFF) as u8,
            (y_flipped >> 8) as u8,
        ]);

        self.send_command(cmd::SET_RAM_X_COUNTER);
        self.send_data(&[(x & 0xFF) as u8, (x >> 8) as u8]);

        self.send_command(cmd::SET_RAM_Y_COUNTER);
        self.send_data(&[
            ((y_flipped + h - 1) & 0xFF) as u8,
            ((y_flipped + h - 1) >> 8) as u8,
        ]);
    }

    fn wait_busy(&mut self, timeout_ms: u32) {
        use esp_hal::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            if self.busy.is_low().unwrap_or(true) {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            #[cfg(target_arch = "riscv32")]
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack));
            }
        }
    }

    // the controller takes a moment to assert busy after
    // MASTER_ACTIVATION; every completion path (is_busy poll,
    // wait_for_low) reads not-yet-started as finished, which let
    // phase 3 rewrite RAM mid-waveform and leave half-driven ghosts.
    // block the few microseconds until the pin rises; a spin is
    // cheaper than any wakeup at this scale, and the timeout covers
    // a dead panel
    fn wait_busy_rise(&mut self) {
        use esp_hal::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_millis(5);
        while self.busy.is_low().unwrap_or(false) {
            if Instant::now() >= deadline {
                log::warn!("ssd1677: busy never rose after activation");
                return;
            }
        }
    }

    fn send_command(&mut self, cmd: u8) {
        let _ = self.dc.set_low();
        let _ = self.spi.write(&[cmd]);
        let _ = self.dc.set_high();
    }

    fn send_data(&mut self, data: &[u8]) {
        let _ = self.dc.set_high();
        let _ = self.spi.write(data);
    }

    // single source of the power-latch policy: assemble CTRL2 from the
    // waveform's base byte, activate, block until the controller
    // asserts busy, then record where the rails end up. panel power is
    // latched between refreshes, so adding the power-up bits only when
    // it is actually off skips the ~100ms booster start inside the
    // waveform on every subsequent page turn
    fn kick(&mut self, base_ctrl2: u8) {
        let mut c = base_ctrl2;
        if self.power == PanelPower::Off {
            c |= ctrl2::POWER_ON;
        }
        if self.policy == PowerPolicy::OffAfterRefresh {
            c |= ctrl2::POWER_OFF;
        }

        self.send_command(cmd::DISPLAY_UPDATE_CONTROL_2);
        self.send_data(&[c]);

        self.send_command(cmd::MASTER_ACTIVATION);
        self.wait_busy_rise();

        // state the rails are in once the waveform completes
        self.power = match self.policy {
            PowerPolicy::Latch => PanelPower::On,
            PowerPolicy::OffAfterRefresh => PanelPower::Off,
        };
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn partial_phase1_bw<F>(
        &mut self,
        strip: &mut StripBuffer,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        delay: &mut Delay,
        draw: &F,
    ) -> Phase1
    where
        F: Fn(&mut StripBuffer),
    {
        self.begin_phase1(
            strip,
            x,
            y,
            w,
            h,
            delay,
            PlaneWrite::Single(cmd::WRITE_RAM_BW),
            draw,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn partial_phase1_bw_inv_red<F>(
        &mut self,
        strip: &mut StripBuffer,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        delay: &mut Delay,
        draw: &F,
    ) -> Phase1
    where
        F: Fn(&mut StripBuffer),
    {
        self.begin_phase1(strip, x, y, w, h, delay, PlaneWrite::BwInvRed, draw)
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_phase1<F>(
        &mut self,
        strip: &mut StripBuffer,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        delay: &mut Delay,
        planes: PlaneWrite,
        draw: &F,
    ) -> Phase1
    where
        F: Fn(&mut StripBuffer),
    {
        if !self.life.ever_gc() {
            return Phase1::NeedsFullFirst;
        }
        self.ensure_inited(delay);

        let Some(win) = self.align_partial_region(x, y, w, h) else {
            return Phase1::EmptyWindow;
        };
        self.write_region_strips(strip, &win, planes, draw);
        Phase1::Written(FrameWritten(win))
    }

    pub(crate) fn partial_start_du(&mut self, written: FrameWritten) -> Driven {
        let FrameWritten(win) = written;
        self.set_partial_ram_area(win.px, win.py, win.pw, win.ph);

        self.send_command(cmd::DISPLAY_UPDATE_CONTROL_1);
        self.send_data(&[0x00, 0x00]);

        self.kick(ctrl2::DU);
        Driven(win)
    }

    #[inline]
    pub(crate) fn is_busy(&mut self) -> bool {
        self.busy.is_high().unwrap_or(false)
    }

    // RED only: phase 3 always follows a phase 1 that wrote the
    // current content into BW RAM (a gray pass replaces phase 3
    // entirely, never precedes it), so rewriting BW here would halve
    // throughput for nothing
    pub(crate) fn partial_phase3_sync<F>(
        &mut self,
        strip: &mut StripBuffer,
        driven: &Driven,
        draw: &F,
    ) where
        F: Fn(&mut StripBuffer),
    {
        self.write_region_strips(
            strip,
            &driven.0,
            PlaneWrite::Single(cmd::WRITE_RAM_RED),
            draw,
        );
    }

    pub(crate) fn needs_initial_refresh(&self) -> bool {
        !self.life.ever_gc()
    }

    /// Physical window for a logical region, for callers that drive a
    /// waveform over an area they did not just write (the deferred AA
    /// pass over everything refreshed since the last one).
    pub(crate) fn region_state(&self, x: u16, y: u16, w: u16, h: u16) -> Option<Window> {
        self.align_partial_region(x, y, w, h)
    }

    /// Power off analog drivers after each partial refresh to prevent
    /// sunlight-induced fading on white-bezel X4 models.
    pub(crate) fn set_sunlight_mode(&mut self, enabled: bool) {
        self.policy = if enabled {
            PowerPolicy::OffAfterRefresh
        } else {
            PowerPolicy::Latch
        };
    }

    pub(crate) fn write_full_frame<F>(
        &mut self,
        strip: &mut StripBuffer,
        delay: &mut Delay,
        draw: &F,
    ) -> FrameWritten
    where
        F: Fn(&mut StripBuffer),
    {
        self.ensure_inited(delay);

        delay.delay_millis(1);

        // render each strip once and send it to both RAMs; running the
        // draw callback per plane doubled the CPU side of every full GC
        self.write_region_strips(strip, &Window::FULL, PlaneWrite::DualSame, draw);
        FrameWritten(Window::FULL)
    }

    /// Start a full GC refresh over the frame just written.
    pub(crate) fn start_full_update(&mut self, written: FrameWritten, kind: FullKind) -> Driven {
        let FrameWritten(win) = written;

        self.send_command(cmd::DISPLAY_UPDATE_CONTROL_1);
        self.send_data(&[0x40, 0x00]);

        match kind {
            FullKind::Fast => {
                // fake a 90C panel temperature so LUT_LOAD picks the
                // shortest OTP full-clear waveform (~1.7s measured,
                // matching CrossPoint's 1720ms HALF refresh; the
                // room-temperature waveform runs ~2.3s). TEMP_LOAD
                // stays cleared in the base byte so the controller
                // keeps this value instead of re-reading the sensor.
                // the next DU re-reads it. trick from CrossPoint
                // Reader, proven on this exact panel
                self.send_command(cmd::WRITE_TEMP_REGISTER);
                self.send_data(&[0x5A]);
                self.kick(ctrl2::FULL);
            }
            FullKind::Clean => self.kick(ctrl2::FULL_CLEAN),
        }
        Driven(win)
    }

    /// Close out a full GC: the panel now has a defined bimodal state,
    /// so partial DUs are allowed from here on.
    pub(crate) fn finish_full_update(&mut self, driven: Driven) {
        let Driven(_) = driven;
        self.life.gc_done();
    }

    /// Load a custom LUT waveform into the SSD1677.
    /// Data layout: 105 bytes (waveform + timing + frame rate),
    /// then 5 bytes of voltages (VGH, VSH1, VSH2, VSL, VCOM).
    fn load_custom_lut(&mut self, lut: &[u8]) {
        // First 105 bytes: VS entries + TP/RP groups + frame rate
        self.send_command(cmd::WRITE_LUT);
        self.send_data(&lut[..105]);

        // Voltage registers
        self.send_command(cmd::GATE_VOLTAGE);
        self.send_data(&lut[105..106]);

        self.send_command(cmd::SOURCE_VOLTAGE);
        self.send_data(&lut[106..109]);

        self.send_command(cmd::WRITE_VCOM);
        self.send_data(&lut[109..110]);
    }

    /// Start a grayscale refresh using the custom LUT.
    /// Call after LSB plane → BW RAM and MSB plane → RED RAM are written.
    fn start_grayscale_refresh(&mut self, win: &Window) {
        self.load_custom_lut(&LUT_GRAYSCALE);
        self.set_partial_ram_area(win.px, win.py, win.pw, win.ph);

        self.send_command(cmd::DISPLAY_UPDATE_CONTROL_1);
        self.send_data(&[0x00, 0x00]);

        self.kick(ctrl2::CUSTOM_LUT);
    }

    /// Drop the analog rails and clock (ANALOG_OFF + CLOCK_OFF as a
    /// standalone activation, ~200ms). Returns whether anything was
    /// done; the next `kick` re-adds the power-up bits. The latch is
    /// what makes page turns skip the booster start, so this is for
    /// idle stretches and sleep, never between consecutive refreshes.
    pub(crate) fn power_off(&mut self) -> bool {
        if self.power != PanelPower::On {
            return false;
        }
        self.send_command(cmd::DISPLAY_UPDATE_CONTROL_2);
        self.send_data(&[0x83]);
        self.send_command(cmd::MASTER_ACTIVATION);
        self.wait_busy(POWER_OFF_TIME_MS);
        self.power = PanelPower::Off;
        true
    }

    #[inline]
    pub(crate) fn is_powered(&self) -> bool {
        self.power == PanelPower::On
    }

    // mode 1: image retained, ~3 uA; requires hw reset to wake
    pub(crate) fn enter_deep_sleep(&mut self) {
        self.power_off();

        self.send_command(cmd::DEEP_SLEEP);
        self.send_data(&[0x01]);
        self.life.slept();
    }
}

impl<SPI, DC, RST, BUSY, E> DisplayDriver<SPI, DC, RST, BUSY>
where
    SPI: SpiDevice<Error = E>,
    DC: OutputPin,
    RST: OutputPin,
    BUSY: InputPin + embedded_hal_async::digital::Wait,
{
    pub(crate) fn busy_pin(&mut self) -> &mut BUSY {
        &mut self.busy
    }

    // bound the busy-pin wait so a stuck EPD cannot wedge the device.
    // `ctx` is logged on timeout so the caller can be identified in serial.
    pub(crate) async fn wait_busy_async(
        &mut self,
        ctx: &'static str,
    ) -> Result<(), embassy_time::TimeoutError> {
        use embassy_time::{Duration, with_timeout};
        match with_timeout(
            Duration::from_millis(BUSY_TIMEOUT_MS),
            self.busy.wait_for_low(),
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                log::error!(
                    "wait_busy_async: TIMEOUT after {}ms at {}",
                    BUSY_TIMEOUT_MS,
                    ctx
                );
                Err(e)
            }
        }
    }

    /// Perform a grayscale antialiasing pass.
    ///
    /// Renders content once per strip in GrayDual mode, writing LSB plane
    /// to BW RAM and MSB plane to RED RAM, then triggers a grayscale LUT
    /// refresh. Both RAMs keep the gray plane data afterwards: it is the
    /// index for a later [`Self::grayscale_revert_pass`], so the caller
    /// must track the region as gray-coded and stale.
    ///
    /// On timeout the caller must force a full GC on the next refresh
    /// to bring panel and planes back in sync.
    pub(crate) async fn grayscale_pass<F>(
        &mut self,
        strip: &mut StripBuffer,
        win: &Window,
        draw: &F,
    ) -> Result<(), embassy_time::TimeoutError>
    where
        F: Fn(&mut StripBuffer),
    {
        // single draw pass fills both LSB (buf) and MSB (gray_buf)
        // planes; the bracket owns the restore so a later BW frame
        // cannot inherit the gray polarity
        crate::perf_begin!(_t0);
        strip.with_gray_dual(|strip| {
            self.write_region_strips(strip, win, PlaneWrite::GrayDual, draw)
        });
        crate::perf_event!(
            "render",
            "gray write_ms={} px={} py={} pw={} ph={}",
            _t0.elapsed().as_millis(),
            win.px,
            win.py,
            win.pw,
            win.ph
        );

        crate::perf_begin!(_t1);
        self.start_grayscale_refresh(win);
        self.wait_busy_async("grayscale_refresh").await?;
        crate::perf_event!("render", "gray wave_ms={}", _t1.elapsed().as_millis());

        // both RAMs deliberately keep the gray plane data: the revert
        // pass reads them as its waveform index, so restoring BW here
        // would break it. the caller marks the region gray-coded and
        // stale; the next partial over it reverts, then rewrites both
        // planes via inv_red
        Ok(())
    }

    /// Revert pass over `win`: drives every AA gray pixel back to its
    /// nearest rail using [`LUT_GRAYSCALE_REVERT`], indexed by the
    /// gray planes still resident in RAM from the preceding
    /// [`Self::grayscale_pass`]. No RAM writes.
    pub(crate) async fn grayscale_revert_pass(
        &mut self,
        win: &Window,
    ) -> Result<(), embassy_time::TimeoutError> {
        self.load_custom_lut(&LUT_GRAYSCALE_REVERT);
        self.set_partial_ram_area(win.px, win.py, win.pw, win.ph);

        self.send_command(cmd::DISPLAY_UPDATE_CONTROL_1);
        self.send_data(&[0x00, 0x00]);

        // display with the custom LUT (no LUT_LOAD); the following DU
        // sets LUT_LOAD and restores the OTP waveform
        self.kick(ctrl2::CUSTOM_LUT);

        self.wait_busy_async("grayscale_revert").await
    }
}

