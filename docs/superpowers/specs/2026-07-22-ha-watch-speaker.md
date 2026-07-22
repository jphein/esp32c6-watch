# ESP32-C6 Watch — HA Speaker (media_player + announce queue)

**Date:** 2026-07-22
**Status:** design + build for review (NOT deployed / NOT flashed / HA not restarted)
**Extends:** `2026-07-21-ha-watch-component.md` (the native plain-HTTP component)
**Branch:** `feat/ha-speaker` (off `dream/http-climate` @ `886e7d1`)

## Goal

Give HA automations and `tts.speak` a way to **play audio on the watch**, as part
of the existing `esp32c6_watch` custom component (JP's directive: the HA speaker is
part of the component, **not** a separate bridge). The watch already speaks plain
HTTP and *pulls* from HA (climate/energy). The speaker keeps that model: HA renders
+ queues audio, the watch polls + drains. No push, no persistent connection, no new
host.

## Decision: `media_player` entity (not a `notify` service)

**Chosen: a `media_player` entity ("ESP32-C6 Watch").** Why:

- `tts.speak` targets a `media_player_entity_id` and works **out of the box** — the
  TTS integration renders the message, resolves it to a proxy URL, and calls the
  entity's `async_play_media`. A `notify`-style service would force JP to wire TTS
  rendering by hand in every automation.
- `media_player.play_media` also lets automations play arbitrary media
  (`media-source://…`, local files) with no extra service surface.
- It presents as a normal HA device/entity (dashboards, assist satellite targeting
  via `MEDIA_ANNOUNCE`).

The only cost is that a `media_player` implies richer state semantics than a pull
announcer has. We handle that honestly: `supported_features = PLAY_MEDIA |
MEDIA_ANNOUNCE` only (no transport/volume controls we can't honor), and `state`
reports **PLAYING while audio is queued for the watch, IDLE once drained** — the
truthful proxy for "the watch has audio waiting," since we cannot observe the
device's actual playback (it pulls on its own schedule).

## Architecture

```
tts.speak / play_media
      │  async_play_media(media_id)
      ▼
WatchMediaPlayer (media_player.py)
      │  resolve media-source → URL → ffmpeg → 16k/mono/s16le PCM
      ▼
AnnounceQueue (announce.py)   ── bounded FIFO, drop-oldest, byte-capped
      ▲  get() / pending()
      │
GET /watch/announce[/pending] (api.py, same aiohttp app + token middleware + @_safe)
      ▲  HTTP pull
      │
   watch firmware (Morpheus-orchestrator's side — poller + I2S TX)
```

New files: `announce.py` (queue, stdlib-only, unit-testable), `media_player.py`
(platform + entity + ffmpeg transcode). Touched: `__init__.py` (create queue, wire
into app + `hass.data`, forward/unload the platform), `api.py` (two handlers +
`APP_QUEUE`), `const.py`, `config_flow.py`, `strings.json`/`en.json`, `manifest.json`
(deps `ffmpeg`, `media_source`; version → `0.2.0`), `README.md`.

## Endpoint contract (crisp — this is what the firmware implements against)

Both endpoints are on the **same** listener (default `10.0.11.110:8124`), behind the
**same optional `X-Watch-Token` header** as the climate/energy endpoints, and wrapped
by the same `@_safe` (any handler error → HTTP 500, listener survives).

### `GET /watch/announce/pending`

Cheap poll — no dequeue. Always `200`:

```json
{"pending": true, "bytes": 20480}
```

- `pending` — `true` iff at least one clip is queued.
- `bytes` — total bytes of PCM currently queued (sum over all queued clips).

### `GET /watch/announce`

Returns the **next** queued clip and **dequeues** it (FIFO, oldest first):

- `200 OK`, `Content-Type: application/octet-stream`, body = **raw headerless PCM**:
  **16 kHz, mono, signed 16-bit little-endian** — byte-identical to the watch's STT
  upload format, ready to feed the shared I2S TX. One HTTP response = one clip; the
  firmware should read to EOF / `Content-Length`.
- `204 No Content` when the queue is empty (no body).

**Firmware poll suggestion (contract-only, not prescriptive):** poll
`/announce/pending` on the existing HA poll cadence; when `pending` is true, `GET
/announce` and stream the body straight to I2S TX; repeat until `204` to drain a
backlog. Each `GET /announce` is exactly one clip — do not assume clip boundaries
within a single response.

## The queue (`AnnounceQueue`)

- Byte-bounded FIFO. Default cap **2 MiB** (`DEFAULT_MAX_QUEUE_BYTES`, ≈ 64 s of
  16 kHz mono PCM), configurable via the options flow (`max_queue_bytes`,
  32 KiB–16 MiB).
- **Overflow = drop oldest.** When a new clip would exceed the cap, the oldest clips
  are popped until it fits; every drop is logged (`warning`). A single clip larger
  than the whole cap is enqueued anyway (dropping it would silence the announcement)
  but flagged.
- An `asyncio.Lock` guards `put`/`get`; `pending()` is a cheap lock-free snapshot
  (safe on the single-threaded loop). An optional `on_change` hook lets the
  media_player refresh its HA state on enqueue/drain.

## Transcode

`media_player._async_transcode_to_pcm`: `async_get_clientsession(hass)` fetches the
resolved URL, pipes the bytes through **HA's ffmpeg** (`get_ffmpeg_manager(hass).binary`)
with `-f s16le -acodec pcm_s16le -ac 1 -ar 16000`, and returns stdout. 30 s timeout;
non-zero exit → `RuntimeError` (surfaces in the service call, queue untouched).
`media-source://` ids are resolved first; the final URL is normalized with
`async_process_play_media_url` so ffmpeg gets an absolute URL.

## Config additions

- `max_queue_bytes` (NumberSelector, default 2 MiB) — the only new option.
- **Not added:** a "default TTS engine/voice" option. In the pull/media_player model
  the caller of `tts.speak` chooses the engine; the component only receives
  already-rendered audio, so there is nowhere to consume such a default. Adding it
  would be unwired cruft. (Justification per the task's "justify whichever you pick.")
- PCM format (16k/mono/s16le) is a fixed constant, **not** an option — it must match
  the firmware's I2S TX exactly.

## Validation (HA not installed here)

- `python3 -m py_compile` — all 6 `.py` compile.
- `json.load` — `manifest.json`, `strings.json`, `translations/en.json` valid;
  `strings.json == en.json`.
- Manifest key order hassfest-clean (`domain`, `name`, rest alphabetical); deps sorted.
- Isolated logic harness (stubs aiohttp + HA, drives the **real** `AnnounceQueue` and
  the **real** `/announce` + `/pending` handlers): empty→`204`; enqueue→`pending`
  true + correct `bytes`; FIFO drain returns the right clip, octet-stream, and
  dequeues; empties back to `204`; drop-oldest keeps total ≤ cap and evicts the
  oldest; oversized single clip passes through; `on_change` fires on put+get; empty
  clip is a no-op. **All pass.**

## Needs on-glass / HA verification (assumptions)

1. **Import paths against the live HA version** — `async_process_play_media_url` &
   `MediaType` from `homeassistant.components.media_player`, `get_ffmpeg_manager` from
   `homeassistant.components.ffmpeg`, `media_source.async_resolve_media(hass, id,
   target)` (3-arg). These are stable public APIs but unverifiable offline; confirm on
   the target HA on install.
2. **ffmpeg transcode end-to-end** — that HA's bundled ffmpeg produces the exact PCM
   the watch's I2S TX expects (endianness/rate/mono). Verify by capturing one clip:
   `curl -o clip.pcm .../watch/announce` and inspecting length/rate, or playing it on
   glass.
3. **Endpoint reachability on the VLAN-11 leg** — same unproven point as the base
   component (host-networked bind reachable at `10.0.11.110:8124`); the announce
   endpoints share that fate.
4. **`tts.speak` round-trip** — target `media_player.esp32_c6_watch` from an
   automation and confirm a clip lands on `/announce/pending`.

## Out of scope (owned by the firmware side / orchestrator)

The watch-side announce poller and PCM playback through the shared I2S TX. This work
only defines and serves the endpoint contract above.
