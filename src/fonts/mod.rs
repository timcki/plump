// build-time rasterised bitmap fonts for e-ink rendering
// TTFs rasterised by build.rs via fontdue into 2-bit tables in flash
// zero heap, zero parsing at runtime
//
// font families:
//   Bookerly  reader (default)
//   Atkinson  reader (selectable for legibility)
//   Inter     UI everywhere (chrome, menus, status bars, button labels)
//
// five size tiers: 0=XSmall  1=Small  2=Medium  3=Large  4=XLarge

pub mod bitmap;

#[allow(clippy::all)]
pub mod font_data {
    include!(concat!(env!("OUT_DIR"), "/font_data.rs"));
}

use crate::drivers::strip::StripBuffer;
use bitmap::BitmapFont;

pub const FONT_SIZE_COUNT: usize = 5;

pub const FONT_SIZE_NAMES: &[&str] = &["XSmall", "Small", "Medium", "Large", "XLarge"];

// font family identifies which TTF source the glyphs were rasterised
// from. Inter is reserved for UI; Bookerly and Atkinson are user-
// selectable for the reader's body + heading. Phosphor is built but
// deliberately not in this enum: it's an icon font (private-use
// codepoints only), accessed via `icon_font` rather than as a body
// face the user can pick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Bookerly,
    Atkinson,
    Inter,
}

impl Family {
    /// true when the underlying TTF was found at build time. when
    /// false, accessors return stub glyphs and the renderer should
    /// keep its default metrics.
    #[inline]
    pub const fn has_regular(self) -> bool {
        match self {
            Family::Bookerly => font_data::BOOKERLY_HAS_REGULAR,
            Family::Atkinson => font_data::ATKINSON_HAS_REGULAR,
            Family::Inter => font_data::INTER_HAS_REGULAR,
        }
    }
}

// reader-only font choice — subset of Family that excludes Inter
// (Inter is never selectable as a reader font).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReaderFont {
    Bookerly,
    Atkinson,
}

pub const NUM_READER_FONTS: u8 = 2;
pub const READER_FONT_NAMES: &[&str] = &["Bookerly", "Atkinson"];

impl ReaderFont {
    #[inline]
    pub const fn family(self) -> Family {
        match self {
            Self::Bookerly => Family::Bookerly,
            Self::Atkinson => Family::Atkinson,
        }
    }

    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bookerly => "Bookerly",
            Self::Atkinson => "Atkinson",
        }
    }

    // map a u8 from SETTINGS.TXT or the kernel SystemSettings field
    // to the enum. unknown values fall back to Bookerly (the default).
    #[inline]
    pub const fn from_idx(i: u8) -> Self {
        match i {
            1 => Self::Atkinson,
            _ => Self::Bookerly,
        }
    }

    #[inline]
    pub const fn to_idx(self) -> u8 {
        match self {
            Self::Bookerly => 0,
            Self::Atkinson => 1,
        }
    }
}

// pre-resolved body + heading font pair for a given size index (UI).
#[derive(Clone, Copy)]
pub struct UiFonts {
    pub body: &'static BitmapFont,
    pub heading: &'static BitmapFont,
}

impl UiFonts {
    pub fn for_size(idx: u8) -> Self {
        Self {
            body: ui_body_font(idx),
            heading: ui_heading_font(idx),
        }
    }
}

// human-readable name for size index (clamped to valid range)
#[inline]
pub fn font_size_name(idx: u8) -> &'static str {
    FONT_SIZE_NAMES
        .get(idx as usize)
        .copied()
        .unwrap_or("Small")
}

#[inline]
pub const fn max_size_idx() -> u8 {
    (FONT_SIZE_COUNT - 1) as u8
}

// dispatch on (family, idx) to the right rasterised constant.
// idx clamped to the valid range to avoid panics from stale settings.
pub fn body_font(family: Family, idx: u8) -> &'static BitmapFont {
    match (family, idx.min(max_size_idx())) {
        (Family::Bookerly, 0) => &font_data::BOOKERLY_REGULAR_BODY_XSMALL,
        (Family::Bookerly, 1) => &font_data::BOOKERLY_REGULAR_BODY_SMALL,
        (Family::Bookerly, 2) => &font_data::BOOKERLY_REGULAR_BODY_MEDIUM,
        (Family::Bookerly, 3) => &font_data::BOOKERLY_REGULAR_BODY_LARGE,
        (Family::Bookerly, _) => &font_data::BOOKERLY_REGULAR_BODY_XLARGE,
        (Family::Atkinson, 0) => &font_data::ATKINSON_REGULAR_BODY_XSMALL,
        (Family::Atkinson, 1) => &font_data::ATKINSON_REGULAR_BODY_SMALL,
        (Family::Atkinson, 2) => &font_data::ATKINSON_REGULAR_BODY_MEDIUM,
        (Family::Atkinson, 3) => &font_data::ATKINSON_REGULAR_BODY_LARGE,
        (Family::Atkinson, _) => &font_data::ATKINSON_REGULAR_BODY_XLARGE,
        (Family::Inter, 0) => &font_data::INTER_REGULAR_BODY_XSMALL,
        (Family::Inter, 1) => &font_data::INTER_REGULAR_BODY_SMALL,
        (Family::Inter, 2) => &font_data::INTER_REGULAR_BODY_MEDIUM,
        (Family::Inter, 3) => &font_data::INTER_REGULAR_BODY_LARGE,
        (Family::Inter, _) => &font_data::INTER_REGULAR_BODY_XLARGE,
    }
}

pub fn heading_font(family: Family, idx: u8) -> &'static BitmapFont {
    match (family, idx.min(max_size_idx())) {
        (Family::Bookerly, 0) => &font_data::BOOKERLY_REGULAR_HEADING_XSMALL,
        (Family::Bookerly, 1) => &font_data::BOOKERLY_REGULAR_HEADING_SMALL,
        (Family::Bookerly, 2) => &font_data::BOOKERLY_REGULAR_HEADING_MEDIUM,
        (Family::Bookerly, 3) => &font_data::BOOKERLY_REGULAR_HEADING_LARGE,
        (Family::Bookerly, _) => &font_data::BOOKERLY_REGULAR_HEADING_XLARGE,
        (Family::Atkinson, 0) => &font_data::ATKINSON_REGULAR_HEADING_XSMALL,
        (Family::Atkinson, 1) => &font_data::ATKINSON_REGULAR_HEADING_SMALL,
        (Family::Atkinson, 2) => &font_data::ATKINSON_REGULAR_HEADING_MEDIUM,
        (Family::Atkinson, 3) => &font_data::ATKINSON_REGULAR_HEADING_LARGE,
        (Family::Atkinson, _) => &font_data::ATKINSON_REGULAR_HEADING_XLARGE,
        (Family::Inter, 0) => &font_data::INTER_REGULAR_HEADING_XSMALL,
        (Family::Inter, 1) => &font_data::INTER_REGULAR_HEADING_SMALL,
        (Family::Inter, 2) => &font_data::INTER_REGULAR_HEADING_MEDIUM,
        (Family::Inter, 3) => &font_data::INTER_REGULAR_HEADING_LARGE,
        (Family::Inter, _) => &font_data::INTER_REGULAR_HEADING_XLARGE,
    }
}

fn bold_body_font(family: Family, idx: u8) -> &'static BitmapFont {
    match (family, idx.min(max_size_idx())) {
        (Family::Bookerly, 0) => &font_data::BOOKERLY_BOLD_BODY_XSMALL,
        (Family::Bookerly, 1) => &font_data::BOOKERLY_BOLD_BODY_SMALL,
        (Family::Bookerly, 2) => &font_data::BOOKERLY_BOLD_BODY_MEDIUM,
        (Family::Bookerly, 3) => &font_data::BOOKERLY_BOLD_BODY_LARGE,
        (Family::Bookerly, _) => &font_data::BOOKERLY_BOLD_BODY_XLARGE,
        (Family::Atkinson, 0) => &font_data::ATKINSON_BOLD_BODY_XSMALL,
        (Family::Atkinson, 1) => &font_data::ATKINSON_BOLD_BODY_SMALL,
        (Family::Atkinson, 2) => &font_data::ATKINSON_BOLD_BODY_MEDIUM,
        (Family::Atkinson, 3) => &font_data::ATKINSON_BOLD_BODY_LARGE,
        (Family::Atkinson, _) => &font_data::ATKINSON_BOLD_BODY_XLARGE,
        (Family::Inter, 0) => &font_data::INTER_BOLD_BODY_XSMALL,
        (Family::Inter, 1) => &font_data::INTER_BOLD_BODY_SMALL,
        (Family::Inter, 2) => &font_data::INTER_BOLD_BODY_MEDIUM,
        (Family::Inter, 3) => &font_data::INTER_BOLD_BODY_LARGE,
        (Family::Inter, _) => &font_data::INTER_BOLD_BODY_XLARGE,
    }
}

fn italic_body_font(family: Family, idx: u8) -> &'static BitmapFont {
    match (family, idx.min(max_size_idx())) {
        (Family::Bookerly, 0) => &font_data::BOOKERLY_ITALIC_BODY_XSMALL,
        (Family::Bookerly, 1) => &font_data::BOOKERLY_ITALIC_BODY_SMALL,
        (Family::Bookerly, 2) => &font_data::BOOKERLY_ITALIC_BODY_MEDIUM,
        (Family::Bookerly, 3) => &font_data::BOOKERLY_ITALIC_BODY_LARGE,
        (Family::Bookerly, _) => &font_data::BOOKERLY_ITALIC_BODY_XLARGE,
        (Family::Atkinson, 0) => &font_data::ATKINSON_ITALIC_BODY_XSMALL,
        (Family::Atkinson, 1) => &font_data::ATKINSON_ITALIC_BODY_SMALL,
        (Family::Atkinson, 2) => &font_data::ATKINSON_ITALIC_BODY_MEDIUM,
        (Family::Atkinson, 3) => &font_data::ATKINSON_ITALIC_BODY_LARGE,
        (Family::Atkinson, _) => &font_data::ATKINSON_ITALIC_BODY_XLARGE,
        (Family::Inter, 0) => &font_data::INTER_ITALIC_BODY_XSMALL,
        (Family::Inter, 1) => &font_data::INTER_ITALIC_BODY_SMALL,
        (Family::Inter, 2) => &font_data::INTER_ITALIC_BODY_MEDIUM,
        (Family::Inter, 3) => &font_data::INTER_ITALIC_BODY_LARGE,
        (Family::Inter, _) => &font_data::INTER_ITALIC_BODY_XLARGE,
    }
}

// UI helpers: every UI surface uses Inter regardless of the reader
// font selection. these wrappers exist so callers don't have to spell
// `Family::Inter` at every site. when the Inter TTF hasn't been
// supplied to assets/fonts/Inter/, we fall back to Bookerly so the
// device stays legible — the build is otherwise fully functional.
#[inline]
fn ui_family() -> Family {
    if Family::Inter.has_regular() {
        Family::Inter
    } else {
        Family::Bookerly
    }
}

#[inline]
pub fn ui_body_font(idx: u8) -> &'static BitmapFont {
    body_font(ui_family(), idx)
}

#[inline]
pub fn ui_heading_font(idx: u8) -> &'static BitmapFont {
    heading_font(ui_family(), idx)
}

// chrome font (quick-menu items, loading text, status bar)
// always Inter XSmall body for compact display
#[inline]
pub fn chrome_font() -> &'static BitmapFont {
    ui_body_font(0)
}

// button label font (edge-of-screen action labels)
#[inline]
pub fn button_label_font() -> &'static BitmapFont {
    ui_body_font(0)
}

/// Phosphor icon font for chrome widgets. Tab bar uses MEDIUM (idx 2);
/// panel rows pick smaller sizes via the same dispatch.
#[inline]
pub fn icon_font(idx: u8) -> &'static BitmapFont {
    match idx.min(max_size_idx()) {
        0 => &font_data::PHOSPHOR_REGULAR_BODY_XSMALL,
        1 => &font_data::PHOSPHOR_REGULAR_BODY_SMALL,
        2 => &font_data::PHOSPHOR_REGULAR_BODY_MEDIUM,
        3 => &font_data::PHOSPHOR_REGULAR_BODY_LARGE,
        _ => &font_data::PHOSPHOR_REGULAR_BODY_XLARGE,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Regular,
    Bold,
    Italic,
    Heading,
}

impl Style {
    /// Resolve a font style from accumulated markup-state flags.
    ///
    /// Both the K-P typesetting pipeline (`layout::pipeline`) and the
    /// runtime renderer (`apps::reader::mod`/`paging::measure_line`)
    /// funnel through this single resolver, so K-P's measurement and
    /// the renderer's draw widths cannot diverge on nested markup
    /// (e.g. `<b><i>x</i></b>` previously measured as Bold and drew
    /// as Italic, overrunning the column).
    ///
    /// Policy:
    ///   * `h1` / `h2` / `h3` (`hlevel ∈ 1..=3` with `heading=true`)
    ///     resolve to `Heading`.
    ///   * `h4` / `h5` / `h6` resolve to `Bold` — matches the
    ///     `TextStyle::is_h4_h6_bold` intent that low-tier headings
    ///     render as body bold, not heading font.
    ///   * Otherwise `bold` beats `italic` (heading-priority then
    ///     bold-priority, matching the in-scanner flag composition).
    ///
    /// `underline` / `strike` are draw-side decorations only — they
    /// never change glyph metrics, so callers track those separately.
    #[inline]
    pub const fn from_flags(bold: bool, italic: bool, heading: bool, hlevel: u8) -> Self {
        if heading && hlevel >= 1 && hlevel <= 3 {
            Style::Heading
        } else if bold || (heading && hlevel >= 4) {
            Style::Bold
        } else if italic {
            Style::Italic
        } else {
            Style::Regular
        }
    }
}

// complete set of four style variants from a single family at a given
// size tier. missing weights fall back to regular automatically.
#[derive(Clone, Copy)]
pub struct FontSet {
    regular: &'static BitmapFont,
    bold: &'static BitmapFont,
    italic: &'static BitmapFont,
    heading: &'static BitmapFont,
}

impl FontSet {
    fn from_fonts(
        regular: &'static BitmapFont,
        bold_candidate: &'static BitmapFont,
        italic_candidate: &'static BitmapFont,
        heading: &'static BitmapFont,
    ) -> Self {
        let bold = if bold_candidate.glyph('A').advance > 0 {
            bold_candidate
        } else {
            regular
        };
        let italic = if italic_candidate.glyph('A').advance > 0 {
            italic_candidate
        } else {
            regular
        };
        Self {
            regular,
            bold,
            italic,
            heading,
        }
    }

    /// Build a FontSet from any registered family at the given size.
    pub fn for_family(family: Family, idx: u8) -> Self {
        Self::from_fonts(
            body_font(family, idx),
            bold_body_font(family, idx),
            italic_body_font(family, idx),
            heading_font(family, idx),
        )
    }

    /// Build a FontSet for the reader's selected font.
    #[inline]
    pub fn for_reader(font: ReaderFont, idx: u8) -> Self {
        Self::for_family(font.family(), idx)
    }

    /// Build a FontSet for UI. Inter when present; Bookerly fallback
    /// when no Inter TTF was supplied at build time.
    #[inline]
    pub fn for_ui(idx: u8) -> Self {
        Self::for_family(ui_family(), idx)
    }

    #[inline]
    pub fn font(&self, style: Style) -> &'static BitmapFont {
        match style {
            Style::Regular => self.regular,
            Style::Bold => self.bold,
            Style::Italic => self.italic,
            Style::Heading => self.heading,
        }
    }

    #[inline]
    pub fn line_height(&self, style: Style) -> u16 {
        self.font(style).line_height
    }

    #[inline]
    pub fn ascent(&self, style: Style) -> u16 {
        self.font(style).ascent
    }

    /// Em size (rasterisation px) of the body face; the basis for
    /// user line spacing, identical across families at the same tier.
    #[inline]
    pub fn em_px(&self) -> u16 {
        self.regular.em_px
    }

    #[inline]
    pub fn advance(&self, ch: char, style: Style) -> u8 {
        self.font(style).advance(ch)
    }

    #[inline]
    pub fn advance_byte(&self, b: u8, style: Style) -> u8 {
        self.font(style).advance(bitmap::byte_to_char(b))
    }

    #[inline]
    pub fn draw_char(
        &self,
        strip: &mut StripBuffer,
        ch: char,
        style: Style,
        cx: i32,
        baseline: i32,
    ) -> u8 {
        self.font(style).draw_char(strip, ch, cx, baseline)
    }

    pub fn draw_bytes(
        &self,
        strip: &mut StripBuffer,
        text: &[u8],
        style: Style,
        cx: i32,
        baseline: i32,
    ) -> i32 {
        self.font(style).draw_bytes(strip, text, cx, baseline)
    }

    pub fn draw_str(
        &self,
        strip: &mut StripBuffer,
        text: &str,
        style: Style,
        cx: i32,
        baseline: i32,
    ) -> i32 {
        self.font(style).draw_str(strip, text, cx, baseline)
    }
}
