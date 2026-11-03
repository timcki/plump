# plane order with RED actually read: 0x21 = 00 (one byte) and 00 00
from lab import *

init()
clean_clear()
base = cells(shoot("order2-base"), 4, 2)
log("baseline:\n" + fmt(base))
for c1 in [(0x00,), (0x00, 0x00)]:
    for row in range(4):
        clean_clear()
        plane(0x26, "grid 4 1 0011")
        plane(0x24, "grid 4 1 0101")
        load_lut(lut({row: [0xAA]}, groups=[(10, 10, 10, 10, 0)]))
        ctrl1(*c1)
        t = act(0xCC)
        c = cells(shoot(f"order2-{len(c1)}-row{row}"), 4, 2)
        log(f"0x21={hexs(c1)} VSL row {row}: busy_ms={t['busy_ms']}\n" + fmt(c - base))
