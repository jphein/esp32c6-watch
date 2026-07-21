# Watch ↔ HA Climate Bridge (Node-RED)

Bridges the esp32c6-watch to Home Assistant `climate.*` entities (Nest thermostats,
minisplits) over MQTT. The watch never talks to HA's API directly — it only speaks
MQTT to mosquitto (`10.0.6.11:1883`), and this Node-RED flow translates.

**Design spec:** `docs/superpowers/specs/2026-07-20-ha-climate-control-design.md`.
**This is an importable artifact — review it before importing into your live Node-RED.**

## Topic contract

| Direction | Topic | Payload |
|---|---|---|
| HA → watch (state) | `watch/climate/<object_id>/state` (**retained**) | `{"name","cur","set","mode","action","min","max","step","modes":[..]}` |
| watch → HA (command) | `watch/climate/<object_id>/set` | `{"set":72.0}` **or** `{"mode":"heat"}` |

`<object_id>` = the entity_id minus `climate.` (e.g. `climate.living_room` → `living_room`).

**Encodings** (must match `crates/climate-model` + the Slint `ClimateCard`):
- `mode`: `0 off · 1 heat · 2 cool · 3 auto · 4 fan_only · 5 dry` (HA `heat_cool` → `3`)
- `action`: `0 idle/off/fan · 1 heating/preheating · 2 cooling/drying/defrosting`
- `modes`: array of the supported mode ints (the watch turns this into its bitmask)

Because state is **retained**, a freshly-subscribed watch gets current values immediately,
and **any new `climate.*` entity auto-appears on the watch** — no firmware change. That's how
the 2 Tuya minisplits (Layer A below) light up once integrated.

## Import steps

1. Node-RED → menu → Import → paste `climate-bridge.flow.json`.
2. **Broker config** (`watch-mqtt`): open it, confirm host `10.0.6.11:1883`, and set the
   username/password (`jp` / your mosquitto pw) under Security. (Not stored in the file.)
3. **HA server nodes** (`climate.* changed` + `climate.set_*`): open each, and in the
   *Server* dropdown re-select your existing Home Assistant connection (the placeholder
   `ha_server_ref` won't resolve on import — this is normal Node-RED behavior).
4. **`climate.set_* (from msg.payload)` node** — configure it to take the call from
   `msg.payload`: leave *Domain*/*Service* blank and set the node to read them from the
   message (in `node-red-contrib-home-assistant-websocket` this is the "Use msg.payload
   for domain/service/target/data" option, or map `domain=payload.domain`,
   `service=payload.service`, `target=payload.target`, `data=payload.data`). The
   `parse cmd` function already emits exactly that shape.
5. Deploy. The `climate.* changed` node has *output on connect* on, so it seeds all
   retained state topics at startup.

## Test

- `mosquitto_sub -h 10.0.6.11 -u jp -P … -t 'watch/climate/#' -v` → you should see a
  retained `…/state` line per climate entity within a few seconds of deploy.
- Publish a command by hand:
  `mosquitto_pub -h 10.0.6.11 -u jp -P … -t 'watch/climate/<oid>/set' -m '{"set":70}'`
  → the entity's setpoint changes in HA. Then `{"mode":"cool"}` → mode changes.
- Then the watch's Climate screen mirrors it (once Layer B firmware ships).

## Layer A — integrate the 2 Tuya minisplits (so they appear on the watch)

They're **Tuya-compatible**, so once they're `climate.*` entities in HA the bridge picks
them up automatically. Recommended path — **LocalTuya** (local control, no cloud latency):

1. Get each unit's `device_id` + `local_key`:
   - Easiest: `pip install tinytuya && python -m tinytuya wizard` — needs a free
     [Tuya IoT Platform](https://iot.tuya.com) developer account with a Cloud project,
     link your Smart Life app account, then the wizard dumps every device's id + local key.
2. HA → Settings → Integrations → **LocalTuya** (HACS) → add each device with its
   id + local key + IP; map the DPS to the climate platform (target temp, current temp,
   mode, on/off). LocalTuya's climate template covers standard Tuya AC DPs.
3. Fallback if local keys are painful: the official **Tuya** cloud integration exposes the
   same units as `climate.*` (cloud-dependent) — also works with this bridge unchanged.

Either way the result is 2 more `climate.*` entities → 2 more cards on the watch, zero
firmware/bridge change. I'll walk the tinytuya wizard with you when you're ready (it needs
your Tuya account) — that's the one Layer-A step I can't do without your credentials.
