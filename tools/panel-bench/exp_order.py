# which plane is the LUT index MSB, and what each VS code does.
# every run: clean clear (top black / bottom white), then RED = col bit 1,
# BW = col bit 0 (cols left->right {RED,BW} = 00 01 10 11), one LUT row
# live with one VS code for 40 frames, photo, 4x2 cell luminance
from lab import *

init()
clean_clear()
base = cells(shoot("order-base"), 4, 2)
log("baseline (top black, bottom white):\n" + fmt(base))
for code, name in [(0xAA, "VSL?10"), (0x55, "VSH1?01"), (0xFF, "VSH2?11")]:
    for row in range(4):
        clean_clear()
        plane(0x26, "grid 4 1 0011")
        plane(0x24, "grid 4 1 0101")
        load_lut(lut({row: [code]}, groups=[(10, 10, 10, 10, 0)]))
        t = act(0xCC)
        c = cells(shoot(f"order-{code:02X}-row{row}"), 4, 2)
        d = c - base
        log(f"code {code:02X} ({name}) row {row}: busy_ms={t['busy_ms']}\n" + fmt(d))
