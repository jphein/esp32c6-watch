# Speak-the-Notification — TTS Read-Aloud Design Spec

**Date:** 2026-07-27
**Author / implementer:** Morpheus
**Branch:** `fix/climate-oom-chime-voicing` (design), implementation branch TBD
**Status:** design — validated against live Azure before a line of firmware was written
**Goal (orchestrator):** read notifications aloud through a TTS pipeline, reusing "the azure dragon
voice" from the existing STT integration.

---

## 0. TL;DR for the reviewer

1. **No Azure credentials go on the watch, and none are invented.** The existing STT path already
   proxies through a LAN bridge that holds the key. TTS adds one route to that same bridge.
2. **Azure emits `raw-16khz-16bit-mono-pcm` for the DragonHD voice — measured, 200 OK.** That is
   byte-for-byte the format `audio_out::play_pcm` already eats. **There is no decode step on the
   watch.** No MP3, no Opus, no resampler.
3. **Azure's own stream stalls for up to 782 ms between chunks (measured).** The watch can bridge
   64 ms. So the bridge fully synthesizes, *then* streams at LAN line rate. All jitter is absorbed by
   the host with 32 GB of RAM, not the one with 186 KB.
4. **The utterance is never held in memory anywhere on the watch.** It streams through a 512 B
   window with backpressure from the existing 8-slot playback channel. Peak new cost: **≈2.4 KB**.
5. **Zero new Embassy tasks.** Driven from the main loop, precedent being the STT push-to-talk flow.
6. **A trap the brief did not list, found during study and designed around: the amp-gate deadlock**
   (§6.2). Left unhandled, TTS would have shipped *silent while logging complete success* — the exact
   failure signature that already burned the ping chime.

---

## 1. What already exists (and why TTS is mostly a mirror of it)

### 1.1 The STT path — the model to follow

`src/net/voice_stt.rs` does **not** talk to Azure. It talks to a LAN bridge:

```
watch ──plain HTTP, no TLS, no DNS──> 10.0.11.11:8090  (ubox0, VLAN 11)
        POST /stt  Transfer-Encoding: chunked
        body: raw mono 16 kHz s16le PCM
        <── 200 {"text": "..."}
```

The bridge is `~/Projects/speech-to-cli/watch_bridge.py`. It imports speech-to-cli's own `speech.py`
and `state.load_config()`, holds the Azure key, and does the HTTPS hop. **The watch never sees a
secret.** `systemctl is-active watch-bridge` on ubox0 → `active`.

This shape exists because the watch is plain-HTTP-only. That constraint has not changed, so TTS
inherits the same shape — reversed in direction.

### 1.2 The dragon voice — located, confirmed, not invented

From `~/.config/speech-to-cli/config.json` (identical on familiar and ubox0). These are
configuration values, not secrets; the key itself was never read or printed:

| key | value |
|---|---|
| `voice` | **`en-US-Ava:DragonHDLatestNeural`** ← the dragon voice |
| `fast_voice` | `en-US-AvaNeural` |
| `region` (STT) | `westus` |
| `tts_region` | **`eastus`** — DragonHD is region-limited; speech-to-cli already splits this |
| `key` | set (84 chars) |
| `tts_key` | null → falls back to `key` |

The request shape is lifted verbatim from `speech_tts.py::_prepare_tts`, so the bridge reuses JP's
existing account, endpoint, region-split and voice selection rather than defining a parallel one.
**If JP changes `voice` in the speech-to-cli config, the watch follows automatically** — that is the
point of routing through the bridge rather than hardcoding.

---

## 2. Measurements that drove the design

All run live against Azure from `familiar`, 2026-07-27.

### 2.1 Output format — can we skip decoding entirely?

```
raw-16khz-16bit-mono-pcm    200    86,400 B    2.70 s audio    1.91 s wall
raw-24khz-16bit-mono-pcm    200   124,800 B    2.60 s audio    2.11 s wall
```

**Yes.** DragonHD honours a 16 kHz raw PCM request. This is the highest-leverage result in the spec:
it deletes an entire subsystem. Contrast the ping chime, which is stored at 8 kHz and must be
zero-order-hold doubled inside `next_long_chunk` — TTS needs none of that. Bytes off the socket go
straight into the playback channel.

Working constant for everything below: **16 kHz mono s16le = 32,000 B/s = 32 B/ms**, and
`PLAY_CHUNK` (512 B) = **16 ms**.

### 2.2 Streaming jitter — can we relay Azure straight through?

| text | chars | audio | TTFB | full synth | **max inter-chunk gap** | synth vs realtime |
|---|---|---|---|---|---|---|
| short | 22 | 1.72 s | 1.32 s | 1.85 s | **255 ms** | −0.13 s |
| typical notify | 67 | 4.25 s | 1.15 s | 2.71 s | **542 ms** | +1.54 s |
| long notify | 179 | 10.45 s | 1.19 s | 5.62 s | **782 ms** | +4.83 s |

**No.** The watch's underrun tolerance is `TAIL_STEREO_BYTES` = one ring + one descriptor ≈ **64 ms**,
behind a **128 ms** queue. Azure stalls 4–12× longer than that, on every utterance.

A naive relay would therefore: drain the queue → tail expires → `PLAYBACK_ACTIVE` and `AMP_REQUEST`
drop → amp powers down → next chunk opens a *brand-new* session that waits on `AMP_READY` again.
Audible result: chopped speech with a pop at every seam. It would have read as a firmware bug and
cost a debugging cycle.

Two further observations:
- **TTFB is ~1.2 s regardless of length** — it is HD-voice warm-up, not per-character cost. So
  buffering the whole utterance adds far less than it appears to.
- **Full synthesis beats realtime** for anything ≥ ~2 s of audio (+1.54 s, +4.83 s of margin). The
  bridge finishes writing before the watch finishes playing.

**Decision: the bridge synthesizes fully, then streams.** Variance moves to the host that can absorb
it. This is the central architectural choice of the design.

---

## 3. Architecture

```
  notification (notify::push)
        │
        │  trigger policy (§7)
        ▼
  main loop ── voice_tts::speak_text() ──────────────────────────┐
        │                                                        │
        │  POST /tts {"text":"..."}   plain HTTP, 10.0.11.11:8090 │
        ▼                                                        │
  watch_bridge.py (ubox0)                                        │
        │  1. compose SSML (escaped)                             │
        │  2. Azure TTS, raw-16khz-16bit-mono-pcm  [FULL buffer]  │
        │  3. reply 200, Content-Length: N, body = raw PCM        │
        ▼                                                        │
  socket ──512 B reads──> PLAYBACK.send().await ─────────────────┘
                                │   ▲
                                │   └── backpressure: 8 slots = 128 ms
                                ▼
                    PlaybackFeeder::fill_stereo  (silent_clock_task)
                                │  mono → stereo, 16 kHz TX ring
                                ▼
                          ES8311 → amp (GPIO6) → speaker
```

The watch holds, at any instant, one 512 B chunk plus whatever is in the 8-slot channel. **A 10 s
utterance (334 KB) flows through ~2.5 KB of RAM.**

### 3.1 Why not the `LONG_CLIP` path

`LONG_CLIP` streams from a `&'static [u8]` holding the *entire* clip. A typical utterance is
**136 KB**; the long one **334 KB**. Main heap is 186 KB total and Slint already lives in it.
**`LONG_CLIP` is categorically unusable for TTS** — this is arithmetic, not preference. It stays as
it is, serving the 22 KB ping chime it was built for.

### 3.2 Why the channel, not a static buffer — the wake-up trap, restated

`silent_clock_task` idles in `select(audio_out::next_clip(), rearm_requested())`, i.e. parked on
`PLAYBACK.receive()`. **Only a chunk arriving through the channel opens a playback session.** Arming
a buffer alone leaves the task asleep and the speaker silent — the bug that already cost hours.

TTS pushes **every** chunk through the channel, so it is immune by construction. Stating it here so
that a future optimization pass does not "improve" it into the LONG_CLIP shape and re-trip the trap.

---

## 4. Bridge contract — `POST /tts`

New route in `watch_bridge.py`, alongside `/stt`. Same port, same process, same config.

**Request** (watch → bridge):

```
POST /tts HTTP/1.1
Host: 10.0.11.11:8090
Content-Type: application/json
Content-Length: <n>
Connection: close

{"text":"Home Assistant. Garage door left open."}
```

`text` is capped at **400 chars** bridge-side (watch sends ≤ ~200; see §5.2). Optional `"voice"`
override is accepted but the watch does not send one — it inherits JP's configured dragon voice.

**Response** (bridge → watch), success:

```
HTTP/1.1 200 OK
Content-Type: application/octet-stream
Content-Length: <pcm bytes>          ← exact; watch uses it for progress + sanity cap
X-Audio-Format: raw-16khz-16bit-mono-pcm
Connection: close

<raw mono 16 kHz s16le PCM>
```

**Errors** mirror `/stt` exactly: `400 {"error":...}` bad/empty text, `502 {"error":"azure: ..."}`
upstream failure. The watch surfaces the string, same as STT does today.

`GET /health` extends to report TTS readiness: `{"ok":true,"region":...,"tts_region":...,"voice":...}`.

### 4.1 Bridge-side responsibilities (deliberately pushed off the watch)

1. **Full synthesis before the first byte** — §2.2.
2. **XML escaping / SSML composition.** Notification bodies arrive from MQTT and are attacker-
   influenced. Escaping `& < > "` is a real injection surface and it belongs on the side that has a
   real XML library. (The watch *also* sanitizes — defence in depth, §5.2.)
3. **`Content-Length`** so the watch can bound the transfer and drive a progress indicator.
4. **Hard duration cap** — reject/truncate above ~30 s of audio so a malformed notification cannot
   hold the amp up indefinitely.

### 4.2 Vendored (was: standing risk)

`watch_bridge.py` used to exist **only** on ubox0 and familiar — in no repository at all, while two
shipped firmware features depended on it. A host rebuild would have taken the watch's voice with it.

Now vendored at **`tools/watch-bridge/`** (approved by the orchestrator): the patched bridge, a
`deploy.sh` that backs up → deploys → restarts → health-checks with **automatic rollback**, and a
README. Both hosts' copies were verified byte-identical (`md5` match) before vendoring, so there is
no fork to reconcile.

> **⚠️ 2026-08-14 — read the paragraph above narrowly; it was true and still misled.**
> "Both hosts' copies are byte-identical" was a claim about **host ↔ host**. It says nothing about
> **repo ↔ host**, and for 17 days it was quietly read as if it did. `#68` then added `POST /tts`
> *to the vendored copy only*: `deploy.sh` was never run, so the hosts stayed on the 134-line
> STT-only build and `/tts` 404'd on both while `#11` sat closed as shipped.
>
> **Vendoring is not deploying.** A repo that vendors a live artifact acquires a second, invisible
> obligation — keeping the hosts caught up — and nothing in the commit that adds a feature reminds
> you of it. The parity assertion above is what stopped anyone from checking: it reads like an
> all-clear, so no one ran the one command that would have shown the truth.
>
> **`./deploy.sh --host <h> --dry-run` is the staleness detector.** It diffs repo↔host and touches
> nothing. Run it before believing any claim in this section; treat *this* sentence, not the
> paragraph above, as the current guidance.
>
> Reconciled and deployed 2026-08-14 — both hosts now serve `/stt` **and** `/tts` at
> `4e0a191e689daea13768fe75d3e4f402`, verified by a TTS→STT round trip on each
> (`"the quick brown fox…"` synthesized, posted back, transcribed verbatim). The 134-line
> predecessors are preserved on-host as `watch_bridge.py.bak-2026-08-14`.

**No secret is vendored, and none ever should be.** The bridge still reads the Azure key from
`~/.config/speech-to-cli/config.json` *on the bridge host at runtime*; `deploy.sh` refuses to ship a
file that looks like it picked up a credential.

---

## 5. Firmware modules

### 5.1 `crates/tts-proto` — new host-tested no_std crate

Follows the `crates/mic-dsp` / `crates/climate-model` pattern exactly: plain `no_std` lib, workspace
member via the existing `members = [".", "crates/*"]`, real `tests/`. esp-hal cannot build on host,
so anything worth testing must live here rather than in `src/`.

Pure logic, all host-tested:

| function | why it is worth testing |
|---|---|
| `compose_utterance(source, title, body, &mut buf)` | title/body join, source prefix ("Home Assistant."), cap enforcement, sentence punctuation |
| `sanitize_speech_text` | strip/replace chars that break JSON or SSML; ASCII-clamp mirroring `notify::sanitize` |
| `encode_json_request(text, &mut buf)` | JSON string escaping (`"`, `\`, control chars) — hand-rolled, so it gets real tests |
| `parse_response_head(&[u8])` | status code + `Content-Length` + header/body split; partial-header handling |
| `chunk_ms(bytes)` / `bytes_for_ms(ms)` | the 32 B/ms math used by every budget decision |

Round-trip and adversarial tests: MQTT-shaped payloads containing `"`, `\`, `<`, `&`, emoji, a
128-char maximal notification, truncated headers, missing `Content-Length`, non-200 statuses.

### 5.2 `src/net/voice_tts.rs` — the streaming client

Deliberately parallel to `voice_stt.rs` (same error style `&'static str`, same dotted-quad + plain
HTTP, same bridge IP/port constants re-exported from `voice_stt` so there is one source of truth).

```rust
/// Speak `text` through the bridge, streaming PCM into the playback channel.
/// Runs ON THE MAIN LOOP (no new task). `amp`/`codec` are pumped through
/// service_amp each iteration — see §6.2, this is load-bearing.
pub async fn speak_text<I: I2c>(
    stack: Stack<'static>,
    text: &str,
    amp: &mut Output<'static>,
    codec: &mut Es8311<I>,
    should_stop: &mut dyn FnMut() -> bool,
) -> Result<Spoken, Error>;
```

`should_stop` is `dyn` rather than generic on purpose: one instantiation instead of one per closure
type. (The original reason was "this binary is out of ROM (§6.7)", which is no longer true — ROM is
51.2 % used since #67. The `dyn` choice stands on its own: one instantiation is the right shape for a
callback that has exactly one caller, and it is why `get_json` is non-generic too, worth 5,560 B.)

Loop shape:

1. Connect, send the JSON request head + body.
2. Read + parse the response head (`tts_proto::parse_response_head`).
3. **Stream loop**, until `Content-Length` consumed:
   - `socket.read(&mut pcm[..512])`
   - `audio_out::push_chunk(&pcm[..n]).await`  ← **awaits queue space; this is the backpressure**
   - `audio_out::service_amp(amp, codec)`      ← **§6.2, every iteration**
   - check `cancel()` (screen-off / user dismiss / app change) → abort cleanly
4. Return bytes played.

Because the send *awaits*, the watch reads from TCP only as fast as the speaker consumes. The TCP
receive window then applies the same backpressure upstream to the bridge for free. No rate math, no
timers, no drift — the speaker's own clock paces the whole pipeline.

### 5.3 `src/peripherals/audio_out.rs` — one small seam addition

The module currently exposes only the **synchronous, truncating** `play_pcm`. It needs an awaiting
counterpart. Notably, `audio_out.rs` already carries an **orphaned doc comment** (lines ~159–172)
describing exactly this function — "Queue an ENTIRE clip, awaiting queue space so an arbitrarily long
clip plays in full" — left behind when that approach was replaced by `LONG_CLIP` for the chime. The
comment now sits incorrectly attached to `struct LongClip`. This design **restores the function the
comment describes** (and re-attaches the comment):

```rust
/// Queue one chunk, AWAITING space. Backpressure seam for streamed sources
/// (#TTS). Unlike `play_pcm`, cannot truncate. Raises the mic/amp gates on the
/// first chunk exactly as `play_pcm` does (order matters: mic suppressed before
/// the first sample can reach the speaker).
pub async fn push_chunk(pcm: &[u8]) { ... }

/// True if a session we opened was aborted underneath us (feeder.abort()).
pub fn session_aborted() -> bool { ... }
```

Why the chime's dedicated-task approach is **not** reused: that task is what caused the
100 %-reproducible `Instruction access fault mepc=0x2`. `push_chunk` needs no task — the caller is
already an async context (the main loop).

`PLAY_QUEUE_DEPTH` stays at **8**. §2.2's bridge-side buffering removes the reason to grow it, and
growing it would add `.bss` against the stack gap for no benefit.

### 5.4 `src/peripherals/config.rs` — the setting

Bump `SWCFG6` → **`SWCFG7`**, adding one byte:

```rust
/// Read notifications aloud (#TTS). 0 = Off, 1 = OnDemand (default), 2 = Auto.
pub speak_notifications: u8,
```

V6 configs migrate forward with the default `1` (OnDemand), so existing watches keep working and
nothing starts talking unprompted after an OTA. Volume/mute already exist (`volume`, `muted` →
`MASTER_VOL_REG`) and are honoured for free: a muted watch runs the amp cycle but the codec is
silent, so mute needs **no new code**.

---

## 6. The hazards, and how each is neutralized

### 6.1 Memory — the binding constraint

Everything the change adds, in bytes:

| item | bytes | where | note |
|---|---|---|---|
| socket RX buffer | 1024 | main task future (.bss) | same as `voice_stt` |
| socket TX buffer | 512 | main task future (.bss) | request is small; STT's 1024 halved |
| PCM staging buffer | 512 | main task future (.bss) | one `PLAY_CHUNK` |
| request/compose buffers | ~320 | main task future (.bss) | 200-char text + JSON escape headroom |
| `speak_notifications` | 1 | flash config | + `WatchConfig` field |
| `session_aborted` atomic | 1 | .bss | |
| **total new `.bss`** | **≈2.4 KB** | | |

**Heap: zero.** No allocation on the TTS path — no `format!`, no `alloc::` anywhere (note
`voice_stt` uses `format!` for its head; TTS composes into a `heapless::String` instead, which is
strictly better and avoids touching the 186 KB pool at all).

### 6.1.1 MEASURED (2026-07-27, against `9363d52`) — the estimate was 6× pessimistic

| | baseline | with `tts` | delta |
|---|---|---|---|
| `_bss_end` | `0x4085c068` | `0x4085c208` | **+416 B** |
| stack gap | 75,176 B (73.4 KB) | 74,760 B (73.0 KB) | −416 B |
| vs `STACK_FLOOR` (71,680 B) | +3,496 B | **+3,080 B — PASSES** | |
| ROM free (4 MiB region) | 6,952 B | **−3,024 B — OVERFLOWS** | −9,976 B |

> **Superseded 2026-07-29** — the region is 6 MiB now (#67); ROM is 51.2 % used
> with ~3.07 MB free, so this row no longer describes the build.

**RAM is a non-issue: +416 B, not the ~2.4 KB budgeted.** rustc *did* overlap `speak_text`'s frame
with `stream_utterance`'s — they sit in disjoint branches of the same main-loop generator and never
run concurrently. §6.1 explicitly refused to rely on that; it turned out to hold, and the pessimistic
budget simply had slack. The boot assert passes with 3 KB to spare.

**ROM is the real constraint, and it is not this feature's fault** — see §6.7.

With `tts` compiled OUT (the shipped default), the residue is **136 B of `.bss` and 116 B of ROM**:
the always-compiled `SpeakMode`, `ButtonAction::Speak`, and the v7 config field. The config record
format is deliberately feature-INDEPENDENT so a `tts` and a non-`tts` build read and write byte-
identical flash records.

### 6.7 The firmware WAS out of ROM — resolved 2026-07-29, this section is history

> **SUPERSEDED.** `build.rs::widen_rom_region` (#67) raised the region from the 4 MiB
> esp-hal hardcodes to the 6 MiB `partitions.csv` already reserves per OTA slot. ROM is
> now **51.2 % used with ~3.07 MB free**, so nothing below is a live constraint.
>
> The remaining cost of `tts` is **328 B of stack**, measured: default 80,592 B gap
> (+8,912 over the 71,680 floor) vs `tts` 80,264 B (+8,584). `story,tts` is 74,904 B
> (+3,224). The feature is one line from ON and the only reason left is a product
> decision — it makes the watch speak notifications aloud unprompted.
>
> Kept rather than deleted because §6.7's *reasoning* is still the reference for how the
> ROM ceiling was diagnosed, and because "the firmware is out of ROM" was cited as
> settled fact in four places for two days after it stopped being true.

### 6.7 (historical) The firmware is out of ROM (project-level, pre-existing)

`esp-hal` hardcodes a **4 MiB** ROM region for the C6 in `ld/esp32c6/memory.x`
(`LENGTH = 0x400000 - 0x20`). At `9363d52` the binary had **6,952 bytes free — 0.17 %**. This
feature needs ~9.9 KB, so it overflows by ~3.0 KB.

This is not a TTS problem. **No feature of meaningful size can land on this firmware right now.**
The release profile is already `opt-level = 's'` + `lto = 'fat'` + `codegen-units = 1`, so there is
no easy lever left; removing `core::fmt` uses from the TTS path recovered only 286 B, because
`voice_stt`'s `format!` already links it.

**The fix, and why it is legitimate rather than a hack:** `partitions.csv` already reserves
**6 MiB per OTA slot** ("6MB A/B slots since #50"), and the C6's ROM MMU window is
`[0x42000000, 0x42800000)` = 8 MiB. The linker script is simply under-declaring the space the
project's own partition table has already set aside. Raising the region to 6 MiB was tested:

> patched `LENGTH` to `0x600000 - 0x20` in the build tree → **links clean, 2,094,128 B to spare.**

It is a **flash-layout change only** — it does not move `_bss_end` or the stack, so it is *not* in
the #65 crash class. But it does need on-glass verification (boot + an OTA cycle) that this task is
not permitted to do, and overriding esp-hal's generated `memory.x` needs a real mechanism (its
build script emits its `-L` before ours, so a plain `rustc-link-search` will not win).

**Therefore `tts` ships as a default-OFF cargo feature** — the same discipline as
`audio_out::CHIME_ENABLED` ("gated behind one flag instead of reverted"). The tree stays green, the
work stays reviewable and host-tested, and the feature turns on in one line once the ROM budget
exists. Raising the ROM region is a separate, larger decision that belongs to JP.

**PLAY_QUEUE_DEPTH unchanged**, so no new channel storage.

Two things to verify empirically rather than assume, both called out as build gates in §8:
- `async fn main` is an Embassy task, so **locals in `speak_text` land in `.bss`, not on the
  stack** — they raise `_bss_end` and *steal* from the stack gap. That is precisely the #65 blast
  radius, so the boot-time `[STACK] gap = N B` print is the acceptance criterion, not a guess.
- Rust *may* overlap `speak_text`'s frame with `stream_utterance`'s (they are in disjoint branches
  and never run concurrently), in which case the real cost is ~0. **Not relied upon.** Budget above
  assumes no overlap — the pessimistic case.

Against a ≥70 KB floor with 2.4 KB of pessimistic growth, this is expected to pass comfortably. If it
does not, the documented remedy applies: trim the main `heap_allocator!` to grow the stack.

### 6.2 The amp-gate deadlock — the trap this design exists to avoid

**Not in the brief. Found by reading `gate_open()` against the STT call site.**

`PlaybackFeeder::gate_open()` holds every sample until `AMP_READY`. `AMP_READY` is set **only** by
`audio_out::service_amp`. `service_amp` is called **only from the main loop** — it needs `&mut
Output` (amp GPIO) and `&mut Es8311` (I2C), which the main loop owns.

The STT flow parks the main loop for the entire utterance
(`join(stream_utterance, monitor).await`, `main.rs:3829`) and gets away with it **because STT never
plays audio.**

If TTS is driven the same way, `service_amp` never runs while we are streaming. `AMP_READY` never
rises. Every chunk stalls behind the `AMP_WAIT_MS = 1000` failsafe, which then drains the queue
**into a muted DAC**.

The symptom would be: **"TTS is silent, but the log says every byte streamed."** That is the same
false-success signature that already burned the ping chime — where telemetry was added precisely
because "the first rework shipped silent and the console's `ok chime` ack proved nothing."

**Neutralized** by passing `amp`/`codec` into `speak_text` and calling `service_amp` on every
iteration of the stream loop. This is not a workaround; it is the pattern `service_amp`'s own doc
comment already prescribes ("plus inline right after each `play_pcm` call site for same-tick raise").

### 6.3 No new tasks

Adding one Embassy task previously produced a 100 %-reproducible `Instruction access fault
mepc=0x2` under `debug-console`. **This design adds zero tasks.** The main-loop drive is precedent-
following, not novel — the STT PTT flow already parks the main loop for seconds by design, and the
loop's own budget banner documents that.

### 6.4 Do not render mid-utterance

`main.rs:3795` documents that painting the Slint scene blocks the single-threaded executor for tens
of ms and starves the audio DMA — STT sacrificed its live level bar for exactly this reason. With a
128 ms playback queue the same rule binds TTS: **paint state before and after the utterance, never
during.** TTS sacrifices nothing to comply (there is no live meter to animate).

### 6.5 Half-duplex

`push_chunk` raises `PLAYBACK_ACTIVE` before the first sample, so `mic_capture_task` discards capture
windows for the duration — no AEC needed, the existing gate covers it. A TTS utterance and a PTT
capture can never overlap: both are driven from the same main loop, serially.

### 6.6 Cancellation — a correctness requirement, not polish

**Raised to required by the orchestrator, and the reason is sharper than "nice to have":** JP has
been chasing repeated "the watch is frozen" reports today, several of which turned out to be
responsive firmware behind a stuck-looking UI. Speaking parks the main loop for **seconds**. Ship
that without an escape hatch and read-aloud becomes a new source of exactly the symptom he is
already hunting.

So a **tap stops speech**. `speak_text` takes `should_stop`, polled every `CANCEL_POLL_CHUNKS` (4)
chunks — a **64 ms** worst-case reaction, inside the window where a tap still feels instant. It is
rate-limited because the poll reads the touch controller over the I2C bus the codec shares; polling
all 62 chunks/second would triple bus traffic for no perceptible gain.

On stop: `audio_out::drain_queue()` drops what is still queued, the socket aborts, and the amp is
**not** hard-cut — the feeder finishes its staged chunk and pads silence, which scrubs the ring and
releases the amp itself. Hard-cutting mid-sample is the "reverse order pops" failure `service_amp`
documents; it would put a click on the end of every interruption. Costs ~64 ms of trailing silence.

The user-visible half matters too: a toast reading **"Reading aloud — tap to stop"** is painted
*before* any audio is queued (never during — that would starve the DMA), so the pause is legible as
speech rather than as a hang.

### 6.6.1 Other cancellation sources

Long utterances (up to ~7 s from a maximal notification) must be interruptible. `speak_text` takes a
`cancel` closure checked each chunk: screen-off, app change, a second notification arriving, or the
user tapping stop. On cancel it stops reading, `socket.abort()`s, and lets the feeder's tail drain
naturally — which also scrubs the ring to silence, preserving the idle-ring invariant.

`session_aborted()` covers the reverse direction: if `feeder.abort()` fires (DMA re-arm) while we are
mid-stream, our sends would otherwise pour into a drained channel forever. Detecting it ends the
stream cleanly.

---

## 7. Trigger policy — on-demand default, narrowly-gated auto

**Recommendation: `OnDemand` (mode 1) as the shipped default.**

Auto-read-everything is wrong here for three independent reasons:

1. **It parks the main loop for seconds.** A notification landing mid-game would freeze a framebuffer
   app for the length of the utterance. The main loop is the game loop.
2. **Privacy.** The watch is on a wrist, in rooms, with people. Reading every HA notification aloud
   is a behaviour users switch off once and never switch back on.
3. **The ambient cue already exists.** The #58 ping chime already says "something arrived" in 480 ms
   without a network round-trip. Speech should be the thing you *ask* for, not the default response
   to an event.

The three modes:

| mode | behaviour |
|---|---|
| `0` Off | never speaks; the TTS path is inert |
| `1` **OnDemand** *(default)* | a speaker affordance on the notification card / shade reads that card |
| `2` Auto | reads on arrival, **but only when** all of: screen on, on Watchface or shade (never inside an app), not muted, WiFi up, and no utterance already in flight |

Mode 2's gating is what makes it defensible: every condition maps to a specific failure it prevents
(§7's reasons 1 and 2, plus underrun from a cold WiFi association). Bursts are handled by "no
utterance in flight" — the newest arrival is spoken, the rest are left for the shade, matching the
existing toast behaviour where "the badge carries the real count".

Spoken text composition (`tts_proto::compose_utterance`): `"<source>. <title>. <body>"`, e.g.
*"Home Assistant. Garage door. Left open for fifteen minutes."* Source prefix gives the listener
context they would otherwise get from the card glyph. Capped at TITLE_CAP + BODY_CAP + prefix
≈ **200 chars → ~7 s of audio** worst case, well inside the bridge's 30 s cap.

**UI note:** the affordance itself (speaker glyph on the shade card, pressed state, the speaking
indicator) is Luna's territory under the in-flight UI overhaul, and must follow the 1-frame bold
pressed-state standard from `2026-07-23-ui-overhaul-design.md`. This spec defines the seam
(`speak_notification(idx)`), not the pixels.

---

## 8. Build & acceptance gates

Per the brief: **build only, do not flash — both watches are in use.**

1. `cargo test -p tts-proto --target x86_64-unknown-linux-gnu` — host tests green.
2. Each host crate with `-p`. **Do NOT use `cargo test --workspace`** — see §8.4.
3. `fambuild build --release --bin esp32c6-watch` — clean.
4. `fambuild build --release --bin esp32c6-watch --features debug-console` — clean. **Both**
   configurations, because release and debug-console builds have historically diverged on exactly
   this class of memory bug (#65: "debug-console builds never crash, which is exactly why every
   automated check passed while release builds died").
5. **Report the `[STACK] gap` delta** vs the pre-change build. This is the real acceptance criterion
   for the memory budget in §6.1 — measured, not asserted.
6. Bridge: `POST /tts` exercised end-to-end from familiar with `curl`, output piped to `aplay -f
   S16_LE -r 16000 -c 1` to confirm intelligible dragon-voice audio before any firmware runs.

### 8.1 Results (2026-07-27)

| gate | result |
|---|---|
| `cargo test -p tts-proto` | **30 passed** (12 compose, 18 wire) |
| all 11 host crates | **178 passed, 0 failed** — no regressions |
| `fambuild --release` (default, `tts` off) | **links clean**; +136 B `.bss`, +116 B ROM |
| `fambuild --release --features debug-console` | **links clean** |
| `fambuild --release --features tts` | ROM overflow by **2,938 B** — expected, §6.7 |
| `fambuild --release --features tts,debug-console` | ROM overflow by **8,750 B** (debug-console is itself ~5.8 KB) |
| stack gap (default build) | 75,040 B — **+3,360 B over the floor** |
| stack gap (with `tts`) | 74,760 B — **+3,080 B over the floor** |
| bridge `POST /tts` end-to-end | **verified on a throwaway instance**, §8.2 |

### 8.1.1 Running the host tests — two papercuts

**`cargo test --workspace` does not work in this repo**, and the failure is confusing rather than
obvious: the workspace root member is the *firmware* crate, so `--workspace` tries to build it for
the host and dies inside `esp-sync` with ``cannot find module or crate `riscv` ``. Nothing to do
with the crate under test. Test host crates individually instead:

```sh
cargo test -p tts-proto --target x86_64-unknown-linux-gnu
```

`--target` is required too: `.cargo/config.toml` sets the default target to
`riscv32imac-unknown-none-elf`, so a bare `cargo test -p <crate>` fails with ``can't find crate for
`test` `` / "`#[panic_handler]` function required". Both messages point away from the real cause.

(Pre-existing, not introduced here — recorded so the next person loses minutes instead of an hour.)

### 8.2 Bridge verified end-to-end (no watch touched)

Ran the patched bridge as a disposable instance on `familiar:8097`, exercised it, tore it down.
Production STT bridges (familiar:8090, ubox0:8090) never touched — re-verified healthy afterwards,
and `grep -c _do_tts` on the production file is **0**.

- happy path → **200**, 126,400 B = **3.95 s**; `Content-Length` exact and even (never splits a sample)
- signal: peak 19204 (headroom, no clip), rms 2183, 50.5 % active — speech with pauses
- empty / whitespace text → **400**; malformed JSON → **400**; wrong path → **404**
- **TTS → STT loopback** → `"Home Assistant garage door left open for 15 minutes."`
- **SSML injection** (`</voice><voice name="en-US-GuyNeural">`) → spoken as literal words, **no voice
  switch**; escaping holds

The loopback is the strongest evidence available without hardware: feeding the output back through
the existing `/stt` route simultaneously proves the bytes really are mono 16 kHz s16le, that the
level is right, and that the speech is intelligible.

#### RULE: verify port OWNERSHIP before believing a health check

The first test instance bound port **8091** and answered `{"status":"ok"}`. That was
**llama-server** — my bridge had already crashed on an import error. The health check was green and
completely meaningless.

**A health endpoint answering from a *different service* while your process is dead is a false
success, and this project keeps getting bitten by that exact shape:** the debug console's `ok chime`
ack while zero samples reached the speaker; a soak reporting `0 % stable` against an unplugged
watch; `feeder.abort()` dropping a clip without setting the completion latch, making failure
indistinguishable from success at every observation point.

So: **`ss -ltnp` to confirm which PID owns the port before trusting anything it says.** On familiar,
**8090 and 8091 are already taken** (the STT bridge and llama-server). Generalised: a check that
cannot distinguish "working" from "something else answered" is not a check.

### 8.3 Still requires on-glass verification (orchestrator / JP)

Amp raises once and stays up for the whole utterance (no per-chunk cycling), speech is continuous
and un-chopped, mic stays suppressed throughout, and the ring returns to silence with no trailing
pop. Plus, if the ROM region is raised: a clean boot and a full OTA cycle at 6 MiB.

---

## 9. Blast radius

| file | change |
|---|---|
| `crates/tts-proto/**` | **new** — pure logic + host tests |
| `Cargo.toml` | one dependency line (workspace glob already covers the member) |
| `src/net/voice_tts.rs` | **new** — streaming client |
| `src/net/mod.rs` | one `pub mod` line |
| `src/peripherals/audio_out.rs` | `push_chunk` + `session_aborted`; re-attach the orphaned doc comment |
| `src/peripherals/config.rs` | `SWCFG7` + `speak_notifications` + migration |
| `src/notify.rs` | expose a "speak this card" hook; no change to the ring itself |
| `src/main.rs` | trigger + call site in the existing loop; **no new task, no new static** |
| `tools/watch-bridge/**` | **new** — vendored bridge (`POST /tts` + `/health`), `deploy.sh`, README |

Callers affected: none existing. `play_pcm`, `play_chime`, `LONG_CLIP` and the whole SFX path are
untouched — `push_chunk` is additive alongside them.

---

## 10. Explicitly out of scope

- **Making the watch an HA-controllable media_player** (the backlog item where HA pushes TTS *to* the
  watch). This design builds the exact seam that feature needs — `push_chunk` + a streaming PCM
  source — but the MQTT/HA half is separate work.
- Barge-in / voice-interrupt (no AEC on the C6; half-duplex is a hardware fact).
- On-device TTS. Not remotely feasible at 186 KB.
