# which activations swap the plane views (ping-pong)
from lab import *

def ram(sel):
    cmd(f"c 41 {sel:02X}", "c 4E 00 00", "c 4F DF 01")
    return read(0x27, 4)[1:]

def probe(label, c2, custom=False, c1=0x00):
    plane(0x24, "marker")      # first bytes 00 00 FF
    plane(0x26, "solid 1")     # FF FF FF
    if custom:
        load_lut(lut({}))      # all rows no-op
    ctrl1(c1)
    t = act(c2)
    bw, red = ram(0), ram(1)
    swapped = bw[0] == 0xFF and red[0] == 0x00
    log(f"{label:34s} 0x22={c2:02X} busy_ms={t['busy_ms']:>5} BW={hexs(bw)} RED={hexs(red)} swapped={swapped}")

init()
clean_clear("solid 1")
probe("OTP DU mode2 + LUT load", 0x3C)
probe("OTP DU mode2 no temp", 0x1C)
probe("OTP mode1 clean", 0x34)
probe("OTP mode1 + RED bypass", 0x34, c1=0x40)
probe("OTP DU mode2 + RED bypass", 0x3C, c1=0x40)
probe("custom LUT, 0x0C (mode2 bit)", 0x0C, custom=True)
probe("custom LUT, 0xCC", 0xCC, custom=True)
probe("custom LUT, 0x04 (mode1 bit)", 0x04, custom=True)
probe("custom LUT, 0xC7 (mode1 +off)", 0xC7, custom=True)
probe("custom LUT, 0xCF (mode2 +off)", 0xCF, custom=True)
