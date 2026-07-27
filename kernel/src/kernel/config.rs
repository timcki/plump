// system configuration: key=value text in _PLUMP/SETTINGS.TXT
//
// SystemSettings and WifiConfig are kernel-owned configuration;
// the SettingsApp in apps/ provides the UI for editing them

pub const SETTINGS_FILE: &str = "SETTINGS.TXT";
pub const SETTINGS_BUF_CAP: usize = 768;

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

// reader font family (0 = Bookerly, 1 = Atkinson Hyperlegible). the
// distro maps this u8 onto its `ReaderFont` enum; the kernel stores it
// opaquely so the kernel/distro split is preserved.
pub const DEFAULT_READER_FONT: u8 = 0;
pub const NUM_READER_FONTS: u8 = 2;

// reading themes: named margin presets. spacing is a separate
// setting (`line_spacing`); a theme only positions the text block.
//
// theme index is stored as a single u8 in SETTINGS.TXT:
//   0 = Compact   – narrow margins, max content
//   1 = Default   – balanced for most books
//   2 = Relaxed   – wider margins, easier on the eyes
//   3 = Spacious  – large margins, paperback feel

pub const NUM_READING_THEMES: u8 = 4;
pub const DEFAULT_READING_THEME: u8 = 1;

#[derive(Clone, Copy)]
pub struct ReadingTheme {
    pub name: &'static str,
    pub margin_h: u16, // horizontal margin in pixels
    pub margin_v: u16, // vertical margin (top offset) in pixels
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
    },
    ReadingTheme {
        name: "Default",
        margin_h: 16,
        margin_v: 4,
    },
    ReadingTheme {
        name: "Relaxed",
        margin_h: 24,
        margin_v: 8,
    },
    ReadingTheme {
        name: "Spacious",
        margin_h: 40,
        margin_v: 12,
    },
];

// line spacing: percentage of the font's em size (the size tier's
// rasterisation px), so the same step reads identically in every
// family. the renderer clamps the result to the family's native line
// height, so the bottom step can sit slightly looser on tall faces
// (Bookerly's native metric is 1.35 em).
pub const NUM_LINE_SPACINGS: u8 = 5;
pub const DEFAULT_LINE_SPACING: u8 = 2;
pub const LINE_SPACING_PCT: [u16; NUM_LINE_SPACINGS as usize] = [130, 145, 160, 180, 200];

/// Spacing step -> percent of em; out-of-range falls back to default.
pub fn line_spacing_pct(idx: u8) -> u16 {
    let i = (idx as usize).min(LINE_SPACING_PCT.len() - 1);
    LINE_SPACING_PCT[i]
}

// settings files written before the spacing split carry only a theme;
// seed the new setting from the spacing the old theme bundled
// (Compact 100% / Default 120% / Relaxed 140% / Spacious 160% of the
// native metric, mapped to the nearest em step).
const LEGACY_THEME_SPACING: [u8; NUM_READING_THEMES as usize] = [0, 2, 3, 4];

// text alignment for the reader (0 = Left, 1 = Justify).
// the v1 mockup assumes justified body text; users can flip to left
// via Settings if they prefer.
pub const NUM_TEXT_ALIGNMENTS: u8 = 2;
pub const DEFAULT_TEXT_ALIGNMENT: u8 = 1;

const TEXT_ALIGNMENT_NAMES: &[&str] = &["Left", "Justify"];

#[derive(Clone, Copy)]
pub struct SystemSettings {
    // power settings
    pub sleep_timeout: u16,    // minutes idle before sleep; 0 = never
    pub ghost_clear_every: u8, // partial refreshes before forced full GC

    // font settings
    pub book_font_size_idx: u8, // 0 = XSmall, 1 = Small, 2 = Medium, 3 = Large, 4 = XLarge
    pub ui_font_size_idx: u8,   // 0 = XSmall, 1 = Small, 2 = Medium, 3 = Large, 4 = XLarge
    pub reader_font: u8,        // 0 = Bookerly, 1 = Atkinson Hyperlegible

    // reading settings
    pub reading_theme: u8, // index into READING_THEMES (margins)
    pub line_spacing: u8,  // index into LINE_SPACING_PCT

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
            reader_font: DEFAULT_READER_FONT,
            reading_theme: DEFAULT_READING_THEME,
            line_spacing: DEFAULT_LINE_SPACING,
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
        self.reader_font = self.reader_font.min(NUM_READER_FONTS - 1);
        self.reading_theme = self.reading_theme.min(NUM_READING_THEMES - 1);
        self.line_spacing = self.line_spacing.min(NUM_LINE_SPACINGS - 1);
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

fn parse_bool(val: &[u8]) -> bool {
    matches!(val, b"1" | b"true")
}

impl SystemSettings {
    fn apply_setting(&mut self, key: &[u8], val: &[u8], wifi: &mut WifiConfig) {
        match key {
            b"sleep_timeout" => {
                if let Some(v) = parse_u16(val) {
                    self.sleep_timeout = v;
                }
            }
            b"ghost_clear" => {
                if let Some(v) = parse_u16(val) {
                    self.ghost_clear_every = v as u8;
                }
            }
            b"book_font" => {
                if let Some(v) = parse_u16(val) {
                    self.book_font_size_idx = v as u8;
                }
            }
            b"ui_font" => {
                if let Some(v) = parse_u16(val) {
                    self.ui_font_size_idx = v as u8;
                }
            }
            b"reader_font" => {
                if let Some(v) = parse_u16(val) {
                    self.reader_font = v as u8;
                }
            }
            b"reading_theme" => {
                if let Some(v) = parse_u16(val) {
                    self.reading_theme = v as u8;
                }
            }
            b"line_spacing" => {
                if let Some(v) = parse_u16(val) {
                    self.line_spacing = v as u8;
                }
            }
            b"swap_buttons" => self.swap_buttons = parse_bool(val),
            b"sunlight_fix" => self.sunlight_fix = parse_bool(val),
            b"text_aa" => self.text_aa = parse_bool(val),
            b"reader_status" => self.reader_status = parse_bool(val),
            b"text_alignment" => {
                if let Some(v) = parse_u16(val) {
                    self.text_alignment = v as u8;
                }
            }
            b"wifi_ssid" => wifi.set_ssid(val),
            b"wifi_pass" => wifi.set_pass(val),
            _ => {}
        }
    }

    /// Parse a SETTINGS.TXT blob into self + wifi config.
    pub fn parse_txt(&mut self, data: &[u8], wifi: &mut WifiConfig) {
        let mut saw_line_spacing = false;
        for line in data.split(|&b| b == b'\n') {
            let line = trim(line);
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            if let Some(eq) = line.iter().position(|&b| b == b'=') {
                let key = trim(&line[..eq]);
                let val = trim(&line[eq + 1..]);
                saw_line_spacing |= key == b"line_spacing";
                self.apply_setting(key, val, wifi);
            }
        }
        // pre-split settings file: derive spacing from the old theme
        if !saw_line_spacing {
            let t = (self.reading_theme as usize).min(LEGACY_THEME_SPACING.len() - 1);
            self.line_spacing = LEGACY_THEME_SPACING[t];
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
    wr.put(b"# reader font (0=Bookerly, 1=Atkinson)\n");
    wr.kv_num(b"reader_font", self.reader_font as u16);

    wr.put(b"\n# reading settings (0=Compact, 1=Default, 2=Relaxed, 3=Spacious)\n");
    wr.kv_num(b"reading_theme", self.reading_theme as u16);
    wr.put(b"# line spacing (0=1.30x 1=1.45x 2=1.60x 3=1.80x 4=2.00x of em)\n");
    wr.kv_num(b"line_spacing", self.line_spacing as u16);

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
