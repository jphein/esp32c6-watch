# ESP32-C6 Watch ↔ Home Assistant — Native HTTP Component

**Date:** 2026-07-21
**Status:** design + build for review (NOT deployed / NOT flashed)
**Supersedes:** `2026-07-20-ha-climate-control-design.md` (MQTT + Node-RED bridge)
**Branch:** `dream/http-climate` (off `main` @ `2a375e9`)

## Goal

Replace the interim **Node-RED climate bridge + MQTT broker** with a **native
Home Assistant custom integration** (`esp32c6_watch`) that the watch talks to over
**plain HTTP** (dotted-quad, no TLS, no DNS) — the same transport the rest of the
watch net stack already uses (`voice_stt.rs`, `ota_http.rs`, `weather.rs`). This
drops two subsystems (Node-RED flow + bare MQTT broker leg) and moves all logic
into one reviewable HA component that reads `climate.*` / energy entities natively
via `hass.states` and calls `climate.*` services directly.

The watch's existing correctness core — `crates/climate-model` and
`ui/slint/climate.slint` — is reused **UNCHANGED**. Only the transport module
swaps: `src/net/mqtt_climate.rs` → `src/net/http_climate.rs`.

## Why this is cleaner than the MQTT/Node-RED path

- The MQTT climate path was baked to `MQTT_BROKER="10.0.6.11:1883"` — a **VLAN-6**
  address, **firewalled off the watch's VLAN-11 roam network** (10.0.11.0/24). It
  could not actually reach the broker from roam; the feature was never provable
  end-to-end on-glass. (Same class of bug the STT gateway hit and solved by moving
  the endpoint onto a roam-reachable IP.)
- Node-RED added a second moving part (flow JSON, companion nodes) and the broker
  added a third (auth, retained-topic semantics, LWT). All of it existed only to
  shuttle `climate.*` state/commands the HA process already holds in memory.
- A native component reads `hass.states` and calls `climate.set_temperature` /
  `climate.set_hvac_mode` directly — no broker, no flow, no retained topics, no LWT.

## Network reachability (the pivotal constraint)

**Finding (verified 2026-07-21 via `ssh jp@10.0.6.108`):** HA's `http:` block sets
`ssl_certificate: /ssl/fullchain.pem` + `ssl_key: /ssl/privkey.pem`. **HA serves
HTTPS on :8123, not plain HTTP.** The watch has no TLS, so it **cannot** hit HA's
main port directly. This kills the naïve "point the watch at `http://10.0.11.110:8123`"
plan.

**HA VM topology (from `ha` skill, verified):** HAOS on KVM, **quad-homed**, HA
core runs host-networked so a bound socket answers on every leg:

| Leg | IP | Notes |
|---|---|---|
| VLAN6 (server) | 10.0.6.108 | default gw leg; behind Caddy `ha.jphe.in` |
| VLAN8 (iot) | 10.0.8.111 | MQTT `ha-vlan8` leg |
| VLAN10 | 10.0.10.222 | |
| **VLAN11 (roam)** | **10.0.11.110** | **same L2 as the watch** — no inter-VLAN routing |

The watch associates to SSID `roam` (VLAN11, 10.0.11.0/24). HA already has a
**VLAN-11 leg at 10.0.11.110** on the same L2 — so a socket the component binds on
`0.0.0.0:<port>` is reachable from the watch **same-subnet**, exactly like the
MQTT broker's 10.0.11.110 leg is.

### Transport decision — component-owned plain-HTTP listener (primary)

The `esp32c6_watch` integration starts **its own `aiohttp` web server on a
dedicated plain-HTTP port (default `8124`), bound to `0.0.0.0`**, inside HA's event
loop (via `aiohttp.web.AppRunner` + `TCPSite`, torn down on unload). Because HA
core is host-networked, that socket answers on the VLAN-11 leg:

```
watch (10.0.11.x, plain HTTP) ──> http://10.0.11.110:8124/watch/...  ──> esp32c6_watch component ──> hass.states / climate.* services
```

- **No** Node-RED, **no** MQTT broker, **no** ubox0 proxy. One component, one port.
- Plain HTTP, dotted-quad — satisfies the watch's net constraints verbatim.
- The port bypasses HA's TLS/auth (that lives on 8123). Mitigations: (a) VLAN-11 is
  firewalled off the server LAN; (b) an optional shared-secret token header
  (`X-Watch-Token`) the view checks; (c) the endpoint only exposes `climate.*` +
  a fixed energy summary — no general HA access. This is the same trust model the
  plain-HTTP STT bridge already runs under.

**One thing to verify on-device (JP-gated, since we do not deploy):** that a socket
bound by the HA-core process on `0.0.0.0:8124` is externally reachable on
10.0.11.110:8124. This is expected (HAOS host-networking is why 8123 and the MQTT
addon are reachable per-leg), but it is the single unproven assumption.

### Fallback — thin ubox0 proxy (documented, not built)

If the in-process bind is *not* externally reachable on HAOS for some reason, run
the endpoints as normal `HomeAssistantView`s on HA's HTTPS :8123 and put a **dumb
plain-HTTP→HTTPS reverse proxy on ubox0's VLAN-11 leg (10.0.11.11)** — the exact
box/pattern the STT gateway uses (`watch_bridge.py` on ubox0 :8090). The watch
then targets `http://10.0.11.11:<port>`. This reintroduces one thin hop (no logic,
no broker) but is strictly a contingency; the primary design needs no extra host.

## HTTP endpoint contract

Base URL from the watch: `http://<HA_HTTP>` where `HA_HTTP` defaults to
`10.0.11.110:8124` (baked at build time, overridable — see firmware §). All
responses `Content-Type: application/json`. All request/response bodies are the
**same JSON shapes `crates/climate-model` already parses** — the crate is untouched.

### `GET /watch/climate/state`

Returns a JSON **array** of all exposed climate entities. Each element is exactly a
`climate-model` state object **plus an `"id"` field** (the HA `object_id`).
`parse_state` ignores unknown keys, so the extra `"id"` is transparent to the crate;
the firmware pulls `"id"` itself before handing the object to `parse_state`.

```json
[
  {"id":"kitchen_thermostat","name":"Kitchen Floor","cur":71.5,"set":72,
   "mode":"heat","action":"heating","min":50,"max":90,"step":1.0,
   "modes":["off","heat","cool","auto"]},
  {"id":"bedroom_minisplit","name":"Bedroom Minisplit","cur":74,"set":72,
   "mode":"cool","action":"cooling","min":61,"max":86,"step":1.0,
   "modes":["off","heat","cool","auto","fan_only","dry"]}
]
```

Field mapping (HA attribute → JSON key), all in the entity's native unit (°F here —
watch is a unit-agnostic passthrough):

| JSON | HA source |
|---|---|
| `id` | `entity_id` minus the `climate.` prefix (`object_id`) |
| `name` | `friendly_name` |
| `cur` | `current_temperature` (may be `null`) |
| `set` | `temperature`; for a dual-setpoint (`heat_cool`) entity → midpoint of `target_temp_low`/`high` (see below) |
| `mode` | entity `state` (`heat_cool` normalized to `auto` — see below) |
| `action` | `hvac_action` |
| `min` | `min_temp` |
| `max` | `max_temp` |
| `step` | `target_temp_step` (default `1.0` if absent) |
| `modes` | `hvac_modes` (`heat_cool` normalized to `auto`; deduped) |

### `POST /watch/climate/<object_id>/set`

Body is one of the `climate-model` command encodings (`encode_set_temp` /
`encode_set_mode`) — **unchanged**:

```json
{"set":72.0}      // → climate.set_temperature
{"mode":"heat"}   // → climate.set_hvac_mode
```

- `{"set":X}` → `climate.set_temperature {entity_id, temperature: X}`. For a
  `heat_cool` entity (single `set` from the watch), set `target_temp_low`/`high`
  preserving the current spread around `X` (or a default ±2°F) — see decision D3.
- `{"mode":M}` → `climate.set_hvac_mode` with the **capability-aware** mapping below.
- Returns `200 {"ok":true}` on accepted, `4xx {"error":"..."}` on unknown entity /
  bad body. The watch is optimistic + reconciles on the next poll; it does not
  depend on the response body, only the 200.

### `GET /watch/energy`

Returns the energy summary in the **same shape `parse_energy` already parses**:

```json
{"battery_pct":62,"solar_w":405,"grid_w":946,"charging":true}
```

- `grid_w`: signed, **+ import / − export** (contract preserved). With no whole-home
  CT clamp installed, the default source (`sensor.solar_arbitrage_grid_draw`) is
  import-positive; export is not measured → reported as import-only. (Decision D4.)
- The `online` flag the firmware used to get from the MQTT LWT is **gone**; the
  firmware now derives energy reachability from GET success/failure directly.

### `GET /watch/climate/roster` (optional / parity)

`["kitchen_thermostat","bedroom_minisplit",...]` — the exposed object-ids. The
watch derives everything it needs from the state array, so this is informational
(kept for symmetry with the old contract; the firmware does not require it).

### `GET /watch/version` (health / sigil)

`{"component":"esp32c6_watch","version":"0.1.0","entities":<n>}` — a cheap liveness
probe and a hook for realm-sigil-style versioning.

## Capability-aware `auto` ↔ `heat_cool` (the real-device case)

Verified against live HA (2026-07-21):

- **Nests** (`climate.*_thermostat`, `laundry_thermostat_2`): `hvac_modes =
  [heat, cool, heat_cool, off]` — **`heat_cool`, no `auto`.**
- **Minisplits** (`climate.kitchen_minisplit`, `bedroom_minisplit`, …):
  `hvac_modes = [off, fan_only, heat, cool, dry, auto]` — **`auto`, no `heat_cool`.**

The watch UI has a single "Auto" button; `climate-model` folds **both** `auto` and
`heat_cool` → `HvacMode::Auto` on parse, and emits `"auto"` on command
(`HvacMode::Auto.to_ha() == "auto"`). So:

- **State (out):** the component normalizes `heat_cool` → `auto` in both the `mode`
  field and the `modes` list, so a Nest presents an "Auto" chip the UI can render.
- **Command (in):** when the watch sends `{"mode":"auto"}`, the component inspects
  the target entity's real `hvac_modes`:
  - `auto` supported → `set_hvac_mode(auto)` (minisplit);
  - else `heat_cool` supported → `set_hvac_mode(heat_cool)` (Nest);
  - else no-op (unsupported).
  Every other mode (`off`/`heat`/`cool`/`fan_only`/`dry`) passes through if the
  entity supports it, else is ignored. This is the capability translation the old
  Node-RED bridge did, now native (the component has the entity's real modes).

## Energy entity mapping (configurable; discovered defaults)

Defaults chosen from live HA summary sensors (2026-07-21); all overridable via the
config-flow options so JP can repoint without editing code:

| Watch field | Default entity | Value seen |
|---|---|---|
| `battery_pct` | `sensor.battery_average_soc` | 62 % |
| `solar_w` | `sensor.total_solar_power` | 405 W |
| `grid_w` | `sensor.solar_arbitrage_grid_draw` | 946 W (import +) |
| `charging` | *(optional)* battery-power sensor sign, else `false` | — |

## Component architecture (`custom_components/esp32c6_watch/`)

Delivered in-repo at `ha/custom_components/esp32c6_watch/` (this branch) for review;
deploys to `/homeassistant/custom_components/esp32c6_watch/` on the HA VM. HACS is
present, so it can also be added as a HACS *custom repository*; manual copy
(`cat … | ssh jp@10.0.6.108 "sudo tee …"`) is the simplest install.

- `manifest.json` — `domain: esp32c6_watch`, `iot_class: local_push`, deps `[http]`,
  version, code owner.
- `__init__.py` — `async_setup_entry`: read options (port, token, entity filters,
  energy entity map); build the `aiohttp.web.Application`, register routes, start an
  `AppRunner` + `TCPSite("0.0.0.0", port)`; store the runner for teardown in
  `async_unload_entry`.
- `views.py` (or handlers in `__init__`) — the five endpoints above; pure reads of
  `hass.states` + `hass.services.async_call("climate", …)`; JSON built to the
  contract; optional `X-Watch-Token` check.
- `config_flow.py` — a simple flow (port, optional token, include/exclude climate
  entities, energy entity map) so no YAML editing is required; sensible defaults so
  "just install it" works.
- `const.py`, `strings.json`/`translations/en.json`.

Entity exposure: **default = all `climate.*` entities**, with an optional
exclude-list to hide the MQTT-HVAC duplicates (`climate.kitchen_mqtt_hvac`,
`climate.bedroom_mqtt_hvac`) that mirror the minisplits. New climate entities
auto-appear on the watch with zero firmware/component change (the core promise).

## Firmware (`src/net/http_climate.rs`)

Replaces `src/net/mqtt_climate.rs` with a **plain-HTTP client** mirroring
`voice_stt.rs` / `ota_http.rs`. **Keeps the identical public surface** so `main.rs`
and `climate_task` change only the module path (`mqtt_climate` → `http_climate`):

- Re-exports the same types: `ClimateStateMutex`, `EnergyStateMutex`,
  `ClimateCmdChannel`/`Receiver`/`Sender`, `CloseSignal`, `EnergyState`,
  `ClimateCmd`, `ObjId`, `OBJ_ID_CAP`.
- Same entry point signature:
  `run_climate_session(stack, state, energy, cmd_rx, close) -> Result<(), Error>`.
- **Config (baked, like `MQTT_BROKER`), configurable per `voice_stt::default_bridge_ip()`:**
  `HA_HTTP` = `option_env!("HA_HTTP")` default `"10.0.11.110:8124"`; a
  `default_ha_addr() -> Ipv4Address` + `HA_PORT` pair; optional `WATCH_HA_TOKEN`
  → `X-Watch-Token` header.
- **Poll loop** (`select3`): a poll timer (fires immediately on session open, then
  every ~2 s), `cmd_rx.receive()`, and `close.wait()`.
  - **timer** → `GET /watch/climate/state` (parse the array: split top-level
    objects, pull `"id"`, feed each object to `climate_model::parse_state`, upsert
    into `ClimateState`) + `GET /watch/energy` (reuse `parse_energy`; set
    `energy.online` from GET success/failure).
  - **cmd** → `POST /watch/climate/<obj>/set` with
    `climate_model::encode_set_temp`/`encode_set_mode` (unchanged); then poll soon
    to reconcile.
  - **close** → return `Ok(())`.
- Transient GET/POST failures **do not** end the session (HTTP is stateless — no
  connection to "lose"); the session returns only on `close`. This preserves the
  main-loop invariant that WiFi is held while a HA screen is open and mesh is
  restored on close (unchanged `climate_task`/lifecycle, oracle-t10 guarantee).
- **C4/C5/E2 preserved for free:** the optimistic + 400 ms-debounced + 5 s-revert
  logic lives in `main.rs`, which is untouched except the module path. The
  command-channel/reconcile flow is identical.

### Stack-guardrail impact (crash-critical, memory `c6-stack-geometry-and-esp-hal-guard`)

The v0.5.1 note flagged the MQTT session future's on-stack socket buffers as `.bss`
pressure. `http_climate` puts its HTTP rx/tx/parse buffers on the **heap** (`vec!`,
like `ota_http`) rather than in stack arrays, so the `climate_task` future shrinks →
`_bss_end` does not rise → the stack gap is **≥** the MQTT baseline. Dropping
`mqtt_climate.rs` also removes its inbound-frame statics. The boot-time
`gap ≥ 46 KB` assert stays the tripwire; the build report records the measured
`nm _stack_start − _bss_end`.

## Testing / rollout

- `crates/climate-model` host tests: **unchanged and still pass** (crate untouched).
- Component: unit-test the JSON builders + capability mapping against the recorded
  live entity attributes (Nest heat_cool, minisplit auto). Manual: `curl
  http://10.0.6.108:8124/watch/climate/state` (or the VLAN-11 leg from a roam host)
  once installed.
- On-glass (JP-gated): install component → open Climate on the watch → see the
  roster → adjust a setpoint → device responds → close → mesh recovers. Then Energy.

## Decisions for JP

- **D1 (primary transport):** component-owned plain-HTTP listener on
  `0.0.0.0:8124`, reachable at `10.0.11.110:8124` from roam. Verify the in-process
  bind is externally reachable on HAOS on first install (the one unproven point);
  fallback = ubox0 proxy. **Recommend D1.**
- **D2 (auth):** optional `X-Watch-Token` shared secret (stored in vault, baked via
  `WATCH_HA_TOKEN`) + VLAN-11 isolation. Recommend enabling the token.
- **D3 (`heat_cool` setpoint):** on `{"set":X}` for a dual-setpoint entity, set
  low/high around `X` preserving the current spread (default ±2°F). Alternative:
  reject setpoint on heat_cool and only allow mode. Recommend the spread approach.
- **D4 (`grid_w` export):** no whole-home CT clamp → export unmeasured; `grid_w` is
  import-only for now. Accept, or point `grid_w` at a different signed sensor if one
  exists.
- **D5 (entity exposure):** default all `climate.*`, exclude the two `*_mqtt_hvac`
  duplicates. Confirm the exclude list.
- **D6 (component home):** kept in the `esp32c6-watch` repo on `dream/http-climate`
  for a single coupled review; move to `~/Projects/ha` on merge if preferred.
```
