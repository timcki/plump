// background work queue
//
// offloads CPU-heavy processing (HTML strip, image decode) to a
// dedicated embassy task while the main UI loop stays responsive
//
// generation-based cancellation: bump generation and drain() to
// discard stale work; no explicit cancel signal needed
//
// channel capacity 2 for natural back-pressure; worker drops input
// buffers before sending results so peak heap is bounded

extern crate alloc;

use alloc::vec::Vec;
use core::cell::Cell;

use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

// 1-bit decoded image
pub struct DecodedImage {
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
    pub stride: usize,
}

impl core::fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DecodedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("data_len", &self.data.len())
            .field("stride", &self.stride)
            .finish()
    }
}

pub type ImageDecodeFn = fn(&[u8], bool, u16, u16) -> Result<DecodedImage, &'static str>;

static IMAGE_DECODER: Mutex<Cell<Option<ImageDecodeFn>>> = Mutex::new(Cell::new(None));

pub fn register_image_decoder(f: ImageDecodeFn) {
    critical_section::with(|cs| IMAGE_DECODER.borrow(cs).set(Some(f)));
}

fn get_image_decoder() -> ImageDecodeFn {
    critical_section::with(|cs| IMAGE_DECODER.borrow(cs).get())
        .expect("work_queue: no image decoder registered")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum BgWorkKind {
    Idle = 0,
    DecodeImage = 1,
}

/// A work-queue generation.
///
/// [`reset`] is the only mint: it bumps the counter, makes the new
/// value active, and drains both channels in one step, so a caller
/// cannot conjure a generation, arm one without draining, or mix up
/// the counter with the active value. Holding one is a claim on
/// results, not a way to make one current: [`resume`] takes a
/// previously minted value back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WorkGen(u16);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BgStatus {
    pub kind: BgWorkKind,
    pub generation: WorkGen,
}

impl BgStatus {
    pub const IDLE: Self = Self {
        kind: BgWorkKind::Idle,
        generation: WorkGen(0),
    };

    #[inline]
    pub const fn is_active(&self) -> bool {
        !matches!(self.kind, BgWorkKind::Idle)
    }
}

static STATUS: Mutex<Cell<BgStatus>> = Mutex::new(Cell::new(BgStatus::IDLE));

#[inline]
fn status() -> BgStatus {
    critical_section::with(|cs| STATUS.borrow(cs).get())
}

/// Returns `true` only when the worker is not active AND there is no
/// pending work in the input channel.  This avoids a race where the
/// main task submits work but the executor hasn't scheduled the worker
/// yet — `status()` still shows IDLE but the item is queued.
#[inline]
pub fn is_idle() -> bool {
    !status().is_active() && WORK_IN.is_empty()
}

fn set_status(s: BgStatus) {
    critical_section::with(|cs| STATUS.borrow(cs).set(s));
}

// the generation results are matched against; only reset/resume move it
static ACTIVE_GEN: Mutex<Cell<WorkGen>> = Mutex::new(Cell::new(WorkGen(0)));
// monotonic source of fresh generations; never read for matching
static GEN_COUNTER: Mutex<Cell<WorkGen>> = Mutex::new(Cell::new(WorkGen(0)));

fn next_generation() -> WorkGen {
    critical_section::with(|cs| {
        let c = GEN_COUNTER.borrow(cs);
        let g = WorkGen(c.get().0.wrapping_add(1));
        c.set(g);
        ACTIVE_GEN.borrow(cs).set(g);
        g
    })
}

#[inline]
fn active_generation() -> WorkGen {
    critical_section::with(|cs| ACTIVE_GEN.borrow(cs).get())
}

/// Make a previously minted generation active again, so in-flight
/// results submitted under it count as current. Used when an app that
/// owns pending work is resumed after another app reset the queue.
pub fn resume(g: WorkGen) {
    critical_section::with(|cs| ACTIVE_GEN.borrow(cs).set(g));
}

pub enum WorkTask {
    DecodeImage {
        path_hash: u32,
        data: Vec<u8>,
        is_jpeg: bool,
        max_w: u16,
        max_h: u16,
    },
}

struct WorkItem {
    generation: WorkGen,
    task: WorkTask,
}

pub enum WorkOutcome {
    ImageReady { path_hash: u32, image: DecodedImage },
    ImageFailed { path_hash: u32, error: &'static str },
}

pub struct WorkResult {
    generation: WorkGen,
    pub outcome: WorkOutcome,
}

impl WorkResult {
    /// Generation this result was submitted under.
    #[inline]
    pub fn generation(&self) -> WorkGen {
        self.generation
    }

    #[inline]
    pub fn is_current(&self) -> bool {
        self.generation == active_generation()
    }
}

static WORK_IN: Channel<CriticalSectionRawMutex, WorkItem, 2> = Channel::new();
static WORK_OUT: Channel<CriticalSectionRawMutex, WorkResult, 2> = Channel::new();

// true if the input channel has room for at least one more item.
#[inline]
pub fn can_submit() -> bool {
    !WORK_IN.is_full()
}

pub fn submit(generation: WorkGen, task: WorkTask) -> bool {
    WORK_IN.try_send(WorkItem { generation, task }).is_ok()
}

#[inline]
pub fn try_recv() -> Option<WorkResult> {
    WORK_OUT.try_receive().ok()
}

/// Resolves when a result is waiting in the output channel without
/// consuming it (level-triggered). The scheduler arms this only while
/// the app layer reported `BgOutcome::WaitingExternal`; armed while
/// idle, a stale-generation result nobody consumes would keep the
/// select permanently ready. Single-waker safe: the main loop is the
/// only async awaiter of WORK_OUT (the worker sends, apps try_recv).
pub fn result_ready() -> impl core::future::Future<Output = ()> + 'static {
    WORK_OUT.ready_to_receive()
}

pub fn drain() {
    while WORK_IN.try_receive().is_ok() {}
    while WORK_OUT.try_receive().is_ok() {}
}

/// Mint a fresh generation, make it active, and drop everything queued
/// under the old one. The only way to obtain a [`WorkGen`].
pub fn reset() -> WorkGen {
    let g = next_generation();
    drain();
    log::debug!("[work] reset -> gen {:?}", g);
    g
}

#[embassy_executor::task]
pub async fn worker_task() -> ! {
    log::debug!("[work] worker ready");

    loop {
        set_status(BgStatus::IDLE);
        let item = WORK_IN.receive().await;

        let g = item.generation;
        if g != active_generation() {
            log::debug!(
                "[work] skip stale item (gen {:?} != active {:?})",
                g,
                active_generation()
            );
            drop(item);
            continue;
        }

        match item.task {
            WorkTask::DecodeImage {
                path_hash,
                data,
                is_jpeg,
                max_w,
                max_h,
            } => {
                set_status(BgStatus {
                    kind: BgWorkKind::DecodeImage,
                    generation: g,
                });

                let fmt = if is_jpeg { "JPEG" } else { "PNG" };
                log::debug!(
                    "[work] img {:#010X}: decode {} ({} bytes, {}x{}, gen {:?})",
                    path_hash,
                    fmt,
                    data.len(),
                    max_w,
                    max_h,
                    g,
                );

                let decode = get_image_decoder();
                let result = decode(&data, is_jpeg, max_w, max_h);
                drop(data);

                if g != active_generation() {
                    log::debug!(
                        "[work] img {:#010X}: discarded (gen {:?} stale)",
                        path_hash,
                        g,
                    );
                    continue;
                }

                let outcome = match result {
                    Ok(image) => {
                        log::debug!(
                            "[work] img {:#010X}: {}x{} ({}B 1-bit)",
                            path_hash,
                            image.width,
                            image.height,
                            image.data.len(),
                        );
                        WorkOutcome::ImageReady { path_hash, image }
                    }
                    Err(e) => {
                        log::warn!("[work] img {:#010X}: decode failed: {}", path_hash, e,);
                        WorkOutcome::ImageFailed {
                            path_hash,
                            error: e,
                        }
                    }
                };

                WORK_OUT
                    .send(WorkResult {
                        generation: g,
                        outcome,
                    })
                    .await;
            }
        }
    }
}
