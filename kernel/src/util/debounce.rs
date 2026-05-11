// debounce: hold a pending value until a quiet window has elapsed.
//
// idiomatic use is write-coalescing. tag a struct as "dirty" with the
// latest version (`note`); have a poll-loop call `try_take` to retrieve
// it once quiescent. force_take ignores the window for pre-sleep /
// transition flushes.
//
// the reader currently hand-rolls this pattern via `persist_next_flush_at`;
// chunk F (day stats) replaces that with `Debounce<DayStats>` and a
// future chunk migrates the reader.

use embassy_time::{Duration, Instant};

pub struct Debounce<T: Copy + Eq> {
    pending: Option<T>,
    deadline: Option<Instant>,
    quiet: Duration,
}

impl<T: Copy + Eq> Debounce<T> {
    pub const fn new(quiet_ms: u64) -> Self {
        Self {
            pending: None,
            deadline: None,
            quiet: Duration::from_millis(quiet_ms),
        }
    }

    /// Note a new value, resetting the quiet window.
    ///
    /// If `value` equals the already-pending value, the deadline is
    /// still pushed (the caller signalled fresh activity).
    pub fn note(&mut self, value: T) {
        self.pending = Some(value);
        self.deadline = Some(Instant::now() + self.quiet);
    }

    /// Returns the pending value if the quiet window has elapsed.
    /// Clears the pending slot on take.
    pub fn try_take(&mut self) -> Option<T> {
        let deadline = self.deadline?;
        if Instant::now() < deadline {
            return None;
        }
        let value = self.pending.take()?;
        self.deadline = None;
        Some(value)
    }

    /// Returns the pending value immediately, regardless of the window.
    /// Use on suspend / pre-sleep / forced transitions.
    pub fn force_take(&mut self) -> Option<T> {
        self.deadline = None;
        self.pending.take()
    }

    /// True if there's a value waiting (regardless of window state).
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Discard any pending value without taking it.
    pub fn clear(&mut self) {
        self.pending = None;
        self.deadline = None;
    }
}
