//! Shared I2S TX playback seam (issue #23): sound effects through the
//! always-running silent-clock ring.
//!
//! # Why this shape
//!
//! The I2S TX is the full-duplex clock MASTER (`signal_loopback`): its
//! free-running BCLK/WS is what clocks the ES7210 mic ADC. The TX therefore
//! streams a circular DMA ring forever ([`mic_capture::silent_clock_task`]) —
//! playback must SUBSTITUTE samples into that ring, never own or stop the
//! transfer. This module is the substitution seam:
//!
//! - [`play_pcm`] queues **mono 16 kHz s16le** PCM (the project-standard
//!   format: STT, HA speaker, bridge) into a bounded channel, non-blocking.
//! - [`PlaybackFeeder`] (driven by `silent_clock_task`'s ring top-up loop)
//!   drains the channel, expands mono → stereo (`mic_dsp::mono_to_stereo_le`,
//!   the TX ring runs Data16Channel16), and hands the samples to
//!   `DmaTransferTxCircular::push`; silence otherwise.
//! - [`service_amp`] (called from the main loop, which owns the amp GPIO and
//!   the ES8311 via the shared I2C bus) sequences the speaker amp (GPIO6) +
//!   codec power around playback: both are ON only while a clip is in flight.
//!
//! # Queue semantics
//!
//! [`play_pcm`] REJECTS the remainder when the queue fills (returns bytes
//! actually queued): a full queue means audio is already saturated, and
//! truncating the new clip's tail beats garbling the in-flight one
//! (drop-oldest would corrupt mid-clip). Depth 8 × 512 B = 128 ms.
//!
//! # Half-duplex
//!
//! No AEC on the C6 — the mic would just hear the speaker. [`PLAYBACK_ACTIVE`]
//! is set from the first queued byte until the post-clip tail has played out;
//! `mic_capture_task` discards capture windows while it is set.
//!
//! # Pop insurance
//!
//! Three layers, all cheap: (1) clips are synthesized with attack/release
//! ramps (mic-dsp); (2) the amp powers up into an actively-driven all-silence
//! line (the ring streams zeros continuously) and the feeder holds the clip
//! until [`AMP_READY`], so ≥ one ring (~48 ms) of driven silence precedes the
//! first real sample; (3) the feeder pads [`TAIL_STEREO_BYTES`] of silence
//! after the last sample — which also scrubs the ring back to all-zero (ring
//! invariant: all-silence whenever idle) — before releasing the amp.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Instant;
use embedded_hal::i2c::I2c;
use esp_hal::gpio::Output;
use heapless::Vec;

use crate::peripherals::audio::Es8311;
use crate::peripherals::mic_capture::STEREO_CHUNK;

/// Playback sample rate (mono in, matches the 16 kHz TX ring). Part of the
/// seam contract for streamed sources (HA speaker follow-up) — unused by the
/// in-tree SFX callers, which synthesize at 16 kHz directly.
#[allow(dead_code)]
pub const PLAY_SAMPLE_RATE: u32 = 16_000;
/// One queued chunk in MONO bytes (= 256 samples = 16 ms @ 16 kHz — same
/// granularity as the mic path's `MONO_CHUNK`).
pub const PLAY_CHUNK: usize = 512;
/// Queue depth: 8 × 16 ms = 128 ms of buffered audio. Covers every SFX clip
/// whole (beep = 4 chunks) with headroom for a streamed source later (#HA-TTS).
pub const PLAY_QUEUE_DEPTH: usize = 8;

/// The TX clock ring in STEREO bytes. 3 descriptors × `STEREO_CHUNK` — the
/// same 3-descriptor circular geometry as the mic RX ring (whole-descriptor
/// `available()` growth, no partial windows). 3072 B ≈ 48 ms @ 16 kHz stereo.
pub const TX_RING_LEN: usize = 3 * STEREO_CHUNK;

/// Post-clip silence padding in STEREO bytes: one full ring (guarantees every
/// ring byte is re-zeroed — the idle-ring invariant) + one descriptor of DAC
/// flush ≈ 64 ms. Also bridges same-length underrun gaps in a streamed source
/// without dropping the amp between chunks.
const TAIL_STEREO_BYTES: usize = TX_RING_LEN + STEREO_CHUNK;

/// If the main loop hasn't raised the amp this long after a clip was queued
/// (it normally does within one tick), play into the muted DAC anyway so the
/// queue always drains and the mic-suppression flag can never stick.
const AMP_WAIT_MS: u64 = 1000;

/// A queued chunk of mono 16 kHz s16le PCM.
pub type PcmChunk = Vec<u8, PLAY_CHUNK>;

/// Bounded SFX queue: producers anywhere (main loop, debug console) →
/// consumer is the clock task's [`PlaybackFeeder`].
static PLAYBACK: Channel<CriticalSectionRawMutex, PcmChunk, PLAY_QUEUE_DEPTH> = Channel::new();

/// Half-duplex gate: true from enqueue until the post-clip tail has played.
/// `mic_capture_task` discards capture windows while set (no AEC — the mic
/// would only hear the speaker).
pub static PLAYBACK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Persisted master-volume as the ES8311 0x32 register value (#59). Set from
/// the config volume step at boot + on every volume change; read by
/// [`service_amp`] so EVERY clip (chime/beeps/clicks/tick) plays at the stored
/// level. Default = the config default step 11 via [`vol_to_reg`]. `0x00` when
/// muted (codec silent while the amp still cycles normally).
pub static MASTER_VOL_REG: AtomicU8 = AtomicU8::new(0xD0);

/// Map a volume STEP (0..=15) + mute to the ES8311 master-volume register
/// (0x32). Muted → 0. Otherwise a linear ramp `0x30..=0xFF` so even step 0
/// stays audibly present (true silence is the separate mute), step 15 = max.
pub fn vol_to_reg(level: u8, muted: bool) -> u8 {
    if muted {
        return 0x00;
    }
    let level = level.min(15) as u16;
    (0x30 + level * (0xFF - 0x30) / 15) as u8
}

/// Apply a volume step + mute to the master-volume atomic AND, if a clip's amp
/// is up right now, to the live codec so a change mid-playback is heard at
/// once (the usual case: the change itself queues a feedback tick). Returns
/// the register value stored.
pub fn set_master_volume<I: I2c>(codec: &mut Es8311<I>, level: u8, muted: bool) -> u8 {
    let reg = vol_to_reg(level, muted);
    MASTER_VOL_REG.store(reg, Ordering::Relaxed);
    if AMP_READY.load(Ordering::Relaxed) {
        let _ = codec.set_volume(reg);
    }
    reg
}

/// "Amp + codec should be ON" — set by [`play_pcm`], cleared by the feeder
/// after the tail. The main loop's [`service_amp`] acts on the edges.
static AMP_REQUEST: AtomicBool = AtomicBool::new(false);

/// "Amp + codec ARE on" — set/cleared only by [`service_amp`]. The feeder
/// holds clips until this is true so no audio is spent into a muted DAC.
static AMP_READY: AtomicBool = AtomicBool::new(false);

/// Queue mono 16 kHz s16le PCM for playback on the shared TX ring.
///
/// Non-blocking. Returns the number of bytes actually queued: when the queue
/// fills, the REMAINDER IS REJECTED (see module docs — never drops-oldest).
/// Safe from any task; the amp comes up via the main loop's [`service_amp`].
pub fn play_pcm(pcm: &[u8]) -> usize {
    let mut queued = 0;
    for chunk in pcm.chunks(PLAY_CHUNK) {
        let Ok(v) = PcmChunk::from_slice(chunk) else { break };
        if PLAYBACK.try_send(v).is_err() {
            break; // full: reject the remainder
        }
        queued += chunk.len();
    }
    if queued > 0 {
        // Order matters for the half-duplex gate: suppress the mic before the
        // first sample can possibly reach the speaker.
        PLAYBACK_ACTIVE.store(true, Ordering::Relaxed);
        AMP_REQUEST.store(true, Ordering::Relaxed);
    }
    queued
}

/// True while a clip (or its amp-release tail) is still in flight. Seam API
/// for pacing streamed sources (HA speaker follow-up); SFX callers fire-and-
/// forget, so nothing in-tree calls it yet.
#[allow(dead_code)]
pub fn busy() -> bool {
    PLAYBACK_ACTIVE.load(Ordering::Relaxed)
}

/// Await the chunk that opens the next playback session (idle wait for the
/// clock task — resolves the instant [`play_pcm`] queues something).
pub async fn next_clip() -> PcmChunk {
    PLAYBACK.receive().await
}

/// Amp (GPIO6) + ES8311 sequencing, driven once per main-loop tick (plus
/// inline right after each `play_pcm` call site for same-tick raise). Both
/// stay OFF except while playback is in flight — power + pop discipline.
///
/// Edge-triggered on [`AMP_REQUEST`]:
/// - raise: codec `unmute()` FIRST, then amp HIGH. The I2S data line is
///   already actively driven (the ring streams zeros), so the amp powers
///   into real silence — never the floating-line white-noise of the boot
///   hazard — and ≥ one ring of driven silence follows before the feeder
///   releases the first sample.
/// - drop: amp LOW first, then full codec `shutdown()` (back to the boot
///   state, ~0 mA) — reverse order pops.
pub fn service_amp<I: I2c>(amp: &mut Output<'static>, codec: &mut Es8311<I>) {
    let want = AMP_REQUEST.load(Ordering::Relaxed);
    let have = AMP_READY.load(Ordering::Relaxed);
    if want && !have {
        let _ = codec.unmute();
        // unmute() writes its own ~80% default to 0x32; override with the
        // persisted master volume (#59) so every clip honors the stored level
        // (and stays silent when muted).
        let _ = codec.set_volume(MASTER_VOL_REG.load(Ordering::Relaxed));
        amp.set_high();
        AMP_READY.store(true, Ordering::Relaxed);
    } else if !want && have {
        amp.set_low();
        let _ = codec.shutdown();
        AMP_READY.store(false, Ordering::Relaxed);
    }
}

/// Sample source for the clock task's ring top-up: the currently-playing clip
/// chunk, else silence. Owned by `silent_clock_task`; single-threaded
/// (cooperative executor), so `fill_stereo` never races `play_pcm`.
pub struct PlaybackFeeder {
    /// Mono chunk currently being expanded into the ring.
    current: Option<PcmChunk>,
    /// Mono byte offset into `current`.
    offset: usize,
    /// Remaining post-sample silence (STEREO bytes) before the session ends.
    /// Re-armed to [`TAIL_STEREO_BYTES`] by every real sample written.
    tail: usize,
    /// Session start — anchors the [`AMP_WAIT_MS`] failsafe.
    started: Instant,
}

impl Default for PlaybackFeeder {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackFeeder {
    pub fn new() -> Self {
        Self { current: None, offset: 0, tail: 0, started: Instant::now() }
    }

    /// Open a playback session with the chunk that woke the clock task.
    pub fn begin(&mut self, first: PcmChunk) {
        self.current = Some(first);
        self.offset = 0;
        self.tail = 0;
        self.started = Instant::now();
    }

    /// Session finished: clip drained AND the tail (ring scrub + DAC flush)
    /// fully pushed. The ring content is all-zero again at this point.
    pub fn is_idle(&self) -> bool {
        self.current.is_none() && self.tail == 0
    }

    /// Abandon the session (transfer error → re-arm): drop the in-flight clip
    /// and everything queued, release the mic + amp immediately. The ring may
    /// briefly replay stale clip bytes after the re-arm (rare; amp drops on
    /// the next main tick) — accepted for a path that only fires when the
    /// executor stalled longer than the whole ring.
    pub fn abort(&mut self) {
        self.current = None;
        self.tail = 0;
        while PLAYBACK.try_receive().is_ok() {}
        PLAYBACK_ACTIVE.store(false, Ordering::Relaxed);
        AMP_REQUEST.store(false, Ordering::Relaxed);
    }

    /// The feeder releases real samples only once the amp is up ([`AMP_READY`])
    /// — or after [`AMP_WAIT_MS`], the drain-anyway failsafe (muted DAC).
    fn gate_open(&self) -> bool {
        AMP_READY.load(Ordering::Relaxed)
            || Instant::now() - self.started >= embassy_time::Duration::from_millis(AMP_WAIT_MS)
    }

    /// Produce the next `out.len()` STEREO ring bytes: clip samples while a
    /// chunk is staged (mono → stereo via mic-dsp), silence otherwise. Clears
    /// the playback/amp flags when the session completes. `out.len()` must be
    /// a multiple of 4 (whole stereo frames — the caller aligns).
    pub fn fill_stereo(&mut self, out: &mut [u8]) {
        let mut i = 0;
        while i + 4 <= out.len() {
            // Stage the next chunk (only past the amp gate, so no audio is
            // spent into a muted DAC while the main loop raises the amp).
            if self.current.is_none() && self.gate_open() {
                if let Ok(c) = PLAYBACK.try_receive() {
                    self.current = Some(c);
                    self.offset = 0;
                }
            }
            if let Some(c) = &self.current {
                if self.gate_open() {
                    let n = mic_dsp::mono_to_stereo_le(&c[self.offset..], &mut out[i..]);
                    if n > 0 {
                        self.offset += n / 2;
                        i += n;
                        self.tail = TAIL_STEREO_BYTES; // re-arm the release pad
                    }
                    if self.offset + 2 > c.len() {
                        self.current = None; // chunk drained; loop restages
                    }
                    continue;
                }
                // Amp not up yet: hold the clip, emit silence below.
            }
            // Silence for the REST of the buffer: producers run on the same
            // single-threaded executor, so nothing can arrive mid-call.
            let pad = (out.len() - i) & !3;
            out[i..i + pad].fill(0);
            i += pad;
            if self.tail > 0 && self.current.is_none() {
                self.tail = self.tail.saturating_sub(pad);
                if self.tail == 0 {
                    // Tail fully pushed → every ring byte re-zeroed + DAC
                    // flushed. Release the mic and schedule the amp drop.
                    PLAYBACK_ACTIVE.store(false, Ordering::Relaxed);
                    AMP_REQUEST.store(false, Ordering::Relaxed);
                }
            }
        }
        // Guard any (never-expected) trailing non-frame bytes.
        out[i..].fill(0);
    }
}
