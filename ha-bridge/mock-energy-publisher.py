#!/usr/bin/env python3
"""Bench mock for the watch Energy screen — fakes the home battery/solar/grid feed
so the Energy screen can be validated on-glass WITHOUT touching JP's live HA.

Only ever speaks MQTT on `watch/energy/#` (never calls HA). It publishes a
RETAINED `watch/energy/state` with drifting values (a solar day/night sweep,
battery charge/discharge, grid flipping import↔export) so the screen shows live
movement and every colour branch is exercised, plus a RETAINED
`watch/energy/avail` presence flag (online, with an `offline` LWT).

Wire contract (from luna's energy contract + the real `energy-bridge.flow.json`,
which aggregates HA sensors into these exact keys):
  watch/energy/state  (retained) = {"batt":78,"solar":3400,"grid":-1200,"chg":true}
      batt  : home battery %, 0..100        (int)
      solar : solar production, W, >= 0      (int)
      grid  : grid power, W, SIGNED — >0 IMPORT (buying), <0 EXPORT (selling)
      chg   : home battery charging          (bool)
  watch/energy/avail  (retained) = "online" | "offline"   (offline = LWT)

Screen branches this drives (ui/slint/energy.slint):
  grid >  +50  → IMPORT (warn/red)   ·  grid < -50 → EXPORT (accent/teal)  ·  else IDLE (dim)
  charging     → batt teal           ·  batt < 20  → batt red              ·  else soft
  solar        → gold/teal magnitude

Broker + creds from the environment (never hardcoded):
  MQTT_HOST (default 10.0.6.11)  MQTT_PORT (default 1883)
  MQTT_USER (default jp)         MQTT_PASS (required for live; e.g. `bw get password mosquitto`)

Usage:
  export MQTT_PASS=...
  python3 mock-energy-publisher.py            # live: retained state + avail=online + drift
  python3 mock-energy-publisher.py --once     # publish one state + online, then exit
  python3 mock-energy-publisher.py --clear    # wipe the retained energy topics, then exit
  python3 mock-energy-publisher.py --dry-run  # print a few drifted frames; no broker, no paho
"""

import argparse
import json
import math
import os
import random
import signal
import sys
import time
from datetime import datetime

STATE_TOPIC = "watch/energy/state"
AVAIL_TOPIC = "watch/energy/avail"

# --- simulation constants ---
SOLAR_PEAK = 5000      # W at midday
DAY_PERIOD_S = 120.0   # one full sine cycle (≈60s daytime + 60s "night") — fast for a bench
HOUSE_BASE = 900       # W baseline house load
HOUSE_NOISE = 300      # W ± noise on the load
CHARGE_MAX = 2500      # W the home battery will absorb
# Deliberately BELOW the night house-load so the battery can't fully cover it →
# there's always residual grid IMPORT at night/dawn/dusk (exercises the red
# IMPORT branch), while midday surplus drives EXPORT. Both signs every cycle.
DISCHARGE_MAX = 400    # W the home battery will supply


class Sim:
    """Rolling home-energy state. `t` is seconds since start (drives the solar sweep)."""
    def __init__(self):
        self.t = 0.0
        self.batt = 18.0          # start low → exercises the <20 red branch, then charges up
        self.solar = 0
        self.grid = 0
        self.chg = False
        self._recompute()

    def _solar_now(self):
        # Half-sine day: negative half of the sine is clamped to 0 (night).
        return max(0.0, SOLAR_PEAK * math.sin(2 * math.pi * self.t / DAY_PERIOD_S))

    def step(self, dt):
        self.t += dt
        self._recompute()

    def _recompute(self):
        solar = self._solar_now()
        load = HOUSE_BASE + random.uniform(-HOUSE_NOISE, HOUSE_NOISE)
        surplus = solar - load
        if surplus > 0:
            # Charge (until full), export the remainder.
            charge = 0.0 if self.batt >= 100 else min(surplus, CHARGE_MAX)
            self.chg = charge > 0
            self.batt = min(100.0, self.batt + charge / 900.0)   # ~%/tick
            export = surplus - charge
            self.grid = -export                                   # <0 = exporting
        else:
            # Discharge (if any) to cover the deficit, import the rest.
            deficit = -surplus
            discharge = 0.0 if self.batt <= 0 else min(deficit, DISCHARGE_MAX)
            self.chg = False
            self.batt = max(0.0, self.batt - discharge / 900.0)
            imp = deficit - discharge
            self.grid = imp                                       # >0 = importing
        self.solar = solar

    def state(self):
        return {
            "batt": int(round(self.batt)),
            "solar": int(round(self.solar)),
            "grid": int(round(self.grid)),
            "chg": bool(self.chg),
        }


def log(msg):
    print(f"{datetime.now().strftime('%H:%M:%S')}  {msg}", flush=True)


def label(st):
    """Human tag for the log so you can eyeball which colour branch is active."""
    g = st["grid"]
    gtag = "IMPORT" if g > 50 else ("EXPORT" if g < -50 else "IDLE")
    btag = "chg" if st["chg"] else ("LOW" if st["batt"] < 20 else "dischg")
    return f"[{gtag} {btag}]"


# ------------------------------------------------------------------ dry-run ---

def do_dry_run(interval):
    log("DRY RUN — a few drifted frames that WOULD be published (no broker, no paho):")
    print(f"  RETAIN {AVAIL_TOPIC}  online")
    sim = Sim()
    for _ in range(8):
        st = sim.state()
        print(f"  RETAIN {STATE_TOPIC}  {json.dumps(st)}   {label(st)}")
        sim.step(interval if interval else 6.0)
    print(f"  (LWT / clean-exit → RETAIN {AVAIL_TOPIC}  offline)")


# --------------------------------------------------------------------- live ---

def make_client():
    import paho.mqtt.client as mqtt  # lazy: dry-run needs no paho
    try:
        c = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="mock-energy-bench")
    except (AttributeError, TypeError):
        c = mqtt.Client(client_id="mock-energy-bench")  # paho 1.x
    return c


def run_live(args):
    host = os.environ.get("MQTT_HOST", "10.0.6.11")
    port = int(os.environ.get("MQTT_PORT", "1883"))
    user = os.environ.get("MQTT_USER", "jp")
    passwd = os.environ.get("MQTT_PASS")
    if passwd is None and not args.allow_anonymous:
        log("ERROR: MQTT_PASS not set. `export MQTT_PASS=$(bw get password mosquitto)` "
            "or pass --allow-anonymous.")
        return 2

    client = make_client()
    if passwd is not None:
        client.username_pw_set(user, passwd)
    # Last Will: if we drop uncleanly, the broker marks the source offline.
    client.will_set(AVAIL_TOPIC, "offline", qos=0, retain=True)

    sim = Sim()

    def on_connect(c, userdata, flags, rc, *_):
        if str(rc) not in ("0", "Success"):
            log(f"connect failed: rc={rc}")
            return
        log(f"connected to {host}:{port} as {user!r}")
        if args.clear:
            c.publish(STATE_TOPIC, payload=None, qos=0, retain=True)
            c.publish(AVAIL_TOPIC, payload=None, qos=0, retain=True)
            log("cleared retained energy topics — exiting")
            c.disconnect()
            return
        c.publish(AVAIL_TOPIC, "online", qos=0, retain=True)
        st = sim.state()
        c.publish(STATE_TOPIC, json.dumps(st), qos=0, retain=True)
        log(f"online + seeded state {json.dumps(st)} {label(st)}")

    client.on_connect = on_connect

    stopping = {"v": False}

    def stop(*_):
        stopping["v"] = True
        log("shutting down → avail offline (retained state left in place; --clear to wipe)")
        try:
            client.publish(AVAIL_TOPIC, "offline", qos=0, retain=True)
            time.sleep(0.2)
            client.disconnect()
        except Exception:
            pass

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)

    client.connect(host, port, keepalive=30)

    if args.once or args.clear:
        client.loop_forever()      # on_connect publishes once / clears, then disconnects
        return 0

    client.loop_start()
    try:
        while not stopping["v"]:
            time.sleep(args.interval)
            sim.step(args.interval)
            st = sim.state()
            client.publish(STATE_TOPIC, json.dumps(st), qos=0, retain=True)
            log(f"{json.dumps(st)} {label(st)}")
        client.loop_stop()
    except Exception as e:
        log(f"loop error: {e}")
        return 1
    return 0


def main():
    ap = argparse.ArgumentParser(description="Bench mock energy publisher for the watch Energy screen.")
    ap.add_argument("--interval", type=float, default=3.0, help="seconds between state pushes (default 3)")
    ap.add_argument("--once", action="store_true", help="publish one state + online, then exit")
    ap.add_argument("--clear", action="store_true", help="wipe the retained energy topics, then exit")
    ap.add_argument("--dry-run", action="store_true", help="print sample frames (no broker, no paho)")
    ap.add_argument("--allow-anonymous", action="store_true", help="connect without MQTT_PASS")
    args = ap.parse_args()

    if args.dry_run:
        do_dry_run(args.interval)
        return 0
    return run_live(args)


if __name__ == "__main__":
    sys.exit(main())
