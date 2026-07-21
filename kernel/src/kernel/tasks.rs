// embassy spawned tasks: input polling (housekeeping and idle sleep
// run on Instant deadlines inside the scheduler main loop)

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

#[embassy_executor::task]
pub async fn input_task(mut input: InputDriver) -> ! {
    let mut idle_ticks: u32 = 0;

    let raw = input.read_battery_mv();
    BATTERY_MV.signal(battery::adc_to_battery_mv(raw));
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

        if RESET_HOLD.try_take().is_some() {
            input.reset_hold_state();
        }

        if let Some(ev) = input.poll() {
            let _ = INPUT_EVENTS.try_send(ev);
            idle_ticks = 0; // reset to fast polling on any event
        } else {
            idle_ticks = idle_ticks.saturating_add(1);
        }

        if Instant::now() >= battery_at {
            battery_at = Instant::now() + Duration::from_secs(timing::BATTERY_INTERVAL_SECS);
            let raw = input.read_battery_mv();
            BATTERY_MV.signal(battery::adc_to_battery_mv(raw));
        }
    }
}
