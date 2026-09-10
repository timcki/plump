// tab identity for the persistent 5-tab navigation.
//
// Reader is NOT a tab; it's a modal overlay over whichever tab
// initiated it (see Chunk B for the Nav<Tab, Modal> rewrite). Tab is
// just the dead-letter enum during Chunk A so later chunks can wire
// dispatch / chrome / edge-overflow without further enum churn.
//
// ── the order is Home-centred, and it means something ─────────────
//
//     Upload   Settings   HOME   Library   Stats
//        <-- the device        your books -->
//
// Home sits in the middle of the five, so Right walks out through the
// books (what you are reading, then how much you have read) and Left
// walks out through the device (how it behaves, then getting files
// onto it). Two consequences worth keeping:
//
//   * nothing is more than two presses from Home, where the old
//     Home-first order put Upload four away;
//   * the direction carries meaning, so a screen's neighbours are
//     never an arbitrary pair.
//
// Nothing persists this order: the RTC session stores `AppId`
// discriminants, and `index` is only read by `left` / `right` and the
// chrome's slot loop. It is safe to reorder, and this comment is the
// only reason not to.

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Tab {
    Home,
    Library,
    Stats,
    Settings,
    Upload,
}

impl Tab {
    /// Left to right, with Home in the middle. See the module note.
    pub const ORDER: [Tab; 5] = [
        Tab::Upload,
        Tab::Settings,
        Tab::Home,
        Tab::Library,
        Tab::Stats,
    ];

    #[inline]
    pub fn index(self) -> usize {
        match self {
            Tab::Upload => 0,
            Tab::Settings => 1,
            Tab::Home => 2,
            Tab::Library => 3,
            Tab::Stats => 4,
        }
    }

    /// Neighbour to the left, or None if already leftmost.
    pub fn left(self) -> Option<Tab> {
        let i = self.index();
        if i == 0 { None } else { Some(Self::ORDER[i - 1]) }
    }

    /// Neighbour to the right, or None if already rightmost.
    pub fn right(self) -> Option<Tab> {
        let i = self.index();
        if i + 1 >= Self::ORDER.len() {
            None
        } else {
            Some(Self::ORDER[i + 1])
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Library => "Library",
            Tab::Stats => "Stats",
            Tab::Settings => "Settings",
            Tab::Upload => "Upload",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_the_middle() {
        // one press either way
        assert_eq!(Tab::Home.right(), Some(Tab::Library));
        assert_eq!(Tab::Home.left(), Some(Tab::Settings));
        // two presses either way, and no further
        assert_eq!(Tab::Library.right(), Some(Tab::Stats));
        assert_eq!(Tab::Settings.left(), Some(Tab::Upload));
    }

    #[test]
    fn the_line_has_two_ends() {
        assert_eq!(Tab::Upload.left(), None);
        assert_eq!(Tab::Stats.right(), None);
    }

    #[test]
    fn nothing_is_more_than_two_presses_from_home() {
        for tab in Tab::ORDER {
            let d = tab.index().abs_diff(Tab::Home.index());
            assert!(d <= 2, "{:?} is {} presses from Home", tab, d);
        }
    }

    #[test]
    fn order_matches_index() {
        for (i, tab) in Tab::ORDER.iter().copied().enumerate() {
            assert_eq!(tab.index(), i);
        }
    }
}
