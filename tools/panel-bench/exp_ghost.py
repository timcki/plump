# page-turn ghosting at block level. pages A and B are 6x10 grids of
# 80 px cells; every cell is one of four transitions A->B (WW, BW =
# black->white, WB, BB), 15 each, shuffled so lighting gradients average
# out. each variant starts from a clean clear to A, turns N times ending
# on B, and is compared cell by cell to B after a clean clear.
#   ghost_bw: residual darkness where A was black, now white (want 0)
#   ghost_wb: residual lightness where A was white, now black (want 0)
#   haze_ww / haze_bb: drift of cells that never changed
import random
import sys
from lab import *

NX, NY = 6, 10
rng = random.Random(7)
classes = ["WW", "BW", "WB", "BB"] * 15
rng.shuffle(classes)
A = "".join("1" if c[0] == "W" else "0" for c in classes)
B = "".join("1" if c[1] == "W" else "0" for c in classes)
PA, PB = f"grid {NX} {NY} {A}", f"grid {NX} {NY} {B}"
idx = {k: [i for i, c in enumerate(classes) if c == k] for k in ("WW", "BW", "WB", "BB")}


def page(n: int) -> str:
    # turn n (1-based) shows B on odd turns, A on even
    return PB if n % 2 else PA


def turns(n, style="delta", c2=0x3C):
    for k in range(1, n + 1):
        new = page(k)
        if style == "delta":         # production: BW=new, DU, RED=new
            plane(0x24, new)
            ctrl1(0x00)
            act(c2)
            plane(0x26, new)
        elif style == "pp":          # trust ping-pong: BW only
            plane(0x24, new)
            ctrl1(0x00)
            act(c2)
        elif style == "inv":         # production redrive (abandoned phase 3)
            plane(0x24, new)
            plane(0x26, new + " inv")
            ctrl1(0x00)
            act(c2)


def measure(name, ref):
    c = cells(shoot(name), NX, NY, pad=14).ravel()
    d = c - ref
    m = {k: d[v].mean() for k, v in idx.items()}
    span = ref[idx["WW"]].mean() - ref[idx["BB"]].mean()
    log(f"{name:24s} bw={m['BW']:+6.1f} wb={m['WB']:+6.1f} ww={m['WW']:+6.1f} bb={m['BB']:+6.1f}"
        f"  (white-black span {span:.0f}; worst bw cell {d[idx['BW']].min():+.0f})")
    return m


def main():
    global ref
    init()
    calibrate()
    clean_clear(PB)
    ref = cells(shoot("ghost-ref"), NX, NY, pad=14).ravel()

    variants = [
        ("delta x1", lambda: turns(1)),
        ("delta x11", lambda: turns(11)),
        ("pingpong-only x11", lambda: turns(11, "pp")),
        ("inv_red x11", lambda: turns(11, "inv")),
        ("delta 0x1C x11", lambda: turns(11, c2=0x1C)),
        ("delta x31", lambda: turns(31)),
    ]
    only = sys.argv[1:]
    for name, run in variants:
        if only and not any(o in name for o in only):
            continue
        clean_clear(PA)
        run()
        measure(name.replace(" ", "_"), ref)

    clean_clear(PB)
    measure("ref-again", ref)


if __name__ == '__main__':
    main()
