// hardware init, construct Kernel + AppManager, boot, run

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::ram;
use esp_hal::timer::timg::TimerGroup;
use log::info;

use plump::apps::Launcher;
use plump::apps::files::FilesApp;
use plump::apps::home::HomeApp;
use plump::apps::manager::AppManager;
use plump::apps::reader::ReaderApp;
use plump::apps::settings::SettingsApp;
use plump::apps::stats::StatsApp;
use plump::apps::widgets::{ButtonFeedback, QuickMenu};
use plump::board::action::ButtonMapper;
use plump::board::{Board, speed_up_spi};
use plump::drivers::battery;
use plump::drivers::input::InputDriver;
use plump::drivers::sdcard::SdStorage;
use plump::drivers::strip::StripBuffer;
use plump::kernel::BookmarkCache;
use plump::kernel::BootConsole;
use plump::kernel::Kernel;
use plump::kernel::dir_cache::DirCache;
use plump::kernel::tasks;
use plump::kernel::work_queue;
use plump::ui::paint_stack;
use static_cell::{ConstStaticCell, StaticCell};

esp_bootloader_esp_idf::esp_app_desc!();

// heavy statics: kept out of the async future to keep it ~200 B

static STRIP: ConstStaticCell<StripBuffer> = ConstStaticCell::new(StripBuffer::new());
static READER: ConstStaticCell<ReaderApp> = ConstStaticCell::new(ReaderApp::new());
static LAUNCHER: ConstStaticCell<Launcher> = ConstStaticCell::new(Launcher::new());
static QUICK_MENU: ConstStaticCell<QuickMenu> = ConstStaticCell::new(QuickMenu::new());
static BUMPS: ConstStaticCell<ButtonFeedback> = ConstStaticCell::new(ButtonFeedback::new());
static DIR_CACHE: ConstStaticCell<DirCache> = ConstStaticCell::new(DirCache::new());
static BM_CACHE: ConstStaticCell<BookmarkCache> = ConstStaticCell::new(BookmarkCache::new());
// BootConsole is heap-allocated during boot and dropped after display,
// reclaiming ~3 KB that would otherwise sit unused in .bss forever.

static HOME: StaticCell<HomeApp> = StaticCell::new();
static FILES: StaticCell<FilesApp> = StaticCell::new();
static SETTINGS: StaticCell<SettingsApp> = StaticCell::new();
static STATS: StaticCell<StatsApp> = StaticCell::new();

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    paint_stack();
    // 108 KB main DRAM heap; leaves ~56 KB for stack
    esp_alloc::heap_allocator!(size: 110_592);
    // reclaim ~64 KB from 2nd-stage bootloader; net heap ~172 KB
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64_000);

    let mut console = alloc::boxed::Box::new(BootConsole::new());
    console.push("plump 0.1.0");
    console.push("esp32c3 rv32imc 160mhz");
    console.push("heap: 172K (108K + 64K reclaimed)");

    info!("booting...");

    // Safety: TIMG0 and SW_INTERRUPT are cloned here and consumed by
    // esp_rtos::start. They are never used again after this point.
    // Board::init (which takes ownership of `peripherals`) does not
    // touch TIMG0 or SW_INTERRUPT, see the pin ownership table in
    // board/mod.rs for the full split.
    let timg0 = TimerGroup::new(unsafe { peripherals.TIMG0.clone_unchecked() });
    let sw_ints =
        SoftwareInterruptControl::new(unsafe { peripherals.SW_INTERRUPT.clone_unchecked() });
    esp_rtos::start(timg0.timer0, sw_ints.software_interrupt0);

    // Peripherals move into Board::init, which splits them across
    // init_input (ADC pins, GPIO3, IO_MUX) and init_spi_peripherals
    // (SPI2, DMA, display + SD GPIOs). each peripheral is used in
    // exactly one place, see the ownership table in board/mod.rs.
    let board = Board::init(peripherals);
    console.push("spi: dma ch0, 4096B tx+rx");

    let mut epd = board.display.epd;
    let mut delay = Delay::new();
    epd.init(&mut delay);
    console.push("epd: ssd1677 800x480 init");

    speed_up_spi();
    console.push("spi: 400kHz -> 20MHz");

    let sd = match board.storage.sd_card {
        Some(card) => {
            console.push("sd: card detected");
            SdStorage::mount(card).await
        }
        None => {
            console.push("sd: not found");
            SdStorage::empty()
        }
    };

    let sd_ok = sd.probe_ok();
    if sd_ok {
        console.push("sd: fat32 mounted");
        if let Err(e) = sd.ensure_plump_dir_async().await {
            console.push("sd: plump dir failed");
            log::warn!("ensure_plump_dir: {:?}", e);
        }
    }

    let mut input = InputDriver::new(board.input);
    let battery_mv = battery::adc_to_battery_mv(input.read_battery_mv());

    let mut kernel = Kernel::new(
        sd,
        epd,
        STRIP.take(),
        DIR_CACHE.take(),
        BM_CACHE.take(),
        delay,
        sd_ok,
        battery_mv,
    );

    let mut app_mgr = AppManager::new(
        LAUNCHER.take(),
        HOME.init(HomeApp::new()),
        FILES.init(FilesApp::new()),
        READER.take(),
        SETTINGS.init(SettingsApp::new()),
        STATS.init(StatsApp::new()),
        QUICK_MENU.take(),
        BUMPS.take(),
        ButtonMapper::new(),
    );

    console.push("kernel: constructed");

    // skip boot console on valid RTC wake — saves one full EPD refresh
    // (~1.6s). the console is only useful for cold boot diagnostics.
    if kernel.has_valid_session() {
        info!("boot: skipping boot console (RTC session valid)");
        drop(console); // reclaim ~3 KB of heap
    } else {
        kernel.show_boot_console(&console).await;
        drop(console); // reclaim ~3 KB of heap
    }

    kernel.boot(&mut app_mgr).await;

    // register the image decoder so the kernel's worker task can
    // decode JPEG/PNG without depending on smol-epub directly
    work_queue::register_image_decoder(|data, is_jpeg, max_w, max_h| {
        let raw = if is_jpeg {
            smol_epub::jpeg::decode_jpeg_fit(data, max_w, max_h)
        } else {
            smol_epub::png::decode_png_fit(data, max_w, max_h)
        };
        raw.map(|img| work_queue::DecodedImage {
            width: img.width,
            height: img.height,
            data: img.data,
            stride: img.stride,
        })
    });

    spawner
        .spawn(tasks::input_task(input))
        .expect("spawn input_task");
    spawner
        .spawn(tasks::housekeeping_task())
        .expect("spawn housekeeping_task");
    spawner
        .spawn(tasks::idle_timeout_task())
        .expect("spawn idle_timeout_task");
    spawner
        .spawn(work_queue::worker_task())
        .expect("spawn worker_task");
    info!("kernel ready.");

    kernel.run(&mut app_mgr).await
}
