/* replaces esp-hal's ld/sections/rwtext.x (1.0.0, preprocessed for
   riscv), included under a unique name for the same -L order reason as
   stack-plump.x. re-diff on esp-hal bumps.

   only change: .wifiextrairam, .wifislpiram and .wifislprxiram moved to
   flash (text-plump.x). they match ESP-IDF's ESP_WIFI_EXTRA_IRAM_OPT and
   ESP_WIFI_SLP_IRAM_OPT, both off by default in IDF: throughput and
   power-save knobs, not correctness. IRAM is only required while the
   flash cache is disabled, which nothing on this firmware does (no
   esp-storage, no flash writes). on the C3 IRAM is the same SRAM as the
   heap, so this hands ~17 KB of WiFi code that only upload mode runs
   back to the heap. .iram1 (IRAM_ATTR, ISR-safe code) stays in RAM */

.trap : ALIGN(4)
{
  _trap_section_origin = .;
  KEEP(*(.trap));
  *(.trap.*);
} > RWTEXT

.rwtext : ALIGN(4)
{
  . = ALIGN (4);
  *(.rwtext.literal .rwtext .rwtext.literal.* .rwtext.*)
  /* unconditionally add patched SPI-flash ROM functions (from esp-rom-sys) - the linker is still happy if there are none */
  *:esp_rom_spiflash.*(.literal .literal.* .text .text.*)
  . = ALIGN(4);
} > RWTEXT

.rwtext.wifi :
{
  . = ALIGN(4);
  *( .wifi0iram  .wifi0iram.*)
  *( .wifirxiram  .wifirxiram.*)
  *( .phyiram  .phyiram.*)
  *( .iram1  .iram1.*)
  *( .coexiram.* )
  . = ALIGN(4);

  _rwtext_len = . - ORIGIN(RWTEXT);
} > RWTEXT
