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
//! snippet). TX (beep) stays blocking-mode and untouched — RX uses the blocking
//! circular API polled from the task, so no I2S mode change is needed.

use core::sync::atomic::{AtomicBool, Ordering};

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

/// Re-arm flags. AOD light sleep (`Rtc::sleep_light`) clock-gates the I2S
/// peripheral, which permanently stalls the continuous silent-TX DMA (the shared
/// mic clock) and the RX capture DMA — after the first watchface AOD the mic
/// goes dead and never recovers. The main loop sets both true after every
/// light-sleep wake; [`silent_clock_task`] and [`mic_capture_task`] drop and
/// re-arm their transfers so the full-duplex clock + capture come back.
pub static CLOCK_REARM: AtomicBool = AtomicBool::new(false);
pub static RX_REARM: AtomicBool = AtomicBool::new(false);

/// Shared-clock generator: a continuous SILENT circular TX. TX is the I2S master
/// (`signal_loopback`), so this free-runs BCLK/WS and lets the ES8311 ADC clock
/// data onto ASDOUT while RX slaves to it. Owns `i2s_tx`; re-arms on
/// [`CLOCK_REARM`] (see its docs) so the clock survives AOD light sleep.
#[embassy_executor::task]
pub async fn silent_clock_task(mut i2s_tx: I2sTx<'static, Blocking>, silence: &'static [u8]) {
    loop {
        let xfer = match i2s_tx.write_dma_circular(&silence) {
            Ok(x) => x,
            Err(_) => {
                Timer::after(Duration::from_millis(50)).await;
                continue;
            }
        };
        // Hold the clock running until a re-arm is requested (post light-sleep).
        while !CLOCK_REARM.swap(false, Ordering::Relaxed) {
            Timer::after(Duration::from_millis(100)).await;
        }
        drop(xfer); // stop the stalled transfer; the outer loop re-arms a fresh one
        Timer::after(Duration::from_millis(2)).await;
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
    // === MIC-CAPTURE VERIFY probe (temporary; remove before v0.6.1 merge) ===
    // Confirms the ADC serial-out is now LIVE under the native full-duplex clock: prints
    // the running peak |sample| + the first raw i16 frames as hex, throttled so serial is
    // not flooded. A non-zero peak that rises when JP speaks = MIC FIXED.
    let mut probe_ctr: u32 = 0;
    let mut probe_peak: i16 = 0;
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
            if !RECORDING.load(Ordering::Relaxed) && !METER.load(Ordering::Relaxed) {
                continue; // idle: popped = drained + re-armed; just discard
            }
            let mut off = 0;
            while off + STEREO_CHUNK <= n {
                let window = &popbuf[off..off + STEREO_CHUNK];
                // [VERIFY] running peak |sample| across the raw stereo window.
                let mut i = 0;
                while i + 1 < window.len() {
                    let a = i16::from_le_bytes([window[i], window[i + 1]]).saturating_abs();
                    if a > probe_peak {
                        probe_peak = a;
                    }
                    i += 2;
                }
                let mut mono_buf = [0u8; MONO_CHUNK];
                let m = voice_stt::stereo_to_mono_le(window, &mut mono_buf, MIC_RIGHT_CHANNEL);
                if let Ok(chunk) = MicChunk::from_slice(&mono_buf[..m]) {
                    let _ = sender.try_send(chunk); // drop on full = shed oldest audio (bounded latency)
                }
                off += STEREO_CHUNK;
            }
            // [VERIFY] throttled report (~every 16 pops). Remove before v0.6.1 merge.
            probe_ctr = probe_ctr.wrapping_add(1);
            if probe_ctr % 16 == 0 {
                let s0 = u16::from_le_bytes([popbuf[0], popbuf[1]]);
                let s1 = u16::from_le_bytes([popbuf[2], popbuf[3]]);
                let s2 = u16::from_le_bytes([popbuf[4], popbuf[5]]);
                let s3 = u16::from_le_bytes([popbuf[6], popbuf[7]]);
                esp_println::println!(
                    "[MICHEX] n={} peak={} raw=[{:04x} {:04x} {:04x} {:04x}]",
                    n, probe_peak, s0, s1, s2, s3
                );
                probe_peak = 0;
            }
        }
        // xfer dropped here → outer loop re-arms the transfer (recover from overrun)
    }
}
