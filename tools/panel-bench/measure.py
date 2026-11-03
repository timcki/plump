#!/usr/bin/env -S uv run --script
# /// script
# dependencies = ["numpy", "opencv-python-headless"]
# ///
"""rectify a camshot of the panel and measure regions.

  measure.py calibrate PHOTO          find the four corner squares of the
                                      `grid 12 20` calibration page, save
                                      the homography to target/cal.json
  measure.py rect PHOTO [OUT]         warp PHOTO to a 480x800 portrait image
  measure.py cols PHOTO N [ROWS]      mean luminance of N equal portrait
                                      columns (x ROWS rows, default 1)
  measure.py box PHOTO x y w h ...    mean / p5 / p95 luminance of boxes

luminance is reported raw (0..255, camera auto exposure) and, when the
photo shows the bezel strip under the panel, normalised against it.
"""

import json
import os
import sys

import cv2
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
CAL = os.path.join(HERE, "target", "cal.json")
W, H = 480, 800
# corner square centres of the calibration grid (12 x 20 cells of 40 px)
DST = np.float32([[20, 20], [460, 20], [20, 780], [460, 780]])


def gray(path: str) -> np.ndarray:
    img = cv2.imread(path)
    if img is None:
        sys.exit(f"measure: cannot read {path}")
    return cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)


def squares(g: np.ndarray, c: int, block: int) -> np.ndarray:
    th = cv2.adaptiveThreshold(g, 255, cv2.ADAPTIVE_THRESH_MEAN_C, cv2.THRESH_BINARY_INV, block, c)
    th = cv2.morphologyEx(th, cv2.MORPH_OPEN, np.ones((3, 3), np.uint8))
    n, _, stats, cents = cv2.connectedComponentsWithStats(th)
    out = []
    for i in range(1, n):
        x, y, w, h, a = stats[i]
        area = g.shape[0] * g.shape[1]
        if 2e-4 * area < a < 1e-2 * area and 0.7 < w / h < 1.4 and a > 0.75 * w * h:
            out.append((a, *cents[i]))
    return np.float32(out).reshape(-1, 3)


def calibrate(path: str) -> None:
    # local threshold: glare makes some squares mid-gray and the dark
    # background around the device defeats a global one. sweep the
    # offset until exactly four similar compact blobs remain
    g = cv2.GaussianBlur(gray(path), (5, 5), 0)
    pts = None
    for block in (51, 151, 251):
        for c in (40, 32, 25, 20, 16, 12, 9):
            q = squares(g, c, block)
            if len(q) == 4 and q[:, 0].max() < 2.5 * q[:, 0].min():
                pts = q[:, 1:]
                break
        if pts is not None:
            break
    if pts is None:
        sys.exit("measure: corner squares not found")
    s = pts.sum(1)
    d = pts[:, 0] - pts[:, 1]
    tl, br = pts[np.argmin(s)], pts[np.argmax(s)]
    tr, bl = pts[np.argmax(d)], pts[np.argmin(d)]
    src = np.float32([tl, tr, bl, br])
    m = cv2.getPerspectiveTransform(src, DST)
    os.makedirs(os.path.dirname(CAL), exist_ok=True)
    with open(CAL, "w") as f:
        json.dump({"h": m.tolist(), "src": src.tolist(), "photo": path}, f)
    print("corners", src.tolist())
    print("saved", CAL)


def rect(path: str) -> np.ndarray:
    with open(CAL) as f:
        m = np.array(json.load(f)["h"])
    return cv2.warpPerspective(gray(path), m, (W, H), flags=cv2.INTER_AREA)


def stat(a: np.ndarray) -> str:
    return f"mean={a.mean():6.1f} p5={np.percentile(a, 5):6.1f} p95={np.percentile(a, 95):6.1f}"


def main() -> None:
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    cmd, path = sys.argv[1], sys.argv[2]
    if cmd == "calibrate":
        calibrate(path)
        return
    r = rect(path)
    # inset so edge blur and calibration error stay out of every box
    pad = 6
    if cmd == "rect":
        out = sys.argv[3] if len(sys.argv) > 3 else os.path.splitext(path)[0] + "-rect.png"
        cv2.imwrite(out, r)
        print(out)
    elif cmd == "cols":
        n = int(sys.argv[3])
        rows = int(sys.argv[4]) if len(sys.argv) > 4 else 1
        for j in range(rows):
            y0, y1 = j * H // rows, (j + 1) * H // rows
            cells = []
            for i in range(n):
                x0, x1 = i * W // n, (i + 1) * W // n
                cells.append(r[y0 + pad : y1 - pad, x0 + pad : x1 - pad].mean())
            print(f"row {j}: " + " ".join(f"{c:6.1f}" for c in cells))
    elif cmd == "box":
        v = list(map(int, sys.argv[3:]))
        for k in range(0, len(v), 4):
            x, y, w, h = v[k : k + 4]
            print(f"box {x},{y},{w},{h}: {stat(r[y : y + h, x : x + w])}")
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
