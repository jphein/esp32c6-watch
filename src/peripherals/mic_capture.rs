//! MC2 — mic capture: ES8311 ADC → I2S RX (circular DMA) → mono PCM chunks.
//!
//! Non-main.rs half of voice capture. Provides:
//!  - [`mic_capture_task`]: an embassy task that owns the built `I2sRx`, drains
//!    the circular DMA ring, extracts one channel to mono (16 kHz 16-bit LE),
//!    and pushes chunks into [`MIC_CHANNEL`] while [`RECORDING`] is set.
//!  - [`MicPcmSource`]: a [`voice_stt::PcmSource`] over the channel, so
//!    `voice_stt::stream_utterance` drains captured audio straight to the STT
//!    bridge. Ends (returns 0) as soon as `RECORDING` clears (push-to-talk release).
//!
//! The I2S RX peripheral + DMA ring are peripheral-owned, so MC5 constructs them
//! in main.rs and spawns [`mic_capture_task`] (see the module docs / hand-off
//! snippet). RX uses the blocking circular API polled from the task. The TX
//! side lives in [`silent_clock_task`]: the always-running full-duplex clock
//! master, which since v0.8.5 doubles as the playback seam (`audio_out`,
//! issue #23) — SFX samples are substituted into its circular ring.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, Ordering};

use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Receiver};
use embassy_time::{Duration, Timer};
use esp_hal::i2s::master::{I2sRx, I2sTx};
use esp_hal::Blocking;
use heapless::Vec;

use crate::net::voice_stt::{self, PcmSource};

/// Capture sample rate — matches the ES8311 ADC config + the STT bridge/Azure.
pub const MIC_SAMPLE_RATE: u32 = 16_000;
/// Which interleaved I2S slot carries the mic (confirm L/R on glass — MC6).
pub const MIC_RIGHT_CHANNEL: bool = false;
/// Starting ES8311 analog-PGA gain (reg 0x14 low nibble) for `enable_adc`;
/// tune on glass so a normal room sits mid-scale without railing (MC6).
pub const MIC_PGA_GAIN: u8 = 0x0A;
/// A capture chunk in MONO bytes: 512 B = 256 samples ≈ 16 ms @ 16 kHz.
pub const MONO_CHUNK: usize = 512;
/// The stereo read that yields one `MONO_CHUNK` (2× — interleaved L/R).
pub const STEREO_CHUNK: usize = MONO_CHUNK * 2;
/// Circular RX DMA ring. Sized so esp-hal's circular special-case (len <= CHUNK*2)
/// splits it into exactly 3 descriptors of MIC_RING_LEN/3 = STEREO_CHUNK bytes each.
/// That makes `available()` grow in whole STEREO_CHUNK units and lets the capture task
/// pop the ENTIRE available amount into a ring-sized buffer with no partial-window
/// remainder. 3072 B = 3 × 1024 ≈ 48 ms @16k stereo. MC5 allocates this static.
pub const MIC_RING_LEN: usize = STEREO_CHUNK * 3;
/// Channel depth (chunks buffered between the capture task and the streamer).
pub const MIC_CHANNEL_DEPTH: usize = 8;

/// A single captured chunk of mono PCM (empty = never sent; end is signalled via
/// [`RECORDING`], not a sentinel).
pub type MicChunk = Vec<u8, MONO_CHUNK>;
/// Channel type MC5 allocates as a `static` and passes to the task + source.
pub type MicChannel = Channel<CriticalSectionRawMutex, MicChunk, MIC_CHANNEL_DEPTH>;
type MicReceiver = Receiver<'static, CriticalSectionRawMutex, MicChunk, MIC_CHANNEL_DEPTH>;

/// Push-to-talk gate. MC5 sets it true on the Voice-page press (after
/// `enable_adc`) and false on release; the capture task only pushes while set,
/// and [`MicPcmSource`] ends the utterance the instant it clears.
pub static RECORDING: AtomicBool = AtomicBool::new(false);

/// Meter gate (#28). Set while the SoundLevel screen is open; the capture task
/// pushes chunks whenever EITHER this OR [`RECORDING`] is set, so the shared
/// mic feeds the level meter (drained by the main loop → `mic_dsp::rms_dbfs`)
/// as well as voice PTT. Voice + meter are mutually-exclusive screens, so a
/// single [`MIC_CHANNEL`] with one active consumer at a time suffices.
pub static METER: AtomicBool = AtomicBool::new(false);

/// Live capture level in **dBFS** (integer) of the most recent window — written by
/// [`mic_capture_task`] whenever RECORDING/METER is set. The Voice PTT flow parks
/// the main loop for the whole hold, so it reads this atomic to drive the
/// "LISTENING" level bar + pulse in real time (see the PTT `monitor` future in
/// main.rs). Resets to [`mic_dsp::DBFS_FLOOR`] at the start of each utterance.
pub static MIC_LEVEL: AtomicI32 = AtomicI32::new(-60);

/// User-adjustable **digital** capture gain, Q8 fixed-point (256 = 1.0×), applied
/// in [`mic_capture_task`] to the mono PCM before it feeds the meter + STT stream.
/// The ES7210 analog PGA is near its ceiling (+36 dB), so this is the headroom the
/// Sound-app gain stepper drives to lift quiet speech past Azure's energy floor.
/// Set from [`GAIN_STEPS_Q8`] indexed by the UI. Note: digital gain lifts signal
/// AND noise equally (SNR unchanged) — it helps level, not intelligibility.
pub static MIC_GAIN_Q8: AtomicU16 = AtomicU16::new(256);
/// Digital-gain ladder the Sound-app stepper walks: 0..+18 dB in 3 dB steps.
/// Q8 factor = round(256 · 10^(dB/20)). Index into both tables in lock-step.
pub const GAIN_STEPS_Q8: [u16; 7] = [256, 362, 511, 722, 1020, 1439, 2032];
pub const GAIN_STEPS_DB: [u8; 7] = [0, 3, 6, 9, 12, 15, 18];

/// Re-arm flags. AOD light sleep (`Rtc::sleep_light`) clock-gates the I2S
/// peripheral, which permanently stalls the continuous silent-TX DMA (the shared
/// mic clock) and the RX capture DMA — after the first watchface AOD the mic
/// goes dead and never recovers. The main loop sets both true after every
/// light-sleep wake; [`silent_clock_task`] and [`mic_capture_task`] drop and
/// re-arm their transfers so the full-duplex clock + capture come back.
pub static CLOCK_REARM: AtomicBool = AtomicBool::new(false);
pub static RX_REARM: AtomicBool = AtomicBool::new(false);

/// Session poll cadence: how often the ring is topped up while a clip plays.
/// Descriptors complete every 16 ms; 4 ms keeps the substitution responsive
/// with wide margin against `Late` (needs a > 48 ms stall to lose the ring).
const PUSH_POLL_MS: u64 = 4;

/// Shared-clock generator + playback seam: a continuous circular TX that is
/// SILENT except while a queued SFX clip plays (issue #23). TX is the I2S
/// master (`signal_loopback`), so this free-runs BCLK/WS and lets the mic ADC
/// clock data onto ASDOUT while RX slaves to it. Owns `i2s_tx`; re-arms on
/// [`CLOCK_REARM`] (see its docs) so the clock survives AOD light sleep.
///
/// # Playback mechanics (why sessions re-arm the transfer)
///
/// esp-hal 1.1.1's `DmaTransferTxCircular` push-state has a one-way trap: the
/// C6 GDMA runs the circular chain with `check_owner=false` (the ring loops in
/// HARDWARE forever — the clock physically cannot stop) but `auto_write_back=
/// true`, so consumed descriptors flip to CPU-owned. Only `push()` flips them
/// back — and once ALL descriptors are CPU-owned (one un-pushed ring lap,
/// i.e. always after ~48 ms of idle), `available()`/`push()` return
/// `DmaError::Late` forever. Pushing into the long-idle transfer is therefore
/// impossible by construction, and keeping the state alive by pushing silence
/// nonstop would let any executor stall > one ring lap (heavy Slint render,
/// blocking flash op) poison the state mid-capture.
///
/// So the task idles HANDS-OFF — exactly the proven pre-v0.8.5 behavior, no
/// `available()` polling, the DMA replays the (all-zero) ring — and each
/// playback session opens with a deliberate drop + `write_dma_circular`
/// re-arm to reset the push-state. That re-arm is the same sub-millisecond
/// stop/start the AOD [`CLOCK_REARM`] recovery has always used, and it lands
/// inside the half-duplex window: `audio_out::play_pcm` sets
/// [`audio_out::PLAYBACK_ACTIVE`] before the session opens, so the mic is
/// already discarding. The ring is all-silence whenever idle (the feeder's
/// tail scrubs it — see `audio_out`), so the fresh transfer replays silence.
///
/// During a session the loop polls every [`PUSH_POLL_MS`] and tops the ring
/// up via [`audio_out::PlaybackFeeder`]: clip samples (mono → stereo) while
/// staged, silence otherwise. Any `Late`/DMA error mid-session aborts the
/// clip and re-arms — the mic clock always comes back.
#[embassy_executor::task]
pub async fn silent_clock_task(mut i2s_tx: I2sTx<'static, Blocking>, ring: &'static [u8]) {
    use crate::peripherals::audio_out::{self, PlaybackFeeder};

    let mut feeder = PlaybackFeeder::new();
    // A clip chunk received during idle-wait; opens a session on the NEXT
    // (fresh) transfer armed at the top of the loop.
    let mut pending: Option<audio_out::PcmChunk> = None;
    loop {
        let mut xfer = match i2s_tx.write_dma_circular(&ring) {
            Ok(x) => x,
            Err(_) => {
                Timer::after(Duration::from_millis(50)).await;
                continue;
            }
        };
        // === Playback session (fresh transfer → clean push-state) ===
        if let Some(first) = pending.take() {
            feeder.begin(first);
            let clean = loop {
                if CLOCK_REARM.swap(false, Ordering::Relaxed) {
                    break false; // light-sleep gated the clock mid-clip
                }
                if !top_up(&mut xfer, &mut feeder) {
                    break false; // Late/DMA error (executor stalled > ring)
                }
                if feeder.is_idle() {
                    break true; // clip + tail pushed; ring scrubbed to silence
                }
                Timer::after(Duration::from_millis(PUSH_POLL_MS)).await;
            };
            if !clean {
                // A long clip (the #58 ping chime) SURVIVES a DMA error; only a
                // queue-fed clip is abandoned.
                //
                // Why this matters: a received ping wakes the screen and forces a
                // full-frame repaint (~200 ms measured) — four times the 48 ms
                // ring — so `top_up` reliably hits `Late` on exactly the path the
                // chime exists for. The old unconditional `abort()` threw the
                // melody away, so a real ping made NO sound and logged NOTHING,
                // while the debug console's identical `play_chime()` worked
                // perfectly because nothing repaints after it.
                if feeder.resync() {
                    // Melody still has bytes left: re-arm and keep streaming from
                    // where LONG_CLIP left off. `pending` stays None — the feeder
                    // pulls the next chunk itself.
                    drop(xfer);
                    Timer::after(Duration::from_millis(2)).await;
                    pending = audio_out::next_resume_chunk();
                    continue;
                }
                feeder.abort();
                drop(xfer); // stop the poisoned/stalled transfer …
                Timer::after(Duration::from_millis(2)).await;
                continue; // … and re-arm a fresh clock immediately
            }
        }
        // === Idle: hands off the running transfer (see doc-comment) ===
        // The DMA loops the silent ring in hardware; wake on the next queued
        // clip (instant) or a CLOCK_REARM (polled) — both re-arm the transfer.
        match select(audio_out::next_clip(), rearm_requested()).await {
            Either::First(chunk) => pending = Some(chunk),
            Either::Second(()) => {}
        }
        drop(xfer); // stop the stalled/stale transfer; the outer loop re-arms
        Timer::after(Duration::from_millis(2)).await;
    }
}

/// Resolves when [`CLOCK_REARM`] is raised (post light-sleep), consuming it.
async fn rearm_requested() {
    while !CLOCK_REARM.swap(false, Ordering::Relaxed) {
        Timer::after(Duration::from_millis(100)).await;
    }
}

/// Push everything the DMA has consumed back into the circular TX ring —
/// clip samples while the feeder has them staged, silence otherwise. Returns
/// false on a DMA error (`Late` after an executor stall — caller re-arms).
///
/// `available()` grows in whole ring-descriptor units (`STEREO_CHUNK`), so
/// the stage buffer drains it in ≤ one-descriptor slices; `& !3` guards whole
/// stereo frames. Plain fn (not async) so the 1 KB stage stays off the
/// statically-allocated task future.
fn top_up(
    xfer: &mut esp_hal::dma::DmaTransferTxCircular<'_, I2sTx<'static, Blocking>>,
    feeder: &mut crate::peripherals::audio_out::PlaybackFeeder,
) -> bool {
    let mut stage = [0u8; STEREO_CHUNK];
    loop {
        let avail = match xfer.available() {
            Ok(a) => a,
            Err(_) => return false,
        };
        let n = avail.min(STEREO_CHUNK) & !3;
        if n == 0 {
            return true;
        }
        feeder.fill_stereo(&mut stage[..n]);
        if xfer.push(&stage[..n]).is_err() {
            return false;
        }
    }
}

/// [`PcmSource`] backed by [`MIC_CHANNEL`]. Hand `voice_stt::stream_utterance`
/// a `&mut MicPcmSource` while the button is held.
pub struct MicPcmSource {
    rx: MicReceiver,
}

impl MicPcmSource {
    pub fn new(rx: MicReceiver) -> Self {
        Self { rx }
    }
}

impl PcmSource for MicPcmSource {
    async fn next_chunk(&mut self, buf: &mut [u8]) -> usize {
        if !RECORDING.load(Ordering::Relaxed) {
            return 0; // push-to-talk released
        }
        // Wait for the next captured chunk, but bail immediately (end-of-utterance)
        // if RECORDING clears mid-wait so a release is always responsive.
        match select(self.rx.receive(), recording_cleared()).await {
            Either::First(chunk) => {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                n
            }
            Either::Second(()) => 0,
        }
    }
}

/// Resolves when [`RECORDING`] goes false (polled cheaply).
async fn recording_cleared() {
    while RECORDING.load(Ordering::Relaxed) {
        Timer::after(Duration::from_millis(10)).await;
    }
}

/// Capture task: drain the ES8311 ADC over I2S RX circular DMA into mono chunks.
///
/// Owns the built `I2sRx` + the `'static` DMA ring. Runs forever: while
/// [`RECORDING`] it pops stereo frames, extracts mono, and `try_send`s chunks
/// (dropping on channel-full = shed oldest audio under backpressure); while idle
/// it drains-and-discards so the circular ring never overflows.
#[embassy_executor::task]
pub async fn mic_capture_task(
    mut i2s_rx: I2sRx<'static, Blocking>,
    ring: &'static mut [u8; MIC_RING_LEN],
    sender: embassy_sync::channel::Sender<'static, CriticalSectionRawMutex, MicChunk, MIC_CHANNEL_DEPTH>,
) {
    // Circular RX with FULL-DRAIN + OVERRUN RECOVERY. esp-hal's DmaTransferRxCircular
    // has two traps this navigates:
    //  (1) `pop(buf)` returns Err(BufferTooSmall) unless buf.len() >= the *entire*
    //      currently-available amount, and `available()` grows in whole-descriptor
    //      (STEREO_CHUNK) units. Popping into a small STEREO_CHUNK buffer therefore
    //      fails the instant one descriptor completes — the consumer never receives
    //      bytes (the real zero-PCM cause). So we pop the WHOLE ring's worth into
    //      `popbuf` (ring-sized) and process it in STEREO_CHUNK windows. MIC_RING_LEN
    //      = 3×STEREO_CHUNK, so a pop is always a whole number of windows (no partial
    //      remainder → no dropped samples).
    //  (2) once the ring laps (all descriptors CPU-owned) `available()`/`pop()` return
    //      Err(Late) permanently, and pop() is the only thing that re-arms descriptors
    //      (owner → DMA). Draining the full amount every tick keeps the ring empty so
    //      this never happens in steady state; the outer loop re-arms via a fresh
    //      read_dma_circular if it ever does (e.g. a one-off startup stall).
    //
    // WriteBuffer needs a `&'static mut`, so re-materialise one from the (truly
    // 'static) ring by raw pointer on each restart; the previous transfer is always
    // dropped first, so there is never an aliasing `&mut`.
    // NOTE (mic topology fix): the ES8311 record path is enabled in main.rs (enable_adc)
    // and clocked by the continuous SILENT full-duplex TX — signal_loopback=true makes the
    // SoC TX the single BCLK/WS master and this RX slaves to it. The earlier "HW-blocked"
    // verdict was OVERTURNED: vendor firmware captured JP's voice, proving the mic HW is
    // fine; the gap was the serial-clock topology, now fixed. This RX pipeline (DMA drain
    // + frame flow) was already proven end-to-end and is unchanged.
    let ring_ptr: *mut [u8; MIC_RING_LEN] = ring;
    let mut popbuf = [0u8; MIC_RING_LEN]; // holds a full ring's worth (max available)
    'restart: loop {
        let ring_ref: &'static mut [u8; MIC_RING_LEN] = unsafe { &mut *ring_ptr };
        let mut xfer = match i2s_rx.read_dma_circular(ring_ref) {
            Ok(x) => x,
            Err(_) => {
                Timer::after(Duration::from_millis(50)).await;
                continue 'restart; // RX DMA failed to start; retry
            }
        };
        loop {
            // Re-arm after an AOD light-sleep wake gated the RX DMA (a stalled
            // ring can sit at available()==0 forever, never hitting the Err path).
            if RX_REARM.swap(false, Ordering::Relaxed) {
                break; // → 'restart re-arms read_dma_circular
            }
            let avail = match xfer.available() {
                Ok(n) => n,
                Err(_) => break, // Late/overrun → drop xfer & re-arm the descriptor chain
            };
            if avail == 0 {
                Timer::after(Duration::from_millis(4)).await;
                continue;
            }
            // Pop the ENTIRE available amount — popbuf is ring-sized so it always fits,
            // and pop() re-arms every consumed descriptor (owner → DMA), preventing lap.
            let n = match xfer.pop(&mut popbuf[..]) {
                Ok(n) => n,
                Err(_) => break, // BufferTooSmall can't happen (popbuf = ring); a Late → re-arm
            };
            // Half-duplex (#23): while a clip plays, the mic would only hear
            // the speaker (no AEC on the C6) — discard the windows outright.
            if crate::peripherals::audio_out::PLAYBACK_ACTIVE.load(Ordering::Relaxed)
                || (!RECORDING.load(Ordering::Relaxed) && !METER.load(Ordering::Relaxed))
            {
                continue; // idle/suppressed: popped = drained + re-armed; discard
            }
            let mut off = 0;
            while off + STEREO_CHUNK <= n {
                let window = &popbuf[off..off + STEREO_CHUNK];
                let mut mono_buf = [0u8; MONO_CHUNK];
                let m = voice_stt::stereo_to_mono_le(window, &mut mono_buf, MIC_RIGHT_CHANNEL);
                // Apply user digital gain (Q8) IN-PLACE so both the meter and the STT
                // stream see the boost; clamp to i16 so a hot setting saturates cleanly.
                let g = MIC_GAIN_Q8.load(Ordering::Relaxed) as i32;
                if g != 256 {
                    let mut k = 0;
                    while k + 1 < m {
                        let s = i16::from_le_bytes([mono_buf[k], mono_buf[k + 1]]) as i32;
                        let b = (((s * g) >> 8).clamp(-32768, 32767) as i16).to_le_bytes();
                        mono_buf[k] = b[0];
                        mono_buf[k + 1] = b[1];
                        k += 2;
                    }
                }
                // Live input level (dBFS) of this window → drives the PTT LISTENING bar.
                let samp_n = m / 2;
                let mut lvl_buf = [0i16; MONO_CHUNK / 2];
                for k in 0..samp_n {
                    lvl_buf[k] = i16::from_le_bytes([mono_buf[2 * k], mono_buf[2 * k + 1]]);
                }
                if samp_n > 0 {
                    MIC_LEVEL.store(mic_dsp::rms_dbfs(&lvl_buf[..samp_n]) as i32, Ordering::Relaxed);
                }
                if let Ok(chunk) = MicChunk::from_slice(&mono_buf[..m]) {
                    let _ = sender.try_send(chunk); // drop on full = shed oldest audio (bounded latency)
                }
                off += STEREO_CHUNK;
            }
        }
        // xfer dropped here → outer loop re-arms the transfer (recover from overrun)
    }
}
