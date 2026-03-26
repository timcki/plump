// input policy: power-button state machine above raw events
//
// the low-level input driver emits Press/Release/LongPress/Repeat
// for all buttons including Power. this policy layer intercepts
// power-button events and resolves them into semantic inputs:
//
//   Press(Power)     → arm pending short press (no immediate action)
//   Release(Power)   → if pending → SemanticInput::MenuTap
//                       if long-press fired → ignore (eat the release)
//   LongPress(Power) → RequestSleep
//   Repeat(Power)    → ignore
//
// non-power events pass through unchanged.

use crate::board::button::Button;
use crate::drivers::input::Event;

/// Semantic inputs produced by the policy layer.
/// These bypass the ButtonMapper → ActionEvent path entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticInput {
    /// Power button short-press confirmed by release.
    /// App layer toggles the quick menu.
    MenuTap,
}

/// Result of resolving a hardware event through the policy layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedInput {
    /// Pass the event through to normal app dispatch.
    Forward(Event),
    /// A semantic input for the app layer (no raw event).
    Semantic(SemanticInput),
    /// The caller should enter sleep.
    RequestSleep,
    /// Drop the event silently.
    Ignore,
}

/// Internal power-button state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerState {
    /// No power-button activity.
    Idle,
    /// Press(Power) received; waiting for Release or LongPress.
    PendingShortPress,
    /// LongPress(Power) fired; eating the subsequent Release.
    LongPressFired,
}

/// Stateful policy resolver.
pub struct InputPolicyState {
    power: PowerState,
}

impl Default for InputPolicyState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputPolicyState {
    pub const fn new() -> Self {
        Self {
            power: PowerState::Idle,
        }
    }

    /// Resolve a raw hardware event into a policy outcome.
    ///
    /// Power-button events are consumed by the state machine.
    /// All other events pass through as `Forward`.
    pub fn resolve(&mut self, event: Event) -> ResolvedInput {
        match event {
            Event::Press(Button::Power) => {
                self.power = PowerState::PendingShortPress;
                ResolvedInput::Ignore
            }

            Event::Release(Button::Power) => match self.power {
                PowerState::PendingShortPress => {
                    self.power = PowerState::Idle;
                    ResolvedInput::Semantic(SemanticInput::MenuTap)
                }
                PowerState::LongPressFired => {
                    self.power = PowerState::Idle;
                    ResolvedInput::Ignore
                }
                PowerState::Idle => ResolvedInput::Ignore,
            },

            Event::LongPress(Button::Power) => {
                self.power = PowerState::LongPressFired;
                ResolvedInput::RequestSleep
            }

            Event::Repeat(Button::Power) => ResolvedInput::Ignore,

            _ => ResolvedInput::Forward(event),
        }
    }
}
