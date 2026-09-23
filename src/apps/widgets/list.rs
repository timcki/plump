// list selection and scrolling helper
//
// a selection cursor within a scrollable list: wrapping moves and
// scroll adjustment
use crate::ui::{wrap_next, wrap_prev};

#[derive(Clone, Copy, Debug, Default)]
pub struct ListSelection {
    pub selected: usize,
    pub scroll: usize,
    pub count: usize,
    pub visible: usize,
}

impl ListSelection {
    pub const fn new(count: usize, visible: usize) -> Self {
        Self {
            selected: 0,
            scroll: 0,
            count,
            visible,
        }
    }

    pub fn set_count(&mut self, count: usize) {
        self.count = count;
        if count == 0 {
            self.selected = 0;
            self.scroll = 0;
        } else {
            if self.selected >= count {
                self.selected = count - 1;
            }
            self.scroll_into_view();
        }
    }

    pub fn set_visible(&mut self, visible: usize) {
        self.visible = visible;
        self.scroll_into_view();
    }

    pub fn move_next(&mut self) -> bool {
        if self.count == 0 {
            return false;
        }
        let old = self.selected;
        self.selected = wrap_next(self.selected, self.count);
        if self.selected < old {
            // wrapped to start
            self.scroll = 0;
        } else {
            self.scroll_into_view();
        }
        self.selected != old
    }

    pub fn move_prev(&mut self) -> bool {
        if self.count == 0 {
            return false;
        }
        let old = self.selected;
        self.selected = wrap_prev(self.selected, self.count);
        if self.selected > old {
            // wrapped to end
            self.scroll = self.count.saturating_sub(self.visible);
        } else {
            self.scroll_into_view();
        }
        self.selected != old
    }

    pub fn scroll_into_view(&mut self) {
        if self.count == 0 || self.visible == 0 {
            return;
        }

        // selected is above visible window
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }

        // selected is below visible window
        if self.selected >= self.scroll + self.visible {
            self.scroll = self.selected + 1 - self.visible;
        }

        // clamp scroll to valid range
        let max_scroll = self.count.saturating_sub(self.visible);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    pub fn visible_count(&self) -> usize {
        self.visible.min(self.count.saturating_sub(self.scroll))
    }
}

