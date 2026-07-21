# HA Climate Control — Design Spec

**Date:** 2026-07-20
**Status:** approved-in-principle (JP: "as you suggest"); pending spec review
**Goal:** Control Home Assistant `climate.*` entities (Nest heat-pump thermostats + minisplits) from the watch — view current temp / setpoint / mode and adjust setpoint + mode — with new devices auto-appearing on the watch as they're integrated into HA.

## Context / constraints

- The watch is **plain-HTTP/MQTT-only on the LAN** (no TLS/DNS). MQTT broker = HA mosquitto at `10.0.6.11:1883` (creds baked in gitignored `.cargo/config.toml`).
- `src/net/mqtt_ha.rs` today is **publish-only** (fire-and-forget QoS-0 burst: CONNECT + PUBLISH×3 + DISCONNECT, plus one retained discovery config for the battery sensor). No SUBSCRIBE.
- The watch's climate devices are **NOT MQTT-native** (nothing under `homeassistant/climate/#`). Nests + 2 minisplits are behind native HA integrations; 2 more minisplits (Tuya) are not yet integrated.
- Single 2.4 GHz radio shared between WiFi and the ESP-NOW mesh — WiFi and mesh can't run simultaneously (switch, not coexist). The watch already bursts WiFi (connect → NTP/weather → disconnect → mesh ch6).

## Inventory (2026-07-20, per JP)

| Device | In HA? | Integration |
|---|---|---|
| Nest thermostats (main heat pump) | ✅ | Nest (native) |
| Minisplit ×2 | ✅ | native |
| Minisplit ×2 | ❌ | **Tuya** → Layer A |

## Architecture

Three components across two systems. **Core principle: the watch renders whatever the bridge publishes — no hardcoded entity list.**

### A. Node-RED bridge (HA side)

Runs on the existing Node-RED instance; talks to HA (companion nodes) + the MQTT broker. Delivered as an **importable flow JSON** + setup notes.

- **State out:** trigger on HA `climate.*` state changes → publish compact JSON to **retained** topic `watch/climate/<object_id>/state`. Payload:
  ```json
  {"name":"Living Room","cur":71.5,"set":72,"mode":"heat","action":"heating","min":50,"max":90,"step":1.0,"modes":["off","heat","cool","auto"]}
  ```
  Retained so a freshly-subscribed watch gets current state immediately. Emit on HA start + every change.
- **Command in:** MQTT-in on `watch/climate/+/set` → parse `{"set":<temp>}` and/or `{"mode":"<hvac_mode>"}` → `call-service`:
  - `climate.set_temperature` `{entity_id, temperature}`
  - `climate.set_hvac_mode` `{entity_id, hvac_mode}`
- **Roster (retained):** publish `watch/climate/roster` = JSON array of `object_id`s, so the watch knows the device set deterministically (belt-and-suspenders alongside the wildcard subscription).

The bridge is integration-agnostic: it calls `climate.*` services, so it works for Nest, the native minisplits, and the Tuya minisplits once they land — with zero watch or bridge change.

### B. Watch MQTT client (bidirectional)

New module `src/net/mqtt_climate.rs` (keep `mqtt_ha.rs` publish-burst intact for telemetry). Bidirectional MQTT 3.1.1 session:

- CONNECT (clean session) → SUBSCRIBE `watch/climate/+/state` + `watch/climate/roster` (QoS 0) → handle inbound PUBLISH → PUBLISH commands to `watch/climate/<id>/set` → PINGREQ keepalive → DISCONNECT on close.
- **Session lifecycle** tied to the Climate screen: on open, ensure WiFi up (reconnect from mesh if needed — reuse #23's `RadioMode` if landed, else a wifi-hold flag) + open the MQTT session; on close, DISCONNECT + return to mesh. **Never strand the mesh** — closing the screen always restores mesh (structural guarantee, same discipline as #23).
- Parsing/encoding lives in a pure host-testable crate (below), not in the async I/O module.

### B′. `crates/climate-model` (pure, host-testable)

Per the host-testable-crates pattern (esp-hal build.rs panics on host → pure logic in standalone `no_std` crates).

- `parse_state(&[u8]) -> Option<ClimateEntity>` — bounded, never panics on malformed/partial JSON (skip the entity).
- `ClimateEntity { name: String<32>, cur: Option<f32>, set: Option<f32>, mode: HvacMode, action: HvacAction, min: f32, max: f32, step: f32, modes: heapless::Vec<HvacMode, 7> }`.
- `ClimateState { entities: heapless::Vec<(ObjId, ClimateEntity), N> }` — upsert-by-object-id as state messages arrive.
- `encode_set_temp(temp) / encode_set_mode(mode) -> heapless::String` — command payloads.
- `clamp_step(cur_set, delta, min, max, step) -> f32` — setpoint ± logic.
- Host tests: thermostat JSON, minisplit with dry/fan modes, missing/extra fields, clamp/step at bounds, command encode round-trip.

### C. Watch Climate screen (Slint)

`ui/slint/climate.slint` + shell wiring (AppState::Climate, SYSTEM launcher tile, new icon-id). Slint **overlay** (renders through the resident scene, like WLED/Energy — no framebuffer).

- **List view:** VecModel `[ClimateCard]` — one card per entity: name, current temp (large), setpoint, mode chip, hvac-action color (heating=warm, cooling=cool, idle=neutral). Built dynamically from `ClimateState`.
- **Detail view** (tap a card): setpoint **−/+** (by `step`, clamped `min..max`), mode selector segmented control filtered to the entity's supported `modes` (off/heat/cool/auto/fan/dry).
- Each adjustment → **optimistic UI update** + **debounced** command publish (~400 ms after the last tap) → reconcile on the next inbound state message.
- States: "connecting…", "HA unreachable", "no climate devices".

## Data flow

```
Open Climate screen → WiFi session up → SUBSCRIBE watch/climate/+/state
   → receive retained states → render cards
User taps +/− → optimistic update + debounced PUBLISH watch/climate/<id>/set {"set":72}
   → Node-RED → climate.set_temperature → HA → device
   → HA state change → Node-RED → watch/climate/<id>/state (retained) → watch reconciles
Close screen → MQTT DISCONNECT → RadioMode back to mesh
```

## Error handling

- No WiFi / MQTT connect fail → screen shows "HA unreachable"; **mesh unaffected**.
- QoS-0 command not reflected in state within ~5 s → optimistic value reverts to last confirmed (reconcile-on-state; no command ACK to rely on).
- Malformed state JSON → skip that entity (bounded parse, host-tested, no panic — untrusted-input discipline like the rssi clip hardening).
- WiFi session drop → one reconnect attempt, else "reconnecting…".
- Closing the screen (or any error) always returns to mesh — mesh cannot be stranded.

## Units

HA is the source of truth. Bridge publishes the entity's native unit (Nest = °F). Watch is a **unit-agnostic passthrough**: display the number, label °F, send back the same unit. No conversion on the watch.

## Testing

- `crates/climate-model` host tests (above) — the correctness core.
- Node-RED flow: manual — set a Nest setpoint from the watch → HA reflects it; change a minisplit mode → device responds.
- On-glass (JP-gated): open Climate → see Nests + 2 minisplits → adjust a setpoint → device responds → close → mesh recovers.

## Layer A (parallel, HA side): integrate the 2 Tuya minisplits

- **Path:** **LocalTuya** (local control, no cloud latency, survives internet loss) — needs each device's `device_id` + `local_key` (via a Tuya IoT Platform dev account or the `tuya-cli`/`tinytuya` wizard). Fallback: official **Tuya** cloud integration (easier, cloud-dependent).
- Result: 2 new `climate.*` entities → **auto-appear on the watch** via the bridge, zero firmware change.
- This is HA/hardware config, not watch firmware — a separate track, parallelizable with Layer B.

## Scope / phasing

- **v1 = Layer B**: Node-RED bridge + `crates/climate-model` + `mqtt_climate.rs` + Climate screen, controlling the existing HA climate entities (Nests + 2 minisplits). Targets a v0.5.0 release (new user-facing subsystem).
- **Layer A**: Tuya minisplit integration (HA side) — proceeds in parallel; devices appear when done.

## YAGNI (explicitly out of v1)

- No scheduling / automations authored from the watch (HA owns that).
- No device config/pairing UI on the watch (HA owns config).
- No granular fan-speed / swing / humidity control in v1 — mode + setpoint only; add later if wanted.
- No multi-broker / auth beyond the existing baked creds.
