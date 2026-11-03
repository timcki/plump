# head-to-head over 31 turns, same session, per-variant references
import exp_ghost as g
from lab import *
from exp_ghost3 import variant
from exp_customdu2 import custom_turns, du_lut

init()
calibrate()
variant("f_factory_du_x31", lambda: g.turns(31))
variant("f_inv_red_x31", lambda: g.turns(31, "inv"))
variant("f_custom_d40_h6_v18_x31", lambda: custom_turns(31, 0x18, drive=40, hold=6))
variant("f_custom_d40_h6_v20_x31", lambda: custom_turns(31, 0x20, drive=40, hold=6))
