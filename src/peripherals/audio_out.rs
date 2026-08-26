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

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Instant;
use embedded_hal::i2c::I2c;
use esp_hal::gpio::Output;
use heapless::Vec;

use crate::board;
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
///
/// # Growing this does NOT buy runway across a blocking paint — read before trying
///
/// The obvious fix, when a full-frame render starves the speaker, is to deepen
/// this queue. **It cannot help, and the reason is worth stating once here rather
/// than being rediscovered.** This channel is drained by `silent_clock_task` —
/// an Embassy task on the *same single-threaded executor* that a paint blocks. A
/// paint therefore stops the drain as well as the producer, so the only buffer
/// the DMA can still consume from is [`TX_RING_LEN`] (**48 ms**), no matter how
/// many chunks are sitting here.
///
/// So: 128 ms is the *backpressure* window for a streamed source, and 48 ms is
/// the *underrun* window for anything that blocks the executor. They are
/// different numbers answering different questions, and conflating them sends you
/// after the wrong constant. Growing the ring is the lever that would work, and
/// `TX_RING_LEN`'s own docs record that being tried twice and reverted (16 KB in
/// `.bss` trips the stack floor; 16 KB on the heap froze both watches).
///
/// The working approach is to keep the blocking work *shorter than 48 ms* —
/// partial rendering, small fixed dirty regions — or to move the repaint out of
/// the audio window entirely (`ping_visual_due` in `main.rs`). See
/// `net::story_play::PAINT_BUDGET_MS`, which enforces the former with a measured,
/// self-disabling gate.
pub const PLAY_QUEUE_DEPTH: usize = 8;

/// The TX clock ring in STEREO bytes: 3 descriptors × `STEREO_CHUNK` = 3072 B
/// ≈ 48 ms @ 16 kHz stereo s16le (64,000 B/s). Same 3-descriptor circular
/// geometry as the mic RX ring (whole-descriptor `available()` growth, no
/// partial windows). It lives in a `StaticCell` — `TX_RING` in `main.rs` — so
/// it is `.bss`, NOT the heap.
///
/// # Do not grow this ring
///
/// 48 ms is short enough that a long executor stall starves the feeder: the #58
/// ping's FULL-FRAME repaint (~200 ms measured) lands inside the 700 ms melody,
/// so the chime came out intermittently — JP: "I did hear a ring-like thing once
/// or twice but not reliably". The 12 ms confirm tick never missed, because it
/// finishes before the stall begins.
///
/// Enlarging the ring to 16 descriptors (16,384 B ≈ 256 ms) is the obvious fix.
/// It was tried BOTH ways and reverted BOTH times (987be4e) — a measured dead
/// end, not an untried idea:
///
/// - **In `.bss`** — the stack gap is `_stack_start - _bss_end`, so `.bss`
///   growth comes straight out of the stack. 16 KB trips the 70 KB
///   `STACK_FLOOR` boot assert in `main.rs`; overriding that assert is the worse
///   failure, not the escape hatch (#65: a stack overflow silently smashed the
///   WiFi blob globals, and the crash looked layout-sensitive rather than
///   stack-related).
/// - **On the heap** — starved the launcher into an allocation failure and froze
///   both watches. The main pool has no room to give (#75).
///
/// RAM is exhausted, so the REPAINT moved instead of the buffer: the ping's
/// visual pulse is deferred until the clip has played out (`ping_visual_due` in
/// `main.rs`). That is what made the chime reliable — not a bigger ring.
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

/// Millisecond stamp of when [`AMP_READY`] last went true.
///
/// `AMP_READY` records that the GPIO was driven and the codec unmuted — it is a
/// FLAG, not physical readiness. The ES8311 leaving shutdown and the speaker amp
/// settling take real time, so releasing samples the instant the flag flips
/// plays the head of a clip into an amp that is not producing output yet.
///
/// Symptom this fixes: the FIRST ping chime after the screen had gone idle was
/// inaudible while the log reported a full `11200/11200 B` streamed, and the
/// SECOND ping — with the codec recently cycled and settling faster — was clearly
/// audible. Byte counts prove the plumbing, never the sound.
/// (u32 — riscv32 has no 64-bit atomics. Wraps after ~49 days of uptime; the
/// worst case is one clip released early, once, which is harmless.)
static AMP_READY_MS: AtomicU32 = AtomicU32::new(0);

/// Driven-silence pre-roll after the amp is raised, before real samples are
/// released. The ring is already streaming zeros, so this is silence into a
/// settling amp — the same pop-insurance idea as the existing one-ring lead-in,
/// just long enough for a COLD codec (full shutdown -> unmute).
///
/// Costs latency only on the first sound after idle: back-to-back SFX (touch
/// ticks) find the amp already up and settled, so they are unaffected.
const AMP_SETTLE_MS: u64 = 120;

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

/// Queue ONE chunk, awaiting queue space — the backpressure seam for a source
/// that is arbitrarily long and arrives over time (TTS read-aloud, #read-aloud;
/// the HA-speaker follow-up will use it too).
///
/// Unlike [`play_pcm`] (synchronous, truncates the remainder once the 128 ms
/// queue fills) this **cannot truncate**: it yields until the clock task's
/// feeder has drained a slot. A TTS utterance is 2–10 s — 120–650 chunks — so
/// truncation is not an edge case there, it is the default outcome.
///
/// Returns `false` if the queue did not drain within [`PUSH_TIMEOUT`]. The
/// caller MUST abandon the clip on `false`: it means the clock task is wedged,
/// and awaiting forever would park the main loop (and with it the UI, the
/// touch scan, and `service_amp`) permanently.
///
/// # Why the gates are raised BEFORE the send, unlike `play_pcm`
///
/// `play_pcm` can raise `PLAYBACK_ACTIVE`/`AMP_REQUEST` after its enqueue loop
/// because it never yields — the feeder cannot run in between. This function
/// **awaits**, so the clock task can wake, take the chunk and start clocking it
/// out before we ever get to the store. That would open a playback session with
/// the mic gate still open and the amp still unrequested. So the gates go up
/// first, and only for a non-empty clip (raising them for an empty one would
/// suppress the mic forever, since nothing would ever arrive to release them).
pub async fn push_chunk(pcm: &[u8]) -> bool {
    if pcm.is_empty() {
        return true;
    }
    PLAYBACK_ACTIVE.store(true, Ordering::Relaxed);
    AMP_REQUEST.store(true, Ordering::Relaxed);
    for chunk in pcm.chunks(PLAY_CHUNK) {
        let Ok(v) = PcmChunk::from_slice(chunk) else {
            return true; // chunks() guarantees <= PLAY_CHUNK; unreachable
        };
        if embassy_time::with_timeout(PUSH_TIMEOUT, PLAYBACK.send(v)).await.is_err() {
            return false;
        }
    }
    true
}

/// How long [`push_chunk`] waits for a queue slot before declaring the consumer
/// wedged. The queue drains in 128 ms when healthy, so this is ~16× headroom —
/// long enough to ride out an executor hiccup, short enough that a genuinely
/// stuck clock task cannot hang the main loop.
const PUSH_TIMEOUT: embassy_time::Duration = embassy_time::Duration::from_secs(2);

/// Latched when [`PlaybackFeeder::abort`] tears a session down (DMA re-arm).
///
/// A streamed producer needs this: `abort` drains the queue and drops the
/// session, so a producer that kept pushing would pour chunks into a channel
/// nobody is draining *for its session* and narrate to a dead speaker. Cleared
/// by [`begin_stream`] at the start of each utterance.
static STREAM_ABORTED: AtomicBool = AtomicBool::new(false);

/// "A queue-fed stream is mid-flight" — set by [`begin_stream`], cleared by
/// [`end_stream`].
///
/// Distinguishes a STREAM (audio arriving over time; the bounded queue is the
/// ONLY copy) from a static long clip (the whole thing sits in [`LONG_CLIP`]
/// with a cursor, so the queue is redundant). [`PlaybackFeeder::resync`] must
/// treat them oppositely: draining the queue is free for the chime and
/// destroys undelivered audio for a stream.
static STREAM_LIVE: AtomicBool = AtomicBool::new(false);

/// Arm a streamed utterance: clears the abort latch. Call once before the first
/// [`push_chunk`].
pub fn begin_stream() {
    STREAM_ABORTED.store(false, Ordering::Relaxed);
    STREAM_LIVE.store(true, Ordering::Relaxed);
}

/// Did the feeder abort the session we were streaming into?
pub fn stream_aborted() -> bool {
    STREAM_ABORTED.load(Ordering::Relaxed)
}

/// Drop every queued chunk without touching the amp or the half-duplex gate.
///
/// The interruption path (tap-to-stop during TTS): the feeder finishes its staged
/// chunk and pads silence, which scrubs the ring to all-zero and clears both
/// flags on its own. That is the POP-FREE path and reuses proven machinery.
/// Hard-cutting the amp mid-sample is exactly the "reverse order pops" failure
/// [`service_amp`] documents, and would put a click on every interruption; ~64 ms
/// of trailing silence is the better trade.
///
/// Only pointer/index work runs inside the channel's critical section — at most
/// [`PLAY_QUEUE_DEPTH`] pops, never a per-sample loop (#58's lesson: a 128-
/// iteration copy inside a critical section starved the I2S DMA into a `Late`).
///
/// Restored after a merge dropped it: resolving an `audio_out.rs` conflict with
/// `--ours` kept my `STREAM_LIVE` work but discarded this, breaking
/// `--features tts` compilation. Losing a function that only a feature-gated
/// build references is exactly what a default-off feature hides.
pub fn drain_queue() {
    while PLAYBACK.try_receive().is_ok() {}
}

/// A long `&'static` clip streamed straight off its own buffer (the #58 ping
/// chime): registered once at boot, played by arming `pos`/`playing`.
struct LongClip {
    pcm: Option<&'static [u8]>,
    /// Mono bytes already handed to the feeder.
    pos: usize,
    playing: bool,
}

/// The long-clip source the feeder falls back to when the SFX queue is empty.
///
/// # Why not the queue, and why not a task
///
/// The chime is 480 ms (30 × [`PLAY_CHUNK`]) but the queue holds 128 ms (8), and
/// [`play_pcm`] is synchronous — it cannot yield to let the clock task drain —
/// so 73 % of the melody was rejected and the ping sounded silent. The first fix
/// attempt streamed it from a dedicated Embassy task; that task made the watch
/// panic **100 %** of the time under the `debug-console` build (`Instruction
/// access fault mepc=0x2` inside `esp_rtos::task::task_wrapper` — one task too
/// many for the RTOS's per-task resources). Streaming from the feeder that
/// already exists needs no task, never blocks a caller, and can't truncate.
static LONG_CLIP: embassy_sync::blocking_mutex::Mutex<
    CriticalSectionRawMutex,
    core::cell::RefCell<LongClip>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::RefCell::new(LongClip {
    pcm: None,
    pos: 0,
    playing: false,
}));

/// Register the ping-chime PCM (heap-leaked `&'static` built at boot). Must be
/// called before [`play_chime`] can do anything.
pub fn register_chime(pcm: &'static [u8]) {
    LONG_CLIP.lock(|c| c.borrow_mut().pcm = Some(pcm));
}

/// Fire the ping chime: stream the FULL melody off its static buffer.
///
/// Non-blocking, safe from any task (the ping RX path and the debug console both
/// use it). Re-arms from the start if one is already playing — a ping is a
/// can't-miss event, and the RX site already dedups/gates repeats.
///
/// # The first chunk goes through the QUEUE on purpose
///
/// `silent_clock_task` idles in `select(next_clip(), rearm_requested())`, i.e.
/// parked on `PLAYBACK.receive()`. It therefore only opens a playback session
/// when a chunk arrives **through the channel** — arming [`LONG_CLIP`] alone
/// left the task asleep and the chime SILENT (no `fill_stereo` call ever
/// happened). So: hand chunk 0 to the queue to wake it and open the session,
/// and let the feeder pull chunks 1..n straight off the static buffer.
pub fn play_chime() -> (bool, usize, usize) {
    if !CHIME_ENABLED {
        return (false, 0, 0); // #65 gate — see CHIME_ENABLED
    }
    // Arm at the start, then pull chunk 0 through `next_long_chunk` like every
    // other chunk. It must NOT be sliced straight out of the buffer: the clip is
    // stored at 8 kHz and the ring runs at 16 kHz, so a raw slice plays the first
    // 32 ms at DOUBLE SPEED and advances the cursor twice as far as it should.
    let armed = LONG_CLIP.lock(|c| {
        let mut c = c.borrow_mut();
        if c.pcm.is_none() {
            return false;
        }
        c.pos = 0;
        c.playing = true;
        true
    });
    if !armed {
        return (false, 0, 1); // 1 = no pcm registered
    }
    CHIME_BYTES.store(0, Ordering::Relaxed);
    CHIME_DONE.store(false, Ordering::Relaxed);
    let Some(first) = next_long_chunk() else {
        stop_long_clip();
        return (false, 0, 2); // 2 = next_long_chunk yielded nothing
    };
    let n = first.len();
    if PLAYBACK.try_send(first).is_ok() {
        // Same ordering as play_pcm: suppress the mic and request the amp before
        // the first sample can reach the speaker.
        PLAYBACK_ACTIVE.store(true, Ordering::Relaxed);
        AMP_REQUEST.store(true, Ordering::Relaxed);
        (true, n, 0)
    } else {
        stop_long_clip(); // queue full — don't leave a half-armed clip behind
        (false, n, 3) // 3 = PLAYBACK queue full
    }
}

/// Raise the amp WITHOUT queueing audio, so it can power up and settle while the
/// caller does something slow (the #58 ping's full-frame repaint).
///
/// Why this exists: a 700 ms clip cannot survive a ~200 ms executor stall with a
/// 48 ms DMA ring — the ring underruns and the melody comes out intermittently
/// ("heard a ring-like thing once or twice but not reliably"). The 12 ms tick
/// always works because it finishes before the stall. So the ping path raises the
/// amp first, lets the repaint happen while the amp settles into driven silence,
/// and only THEN releases the clip into a quiet executor.
///
/// Does NOT set `PLAYBACK_ACTIVE`: no samples are queued yet, so there is nothing
/// for the mic to overhear, and leaving capture suppressed for the whole repaint
/// would be a needless half-duplex window.
pub fn prearm_amp() {
    AMP_REQUEST.store(true, Ordering::Relaxed);
}

/// Is the ping chime allowed to reach the speaker? **Currently `false` (#65).**
///
/// The chime path itself is correct and was measured on-glass streaming the full
/// clip (22400/22400 B). But enabling it flipped **release** builds to a 100 %
/// boot panic at 2.7 s inside `ppRxFragmentProc` — the WiFi blob's RX-fragment
/// path, i.e. #61's crash site. Measured on the same watch minutes apart:
/// chime off 0/4 crash, chime on 5/5 crash.
///
/// None of the added code can be running at 2.7 s (no ping has occurred yet).
/// The whole delta is ~8 bytes of `.bss` plus one atomic swap per main-loop tick,
/// so this is the layout-sensitive latent corruption tracked in #65 — not a fault
/// in this module. `debug-console` builds never crash, which is exactly why every
/// automated check passed while release builds died.
///
/// Gated behind one flag instead of reverted: flip to `true` when #65 lands and
/// the full path (streaming + the #58b voicing) is live again with no other work.
pub const CHIME_ENABLED: bool = true;

/// Registered chime length in mono bytes (0 before `register_chime`).
fn chime_len() -> usize {
    LONG_CLIP.lock(|c| c.borrow().pcm.map_or(0, |p| p.len()))
}

/// Mono bytes of the current chime handed to the ring so far, and a
/// completion latch — telemetry so playback is provable from the serial log
/// instead of only by ear (the first rework shipped silent and the console's
/// "ok chime" ack proved nothing).
static CHIME_BYTES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static CHIME_DONE: AtomicBool = AtomicBool::new(false);

/// Take the "chime finished" latch, returning `(streamed, total)` mono bytes.
/// The main loop logs it once per chime.
pub fn chime_done_take() -> Option<(usize, usize)> {
    if !CHIME_ENABLED {
        return None; // no chime can be in flight; keeps the per-tick cost at zero
    }
    if CHIME_DONE.swap(false, Ordering::Relaxed) {
        Some((CHIME_BYTES.load(Ordering::Relaxed), chime_len()))
    } else {
        None
    }
}

/// Next chunk of the streaming long clip, or `None` when idle/finished.
/// Advances `pos`; clears `playing` on the last chunk.
fn next_long_chunk() -> Option<PcmChunk> {
    // CLAIM a source range under the lock; BUILD the samples outside it.
    //
    // The lock is a critical section (interrupts OFF). Expanding 8 kHz -> 16 kHz
    // is a 128-iteration copy loop, and running that with interrupts disabled —
    // every ~16 ms, from the feeder — starved the I2S DMA long enough to trip a
    // `Late` error. `top_up` then failed, `silent_clock_task` called
    // `feeder.abort()`, and abort drops the clip WITHOUT setting CHIME_DONE:
    // the chime went silent with no completion line and no panic. Keep the
    // critical section to pointer/index bookkeeping only.
    let (pcm, start, end) = LONG_CLIP.lock(|c| {
        let mut c = c.borrow_mut();
        if !c.playing {
            return None;
        }
        let pcm = c.pcm?;
        if c.pos >= pcm.len() {
            c.playing = false;
            CHIME_DONE.store(true, Ordering::Relaxed);
            return None;
        }
        let start = c.pos;
        // Half a chunk of 8 kHz source -> a full 16 kHz chunk out.
        let end = (start + PLAY_CHUNK / 2).min(pcm.len());
        c.pos = end;
        if end >= pcm.len() {
            c.playing = false;
            CHIME_DONE.store(true, Ordering::Relaxed);
        }
        Some((pcm, start, end))
    })?;

    // Zero-order hold 2x. Fine here: pure sines <= 1046 Hz, so the images land
    // far above anything the watch speaker reproduces.
    let mut chunk = PcmChunk::new();
    for frame in pcm[start..end].chunks_exact(2) {
        // Capacity is exact by construction: (PLAY_CHUNK/2) bytes in -> PLAY_CHUNK out.
        let _ = chunk.extend_from_slice(frame);
        let _ = chunk.extend_from_slice(frame);
    }
    if chunk.is_empty() {
        return None;
    }
    CHIME_BYTES.fetch_add(end - start, Ordering::Relaxed);
    Some(chunk)
}

/// Is a long clip still mid-stream? Used by the DMA-error recovery path to
/// decide between resuming the melody and giving up on it.
fn long_clip_live() -> bool {
    LONG_CLIP.lock(|c| {
        let c = c.borrow();
        c.playing && c.pcm.is_some_and(|p| c.pos < p.len())
    })
}

/// Is a queue-fed stream mid-flight?
pub fn stream_live() -> bool {
    STREAM_LIVE.load(Ordering::Relaxed)
}

/// Mark a queue-fed stream finished (producer done pushing). Idempotent.
pub fn end_stream() {
    STREAM_LIVE.store(false, Ordering::Relaxed);
}

/// A chunk to re-open a playback session with after a DMA `Late`.
///
/// Static long clip first (its cursor is authoritative), else whatever the queue
/// still holds — that second case is what keeps a network-fed stream alive across
/// a stall. `None` simply idles; `silent_clock_task` then waits on `next_clip()`
/// and the next arriving frame reopens the session.
pub fn next_resume_chunk() -> Option<PcmChunk> {
    next_long_chunk().or_else(|| PLAYBACK.try_receive().ok())
}

/// Abandon any streaming long clip (transfer re-arm / abort path).
fn stop_long_clip() {
    LONG_CLIP.lock(|c| {
        let mut c = c.borrow_mut();
        c.playing = false;
        c.pos = 0;
    });
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
        amp_drive(amp, true);
        AMP_READY_MS.store(Instant::now().as_millis() as u32, Ordering::Relaxed);
        AMP_READY.store(true, Ordering::Relaxed);
    } else if !want && have {
        amp_drive(amp, false);
        let _ = codec.shutdown();
        AMP_READY.store(false, Ordering::Relaxed);
    }
}

/// Drive the speaker-amp enable GPIO respecting the board's polarity.
///
/// The C6's SGM's enable is active-HIGH (GPIO6); the S3-CYD's SC8002B is
/// active-LOW (GPIO1, `board::AMP_ACTIVE_LOW`). `on ^ AMP_ACTIVE_LOW` picks the
/// level: active-high ON = high, active-low ON = low — and the boot-time
/// `Output::new` level in main.rs mirrors this so the amp is released before
/// the codec is muted (white-noise discipline #23).
#[inline]
fn amp_drive(amp: &mut Output<'static>, on: bool) {
    if on ^ board::AMP_ACTIVE_LOW {
        amp.set_high();
    } else {
        amp.set_low();
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
    /// A DMA `Late` error killed the transfer, but a STREAMING long clip should
    /// survive it: drop the in-flight ring state and let the caller re-arm, then
    /// keep feeding from wherever [`LONG_CLIP`] left off.
    ///
    /// This exists because [`abort`] is too destructive for the case that
    /// actually happens on this watch. A received ping wakes the screen and
    /// forces a FULL-FRAME repaint (~200 ms — measured), which is far longer than
    /// the 48 ms DMA ring, so `top_up` reliably hits `Late` on the ping path.
    /// `abort` then threw the melody away and cleared the completion latch's
    /// clip, so a ping produced NO sound and NO log line while the console's
    /// identical `play_chime()` worked perfectly (nothing repaints after it).
    ///
    /// Returns true if a long clip is still live and worth resuming.
    pub fn resync(&mut self) -> bool {
        self.current = None;
        self.tail = 0;
        // For a STATIC long clip the queue is redundant (LONG_CLIP holds the whole
        // melody with a cursor), so stale ring data is dropped. For a QUEUE-FED
        // stream the queue is the only copy of audio that arrived over the
        // network — draining it throws away speech that was never played. This
        // distinction is the difference between the chime resuming and walkie-talkie
        // RX being silent: `Late` fires on any >48 ms stall, the watchface repaints
        // full-frame (~200 ms) at 1 Hz, so an unconditional drain kills reception
        // roughly once per second.
        if !stream_live() {
            while PLAYBACK.try_receive().is_ok() {}
        }
        let live = long_clip_live() || stream_live();
        if live {
            // Keep the mic suppressed and the amp up: the melody continues on the
            // fresh transfer. Re-stamp so the amp gate re-arms cleanly.
            self.started = Instant::now();
        }
        live
    }

    pub fn abort(&mut self) {
        self.current = None;
        self.tail = 0;
        while PLAYBACK.try_receive().is_ok() {}
        stop_long_clip(); // don't resume a half-played melody after the re-arm
        // Tell a streamed producer (TTS) its session died, so it stops pushing
        // into a channel that is no longer feeding a live session — the same
        // reason `stop_long_clip` exists for the chime.
        STREAM_ABORTED.store(true, Ordering::Relaxed);
        PLAYBACK_ACTIVE.store(false, Ordering::Relaxed);
        AMP_REQUEST.store(false, Ordering::Relaxed);
    }

    /// The feeder releases real samples only once the amp is up ([`AMP_READY`])
    /// — or after [`AMP_WAIT_MS`], the drain-anyway failsafe (muted DAC).
    fn gate_open(&self) -> bool {
        // The amp must be up AND settled: see AMP_READY_MS. The failsafe still
        // drains the queue after AMP_WAIT_MS so a missed raise can never wedge
        // the mic-suppression flag.
        let settled = AMP_READY.load(Ordering::Relaxed)
            && (Instant::now().as_millis() as u32)
                .wrapping_sub(AMP_READY_MS.load(Ordering::Relaxed)) as u64
                >= AMP_SETTLE_MS;
        settled
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
            // Queue first, then the streaming long clip (#58) — a queued SFX is
            // always short and shouldn't wait behind 480 ms of melody.
            if self.current.is_none() && self.gate_open() {
                if let Ok(c) = PLAYBACK.try_receive() {
                    self.current = Some(c);
                    self.offset = 0;
                } else if let Some(c) = next_long_chunk() {
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
