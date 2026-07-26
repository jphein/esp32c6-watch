# OTA firmware deploy (WiFi, no USB)

Push a new firmware image to the watch over WiFi instead of USB-flashing.

## Push OTA (one command, zero-touch) — preferred

```bash
tools/ota_push.sh
```

Does everything: stamps a build id, builds on `familiar`, converts to an app
image, uploads to the OTA server, and publishes a **retained** MQTT announce.
The watch picks the announce up on its **next MQTT window** — the boot burst
(so: reboot the watch, or wait for its next boot) or any open Climate/Energy
session — and updates itself with no taps ("Updating firmware…" toast, then it
reboots into the new build).

- **Topic**: `watch/ota/announce` on the HA broker (`MQTT_BROKER`,
  `10.0.11.110:1883`).
- **Payload**: `OTA|<build_id>|<url>` — `<build_id>` is unix-seconds, `<url>`
  optional (empty/absent = the baked `OTA_URL`).
- **Retained** is what makes push work on the single bursty radio: a watch
  offline at publish time still gets the announce on its next window.
- **Monotonicity gate**: the running firmware bakes its own build id
  (`OTA_BUILD` in `.cargo/config.toml [env]`, stamped by the script). An
  announce triggers only if its `build_id` is **strictly greater**, so the
  still-retained announce after the reboot can never re-trigger-loop; dev
  builds without `OTA_BUILD` run as id 0 (any announce triggers them).
- The script **reads** `MQTT_BROKER`/`MQTT_USER`/`MQTT_PASS`/`OTA_URL` from the
  gitignored `.cargo/config.toml` — no credentials live in the committed script.
- `tools/ota_push.sh --announce-only` re-publishes the announce for the
  already-uploaded image (same stamped build id).
- Watch-side log lines: `[OTA] announce received/accepted/rejected …`, then
  `[OTA] push: build <id> queued (zero-touch)`.

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
   - or an error on failure — the running firmware is untouched, just retry.
     The download errors name the failure mode:
     - `stalled (10s, no data)` — the transfer went quiet mid-body (server or
       link died); also `stalled in headers` / `connect timeout (10s, server
       down?)` for the earlier phases,
     - `timeout (5 min overall)` — the transfer stayed alive but was too slow
       to finish inside the hard cap,
     - plus `image larger than ota slot`, `http status not 200`, ….

## Notes

- The image size must fit the OTA app slot (the download aborts with
  `image larger than ota slot` otherwise). The first byte is checked for the ESP
  app-image magic (`0xE9`) before anything is flashed.
- `OTA_URL` is compile-time. Change the server/path → edit `.cargo/config.toml`
  and rebuild.
- Serving the file: any static HTTP server rooted at `/home/jp/watch-ota/` works,
  e.g. `cd /home/jp/watch-ota && python3 -m http.server 8000`.

## ⚠️ USB-flash rule (learned 2026-07-23)

`espflash flash <elf>` **without `--partition-table partitions.csv` silently
rewrites the DEFAULT factory-only table**, destroying the A/B OTA layout
(next OTA attempt fails "no otadata partition"). EVERY USB flash of this
project must pass `--partition-table partitions.csv`. OTA updates
(`/watch/announce` or the Settings button) never touch the table — only USB
flashes can break it.

## Targeted (per-watch) push

Every watch derives a **sigil identity** from its efuse MAC (v0.8.4+) and
subscribes to its own topic alongside the fleet one:

```
tools/ota_push.sh                          # fleet: watch/ota/announce (all watches)
tools/ota_push.sh --target mythic-throne   # one watch: watch/mythic-throne/ota
```

Current fleet: `eldritch-lantern` (98:A3:16:A7:2F:E4) and `mythic-throne`
(98:A3:16:A5:A7:F8). A watch's sigil is on its System page and in the
`[SIGIL]` boot log line.

## Partition layout v2 (#50, 2026-07-25)

Slots grew 4MB → **6MB** (ota_0 @0x10000, ota_1 @0x610000, config @0xC10000).
Deployed via a full USB flash with the new `partitions.csv` on every watch —
**config records wiped by the move** (theme/toggles re-set once; creds are
baked). OTA images up to ~6.2MB now fit; the ota_push gate is updated.
