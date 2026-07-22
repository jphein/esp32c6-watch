# ESP32-C6 Watch — Home Assistant integration

A native HA custom integration (`esp32c6_watch`) that serves **plain HTTP** to
JP's ESP32-C6 watch, replacing the interim Node-RED + MQTT climate bridge. It
reads `climate.*` and energy entities natively via `hass.states` and calls
`climate.*` services directly — no broker, no flow, no retained topics. It also
exposes the watch as a **`media_player`** so HA automations and `tts.speak` can
play audio on it (see *Speaker / announcements* below).

## Why a dedicated port (not :8123)

HA serves **HTTPS on :8123** (`ssl_certificate` is set), and the watch has no
TLS. So the integration starts **its own `aiohttp` listener on a dedicated
plain-HTTP port (default `8124`)**, bound to `0.0.0.0` inside HA's event loop.
HA core is host-networked, so that socket answers on every VM leg — including
the **VLAN-11 leg `10.0.11.110`**, which is the same L2 as the watch's `roam`
network. The watch therefore reaches it same-subnet at:

```
http://10.0.11.110:8124/watch/...
```

The listener bypasses HA's TLS/auth. Mitigations: (a) VLAN-11 is firewalled off
the server LAN; (b) an optional `X-Watch-Token` shared-secret header the handlers
check with a constant-time compare; (c) the endpoints only expose `climate.*` +
a fixed energy summary — no general HA access.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET  | `/watch/climate/state` | Array of climate-model state objects (one per exposed entity, each with an `id`). |
| POST | `/watch/climate/<object_id>/set` | Body `{"set":<float>}` or `{"mode":"<hvac>"}`. |
| GET  | `/watch/energy` | `{"battery_pct","solar_w","grid_w","charging"}`. |
| GET  | `/watch/climate/roster` | Array of exposed object ids (informational). |
| GET  | `/watch/version` | `{"component","version","entities"}` liveness probe. |
| GET  | `/watch/announce/pending` | `{"pending":bool,"bytes":int}` — cheap poll for queued audio. |
| GET  | `/watch/announce` | Next queued clip as raw PCM (`application/octet-stream`), and dequeues it; `204` if empty. |

`heat_cool` is normalized to `auto` in the state output (mode + modes list). On a
`{"mode":"auto"}` command the handler is capability-aware: it uses the entity's
real `hvac_modes` — `auto` if supported, else `heat_cool`, else no-op. A
`{"set":X}` on a dual-setpoint (`heat_cool`) entity sets `target_temp_low`/`high`
around `X`, preserving the current spread (default ±2°F).

## Speaker / announcements

The integration also registers a **`media_player` entity — "ESP32-C6 Watch"** —
so any HA automation or `tts.speak` can play audio on the watch. It is a
**play-only announcer** that fits the watch's HTTP-*pull* model: HA renders the
audio, transcodes it, and queues it; the watch polls and drains the queue. HA
never pushes to the device (no open connection).

On `play_media` / `tts.speak`, the entity:

1. Resolves the media (TTS-proxy URL or a `media-source://` URI).
2. Transcodes it with HA's ffmpeg helper to **headerless 16 kHz mono 16-bit
   signed-LE PCM** — the exact format the watch feeds its I2S TX (same as its
   STT upload).
3. Enqueues the PCM on a **bounded FIFO** (capped by total bytes; oldest clips
   are dropped first when full, and drops are logged).

The watch then pulls it via the two announce endpoints (both behind the same
`X-Watch-Token` check as the other endpoints):

- `GET /watch/announce/pending` → `{"pending":bool,"bytes":int}` (cheap, poll often).
- `GET /watch/announce` → the next clip as raw PCM and **dequeues** it, or `204`
  when the queue is empty.

Example automation action:

```yaml
- service: tts.speak
  target:
    entity_id: tts.piper           # whichever TTS engine you use
  data:
    media_player_entity_id: media_player.esp32_c6_watch
    message: "Laundry is done."
```

Or play an arbitrary media file:

```yaml
- service: media_player.play_media
  target:
    entity_id: media_player.esp32_c6_watch
  data:
    media_content_type: music
    media_content_id: media-source://media_source/local/chime.mp3
```

The media_player reports `playing` while a clip is queued for the watch and
`idle` once drained.

## Configuration

All via the UI config flow (Settings → Devices & Services → Add Integration →
"ESP32-C6 Watch"). Single instance. Defaults are pre-filled from live HA
(2026-07-21):

| Field | Default |
|---|---|
| Port | `8124` |
| Token | *(empty — disabled)* |
| Speaker queue cap (bytes) | `2097152` (≈ 64 s of 16 kHz mono PCM) |
| Excluded climate object ids | `kitchen_mqtt_hvac, bedroom_mqtt_hvac` |
| Battery % sensor | `sensor.battery_average_soc` |
| Solar W sensor | `sensor.total_solar_power` |
| Grid W sensor | `sensor.solar_arbitrage_grid_draw` |
| Charging sensor | *(unset → `charging: false`)* |

All climate entities are exposed by default; the two `*_mqtt_hvac` duplicates
(mirrors of the minisplits) are excluded. New climate entities auto-appear on the
watch with zero firmware/component change. Options are editable after setup
(Configure); saving reloads the listener.

## Install

### Manual copy (simplest)

The HA SSH addon lacks an scp subsystem, so `cat … | ssh … sudo tee` each file:

```bash
DEST=/homeassistant/custom_components/esp32c6_watch
ssh jp@10.0.6.108 "sudo mkdir -p $DEST/translations"
for f in manifest.json const.py __init__.py api.py announce.py media_player.py config_flow.py strings.json README.md; do
  cat ha/custom_components/esp32c6_watch/$f | ssh jp@10.0.6.108 "sudo tee $DEST/$f > /dev/null"
done
cat ha/custom_components/esp32c6_watch/translations/en.json | \
  ssh jp@10.0.6.108 "sudo tee $DEST/translations/en.json > /dev/null"
```

Then restart HA and add the integration from the UI.

### HACS custom repository

HACS is installed on this HA. Add
`https://github.com/jphein/esp32c6-watch` as a custom repository (category
*Integration*), install, restart HA, then add the integration from the UI.

## Verify the port is reachable (JP-gated)

The one unproven assumption is that a socket bound by the HA-core process on
`0.0.0.0:8124` is externally reachable on the VLAN-11 leg. Expected (host
networking is why :8123 and the MQTT addon are reachable per-leg), but confirm
once from a roam-side host:

```bash
curl http://10.0.11.110:8124/watch/version
curl http://10.0.11.110:8124/watch/climate/state
# speaker: enqueue a clip from HA (tts.speak / play_media), then:
curl http://10.0.11.110:8124/watch/announce/pending          # {"pending":true,"bytes":…}
curl -o clip.pcm http://10.0.11.110:8124/watch/announce       # raw PCM, dequeues it
```

(If a token is set, add `-H "X-Watch-Token: <token>"` to each request.)

If it is *not* reachable, the design-doc fallback is a thin plain-HTTP→HTTPS
reverse proxy on ubox0's VLAN-11 leg (the STT-gateway pattern); see
`docs/superpowers/specs/2026-07-21-ha-watch-component.md`.
