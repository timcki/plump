// quick menu: the bottom sheet at its smallest height (sheet S in
// mockups/xteink_x4_reader_contents.html). the active app supplies
// its actions, a title and a meta line; the menu adds the core rows
// (clear ghosting, then go home or sleep) as a second group.
//
// the header is the hand-off to the contents sheet: in the reader the
// meta line names the chapter being read, and choosing Contents grows
// the same sheet with that chapter as the highlighted row.

use core::fmt::Write as _;

use plump_kernel::util::FixedStr;

use crate::board::action::Action;
use crate::drivers::strip::StripBuffer;
pub use crate::kernel::app::{MAX_APP_ACTIONS, QuickAction, QuickActionKind};
use crate::ui::stack_fmt::StackFmt;
use crate::ui::{wrap_next, wrap_prev};

use super::sheet::{self, HintSlot, RowLead, RowSpec, SheetFonts, SheetGeom, ValueChip};

const NUM_CORE: usize = 2;
const MAX_ITEMS: usize = MAX_APP_ACTIONS + NUM_CORE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickMenuResult {
    Consumed,
    Close,
    RefreshScreen,
    GoHome,
    Sleep,
    AppTrigger(u8),
}

/// What the active app tells the menu about itself when it opens.
pub struct MenuContext<'a> {
    pub title: &'a str,
    pub meta: &'a str,
    /// home shows Sleep in place of Go home
    pub on_home: bool,
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
    CoreSleep,
}

#[derive(Clone, Copy)]
struct MenuItem {
    label: &'static str,
    icon: Option<char>,
    value: FixedStr<20>,
    kind: MenuItemKind,
}

impl MenuItem {
    const EMPTY: Self = Self {
        label: "",
        icon: None,
        value: FixedStr::EMPTY,
        kind: MenuItemKind::CoreRefresh,
    };
}

pub struct QuickMenu {
    pub open: bool,
    items: [MenuItem; MAX_ITEMS],
    count: usize,
    app_count: usize,
    selected: usize,
    pub dirty: bool,
    geom: SheetGeom,
    title: FixedStr<64>,
    meta: FixedStr<64>,
    ui_font_idx: u8,
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
            dirty: false,
            geom: SheetGeom::anchored(0, None, sheet::ROW_H),
            title: FixedStr::EMPTY,
            meta: FixedStr::EMPTY,
            ui_font_idx: 1,
        }
    }

    pub fn set_ui_font_size(&mut self, idx: u8) {
        self.ui_font_idx = idx;
    }

    pub fn show(&mut self, app_actions: &[QuickAction], ctx: MenuContext<'_>) {
        let n_app = app_actions.len().min(MAX_APP_ACTIONS);
        self.app_count = n_app;

        // app actions form the first group
        let mut idx = 0;
        for a in app_actions.iter().take(n_app) {
            self.items[idx] = MenuItem {
                label: a.label,
                icon: a.icon,
                value: a.value,
                kind: match a.kind {
                    QuickActionKind::Cycle { value, options } => MenuItemKind::AppCycle {
                        id: a.id,
                        value,
                        options,
                    },
                    QuickActionKind::Trigger { .. } => MenuItemKind::AppTrigger { id: a.id },
                },
            };
            idx += 1;
        }

        // core rows form the second group
        self.items[idx] = MenuItem {
            label: "Clear ghosting",
            icon: Some(sheet::ICON_ERASER),
            value: FixedStr::EMPTY,
            kind: MenuItemKind::CoreRefresh,
        };
        idx += 1;
        self.items[idx] = if ctx.on_home {
            MenuItem {
                label: "Sleep",
                icon: Some(sheet::ICON_MOON),
                value: FixedStr::EMPTY,
                kind: MenuItemKind::CoreSleep,
            }
        } else {
            MenuItem {
                label: "Go home",
                icon: Some(sheet::ICON_HOUSE),
                value: FixedStr::EMPTY,
                kind: MenuItemKind::CoreHome,
            }
        };
        idx += 1;

        self.count = idx;
        self.selected = 0;
        self.open = true;
        self.dirty = true;
        self.title.set(ctx.title.as_bytes());
        self.meta.set(ctx.meta.as_bytes());

        let group_break = if n_app > 0 { Some(n_app - 1) } else { None };
        let fonts = SheetFonts::for_ui(self.ui_font_idx);
        self.geom = SheetGeom::anchored(self.count, group_break, fonts.row_h(fonts.body, false, false));
    }

    pub fn hide(&mut self) {
        self.open = false;
        self.dirty = true;
    }

    pub fn region(&self) -> crate::ui::Region {
        self.geom.region
    }

    pub fn app_cycle_value(&self, id: u8) -> Option<u8> {
        for i in 0..self.app_count {
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
            MenuItemKind::CoreSleep => {
                self.hide();
                QuickMenuResult::Sleep
            }
        }
    }

    pub fn draw(&self, strip: &mut StripBuffer) {
        if !self.open {
            return;
        }
        let fonts = SheetFonts::for_ui(self.ui_font_idx);

        sheet::draw_frame(strip, &self.geom);
        let title = if self.title.is_empty() {
            "Menu"
        } else {
            self.title.as_str()
        };
        sheet::draw_header(strip, &self.geom, &fonts, title, "MENU", self.meta.as_str());
        sheet::draw_groups(strip, &self.geom);

        let mut val = StackFmt::<32>::new();
        for i in 0..self.count {
            let item = &self.items[i];
            let selected = i == self.selected;
            val.clear();
            match item.kind {
                MenuItemKind::AppCycle { value, options, .. } => {
                    let name = options.get(value as usize).copied().unwrap_or("?");
                    // the selected cycle row shows its adjust arrows
                    if selected {
                        let _ = write!(val, "\u{2039} {} \u{203A}", name);
                    } else {
                        let _ = write!(val, "{}", name);
                    }
                }
                MenuItemKind::AppTrigger { .. } | MenuItemKind::CoreHome => {
                    if item.value.is_empty() {
                        let _ = write!(val, "\u{2192}");
                    } else {
                        let _ = write!(val, "{} \u{2192}", item.value.as_str());
                    }
                }
                MenuItemKind::CoreRefresh | MenuItemKind::CoreSleep => {}
            }
            sheet::draw_row(
                strip,
                &self.geom,
                i,
                &fonts,
                &RowSpec {
                    lead: item.icon.map_or(RowLead::None, RowLead::Icon),
                    text: item.label,
                    text_font: fonts.body,
                    value: val.as_str(),
                    selected,
                    sub: "",
                    progress: None,
                    chip: ValueChip::None,
                },
            );
        }

        let hints: &[(HintSlot, &str)] = match self.items[self.selected].kind {
            MenuItemKind::AppCycle { .. } => &[
                (HintSlot::Back, "CLOSE"),
                (HintSlot::Ok, "SELECT"),
                (HintSlot::LeftRight, "\u{2039} ADJUST \u{203A}"),
            ],
            _ => &[(HintSlot::Back, "CLOSE"), (HintSlot::Ok, "SELECT")],
        };
        sheet::draw_hints(strip, &self.geom, &fonts, hints);
    }
}
