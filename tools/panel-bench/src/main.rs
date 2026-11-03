// panel bench: a standalone firmware that hands the SSD1677 to the host.
//
// no kernel, no apps, no SD. the EPD lines are bit-banged so the bench
// can also read back from the controller (3-wire style, SDA turned
// around) and watch BUSY with microsecond resolution. the host speaks a
// line protocol over the USB-Serial-JTAG console; every command answers
// with one or more lines and a final `ok` or `err <why>`.
//
//   ping                      liveness
//   rst                       hardware reset (RST high 20 ms, low 2, high 20)
//   c HH [HH ..]              command HH with data bytes
//   rd HH N [dummy]           command HH, read N bytes from SDA (and MISO)
//   pat PP SPEC [inv]         fill RAM plane PP (24 BW / 26 RED), full panel
//   act HH                    0x22 HH, 0x20, wait BUSY low; prints timing
//   go HH                     0x22 HH, 0x20, return at once
//   wait                      wait BUSY low, prints elapsed since `go`
//   busy                      BUSY level
//   pins                      GPIO_IN snapshot of every EPD-related pad
//   ms N                      sleep N ms
//
// SPEC (portrait coordinates, 480 wide x 800 tall, 1 = white):
//   solid B                   every pixel B
//   grid C R BITS             C x R cells, BITS row-major from top left
//   checker S                 S px checkerboard
//   hlines S / vlines S       stripes, S px on / S px off
//   text SEED                 fake text page (stroke glyphs on lines)
//   marker                    white page, black L in the top-left corner,
//                             black bar along the top edge, dot bottom right
//   ramp                      16 vertical bands of increasing dither density

#![no_std]
#![no_main]

use core::fmt::Write as _;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Flex, Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::time::Instant;
use esp_hal::usb_serial_jtag::UsbSerialJtag;

esp_bootloader_esp_idf::esp_app_desc!();

const GPIO_OUT_W1TS: *mut u32 = 0x6000_4008 as *mut u32;
const GPIO_OUT_W1TC: *mut u32 = 0x6000_400C as *mut u32;
const GPIO_ENABLE_W1TS: *mut u32 = 0x6000_4024 as *mut u32;
const GPIO_ENABLE_W1TC: *mut u32 = 0x6000_4028 as *mut u32;
const GPIO_IN: *const u32 = 0x6000_403C as *const u32;

const DC: u32 = 1 << 4;
const RST: u32 = 1 << 5;
const BUSY: u32 = 1 << 6;
const MISO: u32 = 1 << 7;
const SCK: u32 = 1 << 8;
const SDA: u32 = 1 << 10;
const CS: u32 = 1 << 21;

const PHYS_W: usize = 800;
const PHYS_H: usize = 480;
const ROW_BYTES: usize = PHYS_W / 8;
const PORTRAIT_W: usize = PHYS_H;
const PORTRAIT_H: usize = PHYS_W;

// GPIO12/13 have no esp-hal type on the C3 (flash pads, free in DIO
// mode): route them to plain GPIO output and drive high by register
fn raw_output_high(pin: u32) {
    unsafe {
        let mux = (0x6000_9000 + pin * 4) as *mut u32;
        mux.write_volatile((mux.read_volatile() & !(0b111 << 12)) | (1 << 12));
        ((0x6000_4554 + pin * 4) as *mut u32).write_volatile(0x80);
        GPIO_ENABLE_W1TS.write_volatile(1 << pin);
        GPIO_OUT_W1TS.write_volatile(1 << pin);
    }
}

#[inline(always)]
fn hi(m: u32) {
    unsafe { GPIO_OUT_W1TS.write_volatile(m) }
}
#[inline(always)]
fn lo(m: u32) {
    unsafe { GPIO_OUT_W1TC.write_volatile(m) }
}
#[inline(always)]
fn input() -> u32 {
    unsafe { GPIO_IN.read_volatile() }
}

fn spin(n: u32) {
    for _ in 0..n {
        core::hint::spin_loop();
    }
}

struct Bus;

impl Bus {
    #[inline(always)]
    fn byte(b: u8) {
        for i in (0..8).rev() {
            if (b >> i) & 1 == 1 {
                hi(SDA);
            } else {
                lo(SDA);
            }
            hi(SCK);
            lo(SCK);
        }
    }

    fn cmd(c: u8) {
        lo(CS);
        lo(DC);
        Self::byte(c);
        hi(DC);
        hi(CS);
    }

    fn data(d: &[u8]) {
        lo(CS);
        hi(DC);
        for &b in d {
            Self::byte(b);
        }
        hi(CS);
    }

    fn cmd_data(c: u8, d: &[u8]) {
        Self::cmd(c);
        if !d.is_empty() {
            Self::data(d);
        }
    }

    /// command, then turn SDA around and clock `out.len()` bytes in,
    /// sampling SDA and MISO on each rising edge at a slow clock
    fn read(c: u8, sda: &mut [u8], miso: &mut [u8]) {
        lo(CS);
        lo(DC);
        Self::byte(c);
        hi(DC);
        unsafe { GPIO_ENABLE_W1TC.write_volatile(SDA) };
        spin(200);
        for i in 0..sda.len() {
            let (mut a, mut b) = (0u8, 0u8);
            for _ in 0..8 {
                hi(SCK);
                spin(60);
                let v = input();
                a = (a << 1) | ((v & SDA != 0) as u8);
                b = (b << 1) | ((v & MISO != 0) as u8);
                lo(SCK);
                spin(60);
            }
            sda[i] = a;
            miso[i] = b;
        }
        hi(CS);
        unsafe { GPIO_ENABLE_W1TS.write_volatile(SDA) };
    }
}

fn busy() -> bool {
    input() & BUSY != 0
}

// ── patterns ────────────────────────────────────────────────────────

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
}

enum Spec<'a> {
    Solid(bool),
    Grid { c: usize, r: usize, bits: &'a [u8] },
    Checker(usize),
    HLines(usize),
    VLines(usize),
    Text(u32),
    Marker,
    Ramp,
}

// fake text: lines 30 px apart, glyph cells 12 px wide, words of 2-9
// glyphs. a glyph is an 'n'/'m'/'o'/'l' style stroke shape 2 px thick,
// enough edge density to show ghosting like real body text does
fn text_pixel(lx: usize, ly: usize, line_words: &[[u16; 24]; 26], seed: u32) -> bool {
    const TOP: usize = 40;
    const PITCH: usize = 30;
    const MARGIN: usize = 24;
    if ly < TOP || lx < MARGIN || lx >= PORTRAIT_W - MARGIN {
        return true;
    }
    let line = (ly - TOP) / PITCH;
    if line >= 25 {
        return true;
    }
    let y = (ly - TOP) % PITCH;
    if !(4..22).contains(&y) {
        return true;
    }
    let x = lx - MARGIN;
    let cell = x / 12;
    let gx = x % 12;
    let mask = line_words[line];
    if cell >= 36 || (mask[cell / 2] >> ((cell & 1) * 8)) & 0xFF == 0 {
        return true;
    }
    let kind = ((mask[cell / 2] >> ((cell & 1) * 8)) & 0xFF) as u32 ^ seed;
    let ink = match kind % 4 {
        // n
        0 => (gx < 2 || (7..9).contains(&gx)) && y >= 8 || (y < 10 && y >= 8 && gx < 9),
        // l with a foot
        1 => (3..5).contains(&gx) || (y >= 20 && gx < 8),
        // o
        2 => {
            (y >= 8 && (gx < 2 || (7..9).contains(&gx)))
                || ((8..10).contains(&y) || y >= 20) && gx < 9
        }
        // m-ish: three stems
        _ => y >= 8 && (gx < 2 || (4..6).contains(&gx) || (8..10).contains(&gx)) || (8..10).contains(&y) && gx < 10,
    };
    !ink
}

fn build_text_layout(seed: u32) -> [[u16; 24]; 26] {
    let mut rng = Rng(seed.wrapping_mul(2654435761).max(1));
    let mut out = [[0u16; 24]; 26];
    for line in out.iter_mut() {
        let mut cell = 0usize;
        let short = rng.next() % 6 == 0;
        let limit = if short { 18 } else { 36 };
        while cell < limit {
            let wl = 2 + (rng.next() % 8) as usize;
            for k in 0..wl {
                let c = cell + k;
                if c >= limit {
                    break;
                }
                let g = (1 + rng.next() % 255) as u16;
                line[c / 2] |= g << ((c & 1) * 8);
            }
            cell += wl + 1;
        }
    }
    out
}

fn pixel(spec: &Spec, lx: usize, ly: usize, text: &[[u16; 24]; 26], seed: u32) -> bool {
    match *spec {
        Spec::Solid(b) => b,
        Spec::Grid { c, r, bits } => {
            let cx = (lx * c / PORTRAIT_W).min(c - 1);
            let cy = (ly * r / PORTRAIT_H).min(r - 1);
            bits.get(cy * c + cx).copied() == Some(b'1')
        }
        Spec::Checker(s) => ((lx / s) + (ly / s)) % 2 == 0,
        Spec::HLines(s) => (ly / s) % 2 == 1,
        Spec::VLines(s) => (lx / s) % 2 == 1,
        Spec::Text(_) => text_pixel(lx, ly, text, seed),
        Spec::Marker => {
            let l = (lx < 120 && (40..56).contains(&ly)) || ((40..56).contains(&lx) && ly < 200 && ly >= 40);
            let bar = ly < 16;
            let dot = lx >= PORTRAIT_W - 60 && ly >= PORTRAIT_H - 60 && lx < PORTRAIT_W - 20 && ly < PORTRAIT_H - 20;
            !(l || bar || dot)
        }
        Spec::Ramp => {
            let band = lx * 16 / PORTRAIT_W;
            // ordered 4x4 bayer
            const B: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
            B[ly % 4][lx % 4] >= band as u8
        }
    }
}

fn set_full_window() {
    // data entry 0x01 (X inc, Y dec), as production; Y starts at 479
    Bus::cmd_data(0x11, &[0x01]);
    Bus::cmd_data(0x44, &[0x00, 0x00, ((PHYS_W - 1) & 0xFF) as u8, ((PHYS_W - 1) >> 8) as u8]);
    Bus::cmd_data(0x45, &[((PHYS_H - 1) & 0xFF) as u8, ((PHYS_H - 1) >> 8) as u8, 0x00, 0x00]);
    Bus::cmd_data(0x4E, &[0x00, 0x00]);
    Bus::cmd_data(0x4F, &[((PHYS_H - 1) & 0xFF) as u8, ((PHYS_H - 1) >> 8) as u8]);
}

fn fill_plane(ram: u8, spec: &Spec, inv: bool) {
    let seed = if let Spec::Text(s) = spec { *s } else { 0 };
    let text = build_text_layout(seed);
    set_full_window();
    Bus::cmd(ram);
    lo(CS);
    hi(DC);
    let mut row = [0u8; ROW_BYTES];
    for r in 0..PHYS_H {
        // production mapping: streamed row r is portrait column 479 - r,
        // bit 7 of byte b is portrait row 8b
        let lx = PORTRAIT_W - 1 - r;
        for (b, out) in row.iter_mut().enumerate() {
            let mut v = 0u8;
            for k in 0..8 {
                let ly = b * 8 + k;
                if pixel(spec, lx, ly, &text, seed) ^ inv {
                    v |= 0x80 >> k;
                }
            }
            *out = v;
        }
        for &b in row.iter() {
            Bus::byte(b);
        }
    }
    hi(CS);
}

// ── line protocol ───────────────────────────────────────────────────

fn hex(s: &str) -> Option<u8> {
    u8::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

struct Out<'a, 'd>(&'a mut UsbSerialJtag<'d, esp_hal::Blocking>);
impl core::fmt::Write for Out<'_, '_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.write(s.as_bytes()).map_err(|_| core::fmt::Error)
    }
}

fn parse_spec<'a>(words: &mut core::str::SplitWhitespace<'a>) -> Result<Spec<'a>, &'static str> {
    let kind = words.next().ok_or("spec?")?;
    let num = |w: Option<&str>| w.and_then(|s| s.parse::<usize>().ok()).ok_or("number?");
    Ok(match kind {
        "solid" => Spec::Solid(num(words.next())? != 0),
        "grid" => {
            let c = num(words.next())?;
            let r = num(words.next())?;
            let bits = words.next().ok_or("bits?")?.as_bytes();
            if c == 0 || r == 0 || bits.len() != c * r {
                return Err("grid size");
            }
            Spec::Grid { c, r, bits }
        }
        "checker" => Spec::Checker(num(words.next())?.max(1)),
        "hlines" => Spec::HLines(num(words.next())?.max(1)),
        "vlines" => Spec::VLines(num(words.next())?.max(1)),
        "text" => Spec::Text(num(words.next())? as u32),
        "marker" => Spec::Marker,
        "ramp" => Spec::Ramp,
        _ => return Err("unknown spec"),
    })
}

fn wait_busy(limit_ms: u64) -> (bool, u64) {
    let t0 = Instant::now();
    while busy() {
        if t0.elapsed().as_millis() >= limit_ms {
            return (false, t0.elapsed().as_micros());
        }
    }
    (true, t0.elapsed().as_micros())
}

fn activate(c2: u8) -> u64 {
    Bus::cmd_data(0x22, &[c2]);
    Bus::cmd(0x20);
    // time until BUSY rises, in us
    let t = Instant::now();
    while !busy() && t.elapsed().as_micros() < 20_000 {}
    t.elapsed().as_micros()
}

#[esp_hal::main]
fn main() -> ! {
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let delay = Delay::new();

    // keep the board latched and the SD card deselected
    raw_output_high(13);
    raw_output_high(12);

    let _cs = Output::new(p.GPIO21, Level::High, OutputConfig::default());
    let _dc = Output::new(p.GPIO4, Level::High, OutputConfig::default());
    let _rst = Output::new(p.GPIO5, Level::High, OutputConfig::default());
    let _sck = Output::new(p.GPIO8, Level::Low, OutputConfig::default());
    let _busy = Input::new(p.GPIO6, InputConfig::default().with_pull(Pull::None));
    let _miso = Input::new(p.GPIO7, InputConfig::default().with_pull(Pull::Up));
    let mut sda = Flex::new(p.GPIO10);
    sda.apply_output_config(&OutputConfig::default());
    sda.set_low();
    sda.set_input_enable(true);
    sda.set_output_enable(true);

    let mut usb = UsbSerialJtag::new(p.USB_DEVICE);
    let mut line = [0u8; 2048];
    let mut len = 0usize;
    let mut go_at = Instant::now();

    let _ = writeln!(Out(&mut usb), "\r\nbench: ready");

    loop {
        let b = match usb.read_byte() {
            Ok(b) => b,
            Err(_) => continue,
        };
        if b != b'\n' && b != b'\r' {
            if len < line.len() {
                line[len] = b;
                len += 1;
            }
            continue;
        }
        if len == 0 {
            continue;
        }
        let text = core::str::from_utf8(&line[..len]).unwrap_or("");
        let mut out = Out(&mut usb);
        let res = run(text, &mut out, &delay, &mut go_at);
        match res {
            Ok(()) => {
                let _ = writeln!(out, "ok");
            }
            Err(e) => {
                let _ = writeln!(out, "err {}", e);
            }
        }
        len = 0;
    }
}

fn run(text: &str, out: &mut Out, delay: &Delay, go_at: &mut Instant) -> Result<(), &'static str> {
    let mut w = text.split_whitespace();
    let Some(op) = w.next() else { return Ok(()) };
    match op {
        "ping" => {
            let _ = writeln!(out, "pong");
        }
        "rst" => {
            hi(RST);
            delay.delay_millis(20);
            lo(RST);
            delay.delay_millis(2);
            hi(RST);
            delay.delay_millis(20);
        }
        "c" => {
            let c = w.next().and_then(hex).ok_or("cmd?")?;
            let mut buf = [0u8; 256];
            let mut n = 0;
            for t in w {
                if n == buf.len() {
                    return Err("too long");
                }
                buf[n] = hex(t).ok_or("byte?")?;
                n += 1;
            }
            Bus::cmd_data(c, &buf[..n]);
        }
        "rd" => {
            let c = w.next().and_then(hex).ok_or("cmd?")?;
            let n: usize = w.next().and_then(|s| s.parse().ok()).ok_or("count?")?;
            let n = n.min(128);
            let mut a = [0u8; 128];
            let mut b = [0u8; 128];
            Bus::read(c, &mut a[..n], &mut b[..n]);
            let _ = write!(out, "sda");
            for v in &a[..n] {
                let _ = write!(out, " {:02X}", v);
            }
            let _ = write!(out, "\nmiso");
            for v in &b[..n] {
                let _ = write!(out, " {:02X}", v);
            }
            let _ = writeln!(out);
        }
        "pat" => {
            let ram = w.next().and_then(hex).ok_or("plane?")?;
            if ram != 0x24 && ram != 0x26 {
                return Err("plane must be 24 or 26");
            }
            let spec = parse_spec(&mut w)?;
            let inv = w.next() == Some("inv");
            let t = Instant::now();
            fill_plane(ram, &spec, inv);
            let _ = writeln!(out, "write_ms={}", t.elapsed().as_millis());
        }
        "act" => {
            let c2 = w.next().and_then(hex).ok_or("ctrl2?")?;
            let rise = activate(c2);
            let (ok, us) = wait_busy(10_000);
            let _ = writeln!(out, "rise_us={} busy_ms={} done={}", rise, us / 1000, ok);
        }
        "go" => {
            let c2 = w.next().and_then(hex).ok_or("ctrl2?")?;
            let rise = activate(c2);
            *go_at = Instant::now();
            let _ = writeln!(out, "rise_us={}", rise);
        }
        "wait" => {
            let (ok, _) = wait_busy(10_000);
            let _ = writeln!(out, "busy_ms={} done={}", go_at.elapsed().as_millis(), ok);
        }
        "busy" => {
            let _ = writeln!(out, "busy={}", busy() as u8);
        }
        "pins" => {
            let v = input();
            let _ = writeln!(
                out,
                "gpio_in={:08X} dc={} rst={} busy={} miso={} sck={} sda={} cs={}",
                v,
                (v & DC != 0) as u8,
                (v & RST != 0) as u8,
                (v & BUSY != 0) as u8,
                (v & MISO != 0) as u8,
                (v & SCK != 0) as u8,
                (v & SDA != 0) as u8,
                (v & CS != 0) as u8
            );
        }
        "ms" => {
            let n: u32 = w.next().and_then(|s| s.parse().ok()).ok_or("ms?")?;
            delay.delay_millis(n);
        }
        _ => return Err("unknown op"),
    }
    Ok(())
}
