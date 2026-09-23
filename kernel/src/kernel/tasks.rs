// embassy spawned tasks: input polling (housekeeping and idle sleep
// run on Instant deadlines inside the scheduler main loop)

use core::cell::Cell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};

use crate::drivers::battery;
use crate::drivers::input::{Event, InputDriver};

use super::timing;

pub const INPUT_CHANNEL_CAP: usize = 8;
static INPUT_EVENTS: Channel<CriticalSectionRawMutex, Event, INPUT_CHANNEL_CAP> =
    Channel::new();

// signal input_task to reset hold timers after a navigation event is consumed
pub static RESET_HOLD: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[inline]
pub fn request_hold_reset() {
    RESET_HOLD.signal(());
}

pub static BATTERY_MV: Signal<CriticalSectionRawMutex, u16> = Signal::new();

// queued events the scheduler would act on (a press, a long press, a
// repeat); a queued release alone is no reason to stop work
static ACTIONABLE: Mutex<CriticalSectionRawMutex, Cell<u8>> = Mutex::new(Cell::new(0));

#[inline]
fn is_actionable(ev: &Event) -> bool {
    !matches!(ev, Event::Release(_))
}

// false when the channel is full
fn queue_event(ev: Event) -> bool {
    if INPUT_EVENTS.try_send(ev).is_err() {
        return false;
    }
    if is_actionable(&ev) {
        ACTIONABLE.lock(|n| n.set(n.get().saturating_add(1)));
    }
    true
}

fn note_taken(ev: &Event) {
    if is_actionable(ev) {
        ACTIONABLE.lock(|n| n.set(n.get().saturating_sub(1)));
    }
}

/// Next queued input event, if any.
pub fn try_next_event() -> Option<Event> {
    let ev = INPUT_EVENTS.try_receive().ok()?;
    note_taken(&ev);
    Some(ev)
}

/// Wait for the next input event.
pub async fn next_event() -> Event {
    let ev = INPUT_EVENTS.receive().await;
    note_taken(&ev);
    ev
}

/// A press (or long press, or repeat) is waiting to be handled: work
/// that can be picked up again later should stop and let the scheduler
/// take it. A pending release does not count.
pub fn input_pending() -> bool {
    ACTIONABLE.lock(|n| n.get() > 0)
}

/// Button polling. Spawned on an interrupt executor (see `main`), so
/// its 10 ms timer pre-empts long synchronous work in thread mode (a
/// decode, a typeset, a cache step): a press made meanwhile is sampled
/// and queued on time, and waits in `INPUT_EVENTS` until the scheduler
/// gets the CPU back. The ladders have no interrupt of their own.
#[embassy_executor::task]
pub async fn input_task(mut input: InputDriver) -> ! {
    let mut idle_ticks: u32 = 0;

    // deadline-based instead of counting ticks: tick duration varies
    // with the adaptive poll rate, so 3000 ticks silently meant 150s
    // at the slow rate
    let mut battery_at = Instant::now() + Duration::from_secs(timing::BATTERY_INTERVAL_SECS);
    // a release that met a full channel (repeats pile up while a long
    // waveform holds the scheduler). dropping it would leave the policy
    // believing the button is still held, so it is retried each tick
    let mut held_release: Option<Event> = None;

    loop {
        // adaptive polling: fast rate during active input, slow when idle
        let tick_ms = if idle_ticks >= timing::INPUT_IDLE_TICKS {
            timing::INPUT_TICK_SLOW_MS
        } else {
            timing::INPUT_TICK_FAST_MS
        };
        Timer::after(Duration::from_millis(tick_ms)).await;

        if RESET_HOLD.try_take().is_some() {
            input.reset_hold_state();
        }
        if let Some(ev) = held_release
            && queue_event(ev)
        {
            held_release = None;
        }
        let mut got = false;
        while let Some(ev) = input.poll() {
            got = true;
            if held_release.is_some() {
                // nothing may overtake the stuck release; a press made
                // meanwhile is dropped together with its own release
                continue;
            }
            if !queue_event(ev) && matches!(ev, Event::Release(_)) {
                held_release = Some(ev);
            }
        }

        if Instant::now() >= battery_at {
            battery_at = Instant::now() + Duration::from_secs(timing::BATTERY_INTERVAL_SECS);
            let raw = input.read_battery_mv();
            BATTERY_MV.signal(battery::adc_to_battery_mv(raw));
        }

        if got {
            idle_ticks = 0; // reset to fast polling on any event
        } else {
            idle_ticks = idle_ticks.saturating_add(1);
        }
    }
}
