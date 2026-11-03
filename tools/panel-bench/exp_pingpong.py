# does a Mode 2 DU touch RAM (ping-pong), and what do 0x2D / 0x2F say
from lab import *

def ram(plane_sel: int, y: int = 479, n: int = 6) -> list[int]:
    cmd(f"c 41 {plane_sel:02X}", "c 4E 00 00", f"c 4F {y & 0xFF:02X} {y >> 8:02X}")
    return read(0x27, n + 1)

init()
log("0x2F after init:", hexs(read(0x2F, 1)))
log("0x2D after init:", hexs(read(0x2D, 11)))
clean_clear("solid 1")
log("0x2D after F4 (OTP mode 1 load):", hexs(read(0x2D, 11)))
log("0x2F:", hexs(read(0x2F, 1)), "temp 0x1B:", hexs(read(0x1B, 2)))
plane(0x24, "marker")       # BW = marker, RED stays white
log("before DU  BW :", hexs(ram(0)), " RED:", hexs(ram(1)))
ctrl1(0x00)
t = act(0x3C)
log("DU 0x3C busy_ms", t["busy_ms"])
log("after DU   BW :", hexs(ram(0)), " RED:", hexs(ram(1)))
log("0x2D after DU (mode 2 load):", hexs(read(0x2D, 11)))
t = act(0x3C)
log("second DU busy_ms", t["busy_ms"])
log("after DU2  BW :", hexs(ram(0)), " RED:", hexs(ram(1)))
