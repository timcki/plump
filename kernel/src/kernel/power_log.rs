// persistent power log on SD (_PULP/PWR.LOG): one line per boot and
// one per sleep entry, so multi-day battery behaviour can be read
// back from the card without a serial console attached. every line
// is also echoed at info level for the tethered dev loop
//
// example:
//   boot reason=Some(CoreDeepSleep) session=SD wakes=41 bat=3912mV
//   sleep reason="idle timeout" app=Reader up=734s inputs=52 bat=3898mV wakes=42
//
// the file is append-only and never truncated; at ~80 bytes per line
// a wake/sleep pair costs ~160 bytes, so a year of heavy use stays
// under 2 MB. delete it from the card to reset

use core::fmt::Write;

use crate::drivers::sdcard::SdStorage;
use crate::ui::stack_fmt::StackFmt;

pub const FILE: &str = "PWR.LOG";

pub fn boot(
    sd: &SdStorage,
    sd_ok: bool,
    reason: &dyn core::fmt::Debug,
    session: &str,
    wakes: Option<u32>,
    bat_mv: u16,
) {
    let mut line = StackFmt::<160>::new();
    let _ = write!(
        line,
        "boot reason={:?} session={} wakes={} bat={}mV",
        reason,
        session,
        wakes.unwrap_or(0),
        bat_mv
    );
    append(sd, sd_ok, line.as_str());
}

#[allow(clippy::too_many_arguments)]
pub fn sleep(
    sd: &SdStorage,
    sd_ok: bool,
    reason: &str,
    app: &dyn core::fmt::Debug,
    up_secs: u64,
    inputs: u32,
    bat_mv: u16,
    wakes: u32,
) {
    let mut line = StackFmt::<160>::new();
    let _ = write!(
        line,
        "sleep reason=\"{}\" app={:?} up={}s inputs={} bat={}mV wakes={}",
        reason, app, up_secs, inputs, bat_mv, wakes
    );
    append(sd, sd_ok, line.as_str());
}

fn append(sd: &SdStorage, sd_ok: bool, line: &str) {
    log::info!("power: {}", line);
    if !sd_ok {
        return;
    }
    let mut buf = StackFmt::<176>::new();
    let _ = writeln!(buf, "{}", line);
    if let Err(e) = sd.append_in_plump(FILE, buf.as_str().as_bytes()) {
        log::warn!("power log: append failed: {}", e);
    }
}
