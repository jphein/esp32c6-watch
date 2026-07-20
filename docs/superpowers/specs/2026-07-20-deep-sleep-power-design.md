# Deep-Sleep Power / Self-Refresh AOD — Feasibility + Design

Date: 2026-07-20
Status: research finding for JP review (docs-only; no code)
Author: Nebula (dreamteam), task #29
Base: `feat/slint-shell` @ 78356f2
Context: the real battery win the LP-core investigation (#24) pointed to.
**Supersedes T11's AOD** (dim render with the HP core awake) with the
power-optimal version.

## TL;DR verdict — 🟢 GREEN (via **light sleep**, not deep sleep)

The power win is real and large, but the right primitive is **light sleep**
(resume-in-place), not deep sleep (reset-on-wake). Self-refresh AOD works: the
CO5300 holds the frame in its own GRAM, so the panel stays lit with both cores
asleep; an RTC timer wakes the HP core once/min to update the clock, and a touch
tap wakes it instantly. Deep sleep is a useful *second* tier for long "off-wrist"
standby only. Recommend building light-sleep AOD.

| Question | Answer |
|---|---|
| esp-hal deep/light sleep on C6? | 🟢 both present (`sleep_deep -> !`, `sleep_light`) |
| Self-refresh AOD (panel holds frame, cores asleep)? | 🟢 yes — CO5300 GRAM self-refresh |
| Touch-to-wake? | 🟢 **light sleep only** (GPIO15 can't wake deep sleep) |
| Interacts with the BLE-can't-stop issue (#30)? | 🟢 **no** — light sleep needs no teardown |
| Battery win vs T11 active-AOD? | large — display-bound; ~5–10× less (measure) |

## 1. esp-hal sleep API on the C6 — VERIFIED

`esp_hal::rtc_cntl::Rtc`:
- **`sleep_deep(&[&dyn WakeSource]) -> !`** (mod.rs:377) — **diverges**. Deep
  sleep is **reset-on-wake**: the SoC powers down, and on wake it **reboots from
  `main()`**. Only RTC-domain state survives (RTC timer, RTC slow/fast RAM via
  `#[ram(rtc_fast)]`). Everything else (drivers, radios, embassy tasks) re-inits.
- **`sleep_light(&[&dyn WakeSource])`** (mod.rs:385, `#[cfg(sleep_light_sleep)]`)
  — **returns**. Light sleep **suspends and resumes in place**: RAM + peripheral
  state retained, execution continues after the call. *(Confirm the
  `sleep_light_sleep` cfg is enabled for C6 in the esp-hal build metadata — the
  C6 supports light sleep at the IDF/HW level; the API is present.)*
- `sleep(&RtcSleepConfig, &[&dyn WakeSource])` — the generic form.
- `wakeup_cause() -> SleepSource` — why we woke (Timer / Gpio / Ext0/1 / …).

**Wake sources (verified):**
- `TimerWakeupSource::new(Duration)` — RTC timer. Works for both sleep modes.
- `Ext0WakeupSource` / `Ext1WakeupSource` — RTC-IO GPIO wake for **deep** sleep;
  on C6 restricted to the **RTC/LP-IO pins (GPIO0–7)** (esp32c6.rs steals/uninits
  GPIO0..5+ as the wakeup-pin set).
- **`Pin::wakeup_enable(true, WakeEvent::{LowLevel|HighLevel})`** (gpio/mod.rs:1306)
  — digital **GPIO wakeup**, usable on **any** pin in **light** sleep (routed via
  `rtc_cntl.gpio_wakeup`).

**Decisive constraint:** the **touch INT is on GPIO15** (board.rs), which is
**outside the RTC-IO range (0–7)**. Therefore:
- **Deep sleep cannot be woken by touch** — only by the RTC timer or an RTC-GPIO
  button (0–7). (Same class of pin-domain limit as the LP-core I2C finding.)
- **Light sleep *can* be woken by touch** via `GPIO15.wakeup_enable(LowLevel)`.

→ A responsive, tap-to-wake AOD **must** use light sleep. Deep sleep is only for
a deliberate, timer/button-woken deep-standby mode.

## 2. Self-refresh AOD — VERIFIED the panel holds the frame

The CO5300 is a standard MIPI-DCS AMOLED with its own GRAM (driver
`src/drivers/co5300.rs`): `RAMWR 0x2C`, `DISPON 0x29`, `SLPIN 0x10`/`SLPOUT
0x11`, `BRIGHTNESS 0x51`. After a frame is written to GRAM (our line-flusher) and
the display is ON, the **panel self-refreshes from GRAM with no MCU involvement**
— the ESP does not stream continuously (there's no TE pin wired, and none is
needed for a static frame). So the frame stays lit while **both cores sleep**.
Brightness is a single `0x51` write (dim for AOD).

**AOD loop (light sleep):**
1. On entering AOD: render the dim clock frame once → GRAM; `set_brightness(dim)`.
2. Arm wake sources: `TimerWakeupSource(60 s)` **+** `GPIO15.wakeup_enable(LowLevel)`
   (touch INT active-low).
3. `rtc.sleep_light(&[&timer, &touch])` — HP core suspends; panel keeps showing
   the frame from GRAM.
4. On wake, `wakeup_cause()`:
   - **Timer** → update only the changed clock digits (small RAMWR region), maybe
     re-sync mesh briefly if policy wants, then re-sleep.
   - **Gpio (touch)** → exit AOD → bright/interactive (raise brightness, resume
     the normal shell loop).

Only the minute digit changes, so the per-wake work is a tiny partial GRAM write
+ immediate re-sleep — microamp-scale duty cycle.

## 3. State survival + the BLE issue (#30)

- **Light sleep:** RAM + peripherals retained → **all state survives**: drivers
  stay initialized, RTC fine, and crucially **BLE/WiFi/ESP-NOW need no teardown**
  — the whole system pauses and resumes. **This side-steps the
  `ble_host_task`-can't-stop blocker (#30) entirely** — unlike Radio Scan, light
  sleep never has to release the radios. (Radios can be left in modem-sleep or
  the mesh simply pauses for the sub-minute nap; on wake it resumes/re-syncs.)
- **Deep sleep:** reboots → everything re-inits from `main()` cleanly (so #30 is
  moot — it's a fresh boot), but the mesh drops and re-joins each wake and boot
  latency applies. Fine for long standby, wrong for AOD.

**Verdict: the #30 BLE-teardown problem does NOT block this feature** — because
light sleep is the chosen primitive and it requires no radio teardown.

## 4. Wake latency / UX

- **Light sleep wake ≈ sub-millisecond** to resume the CPU (IDF light-sleep exit
  is ~hundreds of µs). Touch tap → wake → raise brightness → repaint: perceptually
  **instant**. Good watch UX.
- **Deep sleep wake = full boot** (driver + radio init, hundreds of ms to
  seconds) — too slow for tap-to-wake, and touch can't trigger it anyway.

## 5. Power estimate (order-of-magnitude — MEASURE on HW)

Using the firmware's own `PowerStats` (`BATTERY_CAPACITY_MAH`) as the measuring
stick; numbers below are estimates to be confirmed with the power page / a meter.
- **Current T11 AOD** (HP awake, dim render): HP core active even dimmed ≈
  **~20–40 mA** (CPU@160 MHz dominates) + dim AMOLED (few mA) + radios.
- **Light-sleep AOD:** HP in light sleep ≈ **~0.5–2 mA** (RAM/peripherals
  retained) + dim mostly-black AMOLED (AMOLED ~ content-proportional, a dark AOD
  face is low, ~2–5 mA) + radios idle/modem-sleep. **Display becomes the
  dominant draw**; total ≈ **~3–6 mA**.
- **Deep sleep** (screen off, storage): ≈ **~7–15 µA** (C6 deep sleep floor) —
  but that's screen-*off* standby, not AOD.

→ Light-sleep AOD is roughly a **5–10× reduction** vs T11 active-AOD, and it's now
**display-bound** (further wins come from AMOLED dimming / darker AOD faces, not
the CPU). Deep sleep only helps a screen-off "deep standby" tier.

## 6. Design — two-tier sleep, superseding T11

Map onto the existing `screen_state` machine (`main.rs`: 3 bright · 2 dim · 1 AOD
· 0 off) which currently keeps the HP core awake in every state:

- **State 1 (AOD): light sleep.** Replace the awake dim-render loop with the
  §2 flow — render once, `sleep_light(timer60s + touch15)`, wake to update the
  minute / on touch. HP idles in light sleep between ticks.
- **State 0 (off) → optional deep-standby:** after a long idle in state 0 (e.g.
  30+ min, or a "storage mode" gesture), enter **deep sleep** with an RTC-timer
  (and/or an RTC-GPIO button if one exists on 0–7) as the only wake — lowest µA.
  Wake = clean reboot. Gate this behind an explicit policy so the watch isn't
  hard to wake by accident.
- **States 2/3 (dim/bright):** unchanged (interactive; HP active).

**Embassy integration (the main implementation wrinkle):** the firmware runs on
`embassy_executor` + `embassy_time`. Manually calling `rtc.sleep_light` must be
coordinated with the embassy time queue so its timer doesn't immediately wake the
core, and so the RTC-timer wake aligns with the next embassy deadline. Approach:
on entering AOD, break out of the normal `select`, compute the next wake (60 s or
next scheduled deadline), arm the RTC timer to it, `sleep_light`, then on return
service the wake and re-enter. This coordination is the primary thing to get
right (and to verify on HW).

## 7. Risks / open questions

1. **Embassy ↔ light-sleep coordination** (above) — the main implementation risk;
   prototype the enter/wake handshake first.
2. **`sleep_light_sleep` cfg for C6** — confirm the esp-hal light-sleep path is
   enabled for our chip/version (very likely; verify at spike time).
3. **Radio behavior across light sleep** — decide policy: modem-sleep (keep
   association, more µA) vs pause mesh for the nap (less µA, brief peer gaps).
   For AOD, pausing is probably fine; confirm mesh re-sync time on wake.
4. **Touch wake level** — FT3168 INT is active-low; `wakeup_enable(LowLevel)`.
   Verify a tap produces a clean low edge that wakes reliably.
5. **AMOLED partial-update** — updating only the minute digits (small RAMWR
   window) minimizes wake work; confirm the CO5300 column/row-address windowing
   the driver already uses supports a sub-region write for the digits.
6. **Deep-standby wake button** — is there a physical button on GPIO0–7 to wake
   deep sleep? (BOOT is typically GPIO9, outside RTC-IO.) If not, deep standby is
   timer-wake-only. Open question for JP.

## 8. Recommendation

Build **light-sleep AOD** as the power-optimal replacement for T11 — it's a clear
GREEN, sidesteps the BLE blocker, and gives instant tap-to-wake. Treat deep sleep
as a later, optional "deep standby" tier for screen-off/off-wrist, timer-woken
only. Prototype the embassy/light-sleep handshake first (the one real risk).

## Open questions for JP

1. Should AOD radios stay in modem-sleep (connected, more µA) or fully pause
   during the nap (less µA, brief mesh gaps)?
2. Want the optional deep-standby tier now, or light-sleep AOD only for v1?
3. Any physical wake button on GPIO0–7, or is deep-standby timer-wake-only?

## Sources
- esp-hal 1.1.1 `src/rtc_cntl/mod.rs` (sleep_deep/sleep_light/wakeup_cause),
  `src/rtc_cntl/sleep/{mod,esp32c6}.rs` (wake sources, C6 RTC-IO pin set),
  `src/gpio/mod.rs` (`wakeup_enable`, WakeEvent).
- `src/drivers/co5300.rs` (RAMWR/DISPON/SLPIN/brightness), `src/board.rs`
  (touch INT = GPIO15), `src/main.rs` (screen_state machine, current AOD).
```
