# watch-bridge — the watch's voice proxy

`watch_bridge.py` is the LAN HTTP bridge that gives the watch a voice in both
directions. It runs on **ubox0, `10.0.11.11:8090`** (VLAN 11 "roam" — the network
the watch associates to).

```
watch --POST /stt--> bridge --HTTPS--> Azure STT   (mic PCM  -> transcript)
watch --POST /tts--> bridge --HTTPS--> Azure TTS   (text     -> speech PCM)
```

## Why a bridge exists at all

The watch is **plain-HTTP only** — no TLS, no DNS, dotted-quad IPv4. It cannot
talk to Azure directly, and we would not want it to: **the Azure key stays on the
bridge host and never touches the device.** The bridge imports speech-to-cli's
own `state.load_config()` and reads the key from
`~/.config/speech-to-cli/config.json` *on the bridge host, at runtime*.

**Nothing in this directory contains a credential, and nothing here ever should.**
`deploy.sh` refuses to ship a file that looks like it acquired one.

## Why it is vendored here

It previously existed **only** on ubox0 and familiar — in no repository at all,
while two shipped firmware features depended on it. A host rebuild would have
taken the watch's voice with it. Vendoring is the fix; the copy here is the
source of truth and `deploy.sh` pushes it out.

> Upstream note: this file originated in `~/Projects/speech-to-cli/`. If it is
> ever adopted into that project properly, delete this copy rather than letting
> the two drift.

## The API

| route | request | response |
|---|---|---|
| `POST /stt` | raw mono 16 kHz s16le PCM (or a RIFF/WAV blob), `Content-Length` or chunked | `200 {"text": "..."}` |
| `POST /tts` | `{"text": "...", "voice": "<optional>"}` | `200` raw mono 16 kHz s16le PCM, exact `Content-Length`, `X-Audio-Format` |
| `GET /health` | — | `200 {"ok", "region", "tts_region", "voice", "tts_format"}` |

Errors mirror each other: `400` bad/empty input, `502 {"error": "azure: ..."}`
upstream failure, `404` unknown path.

### One format, everywhere

Mono **16 kHz s16le** end to end — the mic ring, the STT upload, the TTS reply,
and the watch's playback ring all use it. Azure's DragonHD voices emit
`raw-16khz-16bit-mono-pcm` natively (verified 2026-07-27), so **the bridge does
no transcoding and the watch does no decoding**.

### `/tts` fully synthesizes before it replies — on purpose

Azure's own response stream stalls **255–782 ms** between chunks, while the watch
can only bridge a **64 ms** gap behind a 128 ms playback queue. Relaying live
would underrun every utterance: the amp cycles, and speech comes out chopped and
popping. Synthesizing completely and *then* streaming at LAN line rate puts all
that variance on the host with gigabytes of RAM instead of the one with 186 KB.

It costs less than it sounds: Azure's time-to-first-byte is ~1.2 s regardless of
length, and full synthesis beats realtime for anything past ~2 s of audio.

## Deploying

```sh
./deploy.sh --dry-run     # show the diff, change nothing
./deploy.sh               # back up, deploy, restart, health-check (auto-rollback)
./deploy.sh --host familiar
```

Rollback is automatic if the restart or the health check fails.

## Checking it by hand

```sh
ssh ubox0 'curl -s localhost:8090/health'

# speak something and listen to it locally
ssh ubox0 'curl -s -X POST localhost:8090/tts -H "Content-Type: application/json" \
  --data-raw "{\"text\":\"Home Assistant. Garage door left open.\"}"' \
  | aplay -f S16_LE -r 16000 -c 1

# loopback: TTS -> STT should return roughly the words you sent
```

That loopback is the strongest check available without a watch — a clean
transcript proves the format, the level, and the intelligibility in one shot.

Service: `systemctl status watch-bridge` · logs: `journalctl -u watch-bridge -f`.

## Gotcha

On familiar, ports **8090 and 8091 are already taken** (the STT bridge and
llama-server). A health probe against the wrong port returns a cheerful
`{"status":"ok"}` from llama-server while your bridge is dead. Check ownership
with `ss -ltnp` before believing a green check.
