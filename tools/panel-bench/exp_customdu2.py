# custom delta LUT that holds static black: row 00 gets a short VSH1
# pulse, 01 = VSL, 10 = VSH1, 11 = nothing. sweeps the hold pulse and
# VCOM. each turn writes both planes (custom passes swap the views)
import sys
import exp_ghost as g
from lab import *
from exp_ghost3 import variant

def du_lut(drive=30, hold=8, white_hold=0):
    # G0: two drive phases (rows 01/10), G1: one hold phase (rows 00/11)
    rows = {0: [0x00, 0x40 if hold else 0], 1: [0xA0, 0x80 if white_hold else 0],
            2: [0x50, 0], 3: [0x00, 0x80 if white_hold else 0]}
    h = max(hold, white_hold)
    return lut(rows, groups=[(drive, drive, 0, 0, 0), (h, 0, 0, 0, 0)])

def custom_turns(n, vcom=0x10, **kw):
    b = du_lut(**kw)
    v = dict(PROD_VOLT, vcom=vcom)
    for k in range(1, n + 1):
        plane(0x24, g.page(k))
        plane(0x26, g.page(k - 1) if k > 1 else g.PA)
        load_lut(b, v)
        ctrl1(0x00)
        t = act(0xCC)
    log(f"  busy_ms={t['busy_ms']}")

if __name__ == "__main__":
  init()
  calibrate()
  for vcom, hold in [(0x10, 0), (0x10, 6), (0x10, 12), (0x20, 6)]:
    variant(f"h_v{vcom:02X}_hold{hold}_x11", lambda vcom=vcom, hold=hold: custom_turns(11, vcom, hold=hold))
