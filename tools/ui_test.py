#!/usr/bin/env python3
"""ui_test.py — host-side driver for the esp32c6-watch UI test automator.

This talks to the firmware's `debug-console` (feature `debug-console`, ON by
default) over the USB-Serial-JTAG. It lets an AGENT drive the watch UI and
measure render responsiveness WITHOUT a human touching the glass — replacing the
manual flash-and-glass-test loop.

HOW AN AGENT RUNS IT
--------------------
1. Build + flash a debug build (the device owner flashes; the feature is on by
   default so a normal build includes the console):
       fambuild build --release --bin esp32c6-watch     # build on familiar
       espflash flash --chip esp32c6 --port /dev/ttyACM3 --monitor <elf>
   On boot the firmware prints `[DBGCON] ready`.

2. Run the assertion suite (default mode):
       python3 tools/ui_test.py --port /dev/ttyACM3
   Prints PASS/FAIL per check and exits non-zero if any check fails.

3. Drive it manually (REPL) or one-shot a command:
       python3 tools/ui_test.py --port /dev/ttyACM3 repl
       python3 tools/ui_test.py --port /dev/ttyACM3 cmd "launch 13"

4. Use it as a library from another script/agent:
       from ui_test import Watch
       w = Watch("/dev/ttyACM3")
       w.launch(13)                 # raise the Theme overlay (registry idx 13)
       print(w.state())             # {'app': 'Theme', 'page': 0, ...}
       print(w.perf()["max_us"])    # worst render frame in the last 32

COMMAND SET (mirrors src/debug_console.rs)
------------------------------------------
    tap <x> <y>                 synthesise a click at (x, y)   [412x412 panel]
    swipe up|down|left|right    a navigation swipe
    launch <idx>                raise the app at registry index <idx>
    home                        return to the watchface
    state                       AppState + key UI flags
    perf                        last-N render-frame durations (microseconds)
    ping / help                 liveness / usage

Every reply is one line prefixed `[DBGCON] ` so parsing is deterministic; all
other firmware logs are ignored.

REQUIRES: pyserial (`pip install pyserial`). Falls back to a raw termios tty if
pyserial is missing.
"""

from __future__ import annotations

import argparse
import sys
import time

# Registry index -> app name (src/apps/registry.rs REGISTRY order == launch idx).
REGISTRY = [
    "Snake", "WorldSnake", "Game2048", "Tetris", "Flappy", "Maze",   # 0-5
    "Settings",                                                       # 6
    "Wled", "Hunt", "Energy", "Climate", "Voice", "Sound", "Theme",  # 7-13
]
THEME_IDX = REGISTRY.index("Theme")  # 13

REPLY_PREFIX = "[DBGCON] "
PANEL = 412  # square AMOLED, logical px


# --------------------------------------------------------------------------- #
# Serial transport (pyserial, with a stdlib termios fallback)
# --------------------------------------------------------------------------- #
class _Port:
    """Line-oriented serial wrapper. Prefers pyserial; falls back to raw tty."""

    def __init__(self, dev: str, timeout: float):
        self.timeout = timeout
        self._buf = b""
        try:
            import serial  # type: ignore

            # USB-CDC-ACM ignores baud; 115200 is a harmless conventional value.
            self._ser = serial.Serial(dev, 115200, timeout=timeout)
            self._mode = "pyserial"
        except ImportError:
            import os
            import termios
            import tty

            fd = os.open(dev, os.O_RDWR | os.O_NOCTTY)
            tty.setraw(fd)
            # Non-blocking-ish reads via VMIN=0/VTIME; we poll with select.
            attrs = termios.tcgetattr(fd)
            attrs[6][termios.VMIN] = 0
            attrs[6][termios.VTIME] = 0
            termios.tcsetattr(fd, termios.TCSANOW, attrs)
            self._fd = fd
            self._mode = "termios"

    def reset_input(self) -> None:
        self._buf = b""
        if self._mode == "pyserial":
            self._ser.reset_input_buffer()
        else:
            import os

            try:
                while os.read(self._fd, 4096):
                    pass
            except BlockingIOError:
                pass

    def write_line(self, s: str) -> None:
        data = (s + "\n").encode("ascii", "replace")
        if self._mode == "pyserial":
            self._ser.write(data)
            self._ser.flush()
        else:
            import os

            os.write(self._fd, data)

    def _read_some(self) -> bytes:
        if self._mode == "pyserial":
            return self._ser.read(256)
        import os
        import select

        r, _, _ = select.select([self._fd], [], [], 0.05)
        if r:
            try:
                return os.read(self._fd, 256)
            except BlockingIOError:
                return b""
        return b""

    def read_line(self, deadline: float) -> str | None:
        """Return one decoded line (without newline), or None on timeout."""
        while True:
            nl = self._buf.find(b"\n")
            if nl >= 0:
                line = self._buf[:nl]
                self._buf = self._buf[nl + 1:]
                return line.rstrip(b"\r").decode("utf-8", "replace")
            if time.monotonic() > deadline:
                return None
            self._buf += self._read_some()

    def close(self) -> None:
        if self._mode == "pyserial":
            self._ser.close()
        else:
            import os

            os.close(self._fd)


# --------------------------------------------------------------------------- #
# Watch driver
# --------------------------------------------------------------------------- #
class Watch:
    """Drive + measure the watch UI over the debug console."""

    def __init__(self, dev: str = "/dev/ttyACM3", timeout: float = 2.0,
                 settle: float = 0.20, verbose: bool = False):
        self.port = _Port(dev, timeout)
        self.timeout = timeout
        self.settle = settle          # UI settle time after an input command
        self.verbose = verbose

    def close(self) -> None:
        self.port.close()

    def __enter__(self) -> "Watch":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # -- core round-trip ---------------------------------------------------- #
    def cmd(self, line: str) -> str:
        """Send one command, return its `[DBGCON] ...` reply (prefix stripped)."""
        self.port.reset_input()
        self.port.write_line(line)
        deadline = time.monotonic() + self.timeout
        while True:
            raw = self.port.read_line(deadline)
            if raw is None:
                raise TimeoutError(f"no reply to {line!r} within {self.timeout}s")
            if self.verbose:
                print(f"  < {raw}")
            if raw.startswith(REPLY_PREFIX):
                return raw[len(REPLY_PREFIX):]
            # else: an unrelated firmware log line — skip it.

    # -- input helpers (settle so a following state() sees the effect) ------ #
    def tap(self, x: int, y: int) -> str:
        r = self.cmd(f"tap {x} {y}")
        time.sleep(self.settle)
        return r

    def swipe(self, direction: str) -> str:
        r = self.cmd(f"swipe {direction}")
        time.sleep(self.settle)
        return r

    def launch(self, idx: int) -> str:
        r = self.cmd(f"launch {idx}")
        time.sleep(self.settle)
        return r

    def home(self) -> str:
        r = self.cmd("home")
        time.sleep(self.settle)
        return r

    def ping(self) -> str:
        return self.cmd("ping")

    # -- readback helpers --------------------------------------------------- #
    def state(self) -> dict:
        """Parse `state app=.. page=.. launcher=.. screen=.. wifi=.. ble=.. mesh=..`."""
        reply = self.cmd("state")
        fields = reply.split()
        if not fields or fields[0] != "state":
            raise ValueError(f"unexpected state reply: {reply!r}")
        out: dict = {}
        for f in fields[1:]:
            if "=" not in f:
                continue
            k, v = f.split("=", 1)
            out[k] = v if k == "app" else int(v)
        return out

    def perf(self) -> dict:
        """Parse `perf count=.. n=.. frames_us=[..] max_us=.. avg_us=..`."""
        reply = self.cmd("perf")
        if not reply.startswith("perf"):
            raise ValueError(f"unexpected perf reply: {reply!r}")
        out: dict = {"frames_us": []}
        lb, rb = reply.find("["), reply.find("]")
        if lb >= 0 and rb > lb:
            inner = reply[lb + 1:rb].strip()
            if inner:
                out["frames_us"] = [int(x) for x in inner.split(",")]
            reply = reply[:lb] + reply[rb + 1:]
        for f in reply.split():
            if "=" in f:
                k, v = f.split("=", 1)
                if k in ("count", "n", "max_us", "avg_us"):
                    out[k] = int(v)
        return out

    def max_frame_us_during(self, action, warmup: float = 0.05) -> int:
        """Run `action()`, let frames flow, then return the worst recent frame."""
        self.cmd("perf")            # (drains/echoes; ring keeps rolling)
        action()
        time.sleep(warmup)
        return self.perf().get("max_us", 0)


# --------------------------------------------------------------------------- #
# Assertion suite
# --------------------------------------------------------------------------- #
class _Suite:
    def __init__(self):
        self.passed = 0
        self.failed = 0

    def check(self, name: str, ok: bool, detail: str = "") -> None:
        tag = "PASS" if ok else "FAIL"
        line = f"[{tag}] {name}"
        if detail:
            line += f"  ({detail})"
        print(line)
        if ok:
            self.passed += 1
        else:
            self.failed += 1

    def summary(self) -> int:
        total = self.passed + self.failed
        print(f"\n{self.passed}/{total} checks passed"
              + ("" if self.failed == 0 else f", {self.failed} FAILED"))
        return 0 if self.failed == 0 else 1


def run_suite(w: Watch) -> int:
    s = _Suite()

    # 0. Liveness.
    try:
        s.check("console alive (ping)", w.ping() == "ok pong", w.ping())
    except Exception as e:  # noqa: BLE001
        s.check("console alive (ping)", False, repr(e))
        return s.summary()

    # 1. Navigation reflects in state: home -> watchface.
    w.home()
    st = w.state()
    s.check("home -> Watchface", st.get("app") == "Watchface", str(st))

    # 2. Open the launcher (swipe up on the clock page).
    w.swipe("up")
    st = w.state()
    s.check("swipe up opens launcher",
            st.get("app") == "Launcher" and st.get("launcher") == 1, str(st))

    # 3. No frame >100ms during a launcher scroll (the launcher-scroll bug class).
    def scroll():
        for _ in range(4):
            w.cmd("swipe up")
            time.sleep(0.12)
        for _ in range(4):
            w.cmd("swipe down")
            time.sleep(0.12)
    scroll()
    p = w.perf()
    worst = p.get("max_us", 0)
    s.check("no frame >100ms during scroll", worst < 100_000,
            f"worst={worst/1000:.1f}ms frames={len(p['frames_us'])}")

    # 4. Launcher scrolls to the bottom row (proxy): after scrolling, the
    #    bottom-most SYSTEM app (Theme, idx 13) is still reachable and the
    #    launcher stayed open through the scroll (v0.7.0 regression: bottom rows
    #    unreachable). True scroll-offset readout isn't exposed by firmware, so
    #    this asserts reachability + that the scroll didn't collapse the list.
    w.home()
    w.swipe("up")                     # reopen launcher
    for _ in range(4):
        w.cmd("swipe up")             # scroll toward the bottom rows
        time.sleep(0.12)
    st = w.state()
    still_open = st.get("app") == "Launcher"
    w.launch(THEME_IDX)
    st = w.state()
    s.check("launcher bottom row reachable (launch Theme)",
            still_open and st.get("app") == "Theme",
            f"open_after_scroll={still_open} then={st.get('app')}")

    # 5. Theme opens <200ms (the theme-slow-to-load bug class). Measure the
    #    render frame around raising the Theme overlay.
    w.home()

    def open_theme():
        w.cmd(f"launch {THEME_IDX}")
    worst = w.max_frame_us_during(open_theme, warmup=0.25)
    st = w.state()
    opened = st.get("app") == "Theme"
    s.check("Theme opens <200ms", opened and 0 < worst < 200_000,
            f"opened={opened} worst_frame={worst/1000:.1f}ms")

    # 6. Return home cleanly.
    w.home()
    st = w.state()
    s.check("home from Theme -> Watchface", st.get("app") == "Watchface", str(st))

    return s.summary()


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #
def repl(w: Watch) -> int:
    print("debug-console REPL — type a command (tap/swipe/launch/home/state/perf), "
          "or 'quit'.")
    while True:
        try:
            line = input("dbgcon> ").strip()
        except (EOFError, KeyboardInterrupt):
            print()
            return 0
        if line in ("quit", "exit"):
            return 0
        if not line:
            continue
        try:
            print(w.cmd(line))
        except Exception as e:  # noqa: BLE001
            print(f"error: {e}")


def main() -> int:
    ap = argparse.ArgumentParser(description="esp32c6-watch UI test automator (host driver)")
    ap.add_argument("--port", default="/dev/ttyACM3", help="serial device (default /dev/ttyACM3)")
    ap.add_argument("--timeout", type=float, default=2.0, help="per-command reply timeout (s)")
    ap.add_argument("--settle", type=float, default=0.20, help="UI settle delay after input (s)")
    ap.add_argument("-v", "--verbose", action="store_true", help="echo every serial line read")
    ap.add_argument("mode", nargs="?", default="suite",
                    choices=["suite", "repl", "cmd"], help="what to run (default: suite)")
    ap.add_argument("arg", nargs="?", help="command string when mode=cmd")
    args = ap.parse_args()

    try:
        w = Watch(args.port, timeout=args.timeout, settle=args.settle, verbose=args.verbose)
    except Exception as e:  # noqa: BLE001
        print(f"could not open {args.port}: {e}", file=sys.stderr)
        return 2

    try:
        if args.mode == "repl":
            return repl(w)
        if args.mode == "cmd":
            if not args.arg:
                print("mode 'cmd' needs a command string", file=sys.stderr)
                return 2
            print(w.cmd(args.arg))
            return 0
        return run_suite(w)
    finally:
        w.close()


if __name__ == "__main__":
    raise SystemExit(main())
