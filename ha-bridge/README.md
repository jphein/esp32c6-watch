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

**Encodings** — the wire carries **HA's native strings**. The watch's `crates/climate-model`
parses these strings (`HvacMode::from_ha`) and maps them to its internal enum/int for the
Slint UI — the int encoding lives *inside the firmware*, never on the wire:
- `mode`: HA hvac_mode string — `"off" "heat" "cool" "auto" "heat_cool" "dry" "fan_only"`
- `action`: HA hvac_action string — `"idle" "heating" "cooling" "drying" "fan" "off"`
- `modes`: array of the device's supported hvac_mode strings, e.g. `["off","heat","cool","heat_cool"]`

(FYI, the firmware's *internal* UI mapping, not on the wire: mode `off=0 heat=1 cool=2 auto=3
fan_only=4 dry=5` with `heat_cool→3`; action `idle=0 heating=1 cooling=2` with
`drying/defrosting→2`. The Node-RED command node translates `auto↔heat_cool` per device.)

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

---

# Watch ← HA Energy Bridge (`energy-bridge.flow.json`)

Read-only companion to the climate bridge: publishes your **home** energy (battery %,
solar, grid) to a single retained topic the watch's Energy screen mirrors. The watch only
subscribes — there are no energy commands back to HA. Same broker as climate; its own
client id so the two flows coexist.

## Topic contract

| Direction | Topic | Payload |
|---|---|---|
| HA → watch (state) | `watch/energy/state` (**retained**) | `{"battery_pct":78,"solar_w":3400,"grid_w":-1200,"charging":true}` |
| bridge → all (availability, LWT) | `watch/energy/avail` (**retained**) | `online` \| `offline` |

One aggregate JSON object (compact); the bridge always publishes **full state** (all four
keys every frame — never partial deltas), so the watch replaces its state wholesale. A field
is JSON `null` only before that sensor's first reading and **never regresses** to null once
known (values persist in the flow's context). Treat `null` as "unknown / keep last".

| Key | Type | Meaning |
|---|---|---|
| `battery_pct` | int 0..100 | home battery state-of-charge, % |
| `solar_w` | int ≥ 0 | solar production, **watts** (kW sensors auto-scaled) |
| `grid_w` | int, **signed** | grid power, watts — **> 0 importing** (buying), **< 0 exporting** (selling) |
| `charging` | bool | home battery charging |

Keys mirror the `EnergyPage` (`ui/slint/energy.slint`) properties 1:1:
`battery_pct → battery-pct`, `solar_w → solar-w`, `grid_w → grid-w`, `charging → charging`.

**Availability / `conn-state`.** The screen shows *connecting…* until the retained
`watch/energy/state` first arrives, the live readout once it has data, and *HA unreachable*
when the MQTT session drops, `watch/energy/avail` is `offline` (the bridge's Last-Will), or
the value goes stale. Because state is **retained**, a freshly-subscribed watch gets current
values immediately.

## Configure (before deploy)

Two edit points, kept in lock-step (like the climate flow):

1. **`energy sensors changed`** node → its *Entity* list — set to your HA entity_ids.
2. **`route + build energy state`** function → the `ROLES` map at the top — map each of
   those same entity_ids to a role: `batt` · `solar` · `grid` · `chg`.

Grid can be **one signed sensor** (`grid`, + import / − export) **or two positive sensors** —
drop `grid` and map `grid_import` + `grid_export` instead (the function computes
`grid = import − export`). Power roles auto-scale `kW → W` from each sensor's
`unit_of_measurement`.

## Import steps

1. Node-RED → menu → Import → paste `energy-bridge.flow.json`.
2. **Broker config** (`watch-mqtt … energy`): confirm host `10.0.6.11:1883`, set the mosquitto
   username/password under *Security*. This flow uses its own broker/client id
   (`nodered-watch-energy-bridge`), so it coexists with the climate bridge.
3. **HA server node** (`energy sensors changed`): re-select your Home Assistant server in the
   *Server* dropdown (the `ha_server_ref` placeholder won't resolve on import — normal).
4. Set the entity list + `ROLES` map (see *Configure* above).
5. Deploy. The state node has *output on connect* on, so it seeds `watch/energy/state` from
   current values at startup.

## Test

- `mosquitto_sub -h 10.0.6.11 -u jp -P … -t 'watch/energy/#' -v` → within a few seconds of
  deploy you should see a retained `watch/energy/state {...}` line plus `watch/energy/avail online`.
- Move a mapped sensor in HA (or wait for solar to change) → the retained `state` payload updates.
- Stop the flow / Node-RED → `watch/energy/avail` flips to `offline` (the LWT) → the watch's
  Energy screen shows *HA unreachable*.
