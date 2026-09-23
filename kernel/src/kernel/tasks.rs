// embassy spawned tasks: input polling (housekeeping and idle sleep
// run on Instant deadlines inside the scheduler main loop)

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};

use crate::drivers::battery;
use crate::drivers::input::{Event, InputDriver};

use super::timing;

pub const INPUT_CHANNEL_CAP: usize = 8;
pub static INPUT_EVENTS: Channel<CriticalSectionRawMutex, Event, INPUT_CHANNEL_CAP> =
    Channel::new();

// signal input_task to reset hold timers after a navigation event is consumed
pub static RESET_HOLD: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[inline]
pub fn request_hold_reset() {
    RESET_HOLD.signal(());
}

pub static BATTERY_MV: Signal<CriticalSectionRawMutex, u16> = Signal::new();

// the driver lives here rather than inside the task so long
// synchronous work can sample the buttons as well (`sample_input`).
// on this single-core executor the task only runs between such work,
// so a press shorter than one decode or scan would start and end
// between two samples and never be seen
struct Sampler {
    driver: Option<InputDriver>,
    last: Instant,
}

static SAMPLER: Mutex<CriticalSectionRawMutex, RefCell<Sampler>> =
    Mutex::new(RefCell::new(Sampler {
        driver: None,
        last: Instant::from_ticks(0),
    }));

// run `f` on the driver outside the lock: the ADC reads take long
// enough that holding a critical section across them would delay
// interrupts, and nothing else can reach the driver meanwhile (one
// core, and neither caller awaits while holding it)
fn with_driver<R>(f: impl FnOnce(&mut InputDriver) -> R) -> Option<R> {
    let mut driver = SAMPLER.lock(|s| {
        let mut s = s.borrow_mut();
        s.last = Instant::now();
        s.driver.take()
    })?;
    let r = f(&mut driver);
    SAMPLER.lock(|s| s.borrow_mut().driver = Some(driver));
    Some(r)
}

// one sample: queue every event it produced, true if there were any
fn sample_into_channel(input: &mut InputDriver) -> bool {
    if RESET_HOLD.try_take().is_some() {
        input.reset_hold_state();
    }
    let mut any = false;
    while let Some(ev) = input.poll() {
        let _ = INPUT_EVENTS.try_send(ev);
        any = true;
    }
    any
}

/// Sample the buttons now if a fast tick has passed since the last
/// sample. Long synchronous work calls this (through every
/// `KernelHandle::sd` access, and from the image decoder) so input
/// keeps being seen while it runs; the events wait in `INPUT_EVENTS`
/// until the scheduler gets the CPU back.
pub fn sample_input() {
    let due = SAMPLER.lock(|s| {
        Instant::now() - s.borrow().last >= Duration::from_millis(timing::INPUT_TICK_FAST_MS)
    });
    if due {
        with_driver(sample_into_channel);
    }
}

/// Input is waiting to be handled: work that can be picked up again
/// later should stop and let the scheduler take it.
pub fn input_pending() -> bool {
    !INPUT_EVENTS.is_empty()
}

/// Hand the input driver to the sampler; `input_task` polls it from
/// then on. Installed rather than passed to the task so the driver has
/// one static home instead of also living in the task's future.
pub fn install_input(mut input: InputDriver) {
    let raw = input.read_battery_mv();
    BATTERY_MV.signal(battery::adc_to_battery_mv(raw));
    SAMPLER.lock(|s| s.borrow_mut().driver = Some(input));
}

#[embassy_executor::task]
pub async fn input_task() -> ! {
    let mut idle_ticks: u32 = 0;

    // deadline-based instead of counting ticks: tick duration varies
    // with the adaptive poll rate, so 3000 ticks silently meant 150s
    // at the slow rate
    let mut battery_at = Instant::now() + Duration::from_secs(timing::BATTERY_INTERVAL_SECS);

    loop {
        // adaptive polling: fast rate during active input, slow when idle
        let tick_ms = if idle_ticks >= timing::INPUT_IDLE_TICKS {
            timing::INPUT_TICK_SLOW_MS
        } else {
            timing::INPUT_TICK_FAST_MS
        };
        Timer::after(Duration::from_millis(tick_ms)).await;

        let got = with_driver(|input| {
            let got = sample_into_channel(input);
            if Instant::now() >= battery_at {
                battery_at = Instant::now() + Duration::from_secs(timing::BATTERY_INTERVAL_SECS);
                let raw = input.read_battery_mv();
                BATTERY_MV.signal(battery::adc_to_battery_mv(raw));
            }
            got
        })
        .unwrap_or(false);

        if got {
            idle_ticks = 0; // reset to fast polling on any event
        } else {
            idle_ticks = idle_ticks.saturating_add(1);
        }
    }
}
