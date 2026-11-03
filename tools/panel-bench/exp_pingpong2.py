# what the panel shows when DUs repeat with no RAM writes in between
from lab import *
init()
clean_clear("solid 1")
plane(0x24, "marker")
ctrl1(0x00)
for i in range(3):
    t = act(0x3C)
    r = shoot(f"pp-du{i+1}")
    # marker bar and dot vs page
    log(f"DU{i+1} busy_ms={t['busy_ms']} T-stem={r[60:190, 42:54].mean():.0f} page={r[300:500, 150:350].mean():.0f} dot={r[745:775, 425:455].mean():.0f}")
