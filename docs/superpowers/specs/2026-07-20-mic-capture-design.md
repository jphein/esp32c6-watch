# Mic Capture (ES8311 ADC / I2S RX) — Feasibility + v1 Design

Date: 2026-07-20
Status: research finding for JP review (docs-only; no code)
Author: Nebula (dreamteam), task #28
Base: 0deb151

## TL;DR verdict — 🟢 GREEN

Mic capture is straightforward. The I2S RX path is fully supported in esp-hal
~1.1, the codec ADC is ~90% configured already, the wiring exists (just add
DIN=GPIO23), and the RAM cost is a few KB. A v1 **sound-level (dBFS) meter** is
a small, self-contained feature. Recommend building it.

## 1. I2S RX — VERIFIED (the key finding)

**Full-duplex is a single peripheral + single DMA channel.** `I2s::new(I2S0,
DMA_CH1, config)` returns a struct exposing **both** `i2s_rx: RxCreator` and
`i2s_tx: TxCreator`; the one DMA channel is split into rx/tx sub-channels
(esp-hal `i2s/master.rs`: `channel.rx` / `channel.tx`, lines 851-864). On the
C6, RX and TX are forced to the **same config** (16 kHz / 16-bit) — fine, it
matches the existing beep path.

**Pin sharing — no conflict.** `with_bclk`/`with_ws` just route the peripheral's
*own generated* BCLK/WS signals to a GPIO (master mode; `self.i2s.bclk_signal()
.connect_to(pin)`). BCLK/WS are shared across TX and RX. So:
- TX creator routes **MCLK=GPIO19, BCLK=GPIO20, WS/LRCK=GPIO22, DOUT=GPIO21**
  (exactly what `main.rs` does today).
- RX creator only adds **DIN=GPIO23** (`RxCreator::with_din` → `din_signal()
  .connect_to`). It reuses the clocks TX already routed.

**RX read API** (`I2sRx`): `read_words`, `read_dma`, **`read_dma_circular`**
(→ `DmaTransferRxCircular`, ideal for a continuous meter), plus async
`read_dma_circular_async` / `read_dma_async`. Blocking circular DMA is the
simplest fit for v1.

**Concrete change to the existing setup** (`main.rs` ~371-387): today only
`i2s_periph.i2s_tx…build()` is built. To add capture, destructure both creators
from the one `I2s::new`, build TX as now (MCLK/BCLK/WS/DOUT) and also build RX
with `.with_din(GPIO23).build(rx_desc)`. DMA: display owns DMA_CH0, audio owns
DMA_CH1; RX shares DMA_CH1's rx sub-channel — no new channel, no conflict.

For a mic-only meter (no simultaneous playback) you could build **RX-only**
(`i2s_rx.with_bclk.with_ws.with_din.build`), but building both once at boot is
cleaner and keeps the beep path intact.

## 2. ES8311 ADC — what's there vs. what's missing

`src/peripherals/audio.rs::init()` already configures much of the ADC side (as a
byproduct of the DAC setup):
- ADC serial port 16-bit: reg `0x0A = 0x0C` ✅
- ADC OSR: reg `0x03 = 0x10` ✅; ADC div: reg `0x05` ✅
- Analog powered + PGA/modulator enabled: reg `0x0D = 0x01`, reg `0x0E = 0x02` ✅
- ADC EQ bypass + DC-offset cancel: reg `0x1C = 0x6A` ✅

**Missing for a usable capture level** (needs a new `enable_adc()` path):
- **Mic input select + PGA gain** — reg `0x14` (analog input select + PGA gain;
  e.g. `0x1A`-ish for mic + moderate gain). Not set today.
- **ADC digital volume** — reg `0x17` (0 dB ≈ `0xBF`). Not set today.
- **ADC gain/ramp** — reg `0x16` (ADC gain scale). Not set today.
- **ADC unmute** — ensure the ADC mute bit (reg `0x15`) is clear.
- **Re-power after boot** — `main.rs:369` calls `audio_codec.shutdown()`, which
  sets `0x0E=0xFF` / `0x0D=0xFC` (powers the ADC analog **down**). So capture
  must first re-enable `0x0D=0x01`, `0x0E=0x02` (like `unmute()` does), then set
  the mic gain/volume registers above.

Action: add `Es8311::enable_adc(gain)` / `disable_adc()` mirroring the existing
`unmute`/`shutdown` structure. **Exact register values must be confirmed against
the ES8311 datasheet + the Waveshare C reference** (`init()` cites that ref) —
the values above are the right registers; verify the bit patterns.

Note: gain vs. clipping is empirical — the analog PGA (reg 0x14) + ADC digital
volume (0x17) together set sensitivity; tune on-device against ambient + a loud
source so RMS neither floors nor rails.

## 3. RAM / DMA budget

- Config is 16 kHz, 16-bit; peripheral runs **stereo** (forced same-config as
  the stereo beep path) → 64 KB/s. The mic is mono; it lands on one channel
  (verify L vs R on-device), the other is ignored/duplicate.
- Meter window: ~30–50 ms → ~2–3 KB. Circular DMA buffer of **4–8 KB** covers
  30–60 ms with headroom. DMA descriptor chain: a handful (`[DmaDescriptor; 4]`).
- **Total: single-digit KB.** No framebuffer involved (Slint page). Coexists
  with display (DMA_CH0) and beeps (DMA_CH1 tx) fine. ADC analog draws a few mA
  while active → gate it behind page entry/exit and reflect in `PowerStats`
  (`audio_on`).

## 4. v1 design — Sound-Level (dBFS) meter

**Placement:** a Slint launcher app (live-data UI → Slint, same reasoning as the
mesh/sensors pages). On entry: `enable_adc(gain)` + start RX circular DMA. On
exit: stop RX + `disable_adc()`/`shutdown()`.

**Signal path:**
1. `read_dma_circular` into a rolling window (e.g. 512 stereo frames ≈ 32 ms).
2. Per window: take one channel's i16 samples, remove DC (subtract mean), sum of
   squares → `rms = sqrt(Σx² / n)`.
3. `dbfs = 20 · log10(rms / 32768)` (range ≈ −60 dBFS … 0 dBFS). Use `libm`
   (already pulled by Slint) or an integer log approximation.
4. Update the UI at ~10–20 Hz (EWMA-smooth the value so the bar isn't jittery).

**UI (`ui/slint/soundlevel.slint`):** a vertical/horizontal bar or arc mapped
from −60→0 dBFS, a numeric readout, and a peak-hold tick. Reuse `theme.slint`
(accent for level, `warn` near 0 dBFS). Include an on-screen caption:
**"relative level (dBFS), not calibrated dB SPL"** — honesty, because true SPL
needs the mic's sensitivity spec + a calibration reference we don't have.

**Cadence/power:** ADC on only while the page is open; auto-nothing needed since
it's user-foreground. Reflect `audio_on` in PowerStats while active.

## 5. Stretch options (note, not v1)

- **Clap / tap detector** — threshold on the RMS envelope + debounce → a gesture
  event (e.g. double-clap to launch something). Cheap add on top of the meter.
- **Voice memo / recording** — needs a real RAM or flash ring buffer (16 kHz
  mono 16-bit = 32 KB/s → seconds of audio is tens–hundreds of KB). RAM is tight
  (no PSRAM); flash writes to a spare partition (see `partitions.csv`) are the
  path. Materially bigger than the meter — separate feature.
- **FFT / spectrum** — a small `microfft`-style real FFT on a window for a
  frequency-bar display; more CPU + a bit more RAM, but feasible.

## 6. Risks / open questions

1. **Mic channel & polarity** — confirm which I2S slot (L/R) the ES8311 ADC
   drives on this board, and set gain so typical ambient sits mid-scale. On-HW
   tuning task.
2. **ES8311 ADC register values** — the *registers* are identified; confirm the
   exact bit patterns against the datasheet / Waveshare C `es8311` driver before
   trusting capture level.
3. **dBFS ≠ SPL** — v1 is a relative meter; say so in the UI. Calibrated SPL is
   out of scope (needs mic sensitivity + reference).
4. **Full-duplex simultaneity** — v1 doesn't need mic + beep at once; if a future
   feature does, verify the shared DMA_CH1 handles concurrent rx+tx (esp-hal
   models it, but exercise it on HW).

## 7. Recommendation

Build the v1 dBFS meter — it's a clean GREEN: minimal wiring (add DIN=GPIO23),
a small `enable_adc()` addition to `audio.rs`, a few-KB circular DMA read, and a
simple Slint page. Ships as a self-contained launcher app with no storage.

## Sources
- esp-hal 1.1.1 `src/i2s/master.rs` (I2s/RxCreator/TxCreator, read_dma_circular)
- `src/peripherals/audio.rs` (ES8311 init — ADC regs already set), `src/board.rs`
  (ADC out = GPIO23), `src/main.rs` (I2S TX setup ~371-387, boot shutdown :369)
