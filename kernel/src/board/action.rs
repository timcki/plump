// semantic actions decoupled from physical buttons
// apps match on Action, never on HwButton

use crate::board::button::Button;
use crate::drivers::input::{Event, InputEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Next,
    Prev,
    NextJump,
    PrevJump,
    Select,
    Back,
    Menu,
}

pub type ActionEvent = InputEvent<Action>;

// portrait one-handed layout with optional button swap
//
// default layout (right-handed):
//   bottom row: Back  Confirm(=Select)  Left(=PrevJump)  Right(=NextJump)
//   side:       VolUp(=Prev)  VolDown(=Next)
//
// swapped layout (left-handed):
//   bottom row: Left(=PrevJump)  Right(=NextJump)  Back  Confirm(=Select)
//   this swaps the *physical roles* of Back<->Left and Confirm<->Right
//   so the spatial position of Back/OK moves to the right side of the
//   device where the left hand naturally rests.
//   volume buttons are NOT swapped (up=prev, down=next always).
#[derive(Default)]
pub struct ButtonMapper {
    swap_buttons: bool,
}

// (default, swapped) semantic role of one physical button, so the two
// layouts stay side by side instead of drifting across twin matches
const fn roles(button: Button) -> (Action, Action) {
    match button {
        Button::VolDown => (Action::Next, Action::Next),
        Button::VolUp => (Action::Prev, Action::Prev),
        Button::Right => (Action::NextJump, Action::Select),
        Button::Left => (Action::PrevJump, Action::Back),
        Button::Confirm => (Action::Select, Action::NextJump),
        Button::Back => (Action::Back, Action::PrevJump),
        Button::Power => (Action::Menu, Action::Menu),
    }
}

impl ButtonMapper {
    pub const fn new() -> Self {
        Self {
            swap_buttons: false,
        }
    }

    pub fn set_swap(&mut self, swap: bool) {
        self.swap_buttons = swap;
    }

    pub fn is_swapped(&self) -> bool {
        self.swap_buttons
    }

    // never inlined: llvm lowers roles() to per-call-site switch
    // tables in dram, and the perf build sits within bytes of the
    // linker's 160K heap assert; one out-of-line copy keeps it flat
    #[inline(never)]
    pub fn map_button(&self, button: Button) -> Action {
        let (default, swapped) = roles(button);
        if self.swap_buttons { swapped } else { default }
    }

    pub fn map_event(&self, event: Event) -> ActionEvent {
        event.map(|b| self.map_button(b))
    }
}
