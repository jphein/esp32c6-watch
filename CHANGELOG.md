# Changelog

All notable changes to this project are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/); this project uses
[Semantic Versioning](https://semver.org/).

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
