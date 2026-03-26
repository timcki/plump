//! Feature-gated performance instrumentation for pulp-os.
//!
//! All macros compile to nothing when the `perf` cargo feature is disabled.
//! When enabled, they emit structured `key=value` logs via `log::info!`
//! using targets prefixed with `perf::` for filtering.
//!
//! # ESP_LOG filtering
//!
//! `esp-println` compiles the log filter from `ESP_LOG` at build time.
//! The target prefix `perf::` allows selective filtering:
//!
//! - `ESP_LOG="debug,esp_hal=info"` — default dev logging
//! - `ESP_LOG="info"` — info-level logging including perf
//! - `ESP_LOG="warn,perf=info"` — suppress normal info, show only perf
//! - `ESP_LOG="info,perf=off"` — normal logging, hide perf output
//!
//! Note: target filtering only suppresses output. To remove instrumentation
//! cost entirely, build without `--features perf`.
//!
//! # Output format
//!
//! Since `esp-println` doesn't include the log target in output, perf macros
//! prepend `perf::{target}` to the message for easy grepping:
//!
//! ```text
//! INFO - perf::reader NeedInit elapsed_ms=42
//! INFO - perf::storage read bytes=4096
//! ```
//!
//! # Macros
//!
//! ## `perf_event!` — structured point event
//!
//! ```ignore
//! perf_event!("reader", "state_change from={:?} to={:?}", old, new);
//! perf_event!("storage", "read bytes={}", n);
//! ```
//!
//! ## `perf_scope!` — RAII timing guard
//!
//! ```ignore
//! let _g = perf_scope!("reader", "NeedInit");
//! // ... synchronous work only — do NOT hold across .await ...
//! // on drop: INFO - perf::reader NeedInit elapsed_ms=42
//! ```
//!
//! ## `perf_begin!` — capture timestamp for manual timing
//!
//! ```ignore
//! perf_begin!(t0);
//! // ... work ...
//! perf_event!("reader", "manual elapsed_ms={}", t0.elapsed().as_millis());
//! ```
//!
//! When `perf` is off, `perf_begin!` produces no binding and
//! `perf_event!` discards its arguments without type-checking,
//! so references to `t0` inside `perf_event!` compile cleanly.
//!
//! # Storage counters
//!
//! Critical-section-protected counters for SD I/O operations. Always callable
//! (no-op stubs when `perf` is off). Use [`counters::snapshot`] and
//! [`counters::delta`] for
//! per-scope summaries inside `perf_event!`:
//!
//! ```ignore
//! let snap = pulp_kernel::perf::counters::snapshot();
//! // ... work ...
//! let d = pulp_kernel::perf::counters::delta(&snap);
//! perf_event!("storage", "page_load reads={} bytes_r={}", d.sd_reads, d.sd_bytes_read);
//! ```

// ---------------------------------------------------------------------------
// perf_event! — structured point event
// ---------------------------------------------------------------------------

/// Log a structured performance event with `key=value` fields.
///
/// Compiles to nothing when the `perf` feature is off.
/// The first argument is a target literal (e.g. `"reader"`, `"storage"`).
/// The rest is a `format_args!`-style format string with arguments.
///
/// Output includes the `perf::{target}` prefix in the message for grepability.
#[cfg(feature = "perf")]
#[macro_export]
macro_rules! perf_event {
    ($target:literal, $fmt:literal) => {
        log::info!(
            target: concat!("perf::", $target),
            concat!("perf::", $target, " ", $fmt),
        )
    };
    ($target:literal, $fmt:literal, $($args:expr),+ $(,)?) => {
        log::info!(
            target: concat!("perf::", $target),
            concat!("perf::", $target, " ", $fmt),
            $($args),+
        )
    };
}

#[cfg(not(feature = "perf"))]
#[macro_export]
macro_rules! perf_event {
    ($target:literal, $($arg:tt)*) => {};
}

// ---------------------------------------------------------------------------
// perf_scope! — RAII timing guard
// ---------------------------------------------------------------------------

/// Create an RAII timing guard that logs `elapsed_ms` on drop.
///
/// Returns a [`TimingGuard`] when `perf` is on, `()` when off.
/// **Synchronous only** — do NOT hold across `.await` points. This is a
/// usage rule; the helper does not enforce it for you.
///
/// ```ignore
/// let _g = perf_scope!("reader", "NeedInit");
/// // on drop: INFO - perf::reader NeedInit elapsed_ms=42
/// ```
#[cfg(feature = "perf")]
#[macro_export]
macro_rules! perf_scope {
    ($target:literal, $name:expr) => {
        $crate::perf::TimingGuard::new(concat!("perf::", $target), $name)
    };
}

#[cfg(not(feature = "perf"))]
#[macro_export]
macro_rules! perf_scope {
    ($target:literal, $name:expr) => {
        ()
    };
}

// ---------------------------------------------------------------------------
// perf_begin! — capture timestamp for manual timing
// ---------------------------------------------------------------------------

/// Bind a local variable to the current `embassy_time::Instant`.
///
/// Compiles to nothing when `perf` is off. References to the binding
/// inside [`perf_event!`] are safe because that macro also compiles out.
///
/// ```ignore
/// perf_begin!(t0);
/// // ... work ...
/// perf_event!("reader", "done elapsed_ms={}", t0.elapsed().as_millis());
/// ```
#[cfg(feature = "perf")]
#[macro_export]
macro_rules! perf_begin {
    ($name:ident) => {
        let $name = embassy_time::Instant::now();
    };
}

#[cfg(not(feature = "perf"))]
#[macro_export]
macro_rules! perf_begin {
    ($name:ident) => {};
}

// ---------------------------------------------------------------------------
// TimingGuard
// ---------------------------------------------------------------------------

#[cfg(feature = "perf")]
pub struct TimingGuard {
    target: &'static str,
    name: &'static str,
    start: embassy_time::Instant,
}

#[cfg(feature = "perf")]
impl TimingGuard {
    /// Create a new timing guard. Prefer the [`perf_scope!`] macro.
    #[inline]
    pub fn new(target: &'static str, name: &'static str) -> Self {
        Self {
            target,
            name,
            start: embassy_time::Instant::now(),
        }
    }
}

#[cfg(feature = "perf")]
impl Drop for TimingGuard {
    fn drop(&mut self) {
        let ms = self.start.elapsed().as_millis();
        log::info!(
            target: self.target,
            "{} {} elapsed_ms={}",
            self.target,
            self.name,
            ms,
        );
    }
}

// ---------------------------------------------------------------------------
// Storage I/O counters
// ---------------------------------------------------------------------------

/// Critical-section-protected SD I/O counters for per-scope profiling
/// summaries.
///
/// All functions are always callable. When `perf` is off, they are no-ops
/// and the compiler optimises them away entirely.
pub mod counters {
    /// Snapshot of counter values at a point in time.
    ///
    /// When `perf` is off this is a ZST. Field access only compiles
    /// inside [`perf_event!`] (which also compiles out), so this is safe.
    #[cfg(feature = "perf")]
    #[derive(Clone, Copy, Debug)]
    pub struct Snapshot {
        pub sd_reads: u32,
        pub sd_writes: u32,
        pub sd_bytes_read: u32,
        pub sd_bytes_written: u32,
    }

    #[cfg(not(feature = "perf"))]
    #[derive(Clone, Copy, Debug)]
    pub struct Snapshot;

    // -- active counters (perf on) -----------------------------------------
    //
    // ESP32-C3 is RV32IMC (no 'A' atomic extension), so we use
    // critical_section::Mutex<Cell<u32>> instead of AtomicU32.
    // The critical section is ~free on single-core (just disables interrupts).

    #[cfg(feature = "perf")]
    #[allow(dead_code)]
    mod inner {
        use core::cell::Cell;
        use critical_section::Mutex;

        pub(super) static SD_READS: Mutex<Cell<u32>> = Mutex::new(Cell::new(0));
        pub(super) static SD_WRITES: Mutex<Cell<u32>> = Mutex::new(Cell::new(0));
        pub(super) static SD_BYTES_READ: Mutex<Cell<u32>> = Mutex::new(Cell::new(0));
        pub(super) static SD_BYTES_WRITTEN: Mutex<Cell<u32>> = Mutex::new(Cell::new(0));

        #[inline]
        pub(super) fn add(counter: &Mutex<Cell<u32>>, n: u32) {
            critical_section::with(|cs| {
                let c = counter.borrow(cs);
                c.set(c.get().wrapping_add(n));
            });
        }

        #[inline]
        pub(super) fn load(counter: &Mutex<Cell<u32>>) -> u32 {
            critical_section::with(|cs| counter.borrow(cs).get())
        }

        #[inline]
        pub(super) fn store(counter: &Mutex<Cell<u32>>, val: u32) {
            critical_section::with(|cs| counter.borrow(cs).set(val));
        }
    }

    /// Record one SD read operation.
    #[cfg(feature = "perf")]
    #[inline]
    pub fn inc_sd_reads() {
        inner::add(&inner::SD_READS, 1);
    }

    /// Record one SD write operation.
    #[cfg(feature = "perf")]
    #[inline]
    pub fn inc_sd_writes() {
        inner::add(&inner::SD_WRITES, 1);
    }

    /// Record bytes read from SD.
    #[cfg(feature = "perf")]
    #[inline]
    pub fn add_sd_bytes_read(n: u32) {
        inner::add(&inner::SD_BYTES_READ, n);
    }

    /// Record bytes written to SD.
    #[cfg(feature = "perf")]
    #[inline]
    pub fn add_sd_bytes_written(n: u32) {
        inner::add(&inner::SD_BYTES_WRITTEN, n);
    }

    /// Take a snapshot of current counter values.
    #[cfg(feature = "perf")]
    #[inline]
    pub fn snapshot() -> Snapshot {
        // single CS to get a consistent snapshot
        critical_section::with(|cs| Snapshot {
            sd_reads: inner::SD_READS.borrow(cs).get(),
            sd_writes: inner::SD_WRITES.borrow(cs).get(),
            sd_bytes_read: inner::SD_BYTES_READ.borrow(cs).get(),
            sd_bytes_written: inner::SD_BYTES_WRITTEN.borrow(cs).get(),
        })
    }

    /// Compute the delta between a previous snapshot and now.
    #[cfg(feature = "perf")]
    #[inline]
    pub fn delta(before: &Snapshot) -> Snapshot {
        let now = snapshot();
        Snapshot {
            sd_reads: now.sd_reads.wrapping_sub(before.sd_reads),
            sd_writes: now.sd_writes.wrapping_sub(before.sd_writes),
            sd_bytes_read: now.sd_bytes_read.wrapping_sub(before.sd_bytes_read),
            sd_bytes_written: now.sd_bytes_written.wrapping_sub(before.sd_bytes_written),
        }
    }

    /// Reset all counters to zero.
    #[cfg(feature = "perf")]
    #[inline]
    pub fn reset() {
        critical_section::with(|cs| {
            inner::SD_READS.borrow(cs).set(0);
            inner::SD_WRITES.borrow(cs).set(0);
            inner::SD_BYTES_READ.borrow(cs).set(0);
            inner::SD_BYTES_WRITTEN.borrow(cs).set(0);
        });
    }

    // -- stubs (perf off) --------------------------------------------------

    #[cfg(not(feature = "perf"))]
    #[inline]
    pub fn inc_sd_reads() {}

    #[cfg(not(feature = "perf"))]
    #[inline]
    pub fn inc_sd_writes() {}

    #[cfg(not(feature = "perf"))]
    #[inline]
    pub fn add_sd_bytes_read(_n: u32) {}

    #[cfg(not(feature = "perf"))]
    #[inline]
    pub fn add_sd_bytes_written(_n: u32) {}

    #[cfg(not(feature = "perf"))]
    #[inline]
    pub fn snapshot() -> Snapshot {
        Snapshot
    }

    #[cfg(not(feature = "perf"))]
    #[inline]
    pub fn delta(_before: &Snapshot) -> Snapshot {
        Snapshot
    }

    #[cfg(not(feature = "perf"))]
    #[inline]
    pub fn reset() {}
}
