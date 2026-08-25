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
import os
import sys
import time

# Registry index -> app name (src/apps/registry.rs REGISTRY order == launch idx).
REGISTRY = [
    "Snake", "WorldSnake", "Game2048", "Tetris", "Flappy", "Maze",   # 0-5
    "Settings",                                                       # 6
    "Wled", "Hunt", "Energy", "Climate", "Voice", "Sound", "Theme",  # 7-13
    "Lights",                                                         # 14
    "Ping",                                                           # 15
]
THEME_IDX = REGISTRY.index("Theme")    # 13
LIGHTS_IDX = REGISTRY.index("Lights")  # 14
PING_IDX = REGISTRY.index("Ping")      # 15
# Lights hero button centre (lights.slint: cx = width/2, cy = 226px).
LIGHTS_HERO = (206, 226)
# Ping hero button centre (ping.slint: same proven hero geometry as Lights).
PING_HERO = (206, 226)

REPLY_PREFIX = "[DBGCON] "
PANEL = 412  # square AMOLED, logical px


# --------------------------------------------------------------------------- #
# Serial transport (pyserial, with a stdlib termios fallback)
# --------------------------------------------------------------------------- #
class _Port:
    """Line-oriented transport wrapper: serial (pyserial, or a raw termios
    tty when pyserial is missing) or TCP (`tcp://host:port` — the WiFi debug
    channel, same line protocol; see docs/debugging.md).

    TCP sessions send `auth <token>` as their first line when a token is
    given — the firmware debug server drops unauthenticated connections."""

    def __init__(self, dev: str, timeout: float, token: str | None = None):
        self.timeout = timeout
        self._buf = b""
        if dev.startswith("tcp://") or (":" in dev and not dev.startswith("/")):
            import socket

            hostport = dev[len("tcp://"):] if dev.startswith("tcp://") else dev
            host, _, port = hostport.partition(":")
            self._sock = socket.create_connection(
                (host, int(port or 5555)), timeout=max(timeout, 3.0))
            self._sock.settimeout(0.05)
            self._mode = "tcp"
            if token:
                self._sock.sendall(f"auth {token}\n".encode())
            return
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
        elif self._mode == "tcp":
            pass  # dropping buffered socket data would race the server
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
        elif self._mode == "tcp":
            self._sock.sendall(data)
        else:
            import os

            os.write(self._fd, data)

    def _read_some(self) -> bytes:
        if self._mode == "pyserial":
            return self._ser.read(256)
        if self._mode == "tcp":
            import socket

            try:
                data = self._sock.recv(256)
            except socket.timeout:
                return b""
            if data == b"":
                raise ConnectionError(
                    "debug link closed by the watch (auth reject / stop)")
            return data
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
        elif self._mode == "tcp":
            self._sock.close()
        else:
            import os

            os.close(self._fd)


# --------------------------------------------------------------------------- #
# Watch driver
# --------------------------------------------------------------------------- #
class Watch:
    """Drive + measure the watch UI over the debug console."""

    def __init__(self, dev: str = "/dev/ttyACM3", timeout: float = 2.0,
                 settle: float = 0.20, verbose: bool = False,
                 token: str | None = None):
        self.port = _Port(dev, timeout, token=token)
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

    def swipe(self, direction: str, start_y: int | None = None) -> str:
        """Inject a nav swipe. `start_y` drives the edge-zone gestures
        (#29/#32): >=427 = bottom edge (launcher), <=75 = top edge (shade);
        omitted = mid-screen 206 (firmware default)."""
        r = self.cmd(f"swipe {direction}" if start_y is None
                     else f"swipe {direction} {start_y}")
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
            if k == "app":
                out[k] = v
            elif k == "ip":
                out[k] = v  # dotted quad or "none" — never an int
            elif k == "story":
                # page/rows/loading/playing (firmware >= 2026-08-25); keep the
                # raw string too so old scripts that never knew it stay happy.
                parts = v.split("/")
                if len(parts) == 4:
                    out["story_page"], out["story_rows"] = int(parts[0]), int(parts[1])
                    out["story_loading"], out["story_playing"] = int(parts[2]), int(parts[3])
                out[k] = v
            else:
                out[k] = int(v)
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

    def capture(self, secs: float, needle: str = "[LAT]") -> list[str]:
        """Collect raw firmware log lines containing `needle` for `secs`.

        Does NOT reset the input buffer, so lines that raced ahead of the call
        are kept. Used for the [LAT] latency stamps (see `lights` mode)."""
        deadline = time.monotonic() + secs
        out: list[str] = []
        while True:
            raw = self.port.read_line(deadline)
            if raw is None:
                return out
            if self.verbose:
                print(f"  < {raw}")
            if needle in raw:
                out.append(raw)


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

    # 3. Paged launcher (v0.8.0+): swipe up/down FLIPS one section-page per
    #    swipe (AUDIO/GAMES/SYSTEM), not continuous scroll. A flip is a single
    #    full-frame repaint, so the bar is the render floor (~250ms per #53's
    #    202ms-worst-under-load), not the old 100ms scroll threshold.
    def page_flips():
        for _ in range(2):            # AUDIO -> GAMES -> SYSTEM
            w.cmd("swipe up")
            time.sleep(0.2)
        for _ in range(2):            # back to AUDIO
            w.cmd("swipe down")
            time.sleep(0.2)
    page_flips()
    p = w.perf()
    worst = p.get("max_us", 0)
    s.check("no frame >250ms during page flips", worst < 250_000,
            f"worst={worst/1000:.1f}ms frames={len(p['frames_us'])}")

    # 4. Bottom section reachable (paged): flip to the SYSTEM page, then the
    #    SYSTEM app Theme (idx 13) is reachable and the launcher stayed open
    #    through the flips (the v0.7.0 regression this guards: bottom apps
    #    unreachable). Reachability + launcher-stayed-open, no scroll-offset.
    w.home()
    w.swipe("up")                     # reopen launcher (AUDIO page)
    for _ in range(2):
        w.cmd("swipe up")             # flip to the SYSTEM page
        time.sleep(0.2)
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
def run_swallow(w: Watch) -> int:
    """#54: overlays must SWALLOW taps — the chrome beneath must never fire.

    Probe: the shell page-dots hit area (205, 470). A leaked tap advances
    `page` (mod 5) instantly and side-effect-free, which makes it the one
    deterministic chrome probe (the radio dots start slow radio state
    machines — unassertable within a settle window). For each overlay we
    tap the probe point and assert the shell page did NOT move and the
    overlay is still up.

    Shadowed spots (own control at the probe point — the launcher and the
    Settings hub put their OWN page dots there) are probed at the BLE radio
    dot (141, 40) instead, asserting the state.ble flag (it flips
    synchronously on a leaked toggle; WiFi association is async and is
    deliberately not used as a probe).

    Not coverable from the host: the app switcher (needs a real 500ms
    bottom-edge hold) and the power menu (hardware PMIC key) — both sealed
    by the same shared OverlaySwallow; verify on-glass.
    """
    s = _Suite()
    DOTS = (205, 470)   # shell page-dots hit area: leak => page advances
    BLE_DOT = (141, 40) # BLE radio dot: leak => state.ble flips (instant flag)

    def probe_dots(name: str, still_open) -> None:
        st0 = w.state()
        w.tap(*DOTS)
        st1 = w.state()
        ok = (st1.get("page") == st0.get("page")) and still_open(st1)
        s.check(f"{name}: dots tap swallowed", ok,
                f"page {st0.get('page')}->{st1.get('page')} "
                f"app={st1.get('app')} modal={st1.get('modal')}")

    def probe_ble(name: str, still_open) -> None:
        # For overlays whose OWN dots shadow the DOTS point. The BLE toggle
        # flips the state.ble flag synchronously, so a leak is observable
        # within the settle window (unlike WiFi association).
        st0 = w.state()
        w.tap(*BLE_DOT)
        st1 = w.state()
        ok = (st1.get("ble") == st0.get("ble")
              and st1.get("page") == st0.get("page")
              and still_open(st1))
        s.check(f"{name}: BLE-dot tap swallowed", ok,
                f"ble {st0.get('ble')}->{st1.get('ble')} app={st1.get('app')}")

    # Launcher (bottom-edge swipe-up, #29): its own dots sit exactly on the
    # shell dots, so probe the radio band (no launcher control lives there).
    w.home()
    w.swipe("up", 470)
    st = w.state()
    s.check("launcher opens (edge swipe)", st.get("app") == "Launcher", str(st))
    probe_ble("Launcher", lambda st: st.get("app") == "Launcher")
    w.swipe("right")  # close

    # Registry overlays: open by launch idx, probe, close by right-swipe
    # (the cell-close path, so the HA screens' WiFi holds are released).
    # Settings: its own dots shadow the DOTS point and its chevron owns the
    # top-left, so it takes the BLE probe (x141 clears the chevron at x14-92).
    for idx, name in [(6, "Settings"), (7, "Wled"), (8, "Hunt"), (9, "Energy"),
                      (10, "Climate"), (11, "Voice"), (12, "Sound"),
                      (13, "Theme"), (14, "Lights")]:
        w.home()
        w.launch(idx)
        st = w.state()
        if st.get("app") != name:
            s.check(f"{name}: opened", False, str(st))
            continue
        still = lambda st, n=name: st.get("app") == n
        if name == "Settings":
            probe_ble(name, still)
        else:
            probe_dots(name, still)
        w.swipe("right")

    # Notification shade (top-edge swipe-down, #32; state modal=2). With
    # notifications held, CLEAR ALL sits near the probe point — its tap is
    # the shade's OWN control, so the assertions still hold either way.
    w.home()
    w.swipe("down", 40)
    st = w.state()
    s.check("shade opens (edge swipe)", st.get("modal") == 2, str(st))
    probe_dots("Shade", lambda st: st.get("modal") == 2)
    w.swipe("up", 206)  # close (any Up while the shade is open)

    # Edge-zone honesty rides along (#29 acceptance): a MID-screen swipe-up
    # off the clock page must NOT open the launcher.
    w.home()
    for _ in range(5):
        if w.state().get("page") == 1:
            break
        w.swipe("left")
    w.swipe("up", 206)
    st = w.state()
    s.check("mid-screen up on page 1 stays put",
            st.get("app") == "Watchface" and st.get("page") == 1, str(st))
    w.home()
    return s.summary()


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


def run_hotpaths(w: Watch) -> int:
    """Frame-cost report for the hot interactions (launcher open, page flips,
    Lights/Theme overlay opens). Pure measurement — no PASS/FAIL gates; run it
    before/after a perf change and diff the numbers."""
    w.home()
    time.sleep(0.3)

    def measure(name: str, action, warmup: float = 0.30) -> None:
        w.cmd("perf")  # reset the observation window
        action()
        time.sleep(warmup)
        p = w.perf()
        frames = p.get("frames_us", [])
        if frames:
            worst = max(frames)
            avg = sum(frames) // len(frames)
            print(f"{name:<28} frames={len(frames):>2} "
                  f"worst={worst/1000:6.1f}ms avg={avg/1000:6.1f}ms")
        else:
            print(f"{name:<28} (no frames rendered)")

    measure("launcher open (swipe up)", lambda: w.swipe("up"))
    w.home(); time.sleep(0.2)
    measure("page flip left", lambda: w.swipe("left"))
    measure("page flip right", lambda: w.swipe("right"))

    def scroll():
        w.swipe("up"); time.sleep(0.1)
        for _ in range(3):
            w.cmd("swipe up"); time.sleep(0.12)
    measure("launcher scroll x3", scroll, warmup=0.5)
    w.home(); time.sleep(0.2)

    measure("Theme overlay open", lambda: w.launch(THEME_IDX))
    w.home(); time.sleep(0.2)
    measure("Lights overlay open", lambda: w.launch(LIGHTS_IDX))
    w.home(); time.sleep(0.2)
    return 0


def run_lights(w: Watch, wait_state: float = 20.0, wait_reply: float = 12.0) -> int:
    """End-to-end Lights latency: open the screen, wait for the first state
    frame, tap the hero, and report the [LAT] breakdown the firmware prints.

    Interpretation (all `t=` stamps are firmware-uptime ms):
      connect+handshake   TCP+CONNACK+SUBACK once WiFi/DHCP were ready
      open->first-state   the "Finding your room…" duration (0 if state warm)
      cmd queued->published  UI tick -> session task publish (firmware-side)
      published->state rx    broker + HA automation (incl. its settle delay)
      press->state-render    the full firmware-visible round trip
    """
    print(f"opening Lights (idx {LIGHTS_IDX})…")
    w.cmd(f"launch {LIGHTS_IDX}")
    for line in w.capture(wait_state):
        print(line)
    st = w.state()
    if st.get("app") != "Lights":
        print(f"Lights did not open: {st}")
        return 1
    x, y = LIGHTS_HERO
    print(f"tapping hero at ({x},{y})…")
    w.port.write_line(f"tap {x} {y}")   # raw write: cmd() would drop [LAT] races
    for line in w.capture(wait_reply):
        print(line)
    w.home()
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="esp32c6-watch UI test automator (host driver)")
    ap.add_argument("--port", default="/dev/ttyACM3",
                    help="serial device (default /dev/ttyACM3) or tcp://host:port "
                         "(the WiFi debug channel; prefer `watchctl console/test` "
                         "which resolves ports by sigil)")
    ap.add_argument("--token", default=os.environ.get("WATCH_DEBUG_TOKEN"),
                    help="WiFi debug shared secret (env WATCH_DEBUG_TOKEN); "
                         "sent as `auth <token>` on TCP connect")
    ap.add_argument("--timeout", type=float, default=2.0, help="per-command reply timeout (s)")
    ap.add_argument("--settle", type=float, default=0.20, help="UI settle delay after input (s)")
    ap.add_argument("-v", "--verbose", action="store_true", help="echo every serial line read")
    ap.add_argument("mode", nargs="?", default="suite",
                    choices=["suite", "repl", "cmd", "hotpaths", "lights", "swallow"],
                    help="what to run (default: suite)")
    ap.add_argument("arg", nargs="?", help="command string when mode=cmd")
    args = ap.parse_args()

    try:
        w = Watch(args.port, timeout=args.timeout, settle=args.settle,
                  verbose=args.verbose, token=args.token)
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
        if args.mode == "hotpaths":
            return run_hotpaths(w)
        if args.mode == "lights":
            return run_lights(w)
        if args.mode == "swallow":
            return run_swallow(w)
        return run_suite(w)
    except ConnectionError as e:
        # TCP transport: the debug server dropped us (bad/missing token,
        # server stop) — report cleanly instead of a traceback.
        print(f"debug link: {e}", file=sys.stderr)
        return 2
    finally:
        w.close()


if __name__ == "__main__":
    raise SystemExit(main())
