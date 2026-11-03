# bezel-normalised rerun of the key variants, each with its own reference
import exp_ghost as g
from lab import *
from exp_ghost3 import mixed, variant

init()
calibrate()
variant("n_delta_x1", lambda: g.turns(1))
variant("n_delta_x11", lambda: g.turns(11))
variant("n_delta_x31", lambda: g.turns(31))
variant("n_inv_red_x31", lambda: g.turns(31, "inv"))
variant("n_inv_every5_x31", lambda: mixed(31, 5))
variant("n_1C_x31", lambda: g.turns(31, c2=0x1C))
cmd("c 0C AE C7 C3 C0 80")
variant("n_booster80_x31", lambda: g.turns(31))
cmd("c 0C AE C7 C3 C0 40")
variant("n_delta_x31_again", lambda: g.turns(31))
