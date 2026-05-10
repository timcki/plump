use embedded_graphics::{pixelcolor::BinaryColor, prelude::*, primitives::{PrimitiveStyle, RoundedRectangle}};

use crate::board::SCREEN_W;
use crate::board::action::Action;
use crate::drivers::strip::StripBuffer;
use crate::fonts::bitmap::BitmapFont;
use crate::fonts::font_data;
pub use crate::kernel::app::{MAX_APP_ACTIONS, QuickAction, QuickActionKind};
use crate::ui::stack_fmt::StackFmt;
use crate::ui::{Alignment, Region, wrap_next, wrap_prev};

const OVERLAY_W: u16 = 400;
const OVERLAY_X: u16 = (SCREEN_W - OVERLAY_W) / 2;
const OVERLAY_BOTTOM: u16 = 760;
const ITEM_H: u16 = 40;
const ITEM_GAP: u16 = 4;
const ITEM_STRIDE: u16 = ITEM_H + ITEM_GAP;
const PAD_TOP: u16 = 12;
const PAD_BOTTOM: u16 = 10;
const ITEM_INSET: u16 = 8;  // horizontal inset for selection highlight
const LABEL_X: u16 = OVERLAY_X + 16;
const LABEL_W: u16 = 150;
const VALUE_X: u16 = LABEL_X + LABEL_W + 8;
const VALUE_W: u16 = OVERLAY_W - 16 - LABEL_W - 8 - 16;
const HELP_H: u16 = 20;
const SEP_H: u16 = 12;     // vertical space consumed by the separator gap
const SEP_INSET: u16 = 16; // horizontal inset for separator line
const R_BORDER: Size = Size::new(8, 8); // overlay corner radius (matches home card)
const R_ITEM: Size = Size::new(4, 4);   // selection highlight radius (matches home buttons)
const BORDER_W: u32 = 2;               // overlay border stroke width

const NUM_CORE: usize = 2; // Refresh + Go Home
const MAX_ITEMS: usize = MAX_APP_ACTIONS + NUM_CORE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickMenuResult {
    Consumed,
    Close,
    RefreshScreen,
    GoHome,
    AppTrigger(u8),
}

#[derive(Clone, Copy)]
enum MenuItemKind {
    AppCycle {
        id: u8,
        value: u8,
        options: &'static [&'static str],
    },
    AppTrigger {
        id: u8,
    },
    CoreRefresh,
    CoreHome,
}

#[derive(Clone, Copy)]
struct MenuItem {
    label: &'static str,
    kind: MenuItemKind,
}

impl MenuItem {
    const EMPTY: Self = Self {
        label: "",
        kind: MenuItemKind::CoreRefresh,
    };
}

pub struct QuickMenu {
    pub open: bool,
    items: [MenuItem; MAX_ITEMS],
    count: usize,
    app_count: usize,
    selected: usize,
    /// Index of the last action item; a separator is drawn after it
    /// when settings items follow.
    separator_after: Option<usize>,
    pub dirty: bool,
    overlay_region: Region,
    font: Option<&'static BitmapFont>,
}

impl Default for QuickMenu {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickMenu {
    pub const fn new() -> Self {
        Self {
            open: false,
            items: [MenuItem::EMPTY; MAX_ITEMS],
            count: 0,
            app_count: 0,
            selected: 0,
            separator_after: None,
            dirty: false,
            overlay_region: Region::new(0, 0, 0, 0),
            font: None,
        }
    }

    pub fn set_chrome_font(&mut self, font: &'static BitmapFont) {
        self.font = Some(font);
    }

    pub fn show(&mut self, app_actions: &[QuickAction]) {
        let n_app = app_actions.len().min(MAX_APP_ACTIONS);
        self.app_count = n_app;

        // core items first: Go Home (default selected), then Clear ghost
        let mut idx = 0;
        self.items[idx] = MenuItem {
            label: "Go Home",
            kind: MenuItemKind::CoreHome,
        };
        idx += 1;
        self.items[idx] = MenuItem {
            label: "Clear ghost",
            kind: MenuItemKind::CoreRefresh,
        };
        idx += 1;

        // app actions follow
        for a in app_actions.iter().take(n_app) {
            self.items[idx] = MenuItem {
                label: a.label,
                kind: match a.kind {
                    QuickActionKind::Cycle { value, options } => MenuItemKind::AppCycle {
                        id: a.id,
                        value,
                        options,
                    },
                    QuickActionKind::Trigger { .. } => {
                        MenuItemKind::AppTrigger { id: a.id }
                    }
                },
            };
            idx += 1;
        }

        self.count = n_app + NUM_CORE;
        self.selected = 0;
        self.open = true;
        self.dirty = true;

        // find the separator: last action (trigger/core) before first setting (cycle)
        self.separator_after = None;
        let mut last_action = None;
        let mut has_settings = false;
        for i in 0..self.count {
            match self.items[i].kind {
                MenuItemKind::AppCycle { .. } => has_settings = true,
                _ => last_action = Some(i),
            }
        }
        if has_settings {
            self.separator_after = last_action;
        }

        self.overlay_region = self.compute_region();
    }

    pub fn hide(&mut self) {
        self.open = false;
        self.dirty = true;
    }

    pub fn region(&self) -> Region {
        self.overlay_region
    }

    pub fn app_cycle_value(&self, id: u8) -> Option<u8> {
        for i in NUM_CORE..NUM_CORE + self.app_count {
            if let MenuItemKind::AppCycle {
                id: item_id, value, ..
            } = self.items[i].kind
                && item_id == id
            {
                return Some(value);
            }
        }
        None
    }

    pub fn on_action(&mut self, action: Action) -> QuickMenuResult {
        match action {
            Action::Menu | Action::Back => {
                self.hide();
                QuickMenuResult::Close
            }

            Action::Next => {
                let new = wrap_next(self.selected, self.count);
                if new != self.selected {
                    self.selected = new;
                    self.dirty = true;
                }
                QuickMenuResult::Consumed
            }

            Action::Prev => {
                let new = wrap_prev(self.selected, self.count);
                if new != self.selected {
                    self.selected = new;
                    self.dirty = true;
                }
                QuickMenuResult::Consumed
            }

            Action::NextJump => {
                self.adjust_selected(1);
                QuickMenuResult::Consumed
            }

            Action::PrevJump => {
                self.adjust_selected(-1);
                QuickMenuResult::Consumed
            }

            Action::Select => self.activate_selected(),
        }
    }

    fn adjust_selected(&mut self, delta: i8) {
        let item = &mut self.items[self.selected];
        if let MenuItemKind::AppCycle {
            ref mut value,
            options,
            ..
        } = item.kind
        {
            let max = options.len().saturating_sub(1) as u8;
            if delta > 0 && *value < max {
                *value += 1;
                self.dirty = true;
            } else if delta < 0 && *value > 0 {
                *value -= 1;
                self.dirty = true;
            }
        }
    }

    fn activate_selected(&mut self) -> QuickMenuResult {
        match &mut self.items[self.selected].kind {
            MenuItemKind::AppCycle { value, options, .. } => {
                let len = options.len() as u8;
                if len > 0 {
                    *value = (*value + 1) % len;
                    self.dirty = true;
                }
                QuickMenuResult::Consumed
            }
            MenuItemKind::AppTrigger { id, .. } => {
                let id = *id;
                self.hide();
                QuickMenuResult::AppTrigger(id)
            }
            MenuItemKind::CoreRefresh => {
                self.hide();
                QuickMenuResult::RefreshScreen
            }
            MenuItemKind::CoreHome => {
                self.hide();
                QuickMenuResult::GoHome
            }
        }
    }

    fn compute_region(&self) -> Region {
        let sep = if self.separator_after.is_some() { SEP_H } else { 0 };
        let content_h = PAD_TOP + (ITEM_STRIDE * self.count as u16) + sep + HELP_H + PAD_BOTTOM;
        let y = OVERLAY_BOTTOM - content_h;
        Region::new(OVERLAY_X, y, OVERLAY_W, content_h)
    }

    fn item_y(&self, i: usize) -> u16 {
        let sep_extra = match self.separator_after {
            Some(sep_idx) if i > sep_idx => SEP_H,
            _ => 0,
        };
        self.overlay_region.y + PAD_TOP + i as u16 * ITEM_STRIDE + sep_extra
    }

    fn item_label_region(&self, i: usize) -> Region {
        Region::new(LABEL_X, self.item_y(i), LABEL_W, ITEM_H)
    }

    fn item_value_region(&self, i: usize) -> Region {
        Region::new(VALUE_X, self.item_y(i), VALUE_W, ITEM_H)
    }

    fn help_region(&self) -> Region {
        let last = self.count.saturating_sub(1);
        let below_last = self.item_y(last) + ITEM_STRIDE + 2;
        Region::new(OVERLAY_X + 12, below_last, OVERLAY_W - 24, HELP_H)
    }

    fn format_value(&self, i: usize, buf: &mut StackFmt<20>) {
        buf.clear();
        match &self.items[i].kind {
            MenuItemKind::AppCycle { value, options, .. } => {
                let idx = *value as usize;
                let text = if idx < options.len() {
                    options[idx]
                } else {
                    "?"
                };
                let _ = core::fmt::Write::write_str(buf, text);
            }
            MenuItemKind::AppTrigger { .. }
            | MenuItemKind::CoreRefresh
            | MenuItemKind::CoreHome => {
                let _ = core::fmt::Write::write_str(buf, "\u{2192}"); // →
            }
        }
    }

    pub fn draw(&self, strip: &mut StripBuffer) {
        if !self.open {
            return;
        }

        let font = self.font.unwrap_or(&font_data::INTER_REGULAR_BODY_SMALL);

        let outer = self.overlay_region;
        if outer.intersects(strip.logical_window()) {
            // rounded white fill + black border
            let rect = outer.to_rect();
            RoundedRectangle::with_equal_corners(rect, R_BORDER)
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
                .draw(strip)
                .unwrap();
            RoundedRectangle::with_equal_corners(rect, R_BORDER)
                .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, BORDER_W))
                .draw(strip)
                .unwrap();
        }

        let mut val_buf = StackFmt::<20>::new();

        for i in 0..self.count {
            let selected = i == self.selected;
            let row_region = Region::new(
                OVERLAY_X + ITEM_INSET,
                self.item_y(i),
                OVERLAY_W - 2 * ITEM_INSET,
                ITEM_H,
            );

            let fg = if selected && row_region.intersects(strip.logical_window()) {
                RoundedRectangle::with_equal_corners(row_region.to_rect(), R_ITEM)
                    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                    .draw(strip)
                    .unwrap();
                BinaryColor::Off
            } else if selected {
                BinaryColor::Off
            } else {
                BinaryColor::On
            };

            let label_region = self.item_label_region(i);
            let value_region = self.item_value_region(i);

            if label_region.intersects(strip.logical_window()) {
                font.draw_aligned(
                    strip,
                    label_region,
                    self.items[i].label,
                    Alignment::CenterLeft,
                    fg,
                );
            }

            if value_region.intersects(strip.logical_window()) {
                self.format_value(i, &mut val_buf);
                let val_align = match self.items[i].kind {
                    MenuItemKind::CoreRefresh | MenuItemKind::CoreHome
                    | MenuItemKind::AppTrigger { .. } => Alignment::CenterRight,
                    _ => Alignment::Center,
                };
                font.draw_aligned(strip, value_region, val_buf.as_str(), val_align, fg);
            }
        }

        // separator line between actions and settings
        if let Some(sep_idx) = self.separator_after {
            let sep_y = self.item_y(sep_idx) + ITEM_H + (SEP_H / 2);
            let sep_region = Region::new(
                OVERLAY_X + SEP_INSET,
                sep_y,
                OVERLAY_W - 2 * SEP_INSET,
                1,
            );
            if sep_region.intersects(strip.logical_window()) {
                sep_region
                    .to_rect()
                    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                    .draw(strip)
                    .unwrap();
            }
        }

        let help = match &self.items[self.selected].kind {
            MenuItemKind::AppCycle { .. } => "Up/Down: move  Jump: adjust  Sel: cycle  Menu: close",
            _ => "Up/Down: move  Sel: activate  Menu: close",
        };

        let help_region = self.help_region();
        if help_region.intersects(strip.logical_window()) {
            font.draw_aligned(strip, help_region, help, Alignment::Center, BinaryColor::On);
        }
    }
}
