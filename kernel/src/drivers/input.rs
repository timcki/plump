// debounced input from ADC ladders and power button
// one button at a time (ladder hw limitation)
// ADC reads oversampled to reject noise (~40 us per channel)

use esp_hal::time::{Duration, Instant};

use crate::board::InputHw;
use crate::board::button::{Button, ROW1_THRESHOLDS, ROW2_THRESHOLDS};
use crate::kernel::timing;

macro_rules! read_averaged {
    ($adc:expr, $pin:expr) => {{
        let mut sum: u32 = 0;
        for _ in 0..timing::ADC_OVERSAMPLE {
            sum += nb::block!($adc.read_oneshot($pin)).unwrap() as u32;
        }
        (sum / timing::ADC_OVERSAMPLE) as u16
    }};
}

/// A press lifecycle over whatever a layer names its inputs: physical
/// buttons here, semantic actions after the [`crate::board::action::ButtonMapper`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent<T> {
    Press(T),
    Release(T),
    LongPress(T),
    Repeat(T),
}

impl<T> InputEvent<T> {
    /// Translate the carried input, keeping the lifecycle variant.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> InputEvent<U> {
        match self {
            InputEvent::Press(t) => InputEvent::Press(f(t)),
            InputEvent::Release(t) => InputEvent::Release(f(t)),
            InputEvent::LongPress(t) => InputEvent::LongPress(f(t)),
            InputEvent::Repeat(t) => InputEvent::Repeat(f(t)),
        }
    }
}

pub type Event = InputEvent<Button>;

struct EventQueue {
    buf: [Option<Event>; 4],
}

impl EventQueue {
    const fn new() -> Self {
        Self { buf: [None; 4] }
    }

    fn push(&mut self, ev: Event) {
        for slot in self.buf.iter_mut() {
            if slot.is_none() {
                *slot = Some(ev);
                return;
            }
        }
    }

    fn pop(&mut self) -> Option<Event> {
        for slot in self.buf.iter_mut() {
            if let Some(ev) = slot.take() {
                return Some(ev);
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.buf.iter().all(|s| s.is_none())
    }
}

/// Linear progression of a held button, replacing the three booleans
/// and two timestamps that used to encode it.
#[derive(Clone, Copy)]
enum Hold {
    /// Nothing held.
    Idle,
    /// Held, long press not yet due.
    Armed { since: Instant },
    /// Long press fired; repeats run off `last`.
    Repeating { last: Instant },
    /// The hold was acknowledged elsewhere (quick menu took it), so it
    /// emits nothing more until the button is released.
    Consumed,
}

impl Hold {
    /// Restart the hold timer. A consumed hold stays consumed: the
    /// acknowledgement outlives sub-debounce chatter and survives a
    /// press that arrives while nothing was held.
    #[inline]
    fn arm(self, now: Instant) -> Self {
        match self {
            Hold::Consumed => Hold::Consumed,
            _ => Hold::Armed { since: now },
        }
    }
}

pub struct InputDriver {
    hw: InputHw,
    stable: Option<Button>,
    candidate: Option<Button>,
    candidate_since: Instant,
    hold: Hold,
    queue: EventQueue,
}

impl InputDriver {
    pub fn new(hw: InputHw) -> Self {
        let now = Instant::now();
        Self {
            hw,
            stable: None,
            candidate: None,
            candidate_since: now,
            hold: Hold::Idle,
            queue: EventQueue::new(),
        }
    }

    pub fn reset_hold_state(&mut self) {
        self.hold = Hold::Consumed;
    }

    pub fn poll(&mut self) -> Option<Event> {
        if !self.queue.is_empty() {
            return self.queue.pop();
        }

        let raw = self.read_raw();
        let now = Instant::now();

        if raw != self.candidate {
            // raw deviated from stable; restart hold timer so
            // sub-debounce releases don't accumulate into LongPress
            if self.stable.is_some() && raw != self.stable {
                self.hold = self.hold.arm(now);
            }
            self.candidate = raw;
            self.candidate_since = now;
        }

        let debounced = if now - self.candidate_since >= Duration::from_millis(timing::DEBOUNCE_MS)
        {
            self.candidate
        } else {
            self.stable
        };

        if debounced != self.stable {
            if let Some(old) = self.stable {
                self.queue.push(Event::Release(old));
                self.hold = Hold::Idle;
            }
            if let Some(new) = debounced {
                self.queue.push(Event::Press(new));
                self.hold = self.hold.arm(now);
            }
            self.stable = debounced;
            return self.queue.pop();
        }

        if let Some(btn) = self.stable {
            match self.hold {
                Hold::Armed { since } => {
                    let held = now - since;
                    if held >= Duration::from_millis(timing::LONG_PRESS_MS) {
                        self.hold = Hold::Repeating { last: now };
                        log::debug!("input: LongPress({:?}) after {}ms", btn, held.as_millis());
                        return Some(Event::LongPress(btn));
                    }
                }
                Hold::Repeating { last } => {
                    if now - last >= Duration::from_millis(timing::REPEAT_MS) {
                        self.hold = Hold::Repeating { last: now };
                        return Some(Event::Repeat(btn));
                    }
                }
                Hold::Idle | Hold::Consumed => {}
            }
        }

        None
    }

    fn read_raw(&mut self) -> Option<Button> {
        let power_low = crate::board::power_button_is_low();
        if power_low {
            return Some(Button::Power);
        }

        let mv1 = self.read_averaged_row1();
        let mv2 = self.read_averaged_row2();

        Button::from_ladder(mv1, ROW1_THRESHOLDS)
            .or_else(|| Button::from_ladder(mv2, ROW2_THRESHOLDS))
    }

    fn read_averaged_row1(&mut self) -> u16 {
        read_averaged!(self.hw.adc, &mut self.hw.row1)
    }

    fn read_averaged_row2(&mut self) -> u16 {
        read_averaged!(self.hw.adc, &mut self.hw.row2)
    }

    pub fn read_battery_mv(&mut self) -> u16 {
        read_averaged!(self.hw.adc, &mut self.hw.battery)
    }
}
