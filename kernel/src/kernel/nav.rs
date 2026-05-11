// navigation: persistent tabs with an optional modal overlay.
//
// replaces the 4-deep push/pop stack (`super::app::Launcher`). the
// rebuild assumes one active tab at a time with at most one modal
// pushed on top; on-device, only the reader is a modal.
//
// the kernel stays generic over the distro's concrete tab + modal
// types. apps return `NavCmd<T, M>` from `on_event`; the scheduler
// drives lifecycle hooks based on `NavEvent` it gets back.
//
// chunk B.1 lands the types as pure additions. later phases retire
// `Launcher` / `Transition` and rewrite the manager around `Nav`.

use super::app::AppContext;

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

/// command returned by `App::on_event`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavCmd<T, M> {
    /// no change.
    None,
    /// switch the active tab (drops any open modal).
    SetTab(T),
    /// push a modal over the current tab.
    ShowModal(M),
    /// pop the modal back to its underlying tab.
    CloseModal,
}

/// result of `Nav::apply`. carries enough information for the manager
/// to fire the right lifecycle hooks (`on_enter` vs `on_resume`,
/// `on_exit` vs `on_suspend`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NavEvent<T, M> {
    pub left: NavSlot<T, M>,
    pub entered: NavSlot<T, M>,
    /// true => fire `on_suspend` on the left slot (it stays in memory).
    /// false => fire `on_exit` (it's leaving the navigation entirely).
    pub suspended_left: bool,
    /// true => fire `on_resume` on the entered slot (rejoining).
    /// false => fire `on_enter` (fresh entry).
    pub resumed_entered: bool,
}

/// flat navigation: one active tab, at most one modal pushed over it.
pub struct Nav<T: Copy + Eq, M: Copy + Eq> {
    active_tab: T,
    modal: Option<M>,
    pub ctx: AppContext,
}

impl<T: Copy + Eq, M: Copy + Eq> Nav<T, M> {
    pub const fn new(initial_tab: T) -> Self {
        Self {
            active_tab: initial_tab,
            modal: None,
            ctx: AppContext::new(),
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

    /// True iff the user is currently inside the modal (i.e. Reader).
    #[inline]
    pub fn in_modal(&self) -> bool {
        self.modal.is_some()
    }

    /// Force-set the active tab without firing lifecycle hooks.
    /// Used by RTC session restore.
    pub fn restore(&mut self, tab: T, modal: Option<M>) {
        self.active_tab = tab;
        self.modal = modal;
    }

    /// Apply a navigation command. Returns `None` if nothing changed.
    ///
    /// Lifecycle semantics:
    ///  - `SetTab(t)`: closes any modal (modal `on_exit`), then if
    ///    `t != active_tab` suspends the current tab and resumes
    ///    (or enters fresh) the new tab. opening a modal previously
    ///    suspended the underlying tab; switching tabs drops it.
    ///  - `ShowModal(m)`: suspends the current tab, enters the modal.
    ///    if a modal is already open it gets `on_exit` first.
    ///  - `CloseModal`: exits the modal, resumes the underlying tab.
    pub fn apply(&mut self, cmd: NavCmd<T, M>) -> Option<NavEvent<T, M>> {
        match cmd {
            NavCmd::None => None,

            NavCmd::SetTab(t) => {
                if let Some(_m) = self.modal.take() {
                    // closing the modal as part of a tab switch: the
                    // modal's on_exit fires, then the new tab enters.
                    let left = NavSlot::Modal(_m);
                    if t == self.active_tab {
                        // back to the underlying tab; treat as resume.
                        let entered = NavSlot::Tab(self.active_tab);
                        Some(NavEvent {
                            left,
                            entered,
                            suspended_left: false,
                            resumed_entered: true,
                        })
                    } else {
                        // exit modal AND switch tab. the underlying
                        // tab was suspended; that suspension is
                        // implicitly dropped (its on_exit fires below
                        // via a follow-up apply call by the manager
                        // when it processes the chained event). for
                        // simplicity we model this as a single event
                        // from the modal to the new tab, and the
                        // manager fires on_exit on the previous tab
                        // separately by tracking active_tab itself.
                        let prev_tab = self.active_tab;
                        self.active_tab = t;
                        Some(NavEvent {
                            left,
                            entered: NavSlot::Tab(t),
                            suspended_left: false,
                            resumed_entered: needs_resume(prev_tab, t),
                        })
                    }
                } else if t == self.active_tab {
                    None
                } else {
                    let left = NavSlot::Tab(self.active_tab);
                    self.active_tab = t;
                    Some(NavEvent {
                        left,
                        entered: NavSlot::Tab(t),
                        suspended_left: true,
                        resumed_entered: false,
                    })
                }
            }

            NavCmd::ShowModal(m) => {
                if self.modal == Some(m) {
                    return None;
                }
                let left = match self.modal {
                    Some(prev) => NavSlot::Modal(prev),
                    None => NavSlot::Tab(self.active_tab),
                };
                let suspended_left = matches!(left, NavSlot::Tab(_));
                self.modal = Some(m);
                Some(NavEvent {
                    left,
                    entered: NavSlot::Modal(m),
                    suspended_left,
                    resumed_entered: false,
                })
            }

            NavCmd::CloseModal => {
                let m = self.modal.take()?;
                Some(NavEvent {
                    left: NavSlot::Modal(m),
                    entered: NavSlot::Tab(self.active_tab),
                    suspended_left: false,
                    resumed_entered: true,
                })
            }
        }
    }
}

// when a tab switch happens with a previously suspended sibling, the
// kernel can't know if it was actually suspended (depends on whether
// we ever showed it). conservatively report resume so the app gets
// a chance to refresh; cost is just one redraw. apps that need to
// distinguish can do so via `on_enter` vs `on_resume` themselves.
#[inline]
fn needs_resume<T: Eq>(_prev: T, _new: T) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Tab {
        A,
        B,
    }
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Modal {
        Reader,
    }

    #[test]
    fn set_tab_switches_and_reports_event() {
        let mut nav: Nav<Tab, Modal> = Nav::new(Tab::A);
        let ev = nav.apply(NavCmd::SetTab(Tab::B)).unwrap();
        assert_eq!(ev.left, NavSlot::Tab(Tab::A));
        assert_eq!(ev.entered, NavSlot::Tab(Tab::B));
        assert!(ev.suspended_left);
        assert!(!ev.resumed_entered);
        assert_eq!(nav.active_tab(), Tab::B);
    }

    #[test]
    fn set_tab_to_same_is_noop() {
        let mut nav: Nav<Tab, Modal> = Nav::new(Tab::A);
        assert!(nav.apply(NavCmd::SetTab(Tab::A)).is_none());
    }

    #[test]
    fn show_then_close_modal_round_trip() {
        let mut nav: Nav<Tab, Modal> = Nav::new(Tab::A);
        let open = nav.apply(NavCmd::ShowModal(Modal::Reader)).unwrap();
        assert_eq!(open.entered, NavSlot::Modal(Modal::Reader));
        assert!(open.suspended_left);
        let close = nav.apply(NavCmd::CloseModal).unwrap();
        assert_eq!(close.entered, NavSlot::Tab(Tab::A));
        assert!(close.resumed_entered);
        assert!(!nav.in_modal());
    }

    #[test]
    fn set_tab_from_modal_drops_modal_and_switches() {
        let mut nav: Nav<Tab, Modal> = Nav::new(Tab::A);
        nav.apply(NavCmd::ShowModal(Modal::Reader));
        let ev = nav.apply(NavCmd::SetTab(Tab::B)).unwrap();
        assert_eq!(ev.left, NavSlot::Modal(Modal::Reader));
        assert_eq!(ev.entered, NavSlot::Tab(Tab::B));
        assert!(!nav.in_modal());
        assert_eq!(nav.active_tab(), Tab::B);
    }
}
