# is the static-black fade per turn or per elapsed second; booster level 2
import time
import exp_ghost as g
from lab import *

init()
calibrate()
clean_clear(g.PB)
ref = cells(shoot("ghost2-ref"), g.NX, g.NY, pad=14).ravel()

t0 = time.monotonic(); clean_clear(g.PA); g.turns(31); dt = time.monotonic() - t0
g.measure("delta_x31_again", ref)
log(f"31 turns took {dt:.0f} s")

clean_clear(g.PB)
time.sleep(dt)
g.measure(f"idle_{dt:.0f}s_no_turns", ref)

clean_clear(g.PA)
g.turns(1)
time.sleep(dt)
g.measure(f"delta_x1_then_idle", ref)

cmd("c 0C AE C7 C3 C0 80")
clean_clear(g.PB)
ref2 = cells(shoot("ghost2-ref-b80"), g.NX, g.NY, pad=14).ravel()
log(f"booster 80 clean ref vs booster 40 ref: ww {(ref2-ref)[g.idx['WW']].mean():+.1f} bb {(ref2-ref)[g.idx['BB']].mean():+.1f}")
for n in (1, 31):
    clean_clear(g.PA)
    g.turns(n)
    g.measure(f"booster80_delta_x{n}", ref2)
cmd("c 0C AE C7 C3 C0 40")
