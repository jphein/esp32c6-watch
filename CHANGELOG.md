# Changelog

All notable changes to this project are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/); this project uses
[Semantic Versioning](https://semver.org/).

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
