/* replaces esp-hal's ld/sections/text.x (1.0.0, preprocessed for
   riscv); see rwtext-plump.x for why the WiFi sections land here.
   re-diff on esp-hal bumps */

SECTIONS {

  .text : ALIGN(4)
  {
    KEEP(*(.init));
    KEEP(*(.init.rust));
    KEEP(*(.text.abort));
    *(.literal .text .literal.* .text.*)
    *( .wifiextrairam  .wifiextrairam.* )
    *( .wifislpiram  .wifislpiram.* )
    *( .wifislprxiram  .wifislprxiram.* )
  } > ROTEXT

}
