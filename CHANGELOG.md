# Changelog

All notable changes to this project are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/); this project uses
[Semantic Versioning](https://semver.org/).

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
