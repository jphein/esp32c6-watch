# LP-Core Offload — Feasibility + Design Sketch

Date: 2026-07-20
Status: research finding for JP review (docs-only; no code)
Author: Nebula (dreamteam), task #24
Base: e53294f (v0.2.0)

## TL;DR verdict

| Question | Verdict |
|---|---|
| Is the C6 LP core usable from Rust at our esp-hal ~1.1? | 🟢 **GREEN** — well supported |
| Do JP's *listed* offload targets (AOD tick, step drain, mesh wake, RTC) win battery **on this watch**? | 🔴→🟡 **RED-leaning** — the premise doesn't fit this board's power profile |
| Is there *any* niche where LP firmware helps here? | 🟡 narrow — and mostly duplicated by esp-hal's built-in deep-sleep wake sources |

**Bottom line:** the toolchain is real and good, but I have to push back on the
premise. Each of the four listed targets is either **impossible** on the LP core
(display, radio), **already handled by hardware** (IMU pedometer, RTC/self-
refresh), or **already covered by esp-hal without custom LP firmware** (timer/
GPIO wake). The realistic battery win from the listed targets is **≈ 0**. The
higher-leverage power work is HP deep-sleep policy + AMOLED self-refresh AOD —
neither needs the LP core. Recommend **not** building LP-core firmware for these
targets; revisit only if a genuinely autonomous LP-wired sensor task appears.

## 1. Toolchain — VERIFIED (🟢 GREEN)

- **HP-side driver ships in esp-hal 1.1.1**: `esp_hal::soc::esp32c6::lp_core`
  (verified source). `LpCore::new(peripherals.LP_CORE)` /
  `new_with_clock(LpCoreClockSource::{RcFastClk 17.5MHz | XtalD2Clk 20MHz})`,
  `.run(LpCoreWakeupSource::HpCpu)`, `.stop()`.
- **LP firmware HAL**: `esp-lp-hal` (crate, v0.1.0) — bare-metal no_std HAL for
  the LP RISC-V core. LP firmware is a **separate binary** (RISC-V `imc`, no
  atomics) built against `esp-lp-hal`, embedded into the HP image and copied
  into LP_RAM before `run()`. (Exact embed macro/tooling to be confirmed against
  esp-lp-hal 0.1 docs — not present in esp-hal core; lives in the LP tooling.)
- **Shared memory / mailbox**: **LP_RAM = 16 KB @ `0x5000_0000`** (verified —
  `LpCore::new` zero-fills `16 * 1024` bytes there). This region is the HP↔LP
  channel: both cores read/write it; a mailbox convention (flags + payload) is
  layered on top. esp-hal/esp-lp-hal examples provide sync + async mailbox
  patterns.
- **Wake coordination**:
  - HP→LP: `LpCoreWakeupSource::HpCpu` (HP triggers the LP core to run; only
    source exposed on C6 in esp-hal 1.1). The LP core can also run autonomously
    off the LP timer.
  - LP→HP: `esp_hal::rtc_cntl::sleep::WakeFromLpCoreWakeupSource` (verified) —
    the LP core can wake the HP core out of deep sleep.
- LP-domain peripherals available to LP firmware: **LP-IO** (GPIO0–7 only),
  **LP-I2C**, LP-UART, LP timer (`esp_hal::gpio::lp_io`, `i2c::lp_i2c`).

Maturity: LP support is real but young (`esp-lp-hal` 0.1, extensive but example-
driven). Workable, expect rough edges + a two-binary build.

## 2. Why the listed targets don't pay off on THIS watch

### AOD clock tick — ❌ not an LP-core job
- LP_RAM is **16 KB total**; the panel framebuffer is **~202 KB** (410×502
  RGB332). The LP core **cannot render** to the display.
- The CO5300 AMOLED has its **own GRAM and self-refreshes**: once the HP core
  writes the AOD frame, the panel holds it lit with **no core running**. The
  only recurring work is the ~once-per-minute digit update — a **deep-sleep
  RTC-timer wake** of the HP core, not an LP task. The LP core adds nothing.

### Hardware step-counter drain — ❌ hardware already does it + pinout blocks it
- The QMI8658 has an **autonomous hardware pedometer** — it counts while the HP
  core sleeps regardless (already relied on in `main.rs`). Nothing to offload.
- Even if you wanted the LP core to read it during sleep: the board wires I2C
  as **SDA = GPIO8, SCL = GPIO7**, and **LP-IO only reaches GPIO0–7**. SDA on
  GPIO8 is **outside the LP domain** → LP-I2C to the IMU is a **hardware
  blocker** on this board without a rewire. (Verified: `main.rs` pins +
  esp-hal `lp_io` range.)

### Mesh / ESP-NOW wake — ❌ radio is HP-only
- The LP core has **no radio access** (WiFi/BLE/ESP-NOW/802.15.4 all live in the
  HP domain). The LP core cannot send/receive mesh frames. At most it could wake
  the HP core on a schedule so HP does a mesh burst — but that's a timer wake,
  which the **RTC/LP timer already provides** without custom LP firmware.

### RTC — ❌ already native to the LP power domain
- The C6 RTC lives in the LP/always-on domain; timekeeping survives deep sleep
  with **no** custom LP firmware. Nothing to offload.

## 3. Battery-win estimate (honest)

- Dominant draws on this watch (per the firmware's `PowerStats`): **display**,
  then **radio** when active. The HP core idling is not the bottleneck.
- Deep sleep (HP) is already ~tens of µA; RTC-timer and LP-IO/GPIO wake sources
  (built into esp-hal, no LP firmware) bring HP up for periodic work at that
  floor. An LP core *running* costs a few hundred µA — **above** the deep-sleep
  floor — so for the listed poll-and-wake tasks it would **cost more, not less**.
- **Net expected win from the listed targets: ≈ 0** (plausibly negative). The
  LP core only helps when you must run **autonomous logic between HP wakes** that
  the built-in wake sources can't express — none of the four targets qualify.

## 4. Where the real power wins are (redirect)

None of these need the LP core:
1. **HP deep-sleep policy** — sleep the HP core between interactions; wake on
   RTC timer + LP-IO/GPIO (touch/button) via esp-hal's built-in wake sources.
2. **AMOLED self-refresh AOD** — write the AOD frame to CO5300 GRAM, deep-sleep,
   RTC-wake once/min to update the clock. Panel stays lit with both cores off.
3. **Display dim/off + brightness policy** (the biggest single lever).
4. **Radio duty-cycling** — already partly done (WiFi bursts, then drops).

## 5. Design sketch — IF pursued anyway (narrow niche)

The only defensible LP-core use here: **autonomous wake-coordination /
edge-sensing during long HP deep-sleep**, where you want logic (debounce,
threshold, counting) between wakes without spinning up the HP core each time.

- **Build:** a tiny LP firmware crate (`lp-fw/`, target `riscv32imc-unknown-
  none-elf`, against `esp-lp-hal`), embedded into the HP binary, copied to
  LP_RAM by the HP core, started via `LpCore::run`.
- **Mailbox:** a fixed struct at a known LP_RAM offset (`0x5000_0000`): e.g.
  `{ magic:u32, wake_count:u32, event_flags:u32, payload:[u8; N] }`. HP writes
  config, LP writes results; a sequence/flag word guards races (both sides poll;
  no atomics on `imc`, so use a single-writer-per-field discipline + a version
  counter).
- **Runtime:** HP writes config → `LpCore::run` → HP `enter_deep_sleep` with
  `WakeFromLpCoreWakeupSource`. LP wakes on LP timer, does its small task
  (e.g. sample an LP-IO pin, debounce a wake button, increment a counter), and
  asserts the HP wake only when a real event occurs. HP wakes, reads the mailbox.
- **Suitable tasks on this board:** debouncing/latching a physical wake button on
  an LP-IO pin (GPIO0–7); coarse tap-to-wake gating. **Not** the four listed
  targets.
- **Hard limits to respect:** ≤16 KB for LP code+data+mailbox; no radio; no
  display; LP-I2C only to GPIO0–7 devices (the IMU is not reachable); `imc`
  (no atomics, soft-float).

## 6. Prior art

- **esp-hal `lp_core` examples** (`examples/peripheral/lp_core/lp_blinky`, plus
  mailbox / I2C examples) — the canonical Rust C6 LP-core references.
- **`esp-lp-hal`** (docs.espressif.com/projects/rust/esp-lp-hal/0.1.0/esp32c6) —
  the LP-side HAL.
- **ESP-IDF ULP LP-core guide** (docs.espressif.com/.../esp32c6/.../ulp-lp-core)
  — the C reference for capabilities, mailbox, and wake model; the Rust HALs
  mirror it.

## 7. Recommendation

Do **not** implement LP-core firmware for the AOD/step/mesh/RTC targets — the
verified constraints make the win ≈ 0. Invest the power budget in HP deep-sleep
+ AMOLED self-refresh AOD + display/radio duty-cycling instead. Keep the LP core
in the back pocket for a future autonomous LP-IO sensing task if one appears
(and only after confirming the pin is in GPIO0–7).

## Open questions for JP

1. Is the real goal **standby battery life** (→ deep-sleep policy, no LP core) or
   a specific autonomous-sensing behavior (→ maybe LP core)? The former is where
   the wins are.
2. Any appetite for a **hardware rev** that moves I2C SDA into GPIO0–7? That's
   the only path to LP-side IMU access — probably not worth it.
