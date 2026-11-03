# OTP DU with VCOM overridden: load temp + mode-2 LUT (0xB9), write 0x2C,
# display without reload (0xCC). sweep for the value where static black
# and static white both stay put
import sys
import exp_ghost as g
from lab import *
from exp_ghost3 import variant

def vcom_turns(n, vcom):
    for k in range(1, n + 1):
        # 0xB9 swaps the plane views too, so write both after it
        act(0xB9)
        plane(0x24, g.page(k))
        plane(0x26, g.page(k - 1) if k > 1 else g.PA)
        ctrl1(0x00)
        if vcom is not None:
            cmd(f"c 2C {vcom:02X}")
        t = act(0xCC)
    log(f"  busy_ms={t['busy_ms']} 0x2D={hexs(read(0x2D, 2))}")
    plane(0x26, g.page(n))

init()
calibrate()
vals = [int(v, 16) for v in sys.argv[1:]] or [None, 0x10, 0x18, 0x20, 0x28]
for v in vals:
    variant(f"v_{'otp' if v is None else f'{v:02X}'}_x11", lambda v=v: vcom_turns(11, v))
