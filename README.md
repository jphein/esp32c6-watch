# esp32c6-watch

100% Rust `no_std` firmware for the [Waveshare ESP32-C6-Touch-AMOLED-2.06](https://www.waveshare.com/wiki/ESP32-C6-Touch-AMOLED-2.06) smartwatch.

Built on `esp-hal` + Embassy, rendering to the onboard CO5300 AMOLED over QSPI. No RTOS beyond the async executor, no heap-hungry framework — just Rust on a RISC-V microcontroller driving a wrist-sized watch.

## Features

- **Watchface** — time, date, and live sensor readout drawn with `embedded-graphics` and u8g2 fonts.
- **Games** — Snake, Tetris, 2048, Flappy, a maze, and a mesh-aware "world snake".
- **Mesh networking** — SMOLv1 fleet protocol over ESP-NOW (HELLO/ACK/TIME/CFG/RELAY frames). Loop-free time authority: the watch does its own NTP and both adopts time from and serves time to the fleet. Shared wire format with the [`smol`](https://github.com/jphein/smol) project.
- **Connectivity** — WiFi STA with NTP, BLE GATT server (`trouble-host`), MQTT → Home Assistant, weather fetch, and HTTP OTA updates.
- **Pedometer** — hardware step counting via the QMI8658 IMU (keeps counting while the IMU is otherwise idle).
- **Power management** — CPU clock control, power stats, and battery monitoring.
- **Settings** — on-device T9 keyboard for entering WiFi credentials at runtime.
- **Slint UI migration** — a parallel `slint-demo` binary is porting the shell to the [Slint](https://slint.dev) toolkit (no_std software renderer). Work in progress on branch [`feat/slint-shell`](https://github.com/jphein/esp32c6-watch/tree/feat/slint-shell).

## Hardware

- **Board**: Waveshare ESP32-C6-Touch-AMOLED-2.06
- **MCU**: ESP32-C6 (RISC-V, `riscv32imac-unknown-none-elf`)
- **Display**: CO5300 AMOLED over QSPI (RGB332 framebuffer in SRAM — the C6 has no PSRAM)
- **IMU**: QMI8658 (accelerometer + hardware pedometer)
- **Touch**: capacitive touch controller
- **Audio**: I2S

## Build & Flash

Requires the stable Rust toolchain with the RISC-V bare-metal target (pinned in `rust-toolchain.toml`) and [`espflash`](https://github.com/esp-rs/espflash).

```sh
# Install the flash tool
cargo install espflash

# Configure WiFi credentials (creds live only in your local .cargo/config.toml,
# which is gitignored — never committed)
cp .cargo/config.example.toml .cargo/config.toml
# then edit .cargo/config.toml and add under [env]:
#   WIFI_SSID="YourNetwork"
#   WIFI_PASS="YourPassword"

# Build the release firmware
cargo build --release

# Flash + monitor (the configured runner does this on `cargo run`)
espflash flash --monitor --chip esp32c6 target/riscv32imac-unknown-none-elf/release/esp32c6-watch
```

The `.cargo/config.example.toml` sets `espflash flash --monitor --chip esp32c6` as the cargo runner, so `cargo run --release` builds and flashes in one step. WiFi and BLE radios are **off at boot** and toggled from the watchface.

## Credits

This firmware is a port of [**infinition/waveshare-watch-rs**](https://github.com/infinition/waveshare-watch-rs) — the original Rust watch firmware for the ESP32-**S3**-Touch-AMOLED-2.06 — adapted to the ESP32-**C6** board. C6 differences include no PSRAM (RGB332 framebuffer lives in SRAM), no SD card slot, no TE pin in the BSP, and a different GPIO map. Deep thanks to the upstream author for the foundation.

It is also the ESP32-C6 hardware target within JP's [**smol**](https://github.com/jphein/smol) fleet project; the SMOLv1 mesh protocol is shared between the two, and improvements flow both ways.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
