// button definitions and ADC resistance ladder decoding
// two ADC ladders (Row1 GPIO1, Row2 GPIO2) plus power button (GPIO3)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Right,
    Left,
    Confirm,
    Back,
    VolUp,
    VolDown,
    Power,
}

pub const DEFAULT_TOLERANCE: u16 = 150;

// (center_mv, tolerance_mv, button)
pub const ROW1_THRESHOLDS: &[(u16, u16, Button)] = &[
    (3, 50, Button::Right),
    (1113, DEFAULT_TOLERANCE, Button::Left),
    (1984, DEFAULT_TOLERANCE, Button::Confirm),
    (2556, DEFAULT_TOLERANCE, Button::Back),
];

pub const ROW2_THRESHOLDS: &[(u16, u16, Button)] = &[
    (3, 50, Button::VolDown),
    (1659, DEFAULT_TOLERANCE, Button::VolUp),
];

impl Button {
    /// Decode a button from an ADC millivolt reading using a resistance
    /// ladder threshold table. Returns `None` if no threshold matches.
    pub fn from_ladder(mv: u16, thresholds: &[(u16, u16, Button)]) -> Option<Button> {
        for &(center, tolerance, button) in thresholds {
            let low = center.saturating_sub(tolerance);
            let high = center.saturating_add(tolerance);
            if mv >= low && mv <= high {
                return Some(button);
            }
        }
        None
    }
}
