// tab identity for the persistent 5-tab navigation.
//
// Reader is NOT a tab; it's a modal overlay over whichever tab
// initiated it (see Chunk B for the Nav<Tab, Modal> rewrite). Tab is
// just the dead-letter enum during Chunk A so later chunks can wire
// dispatch / chrome / edge-overflow without further enum churn.

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
    pub const ORDER: [Tab; 5] = [
        Tab::Home,
        Tab::Library,
        Tab::Stats,
        Tab::Settings,
        Tab::Upload,
    ];

    #[inline]
    pub fn index(self) -> usize {
        match self {
            Tab::Home => 0,
            Tab::Library => 1,
            Tab::Stats => 2,
            Tab::Settings => 3,
            Tab::Upload => 4,
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

    /// Phosphor Bold v2.1 codepoint for this tab's icon. Keep this in
    /// lockstep with the `phosphor_icons` list in `build.rs`.
    pub fn icon(self) -> char {
        match self {
            Tab::Home => '\u{E2C2}',     // house
            Tab::Library => '\u{E758}',  // books
            Tab::Stats => '\u{E150}',    // chart-bar
            Tab::Settings => '\u{E272}', // gear-six
            Tab::Upload => '\u{E1AE}',   // cloud-arrow-up
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neighbours_at_boundaries() {
        assert_eq!(Tab::Home.left(), None);
        assert_eq!(Tab::Home.right(), Some(Tab::Library));
        assert_eq!(Tab::Upload.left(), Some(Tab::Settings));
        assert_eq!(Tab::Upload.right(), None);
    }

    #[test]
    fn order_matches_index() {
        for (i, tab) in Tab::ORDER.iter().copied().enumerate() {
            assert_eq!(tab.index(), i);
        }
    }
}
