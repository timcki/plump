// what the settings screen is made of.
//
// one declaration per setting: a label, the domain of values it
// accepts, and a get/set pair onto `SystemSettings`. everything else
// (stepping, formatting, what the Select key does) is written once
// against `Domain`, so adding a setting is a `ROWS` entry plus two
// match arms instead of an arm in each of label / format / increment /
// decrement.
//
// `ROWS` is the screen in visual order, captions included, which is
// why there is no logical-to-visual reorder map any more.

use core::fmt::Write as _;

use crate::fonts;
use crate::kernel::config::{
    self, GHOST_CLEAR_STEP, MAX_GHOST_CLEAR, MAX_SLEEP_TIMEOUT, MIN_GHOST_CLEAR,
    NUM_LINE_SPACINGS, NUM_READER_FONTS, NUM_READING_THEMES, NUM_TEXT_ALIGNMENTS,
    SLEEP_TIMEOUT_STEP, SystemSettings,
};
use crate::ui::StackFmt;

/// widest rendered value ("Atkinson", "Spacious", "120 min").
pub const VALUE_CAP: usize = 20;
pub type ValueFmt = StackFmt<VALUE_CAP>;

/// direction of a value change or cursor move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Down,
    Up,
}

impl Step {
    #[inline]
    pub const fn from_hdir(dir: crate::apps::HDir) -> Self {
        match dir {
            crate::apps::HDir::Left => Self::Down,
            crate::apps::HDir::Right => Self::Up,
        }
    }
}

/// how a numeric range renders its value.
#[derive(Clone, Copy)]
pub enum NumFmt {
    /// idle minutes; 0 reads as "Never"
    Minutes,
    /// partial refreshes between forced full clears
    Interval,
    /// index into the line spacing table, shown as a multiplier
    SpacingEm,
}

/// the values a setting accepts.
#[derive(Clone, Copy)]
pub enum Domain {
    /// no value on this screen: the row opens something that owns its
    /// own surface. `get`/`set` are inert for these, and `format`
    /// writes nothing, so the owner supplies the value cell itself
    Action,
    /// two states, each with its own word
    Toggle {
        off: &'static str,
        on: &'static str,
    },
    /// `count` enumerated values rendered by name. a fn pointer rather
    /// than a name table so the existing lookups (font tiers, reader
    /// families, reading themes) stay the single source of the strings
    Named {
        count: u16,
        name: fn(u16) -> &'static str,
    },
    /// numeric value stepped between `min` and `max` inclusive
    Range {
        min: u16,
        max: u16,
        step: u16,
        fmt: NumFmt,
    },
}

/// what the Select key does on a row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// flip in place; no edit session, so Left/Right keep cycling tabs
    Flip,
    /// open an edit session, which captures Left/Right until it closes
    Edit,
    /// hand the screen to the row's own surface (the cache sheet)
    Open,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingId {
    ReaderFont,
    BookFont,
    LineSpacing,
    ReadingTheme,
    TextAlign,
    ReaderStatus,
    UiFont,
    TextAa,
    GhostClear,
    SunlightFix,
    SleepAfter,
    SwapButtons,
    BookCache,
}

/// A read-only figure at the foot of the list. Not a setting: the
/// cursor never lands on one, because `item_at` only answers for
/// `Row::Item`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InfoId {
    Version,
    Storage,
    Battery,
    Uptime,
}

impl InfoId {
    pub const ALL: [Self; 4] = [Self::Version, Self::Storage, Self::Battery, Self::Uptime];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Version => "Firmware",
            Self::Storage => "Book cache",
            Self::Battery => "Battery",
            Self::Uptime => "Awake",
        }
    }
}

/// a visual row: a section caption, an editable setting, or a figure.
pub enum Row {
    Section(&'static str),
    Item(SettingId),
    Info(InfoId),
}

/// the settings screen, top to bottom.
pub const ROWS: &[Row] = &[
    Row::Section("Reading"),
    Row::Item(SettingId::ReaderFont),
    Row::Item(SettingId::BookFont),
    Row::Item(SettingId::LineSpacing),
    Row::Item(SettingId::ReadingTheme),
    Row::Item(SettingId::TextAlign),
    Row::Item(SettingId::ReaderStatus),
    Row::Section("Display"),
    Row::Item(SettingId::UiFont),
    Row::Item(SettingId::TextAa),
    Row::Item(SettingId::GhostClear),
    Row::Item(SettingId::SunlightFix),
    Row::Section("System"),
    Row::Item(SettingId::SleepAfter),
    Row::Item(SettingId::SwapButtons),
    Row::Item(SettingId::BookCache),
    Row::Section("About"),
    Row::Info(InfoId::Version),
    Row::Info(InfoId::Storage),
    Row::Info(InfoId::Battery),
    Row::Info(InfoId::Uptime),
];

/// Where a setting sits on screen, for a caller that has to repaint
/// one row it is not the cursor on.
pub fn row_index(id: SettingId) -> Option<usize> {
    ROWS.iter()
        .position(|r| matches!(r, Row::Item(item) if *item == id))
}

fn font_size_name(v: u16) -> &'static str {
    fonts::font_size_name(v as u8)
}

fn reader_font_name(v: u16) -> &'static str {
    fonts::READER_FONT_NAMES
        .get(v as usize)
        .copied()
        .unwrap_or("Bookerly")
}

fn reading_theme_name(v: u16) -> &'static str {
    config::ReadingTheme::from_idx(v as u8).name
}

fn text_align_name(v: u16) -> &'static str {
    config::text_alignment_name(v as u8)
}

impl SettingId {
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReaderFont => "Reader Font",
            Self::BookFont => "Book Font",
            Self::LineSpacing => "Line Spacing",
            Self::ReadingTheme => "Margins",
            Self::TextAlign => "Text Align",
            Self::ReaderStatus => "Reader Status",
            Self::UiFont => "UI Font",
            Self::TextAa => "Text AA",
            Self::GhostClear => "Ghost Clear",
            Self::SunlightFix => "Sunlight Fix",
            Self::SleepAfter => "Sleep After",
            Self::SwapButtons => "Swap Buttons",
            Self::BookCache => "Book Cache",
        }
    }

    pub const fn domain(self) -> Domain {
        match self {
            Self::ReaderFont => Domain::Named {
                count: NUM_READER_FONTS as u16,
                name: reader_font_name,
            },
            Self::BookFont | Self::UiFont => Domain::Named {
                count: fonts::FONT_SIZE_COUNT as u16,
                name: font_size_name,
            },
            Self::LineSpacing => Domain::Range {
                min: 0,
                max: NUM_LINE_SPACINGS as u16 - 1,
                step: 1,
                fmt: NumFmt::SpacingEm,
            },
            Self::ReadingTheme => Domain::Named {
                count: NUM_READING_THEMES as u16,
                name: reading_theme_name,
            },
            Self::TextAlign => Domain::Named {
                count: NUM_TEXT_ALIGNMENTS as u16,
                name: text_align_name,
            },
            Self::ReaderStatus => Domain::Toggle {
                off: "Hide",
                on: "Show",
            },
            Self::TextAa | Self::SunlightFix => Domain::Toggle {
                off: "Off",
                on: "On",
            },
            Self::GhostClear => Domain::Range {
                min: MIN_GHOST_CLEAR as u16,
                max: MAX_GHOST_CLEAR as u16,
                step: GHOST_CLEAR_STEP as u16,
                fmt: NumFmt::Interval,
            },
            Self::SleepAfter => Domain::Range {
                min: 0,
                max: MAX_SLEEP_TIMEOUT,
                step: SLEEP_TIMEOUT_STEP,
                fmt: NumFmt::Minutes,
            },
            Self::SwapButtons => Domain::Toggle {
                off: "No",
                on: "Yes",
            },
            Self::BookCache => Domain::Action,
        }
    }

    /// The line under the label, for a setting whose label and value
    /// together still do not say what it does. Empty for the ones that
    /// speak for themselves, so the list stays scannable rather than
    /// becoming a manual.
    pub const fn sub(self) -> &'static str {
        match self {
            Self::ReaderFont => "the face the book itself is set in",
            Self::ReaderStatus => "title, chapter bar and page count",
            Self::TextAa => "grey glyph edges, one extra pass a page",
            Self::GhostClear => "full clear after this many page turns",
            Self::SunlightFix => "powers the panel down after each refresh",
            Self::SwapButtons => "next page on the upper button",
            Self::BookCache => "text, layout and figures kept on the card",
            _ => "",
        }
    }

    /// Booleans flip under Select; everything else opens an editor.
    pub const fn activation(self) -> Activation {
        match self.domain() {
            Domain::Toggle { .. } => Activation::Flip,
            Domain::Action => Activation::Open,
            _ => Activation::Edit,
        }
    }

    /// True when changing this setting re-lays out the settings screen
    /// itself, so the caller has to repaint more than the value cell.
    #[inline]
    pub const fn resizes_ui(self) -> bool {
        matches!(self, Self::UiFont)
    }

    pub fn get(self, s: &SystemSettings) -> u16 {
        match self {
            Self::ReaderFont => s.reader_font as u16,
            Self::BookFont => s.book_font_size_idx as u16,
            Self::LineSpacing => s.line_spacing as u16,
            Self::ReadingTheme => s.reading_theme as u16,
            Self::TextAlign => s.text_alignment as u16,
            Self::ReaderStatus => s.reader_status as u16,
            Self::UiFont => s.ui_font_size_idx as u16,
            Self::TextAa => s.text_aa as u16,
            Self::GhostClear => s.ghost_clear_every as u16,
            Self::SunlightFix => s.sunlight_fix as u16,
            Self::SleepAfter => s.sleep_timeout,
            Self::SwapButtons => s.swap_buttons as u16,
            Self::BookCache => 0,
        }
    }

    pub fn set(self, s: &mut SystemSettings, v: u16) {
        match self {
            Self::ReaderFont => s.reader_font = v as u8,
            Self::BookFont => s.book_font_size_idx = v as u8,
            Self::LineSpacing => s.line_spacing = v as u8,
            Self::ReadingTheme => s.reading_theme = v as u8,
            Self::TextAlign => s.text_alignment = v as u8,
            Self::ReaderStatus => s.reader_status = v != 0,
            Self::UiFont => s.ui_font_size_idx = v as u8,
            Self::TextAa => s.text_aa = v != 0,
            Self::GhostClear => s.ghost_clear_every = v as u8,
            Self::SunlightFix => s.sunlight_fix = v != 0,
            Self::SleepAfter => s.sleep_timeout = v,
            Self::SwapButtons => s.swap_buttons = v != 0,
            Self::BookCache => {}
        }
    }

    /// Move the value one step in `dir`, clamped to the domain.
    /// Returns false when it was already at that end.
    pub fn step(self, s: &mut SystemSettings, dir: Step) -> bool {
        let cur = self.get(s);
        let next = match self.domain() {
            // nothing to step: the row opens a surface instead
            Domain::Action => return false,
            // a toggle has no ends to hit: either direction flips it
            Domain::Toggle { .. } => u16::from(cur == 0),
            Domain::Named { count, .. } => clamped(cur, dir, 0, count.saturating_sub(1), 1),
            Domain::Range {
                min, max, step, ..
            } => clamped(cur, dir, min, max, step),
        };
        if next == cur {
            return false;
        }
        self.set(s, next);
        true
    }

    pub fn format(self, s: &SystemSettings, out: &mut ValueFmt) {
        out.clear();
        let v = self.get(s);
        match self.domain() {
            Domain::Action => {}
            Domain::Toggle { off, on } => {
                let _ = out.write_str(if v != 0 { on } else { off });
            }
            Domain::Named { count, name } => {
                let _ = out.write_str(name(v.min(count.saturating_sub(1))));
            }
            Domain::Range { fmt, .. } => match fmt {
                NumFmt::Minutes if v == 0 => {
                    let _ = out.write_str("Never");
                }
                NumFmt::Minutes => {
                    let _ = write!(out, "{} min", v);
                }
                NumFmt::Interval => {
                    let _ = write!(out, "Every {}", v);
                }
                NumFmt::SpacingEm => {
                    let pct = config::line_spacing_pct(v as u8);
                    let _ = write!(out, "{}.{:02}x", pct / 100, pct % 100);
                }
            },
        }
    }
}

fn clamped(cur: u16, dir: Step, min: u16, max: u16, step: u16) -> u16 {
    match dir {
        Step::Up => cur.saturating_add(step).min(max),
        // a stale file can hold a value below min; clamp up rather than
        // letting the subtraction park it there
        Step::Down => cur.saturating_sub(step).max(min).min(max),
    }
}
