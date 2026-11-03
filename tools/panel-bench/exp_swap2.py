# which 0x22 activations swap the 0x24/0x26 views, each twice, in two orders
from lab import *

def ram(sel):
    cmd(f"c 41 {sel:02X}", "c 4E 00 00", "c 4F DF 01")
    return read(0x27, 2)[1]

def probe(c2, custom):
    act(0xC0)
    plane(0x24, "solid 0")
    plane(0x26, "solid 1")
    if custom:
        load_lut(lut({}))
    ctrl1(0x00)
    t = act(c2)
    if t["done"] != "true":
        log(f"  0x22={c2:02X} wedged BUSY; reinit")
        init()
        return "wedged", t["busy_ms"]
    bw, red = ram(0), ram(1)
    return ("swap" if (bw, red) == (0xFF, 0x00) else "keep" if (bw, red) == (0x00, 0xFF) else f"?{bw:02X}/{red:02X}"), t["busy_ms"]

init()
clean_clear("solid 1")
codes = [0x3C, 0xFC, 0x1C, 0x34, 0xF7, 0x14, 0xD7, 0x99, 0xB9, 0x91, 0xB1, 0x0C, 0xCC, 0xCF, 0x04, 0xC4, 0xC7, 0x07, 0x0F, 0xC0, 0x03]
for order in (codes, codes[::-1]):
    for c2 in order:
        for custom in (False, True):
            if c2 in (0x3C, 0xFC, 0x1C, 0x34, 0xF7, 0x14, 0xD7) and custom:
                continue
            r = [probe(c2, custom) for _ in range(2)]
            r = [x if isinstance(x, tuple) else (x, 0) for x in r]
            log(f"0x22={c2:02X} {'custom' if custom else 'otp   '} {r[0][0]} {r[1][0]} busy_ms={r[0][1]}")
    log("--- reversed")
