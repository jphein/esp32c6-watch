# Changelog

All notable changes to this project are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/); this project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

*Tooling and docs only — no firmware change; fold into the next release heading.*

- **`watchctl soak <sigil> [-n trials] [-s seconds]`** — the boot-stability probe is now a
  first-class subcommand (defaults 6 boots × 12 s), resolving the watch by sigil instead of
  needing a raw `/dev/ttyACM*`. Verified 0 % on `eldritch-lantern` (#63).
- **`ui_test.py hotpaths` fixed for the paged launcher** (#63): the suite still carried a
  continuous-scroll step from before v0.8.0 and aborted on it. It now flips one section page
  per swipe (AUDIO → GAMES → SYSTEM and back) and gates at a 250 ms bar, since a page flip is
  one full-frame repaint — the render floor, not the old 100 ms scroll threshold.
- **docs: corrected a stale OTA slot cap.** `docs/debugging.md` still quoted the pre-#50
  **4,128,768 B** image gate in two places; the real cap has been **6,225,920 B** (6 MB slot
  less 64 KB) since the slots grew, and `save-image` now needs `--flash-size 16mb` plus the
  partition table to match. Understating the budget by ~2 MB would have sent someone
  shrinking an image that already fit.

## [0.12.1] — 2026-07-26

- **CRITICAL — the "freezing" was a WiFi-blob crash** (#61). esp-radio 0.18's WiFi blob
  null-dereferences inside `ppRxFragmentProc` during scan/associate: a deterministic panic
  ~2.2 s into boot, **100 % of the time**, straight into a reboot loop. The fix is the one
  RX-aggregation knob 0.18 exposes — `ControllerConfig::with_ampdu_rx_enable(false)` at
  WiFi bring-up. Measured **100 % → 0 %** crash rate on both watches, with association,
  DHCP and #57 roaming all intact. Cable-flashed, since crash-looping firmware can't
  self-OTA.
- **`tools/watch_soak.py`** — the boot-stability harness that came out of the #61 hunt:
  reset a watch N times, classify every boot (WiFi panic / brick / download mode / alive),
  and report a crash rate plus time-to-crash. That crash rate was the loop's measurement
  gate — 0 % is the pass.

## [0.12.0] — 2026-07-26

- **The unmissable ping** (#58) — three upgrades to the #35 receiver. The chime is now a
  four-note rising **major arpeggio** (C5→E5→G5→C6, ~480 ms, soft 8 ms attack and
  exponential decay per note, legato overlap, the top C6 held as the arrival), host-tested
  for pop-free edges, bounded level, and strictly-rising note *order* by per-note
  zero-crossing rate. It plays on receive regardless of screen state. The pulse now lands
  over **framebuffer games** too: a ping suspends the running app through #31's session
  path (state preserved), frees the ~51 KB framebuffer, resumes the Slint scene so the
  pulse can composite — and on dismiss re-launches the app exactly where it was. A fully
  off panel now wakes to the pulse as well, not just AOD/dim. And **every** received ping
  logs an RTC-stamped shade card, whose timestamped body keeps distinct pings from being
  swallowed by notify's consecutive-duplicate suppression, so there's a persistent record
  after the pulse auto-dismisses.
- **Volume and mappable buttons** (#59), persisted in one config extension — **v6**
  (`SWCFG6`, 118 B): a volume byte (0–15 level plus a mute bit) and four button-map bytes
  (BOOT/PWRON × short/long → `ButtonAction`, one of none / volume up / volume down / mute /
  power menu / shutdown / launcher / ping / voice). v1–v5 records load with defaults and
  the first save rewrites v6. Volume applies to **all** codec playback — the master
  register is set at boot and on every change, and re-applied after unmute, so every
  chime, beep, click and touch tick honours the level. A **volume HUD** overlay appears on
  any change and auto-dismisses after ~2 s (dragging re-arms it). Button dispatch is a
  single pending-action path fed by a BOOT press state machine (600 ms long threshold —
  long fires while held, short on release) plus the PWRON poll, with one deliberate
  nuance: a short press acts **only** if the screen was already bright, so a press in the
  dark just wakes the watch. Long always acts after waking, preserving #48's
  hold → power menu. Leaving a game via a mapped action suspends the session first.
- **Climate and Energy are live again** (#60) — and the root cause was not what it looked
  like. The firmware reads *retained MQTT*, but the deployed HA component served HTTP only
  and the old Node-RED MQTT bridge had been retired, so `watch/climate/+/state`,
  `watch/climate/roster`, `watch/energy/state` and `watch/energy/avail` were simply empty
  on the broker. Deploy verification then exposed the deeper reason they stayed empty:
  `media_player.py` imported `homeassistant.helpers.device_info`, a module removed in
  modern HA, so a `ModuleNotFoundError` was thrown *during entry setup* — the HTTP site
  starts before that line, which is why `/watch/*` answered and looked healthy, while
  everything after it never ran. Fixed by importing `DeviceInfo` from its modern home in
  `device_registry` (with a fallback for older HA), which also **restored the
  `media_player` speaker entity**, and by starting the MQTT bridge *before* forwarding the
  media_player platform so the data path can never again be taken down by a secondary
  platform's import break. Component **v0.3.0** now republishes the same data the HTTP
  endpoints compute onto the retained topics the parser reads, and subscribes to
  `watch/climate/+/set` so a watch-side command runs the same dispatch as the HTTP POST
  (retained deliveries ignored, so a stale command can't replay). Verified on live HA:
  all four topic families populate retained, energy live-updates, and a watch→HA command
  is processed cleanly with zero component errors — **no firmware reflash needed**.

## [0.11.0] — 2026-07-26

- **Press-once voice** (#22): a PTT press before WiFi/DHCP was ready used to show
  "Connecting…" and *drop* the press — press-twice UX, and the common case on a
  time-shared radio. An early press now latches and auto-fires the capture through the
  **same** entry path as a real press once the link lands (a fresh hold resets the
  backoff, so the first attempt is immediate). Releasing while it waits cancels, on a
  3-read debounce of the authoritative I²C finger count — the INT pin lies about still
  fingers — so the latch dies within ~300 ms of a lift and can never survive into
  dim/AOD, and a transient I²C miss defers the fire one tick rather than producing a
  junk capture. A 30 s window then reports "WiFi failed". `StackResources` 4 → 5 for
  the burst + STT socket overlap.
- **Watch-to-watch ping** (#35) — the fleet greets by name. Additive SMOLv1 mesh frames
  (`PING` broadcast, `PINGACK` unicast confirm, ACKed at the protocol level on the
  HELLO→ACK idiom, flagged for smol upstreaming, #36). The plugin is a 200 px hero
  reading "PING <sigil>" when the peer is live from HELLOs, with delivery confirmation,
  an honest 2 s no-reply timeout, a 3 s cooldown, and a hint to enable MESH when it's
  off. The receiver wakes AOD/dim to bright, blooms a full-screen accent-ring pulse
  carrying the sender's sigil, and plays a ~300 ms rising E5→B5 two-tone chime; with the
  panel fully off or a game holding the framebuffer it gets the chime plus a shade card.
- `mythic-throne` took this release **over the air, zero-touch** — the first such
  self-update since the v0.10.1 flash-safety fix, and the payoff for #53's net_task,
  #55's slot guard and #57's roaming landing together. (The genuine first was v0.8.4:
  pre-#50, otadata still resolved correctly and zero-touch worked — #55 only turned
  fatal once #50 moved the slots.)
- Image 4.26 MB with a 2.0 MB slot margin (#50's grow paying off); stack 63 KB.

## [0.10.1] — 2026-07-26

**The safety release.**

- **CRITICAL fix: OTA could download into the RUNNING slot and brick the watch**
  (#55, the eldritch-lantern boot-loop). Slot selection trusted otadata
  (`Ota::current_app_partition`) — a boot *request*, not a boot *fact*. Stale
  otadata (left saying "ota_1, Valid" from the pre-#50 4MB layout, which cable
  flashes never rewrite) made "the other slot" resolve to the very partition
  the CPU was executing from; a retained push-OTA announce then triggered a
  zero-touch self-overwrite: every 4KB chunk erase+rewrote the live image
  (replanting the app-descriptor ELF-SHA at flash `0x100B0`) until the erase
  of the sector holding in-use WiFi rodata (`0x152000`, download chunk 322)
  killed the app mid read-modify-write — checksum-broken ota_0 + empty ota_1 =
  "No bootable app partitions". Fixed by deriving the running slot from the
  **MMU** (`PartitionTable::booted_partition` — which physical flash page the
  CPU actually executes from; otadata is never consulted), plus a hard refusal
  if the computed target equals the booted slot.
- **Flash write guard** (#55, systemic): the shared `FlashMutex` now wraps a
  `GuardedFlash` — every `Storage::write` is range-checked (sector-rounded:
  esp-storage RMW erases whole 4KB sectors) against a deny-list holding the
  bootloader + partition-table region and the booted app slot (both slots if
  the boot probe fails; the whole flash if the partition table is unreadable).
  Violations refuse + log (`[FLASH-GUARD] REFUSED …`), never touch flash. The
  range math is the new host-tested `crates/flash-guard` (pure no_std; tests
  include the exact incident vectors). Boot log now prints
  `[OTA] booted from … (MMU)` alongside what otadata *requests*.
- `tools/ota_push.sh --clear`: delete the retained OTA announce (empty
  retained publish). A cable-flashed dev build (`OTA_BUILD=0`) accepts any
  retained announce and zero-touch replaces itself on its next MQTT window —
  clear the topic before bench sessions.

- **Overlays swallow taps** (#54): a shared `OverlaySwallow` seals 13 overlays so the
  chrome beneath is never hit-testable.
- **Multi-pass WiFi scan** (#56): esp-radio 0.18 fixes the active dwell at
  10–20 ms/channel, shorter than a ~100 ms beacon interval, so a single-pass sweep
  misses APs that are actually present. Each channel is now swept twice and merged,
  strongest RSSI winning. Picker visibility only.
- **Firmware WiFi roaming** (#57 — shipped as `v0.10.1-roam`). esp-radio has no
  802.11r FT: the config struct carries zero FT/MDE fields, so it always negotiates
  plain WPA2-PSK — FT was never the cause of anything here. The real problem is that
  its default connect uses `WIFI_FAST_SCAN`, which associates with the **first**
  SSID-matching AP it hears (`sort_method` is ignored in fast-scan) — routinely a
  distant BSSID whose weak-link 4-way handshake times out while a strong AP sits beside
  you, in a house running one roaming SSID across 12 APs. The watch now roams in
  firmware, entirely watch-side with no AP changes: a targeted SSID-filtered multi-pass
  candidate scan pins the strongest BSSID explicitly; a pin that fails twice falls back
  to one driver-side full-scan select and re-scans, so a vanished BSSID can't wedge the
  connect; and while connected it samples RSSI every 2 s, reassociating when it holds at
  ≤ −75 dBm for ~8 s and a candidate is ≥ 12 dB better. On glass:
  `AuthenticationExpired` → 0.
- **The two-day WiFi outage is resolved — and it was never one thing.** A USB3 hub
  jamming 2.4 GHz (unplugged), esp-radio's fast-scan grabbing a far BSSID (#57 above),
  and a loose antenna on `mythic-throne` (reseated). The jamming half had no firmware
  cause; that fix was physical.

## [0.10.0] — 2026-07-26

- **The UI loop no longer owns the radio** (#53). A dedicated `net_task` exclusively
  holds the WiFi controller: commands in (`Raise`/`Drop` a hold, `Scan`, `SetCreds`,
  `Ota`) over a depth-8 channel; a `NetSnapshot` out behind a blocking mutex
  (`WifiPhase`, `radio_started`, scan rows, one-shot NTP/weather handoffs, `OtaPhase`),
  every change signalling `NET_WAKE` on the v0.8.8 coalescing pattern. A **hold mask**
  (`User`/`Burst`/`Session`/`Voice`/`Ota`/`Phy`) replaces the old `wifi_on_request` +
  `session_holds_wifi` + per-tick re-raise scramble — `Phy` being mesh's
  start-the-radio-without-associating case — with 2s/10s/60s/300s exponential backoff
  on consecutive failures. Every WiFi/OTA/scan arm migrated out of the main loop, and
  OTA now survives a mid-download reconnect.
  **Acceptance, measured on glass under a dead-AP outage: worst frame 202 ms,
  `arm_max` 135 ms — where the old code froze for 15 seconds.** Held there by a
  REALTIME BUDGET rule at the loop head (>10 ms of blocking in any arm is a bug, with
  an audited exemption list: full-frame Slint renders, ms-scale flash sector programs,
  the by-design voice PTT park, wake one-offs) plus an RAII per-arm watchdog that
  reports `arm_max_us` / `arm_over10ms` from `debug-console` builds. Independently
  reviewed before merge — a busy-spin, a pin race and a sleep-gate hazard all folded.
- **Edge-gesture shell** (#29 / #31 / #32): a bottom-edge swipe-up opens the launcher
  from any watchface page; a bottom-edge **hold** raises an app switcher with
  suspend / resume / kill and a corner badge for what's still running; a top-edge
  swipe-down pulls down a **notification shade** fed by MQTT (`watch/notify`) plus
  system events, with retained notifies riding the boot-burst MQTT window.
- **Power menu** (#48): an AXP2101 PKEY long-press opens SHUTDOWN / REBOOT. The 4-second
  hardware failsafe stays intact underneath it.
- **12-band FFT spectrum analyzer** in the Sound app (#30), log-spaced for factory
  parity — plus a same-day on-glass fix after the band painted over the page title.
- **OTA slots grown 4 MB → 6 MB** (#50): `ota_0` @ `0x10000`, `ota_1` @ `0x610000`,
  `config` moves to `0xC10000`. The margin was down to **5.4 KB** at consolidation.
  `ota_push.sh` and `watchctl` gates follow the new table, and `save-image` now needs
  `--flash-size 16mb`. Deployed to both watches as a **cable event** — a full flash
  with the new table, which resets the config record, so persisted toggles and theme
  return at defaults exactly once.
- **ROM**: the gesture-shell overlays went component-lean (~90 KB of image reclaimed)
  and the shell chrome conditionals are visible-gated (~21.5 KB more).
- **Heap 214 KB → 198 KB**: the consolidated scene build (power menu + switcher + shade
  + spectrum) overflowed the 46.9 KB stack and tripped esp-hal's stack guard at boot.
  Caught by the wrong-credentials acceptance run rather than in the field — +16 KB of
  stack (gap ≈63 KB), with the heap still ~38 KB clear of the framebuffer need.

## [0.9.1] — 2026-07-25

- **Mesh channel-pin yields to WiFi** — the ch6 ESP-NOW pin was firing between
  association attempts (only an OTA-pending update suppressed it), so it could steal
  the radio from a WiFi connect already in progress. The pin now yields whenever
  `wifi_on_request` is raised. A correctness fix that stands on its own.
  > **Corrected in v0.10.1:** this entry originally credited the pin with the
  > `AuthenticationExpired`-at-good-RSSI failures and single-network scans seen on
  > `mythic-throne`. That diagnosis was wrong. Those came from esp-radio's fast-scan
  > associating with a distant BSSID (#57), a USB3 hub jamming 2.4 GHz, and a loose
  > antenna — see [0.10.1]. The pin change was real, but it never explained the outage.
- The `[MESH] up as node id042` log was a hardcoded string — the mesh had been
  running with the arbitrated sigil id all along; it now prints the real one.

## [0.9.0] — 2026-07-25

- **Touch sounds everywhere** (#49) — one hoisted tap hook plays a 12 ms 1.8 kHz
  tick for *both* input families (the Slint shell's `handle_touch` and the
  framebuffer apps' `AppInput.tap`), never per-widget. Peak ~-15 dBFS: texture,
  not notification. Gated on the persisted toggle, `audio_out::busy()`, and PTT
  recording (half-duplex). The old per-control launch/OTA clicks are gone.
- **Settings hub** — the Settings tile now opens a scene-resident Slint overlay
  (registry kind `Overlay`: no framebuffer, no scene suspend) with five paged
  sections — SOUND, DISPLAY, RADIOS, NETWORK, SYSTEM. Swipe up/down flips pages;
  right-swipe backs out then closes. The old framebuffer Settings app, the T9
  keyboard, and `peripherals/wifi.rs` are deleted.
- **Scan-based WiFi join + QWERTY keyboard** — `scan_async` → dedup by SSID
  keeping best RSSI → strength-sorted top 6 plus "Other network…"; secured picks
  open a 4-layer QWERTY pane with Rust owning the buffer (masking, 24-char tail
  window, held-backspace auto-repeat on the 16 ms touch tick, show-password eye).
  Feeds the same credential-save + station-config rebuild path as the old flow.
- **Config record v5** (`SWCFG5`, 113 B) completes the #46 persistence migration:
  v4's reserved radios-flag bits are spent in place (bit 1 mesh-on, bit 2
  WiFi-forced-off, bit 3 touch-sound-muted — OFF bits inverted so a v4 record's
  zero bits decode to mesh off / WiFi auto / sound **on**) plus a mic-gain
  step-index byte. Boot restores mesh, WiFi intent, mic gain, and touch sound;
  edge-triggered dual-slot mirror saves persist each. v1–v4 records still load.
- **ROM budget** — each distinct font size embeds a whole pre-rendered glyph set,
  so the hub's arrival put the 4 MB app slot ~70 KB over. Seven visually-adjacent
  size consolidations across two rounds freed **~397 KB**. The real acceptance
  test is `espflash save-image` fitting the slot, not `readelf` section math
  (espflash adds ~116 KB of segment padding/metadata): the release image is now
  **3,987,776 B of 4,128,768 = 96.6 %**, 137.7 KB margin (debug-console 96.7 %,
  135.5 KB). Stack gap unchanged at 54.5 KB, floor 46 KB. `tools/ota_push.sh`
  gained an early slot-fit gate with a headroom report so this can't ship blind.
- **`tools/watchctl`** (#20, #21) — a one-command USB/WiFi debug rig for the
  fleet: `list`/`logs`/`reset`/`recover`/`slot`/`deploy`/`flash-full`/`console`/
  `test`/`ota-status`/`endpoint`, `--json`, `--transport usb|wifi|auto`. Watches
  resolve by sigil/efuse-MAC serial via udev, never by `ttyACM` number. `deploy`
  defeats the #20 slot trap (save-image + size gate + write-bin into the
  *booting* slot read from the boot banner + a `[SIGIL]`/`[STACK]` boot verify);
  `reset`/`recover` walk the #21 wedge ladder (verified `espflash reset` →
  `USBDEVFS_RESET` ioctl + re-resolve by serial → report power-cycle).
  `ui_test.py` grows a `tcp://host:port` transport speaking the same console
  protocol, and **`docs/debugging.md`** is the agent field guide to all three
  debug channels. The firmware side of the TCP channel is [#51](https://github.com/jphein/esp32c6-watch/issues/51).

## [0.8.8] — 2026-07-25

- **The fastpath release** — five stacked latency/freeze causes fixed (forensics):
  MQTT state arrivals wake the render loop (was a +1s idle tick); "Finding your
  room" no longer eats a 10s backoff racing DHCP; presses while disconnected
  reject with a hint instead of silently replaying later; Energy reports
  unreachable only on a real offline LWT; the config record is dual-slot
  mirrored so a freeze can't wipe creds/theme/BLE.

## [0.8.7] — 2026-07-25

- **Lights plugin** (#39): room-aware light control — hero button → MQTT → HA
  resolves the watch's Bermuda area and toggles that room, retained state back
  (HA side: `packages/watch_lights.yaml` in the ha repo, field-tested).
- **BLE-sleep lockup hotfix**: light-sleep with the BLE controller active locks
  the chip → BLE-on now tick-idles AOD (continuous adverts keep room presence
  alive). Audio seam (#23) + stable BLE identity (#47/#46 partial) landed as
  v0.8.5/0.8.6 and are folded into this tag.

## [0.8.5] — 2026-07-24

- **Sound is back — shared I2S TX playback seam** (#23): SFX play by substituting
  samples into the always-running silent-clock TX ring (the full-duplex master
  whose BCLK/WS clocks the ES7210 mic), so the mic clock never stops for a beep.
  New `audio_out` module: `play_pcm()` takes project-standard mono 16 kHz s16le
  (queued non-blocking, remainder rejected when full), a feeder expands to the
  ring's stereo, and the speaker amp (GPIO6) + ES8311 power up only while a clip
  is in flight (pops triple-guarded: synth ramps, driven-silence lead-in, tail
  pad). Half-duplex: capture windows are discarded while playing (no AEC).
  Restored consumers: the Snake food beep (dead since the mic work) plus a
  subtle tap-click on launcher tile launches and UPDATE FIRMWARE. SFX synths
  live in `mic-dsp` (host-unit-tested); debug console gains `beep`.
- **Stable BLE address** (#47): the advertised address is now a static-random
  address derived from the efuse MAC (top two bits forced per the BLE spec —
  `eldritch-lantern` → `D8:A3:16:A7:2F:E4`, `mythic-throne` →
  `D8:A3:16:A5:A7:F8`), replacing a *hardcoded fleet-shared* constant that made
  both watches advertise the same MAC. HA/Bermuda room-tracking registrations
  now survive reboots and OTAs.
- **BLE toggle persists** (#46, BLE bit shipped early): config record v4 adds a
  radios-flags byte (bit 0 = BLE-on-at-boot; bits 1–7 reserved for the
  coordinated #44/#45/#46 migration). BLE-on now survives reboots/OTAs; while
  the host is running (it can't stop at runtime), further presses flip the
  persisted intent, so "press, then reboot" turns it off.

## [0.8.4] — 2026-07-24

- **Per-device sigil identity** (#34) derived from the efuse MAC via smol's pinned
  `no_std` sigil corpus (`crates/sigil-id`): this fleet is **eldritch-lantern**
  (node 122) and **mythic-throne** (node 236). MAC-derived mesh node ids retire the
  shared node-42 default; per-device MQTT client ids end session-takeover evictions;
  per-watch OTA topics (`watch/<sigil>/ota`, `tools/ota_push.sh --target <sigil>`);
  the BLE advertisement carries the sigil. First release delivered fully zero-touch
  over the air.

## [0.8.3] — 2026-07-24

- **Reliable zero-touch OTA**: failed attempts re-arm (3×) with the loop unblocked
  between tries so WiFi can reconnect; the ESP-NOW channel pin is suppressed while
  an update is pending (it was stealing the radio from the reconnecting WiFi);
  stall margin 10s→20s, WiFi window 25s→45s. Push-OTA validated end-to-end (#25).

## [0.8.2] — 2026-07-23

- **Aurora wake gesture hints**: duo-tone edge shimmers + chevron echoes bloom for
  ~3s on wake (tap / wrist-raise / boot), hinting the page carousel and swipe-up
  launcher; theme-tokened across all four schemes; per-gesture seen-it latches.

## [0.8.1] — 2026-07-23

- **AOD light-sleep panic fix** (#43): esp-hal 1.1.1's in-sleep RC_FAST calibration
  silently returns 0 when the PCR REF_TICK divider isn't programmed → div-by-zero
  at sleep entry (deterministic on a factory-fresh unit). Boot-time
  `rtc_sleep_cal_init()` programs the FOSC gates + tick config, seeds STORE1, and
  dry-runs the calibration; AOD light-sleep is gated on the probe and can no longer
  panic (failed-cal units tick-idle instead).

## [0.8.0] — 2026-07-23

- **Touch-feedback overhaul**: shared `ui/slint/controls.slint` component library —
  bold one-frame pressed states on ~52 touch targets, ≥44 px hit areas, per-scheme
  pressed tokens; live finger-down feedback in the Settings app + T9 keyboard.
- **Paged launcher**: 3×3 grid, one section per page (AUDIO/GAMES/SYSTEM), instant
  flips — replaces the continuous scroll (unfixably janky at software-render rates).
- **Partial rendering v2** (#18): vendored `i-slint-renderer-software` with
  even-grid dirty regions + a pair-exact flusher; steady frames ~18–29 ms
  (was 90–170 ms), no strip artifacts.
- **OTA both directions**: one-tap self-serve updates (the Settings button raises
  WiFi itself, 5-minute budget, per-phase error strings) and **push OTA** via a
  retained MQTT announce + monotonic `OTA_BUILD` gate (`tools/ota_push.sh`).
- **Wrist-raise wake** (accel-poll tilt detection; QMI8658 INT isn't wired),
  QMI8658 endianness fix (step counter + un-corrupted IMU reads), CTRL9 handshake
  hardening, AXP2101 charger profile (4.1 V / 400 mA), Amber default theme.
- **UI test automator**: `debug-console` feature — drive taps/swipes/launches and
  read per-frame render timings over the USB-Serial-JTAG (`tools/ui_test.py`).
- Panel confirmed CO5300 (#17); even-alignment flush quirk documented.

## [0.7.0] — 2026-07-22

- **The mic works** (#7): the microphones are on a separate **ES7210** 4-channel
  ADC (the ES8311 is playback-only) — new driver + boot init + explicit AXP2101
  ALDO1 mic rail. Voice push-to-talk transcribes for real (LAN bridge → Azure STT);
  Sound app gains a live meter, waveform, and digital gain stepper.
- **Plugin/app registry**: every launcher app is a single registration
  (`src/apps/registry.rs`), object-safe `App` trait, data-driven launcher.
- **Theme system**: 4 schemes (Midnight / Paper / Amber / Violet) + on-glass picker,
  persisted in the config record.
- **Home Assistant component** (`ha/custom_components/esp32c6_watch/`):
  climate/energy HTTP API + a `media_player` speaker with a transcoded-PCM
  announce queue. MQTT retained as the primary climate/telemetry transport.
- A/B OTA partition layout adopted on-device; deploy docs (`docs/ota-deploy.md`).

## [0.6.0] — 2026-07-21

- Voice push-to-talk (WiFi-ready-gated capture streamed to a LAN STT gateway),
  speaker playback fixed, touch responsiveness (non-blocking DMA flush),
  launcher scroll fix + AUDIO section, Dependabot, esp-rs stack current.

## [0.5.1] — 2026-07-20

- **Stack-floor guardrail**: a boot-time check on the SRAM the linker leaves between
  `_bss_end` and `_stack_start`, so a future heap bump can't silently eat the stack
  again (the failure mode that forced v0.5.0's heap trim).
- **Climate/energy polish**: setpoints apply optimistically and any unsent change is
  flushed when the screen closes; Energy gates on a live connection instead of
  rendering `-1` placeholders; the climate roster is published on connect.
- Shared `BackChevron` component + 72 px setpoint steppers — uniform 78×64 back
  targets across climate/energy/wled/hunt; one-off colours moved onto theme tokens;
  Node-RED bridge onboarding notes (`ha-bridge/ONBOARDING.md`).
- `climate-model` golden vectors gain the `set:null` and heat_cool-only Auto-bit cases.

## [0.5.0] — 2026-07-20

- **Home Assistant climate control** — a bidirectional MQTT climate session
  (`src/net/mqtt_climate.rs`) drives real thermostats from the wrist: a Climate list
  screen plus a per-device detail overlay (setpoint steppers, mode picker), with
  `crates/climate-model` as the pure `no_std` state core — host-tested against golden
  vectors, including panic-safety on untrusted device names. Design spec:
  `docs/superpowers/specs/2026-07-20-ha-climate-control-design.md`.
- **The home-energy screen goes live** — v0.4.0's placeholder now shows real
  battery/solar/grid values over MQTT (`battery_pct` / `solar_w` / `grid_w` /
  `charging`, availability via LWT, null-safe parsing).
- **Node-RED bridges** (`ha-bridge/`): climate command/state + energy flows, with
  capability-aware `auto` ↔ `heat_cool` mapping so HA-native strings go on the wire
  rather than pre-encoded ints.
- **Main heap 240 KB → 228 KB, to _grow_ the stack.** On the C6 the stack is whatever
  the linker leaves between `_bss_end` and `_stack_start`, so shrinking the heap is
  what grows the stack — the root cause of the radio-path crash under the new climate
  session. (The reclaimed pool sits above the stack and can't help.)
- `crates/finder` — pure `no_std` nearest-peer range/proximity meter.

## [0.4.0] — 2026-07-20

The feature-integration wave: light-sleep power lands as the default, and four
smol-port features come online as launcher apps + a new sensors readout, all
riding the on-demand framebuffer + Slint-overlay architecture (no scene-suspend
for the button/display apps).

### Added
- **Light-sleep AOD** — the idle/ambient state now enters HP light-sleep between
  wakes (timer + touch/GPIO wake sources), with the CO5300's self-refresh GRAM
  holding the dim clock. Wakes force an external-RTC (`PCF85063`) time read so the
  minute flip never looks stuck despite embassy-time freezing during sleep.
- **WLED WiZmote remote** (launcher → SYSTEM) — a Slint overlay whose tiles
  (On/Off, presets 1-4, dim ±, night) broadcast ESP-NOW WiZmote frames via the
  new `wled-wizmote` crate, reusing the mesh broadcast peer.
- **RSSI treasure-hunt** (launcher → GAMES) — a warmer/colder hunt driven live
  from the mesh roster's smoothed RSSI (`hunt` + `rssi` crates), with trend
  arrows, proximity buckets, and hold-to-confirm FOUND.
- **Home energy screen** (launcher → SYSTEM) — house battery / solar / grid
  overlay (placeholder data until the HA/ESP-NOW feed lands).
- **C6 die temperature** on the Sensors page (`esp_hal` TSENS).
- Workspace reorganised: pure-logic `no_std` crates (`rssi`, `hunt`,
  `wled-wizmote`, `ota-proto`, `scan-model`) under `crates/*`, host-unit-tested.

### Deferred
- **Voice-to-text (MC5)** — mic-capture + STT modules merged but not wired
  (clean TODO at the i2s_rx site); awaiting the full capture-task snippet.

## [0.3.1] — 2026-07-20

On-glass fixes on top of v0.3.0: WiFi actually works, the radio toggles are
finger-sized, and the sensors page shows steps.

### Fixed
- **WiFi toggle** — no longer drops taps (removed a debounce window that silently
  ate a WIFI tap within 1s of the periodic idle check) and no longer silently
  no-ops without credentials — it now toasts "No WiFi credentials — set in
  Settings". With credentials present, WiFi auto-connects and the toggle is a
  responsive off↔on.

### Changed
- **Larger radio tap targets** — the WIFI / BLE / MESH hit areas grew 66×44 →
  78×64 (+72%) so they're reliably finger-tappable; the visible dots stay aligned
  with the battery pill, hit areas span the top strip without clipping the corners.

### Added
- **Step count on the Sensors page.**

## [0.3.0] — 2026-07-20

Migration tail + hardening on top of the Slint shell: always-on display, the
Mesh Familiar on the clock, LP-core power reporting, and — the headline fix —
games and Settings that launch in **any** radio state, after the framebuffer
was reworked to half-resolution.

### Added
- **AOD (always-on display)** rendered by the Slint shell — at the dim idle
  state the clock repaints only on the minute flip (a black `aod` overlay);
  the full shell returns on touch.
- **Mesh Familiar** status cluster on the clock page (known / holding / mood /
  hunger / growth-stage), fed from `FamState`, plus **gyro parallax** that
  nudges the clock face from the accelerometer.
- **LP-core status row** on the Power page.
- **Boot & remote page control** — the watch boots to the persisted default
  page (CFG `S`), and the live remote page-switch is honored again.
- **Finger-friendly radio toggles** — larger WIFI / BLE / MESH tap targets, and
  **MESH is now a real on/off toggle**.

### Changed
- **Half-resolution framebuffer** — the game/Settings framebuffer is now
  205×251 RGB332 (~51 KB, nearest-neighbor upscaled 2× on flush) instead of the
  full-res ~201 KB. Apps still draw at full 410×502 (unchanged); only the
  backing store shrank. This is what lets games launch with WiFi and/or mesh on
  — the full-res buffer could not share the C6's single SRAM region with the
  resident Slint scene + radio stacks.
- **Mesh radio decoupled from WiFi credentials** — ESP-NOW needs only the STA
  radio (PHY) up, not an AP association, so the radio is started when MESH is
  toggled on. Mesh now works with no WiFi credentials.
- The Slint scene is dropped while a game runs and recreated (with all live
  state re-pushed) on return.

### Fixed
- **Games / Settings would not launch** ("RAM busy") — once the Slint scene was
  resident the on-demand full-res framebuffer had no contiguous room. The
  half-res buffer resolves it in every radio state. (Bumping the heap region was
  a dead end: the framebuffer's SRAM competes with the scene-build stack, and
  264–288 KB heaps boot-looped building the Slint scene.)
- **MESH toggle did nothing** — mesh was gated behind the credential-locked WiFi
  path and never ran without creds.
- One-shot shell properties (LP-core row, radios, Familiar, brightness, …) no
  longer blank out after returning from a game — the recreated scene re-pushes
  them.

## [0.2.0] — 2026-07-20

The **Slint UI migration**: the watchface shell is rebuilt on the
[Slint](https://slint.dev) toolkit, replacing the hand-rolled
`embedded-graphics` shell. Games and Settings keep their embedded-graphics
rendering and take the panel over through a mode switch.

### Added
- **Slint watch shell** — a five-page swipe carousel (Clock, Sensors, System,
  Power, Mesh) plus persistent chrome (WiFi/BLE/mesh radio dots, battery pill,
  page dots) and an app **launcher overlay** (Flickable list), all declared in
  `ui/slint/*.slint` and driven from Rust via `ShellUi`.
- **Shared Slint platform module** (`src/ui/slint_platform.rs`) — `EspPlatform`
  + line flusher hoisted out of the `slint-demo` binary so the demo and the
  main firmware share one backend.
- **Live pages**: sensors (accel/gyro/IMU-temp at 100 ms), system (heap/uptime/
  battery, live `esp_alloc` stats), power (per-subsystem mA + runtime estimate +
  brightness slider + reboot), mesh roster (SMOLv1 realm names, RSSI, age).

### Changed
- **Line-streamed, framebuffer-free rendering** — the shell renders through the
  Slint software renderer, streaming 2-line RGB565 strips (~1.6 KB) straight to
  panel GRAM. The shell no longer holds a full-screen framebuffer.
- **On-demand framebuffer** — the ~202 KB RGB332 framebuffer is now allocated
  only when an embedded-graphics app (game/Settings) launches, via a fallible
  `try_reserve_exact`, and freed on exit back to the shell.
- Firmware version now sourced once from `CARGO_PKG_VERSION` (single source of
  truth on the system page).

### Fixed
- **Boot out-of-memory** — because the shell boots framebuffer-free and apps
  allocate on demand, the watch no longer risks the boot-time OOM the always-on
  framebuffer could cause on the PSRAM-less C6 (512 KB SRAM). On allocation
  failure the app launch is refused with a toast and the shell stays up.

### Project
- **Open-sourced** under a dual **MIT OR Apache-2.0** license, with a README and
  upstream attribution to `infinition/waveshare-watch-rs` (the ESP32-S3 Rust
  watch firmware this is a C6 port of). Published at
  `github.com/jphein/esp32c6-watch`.

## [0.1.0]

Initial firmware for the Waveshare ESP32-C6-Touch-AMOLED-2.06: embedded-graphics
watchface, games (Snake, World Snake, 2048, Tetris, Flappy, Maze), SMOLv1 mesh
over ESP-NOW, WiFi STA + NTP, BLE GATT, MQTT → Home Assistant, weather, HTTP OTA,
QMI8658 hardware pedometer, and the CO5300 AMOLED / FT3168 touch drivers.
