# HANDOFF — 2026-07-29, esp32c6-watch

**Active goal (Stop hook):** get Endless LitRPG story stably running on a watch.
**Status: story LOADS and PLAYS on eldritch-lantern.** Remaining polish below.

## Bench state

| watch | port | firmware | notes |
|---|---|---|---|
| eldritch-lantern | /dev/ttyACM1 (1-9) | `story-vol.elf` pending OTA; currently `story-admin.elf` | the product watch — story works here |
| mythic-throne | /dev/ttyACM0 (1-2) | integrated stack, no `story` | control watch; lost the pool lottery, main ~500 B |

`main` = `80c84e3`, pushed through `3a4da89`; the volume fix is committed locally.
ELFs are pinned per flash in the scratchpad (`pinned-*.elf`) — **keep doing this**, an
agent lost three disassembly passes to a rebuilt binary.

## THE key fact about WiFi (was mis-diagnosed for hours)

WiFi was never broken. The watch was on SSID `roam` → 10.0.11.x; the story daemon runs
on **katana 10.0.6.129:8093** and katana has **no 10.0.11.x interface**, so it was a VLAN
crossing. Voice always worked because its bridge is at 10.0.11.11, same subnet.

JP's SSID `admin` (password in the gitignored `.cargo/config.toml`, now set there) fixes
it. Proven in one window: `strongest ch6 rssi-29 (hint, no BSSID pin)` → `[WIFI] connected`
→ `[NTP] unix=…` → `[WX] 67F`. So DHCP/DNS/UDP/TCP all work.

**Persisted config beats compile-time creds**: `option_env!("WIFI_SSID")` is only used
`if watch_cfg.ssid.is_empty()`. To switch SSID you must also erase the config partition:
`espflash erase-region --port /dev/ttyACM1 0xC10000 0x10000`.

**Open trade:** `[MQTT] failed: tcp read` — broker is 10.0.11.110, on the OLD subnet.
`nebula-heap-census` is assessing whether HA telemetry is now orphaned and whether a
DNS name would survive VLAN moves. Infra is READ-ONLY (JP's rule).

## What shipped today (all on main, all preflight-green: 230 tests, 8 combos)

- **Menu freeze FIXED** (JP confirmed on glass): the apps menu was drawing the whole
  watchface underneath — the software renderer has no occlusion culling.
- **Boot wedge FIXED** (0/4 → 6/6, within-device A/B): unbounded ESP-NOW `SendWaiter::wait()`
  spin on the UI loop. Now a bounded `select(send_async, Timer)`.
- **Volume freeze fixed**: `config::save` = two 4 KB sector erases with interrupts
  disabled, fired per slider *sample*. All 11 write sites now deferred to one coalesced
  flush.
- **Panics reboot instead of bricking** (`custom-halt`) — verified 3 recoveries.
- **Two remote DoS fixed**: unbounded peer roster (any RF-range broadcaster could OOM
  the watch) and unlatched ELECT logs (could stall the heartbeat from off-wrist).
- **Mesh channel election** — observe-only (`ELECT_ENFORCE=false`), and **both watches
  independently elected ch6** on JP's real APs at epoch 0. `step()` is gated so the epoch
  cannot climb while observe-only (it was climbing; smol's stays 0 — would have partitioned).
- **Story reader** (opt-in `story` feature) + **mid-playback volume fix** (`80c84e3`).

## Still open

1. **OTA `story-vol.elf` to eldritch** — JP asked for it. `python3 tools/watchctl deploy
   eldritch-lantern <elf> --slot ota_0` (USB) or the OTA path.
2. **OOM reboots not fixed.** Cause is a **boot-to-boot pooled-capacity LOTTERY**, measured
   within one device: identical config/boot heap, boot 1 `items=128 tex=64` → 60,264 free;
   boot 2 `items=256 tex=256` → 34,548 free, main 504 B. Capacity never shrinks. The fix is
   `perf/pool-reserve-at-boot` (pushed, NOT merged) — reserves the measured peak once at
   renderer construction. **Held because it costs 13,376 B permanently, which is the
   headroom story needs.** Land after story is stable or with more heap.
3. **`maxblk_main=0` unverified** — either main genuinely can't serve 1 KB, or the
   `used`-delta region test is blind because `internal-heap-stats` is off by default.
4. **`[PROBE-BUG]` inconclusive** — free is now above the probe ladder's top rung so it
   cannot fire. The original contradiction (`maxblk=32768` in 236 beats while free was
   25-40 KB) only shows when free dips under 32,768.
5. **`harvest_free` experiment** (behind `heap-forensics`, never-ship): flip
   `SCENE_DROP_ON_SUSPEND` at `slint_shell.rs:495` to true, launch a game. Arms — mean hole
   size ~10 B ⇒ fragmentation; `stop=nomem` + large `left` ⇒ lost holes; **a hang with no
   panic ⇒ a CYCLE in the hole list, which is a POSITIVE result, not a broken tool.**
6. **smol mesh port** — `feat/mesh-elect-consensus` pushed, observe-only, channel election
   only (smol's existing crown election deliberately untouched). No NVS: all 6 sectors are
   owned and sharing one is brick-class.
7. `story,debug-console` at **+2,568 B** is the tightest stack margin in the tree.

## Traps I hit repeatedly — do not repeat

- **`pgrep -f` / argv scans kill the script doing the killing** (its own argv contains the
  pattern). Cost three self-kills, including one that silently skipped a flash.
- **Never cross-device A/B.** Every cross-watch conclusion today was retracted. Use
  within-device, one variable, 4+ trials.
- **Worktrees share refs but not HEAD.** `git push <remote> <branch>` pushed a stale local
  branch while my commits sat elsewhere; `HEAD:main` is what publishes. Also had 4 commits
  on a detached HEAD, GC-eligible until an agent caught it.
- Don't hold a serial port while probing — `espflash` then fails in a way that looks
  exactly like dead hardware.

See memory: `heap-attribution-rules`, `out-of-band-liveness-probe`.
