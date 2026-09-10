// geometry and cursor state for the settings list.
//
// this type is the only thing that knows where a row lands on screen.
// the old screen computed row boxes two different ways (a uniform
// stride for dirty marking, a running y that section captions pushed
// along for drawing), so every partial refresh below a caption
// repainted the wrong band. here `rows()` and `row_region()` share one
// walk, so what gets drawn and what gets invalidated cannot disagree.
//
// sections are rows like any other, which is what makes variable row
// heights the normal case instead of a correction applied inside draw.

use crate::apps::AppContext;
use crate::board::SCREEN_W;
use crate::ui::{Region, Theme};

use super::model::{ROWS, Row, SettingId, Step};

const THEME: Theme = Theme::default_v1();

/// left edge and width of a row box. the gutter on the right holds the
/// scroll thumb; it is reserved whether or not the list scrolls so row
/// widths never change under the cursor.
pub const LIST_X: u16 = THEME.margin_lg;
pub const GUTTER_W: u16 = 6;
pub const LIST_W: u16 = SCREEN_W - 2 * THEME.margin_lg - GUTTER_W;

/// share of a row box given to the value column.
pub const VALUE_W: u16 = 150;

/// row heights for the current UI font.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub row_h: u16,
    /// extra height a row with a sub line takes
    pub sub_h: u16,
    pub caption_h: u16,
    pub section_gap: u16,
    /// gap under a caption, so it reads as belonging to the group
    /// beneath it rather than floating between two
    pub caption_pad: u16,
    pub top: u16,
    pub bottom: u16,
}

impl Metrics {
    /// `body_line_h` is the line height of the active UI body font;
    /// rows grow with it so a large UI font never clips its own text.
    pub fn for_line_height(body_line_h: u16) -> Self {
        Self {
            row_h: THEME.row_h.max(body_line_h + 2 * THEME.margin_sm),
            sub_h: crate::fonts::chrome_font().line_height,
            caption_h: body_line_h + THEME.margin_sm,
            section_gap: THEME.section_gap,
            caption_pad: 6,
            top: THEME.content_top() + THEME.margin_sm,
            bottom: THEME.content_bottom() - THEME.margin_sm,
        }
    }
}

/// what a cursor move or value change invalidated.
///
/// returned by the list because only the list knows whether the window
/// scrolled; callers just hand it to `mark`.
#[derive(Clone, Copy)]
pub enum Damage {
    None,
    /// up to two row boxes: the row that lost the highlight and the one
    /// that gained it. `None` entries are rows scrolled out of view
    Rows([Option<Region>; 2]),
    /// the window moved or re-laid out; everything repaints
    Viewport(Region),
}

impl Damage {
    pub fn mark(self, ctx: &mut AppContext) {
        match self {
            Self::None => {}
            Self::Rows(regions) => {
                for r in regions.into_iter().flatten() {
                    ctx.mark_dirty(r);
                }
            }
            Self::Viewport(r) => ctx.mark_dirty(r),
        }
    }

    #[inline]
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

pub struct SettingsList {
    // index into ROWS; always points at a Row::Item
    cursor: usize,
    // index into ROWS of the first painted row
    scroll: usize,
    metrics: Metrics,
}

impl SettingsList {
    pub fn new(body_line_h: u16) -> Self {
        Self {
            cursor: first_item(),
            scroll: 0,
            metrics: Metrics::for_line_height(body_line_h),
        }
    }

    pub fn reset(&mut self) {
        self.cursor = first_item();
        self.scroll = 0;
    }

    /// Re-derive row heights after a UI font change and pull the cursor
    /// back into the (now differently sized) window.
    pub fn set_line_height(&mut self, body_line_h: u16) {
        self.metrics = Metrics::for_line_height(body_line_h);
        self.ensure_visible();
    }

    #[inline]
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    #[inline]
    pub fn selected(&self) -> SettingId {
        // the cursor only ever lands on items; the fallback keeps a bad
        // index from panicking the device instead of misdrawing a row
        item_at(self.cursor).unwrap_or(SettingId::ReaderFont)
    }

    #[inline]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn viewport(&self) -> Region {
        Region::new(
            LIST_X,
            self.metrics.top,
            LIST_W + GUTTER_W,
            self.metrics.bottom.saturating_sub(self.metrics.top),
        )
    }

    /// Box of row `idx`, or `None` when it is scrolled out or does not
    /// fit above the tab bar. A row that only partly fits is never
    /// emitted, so nothing draws under the chrome.
    pub fn row_region(&self, idx: usize) -> Option<Region> {
        if idx < self.scroll || idx >= ROWS.len() {
            return None;
        }
        let mut y = self.metrics.top;
        for i in self.scroll..=idx {
            let h = self.height(i, i == self.scroll);
            if y + h > self.metrics.bottom {
                return None;
            }
            if i == idx {
                return Some(Region::new(LIST_X, y, LIST_W, h));
            }
            y += h;
        }
        None
    }

    /// Value column of row `idx`: the part that changes when a value is
    /// stepped without the row moving.
    pub fn value_region(&self, idx: usize) -> Option<Region> {
        self.row_region(idx).map(value_box)
    }

    pub fn selected_row_region(&self) -> Option<Region> {
        self.row_region(self.cursor)
    }

    pub fn selected_value_region(&self) -> Option<Region> {
        self.value_region(self.cursor)
    }

    /// Rows that fit in the window, top to bottom.
    pub fn rows(&self) -> VisibleRows<'_> {
        VisibleRows {
            list: self,
            idx: self.scroll,
            y: self.metrics.top,
        }
    }

    /// Index range currently painted, for the scroll thumb. `None` when
    /// everything fits.
    pub fn visible_span(&self) -> Option<(usize, usize)> {
        let last = self.rows().last().map(|(idx, _, _)| idx)?;
        if self.scroll == 0 && last + 1 == ROWS.len() {
            return None;
        }
        Some((self.scroll, last))
    }

    /// Move to the next/previous setting, skipping captions and
    /// wrapping at both ends.
    pub fn move_cursor(&mut self, dir: Step) -> Damage {
        let old_cursor = self.cursor;
        let old_scroll = self.scroll;
        let old_region = self.row_region(old_cursor);

        self.cursor = match dir {
            Step::Down => next_item(old_cursor),
            Step::Up => prev_item(old_cursor),
        };
        if self.cursor == old_cursor {
            return Damage::None;
        }

        // wrapping to the top is the one case where the window jumps
        // backwards; everything else scrolls by the minimum
        if dir == Step::Down && self.cursor < old_cursor {
            self.scroll = 0;
        }
        self.ensure_visible();

        if self.scroll != old_scroll {
            Damage::Viewport(self.viewport())
        } else {
            // one scanline of growth: the hairline at the top of the
            // row below is suppressed based on this row's selection,
            // so it changes with the move but sits outside the row's
            // own rect; without it 1px lines accumulated on scroll
            let with_boundary = |r: Region| Region::new(r.x, r.y, r.w, r.h + 1);
            Damage::Rows([
                old_region.map(with_boundary),
                self.row_region(self.cursor).map(with_boundary),
            ])
        }
    }

    fn height(&self, idx: usize, first_painted: bool) -> u16 {
        match ROWS[idx] {
            // the gap belongs to the caption below it, and collapses
            // when the caption is the first row painted
            Row::Section(_) if first_painted => self.metrics.caption_h + self.metrics.caption_pad,
            Row::Section(_) => {
                self.metrics.section_gap + self.metrics.caption_h + self.metrics.caption_pad
            }
            // a sub line makes its row taller rather than smaller type
            Row::Item(id) if !id.sub().is_empty() => self.metrics.row_h + self.metrics.sub_h,
            Row::Item(_) | Row::Info(_) => self.metrics.row_h,
        }
    }

    /// The run of consecutive non-caption rows `idx` belongs to, as
    /// (first, last) indices into `ROWS`. One outline is drawn per run,
    /// which is what makes a section read as a group.
    pub fn group_run(idx: usize) -> (usize, usize) {
        let mut first = idx;
        while first > 0 && !matches!(ROWS[first - 1], Row::Section(_)) {
            first -= 1;
        }
        let mut last = idx;
        while last + 1 < ROWS.len() && !matches!(ROWS[last + 1], Row::Section(_)) {
            last += 1;
        }
        (first, last)
    }

    fn ensure_visible(&mut self) {
        // scrolling up to the first setting of a section brings its
        // caption along, so a section never appears headless
        let target = match self.cursor.checked_sub(1) {
            Some(prev) if matches!(ROWS[prev], Row::Section(_)) => prev,
            _ => self.cursor,
        };
        if target < self.scroll {
            self.scroll = target;
        }
        while self.scroll < self.cursor && self.row_region(self.cursor).is_none() {
            self.scroll += 1;
        }
        // the About group holds no selectable row, so the cursor can
        // never pull it into view: on the last setting, scroll until
        // the foot of the list is on screen as well
        if self.cursor == last_item() {
            while self.scroll < self.cursor && self.row_region(ROWS.len() - 1).is_none() {
                self.scroll += 1;
            }
        }
    }
}

// must match the cell draw_item renders into (inset from the right
// edge by margin_md): a narrower damage box left stubs of the old
// chip on the panel whenever a stepped value got shorter
fn value_box(row: Region) -> Region {
    let w = VALUE_W.min(row.w);
    Region::new(
        row.x + (row.w - w).saturating_sub(THEME.margin_md),
        row.y,
        w,
        row.h,
    )
}

fn item_at(idx: usize) -> Option<SettingId> {
    match ROWS.get(idx) {
        Some(Row::Item(id)) => Some(*id),
        _ => None,
    }
}

fn first_item() -> usize {
    ROWS.iter()
        .position(|r| matches!(r, Row::Item(_)))
        .unwrap_or(0)
}

fn last_item() -> usize {
    ROWS.iter()
        .rposition(|r| matches!(r, Row::Item(_)))
        .unwrap_or(0)
}

fn next_item(from: usize) -> usize {
    let n = ROWS.len();
    (1..n)
        .map(|offset| (from + offset) % n)
        .find(|&idx| item_at(idx).is_some())
        .unwrap_or(from)
}

fn prev_item(from: usize) -> usize {
    let n = ROWS.len();
    (1..n)
        .map(|offset| (from + n - offset) % n)
        .find(|&idx| item_at(idx).is_some())
        .unwrap_or(from)
}

pub struct VisibleRows<'a> {
    list: &'a SettingsList,
    idx: usize,
    y: u16,
}

impl<'a> Iterator for VisibleRows<'a> {
    type Item = (usize, &'a Row, Region);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= ROWS.len() {
            return None;
        }
        let h = self.list.height(self.idx, self.idx == self.list.scroll);
        if self.y + h > self.list.metrics.bottom {
            return None;
        }
        let region = Region::new(LIST_X, self.y, LIST_W, h);
        let idx = self.idx;
        self.idx += 1;
        self.y += h;
        Some((idx, &ROWS[idx], region))
    }
}
