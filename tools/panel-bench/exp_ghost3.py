# per-variant reference: lighting drifts over minutes, so each variant is
# measured against a clean B shot taken just before it
import exp_ghost as g
from lab import *

def mixed(n, every):
    for k in range(1, n + 1):
        new = g.page(k)
        plane(0x24, new)
        if k % every == 0:
            plane(0x26, new + " inv")
        ctrl1(0x00)
        act(0x3C)

def variant(name, run):
    clean_clear(g.PB)
    ref = cells(shoot(name + "-ref"), g.NX, g.NY, pad=14).ravel()
    clean_clear(g.PA)
    run()
    g.measure(name, ref)

def main():
    init()
    calibrate()
    variant("delta_x31", lambda: g.turns(31))
    variant("delta_inv_every5_x31", lambda: mixed(31, 5))
    variant("delta_inv_every10_x31", lambda: mixed(31, 10))
    variant("inv_red_x31", lambda: g.turns(31, "inv"))
    cmd("c 0C AE C7 C3 C0 80")
    variant("booster80_delta_x31", lambda: g.turns(31))
    cmd("c 0C AE C7 C3 C0 40")


if __name__ == '__main__':
    main()
