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

## Bench testing — `mock-climate-publisher.py` (NO live HA)

Validate the watch's Climate screen against **fake** devices, so you never touch
JP's live HA/Node-RED. The mock only speaks MQTT on `watch/climate/#` (it never
calls HA); it fakes the HA→watch side and logs the watch→HA commands.

```bash
python3 -m venv .venv && . .venv/bin/activate && pip install 'paho-mqtt>=2.0'
export MQTT_PASS=$(bw get password mosquitto)     # creds via env, never hardcoded
python3 mock-climate-publisher.py                  # publish 3 fakes + drift + log commands
```
Then flash the watch, open the Climate screen: three cards appear (Hall Nest,
Office/Bedroom Mini-Split), `cur`/`action` move live, and every setpoint/mode
tap is logged here (`CMD OK …`) and — unless `--no-echo` — reflected back into
`/state` so the optimistic-update → reconcile loop is exercised.

- `--dry-run` prints the payloads without a broker or paho (offline contract check).
- `--once` seeds retained state once and exits. `--no-echo` logs commands only.
- **`--clear` wipes the retained fake topics** — run it when done so the fakes
  don't linger on the shared broker.

Env: `MQTT_HOST` (10.0.6.11) · `MQTT_PORT` (1883) · `MQTT_USER` (jp) · `MQTT_PASS`.

⚠️ **Wire-format note:** the mock publishes `mode`/`action`/`modes` as **strings**
(`"heat"`, `"heating"`, `["off","heat",…]`) to match the firmware parser
(`crates/climate-model` / `climate_model_stub.rs` `from_ha`) and the design-spec
JSON example. The "Encodings" int table in §Topic contract above is **stale** and
should be corrected to strings (or the firmware switched to ints — but the parser
is strings today).

⚠️ **Shared-broker caution:** object_ids (`nest_hall`, `split_office`,
`split_bed`) are chosen to not match real HA entities, and the mock never calls
HA. But if the real Node-RED climate flow is *also* deployed and subscribed to
`watch/climate/+/set`, a command for a fake id would reach it (→ a no-op HA
`climate.set_*` on a nonexistent entity). Run the mock when that flow is **not**
deployed, or point `MQTT_HOST` at a scratch broker.

## Bench testing — `mock-energy-publisher.py` (NO live HA)

Validate the watch's **Energy** screen (home battery / solar / grid) against a
**fake** feed. Only touches `watch/energy/#` (never calls HA); mirrors the real
`energy-bridge.flow.json` contract:

- `watch/energy/state` **(retained)** = `{"batt":78,"solar":3400,"grid":-1200,"chg":true}`
  (`batt` 0-100 · `solar` W≥0 · `grid` W signed, **+import / −export** · `chg` bool)
- `watch/energy/avail` **(retained)** = `online` | `offline` (`offline` is the LWT)

```bash
export MQTT_PASS=$(bw get password mosquitto)
python3 mock-energy-publisher.py           # online + retained state, drifting every 3s
```
Runs a solar day/night sweep with battery charge/discharge, so `grid` flips
**EXPORT ↔ IDLE ↔ IMPORT** and the battery goes low(red) → charging(teal) →
discharging every ~120 s cycle — every colour branch in `energy.slint`. Each log
line is tagged `[EXPORT chg]` / `[IMPORT LOW]` etc. so you can eyeball the state.

- `--dry-run` prints sample frames offline (no broker/paho). `--once` seeds one
  frame. **`--clear`** wipes the retained `state`+`avail` topics when done.
- On clean exit (or crash → LWT) it publishes `avail=offline`, so the screen's
  offline handling is testable too.
- Same env vars as the climate mock (`MQTT_HOST`/`PORT`/`USER`/`PASS`).

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
