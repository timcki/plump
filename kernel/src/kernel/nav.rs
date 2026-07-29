// navigation: persistent tabs with an optional modal overlay.
//
// replaces the 4-deep push/pop stack (`super::app::Launcher`). the
// rebuild assumes one active tab at a time with at most one modal
// pushed on top; on-device, only the reader is a modal.
//
// the kernel stays generic over the distro's concrete tab + modal
// types.

/// horizontal navigation direction (passed to `App::on_horizontal`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HDir {
    Left,
    Right,
}

/// result of `App::on_horizontal`. when an app reports `AtEdge`, the
/// manager interprets the press as a tab switch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HResult {
    /// the app consumed the press (e.g. moved its cursor).
    Consumed,
    /// the app declines; the manager should switch tabs.
    AtEdge,
}

/// what's currently visible: a tab or a modal pushed over a tab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavSlot<T, M> {
    Tab(T),
    Modal(M),
}

/// flat navigation state: one active tab, at most one modal over it.
///
/// pure state: no embedded `AppContext` (the existing `Launcher` already
/// owns one; chunks B.4+ retire `Launcher` and move ownership of the
/// `AppContext` here. for now, the manager holds both side by side).
pub struct Nav<T: Copy + Eq, M: Copy + Eq> {
    active_tab: T,
    modal: Option<M>,
}

impl<T: Copy + Eq, M: Copy + Eq> Nav<T, M> {
    pub const fn new(initial_tab: T) -> Self {
        Self {
            active_tab: initial_tab,
            modal: None,
        }
    }

    #[inline]
    pub fn active_tab(&self) -> T {
        self.active_tab
    }

    #[inline]
    pub fn modal(&self) -> Option<M> {
        self.modal
    }

    #[inline]
    pub fn active_slot(&self) -> NavSlot<T, M> {
        match self.modal {
            Some(m) => NavSlot::Modal(m),
            None => NavSlot::Tab(self.active_tab),
        }
    }

    /// Force-set the active tab without firing lifecycle hooks.
    /// Used by RTC session restore.
    pub fn restore(&mut self, tab: T, modal: Option<M>) {
        self.active_tab = tab;
        self.modal = modal;
    }
}
