# factory waveform transition table: start top black / bottom white,
# columns {RED,BW} = 00 01 10 11, one OTP activation with 0x21 = 00
from lab import *
init()
calibrate()
clean_clear()
base = cells(shoot("otp-base"), 4, 2)
log("baseline:\n" + fmt(base))
for c2, name in [(0x3C, "DU mode2 +temp"), (0x1C, "mode2 no temp"), (0x34, "mode1 +temp (clean)"), (0x14, "mode1 no temp")]:
    clean_clear()
    plane(0x26, "grid 4 1 0011")
    plane(0x24, "grid 4 1 0101")
    ctrl1(0x00)
    t = act(c2)
    c = cells(shoot(f"otp-{c2:02X}"), 4, 2)
    log(f"0x22={c2:02X} {name} busy_ms={t['busy_ms']}\n" + fmt(c - base))
