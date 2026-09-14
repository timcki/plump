// the one list row.
//
// every list on the device is made of this: the reader's menu and
// contents sheets, home's recent list, the library, the stats book
// list, the settings screen. mockups/xteink_x4_tab_screens_v2.html.
//
//   [ lead ][ title              ][ value ]
//           [ sub line or bar    ]
//
// the lead is a rank, an icon, the bookmark glyph, or a cached cover
// thumb; the title is set in the book's face when it names a book and
// in the UI face when it names a device label; the value is
// right-aligned and reserves its measured width. a selected row fills
// solid to the group's inner edge and takes its corner radius, and
// the sub line and bar inverted with it.
//
// `sheet` lays these out inside a `SheetGeom`; `RowGroup` below lays
// them out as a plain stack for the tab screens. both call `draw`, so
// the anatomy has one definition.

use core::fmt::Write as _;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{CornerRadii, PrimitiveStyle, Rectangle, RoundedRectangle};

use crate::drivers::strip::StripBuffer;
use crate::fonts::bitmap::BitmapFont;
use crate::kernel::work_queue::DecodedImage;
use crate::ui::{Alignment, Region, StackFmt};

/// Phosphor bookmark-simple: the book being read, or the chapter the
/// reader is in.
pub const ICON_BOOKMARK: char = '\u{E0EA}';

// cells
pub const CELL_PAD: u16 = 12;
pub const LEAD_W: u16 = 28;
pub const CELL_GAP: u16 = 10;

/// Cover thumb in the lead column, matching the cached Mini variant
/// (`cover_cache::MINI_THUMB_*`) so nothing is rescaled: a 1-bit
/// dithered image cannot be resampled without destroying its dither.
pub const COVER_W: u16 = 64;
pub const COVER_H: u16 = 96;

/// Space above and below a cover inside its row. At 4 px the cover
/// sat all but on the separator above it, which read as cramped
/// however few rows the list held.
const COVER_PAD: u16 = 10;

/// Row height for a list whose lead is a cover.
pub const COVER_ROW_H: u16 = COVER_H + 2 * COVER_PAD;

/// Row height everywhere else: one line of UI body text with room
/// around it. It is the floor `row_height` never goes under, not a
/// figure a list should reach for directly -- a row stacking a sub
/// line or a bar under its title needs more, and how much more moves
/// with the user's UI font.
pub const ROW_H: u16 = 44;

/// Space above and below a row's stacked text block. Modest, because
/// a line box already carries its face's leading inside it: at 6 the
/// visible gap above the first glyph is nearer ten.
pub const ROW_PAD: u16 = 6;

pub const GROUP_RADIUS: u32 = 6;

/// Height a row needs for a title in `text_font` plus whatever it
/// stacks under it. Lists size their rows with this rather than
/// reaching for `ROW_H`: the UI font is user-settable across five
/// tiers, so a two-line row at the top of that range is nearly twice
/// a one-line row at the bottom, and a fixed figure clips one end or
/// wastes the other.
pub fn row_height(
    text_font: &BitmapFont,
    small: &BitmapFont,
    sub: bool,
    bar: bool,
) -> u16 {
    let mut block = text_font.line_height;
    if sub {
        block += STACK_GAP + small.line_height;
    }
    if bar {
        block += STACK_GAP + BAR_H;
    }
    (block + 2 * ROW_PAD).max(ROW_H)
}

// position bar under a title: the sleep card's tube, an outlined
// capsule that fills up, rather than a hairline with a solid stub
const BAR_W: u16 = 120;
pub const BAR_H: u16 = 4;
const BAR_RADIUS: u32 = 2;
/// gap above the bar, and between a title and its sub line
const STACK_GAP: u16 = 5;

// the value chip
const CHIP_PAD: u16 = 10;
const CHIP_INSET: u16 = 8;
const CHIP_RADIUS: u32 = 4;

/// Fonts a row draws with. `text` is whatever the caller decided the
/// row's voice is; `small` carries the sub line, the value and the
/// rank.
#[derive(Clone, Copy)]
pub struct RowFonts {
    pub text: &'static BitmapFont,
    pub small: &'static BitmapFont,
    pub icon: &'static BitmapFont,
}

/// What sits in the lead column. Borrows for as long as the spec
/// does, so an app can hand over a cover it owns.
#[derive(Clone, Copy)]
pub enum RowLead<'a> {
    None,
    Icon(char),
    Number(u16),
    Bookmark,
    /// cached Mini cover; `None` strokes the box so a missing cover
    /// reads as intentional rather than broken
    Cover(Option<&'a DecodedImage>),
}

impl RowLead<'_> {
    /// Width the lead reserves before the title.
    pub const fn width(&self) -> u16 {
        match self {
            Self::Cover(_) => COVER_W,
            _ => LEAD_W,
        }
    }
}

/// How a row's value is drawn. The chip is the settings screen's cue
/// for what a press acts on: outlined under the cursor, filled while
/// an edit session has Left and Right.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ValueChip {
    None,
    Cursor,
    Editing,
}

pub struct RowSpec<'a> {
    pub lead: RowLead<'a>,
    pub text: &'a str,
    pub text_font: &'static BitmapFont,
    pub value: &'a str,
    pub selected: bool,
    /// second line under the title, in the chrome font: the context
    /// the value cannot carry (an author, a session count, what a
    /// setting actually does)
    pub sub: &'a str,
    /// (done, total): the reader footer's bar, for a value that is
    /// really a fraction. takes the same slot as `sub`, so a row uses
    /// one or the other
    pub progress: Option<(u32, u32)>,
    pub chip: ValueChip,
}

impl<'a> RowSpec<'a> {
    /// A plain label/value row in the UI voice.
    pub fn label(text: &'a str, value: &'a str, font: &'static BitmapFont) -> Self {
        Self {
            lead: RowLead::None,
            text,
            text_font: font,
            value,
            selected: false,
            sub: "",
            progress: None,
            chip: ValueChip::None,
        }
    }

    pub fn with_sub(mut self, sub: &'a str) -> Self {
        self.sub = sub;
        self
    }

    pub fn lead(mut self, lead: RowLead<'a>) -> Self {
        self.lead = lead;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn progress(mut self, done: u32, total: u32) -> Self {
        self.progress = Some((done, total));
        self
    }

    pub fn chip(mut self, chip: ValueChip) -> Self {
        self.chip = chip;
        self
    }
}

/// Where a row sits in its group, which decides its separator and
/// which corners its selection fill rounds.
#[derive(Clone, Copy)]
pub struct RowEdges {
    pub first: bool,
    pub last: bool,
}

impl RowEdges {
    pub const fn at(i: usize, count: usize) -> Self {
        Self {
            first: i == 0,
            last: i + 1 >= count,
        }
    }

    pub const ONLY: Self = Self {
        first: true,
        last: true,
    };
}

fn rounded(strip: &mut StripBuffer, r: Region, radius: u32, style: PrimitiveStyle<BinaryColor>) {
    RoundedRectangle::with_equal_corners(r.to_rect(), Size::new(radius, radius))
        .into_styled(style)
        .draw(strip)
        .ok();
}

/// Hairline outline around a group of rows.
pub fn draw_group_outline(strip: &mut StripBuffer, region: Region) {
    if !region.intersects(strip.logical_window()) {
        return;
    }
    rounded(
        strip,
        region,
        GROUP_RADIUS,
        PrimitiveStyle::with_stroke(BinaryColor::On, 1),
    );
}

/// One row: hairline above it inside its group, inverted fill when
/// selected, lead, title with its sub line or bar, right-aligned
/// value.
pub fn draw(
    strip: &mut StripBuffer,
    row: Region,
    edges: RowEdges,
    fonts: &RowFonts,
    spec: &RowSpec<'_>,
) {
    if !row.intersects(strip.logical_window()) {
        return;
    }

    if !edges.first {
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
        let r = Size::new(GROUP_RADIUS - 1, GROUP_RADIUS - 1);
        let z = Size::zero();
        RoundedRectangle::new(
            cell,
            CornerRadii {
                top_left: if edges.first { r } else { z },
                top_right: if edges.first { r } else { z },
                bottom_right: if edges.last { r } else { z },
                bottom_left: if edges.last { r } else { z },
            },
        )
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(strip)
        .ok();
        BinaryColor::Off
    } else {
        BinaryColor::On
    };

    let lead_w = spec.lead.width();
    draw_lead(strip, row, lead_w, fonts, spec, fg);

    // value, right aligned, reserving its measured width plus the
    // chip's padding when it wears one
    let text_w_of_value = if spec.value.is_empty() {
        0
    } else {
        fonts.small.measure_str(spec.value)
    };
    let value_w = match (spec.chip, text_w_of_value) {
        (_, 0) => 0,
        (ValueChip::None, w) => w,
        (_, w) => w + 2 * CHIP_PAD,
    };
    if value_w > 0 {
        let value_r = Region::new(row.x + row.w - CELL_PAD - value_w, row.y, value_w, row.h);
        draw_value(strip, value_r, fonts, spec, fg);
    }

    let text_x = row.x + CELL_PAD + lead_w + CELL_GAP;
    let text_w = (row.x + row.w - CELL_PAD)
        .saturating_sub(text_x)
        .saturating_sub(if value_w > 0 { value_w + CELL_GAP } else { 0 });

    // title, then its sub line, then its bar: whichever of the three
    // the row asked for, stacked and centred as one block. halving
    // the row instead would leave a title floating in a tall row
    let title_h = spec.text_font.line_height;
    let sub_h = if spec.sub.is_empty() {
        0
    } else {
        STACK_GAP + fonts.small.line_height
    };
    let bar_h = if spec.progress.is_some() {
        STACK_GAP + BAR_H
    } else {
        0
    };
    let block = title_h + sub_h + bar_h;
    let mut ty = row.y + row.h.saturating_sub(block) / 2;

    draw_ellipsized(
        strip,
        spec.text_font,
        Region::new(text_x, ty, text_w, title_h),
        spec.text,
        fg,
    );
    ty += title_h;

    if sub_h > 0 {
        ty += STACK_GAP;
        draw_ellipsized(
            strip,
            fonts.small,
            Region::new(text_x, ty, text_w, fonts.small.line_height),
            spec.sub,
            fg,
        );
        ty += fonts.small.line_height;
    }

    if let Some((done, total)) = spec.progress {
        ty += STACK_GAP;
        let bar = Region::new(text_x, ty, BAR_W.min(text_w), BAR_H);
        draw_tube_bar(strip, bar, done, total, fg);
    }
}

/// The position bar, as the sleep screen draws it: an outlined tube
/// that fills up. The fill is never narrower than the tube is tall,
/// so one page in reads as a rounded pip rather than a sliver, and it
/// takes the tube's own radius so the two ends agree.
///
/// One definition, used by every bar outside the reader's own footer:
/// these rows, the home card, and the sleep card's own.
pub fn draw_tube_bar(strip: &mut StripBuffer, bar: Region, done: u32, total: u32, fg: BinaryColor) {
    if !bar.intersects(strip.logical_window()) {
        return;
    }
    let radii = Size::new(BAR_RADIUS, BAR_RADIUS);
    RoundedRectangle::with_equal_corners(bar.to_rect(), radii)
        .into_styled(PrimitiveStyle::with_stroke(fg, 1))
        .draw(strip)
        .ok();
    let filled = (bar.w as u32 * done.min(total))
        .checked_div(total)
        .map_or(0, |f| f as u16);
    if filled > 0 {
        let fill = Region::new(bar.x, bar.y, filled.max(bar.h), bar.h);
        RoundedRectangle::with_equal_corners(fill.to_rect(), radii)
            .into_styled(PrimitiveStyle::with_fill(fg))
            .draw(strip)
            .ok();
    }
}

/// Right-aligned value, in a chip when the row asks for one. A filled
/// chip inverts its text against the row it already inverted, so an
/// open edit session reads differently from a cursor at rest.
fn draw_value(
    strip: &mut StripBuffer,
    value_r: Region,
    fonts: &RowFonts,
    spec: &RowSpec<'_>,
    fg: BinaryColor,
) {
    if spec.chip == ValueChip::None {
        fonts
            .small
            .draw_aligned(strip, value_r, spec.value, Alignment::CenterRight, fg);
        return;
    }
    let h = value_r.h.saturating_sub(2 * CHIP_INSET);
    let chip = Region::new(
        value_r.x,
        value_r.y + value_r.h.saturating_sub(h) / 2,
        value_r.w,
        h,
    );
    let filled = spec.chip == ValueChip::Editing;
    let style = if filled {
        PrimitiveStyle::with_fill(fg)
    } else {
        PrimitiveStyle::with_stroke(fg, 1)
    };
    rounded(strip, chip, CHIP_RADIUS, style);
    let text_fg = if filled { invert(fg) } else { fg };
    fonts
        .small
        .draw_aligned(strip, chip, spec.value, Alignment::Center, text_fg);
}

#[inline]
const fn invert(fg: BinaryColor) -> BinaryColor {
    match fg {
        BinaryColor::On => BinaryColor::Off,
        BinaryColor::Off => BinaryColor::On,
    }
}

fn draw_lead(
    strip: &mut StripBuffer,
    row: Region,
    lead_w: u16,
    fonts: &RowFonts,
    spec: &RowSpec<'_>,
    fg: BinaryColor,
) {
    let lead_r = Region::new(row.x + CELL_PAD, row.y, lead_w, row.h);
    match spec.lead {
        RowLead::None => {}
        RowLead::Icon(_) | RowLead::Bookmark => {
            let ch = match spec.lead {
                RowLead::Icon(ch) => ch,
                _ => ICON_BOOKMARK,
            };
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            fonts
                .icon
                .draw_aligned(strip, lead_r, s, Alignment::CenterRight, fg);
        }
        RowLead::Number(n) => {
            let mut s = StackFmt::<8>::new();
            let _ = write!(s, "{}", n);
            fonts
                .small
                .draw_aligned(strip, lead_r, s.as_str(), Alignment::CenterRight, fg);
        }
        RowLead::Cover(cover) => {
            let x = lead_r.x;
            let y = row.y + row.h.saturating_sub(COVER_H) / 2;
            match cover {
                // covers are 1-bit packed; invert the bit on a
                // selected row so a dark cover still reads against
                // the inverted background
                Some(img) => strip.blit_1bpp(
                    &img.data,
                    0,
                    img.width as usize,
                    img.height as usize,
                    img.stride,
                    x as i32,
                    y as i32,
                    !spec.selected,
                ),
                None => {
                    Rectangle::new(
                        Point::new(x as i32, y as i32),
                        Size::new(COVER_W as u32, COVER_H as u32),
                    )
                    .into_styled(PrimitiveStyle::with_stroke(fg, 1))
                    .draw(strip)
                    .ok();
                }
            }
        }
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

// ── a group of rows, stacked ────────────────────────────────────────

/// A run of equal-height rows inside one outline. The sheet lays its
/// rows out itself (two groups inside a fixed frame); a tab screen
/// stacks these down the content area, so this owns the arithmetic
/// that used to be a pile of per-screen `y +=` constants.
#[derive(Clone, Copy)]
pub struct RowGroup {
    pub region: Region,
    pub rows: usize,
    pub row_h: u16,
}

impl RowGroup {
    /// Height an `n`-row group of `row_h` rows occupies, including the
    /// one extra pixel the outline's bottom stroke sits on.
    pub const fn height(rows: usize, row_h: u16) -> u16 {
        rows as u16 * row_h + 1
    }

    pub const fn new(x: u16, y: u16, w: u16, rows: usize, row_h: u16) -> Self {
        Self {
            region: Region::new(x, y, w, Self::height(rows, row_h)),
            rows,
            row_h,
        }
    }

    pub const fn row_region(&self, i: usize) -> Region {
        Region::new(
            self.region.x,
            self.region.y + i as u16 * self.row_h,
            self.region.w,
            self.row_h,
        )
    }

    /// y immediately below the group.
    pub const fn bottom(&self) -> u16 {
        self.region.y + self.region.h
    }

    pub fn draw_outline(&self, strip: &mut StripBuffer) {
        if self.rows > 0 {
            draw_group_outline(strip, self.region);
        }
    }

    /// Outline plus one row, for the common `for i in 0..n` loop.
    pub fn draw_row(&self, strip: &mut StripBuffer, i: usize, fonts: &RowFonts, spec: &RowSpec<'_>) {
        draw(
            strip,
            self.row_region(i),
            RowEdges::at(i, self.rows),
            fonts,
            spec,
        );
    }
}
