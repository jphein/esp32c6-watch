# Debugging the watch — the agent field guide

Everything an agent needs to debug the esp32c6-watch fleet over USB (and,
soon, WiFi) without re-deriving brittle bash. The front door is
**`tools/watchctl`** (host-side, python3 stdlib + optional pyserial — runs
anywhere, no venv).

```
tools/watchctl list                                  # who is on USB, where
tools/watchctl logs eldritch-lantern --seconds 20    # capture serial output
tools/watchctl logs mythic-throne --reset            # boot burst / panic capture
tools/watchctl reset eldritch-lantern                # verified reset (+ ladder)
tools/watchctl recover eldritch-lantern              # the full wedge ladder
tools/watchctl slot mythic-throne                    # booting slot + fingerprint
tools/watchctl deploy eldritch-lantern fw.elf        # slot-trap-proof USB deploy
tools/watchctl flash-full mythic-throne fw.elf --erase-otadata  # provisioning
tools/watchctl console eldritch-lantern              # debug-console REPL
tools/watchctl test eldritch-lantern hotpaths        # ui_test.py suites
tools/watchctl soak eldritch-lantern -n 20           # boot-stability: crash-rate + TTC
tools/watchctl ota-status                            # OTA server journal + announce
tools/watchctl endpoint eldritch-lantern             # WiFi debug endpoint probe
```

Global flags: `--json` (machine output on stdout, chatter on stderr),
`--transport usb|wifi|auto`, `--config <path>`, `-v`.
Exit codes: **0** ok · **2** not-found/usage · **3** recovery-needed ·
**4** gate-failed (image too big etc.).

---

## The fleet

| sigil | efuse MAC == USB serial | notes |
|---|---|---|
| `eldritch-lantern` | `98:A3:16:A7:2F:E4` | |
| `mythic-throne` | `98:A3:16:A5:A7:F8` | |

Hard rules (all encoded in watchctl — listed here so nobody "simplifies" them
away):

- **ttyACM numbers SHUFFLE on every replug.** Never address a watch by
  `/dev/ttyACMn` in a script; resolve by USB serial (udev `ID_SERIAL_SHORT`,
  which equals the efuse MAC). `watchctl` takes sigils.
- **Other Espressif devices share the bench** (C3 fleet nodes, e.g.
  `E8:06:90:65:9F:E4`). They enumerate identically (303a:1001). `watchctl
  list` shows them as *(not fleet)* — do **not** flash watch firmware at them.
- **Some USB ports cannot enumerate a C6 at all** (EPROTO −71): the `1-7.x`
  hub ports, and some mobo ports. Use direct ports; `watchctl list` warns on
  hub paths.
- **Light-sleep gates serial on release builds**: an idle release watch reads
  as 0 bytes — that is *normal*, not a wedge. The boot burst after a reset
  always prints; `debug-console` builds never sleep.
- A chip whose banner says **`waiting for download`** is parked in the ROM
  download mode — a plain reset un-parks it (`watchctl reset` handles this).

## The three debug channels

| channel | what you get | build requirement | tool |
|---|---|---|---|
| **Serial logs** (USB-Serial-JTAG CDC) | `println!`/`log` output, esp-backtrace panics + symbolized backtraces | any build (release: boot burst + event-driven lines only, sleeps in between) | `watchctl logs` |
| **Debug console** (UI automator) | drive the UI (`tap`/`swipe`/`launch`/`home`), read `state`/`perf`, run assertion + perf suites | `--features debug-console` (opt-in, **not** default; disables AOD light-sleep so serial is always live) | `watchctl console` / `watchctl test`, `tools/ui_test.py` |
| **probe-rs JTAG** | halt/reset/memory inspection, GDB server — over the same built-in USB-JTAG, no external probe | any build (debug symbols help); **no log capture** until defmt-rtt is wired in (open follow-up) | `probe-rs` (`~/.cargo/bin`, `--chip esp32c6`) |

`probe-rs list` shows every Espressif JTAG probe on the bench — match the
serial against the fleet table before attaching.

## Workflows

### Capture what the watch is saying right now
```bash
tools/watchctl logs eldritch-lantern --seconds 30
```
Zero lines on a release build at idle is normal (light-sleep). Grep-friendly:
NULs and ANSI escapes are stripped.

### Capture a panic / the boot burst
```bash
tools/watchctl logs eldritch-lantern --reset --seconds 15 > /tmp/boot.log
```
The reader opens **before** the reset (the burst starts ~100 ms after reset —
open-after-reset loses it). Capture RAW to a file, grep afterwards; grepping
the live stream drops burst lines. esp-backtrace prints the panic + backtrace
in this burst on the boot after a crash loop.

### Measure render performance
```bash
tools/watchctl test eldritch-lantern hotpaths   # frame-cost report, no gates
tools/watchctl test eldritch-lantern            # PASS/FAIL assertion suite
tools/watchctl test eldritch-lantern lights     # end-to-end Lights latency
```
Needs a `debug-console` build (see below). Run `hotpaths` before/after a perf
change and diff the numbers. The launcher assertion follows the **paged** launcher
(v0.8.0+): it flips one section-page per swipe and gates at a 250 ms bar — a flip is
a single full-frame repaint, so the render floor is the limit, not the old 100 ms
continuous-scroll threshold.

### Soak-test boot stability
```bash
tools/watchctl soak eldritch-lantern              # 6 boots x 12s (defaults)
tools/watchctl soak eldritch-lantern -n 20 -s 15  # 20 boots, 15s watched each
tools/watch_soak.py /dev/ttyACM3 20 12            # the probe directly: port, trials, secs
```
Resets the watch N times and classifies every boot (WiFi panic / brick / download mode /
alive), reporting a crash rate and time-to-crash. Born fighting #61, where the crash rate
*was* the acceptance gate — a stability fix has to move it to 0 %. Needs `pyserial`.
`watchctl soak` resolves the watch by sigil and wraps the probe (defaults `-n 6`, `-s 12`);
call `watch_soak.py` directly only when you need to name a raw port.

### Drive the UI by hand
```bash
tools/watchctl console eldritch-lantern
dbgcon> launch 13
dbgcon> state
```

### Build + deploy a test build (the slot-trap-proof path)
```bash
# Build on familiar (wake it first if ssh fails: realm wol wake familiar)
fambuild build --release --bin esp32c6-watch --features debug-console
scp familiar:fambuild/<worktree-name>/target/riscv32imac-unknown-none-elf/release/esp32c6-watch /tmp/fw.elf
tools/watchctl deploy eldritch-lantern /tmp/fw.elf
```
`deploy` converts the ELF (`espflash save-image`), gates the size
(≤ 6,225,920 B — the 6 MB slot less 64 KB, since #50 — exit 4 if over), detects
the **booting** slot from the boot
banner, `espflash write-bin`s into *that* slot, resets, and verifies the new
image actually came up (slot + `[SIGIL]` + `[STACK] gap` fingerprint).

### Recover a wedged USB port
```bash
tools/watchctl recover eldritch-lantern
```
Runs the proven #21 ladder — see below. Exit 3 means "power-cycle the watch".

### Provision a fresh / mangled device
```bash
tools/watchctl flash-full mythic-throne /tmp/fw.elf --erase-otadata
```
Full `espflash flash` **with `partitions.csv`** (a full flash without the
table is exactly the #20 trap) and optionally an otadata erase (boot pointer
back to `ota_0`).

### Check the OTA plumbing
```bash
tools/watchctl ota-status
```
Tails the `watch-ota.service` journal on ubox0 (image downloads show as
`GET /watch.bin`) and shows the retained `watch/ota/announce` with the build
id decoded. Broker creds come from the gitignored `.cargo/config.toml [env]`
(same source as `tools/ota_push.sh`) — never hardcoded. All broker traffic
rides ssh through ubox0: publishing/subscribing from katana to the VLAN-11
broker stalls mid-handshake (asymmetric-routing quirk).

## The slot trap (#20)

The flash has A/B app slots — `ota_0 @ 0x10000`, `ota_1 @ 0x410000`
(`partitions.csv`); **otadata** decides which one boots. After any successful
OTA the watch usually boots `ota_1`, but a bare `espflash flash <elf>` always
writes `ota_0` — so the USB-flashed build *silently never runs* ("the old
build won't die"). Worse, racing an otadata erase against a running app's
mark-valid can leave "slot 0 is not bootable".

The reliable deploy is therefore: read the booting slot from the boot-log
line `Loaded app from partition at offset 0x…`, then
`espflash write-bin <that-offset> <app.bin>` — which is exactly what
`watchctl deploy` does (plus the size gate and boot verification).
`watchctl slot` reports the current booting slot + build fingerprint when you
only want to look.

## The wedge recovery ladder (#21)

The C6's USB-Serial-JTAG recurrently wedges (enumerates but espflash can't
sync; worst case EPROTO −71 / drops off the bus). Battery power means a USB
replug does **not** reset the SoC. The ladder, in order:

1. **`espflash reset`** with the boot banner verified (also un-parks a chip
   stuck in download mode).
2. **`USBDEVFS_RESET` ioctl (21780)** on `/dev/bus/usb/<bus>/<dev>` (from
   `/sys/bus/usb/devices/<port>/{busnum,devnum}`) — a host-side
   re-enumeration; needs sudo (passwordless here). The ttyACM number may
   move afterwards — watchctl re-resolves by serial.
3. **Power-cycle the watch** (battery + USB both off) — the only thing that
   resets the wedged peripheral itself.

**NEVER toggle sysfs `authorized`** — it converts a soft wedge into hard
EPROTO. (Encoded nowhere in watchctl on purpose; it is the one move that made
things worse.)

## Builds

- Build host is **familiar**: `fambuild build --release --bin esp32c6-watch
  [--features debug-console]` from a worktree. ELF lands at
  `familiar:~/fambuild/<worktree-name>/target/riscv32imac-unknown-none-elf/release/esp32c6-watch`.
  If ssh fails, wake it: `realm wol wake familiar` (standing authorization —
  don't stall).
- Image gate: the saved image must be **≤ 6,225,920 B** — the 6 MB OTA slot less
  64 KB, since #50 grew the slots (it was 4,128,768 B before). watchctl gates this
  on every deploy/flash-full (exit 4). Note `save-image` needs the flash size *and*
  the table to match the 6 MB layout:
  `espflash save-image --chip esp32c6 --flash-size 16mb --partition-table partitions.csv <elf> <bin>`.
- `debug-console` builds: the console + UI automator, serial always live
  (no light-sleep). Never ship one over OTA to the fleet.
- Over-the-air instead of USB: `tools/ota_push.sh` (see
  `docs/ota-deploy.md`).

## WiFi debug channel — planned / next (firmware follow-on task)

The client side ships in watchctl/ui_test.py **now**; the firmware server is
the next task. Contract the firmware must implement:

- **Feature-gated TCP debug server** extending `debug-console`: same line
  protocol as the serial console (`ping`/`state`/`tap`/`swipe`/`launch`/
  `perf`/`beep`), plus `logs` which mirrors `println!` lines into a ring the
  TCP session drains. Port **5555**.
- **Auth**: LAN-only + shared secret. The client's first line is
  `auth <token>`; the server drops the connection on mismatch or timeout.
  Token lives in `.cargo/config.toml [env]` as `WATCH_DEBUG_TOKEN` (same
  gitignored-config pattern as the MQTT creds / HA token; baked into the
  firmware at build time via `option_env!`).
- **Endpoint discovery via MQTT, not mDNS**: on server start the watch
  publishes RETAINED `watch/<sigil>/debug/endpoint` = `<ip>:5555`, cleared on
  clean shutdown. watchctl resolves it through the broker (ssh ubox0 +
  mosquitto_sub, creds from the same config).

Client-side behavior (already implemented):

- `--transport usb|wifi|auto` (auto = USB if the serial is enumerated, else
  endpoint lookup). Transport matrix:

  | subcommand | USB | WiFi |
  |---|---|---|
  | `logs` | ✓ | ✓ (`--reset` stays USB-only) |
  | `console` / `test` | ✓ | ✓ |
  | `endpoint` | – | ✓ (lookup + connect/auth/ping probe) |
  | `reset` / `recover` / `slot` / `deploy` / `flash-full` | ✓ | ✗ — espflash + the boot banner need the wire |

- `tools/ui_test.py --port tcp://<ip>:5555 --token <tok>` speaks the same
  protocol over TCP (the `_Port` class carries both transports); `watchctl
  console|test` picks the transport and passes the token automatically.

**Security rule**: the debug server is a control plane. Feature-gated builds
only, **never in a release image**, token-gated on top, LAN-only. An OTA'd
fleet image must not contain it.

## Rule for agents without hardware access

USB state is mutable and single-owner. If you are an agent **without** a
hardware pass (no permission to reset/flash), do not run mutating watchctl
commands — write a **run-script** (a short shell script of watchctl calls +
expected outcomes, e.g. in your scratch dir) and hand it to the orchestrator
to execute on the bench. Read-only commands (`list`, `logs` without
`--reset`, `ota-status`, `endpoint`) are safe to run directly.
`tools/watchctl_selftest.sh` is the reference run-script: it exercises every
mutating path and is meant to be run by the device owner.
