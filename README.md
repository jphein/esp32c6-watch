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
- **Seven-app launcher** — six `embedded-graphics` games (Snake, World Snake, 2048, Tetris, Flappy Bird, a tilt-controlled Maze) plus an on-device **Settings** app with a T9 keyboard for entering WiFi credentials at runtime.
- **Connectivity** — WiFi STA with NTP, a BLE GATT server ([`trouble-host`](https://github.com/embassy-rs/trouble)), MQTT → Home Assistant, and a live **weather** fetch.
- **Voice & audio** — an **AUDIO** launcher section (Voice / Sound tiles). **Voice push-to-talk** streams held-button capture over WiFi to a LAN STT gateway (gated on WiFi + DHCP ready), and speaker **playback** (beep) is audible. *Live mic capture from the ES7210 is [in progress](#status--roadmap).*
- **Pedometer** — hardware step counting on the QMI8658 IMU's dedicated engine (keeps counting while the IMU is otherwise idle).
- **Power management** — CPU clock control, live per-subsystem current estimation, battery monitoring, and a brightness slider.
- **OTA updates** — HTTP over-the-air firmware into an A/B partition layout.
- **`defmt-rtt` debug** — feature-gated structured logging over an RTT channel (probe-rs), off by default.

*Also landing (in progress):* a **multi-theme system** — 4 schemes (Midnight / Paper / Amber / Violet) plus a runtime picker — and a **plugin/app registry** that makes each launcher app a single registration. See [Status & roadmap](#status--roadmap).

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
| `ota_0` | app (running) | 4 MB |
| `ota_1` | app (OTA target) | 4 MB |
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
├── drivers/       CO5300 AMOLED, QSPI bus, on-demand framebuffer
├── peripherals/   wifi, ble, imu, touch, rtc, power, power_stats, audio, cpu_clock
├── net/           smol_mesh, familiar, weather, mqtt_ha, ota_http, names
├── ui/            slint_shell, slint_platform, watchface, launcher, power_page, t9_keyboard
├── apps/          snake, world_snake, game2048, tetris, flappy, maze, settings
└── main.rs        single Embassy event loop; owns all peripherals
ui/slint/          the Slint scene: shell.slint + per-page (clock, sensors, system, power, mesh, launcher, theme)
```

Core stack: `esp-hal` ~1.1 · `esp-rtos` 0.3 · `esp-radio` 0.18 (wifi/ble/coex/esp-now) · Embassy (executor/net/time/sync) · `slint` 1.17 · `trouble-host` 0.6 · `embedded-graphics` 0.8 · `heapless` 0.9.

## Status & roadmap

- ✅ **Shipped (through v0.6.0):**
  - *v0.2.0* — Slint UI shell (5-page carousel, launcher, AOD, Mesh Familiar), on-demand framebuffer, SMOLv1 mesh, games, weather, pedometer, BLE, OTA, `defmt-rtt` debug.
  - *v0.4.0* — light-sleep AOD, WLED WiZmote remote, RSSI treasure-hunt game, home-energy screen, C6 die-temperature, workspace split into host-tested `no_std` crates.
  - *v0.6.0* — **voice push-to-talk** (WiFi-ready-gated capture streamed to a LAN STT gateway), **speaker playback** fixed (beep audible), **touch responsiveness** (non-blocking DMA panel flush un-starves touch), launcher scroll fix + an **AUDIO** launcher section (Voice / Sound tiles), Dependabot, and the esp-rs stack on the latest stable.
- 🚧 **In progress:**
  - **Mic capture** — a live dB meter / voice capture fed by the **ES7210** 4-channel mic ADC over I²S RX. The ES7210 driver is being implemented (the ADC needs its own I²C init before it drives `SDOUT1`); the ES8311 handles playback only.
  - **Multi-theme system** — 4 schemes (Midnight / Paper / Amber / Violet) with a runtime picker (`feat/theme-system`; a live preview is on the [showcase site](https://jphein.github.io/esp32c6-watch/theme-preview.html)).
  - **Plugin / app registry** — a single-registration source of truth for launcher apps ([PR #9](https://github.com/jphein/esp32c6-watch/pull/9)).
- 🔭 **Planned:** **Radio Scan** — a passive 802.15.4 monitor (Zigbee/Thread PAN IDs, channels, RSSI) as a dedicated mode that tears the mesh down first, since all three radios share one PHY.

## Credits

This firmware is a port of [**infinition/waveshare-watch-rs**](https://github.com/infinition/waveshare-watch-rs) — the original Rust watch firmware for the ESP32-**S3**-Touch-AMOLED-2.06, by **Fabien (infinition)** — adapted to the ESP32-**C6** board. C6 differences include no PSRAM (RGB332 framebuffer in SRAM), no SD card slot, no TE pin in the BSP, and a different GPIO map. Deep thanks to the upstream author for the foundation.

It is also the ESP32-C6 hardware target within the [**smol**](https://github.com/jphein/smol) fleet project; the SMOLv1 mesh protocol is shared between the two.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
