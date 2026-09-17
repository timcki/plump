# AGENTS.md — pulp-os contributor guide

## What this is

Bare-metal e-reader firmware for the **XTEink X4** (ESP32-C3 RISC-V + SSD1677 800×480 e-paper). Written in Rust, `#![no_std]`, no heap-allocated dispatch, no framebuffer. Async runtime via Embassy on esp-rtos.

The codebase splits into a **kernel** (hardware drivers, scheduling, storage) and a **distro** (apps, fonts, UI widgets). The kernel is generic over the app layer — it never names a concrete app. The distro defines `AppId`, implements `AppLayer`, brings fonts, and writes `main.rs`.

---

## Hardware

| Resource | Detail |
|----------|--------|
| MCU | ESP32-C3, single-core RISC-V RV32IMC, 160 MHz |
| RAM | 400 KB DRAM; ~160 KB heap in a single contiguous region (pinned stack top → end of bootloader-reclaimed RAM), TLSF allocator |
| Stack | 58 KB pinned right after .bss (see `ld/stack-plump.x`), painted with `0xDEAD_BEEF` canary at boot, high-water-mark logged every 5s |
| Display | SSD1677 mono e-paper (GDEQ0426T82, 4.26"), 800×480 physical = **219 PPI**, displayed in portrait (480×800) via 270° rotation. 1 pt = 3.04 px; the portrait page is 55.6 × 92.7 mm |
| Storage | microSD over shared SPI bus (400 kHz probe → 20 MHz run), FAT32 |
| Input | 2 ADC resistance ladders (GPIO1, GPIO2) + power button (GPIO3 IRQ) |
| Battery | Li-ion via ADC, 100K/100K divider on GPIO0 |
| SPI bus | Single SPI2 with DMA (GDMA CH0, 4096B TX+RX), shared between EPD and SD via `CriticalSectionDevice` |

### Pin map

```
GPIO0  battery ADC          GPIO6  EPD BUSY
GPIO1  button row 1 ADC     GPIO7  SPI MISO
GPIO2  button row 2 ADC     GPIO8  SPI SCK
GPIO3  power button          GPIO10 SPI MOSI
GPIO4  EPD DC               GPIO12 SD CS (raw register GPIO)
GPIO5  EPD RST              GPIO13 battery latch (raw register GPIO)
                            GPIO21 EPD CS
```

---

## Development loop (physical device)

### Prerequisites

- Stable Rust ≥ 1.88 (`rust-toolchain.toml` handles this)
- `riscv32imc-unknown-none-elf` target (auto-installed by toolchain file)
- `espflash` (`cargo install espflash`)
- XTEink X4 connected via USB (ESP32-C3 built-in USB-JTAG/serial)
- Sibling directory `../smol-epub` (clone from `github.com/hansmrtn/smol-epub`)

### Build → flash → monitor in one command

```
cargo run --release
```

`.cargo/config.toml` sets the runner to `espflash flash --monitor --chip esp32c3`. This builds, flashes over USB, reboots the chip, and opens the serial console. All `log::info!` output streams in real time.

### Typical cycle timing

| Step | Time |
|------|------|
| Incremental build (small change) | ~10-15s |
| Clean build / big change | ~25s |
| Flash over USB | ~3s |
| Boot + SD init + first EPD refresh | ~3s |
| Navigate to what you're testing | 1-5s |
| **Total round-trip** | **~20-30s** |

### What you see on serial

```
INFO - booting...
INFO - power button: GPIO3 interrupt armed (FallingEdge)
INFO - SPI bus: DMA enabled (CH0, 4096B TX+RX)
INFO - SD card: initialised (attempt 1)
INFO - SD card: 15931539456 bytes (15196 MB)
INFO - SPI bus: 400kHz -> 20MHz
INFO - SD card: filesystem mounted
INFO - bookmarks: loaded 3 entries from SD
INFO - settings: sleep=300 ghost=6 bookfont=2 uifont=1
INFO - ui ready.
INFO - stats: heap 12/160K peak 48K | stack free 42K hwm 14K | bat 87% 3.9V | up 0:05 | SD:ok
```

Then every 5 seconds: heap usage, stack watermark, battery percentage, uptime, SD status.

### Debugging

- **Serial logging**: `log::info!()` / `log::debug!()` — primary debug tool. Controlled by `ESP_LOG` env var in `.cargo/config.toml`
- **Panic backtraces**: `esp-backtrace` prints stack traces over serial before halting
- **GDB**: ESP32-C3 has built-in USB-JTAG — no probe needed. Use `espflash flash` then attach GDB separately. Debug builds (`cargo build` without `--release`) are better for stepping, but the binary is much larger
- **Memory stats**: Heap stats and stack high-water-mark are logged periodically. Stack is painted at boot and scanned for canary pattern

### Key environment

- `ESP_LOG=info` — log level (change to `debug` or `trace` for more)
- Release profile: `opt-level = 's'`, LTO fat, single codegen unit — optimized for size

---

## Repository layout

```
├── kernel/                         pulp-kernel workspace crate (zero app imports)
│   └── src/
│       ├── lib.rs                  crate root, re-exports
│       ├── error.rs                unified Error type (Copy, kind + source tag)
│       ├── kernel/
│       │   ├── mod.rs              Kernel struct — Screen half (display hw) +
│       │   │                       Services half (SD, caches, policy state)
│       │   ├── app.rs              App trait, AppLayer trait, Launcher nav stack,
│       │   │                       Transition, Redraw, AppContext, QuickAction protocol
│       │   ├── scheduler.rs        main event loop, render pipeline, sleep
│       │   ├── screen.rs           typestate refresh sessions (Wave/Settled) over the
│       │   │                       EPD driver; owns red_stale + partial counter
│       │   ├── handle.rs           KernelHandle — app-facing syscall API (file I/O, caches)
│       │   ├── tasks.rs            Embassy tasks: input polling, housekeeping, idle timeout
│       │   ├── work_queue.rs       background work with generation-based cancellation
│       │   ├── bookmarks.rs        16-slot LRU bookmark cache, binary format on SD
│       │   ├── config.rs           SETTINGS.TXT parser/writer, SystemSettings, WifiConfig
│       │   ├── dir_cache.rs        sorted directory cache with title resolution
│       │   ├── console.rs          boot console (FONT_9X18, no fontdue dependency)
│       │   ├── rtc_session.rs      deep-sleep session persistence via RTC FAST memory
│       │   ├── timing.rs           timing constants (poll intervals, debounce, coalescing)
│       │   └── wake.rs             uptime helper (embassy monotonic clock)
│       ├── board/                  board support (pin map, SPI wiring, button layout)
│       │   ├── mod.rs              Board::init — peripheral splitting, DMA setup
│       │   ├── action.rs           ActionEvent — semantic button actions, ButtonMapper
│       │   ├── battery.rs          Li-ion discharge curve calibration
│       │   ├── button.rs           physical Button enum, ADC ladder decoding
│       │   ├── layout.rs           physical button position constants for feedback rendering
│       │   └── raw_gpio.rs         register-level GPIO for SD CS (GPIO12) and the battery
│       │                           latch (GPIO13), deep-sleep pad holds
│       ├── drivers/
│       │   ├── mod.rs              driver re-exports
│       │   ├── ssd1677.rs          EPD display driver — 3-phase partial refresh, strip streaming
│       │   ├── strip.rs            4 KB strip buffer — rotation, glyph blitting, DrawTarget impl
│       │   ├── sdcard.rs           SD card init, sync-to-async adapter, poll_once
│       │   ├── storage.rs          FAT filesystem ops — CRUD for root, _PULP/, subdirs
│       │   ├── input.rs            ADC button polling, debounce, long-press, repeat
│       │   └── battery.rs          ADC-to-millivolt conversion, percentage interpolation
│       ├── ui/
│       │   ├── mod.rs              re-exports, screen dimension constants
│       │   ├── layout.rs           shared layout constants (margins, content top, header width)
│       │   ├── stack_fmt.rs        no-alloc fmt::Write buffers (StackFmt<N>, BorrowedFmt)
│       │   ├── statusbar.rs        stack paint/measurement utilities for RISC-V
│       │   └── widget.rs           Region, Alignment, progress bar, loading indicator
│       └── util/
│           ├── mod.rs              utility re-exports
│           └── utf8.rs             no_std UTF-8 decoder (single-char + iterator)
│
├── src/                            distro / app layer
│   ├── bin/main.rs                 entry point — hardware init, kernel + app manager construction
│   ├── lib.rs                      crate root, re-exports kernel modules
│   ├── ui/
│   │   └── mod.rs                  unified re-exports (kernel primitives + app widgets)
│   ├── fonts/
│   │   ├── mod.rs                  font size tiers (XSmall–XLarge), FontSet lookups
│   │   └── bitmap.rs              BitmapFont struct, glyph lookup, string drawing
│   ├── ui/
│   │   ├── mod.rs                  unified re-exports + chrome
│   │   └── chrome/                 top bar (THE navigation: nameplate +
│   │                               two named arms + battery), panel,
│   │                               section label, filter chip
│   └── apps/
│       ├── mod.rs                  AppId, Modal, Tab + Nav aliases
│       ├── tab.rs                  Tab enum, neighbour helpers, icon
│       ├── manager.rs              AppManager - AppLayer impl, dispatch,
│       │                           lifecycle, Chrome state refresh
│       ├── home.rs                 Continue-reading card + Recent list
│       ├── library.rs              scrollable book list + count caption
│       ├── cover_placeholder.rs    deterministic shape covers
│       ├── cover_cache.rs          bundle Cover variant read / write
│       ├── settings/
│       │   ├── mod.rs              grouped settings (Reading / Display /
│       │   │                       System): input state machine, draw
│       │   ├── model.rs            one declaration per setting (label,
│       │   │                       Domain, get/set onto SystemSettings)
│       │   ├── layout.rs           where the rows land, Damage
│       │   └── cache.rs            per-book cache sheet: picker,
│       │                           Rebuild / Forget, bounded clear
│       ├── stats.rs                Today / Lifetime / Most time spent
│       ├── upload.rs               WiFi HTTP upload server + mDNS
│       ├── reader/
│       │   ├── mod.rs              reader state machine, draw, footer
│       │   ├── paging.rs           text wrapping, page navigation
│       │   ├── epubs.rs            EPUB ZIP/OPF pipeline, chapter cache
│       │   ├── images.rs           inline image decode + dithering
│       │   └── layout/             K-P typesetting: items, breaker,
│       │                           paginate (quarter-line fill), pipeline,
│       │                           PIDX cache (scan lives in smol-epub)
│       └── widgets/
│           ├── mod.rs              widget re-exports
│           ├── row.rs              THE list row (lead / title + sub or
│           │                       bar / value) + RowGroup; every list
│           │                       on the device draws through it
│           ├── bitmap_label.rs     proportional text labels
│           ├── sheet.rs            bottom sheet: frame, header, row
│           │                       groups, hints (menu + contents)
│           ├── sleep_card.rs       continue-reading card painted over
│           │                       the sleep wallpaper (time left,
│           │                       battery)
│           ├── quick_menu.rs       Menu-button sheet (app actions +
│           │                       clear ghosting, go home / sleep)
│           ├── button_feedback.rs  legacy bumps (drawn only when chrome
│           │                       is hidden)
│           ├── selectable_row.rs   inverted-color selection highlight
│           ├── list.rs             ListSelection cursor + scrolling helper
│           └── format.rs           position/percentage formatting helpers
│
├── ld/                             vendored linker chain: pinned stack +
│                                   single contiguous heap (linkall.x,
│                                   esp32c3-plump.x, stack-plump.x)
├── vendor/
│   └── embedded-sdmmc-rs/          hansmrtn async fork of embedded-sdmmc
│                                   with a local patch (FAT32 directory
│                                   walk stops at the end marker); see
│                                   PLUMP-VENDOR.md inside
├── assets/
│   ├── fonts/                      TTF files (regular, bold, italic)
│   └── upload.html                 web UI for WiFi upload mode
│
├── build.rs                        fontdue TTF rasterization at compile time,
│                                   vendored linker chain wiring
├── Cargo.toml                      workspace root
├── rust-toolchain.toml             stable ≥ 1.88 + riscv32imc target
└── .cargo/config.toml              target, runner, build-std, rustflags
```

---

## Architecture deep dive

### Kernel / app split

The `kernel/` crate has **zero imports** from `src/apps/` or `src/fonts/`. The scheduler is generic over `AppLayer`; it never names a concrete app. `AppId` is defined by the distro side in `src/apps/mod.rs`. The kernel only knows `AppIdType::HOME`.

The distro implements `AppLayer` via `AppManager` in `src/apps/manager.rs`, which uses the `with_app!()` macro to dispatch method calls to concrete app structs. This expands to a match on `AppId` at compile time — all monomorphized, no vtable, no `Box`, no `dyn`.

### `no_std` patterns and constraints

**No standard library.** `#![no_std]` with `extern crate alloc`. The heap is a single contiguous ~160 KB region spanning from the pinned stack top to the end of the RAM reclaimed from the 2nd-stage bootloader, managed by `esp_alloc` with the TLSF backend (`ESP_ALLOC_CONFIG_HEAP_ALGORITHM` in `.cargo/config.toml`). The layout comes from the vendored linker chain in `ld/` (`linkall.x` → `esp32c3-plump.x` → `stack-plump.x`), which pins the stack after `.bss` instead of letting it split free DRAM in two; link-time ASSERTs trip if an esp-hal upgrade regresses the layout. Everything else is static or stack.

**Heavy statics.** Large structs live in `ConstStaticCell` / `StaticCell` so the async future stays ~200 bytes:
- `ReaderApp` — ~28 KB (page buffers, chapter state, image cache)
- `DirCache` — ~10 KB (128 directory entries)
- `StripBuffer` — ~4 KB
- `BookmarkCache` — ~1 KB
- `Launcher`, `QuickMenu`, `ButtonFeedback` — smaller

**No `dyn` dispatch.** The `with_app!()` macro in `manager.rs` matches on `AppId` and calls concrete methods. No trait objects, no heap-allocated closures.

**No `std::fmt`.** Stack-based formatting via `StackFmt<N>` and `BorrowedFmt` in `kernel/src/ui/stack_fmt.rs`. These are `fmt::Write` implementations over fixed-size stack buffers that silently truncate on overflow.

**No `std::io`.** All file I/O goes through `KernelHandle` → `storage.rs` → `embedded-sdmmc` `AsyncVolumeManager`. The `poll_once` function drives async futures to completion in a single poll — correct because the SPI bus is blocking (DMA completes before return, never pends).

**No `String` / `Vec` in hot paths.** `alloc::vec::Vec` is used only in the EPUB reader for chapter text buffers and image decode. Everything else uses fixed-size arrays and stack buffers.

**UTF-8 handling.** Custom `decode_utf8_char` and `Utf8Iter` in `kernel/src/util/utf8.rs` — no dependency on `std::str` beyond `core::str::from_utf8`.

### Async runtime

Embassy executor on esp-rtos. Five concurrent tasks:

| Task | Role | File |
|------|------|------|
| `main` | Event loop: input dispatch, app work, rendering | `kernel/scheduler.rs` |
| `input_task` | 10 ms ADC poll (adaptive: fast when active, slow when idle), debounce, battery read | `kernel/tasks.rs` |
| `housekeeping_task` | Status bar (5s), SD check (30s), bookmark flush (30s) | `kernel/tasks.rs` |
| `idle_timeout_task` | Configurable idle timer, signals deep sleep | `kernel/tasks.rs` |
| `worker_task` | Background CPU-heavy work (image decode) | `kernel/work_queue.rs` |

The CPU sleeps (WFI) whenever all tasks are waiting. Two genuine async suspension points in the main loop: (1) `select(input_events, work_ticker)` and (2) EPD busy-pin wait during render.

### Display rendering

**Strip-based, no framebuffer.** The 800×480 display is rendered in 12 horizontal strips of 40 rows each (4 KB per strip). The draw callback fires per strip during SPI DMA transfer. The `StripBuffer` implements `embedded_graphics::DrawTarget` for `BinaryColor`.

The display runs in **portrait mode** via 270° rotation of the physical 800×480 panel → 480×800 logical.

**`blit_1bpp_270` fast path** walks physical memory linearly for the portrait rotation — the innermost loop is sequential byte writes to the strip buffer, giving good cache performance.

**Typestate refresh sessions.** The scheduler never calls driver phases directly; it drives `kernel/screen.rs`: `screen.begin_partial(region, draw)` / `screen.begin_full(draw)` return a `Wave` that mutably borrows the `Screen` until consumed (`wave.settle()` → `Settled`, then `sync_red` / `abandon` / `grayscale` / `finish`). Misordered phases don't compile. `Screen` owns all display-plane state (`red_stale`, partial counter); `begin_partial` transparently picks `bw` vs `inv_red` recovery and expands to full screen when RED RAM is stale.

**3-phase partial refresh (inside a session):**
1. `begin_partial` — write new content to BW RAM via strip streaming, kick DU waveform (~400 ms). During the waveform the EPD charge pump drives pixels autonomously with no SPI traffic, so the SPI bus is free for SD I/O. `Services::wave_window` exploits this window
2. `settle()` after busy-low (or guard timeout)
3. `sync_red` — rewrite RED RAM only (BW already holds the content from phase 1). Skipped during rapid navigation via `abandon` (RED marked stale; next partial uses `inv_red` recovery). Full GC promoted after configurable number of partials to clear ghosting

**Full GC** comes in two kinds (`ssd1677::FullKind`). `Fast` runs the OTP full-clear waveform at a faked 90 °C (`0x1A ← 0x5A` with TEMP_LOAD cleared from CTRL2) so the controller picks the shortest high-temperature LUT, ~1.7 s instead of ~2.3 s (CrossPoint's HALF refresh); it is for entering a book and waking, where the point is to paint. `Clean` runs the same waveform at the real temperature (CTRL2 `0x34`, CrossPoint's FULL refresh) and is the only one that resets the pigment; the periodic ghost-clear promotion and the quick menu's Clear ghosting (`AppContext::request_clean_refresh`) use it. The faked waveform is under-driven at room temperature: a haze it leaves behind survives any number of fast clears.

**Whole-panel rule for custom LUTs.** The RAM window (0x44/0x45) only scopes RAM writes; a waveform scans every gate and drives every source from whatever the planes hold. The OTP DU treats plain content ({0,0}, {1,1}) as no-change, but the grayscale and revert LUTs read a different pair of bits, so AA gray codes must never sit in RAM while a windowed waveform runs (CrossPoint's driver enforces the same: `grayscaleRevert` before any `displayWindow`). `kernel/screen.rs` therefore runs gray passes full-screen only, and a windowed partial over a gray-coded panel first *neutralizes* it: full revert pass, then both planes rewritten with content and the screen marked stale. Every partial is full-screen today, so the neutralize path is not taken. Only the reader uses AA; Home runs plain BW.

**AA page turns re-drive.** After a gray pass both RAM planes hold AA codes, so the next turn reverts the grays and re-drives the whole panel through inv_red (`redrive=true` on every reader partial with AA on). That is a same-direction pulse over the entire background per turn and it accumulates as a grey haze; the periodic `Clean` GC is what resets it, so keep Ghost Clear low with AA on. Making those turns deltas needs the gray pass to keep the page content in RED, which in turn needs the LUT index order and pulse polarities of this panel measured, not inferred: two attempts on 2026-09-17 (content bit in RED with the codes re-homed) made things worse in different ways, so the encoding is unchanged until that is measured on the device.

**Panel power latching.** Every refresh path (DU, GC, grayscale) adds CLOCK_ON + ANALOG_ON to CTRL2 only when power is actually off, so consecutive page turns skip the ~100 ms booster start inside the waveform and the ~200 ms power-off wait after phase 3 (also from CrossPoint). The rails are dropped once nothing has been refreshed for `PANEL_IDLE_OFF_SECS` (30 s, from the parked main loop only) and at deep sleep, since a powered booster draws its quiescent current for the whole awake session. Sunlight mode overrides this: its waveforms carry ANALOG_OFF + CLOCK_OFF so the panel powers off after every refresh to prevent UV-induced fading.

**Dirty-region tracking.** Apps call `ctx.mark_dirty(region)`; regions are unioned per frame. Partial DU or full GC issued accordingly. Coalesced redraw for batch updates (e.g., background title scanning) with a 50 ms window.

### SPI bus sharing

EPD and SD share one SPI2 bus via `CriticalSectionDevice` (RefCell under the hood):

1. Background SD I/O runs **before** any EPD render pass
2. `poll_housekeeping` may do SD I/O, also before render
3. `render()` touches the EPD; during the waveform window `Services::wave_window` runs SD I/O because the EPD charge pump is driving pixels with no SPI commands
4. No SD I/O outside these three sites

The `Kernel` is split into a `Screen` half (EPD + strip + delay) and a `Services` half (SD, caches, policy state); a `Wave` session borrows the screen half while `wave_window` works the services half, so the phase interleaving is checked by the borrow checker. Violating the SPI ordering at runtime panics (RefCell double-borrow), never corrupts — enforced by the single-threaded executor, there's no preemption.

### Input system

Two ADC resistance ladders at 100 Hz (adaptive: fast when active, 50 ms slow when idle). 4-sample oversampling per read. 15 ms debounce, 1 s long-press, 150 ms repeat. The `InputDriver` produces raw `Event`s (Press/Release/LongPress/Repeat of physical `Button`). The `ButtonMapper` translates these to semantic `ActionEvent`s (Next/Prev/Select/Back/Menu). Apps never see hardware buttons.

### Navigation

**Five tab screens on a line with Home in the middle**: `Upload · Settings · HOME · Library · Stats`. Right walks out through the books, Left out through the device, and nothing is more than two presses from Home. `Tab::left`/`right` return `Option` — the line does not wrap, and both ends are visible in the chrome because the missing arm is simply absent.

**There is no bottom bar.** Five icon slots that could not be pressed (no touch on this device) were 64 px of an 800 px display drawing a control that does not exist. The top bar is the navigation instead: one cluster in the left corner reading `<- settings  home  library ->`, all lowercase with the current screen bold at body size, the battery in the far corner, and a hairline under it. `bottom_bar_h` is 8 (a margin), `top_bar_h` is 40, and every screen's row height derives from `Theme::content_bottom()` — so the 56 px reclaimed went into cover padding on Home and the Library and a sixth book on Stats. The reader is unaffected: it sizes its page from `SCREEN_H` and its own pads, never from the chrome constants, so no cached page index moved.

No per-screen figure lives in the bar, and Home carries no caption at all — card, hairline, four rows. Today's reading lives only on Stats (which reads it off the kernel), the book count in the library's caption, the firmware version in Settings' About group.

4-deep stack in `Launcher<Id>`. Transitions: `Push` (new app, suspend current), `Pop` (resume previous), `Replace` (swap current), `Home` (reset to home). Push degrades to Replace when stack is full. Each transition calls `on_suspend` / `on_exit` / `on_enter` / `on_resume` lifecycle methods on the affected apps.

### Two voices

A book's title is set in the **book's own face** (`fonts::body_font(reader_font.family(), tier)`, or `heading_font` for the home card); everything the device says about itself is set in the **UI face** (`ui_body_font` / `ui_heading_font` / `chrome_font`). This holds on every screen, not just inside the reader: the reader's contents sheet, home's card and recent list, and the stats book list all take the reader's family, so switching reader font changes how books are named everywhere. `AppManager::propagate_fonts` pushes it to each screen that names a book. Design: mockups/xteink_x4_tab_screens_v2.html.

### Font pipeline

`build.rs` rasterizes TTF files via fontdue at compile time into 2-bit (4-level) bitmaps. Five size tiers (XSmall through XLarge), three styles (Regular, Bold, Italic). ASCII glyphs (0x20–0x7E) are direct-indexed; extended Unicode (Latin-1, punctuation, currency, math, arrows) is binary-searched. Book and UI font sizes are independently configurable and hot-swappable.

**Per-family px ladders.** Each family in `FAMILIES` carries its own `body` / `heading` px arrays rather than sharing one table. At 219 PPI body text sits in the 16–36 px range, where unhinted rasterization is very sensitive to the exact ppem: at one size a stem lands on a pixel boundary and renders solid, one px either side it straddles two columns and smears into grays. The size that lands well differs per face, so a shared table guarantees some faces sit on a bad one — Bookerly's Medium is 22 px where Atkinson's is 23 px. Tiers were picked by measuring, per face and per candidate px, the fraction of vertical-stroke ink that rasterizes solid plus how close the x-height falls to a whole pixel; each tier holds its x-height across families so switching reader font doesn't change apparent size. Phosphor (icons, no ASCII to measure) rides the Inter ladder.

The kernel ships a built-in `FONT_9X18` mono font (embedded-graphics) for the boot console and sleep screen — works with zero fontdue, zero TTFs.

### Storage layout on SD

```
/                       root — user files (.txt, .epub)
/_PULP/                 app data directory
  SETTINGS.TXT          key=value config (sleep, fonts, theme, wifi)
  BKMK.BIN              recently-read LRU (16 × 48 bytes, binary)
  SESSION.BIN           sleep session copy (battery wakes lose RTC memory)
  PWR.LOG               append-only boot / sleep power log
  TITLES.BIN            filename→title mapping (tab-separated text)
  <hash>.DAT            per-book epub chapter cache (v3 format)
```

### Per-book cache, and clearing it

A book's derived bytes are the bundle (`_PLUMP/BOOKS/<H8>.BIN`) and its image directory (`_PLUMP/_HHHHHHH/`, one dithered figure per file), both keyed by the case-sensitive `fnv1a` of the filename. Its irreplaceable bytes are `_PLUMP/STATS/<filename>` (position and stats, the book record), its BKMK.BIN slot and the RECENT record, keyed by the filename itself. Settings > Book Cache is where that splits into two actions: **Rebuild cache** drops the derived bytes on one press, **Forget book** adds the rest behind a second. Sizes come from `SdStorage::measure_plump_subdir`; clearing is `purge_plump_subdir` in `PURGE_BATCH`-sized passes then `remove_plump_subdir`, which closes the cached `sub_handles` entry first because FAT will not unlink an open directory. Design: mockups/xteink_x4_settings_book_cache.html.

### Reading position

One record per book, `_PLUMP/STATS/<filename>` (`apps::book_record`), holds the position and the reading stats: key=value text closed by an `fnv1a` checksum line, so a torn write drops the position rather than inventing one. The reader is its only writer, from `flush_deferred_persistence` (page turns mark it dirty; flushed on transition, sleep and the 30 s debounce). The position carries the spine index and byte offset with the content format they were counted in, a format-independent anchor (paragraph and word ordinal, `smol_epub::markup::anchor_at` / `offset_of`) so a rebuilt or re-stripped bundle lands on the same sentence, a page hint valid only under the layout key it was counted under, and the display numbers (chapter number and count in TOC units, progress percent) so Home, the recent rows, the library, the quick menu and the sleep card all read the same figures without opening the bundle. The record is keyed by filename: Rebuild cache keeps it, Forget book deletes it, a changed archive size starts the book over.

Restore: `record_load` at NeedBookmark, `apply_pending_position` once the spine is known, `resolve_restore` at NeedPage (anchor to offset when the bundle's content format differs, hint dropped when the layout differs). A book without a record imports the old bundle-header bookmark or BKMK.BIN slot once. The RTC / SD session names the open book only; its place comes from the record the pre-sleep flush wrote.

BKMK.BIN (16-slot LRU, `kernel/bookmarks.rs`) now only orders the recently-read list on Home; its position fields are legacy.

### EPUB reader pipeline

Progressive state machine: `NeedBookmark → NeedInit → NeedOpf → NeedToc → NeedCache → NeedIndex → NeedPage → Ready`. Each state transition does a bounded amount of work and yields to the executor:

1. **ZIP init** — parse central directory from end of file
2. **OPF parse** — extract spine (chapter order), title, TOC file
3. **TOC build** — parse NCX or inline TOC
4. **Chapter cache** — decompress + HTML-strip each chapter (CSS resolved, see Text formatting) into the bundle
5. **Page index** — wrap text at current font size to build page boundaries
6. **Ready** — pages rendered on demand

Background caching runs during the EPD waveform window and between user inputs. It's interruptible: if input arrives during `run_background`, the future is dropped. Partial chapter cache writes are safe because `ch_cached` stays false until the full write completes.

**Loading screens hold.** A loading episode (`ReaderApp::begin_loading`: open, wake, chapter change, font change) paints nothing for its first 400 ms unless the next step is known to be slow (bundle miss, chapter not cached, no layout at this font). A fast open, wake or chapter crossing therefore costs one refresh, the page itself. When the screen does go up it is painted once (plate for entering a book, strip for moving inside it, footer already in its final place) and later stages mark only `STAGE_REGION`. The first frame after entering a book or waking is always a full clear (`first_paint_full`); the manager leaves that request to the reader. Design: mockups/xteink_x4_reader_loading.html.

Rendering takes priority over the background chain: the scheduler's `'bg` loop breaks to render whenever a redraw is render-ready, so loading percentages paint as they change and the first page shows as soon as the reader hits `Ready` — remaining caching continues during the paint's waveform window. Once the page is visible the caching indicator repaints only on 10% progress steps (each repaint costs a DU refresh).

### Text formatting

Styling is resolved **once, at strip time**, and rendered from a stream that every consumer decodes through one iterator.

**The stripper resolves the CSS cascade.** `HtmlStripStream` (smol-epub) walks each open tag through `user-agent defaults < book stylesheet < inline style` (`css::StyleProps::overlay`). The book's stylesheets come from the OPF manifest (`epub::for_each_stylesheet`), parsed at `NeedOpf` into one `CssRules` table (up to 256 rules, on the heap only while chapters are still being cached; `EpubState::css`). An element stack records what each open tag changed so its close restores exactly that: `<b><span class="bold">` does not emit a premature bold-off, `font-style: normal` inside `<em>` turns italic off and back on, an inline tag left open is unwound by the block that closes over it, `display: none` subtrees vanish. Margins collapse the CSS way; `margin-left` maps to left-indent levels; `page-break-before: always` and `<h1>` force a page.

**The stream** (`smol_epub::markup`): UTF-8 text, `\n` for `<br>`, `\n\n` between blocks, two-byte toggles for bold / italic / underline / strike / heading level, `BREAK` and `PAGE_BREAK`, the `IMG_REF` record, and one **absolute block record** `0x01 '{' <align|left> <text_indent_qem> <space_above_qem>` ahead of a paragraph that differs from the defaults. Lengths are quarter-em so the cached bytes are font-independent. `markup::Events` is the only decoder; the K-P item builder, the greedy wrapper, the image prescan and the renderer all use it.

**Layout.** The paragraph's first-line indent becomes a fixed Box at the head of its K-P items (`items::build_paragraph`), so the breaker needs no per-line widths and the justification maths is untouched. `LineRecord` packs left levels and first-line indent into `indent`, alignment, underline / strike line-start bits and the gap above into `align` (`LineLayout::pack_indent` / `pack_align`). `paginate` counts page fill in quarter-lines: a line costs four plus its gap (`gap_quarters`, from `em_px` and `line_h` at page time), the gap is dropped on a page's first line. Image-origin lines cover the whole `IMG_REF` record so a page that starts on an image holds its header.

**Rendering.** `build_page_runs` decodes a page once when it loads: per line the top y, the natural width and gap count, the justification (`extra px per gap`, remainder) and a table of placed style runs (`PageState::runs`, six per line, about 2 KB static). The twelve strip passes only cull runs by x and blit glyphs; no marker is decoded at draw time.

**Kerning.** The reader faces (Bookerly, Atkinson) keep their pair kerning in GPOS, which fontdue never reads, so `build.rs` walks the pair-adjustment lookups with `ttf-parser` and stores, per rasterised tier, two per-glyph class maps and a dense left x right class matrix (`BitmapFont::kern`, about 4 KB per tier, 140 KB of flash in all; Inter is drawn unkerned). One rule everywhere: kerning applies between consecutive glyphs of a word, never across a space, a marker or a style change. The K-P word measure (`items::measure_word`), the page-run measure (`paging::measure_bytes`) and the draw loop all apply it through `FontSet::kern`.

**Hyphenation.** `smol_epub::hyphen` wraps the `hypher` crate (Typst's Liang tries; English and Polish are compiled in, 43 KB of flash) behind smol-epub's `hyphenation` feature. The book's `dc:language` (`EpubMeta::lang`) picks the patterns; no language or an unknown one means only soft hyphens and explicit hyphens can break a word. `items::build_paragraph` turns each break point into a flagged `Item::discretionary` whose pre-break width is the hyphen glyph, and `breaker::consider_break` adds that width when breaking there; an explicit hyphen inside a word is a cheaper, unflagged break that adds nothing. No record bit marks a hyphenated line: `build_page_runs` sees a line that ends inside a word (letter on both sides, or at a soft hyphen) and appends the hyphen glyph to the line's width and drawing. `preindex.kp` logs `hyphenated=` per chapter for tuning `HYPHEN_PENALTY`.

To add an inline style: a marker pair in `markup.rs`, an arm in `Events::next_event`, a field in `markup::Style`, the stripper emitting it (`open_elem`), and a face or decoration in `fonts::Style::from_markup` / the draw loop. To add a block property: a field in `BlockProps` and its record, the stripper's `flush_prefix`, and wherever `LineLayout` needs to carry it. Bump `CONTENT_FMT_LATEST` for stream changes and `LAYOUT_ALGO_VERSION` for `LineRecord` bit changes. `cargo run --example strip_dump -- chapter.xhtml style.css` in `../smol-epub` shows what a real chapter strips to.

### Image decode

Inline images in EPUBs are detected by `IMG_REF` markers in the stripped text. Images are decoded (JPEG/PNG via smol-epub) in the `worker_task` to keep the UI responsive. The worker uses generation-based cancellation — navigating away bumps the generation and drains stale work. Decoded images are 1-bit Floyd-Steinberg dithered, cached to SD as raw bitmaps for instant reload.

### Deep sleep

Idle timeout or power long-press triggers sleep:
1. Save session to RTC FAST memory and to `_PULP/SESSION.BIN`, append a line to `_PULP/PWR.LOG`
2. Flush bookmarks to SD
3. Send CMD0 to SD card (reduces idle current from ~150 µA to ~10 µA)
4. Render sleep screen on EPD: the `SLEEP.BMP` wallpaper (4-level gray, two passes) with the distro's sleep card over it via `AppLayer::draw_sleep_overlay`, the card alone on plain paper when there is no wallpaper, the mono "(sleep)" text when there is no book either
5. EPD deep sleep mode 1 (~3 µA, image retained)
6. Release the battery latch: GPIO13 low, pad held through deep sleep (`board::battery_latch_off`). GPIO13 gates the board's battery-latch MOSFET (vendor firmware and CrossPoint do the same), so on battery the whole board, SD card and panel included, loses power here
7. On USB power the MCU continues into ESP32-C3 deep sleep (~5 µA), GPIO3 wake source

The sleep card is filled by `AppManager::fill_sleep_card` inside `on_active_pre_sleep`, before the active app drops its heap: title, author, chapter and page position from the reader (or the RECENT record when the reader is off the stack), this book's pace from its stats file (seconds per page, needing 10 pages and 5 minutes of history) for the time left in chapter and book, battery, and the bundle's Card cover box-filtered into an 84×126 fixed buffer. The card paints inside both wallpaper passes: `StripBuffer::fill_flat` clears the wallpaper's gray codes under it in the grayscale pass so its glyph edges are anti-aliased on flat paper.

On wake: the MCU resets and the boot sequence runs. On USB the RTC session is restored (instant return without SD reads); on battery the reset is a power-on, so the SD session copy provides the same resume one SD read later. `Board::init` releases the sleep-time pad holds and drives GPIO13 high again.

`_PULP/PWR.LOG` (append-only text, one line per boot and per sleep entry: reset reason, session source, wake count, uptime, input count, battery mV) is the record to read back when battery life looks wrong; the 5 s stats line also carries `idle`, `sleep_in` and `inputs` so an input source that keeps resetting the idle timer shows up as a countdown that never reaches zero.

### Work queue

Dedicated Embassy task for CPU-heavy operations (currently: image decode). Channel capacity 1 for natural back-pressure. Generation-based cancellation: bump a counter and drain channels; worker checks generation before and after processing. Input buffers are dropped before sending results so peak heap is bounded.

### Error handling

Unified `Error` type (`Copy`, 1 discriminant byte + 1 `&'static str` source tag). `ErrorKind` covers storage, parsing, resources, network, and catch-all. The `err!()` and `or_err!()` macros attach `module_path!()` as the source. `ResultExt` trait provides ergonomic `.source()` and `.map_kind()` on `Result`. Interop with smol-epub's `Result<T, &'static str>` via `From` impls.

### WiFi upload

Bypasses normal app dispatch. HTTP server on port 80 + mDNS (pulp.local) on port 5353. Multipart upload with 8.3 filename sanitization. Drag-and-drop web UI (served from `assets/upload.html` compiled into flash). Radio torn down before returning to app loop.

---

## How to add a new app

1. Create `src/apps/myapp.rs` with a struct implementing `App<AppId>`
2. Add `MyApp` variant to `AppId` in `src/apps/mod.rs`
3. Add the variant to the `with_app!()` macro in `src/apps/manager.rs`
4. Add the static cell in `src/bin/main.rs`
5. Wire it into `AppManager::new()` and `AppManager`'s field list
6. Add a menu entry in `src/apps/home.rs` to navigate to it

The kernel doesn't change at all.

## How to add a new widget

Font-dependent widgets go in `src/apps/widgets/`. Font-independent primitives go in `kernel/src/ui/`. If a widget only uses `Region`, `Alignment`, `StripBuffer`, and `BinaryColor`, it belongs in the kernel. If it needs `BitmapFont`, it belongs in the distro.

**Before writing a list, don't.** `widgets/row.rs` is the one list row and every list draws through it: the reader's menu and contents sheets, home's recent list, the settings screen, the stats book list, the book-cache picker. A screen that needs a list uses `RowGroup` (a stack of equal-height rows in one outline) or, inside a sheet, `SheetGeom`; both call `row::draw`, so the anatomy has a single definition. Adding a row capability means adding a field to `RowSpec`, not a second row.

## How to add a new setting

1. Add the field to `SystemSettings` in `kernel/src/kernel/config.rs`
2. Add the key to `apply_setting()` and `write_settings_txt()` in the same file
3. Add a `SettingId` variant, its `label`/`domain`/`get`/`set` arms and a `ROWS` entry in `src/apps/settings/model.rs`
4. If it needs to propagate to apps, add handling in `AppManager::propagate_settings()`

A row that opens a surface instead of stepping a value takes `Domain::Action` (inert `get`/`set`, `Activation::Open`) and owns its own state on `SettingsApp`; the manager draws such a sheet after the shared chrome, since the top bar wins the painter's algorithm over app content. `Book Cache` is the worked example.

---

## Key conventions

- **Apps never touch hardware.** All I/O goes through `KernelHandle`. Apps never see SPI, GPIO, or DMA.
- **No heap in the kernel.** Only the EPUB reader and image decoder allocate from the heap. Everything else is stack or static.
- **log::info! for everything.** Serial output is the primary debugging tool. Err on the side of logging too much — it costs nothing when the device isn't connected to USB.
- **Dirty-region tracking.** Always call `ctx.mark_dirty(region)` with the tightest possible region. The renderer only refreshes the dirty area. Full-screen redraws are expensive (~600ms for GC).
- **Yield for fairness.** Long-running sync work should intersperse `embassy_futures::yield_now().await` calls to let other tasks run.
- **poll_once is sacred.** Only use it for operations that are guaranteed to complete in a single poll (sync SPI). If the future could pend, it will panic.
- **One file open per read session.** Every `open_file_in_dir` scans the directory on SD, so a caller that needs several pieces of one file goes through `SdStorage::with_file_in_plump_subdir` (or `bundle::with_reader` for bundles) and does its reads on the open `FileReader`. The data dir and its subdirs (`BOOKS`, `STATS`) are opened once and cached in `SdStorageInner`; only `Scope::Named` opens a directory per call.
