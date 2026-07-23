# OTA firmware deploy (WiFi, no USB)

Push a new firmware image to the watch over WiFi instead of USB-flashing.

## How it works

- The image URL is **baked in at build time** via `OTA_URL` in `.cargo/config.toml`
  (gitignored). Current value:

  ```
  OTA_URL="http://10.0.11.11:8000/watch.bin"
  ```

  Plain HTTP only — no TLS, no DNS. The host must be a dotted-quad IPv4. The
  server is **ubox0 on VLAN-11** (`10.0.11.11:8000`), the same subnet as the
  watch's `roam` WiFi, serving `/home/jp/watch-ota/`.
- On the watch: **Settings → UPDATE FIRMWARE**. It gates on WiFi being ready
  (associated + DHCP), downloads the image into the *inactive* A/B slot, stages
  it, and reboots to apply. The running slot is never touched, so a failed or
  interrupted download cannot brick the watch.
- **Rollback-safety**: a freshly-OTA'd image boots "on trial". If it stays alive
  ~10 s (peripherals up + main loop running), the firmware marks the slot valid
  (`ota_http::mark_valid_if_pending`, `OtaImageState::PendingVerify → Valid`).
  If the new image crashes before that, the bootloader reverts to the previous
  slot on the next boot. (Auto-revert requires the esp-idf bootloader to be built
  with app-rollback enabled; the app-side confirm is always correct either way.)

## Deploy steps (JP runs these)

1. **Build the ELF** on the fambuild host:

   ```bash
   fambuild build --release --bin esp32c6-watch
   ```

   ELF lands at `target/riscv32imac-unknown-none-elf/release/esp32c6-watch`.

2. **Convert the ELF to an app image** (`.bin` the bootloader can flash). This is
   the *app* image, NOT a merged/full-flash image — OTA writes only the app slot:

   ```bash
   espflash save-image --chip esp32c6 \
     target/riscv32imac-unknown-none-elf/release/esp32c6-watch \
     watch.bin
   ```

3. **Publish to the OTA server** (ubox0, VLAN-11):

   ```bash
   scp watch.bin ubox0:/home/jp/watch-ota/watch.bin
   ```

   (The HTTP server on ubox0 serves `/home/jp/watch-ota/` on port 8000 as
   `http://10.0.11.11:8000/watch.bin`.)

4. **On the watch**: open **Settings**, make sure WiFi shows **CONNECTED**
   (tap CONNECT if not), then tap **UPDATE FIRMWARE**.
   - Status line shows `Updating…` during the download,
   - then `Staged – rebooting` on success (the watch reboots itself),
   - or an error (`Connect WiFi first`, `image larger than ota slot`,
     `http status not 200`, `timeout (30s)`, …) on failure — the running
     firmware is untouched, just retry.

## Notes

- The image size must fit the OTA app slot (the download aborts with
  `image larger than ota slot` otherwise). The first byte is checked for the ESP
  app-image magic (`0xE9`) before anything is flashed.
- `OTA_URL` is compile-time. Change the server/path → edit `.cargo/config.toml`
  and rebuild.
- Serving the file: any static HTTP server rooted at `/home/jp/watch-ota/` works,
  e.g. `cd /home/jp/watch-ota && python3 -m http.server 8000`.
