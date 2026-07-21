# Watch ↔ HA — one-time onboarding (after flashing)

The watch talks **only MQTT** to mosquitto (`10.0.6.11:1883`); two Node-RED flows translate
to/from Home Assistant. Do this **once** and the Climate + Energy screens come alive. ~5 min.
Full reference: [`README.md`](README.md).

**You need:** Node-RED with `node-red-contrib-home-assistant-websocket`, an HA server config
already connected, and mosquitto reachable at `10.0.6.11:1883`.

> 🔑 **Never inline the mosquitto password.** Pull it from vault when you need it:
> `bw get password mosquitto` (→ your mosquitto / HA-MQTT item). The commands below fetch it
> inline with `$(bw get …)` so it never lands in a file or your shell history verbatim.

> 💡 **Finding entity_ids** (needed below): HA → *Developer Tools → States* and filter — e.g.
> `climate.` for thermostats, or `sensor.` / `binary_sensor.` for the energy sensors.

---

## Part 1 — Climate bridge (Nest + minisplits)

1. **Import** — Node-RED → ☰ menu → *Import* → paste [`climate-bridge.flow.json`](climate-bridge.flow.json) → *Import*.
2. **Broker** — open the **`watch-mqtt (HA mosquitto)`** node → confirm host `10.0.6.11`, port `1883` → *Security* tab → user `jp`, password from vault → *Update*.
3. **HA server** — open **`climate.* changed`** and **`climate.set_* (from msg.payload)`** → in each, re-select your Home Assistant server in the *Server* dropdown (the imported `ha_server_ref` placeholder won't resolve — normal).
4. **Call-service mapping** — in **`climate.set_* (from msg.payload)`**: leave *Domain* + *Service* blank and set the node to take them **from `msg.payload`** (the "use msg.payload for domain/service/target/data" option — the upstream `parse cmd` node already emits exactly that shape).
5. **Deploy.** The `climate.* changed` node has *output on connect* on, so it seeds every retained state topic (and the retained `watch/climate/roster`) immediately.

## Part 2 — Energy bridge (home battery / solar / grid)

**Optional & independent.** Skip it and the watch's **Energy** screen simply shows
**"HA unreachable"** — that's expected, not a fault. Set it up when you want live house
battery/solar/grid. Same broker, its own client id, so both flows coexist.

1. **Import** [`energy-bridge.flow.json`](energy-bridge.flow.json) the same way.
2. **Broker** — open **`watch-mqtt … energy`** → set host + vault password (as Part 1 step 2).
3. **HA server** — open **`energy sensors changed`** → re-select your HA server.
4. **Map your sensors** (two spots, keep them in lock-step; find the ids via Dev Tools → States):
   - **`energy sensors changed`** node → *Entity* list → your HA entity_ids.
   - **`route + build energy state`** function → the `ROLES` map at the top → tag each of those entity_ids as `batt` · `solar` · `grid` · `charging`.
   - Grid can be one signed sensor (`grid`, + import / − export) **or** split `grid_import` + `grid_export`. Power auto-scales kW→W.
5. **Deploy.**

---

## Verify (copy-paste)

```sh
# retained state should appear within a few seconds of Deploy
mosquitto_sub -h 10.0.6.11 -u jp -P "$(bw get password mosquitto)" -t 'watch/climate/#' -v
#   → one retained  watch/climate/<entity>/state {...}  per climate entity
#   → one retained  watch/climate/roster  ["living_room","bedroom",...]

mosquitto_sub -h 10.0.6.11 -u jp -P "$(bw get password mosquitto)" -t 'watch/energy/#'  -v
#   → watch/energy/state {"battery_pct":..,"solar_w":..,"grid_w":..,"charging":..}
#   → watch/energy/avail online
```

## Test a command round-trip (climate is bidirectional)

```sh
# set a thermostat's target — <oid> = entity_id minus "climate." (e.g. living_room)
mosquitto_pub -h 10.0.6.11 -u jp -P "$(bw get password mosquitto)" \
  -t 'watch/climate/<oid>/set' -m '{"set":70}'
#   → the entity's setpoint changes in HA

mosquitto_pub -h 10.0.6.11 -u jp -P "$(bw get password mosquitto)" \
  -t 'watch/climate/<oid>/set' -m '{"mode":"cool"}'
#   → the entity's HVAC mode changes in HA
```

Then the watch's **Climate** screen mirrors it, and **Energy** shows live house battery/solar/grid.
(Energy is read-only — no command topic.)

---

## If something's off

- **No `…/state` lines** → the `changed` node's *output on connect* is off, or the HA server
  wasn't re-selected. Re-open the HA node, confirm the server, re-Deploy.
- **`set` command does nothing** → the call-service node isn't reading `msg.payload`
  (Part 1 step 4). Confirm domain/service/target/data all come from the message.
- **Auth failure on connect** → wrong mosquitto user/password on the broker node's *Security* tab.
- **Energy screen "HA unreachable"** → either you haven't set up the energy bridge (expected — it's
  optional), or the flow/Node-RED is down (its `watch/energy/avail` LWT went `offline`), or MQTT
  dropped. Deploy the energy flow / check Node-RED.
- **Energy fields `--` / `0`** → an entity_id in the `ROLES` map doesn't match HA, or that sensor
  hasn't reported yet (values fill in as sensors update; they never regress once known).

**New `climate.*` entities auto-appear** on the watch with zero further setup (retained state) —
so once the 2 Tuya minisplits are `climate.*` in HA (see [`README.md`](README.md) → *Layer A*),
they show up on their own.
