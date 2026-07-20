# Deep-Sleep Power (Light-Sleep AOD) — Implementation Plan (executable)

Date: 2026-07-20
Status: build-ready draft for JP review (docs-only; do NOT implement yet)
Spec: `docs/superpowers/specs/2026-07-20-deep-sleep-power-design.md` (verdict 🟢
      GREEN via light sleep)
Base: `feat/slint-shell` @ fec96d7 (integration HEAD)
Prereq: Slint migration merged (main.rs-heavy; builds on the current loop).

**Supersedes part of T11** — specifically the `screen_state == 1` (AOD) active
render. Everything else in T11 (dim/off states, brightness) stays.

**Adopted defaults (JP, reversible):**
1. AOD radio policy = **quiesce the mesh during AOD light-sleep, resume on wake**
   (max battery; mesh/familiar re-sync on wake — the one real tradeoff).
2. **v1 = light-sleep AOD only**; defer the screen-OFF deep-standby (deep-sleep)
   tier.
3. Wake button = **use what exists** — the loop already has a `boot_button`
   (GPIO9) in its `select3`, so light-sleep wake can use touch (GPIO15) **and**
   the boot button (GPIO9). (Both are digital-GPIO-wake-capable in light sleep;
   neither is RTC-IO, which is fine — v1 is light sleep only.)

Ordered tasks DS0–DS4. **DS0 (the handshake spike) gates the rest.**

---

## DS0 — Embassy ↔ light-sleep coordination spike (DE-RISK FIRST; throwaway) 🚦

**Why:** the one real risk flagged in the spec. The main loop currently awaits
`select3(Timer::after(tick), touch_int.wait_for_falling_edge(),
boot_button.wait_for_falling_edge())` (main.rs ~640) — under esp-rtos that just
WFI-idles the CPU; it does **not** enter chip light-sleep. DS0 proves we can
explicitly `rtc.sleep_light(...)` from inside the loop, wake, and resume cleanly.

- Minimal spike: in a test build, when idle, compute the next deadline, arm
  `TimerWakeupSource(dt)` + `GPIO15.wakeup_enable(LowLevel)` +
  `GPIO9.wakeup_enable(LowLevel)`, call `rtc.sleep_light(&[…])`, then on return
  log `wakeup_cause()` and continue.
- **Characterize (record in the PR):**
  1. Does `sleep_light` return and the loop resume in place (RAM/peripherals
     intact)? 
  2. **Does `embassy_time` survive light sleep or drift?** (The systimer/timg
     backing embassy_time may pause during light sleep; the external PCF85063
     RTC keeps wall-clock time regardless — the on-screen clock is read from it,
     so it's safe, but embassy scheduling/uptime may need a resync. Decide the
     handling here.)
  3. Wake latency touch→resume (expect sub-ms).
  4. Measured current in `sleep_light` vs the current WFI-idle AOD.
- **Confirm** the `sleep_light_sleep` cfg is enabled for C6 in our esp-hal build.
- **PASS** → DS1 builds the AOD on this handshake. **If embassy_time drift is
  unmanageable** → fall back to reading the PCF85063 RTC for all wall-clock time
  on wake (already the source) and treat embassy_time as best-effort.
- **Accept:** documented handshake that sleeps + wakes (timer AND touch) with the
  clock still correct after.

## DS1 — Light-sleep AOD state machine (supersedes T11 AOD render)

Replace the `screen_state == 1` branch (today: `tick = 10 s`, HP stays awake
rendering the dim frame) with the light-sleep flow:
- On **entering** AOD (transition 2→1): render the dim clock frame once via the
  shell (`shell.set_aod(true)` + one `shell.render`), `display.set_brightness(dim)`
  — the CO5300 then holds the frame in GRAM (self-refresh; verified in spec §2).
- **AOD idle:** instead of `select3(Timer::after(10s), …)`, run the DS0 handshake
  — arm wake sources (DS3), `rtc.sleep_light(...)`, resume.
- On **timer wake:** read the PCF85063 RTC, if the **minute changed** repaint only
  the clock (small RAMWR region — reuse the shell's existing address-window path),
  re-sleep. No repaint if only seconds ticked (AOD shows no seconds).
- On **touch/button wake:** exit AOD → `screen_state = 3` (bright), raise
  brightness, resume the normal interactive loop.
- Keep T11's dim(2)/off(0) states as-is.
- **Accept (HW):** AOD face stays lit with the HP core in light-sleep; minute
  updates once/min; tap/button wakes to bright instantly.

## DS2 — Quiesce / resume mesh around AOD sleep (adopted default #1)

- On **entering** AOD light-sleep: quiesce the mesh — stop the ESP-NOW tick /
  broadcast so the radio isn't kept awake (park it; do **not** tear down — light
  sleep pauses it). Note the `familiar.needs_fast_tick()` override (main.rs ~633)
  that currently caps idle sleeps to 400 ms/3 s while the mesh is up — in AOD we
  intentionally **override that cap** and let the 60 s sleep stand (mesh paused).
- On **wake to interactive:** resume the ESP-NOW tick; familiar/peers re-sync
  (expect a few seconds of re-discovery). Because this is **light** sleep, no
  radio re-init / `WIFI::steal()` is needed — the stack resumes (this is why the
  BLE #30 blocker does not apply here).
- **Accept (HW):** during AOD the mesh is quiet (low current); on wake peers
  reappear within the mesh's normal HELLO/ACK window.

## DS3 — Wake-source wiring

- `TimerWakeupSource::new(Duration::from_secs(60))` — minute cadence.
- Touch: `GPIO15.wakeup_enable(true, WakeEvent::LowLevel)` (FT3168 INT active-low).
- Button: `GPIO9.wakeup_enable(true, WakeEvent::LowLevel)` (boot button, already
  polled in the loop).
- On wake, `wakeup_cause()` selects the DS1 branch (timer → update+resleep;
  gpio → exit to bright). Disable the GPIO wake-enables on exit so they don't
  interfere with the normal `wait_for_falling_edge` interrupts.
- **Accept:** all three wake sources fire correctly and are distinguishable via
  `wakeup_cause()`.

## DS4 — HW verify + µA measurement (vs T11 baseline)

- Measure current in three states with a meter (and cross-check the power page):
  **T11 active-AOD (baseline)** vs **DS light-sleep AOD** vs bright.
- Confirm the spec's estimate (T11 ~20–40 mA → light-sleep AOD ~3–6 mA,
  display-bound); record actuals + projected battery life vs
  `PowerStats::BATTERY_CAPACITY_MAH`.
- Verify: minute updates correct over 10+ min; tap/button wake reliable; mesh
  re-syncs on wake; no visual glitch on the self-refreshed frame; clock stays
  correct across many sleep cycles (DS0 embassy_time decision holds).
- Update `PowerStats` so the power page reflects the light-sleep AOD draw.
- **Accept:** measured win recorded; AOD stable on-glass over a long soak.

---

## Sequencing
- **DS0 first and blocking** — its embassy_time verdict shapes DS1.
- DS1 depends on DS0 + DS3 (needs the wake sources to sleep against).
- DS2 layers onto DS1 (mesh policy around the sleep).
- DS3 can be built alongside DS0 (the wake-source API).
- DS4 is the gate.

## Supersede / interaction notes
- **Supersedes T11's `screen_state == 1` AOD render** only; dim/off/bright
  unchanged.
- **Does NOT touch the radios' init/teardown** — light sleep pauses & resumes, so
  the `ble_host_task`-can't-stop issue (#30) is irrelevant here (per the spec).
- Independent of Radio Scan / mic features (different subsystem); no file
  collisions expected beyond the shared `main.rs` loop + `PowerStats`.

## Deferred (v2 / out of this plan)
- **Screen-OFF deep-standby tier** (true `sleep_deep`, timer/RTC-GPIO wake only,
  ~7–15 µA) — JP deferred; would be a state-0 escalation after long idle.
- Dynamic AOD refresh (e.g. update on notification) — v1 is minute-tick only.
```
