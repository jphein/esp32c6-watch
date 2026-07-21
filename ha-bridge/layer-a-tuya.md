# Layer A — bring the 2 Tuya minisplits onto the watch

Goal: make the two Tuya-compatible minisplits controllable from Home Assistant as
`climate.*` entities. Once they exist as `climate.*`, the Node-RED bridge
(`climate-bridge.flow.json`) picks them up **with zero change** and they auto-appear
as cards on the watch — see [§3](#3-why-this-auto-appears-on-the-watch-verified).

**This doc is HA/hardware-side only** — steps + a config template *you* run against
your own HA/Tuya account. Nothing here touches the watch firmware or your live HA
automatically.

**The one blocking input (only you can provide):** your **Tuya account credentials** —
the Smart Life / Tuya Smart app account the minisplits are paired to, plus a (free)
Tuya IoT Platform developer account. Everything in §1 needs those; I can't run the
wizard for you. The rest of the doc is ready to go the moment you have the keys.

Recommended path: **LocalTuya** (local LAN control — no cloud latency, works if the
internet is down). Cloud-Tuya fallback in [§4](#4-fallback-official-tuya-cloud-integration).

---

## 1. Extract each unit's `device_id` + `local_key` (tinytuya wizard)

Tuya local control needs a per-device **`local_key`** (a secret the device and the
Tuya cloud share). It isn't shown in the app — you extract it once via the cloud API.

### 1a. Prerequisites (one-time, ~15 min)
1. The 2 minisplits are already paired in the **Smart Life** (or **Tuya Smart**) app.
   Note which app + which account — the wizard links to that exact account.
2. Create a free **Tuya IoT Platform** developer account: <https://iot.tuya.com>.
3. **Cloud → Create Cloud Project** (Development Method: *Smart Home*). Pick the **Data
   Center** that matches where your app account lives (US → *Western America*, EU →
   *Central Europe*, etc. — a mismatch = "no devices found" later). Record the project's
   **Access ID / Client ID** and **Access Secret / Client Secret** (Project → Overview →
   Authorization Key).
4. In the project: **Devices → Link Tuya App Account → Add App Account** → scan the QR
   with the Smart Life app (Me → ⚙/settings → the scan icon). This imports your paired
   devices (the minisplits) into the project.
5. **Service API → Go to Authorize** → ensure these APIs are subscribed to the project
   (all have a free tier): **IoT Core**, **Authorization**, and **Smart Home Basic
   Service**. (Without IoT Core + Authorization the wizard 401s.)

### 1b. Run the wizard
```bash
python3 -m pip install --user tinytuya       # or: pipx install tinytuya
python3 -m tinytuya wizard
```
It prompts for:
- **API Key** = the project's Access ID (step 3)
- **API Secret** = the Access Secret (step 3)
- **API Region** = `us` / `eu` / `cn` / `in` (match the Data Center from step 3)
- **Device ID** = any one device id from a linked device (pick a minisplit from the
  project's Devices list — used only to bootstrap the pull)

The wizard then writes **`devices.json`** (and `snapshot.json`) in the current dir with,
for **every** linked device:
```json
{
  "name": "Bedroom Minisplit",
  "id":   "bfa1b2c3d4e5f6...",     ← device_id
  "key":  "0123456789abcdef",       ← local_key  (THE secret you need)
  "mac":  "aa:bb:cc:dd:ee:ff",
  "ip":   "10.0.x.y",               ← may be blank; get it from §1c or DHCP
  "ver":  "3.4"                     ← PROTOCOL VERSION — note it (see gotcha below)
}
```
Grab the `id`, `key`, and `ver` for **both** minisplits.

### 1c. Find each unit's LAN IP + confirm it's reachable
```bash
python3 -m tinytuya scan            # LAN broadcast scan → IP + id + protocol per device
```
- **Give each minisplit a DHCP reservation** on the router (per homelab practice) so the
  IP is stable — LocalTuya binds to a fixed IP.
- Local Tuya control is **UDP 6666/6667** (discovery) + **TCP 6668** (control). The HA VM
  must be able to reach the minisplits on those ports. If the minisplits sit on the **IoT
  VLAN** and HA is on VLAN6, add a firewall allow (HA-VM leg → minisplit IPs:6668/tcp +
  6666-6667/udp) or reserve them on a VLAN the HA VM already routes to.

### ⚠️ Gotchas
- **Protocol 3.4 / 3.5** (the wizard's `ver`): these are the newer Tuya local protocols.
  **Use the maintained LocalTuya fork [`xZetsubou/hass-localtuya`](https://github.com/xZetsubou/hass-localtuya)** (HACS) — the original `rospogrigio/localtuya` is stale and does **not** speak 3.4/3.5. Install that fork via HACS → custom repo. (3.1/3.3 devices work on either, but the xZetsubou fork is the safe default.)
- **`local_key` rotates on re-pair.** If you ever delete + re-add a minisplit in the app,
  re-run the wizard and update the key in LocalTuya.
- Keep `devices.json` **private** — the `key` is a device secret (treat like a password;
  don't commit it). Consider stashing the two keys in Vaultwarden.

---

## 2. LocalTuya climate config template

Install **`xZetsubou/hass-localtuya`** via HACS, restart HA, then **Settings →
Devices & Services → Add Integration → LocalTuya → Add a new device**. Per minisplit:

1. Enter **Host** (the reserved IP), **Device ID**, **Local key**, **Protocol version**
   (from §1). LocalTuya connects and shows the device's **live DPS** (a table of
   `dp_id: value`).
2. **Add entity → Platform: `climate`.** Map the DPS below.

### The DPS map (standard Tuya AC/minisplit schema — CONFIRM per device)
DP numbers vary by vendor. The template below is the *common* Tuya AC layout; **confirm
each one** by toggling that control in the Smart Life app and watching which `dp_id`
changes in LocalTuya's live-DPS table during setup.

| LocalTuya climate field | Typical DP | Meaning | Notes |
|---|---|---|---|
| **ID** (power/on-off) | `1` | on/off (bool) | LocalTuya uses this as the entity's on/off |
| **Target Temperature** | `2` | setpoint | see *precision* below |
| **Current Temperature** | `3` | room temp | read-only |
| **HVAC Mode** | `4` | enum: `auto` / `cold` / `hot` / `wind` / `wet` | mapped → HA modes |
| **Fan speed** | `5` (or `28`) | enum: `auto`/`low`/`mid`/`high` | optional |
| **HVAC Action** | — | many Tuya ACs don't report a separate action | leave unset if absent |

Key LocalTuya climate settings:
- **Temperature step:** `1.0` (or `0.5` if the unit supports halves).
- **Min / Max temperature:** e.g. `16` / `31` (°C) — match the unit's range.
- **Precision / Current-temperature precision:** if the DP sends **tenths** (e.g. `225`
  = 22.5°), set precision `0.1` (divide-by-10). If it sends whole degrees, `1.0`. The
  live-DPS value in setup tells you which (a value like `225` for a ~22° room ⇒ tenths).
- **HVAC mode set (enum → HA mode):**
  ```
  auto → auto        (HA: heat_cool/auto)
  cold → cool
  hot  → heat
  wind → fan_only
  wet  → dry
  ```
  (Only include the modes your unit actually exposes — the wizard's device schema or the
  live enum values list them. HA's supported-modes drives the watch's mode chips.)
- **Off:** LocalTuya turns the entity off via the power DP (`1 → false`); "off" then
  appears in `hvac_modes` automatically.

### YAML form (xZetsubou fork supports YAML as well as the UI)
The UI writes this into `.storage`; the equivalent YAML (if you prefer config-as-code)
is roughly:
```yaml
# configuration.yaml (xZetsubou/hass-localtuya)
localtuya:
  - host: 10.0.8.x            # reserved IP of minisplit #1
    device_id: bfa1b2c3d4e5f6...
    local_key: "0123456789abcdef"
    protocol_version: "3.4"
    friendly_name: Bedroom Minisplit
    entities:
      - platform: climate
        friendly_name: Bedroom Minisplit
        id: 1                 # power on/off DP
        target_temperature_dp: 2
        current_temperature_dp: 3
        temperature_step: 1.0
        min_temperature: 16
        max_temperature: 31
        precision: 1.0        # 0.1 if the DP is in tenths
        target_precision: 1.0
        hvac_mode_dp: 4
        hvac_mode_set:
          auto: auto
          cool: cold
          heat: hot
          fan_only: wind
          dry: wet
        fan_speed_dp: 5       # omit if not present
        fan_speed_list: [auto, low, mid, high]
  # …repeat the block for minisplit #2…
```
> Prefer the **UI flow** for the first setup (it shows live DPS so you can confirm the
> mapping visually), then optionally export to YAML. Whichever you use, the result is the
> same: **two `climate.*` entities.**

**Result:** `climate.bedroom_minisplit` + `climate.<other>_minisplit` (object_ids from the
friendly names / entity settings). Rename in HA if you want cleaner ids — the watch just
shows whatever `name` the bridge publishes.

---

## 3. Why this auto-appears on the watch (verified)

No bridge or firmware change is needed — confirmed against the actual flow:

- `climate-bridge.flow.json`'s **`climate.* changed`** node filters `entityId:
  "^climate\\..*$"` (a **domain-wide regex**, `entityIdType: regex`) with *output on
  connect* → the instant the two `climate.tuya_*` entities exist, HA state changes fan
  through it and publish **retained** `watch/climate/<object_id>/state` (per the
  README topic contract).
- The command path is **domain-agnostic**: `parse cmd` emits a `climate.set_*` call from
  `msg.payload` (the `climate.set_* (from msg.payload)` node has blank domain/service and
  reads them from the message) — so `set`/`mode` from the watch drive the Tuya units the
  same as any thermostat.
- Because state is retained + the watch subscribes `watch/climate/+/state`, the two new
  entities become **two new cards** with **zero** watch/bridge edit (README: "any new
  `climate.*` entity auto-appears on the watch").

### Verify it worked
After LocalTuya adds the entities (and re-deploy / restart the Node-RED flow so
*output-on-connect* re-seeds, or just wait for the next state change):
```bash
# expect a retained state line for EACH minisplit within a few seconds:
mosquitto_sub -h 10.0.6.11 -u jp -P '<mqtt pw>' -t 'watch/climate/#' -v
#   watch/climate/bedroom_minisplit/state {"name":"Bedroom Minisplit","cur":22.5,"set":24,"mode":"cool",...}

# drive one by hand (setpoint then mode):
mosquitto_pub -h 10.0.6.11 -u jp -P '<mqtt pw>' -t 'watch/climate/bedroom_minisplit/set' -m '{"set":23}'
mosquitto_pub -h 10.0.6.11 -u jp -P '<mqtt pw>' -t 'watch/climate/bedroom_minisplit/set' -m '{"mode":"cool"}'
# → the minisplit's setpoint/mode change in HA (and on the watch, once Layer B firmware ships).
```
(Per the `ha` skill's per-VLAN-leg rule: `10.0.6.11` is the broker address the bridge/watch
use; the HA-VM Mosquitto also answers on `10.0.6.108`. If a client is on VLAN8/11, target
that VLAN's leg. The bridge + watch are on the VLAN6 path.)

---

## 4. Fallback: official Tuya cloud integration

If a `local_key` proves painful (e.g. the device won't hand it out, or protocol issues),
HA's built-in **Tuya** integration (Settings → Integrations → Tuya, sign in with the Tuya
IoT project + Smart Life account) exposes the same minisplits as `climate.*` — **also
picked up by the bridge unchanged**. Trade-off: cloud round-trip latency + it needs
internet. LocalTuya is preferred for a wrist remote (snappy, offline-tolerant); keep this
as the escape hatch.

---

## Checklist
- [ ] Tuya IoT dev account + Cloud project (Smart Home), Data Center matches app region
- [ ] App account linked; IoT Core + Authorization + Smart Home Basic subscribed
- [ ] `tinytuya wizard` → `id` + `key` + `ver` for both minisplits
- [ ] DHCP reservations for both; HA-VM can reach them on TCP 6668 / UDP 6666-6667
- [ ] LocalTuya (xZetsubou fork if `ver` is 3.4/3.5) → 2 climate entities, DPS confirmed via live values
- [ ] `mosquitto_sub -t 'watch/climate/#'` shows 2 new retained state lines
- [ ] (later) Layer B firmware → the 2 cards render + control on the watch
```
