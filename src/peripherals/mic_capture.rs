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
use esp_hal::i2s::master::I2sRx;
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
/// Circular RX DMA ring size (≈128 ms @16k stereo) — MC5 allocates this static.
pub const MIC_RING_LEN: usize = 8192;
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
    // Circular RX with OVERRUN RECOVERY. esp-hal's DmaTransferRxCircular deadlocks
    // if the ring ever laps the read pointer: once every descriptor is CPU-owned,
    // both `available()` and `pop()` return `Err(Late)` *permanently*, and since
    // `pop()` is the only thing that re-arms descriptors (owner → DMA), a single
    // startup overrun strands capture forever. That was the "avail stuck at 0" bug:
    // the ring filled once, all 3 descriptors went CPU-owned, the DMA parked
    // (in_dscr_empty), and the old `available().unwrap_or(0)` silently swallowed the
    // `Late`. Fix: on any `available()`/`pop()` error, drop the transfer and re-arm
    // via a fresh `read_dma_circular`. The idle-drain below keeps the ring empty so
    // overruns don't recur in steady state (16 kHz stereo = 64 KB/s, trivially kept up).
    //
    // WriteBuffer requires a `&'static mut`, so re-materialise one from the (truly
    // 'static) ring via raw pointer on each restart; the previous transfer is always
    // dropped first, so there is never an aliasing `&mut`.
    let ring_ptr: *mut [u8; MIC_RING_LEN] = ring;
    let mut stereo = [0u8; STEREO_CHUNK];
    let mut dbg_ctr: u32 = 0; // #28 tuning debug throttle (THROWAWAY — remove before v0.6.0 tag)
    let mut raw_ctr: u32 = 0; // raw-DMA fill probe throttle (THROWAWAY — remove before v0.6.0 tag)
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
            let avail = match xfer.available() {
                Ok(n) => n,
                // Late/overrun (ring lapped) → drop xfer & re-arm the descriptor chain.
                Err(_) => break,
            };
            raw_ctr = raw_ctr.wrapping_add(1);
            if raw_ctr % 256 == 0 {
                esp_println::println!("[MICRAW] avail={} B", avail); // THROWAWAY — remove before tag
            }
            if avail < STEREO_CHUNK {
                Timer::after(Duration::from_millis(4)).await;
                continue;
            }
            if xfer.pop(&mut stereo).is_err() {
                break; // pop error (Late) → drop xfer & re-arm
            }
            if !RECORDING.load(Ordering::Relaxed) && !METER.load(Ordering::Relaxed) {
                continue; // idle: discard (keeps the circular ring drained)
            }
            // ===== #28 mic L/R tuning debug — THROWAWAY, REMOVE BEFORE v0.6.0 TAG =====
            // While the SoundLevel meter is open, log BOTH I2S-slot RMS every ~32 chunks
            // (~0.5s) so the JP-hands session reads which slot carries the mic in ONE
            // flash (the louder channel = the mic → set MIC_RIGHT_CHANNEL accordingly).
            if METER.load(Ordering::Relaxed) {
                dbg_ctr = dbg_ctr.wrapping_add(1);
                if dbg_ctr % 32 == 0 {
                    let frames = (STEREO_CHUNK / 4).min(MONO_CHUNK / 2);
                    let mut l = [0i16; MONO_CHUNK / 2];
                    let mut r = [0i16; MONO_CHUNK / 2];
                    for i in 0..frames {
                        l[i] = i16::from_le_bytes([stereo[4 * i], stereo[4 * i + 1]]);
                        r[i] = i16::from_le_bytes([stereo[4 * i + 2], stereo[4 * i + 3]]);
                    }
                    esp_println::println!(
                        "[MICDBG] L={} R={} dBFS  (louder slot = mic -> MIC_RIGHT_CHANNEL)",
                        mic_dsp::rms_dbfs(&l[..frames]) as i32,
                        mic_dsp::rms_dbfs(&r[..frames]) as i32,
                    );
                }
            }
            // ===== end throwaway debug =====
            // One STEREO_CHUNK pop -> exactly MONO_CHUNK mono bytes.
            let mut mono_buf = [0u8; MONO_CHUNK];
            let m = voice_stt::stereo_to_mono_le(&stereo, &mut mono_buf, MIC_RIGHT_CHANNEL);
            if let Ok(chunk) = MicChunk::from_slice(&mono_buf[..m]) {
                let _ = sender.try_send(chunk); // drop on full = shed oldest audio (bounded latency)
            }
        }
        // xfer dropped here → outer loop re-arms the transfer (recover from overrun)
    }
}
