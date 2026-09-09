/* replaces esp-hal's ld/sections/stack.x, included by the uniquely
   named vendored chain (linkall.x -> esp32c3-plump.x -> this file);
   plain filename shadowing loses to esp-hal's OUT_DIR in -L order.

   upstream makes the stack consume all DRAM left after .data/.bss,
   which places it directly below dram2_seg and splits free RAM into
   two heap regions that can never serve one large allocation.

   here the stack is pinned to its historical size right after .bss,
   and everything from the stack top to the end of dram2_seg becomes
   one contiguous heap, registered at boot via _heap_start/_heap_end
   (see src/bin/main.rs). keep this file in sync with upstream when
   bumping esp-hal; the ASSERTs below trip if the layout regresses. */

SECTIONS {
  .stack (NOLOAD) : ALIGN(4)
  {
    _stack_end = ABSOLUTE(.);
    _stack_end_cpu0 = ABSOLUTE(.);

    /* stack_guard for `stack-protector`; offset mirrors
       ESP_HAL_CONFIG_STACK_GUARD_OFFSET (default 60) */
    __stack_chk_guard = ABSOLUTE(_stack_end) + 60;

    /* pinned stack: 59552 bytes, the same envelope the leftover
       stack had before the heap merge */
    . += 0xE8A0;

    . = ALIGN (4);
    _stack_start = ABSOLUTE(.);
    _stack_start_cpu0 = ABSOLUTE(.);
  } > RWDATA
}

/* single contiguous heap: stack top .. end of reclaimed bootloader RAM */
_heap_start = _stack_start;
_heap_end = ORIGIN(dram2_seg) + LENGTH(dram2_seg);

ASSERT(_stack_start <= ORIGIN(dram2_seg), "pinned stack overlaps dram2_seg")
/* 158K: the reader's per-chapter page counts and the sheet menu's
   text buffers (2026-09) took ~1 KB of .bss from the original 160K */
ASSERT(_heap_end - _heap_start >= 158K, "merged heap smaller than expected; check .bss growth")
ASSERT(SIZEOF(.dram2_uninit) == 0, "dram2 statics exist; they would sit inside the merged heap")
