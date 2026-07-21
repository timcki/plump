/* vendored from esp-hal 1.0.0 generated linkall.x; passed by absolute
   path from build.rs so it beats esp-hal's copy. only change: includes
   esp32c3-plump.x instead of esp32c3.x (pinned-stack override) */

INCLUDE "memory.x"

REGION_ALIAS("ROTEXT", IROM);
REGION_ALIAS("RODATA", DROM);

REGION_ALIAS("RWDATA", DRAM);
REGION_ALIAS("RWTEXT", IRAM);

REGION_ALIAS("RTC_FAST_RWTEXT", RTC_FAST);
REGION_ALIAS("RTC_FAST_RWDATA", RTC_FAST);

INCLUDE "esp32c3-plump.x"
INCLUDE "hal-defaults.x"
