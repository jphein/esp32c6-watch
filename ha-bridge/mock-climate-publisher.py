#!/usr/bin/env python3
"""Bench mock for the watch Climate feature — fakes the HA→watch MQTT side so the
Climate screen can be validated on-glass WITHOUT touching JP's live HA/Node-RED.

It ONLY speaks MQTT on the `watch/climate/#` topics (never calls HA or Node-RED):
  * publishes RETAINED `watch/climate/<id>/state` for 3 fake devices + a retained
    `watch/climate/roster`, and drifts cur/action over time so the screen moves;
  * subscribes `watch/climate/+/set`, LOGS + validates every command the watch
    sends, and (unless --no-echo) applies it back into the device's /state so the
    watch's optimistic-update -> reconcile loop is exercised.

Wire contract (from the firmware `crates/climate-model` / `climate_model_stub.rs`
and the design spec) — STRING-valued, NOT the int table in ha-bridge/README.md
(that table is stale; see the note this script prints and the report):
  state:  {"name","cur","set","mode","action","min","max","step","modes":[..]}
          mode ∈ off|heat|cool|heat_cool|fan_only|dry   (heat_cool == "auto")
          action ∈ heating|cooling|idle
          modes  = array of the supported mode strings (watch → bitmask)
  cmd:    {"set":<float>}  and/or  {"mode":"<mode string>"}   on .../set

Broker + creds come from the environment (never hardcoded):
  MQTT_HOST (default 10.0.6.11)  MQTT_PORT (default 1883)
  MQTT_USER (default jp)         MQTT_PASS (required for live; e.g. `bw get password mosquitto`)

Usage:
  export MQTT_PASS=...            # from vault; do NOT commit
  python3 mock-climate-publisher.py               # live: publish + subscribe + drift
  python3 mock-climate-publisher.py --no-echo      # log commands but don't reflect them
  python3 mock-climate-publisher.py --once         # publish current state once, then exit
  python3 mock-climate-publisher.py --clear        # wipe the retained fake topics, then exit
  python3 mock-climate-publisher.py --dry-run      # print the JSON it would publish; no broker, no paho
"""

import argparse
import json
import os
import random
import signal
import sys
import time
from datetime import datetime

STATE_TOPIC = "watch/climate/{id}/state"
SET_TOPIC_WILDCARD = "watch/climate/+/set"
ROSTER_TOPIC = "watch/climate/roster"

VALID_MODES = {"off", "heat", "cool", "heat_cool", "fan_only", "dry"}

# 3 fake devices matching the task: a Nest-like thermostat (no fan/dry) and two
# minisplits (all six modes → modes-mask 63). object_ids are deliberately
# distinct from any real HA entity so a stray command can't drive a live unit.
DEVICES = {
    "nest_hall": {
        "name": "Hall Nest",
        "modes": ["off", "heat", "cool", "heat_cool"],        # mask 0b1111 = 15
        "min": 50.0, "max": 90.0, "step": 1.0,
        "cur": 67.5, "set": 70.0, "mode": "heat",
    },
    "split_office": {
        "name": "Office Mini-Split",
        "modes": ["off", "heat", "cool", "heat_cool", "fan_only", "dry"],  # mask 0b111111 = 63
        "min": 60.0, "max": 86.0, "step": 1.0,
        "cur": 75.0, "set": 72.0, "mode": "cool",
    },
    "split_bed": {
        "name": "Bedroom Mini-Split",
        "modes": ["off", "heat", "cool", "heat_cool", "fan_only", "dry"],  # mask 0b111111 = 63
        "min": 60.0, "max": 86.0, "step": 1.0,
        "cur": 70.0, "set": 71.0, "mode": "heat_cool",
    },
}


def log(msg):
    print(f"{datetime.now().strftime('%H:%M:%S')}  {msg}", flush=True)


def compute_action(mode, cur, set_):
    """What the unit is 'doing' — restricted to heating/cooling/idle, the three
    the firmware's HvacAction::from_ha recognises (unknowns map to idle)."""
    if mode == "heat":
        return "heating" if cur < set_ - 0.1 else "idle"
    if mode == "cool":
        return "cooling" if cur > set_ + 0.1 else "idle"
    if mode == "heat_cool":
        if cur < set_ - 1.0:
            return "heating"
        if cur > set_ + 1.0:
            return "cooling"
        return "idle"
    if mode == "dry":                       # dry ≈ light cooling for the demo
        return "cooling" if cur > set_ else "idle"
    return "idle"                           # off, fan_only


def state_json(dev):
    """The retained /state payload for one device (string-encoded contract)."""
    action = compute_action(dev["mode"], dev["cur"], dev["set"])
    return json.dumps({
        "name": dev["name"],
        "cur": round(dev["cur"], 1),
        "set": dev["set"],
        "mode": dev["mode"],
        "action": action,
        "min": dev["min"],
        "max": dev["max"],
        "step": dev["step"],
        "modes": dev["modes"],
    })


def drift(dev):
    """Nudge cur toward set while active (so the screen shows live movement)."""
    action = compute_action(dev["mode"], dev["cur"], dev["set"])
    if action == "heating":
        dev["cur"] += random.uniform(0.1, 0.4)
    elif action == "cooling":
        dev["cur"] -= random.uniform(0.1, 0.4)
    else:
        dev["cur"] += random.uniform(-0.15, 0.15)   # idle jitter
    dev["cur"] = max(dev["min"] - 5, min(dev["max"] + 5, dev["cur"]))


def validate_command(dev, payload):
    """Return (ok, notes[]) for a decoded /set command against the device."""
    notes = []
    ok = True
    if not isinstance(payload, dict):
        return False, ["payload is not a JSON object"]
    if "set" not in payload and "mode" not in payload:
        return False, ["neither 'set' nor 'mode' present"]
    if "set" in payload:
        v = payload["set"]
        if not isinstance(v, (int, float)):
            ok = False; notes.append(f"'set' is not a number ({v!r})")
        elif not (dev["min"] <= v <= dev["max"]):
            notes.append(f"'set'={v} outside [{dev['min']},{dev['max']}] (watch should clamp)")
    if "mode" in payload:
        m = payload["mode"]
        if not isinstance(m, str):
            ok = False; notes.append(f"'mode' is not a string ({m!r}) — firmware sends a string")
        elif m not in VALID_MODES:
            ok = False; notes.append(f"'mode'={m!r} not a known HA mode")
        elif m not in dev["modes"]:
            notes.append(f"'mode'={m!r} not in this device's supported modes {dev['modes']}")
    return ok, notes


# ------------------------------------------------------------------ dry-run ---

def do_dry_run():
    log("DRY RUN — payloads that WOULD be published (no broker, no paho):")
    print()
    print(f"  RETAIN {ROSTER_TOPIC}")
    print(f"         {json.dumps(list(DEVICES))}")
    for oid, dev in DEVICES.items():
        print(f"  RETAIN {STATE_TOPIC.format(id=oid)}")
        print(f"         {state_json(dev)}")
    print()
    log("subscribe would be: " + SET_TOPIC_WILDCARD)
    log("Contract note: mode/action/modes are STRINGS (per the firmware parser),")
    log("NOT the int table in README.md — the README needs correcting.")


# --------------------------------------------------------------------- live ---

def make_client():
    """Construct a paho Client across the 1.x / 2.x callback-API split."""
    import paho.mqtt.client as mqtt  # lazy: dry-run/--clear help need no paho
    try:
        return mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="mock-climate-bench")
    except (AttributeError, TypeError):
        return mqtt.Client(client_id="mock-climate-bench")  # paho 1.x


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

    def on_connect(c, userdata, flags, rc, *_):
        if str(rc) not in ("0", "Success"):
            log(f"connect failed: rc={rc}")
            return
        log(f"connected to {host}:{port} as {user!r}")
        if args.clear:
            for oid in DEVICES:
                c.publish(STATE_TOPIC.format(id=oid), payload=None, qos=0, retain=True)
            c.publish(ROSTER_TOPIC, payload=None, qos=0, retain=True)
            log("cleared retained fake topics — exiting")
            c.disconnect()
            return
        c.publish(ROSTER_TOPIC, json.dumps(list(DEVICES)), qos=0, retain=True)
        for oid, dev in DEVICES.items():
            c.publish(STATE_TOPIC.format(id=oid), state_json(dev), qos=0, retain=True)
        log(f"seeded retained roster + {len(DEVICES)} device states")
        c.subscribe(SET_TOPIC_WILDCARD, qos=0)
        log(f"subscribed {SET_TOPIC_WILDCARD} — waiting for watch commands")

    def on_message(c, userdata, msg):
        parts = msg.topic.split("/")
        oid = parts[2] if len(parts) >= 4 else "?"
        raw = msg.payload.decode("utf-8", "replace")
        dev = DEVICES.get(oid)
        if dev is None:
            log(f"CMD  {msg.topic}  {raw}  — WARN: unknown device id {oid!r}")
            return
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError as e:
            log(f"CMD  {msg.topic}  {raw}  — MALFORMED JSON: {e}")
            return
        ok, notes = validate_command(dev, payload)
        tag = "OK " if ok else "BAD"
        suffix = ("  [" + "; ".join(notes) + "]") if notes else ""
        log(f"CMD  {tag} {oid}  {raw}{suffix}")
        if args.no_echo:
            return
        if "set" in payload and isinstance(payload["set"], (int, float)):
            dev["set"] = float(payload["set"])
        if "mode" in payload and isinstance(payload["mode"], str) and payload["mode"] in VALID_MODES:
            dev["mode"] = payload["mode"]
        c.publish(STATE_TOPIC.format(id=oid), state_json(dev), qos=0, retain=True)
        log(f"     echo -> {oid} state (set={dev['set']} mode={dev['mode']})")

    client.on_connect = on_connect
    client.on_message = on_message

    stopping = {"v": False}

    def stop(*_):
        stopping["v"] = True
        log("shutting down (retained state left in place; run --clear to wipe)")
        try:
            client.disconnect()
        except Exception:
            pass

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)

    client.connect(host, port, keepalive=30)

    if args.once or args.clear:
        client.loop_forever()   # on_connect publishes once / clears, then disconnects
        return 0

    client.loop_start()
    try:
        while not stopping["v"]:
            time.sleep(args.interval)
            for oid, dev in DEVICES.items():
                drift(dev)
                client.publish(STATE_TOPIC.format(id=oid), state_json(dev), qos=0, retain=True)
        client.loop_stop()
    except Exception as e:
        log(f"loop error: {e}")
        return 1
    return 0


def main():
    ap = argparse.ArgumentParser(description="Bench mock climate publisher for the watch Climate screen.")
    ap.add_argument("--interval", type=float, default=4.0, help="seconds between state pushes (default 4)")
    ap.add_argument("--no-echo", action="store_true", help="log commands but don't reflect them into /state")
    ap.add_argument("--once", action="store_true", help="publish current state once, then exit")
    ap.add_argument("--clear", action="store_true", help="wipe the retained fake topics, then exit")
    ap.add_argument("--dry-run", action="store_true", help="print the payloads (no broker, no paho)")
    ap.add_argument("--allow-anonymous", action="store_true", help="connect without MQTT_PASS")
    args = ap.parse_args()

    if args.dry_run:
        do_dry_run()
        return 0
    return run_live(args)


if __name__ == "__main__":
    sys.exit(main())
