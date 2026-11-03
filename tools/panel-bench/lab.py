"""experiment helpers: drive the bench daemon, photograph, measure.

import from an experiment script run with
`uv run --with numpy --with opencv-python-headless python -I exp_x.py`.
"""

import json
import os
import socket
import subprocess
import sys
import time

import cv2
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
SOCK = os.path.join(HERE, "target", "bench.sock")
CAL = os.path.join(HERE, "target", "cal.json")
SHOTS = os.path.join(ROOT, "target", "camshot", "lab")
# camera name substring for camshot -d; the Continuity camera pings the
# phone on every capture, so the built-in webcam is the quiet default
CAMERA = os.environ.get("BENCH_CAMERA", "FaceTime")
# camshot -c box around the device in the camera frame (setup-specific)
CROP = os.environ.get("BENCH_CROP", "460x720+535+705")


def _cam(path: str, *extra: str) -> list[str]:
    a = [os.path.join(ROOT, "camshot.sh"), "-d", CAMERA, "-o", path, *extra]
    return a + (["-c", CROP] if CROP else [])
W, H = 480, 800
BEZEL_REF = 200.0

_conn = None


def cmd(*lines: str, quiet: bool = True) -> list[str]:
    global _conn
    if _conn is None:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(SOCK)
        _conn = s.makefile("rw")
    out = []
    for line in lines:
        _conn.write(line + "\n")
        _conn.flush()
        while True:
            r = _conn.readline().rstrip("\n")
            if r == "ok":
                break
            if r.startswith("err") or r == "":
                raise RuntimeError(f"{line}: {r}")
            out.append(r)
        if not quiet:
            print(f"> {line[:70]}", *out[-3:], sep="\n  ")
    return out


def kv(lines: list[str]) -> dict:
    d = {}
    for l in lines:
        for tok in l.split():
            if "=" in tok:
                k, v = tok.split("=", 1)
                d[k] = v
    return d


# ── controller ops ──────────────────────────────────────────────────

def hexs(bs) -> str:
    return " ".join(f"{b:02X}" for b in bs)


def init():
    """production init_display, after a hardware reset"""
    cmd("rst", "c 12", "ms 10", "c 18 80", "c 0C AE C7 C3 C0 40", "c 01 DF 01 02",
        "c 3C 01", "c 11 01")


def plane(p: int, spec: str):
    cmd(f"pat {p:02X} {spec}")


def both(spec: str):
    plane(0x24, spec)
    plane(0x26, spec)


def act(c2: int) -> dict:
    return kv(cmd(f"act {c2:02X}"))


def ctrl1(*bs: int):
    cmd(f"c 21 {hexs(bs)}")


def clean_clear(spec: str = "grid 1 2 01"):
    """production FullKind::Clean over a frame (default: top half black)"""
    both(spec)
    ctrl1(0x40, 0x00)
    return act(0xF4)


def read(c: int, n: int) -> list[int]:
    r = cmd(f"rd {c:02X} {n}")
    return [int(x, 16) for x in r[0].split()[1:]]


# ── custom LUTs ─────────────────────────────────────────────────────
#
# 0x32 takes 105 bytes on this driver's reading of the datasheet: five
# VS rows of 10 groups (one byte per group, four 2-bit phases A..D),
# then 10 groups x (TP A, TP B, TP C, TP D, RP), then 5 frame-rate
# bytes. voltages go through 0x03 / 0x04 / 0x2C.

PROD_VOLT = dict(vgh=0x17, vsh1=0x41, vsh2=0xA8, vsl=0x32, vcom=0x30)


def lut(rows: dict, groups=((1, 1, 1, 1, 0),) * 3, fr=0x8F, vcom_row=None) -> list[int]:
    """rows: {row_index: [vs byte per group]}"""
    b = [0] * 105
    for r, vs in rows.items():
        for g, v in enumerate(vs):
            b[r * 10 + g] = v
    if vcom_row:
        for g, v in enumerate(vcom_row):
            b[40 + g] = v
    for g, tp in enumerate(groups):
        b[50 + g * 5 : 50 + g * 5 + 5] = tp
    b[100:105] = [fr] * 5
    return b


def load_lut(b: list[int], volt: dict | None = PROD_VOLT):
    cmd(f"c 32 {hexs(b)}")
    if volt:
        cmd(f"c 03 {volt['vgh']:02X}",
            f"c 04 {volt['vsh1']:02X} {volt['vsh2']:02X} {volt['vsl']:02X}",
            f"c 2C {volt['vcom']:02X}")


# ── camera ──────────────────────────────────────────────────────────

def shoot(name: str, warm: int = 30) -> np.ndarray:
    os.makedirs(SHOTS, exist_ok=True)
    path = os.path.join(SHOTS, f"{name}.jpg")
    subprocess.run(_cam(path, "-w", str(warm)),
                   check=True, capture_output=True)
    img = cv2.imread(path)
    g = cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)
    with open(CAL) as f:
        m = np.array(json.load(f)["h"])
    # warp with a 40 px margin so the bezel is in frame: the plastic is
    # never driven, so its brightness is the per-shot exposure reference
    t = np.array([[1, 0, 40], [0, 1, 40], [0, 0, 1]], float)
    big = cv2.warpPerspective(g, t @ m, (W + 80, H + 80), flags=cv2.INTER_AREA).astype(float)
    bezel = np.concatenate([big[60:840, 8:30].ravel(), big[60:840, 530:552].ravel(),
                            big[848:872, 60:500].ravel()])
    gain = BEZEL_REF / np.median(bezel)
    r = np.clip(big[40:40 + H, 40:40 + W] * gain, 0, 255)
    cv2.imwrite(os.path.join(SHOTS, f"{name}-rect.png"), r.astype(np.uint8))
    return r


def cells(r: np.ndarray, nx: int, ny: int, pad: int = 10) -> np.ndarray:
    out = np.zeros((ny, nx))
    for j in range(ny):
        for i in range(nx):
            x0, x1 = i * W // nx, (i + 1) * W // nx
            y0, y1 = j * H // ny, (j + 1) * H // ny
            out[j, i] = r[y0 + pad : y1 - pad, x0 + pad : x1 - pad].mean()
    return out


def fmt(a: np.ndarray) -> str:
    return "\n".join("  " + " ".join(f"{v:6.1f}" for v in row) for row in np.atleast_2d(a))


def log(*a):
    print(*a, flush=True)


CORNERS = "grid 12 20 " + "".join(
    "0" if (i % 12, i // 12) in {(0, 0), (11, 0), (0, 19), (11, 19)} else "1" for i in range(240))


def calibrate():
    """corner page via a clean clear, then fit the homography; the phone
    reframes between sessions so every experiment starts here"""
    clean_clear(CORNERS)
    path = os.path.join(SHOTS, "cal.jpg")
    os.makedirs(SHOTS, exist_ok=True)
    subprocess.run(_cam(path), check=True, capture_output=True)
    subprocess.run([os.path.join(HERE, "measure.py"), "calibrate", path], check=True, capture_output=True)
    r = shoot("cal-check")
    c = cells(r, 12, 20, pad=8)
    log(f"calibrated: corners {c[0,0]:.0f} {c[0,11]:.0f} {c[19,0]:.0f} {c[19,11]:.0f} vs page {np.median(c):.0f}")
