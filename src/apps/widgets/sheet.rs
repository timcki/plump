// bottom sheet: the one overlay component behind the quick menu, the
// reader's contents list and the expanded contents (mockups/
// xteink_x4_reader_contents.html). every state shares the margins,
// border, header (title + tracked caption), meta line, row groups and
// hint footer; only the height and the rows differ, so growing from
// the menu into the contents keeps everything the eye anchors on in
// place.
//
// the widget is stateless: callers own selection and scrolling and
// describe rows one at a time through `RowSpec`. geometry comes from
// `SheetGeom`, either anchored to the bottom edge with a height that
// fits its rows (sheet S / M) or filling the screen to the top margin
// (sheet L).

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, RoundedRectangle};

use crate::board::layout::{CX_BACK, CX_CONFIRM, CX_LEFT, CX_RIGHT};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::StripBuffer;
use crate::fonts::{self, bitmap::BitmapFont};
use crate::ui::Region;

use super::row::{self, RowEdges, RowFonts};
pub use super::row::{ICON_BOOKMARK, RowLead, RowSpec, ValueChip, draw_ellipsized};

pub const SHEET_MARGIN: u16 = 20;
pub const SHEET_X: u16 = SHEET_MARGIN;
pub const SHEET_W: u16 = SCREEN_W - 2 * SHEET_MARGIN;
pub const SHEET_BOTTOM: u16 = SCREEN_H - SHEET_MARGIN;

// vertical budget, top to bottom: pad, title line, meta line (which
// carries the gap to the first group), rows, hint line, pad
const PAD: u16 = 12;
const HEADER_H: u16 = 30;
const META_H: u16 = 24;
pub use super::row::ROW_H;
const GROUP_GAP: u16 = 10;
const HINT_H: u16 = 26;

const R_SHEET: u32 = 10;
const BORDER_W: u32 = 2;

const CELL_GAP: u16 = 10;

pub const ICON_LIST: char = '\u{E2F2}';
pub const ICON_ERASER: char = '\u{E21E}';
pub const ICON_HOUSE: char = '\u{E2C2}';
pub const ICON_MOON: char = '\u{E330}';
pub const ICON_TEXT_AA: char = '\u{E6EE}';
pub const ICON_PLAY: char = '\u{E3D0}';
pub const ICON_ARROWS_CLOCKWISE: char = '\u{E094}';
pub const ICON_TRASH: char = '\u{E4A6}';

/// Fonts a sheet draws with: heading for the title, body for rows,
/// the chrome font for the meta line, values and hints, and the
/// Phosphor face for row icons.
#[derive(Clone, Copy)]
pub struct SheetFonts {
    pub title: &'static BitmapFont,
    pub body: &'static BitmapFont,
    pub small: &'static BitmapFont,
    pub icon: &'static BitmapFont,
}

impl SheetFonts {
    /// The three faces a row needs. `text` is the default voice for a
    /// row; callers pass a book face per row through `RowSpec`.
    pub fn rows(&self) -> RowFonts {
        RowFonts {
            text: self.body,
            small: self.small,
            icon: self.icon,
        }
    }

    pub fn for_ui(idx: u8) -> Self {
        Self {
            title: fonts::ui_heading_font(idx),
            body: fonts::ui_body_font(idx),
            small: fonts::chrome_font(),
            icon: fonts::icon_font(1),
        }
    }

    /// Height a row of this sheet needs for a title set in `text`,
    /// plus the sub line or bar it stacks under it.
    pub fn row_h(&self, text: &BitmapFont, sub: bool, bar: bool) -> u16 {
        row::row_height(text, self.small, sub, bar)
    }
}

/// Where the sheet sits and how many rows it holds. Rows are laid out
/// as one flat list; `group_break` names the last row of the first
/// group, after which a gap and a second outlined group follow.
///
/// Row height is per group and comes from the caller, not from a
/// constant: a sheet whose rows carry a sub line needs more room than
/// one whose rows are a single label, and both need more as the user
/// steps the UI font up. The two scopes of the book cache sheet sit
/// over a breakdown set in the chrome font, which is the case the
/// second height exists for.
#[derive(Clone, Copy, Debug)]
pub struct SheetGeom {
    pub region: Region,
    pub rows: usize,
    pub group_break: Option<usize>,
    row_h: [u16; 2],
}

impl SheetGeom {
    const fn fixed_h(group_break: Option<usize>) -> u16 {
        let gap = if group_break.is_some() { GROUP_GAP } else { 0 };
        2 * PAD + HEADER_H + META_H + gap + HINT_H
    }

    /// rows belonging to the first group
    const fn first_rows(rows: usize, group_break: Option<usize>) -> usize {
        match group_break {
            Some(b) if b + 1 < rows => b + 1,
            _ => rows,
        }
    }

    const fn rows_h(rows: usize, group_break: Option<usize>, row_h: [u16; 2]) -> u16 {
        let first = Self::first_rows(rows, group_break);
        first as u16 * row_h[0] + (rows - first) as u16 * row_h[1]
    }

    /// Bottom-anchored sheet sized to `rows`.
    pub const fn anchored(rows: usize, group_break: Option<usize>, row_h: u16) -> Self {
        Self::anchored_groups(rows, group_break, [row_h, row_h])
    }

    /// Bottom-anchored sheet whose two groups have different row
    /// heights.
    pub const fn anchored_groups(
        rows: usize,
        group_break: Option<usize>,
        row_h: [u16; 2],
    ) -> Self {
        let h = Self::fixed_h(group_break) + Self::rows_h(rows, group_break, row_h);
        Self {
            region: Region::new(SHEET_X, SHEET_BOTTOM - h, SHEET_W, h),
            rows,
            group_break,
            row_h,
        }
    }

    /// Sheet grown to the top margin; holds as many rows as fit.
    pub const fn full(group_break: Option<usize>, row_h: u16) -> Self {
        let h = SHEET_BOTTOM - SHEET_MARGIN;
        let rows = ((h - Self::fixed_h(group_break)) / row_h) as usize;
        Self {
            region: Region::new(SHEET_X, SHEET_MARGIN, SHEET_W, h),
            rows,
            group_break,
            row_h: [row_h, row_h],
        }
    }

    /// Height of row `i`.
    pub const fn row_h(&self, i: usize) -> u16 {
        match self.group_break {
            Some(b) if i > b => self.row_h[1],
            _ => self.row_h[0],
        }
    }

    fn inner_x(&self) -> u16 {
        self.region.x + PAD
    }

    fn inner_w(&self) -> u16 {
        self.region.w - 2 * PAD
    }

    pub fn header_region(&self) -> Region {
        Region::new(
            self.inner_x(),
            self.region.y + PAD,
            self.inner_w(),
            HEADER_H,
        )
    }

    pub fn meta_region(&self) -> Region {
        Region::new(
            self.inner_x(),
            self.region.y + PAD + HEADER_H,
            self.inner_w(),
            META_H - 6,
        )
    }

    fn rows_y(&self) -> u16 {
        self.region.y + PAD + HEADER_H + META_H
    }

    pub fn row_region(&self, i: usize) -> Region {
        let (gap, above) = match self.group_break {
            Some(b) if i > b => {
                let first = Self::first_rows(self.rows, self.group_break);
                (
                    GROUP_GAP,
                    first as u16 * self.row_h[0]
                        + i.saturating_sub(first) as u16 * self.row_h[1],
                )
            }
            _ => (0, i as u16 * self.row_h[0]),
        };
        Region::new(
            self.inner_x(),
            self.rows_y() + above + gap,
            self.inner_w(),
            self.row_h(i),
        )
    }

    /// Outline rectangle of group 0 or 1.
    fn group_region(&self, g: usize) -> Option<Region> {
        let (first, last) = match (g, self.group_break) {
            (0, Some(b)) => (0, b.min(self.rows.saturating_sub(1))),
            (0, None) => (0, self.rows.saturating_sub(1)),
            (1, Some(b)) if b + 1 < self.rows => (b + 1, self.rows - 1),
            _ => return None,
        };
        if self.rows == 0 {
            return None;
        }
        let top = self.row_region(first);
        let bottom = self.row_region(last);
        // one px taller than its rows: the bottom stroke then sits
        // where the next separator would, so every row is framed by
        // a line above and a line below at the same distance
        Some(Region::new(
            top.x,
            top.y,
            top.w,
            bottom.y + bottom.h + 1 - top.y,
        ))
    }

    pub fn hint_region(&self) -> Region {
        Region::new(
            self.inner_x(),
            self.region.y + self.region.h - PAD - HINT_H + 6,
            self.inner_w(),
            HINT_H - 6,
        )
    }
}

fn rounded(strip: &mut StripBuffer, r: Region, radius: u32, style: PrimitiveStyle<BinaryColor>) {
    RoundedRectangle::with_equal_corners(r.to_rect(), Size::new(radius, radius))
        .into_styled(style)
        .draw(strip)
        .ok();
}

/// White fill plus the 2 px border of the whole sheet.
pub fn draw_frame(strip: &mut StripBuffer, geom: &SheetGeom) {
    let r = geom.region;
    if !r.intersects(strip.logical_window()) {
        return;
    }
    rounded(
        strip,
        r,
        R_SHEET,
        PrimitiveStyle::with_fill(BinaryColor::Off),
    );
    rounded(
        strip,
        r,
        R_SHEET,
        PrimitiveStyle::with_stroke(BinaryColor::On, BORDER_W),
    );
}

/// Title on the left in the heading face, tracked uppercase caption on
/// the right, meta line beneath in the chrome font.
pub fn draw_header(
    strip: &mut StripBuffer,
    geom: &SheetGeom,
    fonts: &SheetFonts,
    title: &str,
    caption: &str,
    meta: &str,
) {
    let hdr = geom.header_region();
    if hdr.intersects(strip.logical_window()) {
        let cap_w = tracked_width(fonts.small, caption, 2);
        let cap_x = hdr.x + hdr.w - cap_w;
        let title_w = hdr.w.saturating_sub(cap_w + CELL_GAP);
        draw_ellipsized(
            strip,
            fonts.title,
            Region::new(hdr.x, hdr.y, title_w, hdr.h),
            title,
            BinaryColor::On,
        );
        let baseline = hdr.y as i32 + ((hdr.h + fonts.small.ascent) / 2) as i32;
        draw_tracked(
            strip,
            fonts.small,
            caption,
            2,
            cap_x as i32,
            baseline,
            BinaryColor::On,
        );
    }
    let meta_r = geom.meta_region();
    if meta_r.intersects(strip.logical_window()) && !meta.is_empty() {
        draw_ellipsized(strip, fonts.small, meta_r, meta, BinaryColor::On);
    }
}

/// Hairline outline around each row group.
pub fn draw_groups(strip: &mut StripBuffer, geom: &SheetGeom) {
    for g in 0..2 {
        if let Some(r) = geom.group_region(g) {
            row::draw_group_outline(strip, r);
        }
    }
}

/// One row of the sheet. The sheet decides the rectangle and where
/// the row sits in its group; `row::draw` decides everything inside.
pub fn draw_row(
    strip: &mut StripBuffer,
    geom: &SheetGeom,
    i: usize,
    fonts: &SheetFonts,
    spec: &RowSpec<'_>,
) {
    let edges = RowEdges {
        first: i == 0 || geom.group_break == Some(i - 1),
        last: geom.group_break == Some(i) || i + 1 == geom.rows,
    };
    row::draw(strip, geom.row_region(i), edges, &fonts.rows(), spec);
}

/// Left and right are marked with angle quotes (`\u{2039}` / `\u{203A}`)
/// wherever the bezel steps a value in place: a hint, or the value
/// cell of a cycle row. They used to be solid triangles, which put
/// more ink beside a word than the word itself carried and made the
/// eye read the marker before the value. Navigation that actually
/// goes somewhere uses arrows instead (see `ui::chrome::top_status`).
///
/// Where a hint sits on the footer line: centred over the bezel
/// button it names. the bottom edge carries Back, OK, Left and
/// Right; Up and Down on the side need no label.
#[derive(Clone, Copy)]
pub enum HintSlot {
    Back,
    Ok,
    Left,
    Right,
    /// one label spanning Left and Right ("\u{2039} ADJUST \u{203A}")
    LeftRight,
}

impl HintSlot {
    fn center_x(self) -> u16 {
        match self {
            Self::Back => CX_BACK,
            Self::Ok => CX_CONFIRM,
            Self::Left => CX_LEFT,
            Self::Right => CX_RIGHT,
            Self::LeftRight => (CX_LEFT + CX_RIGHT) / 2,
        }
    }
}

/// Hint footer: tracked uppercase labels, each centred over the
/// physical button it belongs to and kept inside the sheet.
pub fn draw_hints(
    strip: &mut StripBuffer,
    geom: &SheetGeom,
    fonts: &SheetFonts,
    hints: &[(HintSlot, &str)],
) {
    let r = geom.hint_region();
    if !r.intersects(strip.logical_window()) {
        return;
    }
    let font = fonts.small;
    let baseline = r.y as i32 + ((r.h + font.ascent) / 2) as i32;
    for &(slot, text) in hints {
        let w = tracked_width(font, text, 1);
        let x = slot
            .center_x()
            .saturating_sub(w / 2)
            .min((r.x + r.w).saturating_sub(w))
            .max(r.x);
        draw_tracked(strip, font, text, 1, x as i32, baseline, BinaryColor::On);
    }
}

pub fn tracked_width(font: &BitmapFont, text: &str, tracking: u16) -> u16 {
    let mut w = 0u16;
    let mut utf8 = [0u8; 4];
    for (i, ch) in text.chars().enumerate() {
        w += font.measure_str(ch.encode_utf8(&mut utf8));
        if i > 0 {
            w += tracking;
        }
    }
    w
}

pub fn draw_tracked(
    strip: &mut StripBuffer,
    font: &BitmapFont,
    text: &str,
    tracking: u16,
    mut x: i32,
    baseline: i32,
    fg: BinaryColor,
) {
    for ch in text.chars() {
        let adv = font.draw_char_fg(strip, ch, fg, x, baseline);
        x += adv as i32 + tracking as i32;
    }
}

