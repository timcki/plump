// system configuration: key=value text in _PLUMP/SETTINGS.TXT
//
// SystemSettings and WifiConfig are kernel-owned configuration;
// the SettingsApp in apps/ provides the UI for editing them

pub const SETTINGS_FILE: &str = "SETTINGS.TXT";

// default sleep timeout in minutes
pub const DEFAULT_SLEEP_TIMEOUT: u16 = 10;

// maximum sleep timeout in minutes
pub const MAX_SLEEP_TIMEOUT: u16 = 120;

// increment step for sleep timeout adjustment
pub const SLEEP_TIMEOUT_STEP: u16 = 5;

// default ghost clear interval
pub const DEFAULT_GHOST_CLEAR: u8 = 10;

// minimum ghost clear interval
pub const MIN_GHOST_CLEAR: u8 = 5;

// maximum ghost clear interval
pub const MAX_GHOST_CLEAR: u8 = 100;

// increment step for ghost clear adjustment
pub const GHOST_CLEAR_STEP: u8 = 5;

// default font size index (0=XSmall, 1=Small, 2=Medium, 3=Large, 4=XLarge)
pub const DEFAULT_FONT_SIZE_IDX: u8 = 2;

// reading themes: named presets for margins, spacing, and overall feel.
// each theme bundles margin_h, margin_v, line_spacing_pct into one
// user-friendly selection instead of exposing raw pixel values.
//
// theme index is stored as a single u8 in SETTINGS.TXT:
//   0 = Compact   – narrow margins, tight spacing, max content
//   1 = Default   – balanced for most books
//   2 = Relaxed   – wider margins, looser spacing, easier on the eyes
//   3 = Spacious  – large margins, generous spacing, paperback feel

pub const NUM_READING_THEMES: u8 = 4;
pub const DEFAULT_READING_THEME: u8 = 1;

#[derive(Clone, Copy)]
pub struct ReadingTheme {
    pub name: &'static str,
    pub margin_h: u16,         // horizontal margin in pixels
    pub margin_v: u16,         // vertical margin (top offset) in pixels
    pub line_spacing_pct: u16, // line spacing as percentage (100 = font native)
}

impl ReadingTheme {
    /// Look up a theme by index; falls back to the last theme.
    pub fn from_idx(idx: u8) -> &'static ReadingTheme {
        let i = (idx as usize).min(READING_THEMES.len() - 1);
        &READING_THEMES[i]
    }
}

pub const READING_THEMES: [ReadingTheme; NUM_READING_THEMES as usize] = [
    ReadingTheme {
        name: "Compact",
        margin_h: 8,
        margin_v: 0,
        line_spacing_pct: 100,
    },
    ReadingTheme {
        name: "Default",
        margin_h: 16,
        margin_v: 4,
        line_spacing_pct: 120,
    },
    ReadingTheme {
        name: "Relaxed",
        margin_h: 24,
        margin_v: 8,
        line_spacing_pct: 140,
    },
    ReadingTheme {
        name: "Spacious",
        margin_h: 40,
        margin_v: 12,
        line_spacing_pct: 160,
    },
];

// text alignment for the reader (0 = Left, 1 = Justify)
pub const NUM_TEXT_ALIGNMENTS: u8 = 2;
pub const DEFAULT_TEXT_ALIGNMENT: u8 = 0;

const TEXT_ALIGNMENT_NAMES: &[&str] = &["Left", "Justify"];

#[derive(Clone, Copy)]
pub struct SystemSettings {
    // power settings
    pub sleep_timeout: u16,    // minutes idle before sleep; 0 = never
    pub ghost_clear_every: u8, // partial refreshes before forced full GC

    // font settings
    pub book_font_size_idx: u8, // 0 = XSmall, 1 = Small, 2 = Medium, 3 = Large, 4 = XLarge
    pub ui_font_size_idx: u8,   // 0 = XSmall, 1 = Small, 2 = Medium, 3 = Large, 4 = XLarge

    // reading settings
    pub reading_theme: u8, // index into READING_THEMES

    // control settings
    pub swap_buttons: bool, // swap Back/Select with Left/Right physical buttons

    // display settings
    pub sunlight_fix: bool, // power off analog after each partial refresh (prevents sunlight fading)
    pub text_aa: bool,      // antialiased text via grayscale LUT (slower page turns)

    // reader settings
    pub reader_status: bool, // show book title + page info bar at bottom of reader
    pub text_alignment: u8,  // 0 = Left, 1 = Justify (index into TEXT_ALIGNMENT_NAMES)
}

impl Default for SystemSettings {
    fn default() -> Self {
        Self::defaults()
    }
}

impl SystemSettings {
    pub const fn defaults() -> Self {
        Self {
            sleep_timeout: DEFAULT_SLEEP_TIMEOUT,
            ghost_clear_every: DEFAULT_GHOST_CLEAR,
            book_font_size_idx: DEFAULT_FONT_SIZE_IDX,
            ui_font_size_idx: DEFAULT_FONT_SIZE_IDX,
            reading_theme: DEFAULT_READING_THEME,
            swap_buttons: false,
            sunlight_fix: false,
            text_aa: false,
            reader_status: true,
            text_alignment: DEFAULT_TEXT_ALIGNMENT,
        }
    }

    /// Look up the active reading theme.
    pub fn reading_theme(&self) -> &'static ReadingTheme {
        ReadingTheme::from_idx(self.reading_theme)
    }

    /// Return the display name for the current text alignment.
    pub fn text_alignment_name(&self) -> &'static str {
        let i = (self.text_alignment as usize).min(TEXT_ALIGNMENT_NAMES.len() - 1);
        TEXT_ALIGNMENT_NAMES[i]
    }

    pub fn sanitize(&mut self) {
        self.sanitize_with_max_font(Self::DEFAULT_MAX_FONT_IDX);
    }

    pub fn sanitize_with_max_font(&mut self, max_font: u8) {
        self.sleep_timeout = self.sleep_timeout.min(MAX_SLEEP_TIMEOUT);
        self.ghost_clear_every = self
            .ghost_clear_every
            .clamp(MIN_GHOST_CLEAR, MAX_GHOST_CLEAR);
        self.book_font_size_idx = self.book_font_size_idx.min(max_font);
        self.ui_font_size_idx = self.ui_font_size_idx.min(max_font);
        self.reading_theme = self.reading_theme.min(NUM_READING_THEMES - 1);
        self.text_alignment = self.text_alignment.min(NUM_TEXT_ALIGNMENTS - 1);
    }

    // reasonable default - override via sanitize_with_max_font
    const DEFAULT_MAX_FONT_IDX: u8 = 4;
}

pub const WIFI_SSID_CAP: usize = 32;
pub const WIFI_PASS_CAP: usize = 63;

pub struct WifiConfig {
    ssid: crate::util::FixedStr<WIFI_SSID_CAP>,
    pass: crate::util::FixedStr<WIFI_PASS_CAP>,
}

impl WifiConfig {
    pub const fn empty() -> Self {
        Self {
            ssid: crate::util::FixedStr::EMPTY,
            pass: crate::util::FixedStr::EMPTY,
        }
    }

    pub fn ssid(&self) -> &str {
        self.ssid.as_str()
    }

    pub fn password(&self) -> &str {
        self.pass.as_str()
    }

    pub fn has_credentials(&self) -> bool {
        !self.ssid.is_empty()
    }

    fn set_ssid(&mut self, val: &[u8]) {
        self.ssid.set(val);
    }

    fn set_pass(&mut self, val: &[u8]) {
        self.pass.set(val);
    }
}

fn trim(s: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = s.len();
    while start < end && matches!(s[start], b' ' | b'\t' | b'\r') {
        start += 1;
    }
    while end > start && matches!(s[end - 1], b' ' | b'\t' | b'\r') {
        end -= 1;
    }
    &s[start..end]
}

fn parse_u16(s: &[u8]) -> Option<u16> {
    if s.is_empty() {
        return None;
    }
    let mut val: u16 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add((b - b'0') as u16)?;
    }
    Some(val)
}

fn apply_setting(key: &[u8], val: &[u8], s: &mut SystemSettings, w: &mut WifiConfig) {
    match key {
        b"sleep_timeout" => {
            if let Some(v) = parse_u16(val) {
                s.sleep_timeout = v;
            }
        }
        b"ghost_clear" => {
            if let Some(v) = parse_u16(val) {
                s.ghost_clear_every = v as u8;
            }
        }
        b"book_font" => {
            if let Some(v) = parse_u16(val) {
                s.book_font_size_idx = v as u8;
            }
        }
        b"ui_font" => {
            if let Some(v) = parse_u16(val) {
                s.ui_font_size_idx = v as u8;
            }
        }
        b"reading_theme" => {
            if let Some(v) = parse_u16(val) {
                s.reading_theme = v as u8;
            }
        }
        b"swap_buttons" => {
            s.swap_buttons = val == b"1" || val == b"true";
        }
        b"sunlight_fix" => {
            s.sunlight_fix = val == b"1" || val == b"true";
        }
        b"text_aa" => {
            s.text_aa = val == b"1" || val == b"true";
        }
        b"reader_status" => {
            s.reader_status = val == b"1" || val == b"true";
        }
        b"text_alignment" => {
            if let Some(v) = parse_u16(val) {
                s.text_alignment = v as u8;
            }
        }
        b"wifi_ssid" => w.set_ssid(val),
        b"wifi_pass" => w.set_pass(val),
        _ => {}
    }
}

impl SystemSettings {
    /// Parse a SETTINGS.TXT blob into self + wifi config.
    pub fn parse_txt(&mut self, data: &[u8], wifi: &mut WifiConfig) {
        for line in data.split(|&b| b == b'\n') {
            let line = trim(line);
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            if let Some(eq) = line.iter().position(|&b| b == b'=') {
                let key = trim(&line[..eq]);
                let val = trim(&line[eq + 1..]);
                apply_setting(key, val, self, wifi);
            }
        }
    }
}

struct TxtWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> TxtWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn put(&mut self, data: &[u8]) {
        let n = data.len().min(self.buf.len() - self.pos);
        self.buf[self.pos..self.pos + n].copy_from_slice(&data[..n]);
        self.pos += n;
    }

    fn put_u16(&mut self, val: u16) {
        if val == 0 {
            self.put(b"0");
            return;
        }
        let mut digits = [0u8; 5];
        let mut i = 5;
        let mut v = val;
        while v > 0 {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        self.put(&digits[i..5]);
    }

    fn kv_num(&mut self, key: &[u8], val: u16) {
        self.put(key);
        self.put(b"=");
        self.put_u16(val);
        self.put(b"\n");
    }

    fn kv_str(&mut self, key: &[u8], val: &[u8]) {
        self.put(key);
        self.put(b"=");
        self.put(val);
        self.put(b"\n");
    }
}

impl SystemSettings {
    /// Serialize self + wifi config to SETTINGS.TXT format.
    pub fn write_txt(&self, w: &WifiConfig, buf: &mut [u8]) -> usize {
    let mut wr = TxtWriter::new(buf);
    wr.put(b"# plump settings\n");
    wr.put(b"# lines starting with # are ignored\n\n");

    wr.put(b"# power settings\n");
    wr.kv_num(b"sleep_timeout", self.sleep_timeout);
    wr.kv_num(b"ghost_clear", self.ghost_clear_every as u16);

    wr.put(b"\n# font settings\n");
    wr.kv_num(b"book_font", self.book_font_size_idx as u16);
    wr.kv_num(b"ui_font", self.ui_font_size_idx as u16);

    wr.put(b"\n# reading settings (0=Compact, 1=Default, 2=Relaxed, 3=Spacious)\n");
    wr.kv_num(b"reading_theme", self.reading_theme as u16);

    wr.put(b"\n# display settings\n");
    wr.kv_num(b"sunlight_fix", if self.sunlight_fix { 1 } else { 0 });
    wr.kv_num(b"text_aa", if self.text_aa { 1 } else { 0 });

    wr.put(b"\n# reader settings\n");
    wr.kv_num(b"reader_status", if self.reader_status { 1 } else { 0 });
    wr.put(b"# text alignment (0=Left, 1=Justify)\n");
    wr.kv_num(b"text_alignment", self.text_alignment as u16);

    wr.put(b"\n# control settings\n");
    wr.kv_num(b"swap_buttons", if self.swap_buttons { 1 } else { 0 });

    wr.put(b"\n# wifi credentials for upload mode\n");
    wr.kv_str(b"wifi_ssid", w.ssid.as_bytes());
    wr.kv_str(b"wifi_pass", w.pass.as_bytes());
    wr.pos
    }
}
