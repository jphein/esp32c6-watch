# Mic Capture (dBFS Meter) — Implementation Plan (executable)

Date: 2026-07-20
Status: build-ready draft for JP review (docs-only; do NOT implement yet)
Spec: `docs/superpowers/specs/2026-07-20-mic-capture-design.md` (verdict: 🟢 GREEN)
Base: `feat/slint-shell` @ 78356f2 (integration HEAD)
Prereq: Slint migration merged (shares the launcher/app machinery + shell API).

Ordered bite-sized tasks MC1–MC6. Each is a compiling, revertible commit on a
`feat/mic-capture` branch cut from `main` after the migration lands. MC1–MC3 are
HW-independent and unit-testable; MC4–MC6 assemble the UI + wiring; MC6 is the HW
gate + gain tuning.

---

## MC1 — `Es8311::enable_adc()` / `disable_adc()` (codec ADC path)

Add a capture-enable path to `src/peripherals/audio.rs` mirroring the existing
`unmute()`/`shutdown()` structure. Needed because `main.rs:369` calls
`audio_codec.shutdown()` at boot, which powers the ADC analog **down**
(`0x0E=0xFF`, `0x0D=0xFC`).

**Register writes (semantics VERIFIED vs ES8311 datasheet; exact tuning values
flagged):**
- Re-power analog *(VERIFIED — mirrors working `init()`/`unmute()`):*
  - `0x0D = 0x01` — power up analog bias.
  - `0x0E = 0x02` — enable analog PGA + ADC modulator (bit6 `PDN_PGA` = 0 → PGA
    on). [datasheet: PDN_PGA is `0x0E.bit6`.]
- Mic input + gain:
  - `0x14`: bit6 = 0 (**analog** mic, not DMIC); bits[3:0] = **PGAGAIN** (0–30 dB
    range per datasheet). Start `0x14 = 0x10 | <gain_nibble>`; **exact gain code →
    tune on HW (MC6).** [semantics VERIFIED; value TUNE.]
  - `0x16`: ADC gain scaling. **⚠️ exact value UNVERIFIED — confirm vs
    datasheet/Waveshare `es8311.c`** (ESP-ADF sets it in `es8311_set_mic_gain`).
  - `0x17`: ADC digital volume; ~0 dB ≈ `0xBF`. **⚠️ confirm exact code vs the
    datasheet ADC-volume table.**
  - `0x15`: ADC ramp/mute — ensure ADC **not muted** (default `0x00`). **⚠️
    confirm mute-bit semantics vs datasheet.**
- `disable_adc()`: power the ADC analog back down (reuse `shutdown()`'s
  `0x0E`/`0x0D` writes) so the codec draws ~0 mA between sessions.

**Confirmation source:** the existing `init()` comment says it "mirrors the C
driver es8311_init() from Waveshare examples" — the canonical
[Waveshare ES8311 datasheet](https://files.waveshare.com/wiki/common/ES8311.DS.pdf)
+ [User Guide](https://files.waveshare.com/wiki/common/ES8311.user.Guide.pdf) and
the ESP-ADF `es8311.c` `es8311_config_adc`/`es8311_set_mic_gain` are the truth
sources for the three flagged values.

- **Accept:** compiles; a scratch call to `enable_adc()` then reading back the
  regs over I2C returns the written values (I2C-level check, no audio yet).

## MC2 — I2S full-duplex RX (add capture alongside the beep TX)

Reuse the single `I2s::new(I2S0, DMA_CH1, cfg)` in `main.rs` (~371-387).
- Destructure **both** creators from the one `I2s::new`: build TX as today
  (MCLK=GPIO19, BCLK=GPIO20, WS=GPIO22, DOUT=GPIO21) **and** build RX from the
  same instance with `.with_din(GPIO23).build(rx_desc)` — RX reuses the shared
  BCLK/WS peripheral signals, so **no clock pins are re-consumed** (verified:
  `with_bclk/with_ws` route the peripheral's own signals; esp-hal 1.1.1
  master.rs:851-864 splits DMA_CH1 into rx/tx sub-channels).
- RX descriptors: `static I2S_RX_DESC: StaticCell<[DmaDescriptor; 4]>`.
- **4–8 KB circular RX buffer** (`static`), read via `read_dma_circular`
  (blocking) — sized for ~30–60 ms at 16 kHz stereo 16-bit (64 KB/s).
- Config stays stereo 16-bit (C6 forces same config TX/RX); the mono mic lands
  on one slot — pick the correct L/R in MC3 (confirm in MC6).
- **Accept:** RX DMA fills the circular buffer with non-constant data when the
  mic is live (scratch log of a few samples).

## MC3 — RMS → dBFS math (pure, host-testable)

- `src/peripherals/mic.rs` (or a `mic` module): given a window of i16 samples,
  remove DC (subtract mean), compute `rms = sqrt(Σx²/n)`,
  `dbfs = 20·log10(rms/32768)` via `libm` (already linked by Slint). Clamp to
  [-60, 0]. EWMA-smooth across windows; track a decaying **peak-hold**.
- Window ≈ **512 stereo frames (~32 ms)**; one channel only.
- `#[cfg(test)]` host tests: silence → ≈ -60/-inf floor; full-scale square →
  ≈ 0 dBFS; a known sine at half-scale → ≈ -6 dBFS (±tolerance).
- **Accept:** tests pass on host (`cargo test`).

## MC4 — Slint "Sound Level" screen

- `ui/slint/soundlevel.slint`: a vertical/horizontal **bar or arc** mapped
  −60→0 dBFS + numeric readout + a **peak-hold tick**; near-0 dBFS uses
  `Theme.warn`. Include the on-screen caption **"relative dBFS · not calibrated
  dB SPL"** (honesty — true SPL needs mic sensitivity + calibration).
- `ui/slint/shell.slint`: reachable as a Slint screen via the **same overlay
  pattern as launcher/AOD/Radio-Scan** (`if root.mic-open: SoundLevel {…}`) with
  `in property <float> mic-dbfs`, `in property <float> mic-peak`. **It is a Slint
  screen → needs NO framebuffer**, and (like Radio Scan) must render *through* the
  shell scene — unaffected by morpheus's `Option<WatchShell>` scene-drop (task
  #27). No VecModel needed (scalar values, not a list).
- Iterate visuals in `slint-viewer` with dummy values.
- **Accept:** renders in slint-viewer; bar tracks a dummy dBFS property.

## MC5 — Launcher + ShellRequests + main dispatch wiring

- `src/apps/mod.rs`: add `AppState::SoundLevel`.
- `src/ui/slint_shell.rs`: add to `LAUNCHER_APPS` **and** the `launcher.slint`
  `for` list (lock-step order — existing comment contract); `ShellRequests`:
  `mic_open`/`mic_exit: Cell<bool>`; setters `set_mic_open`, `set_mic_dbfs`,
  `set_mic_peak` (gated on `mic_open`).
- `src/main.rs` launch dispatch: branch `AppState::SoundLevel` **before** the
  launch-drain `Framebuffer::try_new` (Slint screen, no fb — same placement rule
  as Radio Scan RS6). On enter → `enable_adc(gain)` + start RX circular DMA; per
  loop iteration drain the RX buffer, compute dBFS (MC3), push via `set_mic_dbfs`.
  On exit (swipe-right / Back) → stop RX + `disable_adc()`. Reflect `audio_on` in
  `PowerStats` while active.
- **Accept (HW):** launcher → Sound Level → live bar responds to sound; exit
  powers the ADC down (PowerStats audio_on clears).

## MC6 — HW verify + gain tuning

- Flash to C6; confirm the bar tracks ambient vs. a loud source; **tune reg 0x14
  PGA gain (+ 0x16/0x17)** so typical room sits mid-scale and a clap approaches
  0 dBFS without railing.
- **Confirm the mic's I2S slot (L vs R)** — pick the right channel in MC3.
- Confirm the three MC1-flagged register values (0x16/0x17/0x15) produce a clean
  level; lock them.
- Verify no interference with beeps (shared DMA_CH1) and display (DMA_CH0).
- Record flash-size delta; README feature line.
- **Accept:** meter usable on-glass; gain locked; no regressions.

---

## Sequencing
- MC1, MC2, MC3 are largely independent (MC1 codec, MC2 I2S, MC3 pure math) and
  can be built in parallel; MC3 is fully host-testable.
- MC4 (UI) is parallel to MC1–MC3.
- MC5 assembles MC1+MC2+MC3+MC4. MC6 is the HW gate.

## Open questions (resolved on HW in MC6)
1. **Mic I2S slot** — L or R? Set the channel in MC3 accordingly.
2. **PGA gain** (reg 0x14 nibble) — tune for mid-scale ambient.
3. **ES8311 regs 0x16/0x17/0x15 exact values** — confirm vs datasheet; the
   *registers* are correct, the *bit patterns* are the tuning surface.

## Deferred (v2, out of this plan)
- Clap/tap detector (threshold on the RMS envelope → gesture) — cheap add later.
- Voice memo (needs a RAM/flash ring buffer — 32 KB/s mono; materially bigger).
- FFT spectrum display.
- Calibrated dB **SPL** (needs mic sensitivity spec + a reference calibration).
```
