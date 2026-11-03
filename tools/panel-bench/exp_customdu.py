# custom delta LUT: rows 00/11 all VSS (no drive), 01 = VSL x2 phases,
# 10 = VSH1 x2 phases, 30 frames per phase. if static black still fades,
# the cause is DC VCOM during the waveform, not the OTP LUT0 row
import exp_ghost as g
from lab import *
from exp_ghost3 import variant

def custom_turns(n, frames=30, vcom=0x30):
    b = lut({1: [0xA0], 2: [0x50]}, groups=[(frames, frames, 0, 0, 0)])
    v = dict(PROD_VOLT, vcom=vcom)
    for k in range(1, n + 1):
        plane(0x24, g.page(k))
        load_lut(b, v)
        ctrl1(0x00)
        t = act(0xCC)
    log(f"  custom busy_ms={t['busy_ms']}")
    # custom LUT pass swaps planes too; keep RED = shown page explicitly
    plane(0x26, g.page(n))

init()
calibrate()
variant("c_custom_x1", lambda: custom_turns(1))
variant("c_custom_x11", lambda: custom_turns(11))
variant("c_custom_vcom08_x11", lambda: custom_turns(11, vcom=0x08))
variant("c_delta_x11", lambda: g.turns(11))
