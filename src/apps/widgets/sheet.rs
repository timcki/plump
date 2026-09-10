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

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{CornerRadii, PrimitiveStyle, Rectangle, RoundedRectangle};

use crate::board::layout::{CX_BACK, CX_CONFIRM, CX_LEFT, CX_RIGHT};
use crate::board::{SCREEN_H, SCREEN_W};
use crate::drivers::strip::StripBuffer;
use crate::fonts::{self, bitmap::BitmapFont};
use crate::ui::{Alignment, Region, StackFmt};

pub const SHEET_MARGIN: u16 = 20;
pub const SHEET_X: u16 = SHEET_MARGIN;
pub const SHEET_W: u16 = SCREEN_W - 2 * SHEET_MARGIN;
pub const SHEET_BOTTOM: u16 = SCREEN_H - SHEET_MARGIN;

// vertical budget, top to bottom: pad, title line, meta line (which
// carries the gap to the first group), rows, hint line, pad
const PAD: u16 = 12;
const HEADER_H: u16 = 30;
const META_H: u16 = 24;
pub const ROW_H: u16 = 44;
const GROUP_GAP: u16 = 10;
const HINT_H: u16 = 26;

const R_SHEET: u32 = 10;
const R_GROUP: u32 = 6;
const BORDER_W: u32 = 2;

// row cells: lead column (icon or chapter number), text, value
const CELL_PAD: u16 = 12;
const LEAD_W: u16 = 28;
const CELL_GAP: u16 = 10;

// position bar under the title of the chapter being read
const BAR_W: u16 = 120;
const BAR_H: u16 = 3;

pub const ICON_BOOKMARK: char = '\u{E0EA}';
pub const ICON_LIST: char = '\u{E2F2}';
pub const ICON_ERASER: char = '\u{E21E}';
pub const ICON_HOUSE: char = '\u{E2C2}';
pub const ICON_MOON: char = '\u{E330}';
pub const ICON_TEXT_AA: char = '\u{E6EE}';
pub const ICON_PLAY: char = '\u{E3D0}';

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
    pub fn for_ui(idx: u8) -> Self {
        Self {
            title: fonts::ui_heading_font(idx),
            body: fonts::ui_body_font(idx),
            small: fonts::chrome_font(),
            icon: fonts::icon_font(1),
        }
    }
}

/// Where the sheet sits and how many rows it holds. Rows are laid out
/// as one flat list; `group_break` names the last row of the first
/// group, after which a gap and a second outlined group follow.
#[derive(Clone, Copy, Debug)]
pub struct SheetGeom {
    pub region: Region,
    pub rows: usize,
    pub group_break: Option<usize>,
}

impl SheetGeom {
    const fn fixed_h(group_break: Option<usize>) -> u16 {
        let gap = if group_break.is_some() { GROUP_GAP } else { 0 };
        2 * PAD + HEADER_H + META_H + gap + HINT_H
    }

    /// Bottom-anchored sheet sized to `rows`.
    pub const fn anchored(rows: usize, group_break: Option<usize>) -> Self {
        let h = Self::fixed_h(group_break) + rows as u16 * ROW_H;
        Self {
            region: Region::new(SHEET_X, SHEET_BOTTOM - h, SHEET_W, h),
            rows,
            group_break,
        }
    }

    /// Sheet grown to the top margin; holds as many rows as fit.
    pub const fn full(group_break: Option<usize>) -> Self {
        let h = SHEET_BOTTOM - SHEET_MARGIN;
        let rows = ((h - Self::fixed_h(group_break)) / ROW_H) as usize;
        Self {
            region: Region::new(SHEET_X, SHEET_MARGIN, SHEET_W, h),
            rows,
            group_break,
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
        let gap = match self.group_break {
            Some(b) if i > b => GROUP_GAP,
            _ => 0,
        };
        Region::new(
            self.inner_x(),
            self.rows_y() + i as u16 * ROW_H + gap,
            self.inner_w(),
            ROW_H,
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

/// What sits in the lead column of a row.
#[derive(Clone, Copy)]
pub enum RowLead {
    None,
    Icon(char),
    Number(u16),
    /// bookmark glyph: the chapter being read
    Bookmark,
}

pub struct RowSpec<'a> {
    pub lead: RowLead,
    pub text: &'a str,
    pub text_font: &'static BitmapFont,
    pub value: &'a str,
    pub selected: bool,
    /// (done, total): draws a position bar under the text
    pub progress: Option<(u32, u32)>,
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
        if let Some(r) = geom.group_region(g)
            && r.intersects(strip.logical_window())
        {
            rounded(
                strip,
                r,
                R_GROUP,
                PrimitiveStyle::with_stroke(BinaryColor::On, 1),
            );
        }
    }
}

/// One row: hairline above it inside its group, inverted fill when
/// selected, lead glyph, ellipsized text, right-aligned value.
pub fn draw_row(
    strip: &mut StripBuffer,
    geom: &SheetGeom,
    i: usize,
    fonts: &SheetFonts,
    spec: &RowSpec<'_>,
) {
    let row = geom.row_region(i);
    if !row.intersects(strip.logical_window()) {
        return;
    }

    let first_in_group = i == 0 || geom.group_break == Some(i - 1);
    let last_in_group = geom.group_break == Some(i) || i + 1 == geom.rows;
    if !first_in_group {
        Rectangle::new(
            Point::new((row.x + 1) as i32, row.y as i32),
            Size::new((row.w - 2) as u32, 1),
        )
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(strip)
        .ok();
    }

    // the selected row fills its whole cell, from the line above to
    // the line below and edge to edge inside the outline; the outer
    // corners take the outline's radius so the fill never pokes out
    let fg = if spec.selected {
        let cell = Rectangle::new(
            Point::new((row.x + 1) as i32, (row.y + 1) as i32),
            Size::new((row.w - 2) as u32, (row.h - 1) as u32),
        );
        let r = Size::new(R_GROUP - 1, R_GROUP - 1);
        let z = Size::zero();
        let radii = CornerRadii {
            top_left: if first_in_group { r } else { z },
            top_right: if first_in_group { r } else { z },
            bottom_right: if last_in_group { r } else { z },
            bottom_left: if last_in_group { r } else { z },
        };
        RoundedRectangle::new(cell, radii)
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(strip)
            .ok();
        BinaryColor::Off
    } else {
        BinaryColor::On
    };

    // lead column
    let lead_r = Region::new(row.x + CELL_PAD, row.y, LEAD_W, row.h);
    let glyph = match spec.lead {
        RowLead::None => None,
        RowLead::Icon(ch) => Some(ch),
        RowLead::Bookmark => Some(ICON_BOOKMARK),
        RowLead::Number(n) => {
            let mut s = StackFmt::<8>::new();
            let _ = write!(s, "{}", n);
            fonts
                .small
                .draw_aligned(strip, lead_r, s.as_str(), Alignment::CenterRight, fg);
            None
        }
    };
    if let Some(ch) = glyph {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        fonts
            .icon
            .draw_aligned(strip, lead_r, s, Alignment::CenterRight, fg);
    }

    // value, right aligned, reserves its measured width
    let value_w = if spec.value.is_empty() {
        0
    } else {
        fonts.small.measure_str(spec.value)
    };
    if value_w > 0 {
        let value_r = Region::new(row.x + row.w - CELL_PAD - value_w, row.y, value_w, row.h);
        fonts
            .small
            .draw_aligned(strip, value_r, spec.value, Alignment::CenterRight, fg);
    }

    // text, with the position bar below it for the chapter being read
    let text_x = row.x + CELL_PAD + LEAD_W + CELL_GAP;
    let text_w = (row.x + row.w - CELL_PAD)
        .saturating_sub(text_x)
        .saturating_sub(if value_w > 0 { value_w + CELL_GAP } else { 0 });
    match spec.progress {
        None => {
            draw_ellipsized(
                strip,
                spec.text_font,
                Region::new(text_x, row.y, text_w, row.h),
                spec.text,
                fg,
            );
        }
        Some((done, total)) => {
            let text_h = row.h - BAR_H - 10;
            draw_ellipsized(
                strip,
                spec.text_font,
                Region::new(text_x, row.y + 2, text_w, text_h),
                spec.text,
                fg,
            );
            let bar_y = row.y + row.h - BAR_H - 7;
            let bar_w = BAR_W.min(text_w);
            // track: 1 px line, fill: full height up to the position
            Rectangle::new(
                Point::new(text_x as i32, (bar_y + BAR_H / 2) as i32),
                Size::new(bar_w as u32, 1),
            )
            .into_styled(PrimitiveStyle::with_fill(fg))
            .draw(strip)
            .ok();
            let filled = if total == 0 {
                0
            } else {
                ((bar_w as u32 * done.min(total)) / total).max(1) as u16
            };
            Rectangle::new(
                Point::new(text_x as i32, bar_y as i32),
                Size::new(filled as u32, BAR_H as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(fg))
            .draw(strip)
            .ok();
        }
    }
}

/// Where a hint sits on the footer line: centred over the bezel
/// button it names. the bottom edge carries Back, OK, Left and
/// Right; Up and Down on the side need no label.
#[derive(Clone, Copy)]
pub enum HintSlot {
    Back,
    Ok,
    Left,
    Right,
    /// one label spanning Left and Right ("◀ ADJUST ▶")
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

/// Left-aligned, vertically centred text cut with an ellipsis when it
/// overflows `region`.
pub fn draw_ellipsized(
    strip: &mut StripBuffer,
    font: &BitmapFont,
    region: Region,
    text: &str,
    fg: BinaryColor,
) {
    let cut = font.truncate_len(text, region.w);
    if cut >= text.len() {
        font.draw_aligned(strip, region, text, Alignment::CenterLeft, fg);
        return;
    }
    let mut buf = [0u8; 96];
    let mut n = cut.min(buf.len() - 3);
    while n > 0 && !text.is_char_boundary(n) {
        n -= 1;
    }
    buf[..n].copy_from_slice(&text.as_bytes()[..n]);
    buf[n..n + 3].copy_from_slice("\u{2026}".as_bytes());
    let cut_text = core::str::from_utf8(&buf[..n + 3]).unwrap_or(text);
    font.draw_aligned(strip, region, cut_text, Alignment::CenterLeft, fg);
}
