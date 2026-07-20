# Slint UI Migration — Design Spec

Date: 2026-07-19
Status: approved (approach A, JP 2026-07-19)

## Goal

Replace the embedded-graphics watchface shell in the main firmware with Slint,
building on the proven `slint-demo` binary. Games and Settings keep their
embedded-graphics rendering and take the display over via a mode switch.

## Context

- Board: Waveshare ESP32-C6-Touch-AMOLED-2.06 (CO5300 410x502 over QSPI DMA,
  FT3168 touch, no PSRAM — 512KB SRAM total).
- Today `main.rs` owns a boot-allocated RGB332 framebuffer (~202KB heap) that
  every page and app draws into; heap is 240KB DRAM + 56KB reclaimed = 296KB,
  leaving ~94KB for radio stacks, mesh, and apps.
- `slint-demo` proves the alternative: Slint software renderer streaming
  2-line RGB565 strips (1.6KB) straight to panel GRAM, no framebuffer, touch
  mapped to Slint pointer events, full repaint per frame.
- Prior art surveyed: infinition/waveshare-watch-rs (S3 ancestor, stays eg,
  leans on PSRAM), mgrenonville/esp32-mipidsi-clock (Slint+embassy no_std on
  C6, desktop-simulator pattern), zhangzqs/esp-clock-rs (message-driven,
  std, heavier than needed).

## Scope

**In:** watchface pages (Clock, Sensors, System, Power, Mesh incl. Familiar
creature), launcher, persistent chrome (radio dots, battery pill, page dots),
AOD mode — all become Slint. On-demand framebuffer for eg apps.

**Out (unchanged):** games (Snake, World Snake, 2048, Tetris, Flappy, Maze),
Settings + T9 keyboard (they are `AppState` apps and ride the same handover
seam as games), sleep/AOD state machine logic, WiFi window, mesh/familiar
protocol code, all drivers.

**Non-goals:** porting games to Slint; a desktop simulator binary
(`slint-viewer` with dummy properties covers UI iteration); visual redesign
beyond what porting requires.

## Architecture

- `src/ui/slint_platform.rs` (new): `EspPlatform` + `TwoLineFlusher` hoisted
  from `src/bin/slint_demo.rs`; the demo re-uses the shared module.
- The `.slint` source moves to `ui/slint/` and splits per page
  (`shell.slint` root importing `clock.slint`, `sensors.slint`, ...): five
  watchface pages + launcher + chrome + `aod` state. Compiled via existing
  `build.rs` (slint-build) pointed at `ui/slint/shell.slint`; the demo bin
  keeps compiling its own `watchface.slint` until stage 1 retires it.
- `main.rs` keeps the single event loop and ownership of all peripherals.
  Per iteration it is in one of two render modes:
  - **Shell mode** (`AppState::Watchface | Launcher`): poll inputs, push
    changed values into Slint properties, dispatch pointer events,
    `draw_if_needed` via the line flusher.
  - **App mode** (all other `AppState`): existing eg app update/render into
    the on-demand framebuffer + `flush`/`flush_region`, exactly as today.
- Mode switch = which branch the loop takes; the CO5300 driver is stateless
  between flushes, so no driver changes.

## Data flow

- Loop → UI: property setters, called only when the source value changes
  (time 1Hz, battery on its existing cadence, steps 1/min, wifi/ble/mesh
  state, weather). No Slint types escape the UI module.
- UI → loop: Slint callbacks write into `Rc<Cell<...>>` request slots
  (brightness, page navigation, launch-app requests); the loop drains them
  each iteration. Generalizes the demo's `brightness_req` pattern.
- Sleep/AOD: the existing `screen_state` machine stays in Rust. State 1
  (AOD) sets an `aod: bool` property → minimal dim scene, repaint once per
  minute. States 0/2/3 unchanged (brightness/display_off handled in Rust).

## Memory strategy

- Boot no longer allocates the framebuffer. Shell mode runs framebuffer-free.
- `Framebuffer::try_new()` (new, fallible via `try_reserve_exact`) is called
  on app entry; freed on exit back to the shell.
- On allocation failure: Slint toast ("RAM busy"), stay in the shell. This is
  the only new recoverable error path.
- Heap stats (`esp_alloc`) logged at boot and on each mode switch to measure
  Slint scene cost and watch for fragmentation. Fallback if fragmentation
  bites in practice: cache the framebuffer after first successful launch.

## Error handling

- Touch/RTC/power I2C errors: ignore-and-retry, as today.
- Slint init failure at boot: fatal (same class as today's fb alloc).
- App-entry alloc failure: recoverable toast (above).

## Testing

- Per stage: `cargo check`, clippy, both bins build.
- Hardware is the real gate: flash via the espflash runner when the watch is
  on USB; verify boot log, page swipes, app entry/exit, AOD, and a WiFi
  window while in shell mode.
- UI-only iteration: `slint-viewer` with dummy property values.

## Landing order

(Amended at planning time: the full shell is built and exercised through the
`slint-demo` harness first, then `main` cuts over once — this avoids a
parity-reduced intermediate where four pages would temporarily disappear.)

1. Shared `slint_platform` module; demo bin rewired to it.
2. Full Slint shell built page-by-page (clock, sensors, system, power, mesh,
   launcher), verified via the demo harness; `main` untouched.
3. Single cutover: `main`'s watchface+launcher arms replaced by shell mode
   (framebuffer still boot-allocated), then on-demand framebuffer + toast,
   then AOD.
4. Polish (Familiar creature, gyro parallax) and eg shell code deletion.

Each step is a compiling, revertible commit on `feat/slint-shell`; hardware
gates at the cutover, the memory change, AOD, and final ship.

## Open items

- Familiar creature on the Mesh/Clock pages: first cut may be simplified
  Slint shapes; embedded sprite frames if the creature's charm demands it.
- Exact Slint scene heap cost: measured, not assumed, at stage 1.
