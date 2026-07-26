# esp32c6-watch

**100% Rust `no_std` firmware for the [Waveshare ESP32-C6-Touch-AMOLED-2.06](https://www.waveshare.com/wiki/ESP32-C6-Touch-AMOLED-2.06) smartwatch.**

[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-35e0b0)](#license)
[![platform](https://img.shields.io/badge/platform-ESP32--C6%20%C2%B7%20RISC--V-3a7bd5)](https://www.espressif.com/en/products/socs/esp32-c6)
[![rust](https://img.shields.io/badge/rust-no__std%20%C2%B7%20edition%202024-dea584)](https://www.rust-lang.org)

Built on [`esp-hal`](https://github.com/esp-rs/esp-hal) + [Embassy](https://embassy.dev), rendering to the onboard CO5300 AMOLED over QSPI DMA. No RTOS beyond the async executor, no PSRAM, no cloud — a full smartwatch shell, an ESP-NOW mesh, a creature that hops between boards, and a menagerie of games, all on a single RISC-V microcontroller.

> 🌐 **Showcase site:** <https://jphein.github.io/esp32c6-watch/>
> 🔗 **Sibling project:** the [`smol`](https://github.com/jphein/smol) mesh fleet — the SMOLv1 wire format is shared between the two, and improvements flow both ways.

---

## Features

### Slint UI shell — *shipped (v0.2.0)*
The watchface shell runs on the [Slint](https://slint.dev) toolkit's `no_std` software renderer:

- **Five-page carousel** — Clock, Sensors, System, Power, and Mesh — with persistent chrome (radio dots, battery pill, page dots).
- **Launcher overlay** slides up from the Clock page.
- **Always-on display (AOD)** — a dimmed, low-refresh scene for the idle state.
- **The Mesh Familiar** — a living creature that inhabits one board at a time and migrates across the mesh when the board it's on loses power; other nodes show a Weasley-clock pointer toward wherever it currently is.

The renderer streams two-line RGB565 strips (~1.6 KB) straight to panel GRAM, so the ~202 KB framebuffer is gone from boot and allocated on demand only when a full-frame `embedded-graphics` app takes the panel over.

### The rest

- **SMOLv1 ESP-NOW mesh** — routerless fleet networking (`HELLO`/`ACK`/`TIME`/`CFG`/`RELAY` frames). Loop-free time authority: the watch runs its own NTP and both adopts time from and serves it to the fleet.
- **Sixteen-app launcher** — a paged 3×3 grid in three sections. *GAMES:* six `embedded-graphics` titles (Snake, World Snake, 2048, Tetris, Flappy Bird, a tilt-controlled Maze) plus the RSSI treasure **Hunt**. *SYSTEM:* the **Settings** hub (five paged sections, with a scan-based WiFi picker and a QWERTY keyboard for credentials), **Lights**, **Climate**, **Energy**, **Ping**, the **WLED** remote, and the **Theme** picker. *AUDIO:* **Voice** and **Sound**.
- **Watch-to-watch ping** — a hero button greets the other watch by sigil over the mesh (additive SMOLv1 `PING`/`PINGACK` frames with delivery confirmation). The receiver is unmissable: it wakes from always-on *or* fully off, **suspends a running game** to take the whole screen, blooms a full-screen accent-ring pulse naming the sender, plays a four-note rising major arpeggio, and always leaves a timestamped card in the notification shade — then puts the game back exactly where it was.
- **Edge-gesture shell** — a bottom-edge swipe-up opens the launcher from any watchface page; a bottom-edge *hold* raises an **app switcher** (suspend / resume / kill, with a corner badge for what's still running); a top-edge swipe-down pulls down a **notification shade** fed by MQTT plus system events. A power-button long-press opens a **SHUTDOWN / REBOOT** menu, with the AXP2101's 4-second hardware failsafe still intact underneath.
- **A realtime UI** — WiFi, OTA and scanning live in a dedicated `net_task` that exclusively owns the radio, behind a hold-mask + exponential-backoff state machine; the render loop never blocks on the network. Measured on glass *under a dead-AP outage*: worst frame **202 ms**, `arm_max` **135 ms** — the same outage used to freeze the watch for **15 seconds**. A loop-budget rule (>10 ms of blocking in any arm is a bug) plus a per-arm watchdog keep it that way. Association is deliberate rather than lucky: esp-radio has no 802.11r, so the watch **roams in firmware** — a multi-pass candidate scan pins the strongest BSSID explicitly, and it reassociates when the link sits weak and a better AP is available.
- **Connectivity** — WiFi STA with NTP, a BLE GATT server ([`trouble-host`](https://github.com/embassy-rs/trouble)), MQTT → Home Assistant, and a live **weather** fetch.
- **Voice & audio** — an **AUDIO** launcher section (Voice / Sound tiles). **Voice push-to-talk** streams live ES7210 mic capture over WiFi to a LAN STT gateway and shows the transcript on-glass — pressing before the link is up latches and fires itself the moment WiFi lands, so it is never press-twice; the **Sound** app is a live dB meter, waveform, and a 12-band FFT **spectrum analyzer** (log-spaced, factory parity), with a digital gain stepper.
- **Volume and mappable buttons** — a speaker level (0–15, plus mute) that every chime, beep and touch tick honours, a touch **volume HUD**, and BOOT/POWER short- and long-presses bound to actions from *Settings › Buttons* (default: tap = volume ∓, hold = launcher / power menu). The power button always wakes the watch first, so a press in the dark can never trigger an action you can't see.
- **Pedometer** — hardware step counting on the QMI8658 IMU's dedicated engine (keeps counting while the IMU is otherwise idle).
- **Power management** — CPU clock control, live per-subsystem current estimation, battery monitoring, and a brightness slider.
- **OTA updates** — HTTP over-the-air firmware into an A/B partition layout (two 6 MB slots), pull *and* push, surviving a mid-download reconnect. The running slot is derived from the **MMU** rather than from `otadata`, and every flash write is range-checked — an update can never overwrite the image it is executing from.
- **`defmt-rtt` debug** — feature-gated structured logging over an RTT channel (probe-rs), off by default.

*Plus:* a **4-scheme theme system** (Midnight / Paper / **Amber** default / Violet) with an on-glass picker, a **plugin/app registry** (each launcher app is a single registration), **pressed-state touch feedback on every control**, a **paged 3×3 launcher**, **wake gesture hints**, **wrist-raise wake**, **touch sounds on every tap** with a persisted toggle, user toggles (mesh / WiFi intent / mic gain / theme) that **survive reboots**, and a per-device **sigil identity** derived from the efuse MAC (this fleet: `eldritch-lantern` & `mythic-throne`).

Radios (WiFi/BLE) are **off at boot** and toggled from the watchface.

## Hardware

| | |
|---|---|
| **Board** | Waveshare ESP32-C6-Touch-AMOLED-2.06 |
| **MCU** | ESP32-C6 · single-core RISC-V @ up to 160 MHz · target `riscv32imac-unknown-none-elf` |
| **Memory** | 512 KB SRAM on-chip — **no PSRAM** (the RGB332 app framebuffer lives in SRAM) |
| **Display** | CO5300 AMOLED · 410×502 · QSPI with DMA |
| **Touch** | FT3168 capacitive controller (I²C) |
| **IMU** | QMI8658 6-axis accel + gyro, with a hardware pedometer engine |
| **Audio** | **out:** ES8311 mono speaker/playback codec (U1, I²C `0x18`) · **in:** ES7210 4-channel mic ADC (U8, I²C `0x40`) — both on one shared I²S bus |
| **Radios** | WiFi 6 (2.4 GHz) · Bluetooth 5 LE · native 802.15.4 (Zigbee/Thread) |
| **Flash** | 16 MB · A/B OTA partition layout |
| **Power** | battery-backed RTC · CPU clock scaling |

### Pin map

| Peripheral | Bus | Pins |
|---|---|---|
| CO5300 AMOLED | QSPI | `SCLK` GPIO0 · `SDIO0..3` GPIO1–4 · `CS` GPIO5 · `RST` GPIO11 |
| Shared I²C | I²C | `SDA` GPIO8 · `SCL` GPIO7 |
| FT3168 touch | I²C @ `0x38` | `INT` GPIO15 · `RST` GPIO10 |
| QMI8658 IMU | I²C @ `0x6B` | *(shared I²C bus)* |
| RTC (PCF85063) | I²C @ `0x51` | *(shared I²C bus)* |
| Shared I²S clocks | I²S | `MCLK` GPIO19 · `BCLK/SCLK` GPIO20 · `WS/LRCK` GPIO22 · speaker amp-enable GPIO6 |
| ES8311 speaker DAC | I²S + I²C @ `0x18` | `DSDIN` GPIO23 — playback data out (SoC → codec) |
| ES7210 mic ADC | I²S + I²C @ `0x40` | `SDOUT1` GPIO21 — capture data in (ES7210 → SoC, via `R47`) |

> **Audio topology:** playback and capture are **two different chips** sharing one I²S clock domain. The **ES8311** (U1) is the speaker/playback codec — the SoC sends DAC data to it on `DSDIN`/GPIO23. The two onboard MEMS mics (MIC1/MIC2) are analog inputs to a **separate ES7210** 4-channel ADC (U8), whose `SDOUT1` drives the SoC on GPIO21. The ES8311's own ADC is **not** wired to the SoC. *(Verified against the V1.0 schematic, the Waveshare `xiaozhi` vendor sources, and the vendor firmware image.)*

### Flash layout (`partitions.csv`)

| Partition | Type | Size |
|---|---|---|
| `nvs` / `otadata` / `phy_init` | data | 28 KB |
| `ota_0` | app (running) | 6 MB |
| `ota_1` | app (OTA target) | 6 MB |
| `config` | spiffs | 64 KB |

## Build & flash

Requires the stable Rust toolchain with the RISC-V bare-metal target (pinned in [`rust-toolchain.toml`](rust-toolchain.toml) — `riscv32imac-unknown-none-elf`) and [`espflash`](https://github.com/esp-rs/espflash).

```sh
# 1. Install the flash tool
cargo install espflash

# 2. Configure WiFi credentials — creds live only in your local
#    .cargo/config.toml, which is gitignored and never committed.
cp .cargo/config.example.toml .cargo/config.toml
#    then edit .cargo/config.toml and set, under [env]:
#        WIFI_SSID = "YourNetwork"
#        WIFI_PASS = "YourPassword"

# 3. Build the release firmware
cargo build --release

# 4. Flash + monitor
espflash flash --monitor --chip esp32c6 \
  target/riscv32imac-unknown-none-elf/release/esp32c6-watch
```

The `.cargo/config.example.toml` sets `espflash flash --monitor --chip esp32c6` as the cargo runner, so **`cargo run --release`** builds and flashes in one step.

## Project layout

```
src/
├── drivers/       co5300 (AMOLED), qspi_bus, framebuffer (on-demand)
├── peripherals/   ble, imu, touch, rtc, power, power_stats, cpu_clock, die_temp,
│                  audio (shared I²S + ES8311), audio_out (playback seam),
│                  es7210 (mic ADC), mic_capture, config (dual-slot record, v6)
├── net/           net_task (the sole radio owner), smol_mesh, familiar, weather,
│                  mqtt_ha, mqtt_climate, voice_stt, ota_http, sigil, names
├── ui/            slint_shell, slint_platform
├── apps/          registry (single source of truth), session (suspend/resume/kill),
│                  snake, world_snake, game2048, tetris, flappy, maze
├── notify.rs      the notification store behind the shade
├── guarded_flash.rs   range-checked, sector-rounded flash writes
├── board.rs       pin map + board constants
├── debug_console.rs   serial UI-automation console (`debug-console` feature)
└── main.rs        single Embassy event loop; owns all peripherals
crates/           pure-logic `no_std` crates, host-unit-tested: climate-model, hunt, rssi,
                  finder, mic-dsp, scan-model, ota-proto, sigil-id, flash-guard,
                  wled-wizmote — plus
                  the vendored i-slint-renderer-software fork (partial rendering v2)
ui/slint/         the Slint scene: shell.slint, controls.slint (shared components),
                  theme.slint / theme_overlay.slint, and one file per page or overlay —
                  clock, sensors, system, power, mesh, launcher, settings, keyboard,
                  switcher, shade, power_menu, ping, volume, climate, energy, lights, voice,
                  soundlevel, wled, hunt, scan
tools/            watchctl (USB/WiFi debug rig), ota_push.sh (push OTA),
                  ui_test.py (UI automator), watch_soak.py (boot-stability harness)
ha/               the `esp32c6_watch` Home Assistant custom component
ha-bridge/        Node-RED climate + energy flows — superseded in v0.12.0 by the
                  HA component's own MQTT bridge; kept for reference
docs/             debugging.md (agent field guide), deploy notes,
                  vendor-firmware analysis, design specs + plans
```

Core stack: `esp-hal` ~1.1 · `esp-rtos` 0.3 · `esp-radio` 0.18 (wifi/ble/coex/esp-now) · Embassy (executor/net/time/sync) · `slint` 1.17 · `trouble-host` 0.6 · `embedded-graphics` 0.8 · `heapless` 0.9.

## Status & roadmap

The roadmap lives in the [issue tracker](https://github.com/jphein/esp32c6-watch/issues) — every item below links to its issue.

### ✅ Shipped

- *v0.2.0* — Slint UI shell (5-page carousel, launcher, AOD, Mesh Familiar), on-demand framebuffer, SMOLv1 mesh, six games, weather, pedometer, BLE, OTA A/B layout, `defmt-rtt` debug.
- *v0.3.0* — migration tail: **AOD** rendered by the Slint shell, the **Mesh Familiar** status cluster + gyro parallax on the clock, an LP-core power row, and the **half-resolution framebuffer** (205×251 RGB332, ~51 KB, upscaled 2× on flush) that finally let games and Settings launch in *any* radio state — the full-res buffer couldn't share the C6's single SRAM region with a resident Slint scene plus the radio stacks. Mesh also decoupled from WiFi credentials (ESP-NOW needs only the STA PHY).
- *v0.3.1* — on-glass fixes: the WiFi toggle stopped dropping taps and now toasts when credentials are missing, the WIFI/BLE/MESH hit areas grew 66×44 → 78×64 (+72 %), and the Sensors page shows step count.
- *v0.4.0* — light-sleep AOD, WLED WiZmote remote, RSSI treasure hunt, home-energy screen, die-temperature, host-tested `no_std` workspace crates.
- *v0.5.0* — **Home Assistant climate control**: a bidirectional MQTT climate session, a Climate list + per-device detail overlay, and `crates/climate-model` as the pure `no_std` state core; the home-energy screen goes **live** (real battery/solar/grid over MQTT) with Node-RED bridge flows (`ha-bridge/`); main heap trimmed 240 → 228 KB to grow the C6 stack.
- *v0.5.1* — boot-time **stack-floor guardrail**, optimistic setpoints that flush on close, Energy connection gating, and a shared `BackChevron` + 72 px setpoint steppers.
- *v0.6.0* — voice push-to-talk (LAN STT gateway), speaker playback, touch-responsiveness + launcher fixes, AUDIO launcher section.
- *v0.7.0* — **working mic** ([#7](https://github.com/jphein/esp32c6-watch/issues/7): the mics are on a separate **ES7210** ADC — driver + ALDO1 power rail), real on-glass **speech-to-text**, mic gain control, **plugin/app registry**, **4-scheme theme system**, the `esp32c6_watch` **Home Assistant component** (climate/energy + a `media_player` speaker queue).
- *v0.8.0* — **touch-feedback overhaul** (shared `controls.slint` library, bold pressed states on ~52 targets, ≥44 px hit areas), **paged 3×3 launcher**, **partial rendering v2** ([#18](https://github.com/jphein/esp32c6-watch/issues/18): vendored renderer, even-aligned dirty regions), **wrist-raise wake**, IMU step-counter fix, AXP2101 charger profile, **UI test automator** (serial debug console + `tools/ui_test.py`), CO5300 panel confirmed ([#17](https://github.com/jphein/esp32c6-watch/issues/17)).
- *v0.8.1* — AOD light-sleep panic fix ([#43](https://github.com/jphein/esp32c6-watch/issues/43): esp-hal RC_FAST calibration ticks + a never-panic gate).
- *v0.8.2* — duo-tone **aurora wake gesture hints**.
- *v0.8.3* — **reliable zero-touch OTA** ([#25](https://github.com/jphein/esp32c6-watch/issues/25) validated end-to-end): pull *and* push (retained MQTT announce + monotonic build gate, `tools/ota_push.sh`), retry that survives link drops, mesh suppression during updates.
- *v0.8.4* — **per-device sigil identity** ([#34](https://github.com/jphein/esp32c6-watch/issues/34)) from the efuse MAC: `eldritch-lantern` & `mythic-throne`, MAC-derived mesh node ids, per-watch MQTT client ids + OTA topics (`ota_push.sh --target <sigil>`), sigil BLE advertising.
- *v0.8.5* — **sound restored: shared I²S TX playback seam** ([#23](https://github.com/jphein/esp32c6-watch/issues/23)): `audio_out::play_pcm()` (mono 16 kHz s16le) substitutes samples into the always-running silent-clock ring — the mic's clock master never stops for a beep; amp+codec power only while a clip plays; half-duplex capture gate; Snake food beep + launcher/UPDATE-FIRMWARE tap-clicks; `beep` console probe.
- *v0.8.7* — **room-aware Lights plugin** ([#39](https://github.com/jphein/esp32c6-watch/issues/39)): hero button → MQTT → HA resolves the watch's Bermuda room and toggles it, retained state back; plus a BLE-sleep lockup hotfix (BLE-on tick-idles AOD) and stable efuse-derived BLE identities ([#47](https://github.com/jphein/esp32c6-watch/issues/47)).
- *v0.8.8* — **the fastpath release**: state-wake rendering, DHCP-gated session open, press gating, Energy-unreachable only on a real LWT, freeze-proof dual-slot config mirror.
- *v0.9.0* — **touch sounds everywhere** ([#49](https://github.com/jphein/esp32c6-watch/issues/49): one hoisted tap hook across both input families, persisted toggle), a scene-resident **Settings hub** (SOUND/DISPLAY/RADIOS/NETWORK/SYSTEM — the framebuffer Settings app and T9 keyboard are gone), a **scan-based WiFi picker + 4-layer QWERTY keyboard**, **config record v5** completing the [#46](https://github.com/jphein/esp32c6-watch/issues/46) persistence migration (mesh · WiFi intent · touch sound · mic gain), and a ~397 KB **glyph-set consolidation** that brought the app image back inside the 4 MB slot behind a new `ota_push.sh` slot-fit gate.
- *v0.9.1* — the ESP-NOW **channel pin now yields to an active WiFi intent** (it was dropping association auth frames on any watch with MESH persisted on) + a truthful mesh node-id log.
- *v0.10.0* — **the realtime release**: WiFi/OTA/scan move off the render loop into a dedicated `net_task` ([#53](https://github.com/jphein/esp32c6-watch/issues/53)) — *worst frame 202 ms under a dead-AP outage, where the old code froze for 15 s*; the **edge-gesture shell** (swipe-up launcher [#29](https://github.com/jphein/esp32c6-watch/issues/29), bottom-hold **app switcher** [#31](https://github.com/jphein/esp32c6-watch/issues/31), top-swipe **notification shade** [#32](https://github.com/jphein/esp32c6-watch/issues/32)); a **power-button SHUTDOWN/REBOOT menu** ([#48](https://github.com/jphein/esp32c6-watch/issues/48)); a **12-band FFT spectrum analyzer** ([#30](https://github.com/jphein/esp32c6-watch/issues/30)); and **OTA slots grown 4 MB → 6 MB** ([#50](https://github.com/jphein/esp32c6-watch/issues/50), cable-deployed — the margin was down to 5.4 KB).
- *v0.10.1* — **the safety release**: a CRITICAL fix for OTA overwriting the *running* slot ([#55](https://github.com/jphein/esp32c6-watch/issues/55) — it boot-loop-bricked a watch; the running slot now comes from the MMU, never `otadata`, behind a range-checking flash guard), overlays that swallow taps ([#54](https://github.com/jphein/esp32c6-watch/issues/54)), a multi-pass WiFi scan ([#56](https://github.com/jphein/esp32c6-watch/issues/56)), and **firmware WiFi roaming** ([#57](https://github.com/jphein/esp32c6-watch/issues/57), shipped as `v0.10.1-roam`) — esp-radio has no 802.11r, so the watch pins the strongest BSSID itself and reassociates on a weak link.
- *v0.11.0* — **press-once voice** ([#22](https://github.com/jphein/esp32c6-watch/issues/22): an early PTT press latches and auto-fires when the link lands) and **watch-to-watch ping** ([#35](https://github.com/jphein/esp32c6-watch/issues/35): mesh `PING`/`PINGACK`, a sigil-named full-screen pulse and two-tone chime on the receiver). `mythic-throne` took this release **over the air, zero-touch** — the first such self-update since the v0.10.1 flash-safety fix, and the payoff for the net_task, the slot guard, and roaming all landing together.
- *v0.12.1* — **the stability fix** ([#61](https://github.com/jphein/esp32c6-watch/issues/61)): esp-radio 0.18's WiFi blob null-dereferenced in `ppRxFragmentProc` during scan/associate — a deterministic panic ~2.2 s into boot, 100 % of the time, into a reboot loop. Disabling RX AMPDU aggregation took the crash rate from **100 % to 0 %** on both watches with roaming intact; `tools/watch_soak.py` is the harness that measured it.
- *v0.12.0* — **the unmissable ping** ([#58](https://github.com/jphein/esp32c6-watch/issues/58): a four-note rising arpeggio, and a received ping now suspends a running game to take the whole screen and always logs a timestamped shade card), **volume + mappable buttons** ([#59](https://github.com/jphein/esp32c6-watch/issues/59): a speaker level every sound honours, a touch volume HUD, and BOOT/POWER short/long bound to actions in *Settings › Buttons*, persisted in config record v6), and **live Climate + Energy** ([#60](https://github.com/jphein/esp32c6-watch/issues/60)) — the `esp32c6_watch` HA component (v0.3.0) now publishes the retained `watch/*` MQTT topics the firmware actually reads, replacing the retired Node-RED bridge, with the `media_player` speaker restored.

### 🧭 Gesture shell & UI

- [#28](https://github.com/jphein/esp32c6-watch/issues/28) — AOD pixel-shift (burn-in) + typography token sweep
- [#45](https://github.com/jphein/esp32c6-watch/issues/45) — **Face Manager**: long-press the clock to pick faces, reorder/add/remove carousel pages
- [#52](https://github.com/jphein/esp32c6-watch/issues/52) — **Complication Manager**: editable watchface slots rendering any plugin or system surface (builds on [#45](https://github.com/jphein/esp32c6-watch/issues/45))
- [#44](https://github.com/jphein/esp32c6-watch/issues/44) — **Plugin Manager**: toggle + configure plugins on-glass, registry-driven
- [#10](https://github.com/jphein/esp32c6-watch/issues/10) — Emoji-expression face (vendor parity)

### 🔊 Audio pipeline

*(the base of this section — the shared I²S TX playback seam, [#23](https://github.com/jphein/esp32c6-watch/issues/23) — shipped in v0.8.5: `audio_out::play_pcm()`)*

> **Bluetooth earbuds are a permanent no-go on this silicon** ([#62](https://github.com/jphein/esp32c6-watch/issues/62), closed after a forensic dig — recorded here so it isn't re-litigated). The ESP32-C6 has **no ISO link layer**: ESP-IDF gates LE Audio on `SOC_BLE_ISO_SUPPORTED`, which is H4-only, and Espressif's current controller blob ships an `ble_ll_iso` object of 464 bytes with **zero ISO symbols**. The upstream IDF request was closed *"Won't do"* in 2024, not shipped. `esp-radio`'s HCI transport can't carry ISO packets at any version, and `trouble`'s `iso.rs` is a git-only stub with no CIS state machine. Both unicast CIS *and* Auracast need that same missing layer, so **no earbud model changes this**; classic A2DP was never possible either. Real LE Audio would need an ESP32-H4-class revision, which drops WiFi. The live alternatives: HA `media_player` to room speakers ([#24](https://github.com/jphein/esp32c6-watch/issues/24)), or raw PCM over L2CAP CoC to a custom receiver — 256 kbit/s against ~720 kbit/s of measured CoC throughput, no codec required.

- [#33](https://github.com/jphein/esp32c6-watch/issues/33) — **Music player**: Navidrome/Subsonic client + internet radio (KVMR) via a LAN PCM bridge
- [#11](https://github.com/jphein/esp32c6-watch/issues/11) — TTS playback — the watch speaks replies
- [#12](https://github.com/jphein/esp32c6-watch/issues/12) — LLM conversation turn (STT → LLM → TTS)
- [#13](https://github.com/jphein/esp32c6-watch/issues/13) — On-device wake word (ESP-SR WakeNet FFI)
- [#14](https://github.com/jphein/esp32c6-watch/issues/14) — Cloud MCP device-control tool server (with the LLM)

### ⌚⌚ Fleet (two watches)

- [#37](https://github.com/jphein/esp32c6-watch/issues/37) — **Super Find**: multi-radio watch finder (mesh + BLE + HA + WiFi + 802.15.4 + LoRa fusion, Find-My scream mode)
- [#38](https://github.com/jphein/esp32c6-watch/issues/38) — **Meshtastic BLE client**: GPS, LoRa messaging, nodes list
- [#36](https://github.com/jphein/esp32c6-watch/issues/36) — *Epic:* smol parity — peer-sourced mesh OTA, RELAY/CFG downlink, mesh multiplayer games

### 🏠 Home Assistant

- [#24](https://github.com/jphein/esp32c6-watch/issues/24) — Deploy + verify the HA component; firmware announce-poller (**speaker end-to-end**)
- [#40](https://github.com/jphein/esp32c6-watch/issues/40) — **Watering plugin**: sprinkler zones with countdowns + auto-off safety ("water where I'm standing" via per-zone BLE proxies)
- [#41](https://github.com/jphein/esp32c6-watch/issues/41) — **Laundry**: LG ThinQ → HA + machine-done notifications on the wrist
- [#42](https://github.com/jphein/esp32c6-watch/issues/42) — **Cam viewer**: bridge-transcoded RGB565 stills, socket→GRAM streaming, doorbell jump

### 🔧 Platform & tooling

- [#15](https://github.com/jphein/esp32c6-watch/issues/15) — Wi-Fi provisioning — SoftAP captive portal
- [#16](https://github.com/jphein/esp32c6-watch/issues/16) / [#26](https://github.com/jphein/esp32c6-watch/issues/26) — On-glass verifies: charger profile, steps, wrist-raise tuning
- [#51](https://github.com/jphein/esp32c6-watch/issues/51) — Firmware **TCP debug server** (`:5555`, token-gated) — the WiFi half of the `watchctl` rig

### 📡 Radio frontier

- [#2](https://github.com/jphein/esp32c6-watch/issues/2) — 802.15.4 radio enablement (single-image WiFi + Zigbee/Thread)
- [#1](https://github.com/jphein/esp32c6-watch/issues/1) — Radio Scan v2: 802.15.4-2015 frame parsing (Thread/Zigbee PAN IDs, channels, RSSI)

## Credits

This firmware is a port of [**infinition/waveshare-watch-rs**](https://github.com/infinition/waveshare-watch-rs) — the original Rust watch firmware for the ESP32-**S3**-Touch-AMOLED-2.06, by **Fabien (infinition)** — adapted to the ESP32-**C6** board. C6 differences include no PSRAM (RGB332 framebuffer in SRAM), no SD card slot, no TE pin in the BSP, and a different GPIO map. Deep thanks to the upstream author for the foundation.

It is also the ESP32-C6 hardware target within the [**smol**](https://github.com/jphein/smol) fleet project; the SMOLv1 mesh protocol is shared between the two.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
