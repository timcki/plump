#!/usr/bin/env -S uv run --script
# /// script
# dependencies = ["pyserial"]
# ///
"""send bench commands to the X4 running the panel-bench firmware.

usage: bench.py serve [--port P]           hold the port (opening it resets
                                          the chip, so state only survives
                                          behind one long-lived owner)
       bench.py LINE [LINE ...]
       bench.py -f SCRIPT                 (one command per line, # comments)

each LINE is one protocol command (see src/main.rs); replies are printed
as they arrive. exits non-zero on the first `err`.
"""

import argparse
import glob
import sys
import time

import os
import socket

import serial

SOCK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "bench.sock")


def open_port(port: str) -> serial.Serial:
    s = serial.Serial()
    s.port = port
    s.baudrate = 115200
    s.timeout = 0.005
    # usb-serial-jtag resets the chip on a dtr/rts edge; keep both idle
    s.dtr = False
    s.rts = False
    s.open()
    return s


def run(s: serial.Serial, line: str, timeout: float = 15.0) -> list[str]:
    s.write((line + "\n").encode())
    s.flush()
    out: list[str] = []
    buf = b""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        buf += s.read(4096)
        while b"\n" in buf:
            raw, buf = buf.split(b"\n", 1)
            text = raw.decode(errors="replace").strip()
            if not text or text.startswith("bench: ready"):
                continue
            if text == "ok":
                return out
            if text.startswith("err"):
                raise RuntimeError(f"{line!r}: {text}")
            out.append(text)
    raise TimeoutError(f"{line!r}: no reply")


def serve(port: str) -> int:
    s = open_port(port)
    time.sleep(1.0)
    s.reset_input_buffer()
    os.makedirs(os.path.dirname(SOCK), exist_ok=True)
    if os.path.exists(SOCK):
        os.unlink(SOCK)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(SOCK)
    srv.listen(4)
    print(f"bench: serving {port} on {SOCK}", flush=True)
    while True:
        conn, _ = srv.accept()
        # a client that dies mid-command must not take the port owner with it
        try:
            with conn, conn.makefile("rw") as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    print(f"> {line[:60]}", flush=True)
                    try:
                        reply = run(s, line)
                        f.write("\n".join(reply + ["ok"]) + "\n")
                    except (RuntimeError, TimeoutError) as e:
                        print(f"  err {e}", flush=True)
                        f.write(f"err {e}\n")
                    f.flush()
        except (BrokenPipeError, ConnectionResetError):
            print("bench: client went away", flush=True)


def client(lines: list[str]) -> int:
    c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    c.connect(SOCK)
    f = c.makefile("rw")
    for line in lines:
        t0 = time.monotonic()
        f.write(line + "\n")
        f.flush()
        reply = []
        while True:
            r = f.readline()
            if not r:
                print("bench: daemon closed", file=sys.stderr)
                return 3
            r = r.rstrip("\n")
            if r == "ok":
                break
            if r.startswith("err"):
                print(f"bench: {r}", file=sys.stderr)
                return 2
            reply.append(r)
        dt = (time.monotonic() - t0) * 1000
        print(f"> {line[:80]}{'...' if len(line) > 80 else ''}  [{dt:.0f} ms]")
        for r in reply:
            print(f"  {r}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default=None)
    ap.add_argument("-f", "--file")
    ap.add_argument("lines", nargs="*")
    a = ap.parse_args()
    port = a.port or (sorted(glob.glob("/dev/cu.usbmodem*")) or [None])[0]
    if not port:
        print("bench: no usbmodem port", file=sys.stderr)
        return 1
    lines = list(a.lines)
    if a.file:
        with open(a.file) as f:
            lines += [l.split("#", 1)[0].strip() for l in f]
    lines = [l for l in lines if l]
    if lines == ["serve"]:
        return serve(port)
    if os.path.exists(SOCK):
        return client(lines)
    s = open_port(port)
    try:
        s.reset_input_buffer()
        for line in lines:
            t0 = time.monotonic()
            reply = run(s, line)
            dt = (time.monotonic() - t0) * 1000
            print(f"> {line[:80]}{'...' if len(line) > 80 else ''}  [{dt:.0f} ms]")
            for r in reply:
                print(f"  {r}")
    except (RuntimeError, TimeoutError) as e:
        print(f"bench: {e}", file=sys.stderr)
        return 2
    finally:
        s.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
