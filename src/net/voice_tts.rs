//! Text-to-speech playback: send notification text to the LAN bridge, stream
//! the returned PCM straight into the speaker.
//!
//! The exact mirror of [`crate::net::voice_stt`], reversed in direction. Same
//! bridge (`watch_bridge.py`, dotted-quad IPv4, plain HTTP, no TLS, no DNS),
//! same host and port, same `&'static str` error style. The bridge holds the
//! Azure key and does the HTTPS hop; **the watch never sees a secret.**
//!
//!   POST /tts  body: `{"text":"..."}`  -> 200 raw mono 16 kHz s16le PCM
//!
//! Design spec: `docs/superpowers/specs/2026-07-27-tts-notify-readaloud-design.md`.
//!
//! # Why there is no decode step
//!
//! Measured live against Azure (2026-07-27): the configured DragonHD voice
//! honours `X-Microsoft-OutputFormat: raw-16khz-16bit-mono-pcm` with a 200. That
//! is byte-for-byte the format [`audio_out`] already consumes, so bytes come off
//! the socket and go into the playback queue untouched — no MP3, no Opus, no
//! resampler, no codec state. Measured cost of the whole feature: **+416 B of
//! `.bss`** (rustc overlaps this frame with `stream_utterance`'s — they are
//! disjoint branches of the same main-loop generator and never run at once).
//!
//! # Why the BRIDGE buffers the whole utterance, not us
//!
//! Azure's own response stream stalls **255–782 ms** between chunks (measured
//! across short/typical/long utterances). The watch can bridge a **64 ms** gap
//! ([`audio_out`]'s `TAIL_STEREO_BYTES`) behind a **128 ms** queue. A naive
//! relay would therefore underrun on *every* utterance: the feeder's tail
//! expires, `PLAYBACK_ACTIVE`/`AMP_REQUEST` drop, the amp powers down, and the
//! next chunk opens a whole new session — chopped speech with a pop per seam.
//!
//! So the bridge fully synthesizes first and only then streams, at LAN line
//! rate. All the jitter lands on the host with gigabytes of RAM instead of the
//! one with 186 KB. It costs little: time-to-first-byte from Azure is ~1.2 s
//! regardless of length, and full synthesis finishes *faster than realtime* for
//! anything past ~2 s of audio.
//!
//! # Flow control
//!
//! There is no rate math here and no timer pacing. [`audio_out::push_chunk`]
//! awaits queue space, so we read from TCP exactly as fast as the speaker
//! consumes; the receive window then applies the same backpressure upstream to
//! the bridge for free. **The speaker's own clock paces the entire pipeline.**
//!
//! # Two hazards this module is shaped around
//!
//! 1. **The amp gate.** `PlaybackFeeder::gate_open()` withholds every sample
//!    until `AMP_READY`, which only [`audio_out::service_amp`] sets, and that
//!    only runs from the main loop. Since [`speak_text`] *parks* the main loop
//!    for the whole utterance (exactly as the STT push-to-talk flow does), it
//!    must pump `service_amp` itself — hence the `amp`/`codec` parameters. Skip
//!    that and every chunk stalls behind the 1000 ms `AMP_WAIT_MS` failsafe and
//!    drains into a **muted DAC**: audio "succeeds" in the logs and is silent in
//!    the room.
//! 2. **No new task.** Adding one Embassy task previously produced a 100 %
//!    reproducible `Instruction access fault mepc=0x2`. This runs on the
//!    existing main loop; nothing is spawned.
//! 3. **It must be interruptible.** Because the main loop is parked, a long
//!    utterance means seconds of unresponsive UI — which reads to a user as the
//!    watch having frozen (a symptom JP has been chasing for real). So
//!    `should_stop` is polled every [`CANCEL_POLL_CHUNKS`] chunks and a tap
//!    stops playback within ~64 ms. Interruptibility is a correctness
//!    requirement here, not a nicety.
//!
//! # No per-sample work inside a critical section
//!
//! #58 cost two "verified" rounds to a 128-iteration upsample loop left inside
//! `LONG_CLIP.lock()`: with interrupts off every ~16 ms it starved the I2S DMA
//! into a `Late` error, `silent_clock_task` called `feeder.abort()`, and abort
//! drops the clip WITHOUT setting the completion latch — silent, no log, no
//! panic, indistinguishable from success at every observation point. This
//! module therefore takes **no lock at all**: `push_chunk` hands a slice to a
//! channel and every copy happens outside any critical section. Keep it so.
//!
//! # Do not render while this runs
//!
//! Painting the Slint scene blocks the single-threaded executor for tens of ms
//! and starves the audio DMA (the STT path sacrificed its live level bar for
//! this reason). With a 128 ms playback queue the same rule binds here: set the
//! UI state before the call and after it, never during.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_net::{tcp::TcpSocket, Ipv4Address, Stack};
use embassy_time::{with_timeout, Duration};
use embedded_hal::i2c::I2c;
use esp_hal::gpio::Output;

use crate::peripherals::audio::Es8311;
use crate::peripherals::audio_out;

/// Errors are `&'static str` for parity with [`crate::net::voice_stt`].
pub type Error = &'static str;

/// The chunk size the protocol crate does its byte↔ms math against MUST equal
/// the one the playback queue actually uses. They live in different crates
/// (`tts-proto` is host-testable and cannot see `audio_out`), so nothing but
/// this assert stops them drifting — and a silent drift would mis-time every
/// buffer calculation in the streaming loop.
const _: () = assert!(tts_proto::PLAY_CHUNK == audio_out::PLAY_CHUNK);
/// Likewise the sample rate: `audio_out` clocks the ring at 16 kHz and the
/// bridge is asked for `raw-16khz-...`; if either moved, the PCM would play at
/// the wrong speed rather than fail loudly.
const _: () = assert!(tts_proto::SAMPLE_RATE as u32 == audio_out::PLAY_SAMPLE_RATE);

/// The TTS bridge lives in the same process as the STT bridge — one source of
/// truth for the address so they can never drift apart.
pub use crate::net::voice_stt::{default_bridge_ip, BRIDGE_PORT};

/// How long to wait for the response head. The bridge synthesizes the WHOLE
/// utterance before replying (see module docs), so this covers Azure's full
/// round-trip: measured 1.9 s (short) to 5.6 s (long, 10 s of audio), plus
/// generous slack for a cold HD-voice warm-up.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-read timeout once the body is flowing. The bridge already holds every
/// byte in memory at this point, so a stall this long means it died — bail out
/// rather than park the main loop forever.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Socket idle timeout for CONNECT + REQUEST only. Cleared before the response
/// wait so it can't pre-empt [`HEAD_TIMEOUT`] while Azure is still synthesizing
/// (the same trap `voice_stt` documents: a slow-but-valid round trip would
/// otherwise idle-abort and fail as "socket read failed").
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);

/// Hard cap on one utterance: 30 s of audio. A malformed notification (or a
/// misbehaving bridge) must not be able to hold the amp up indefinitely.
const MAX_UTTERANCE_BYTES: usize = 30 * tts_proto::BYTES_PER_SEC;

/// Response-head scratch. Big enough for the bridge's head (~150 B) with the
/// remainder catching the first body bytes that arrive in the same segment.
const HEAD_BUF: usize = 512;

/// How many chunks between `should_stop()` polls.
///
/// The poll reads the touch controller over I2C, and the codec shares that bus,
/// so polling every 16 ms chunk would triple the bus traffic for no benefit.
/// Every 4th chunk is a **64 ms** worst-case reaction time — well inside the
/// ~100 ms where a tap still feels instant, and cheap.
const CANCEL_POLL_CHUNKS: u8 = 4;

/// Cancellation latch: raise to stop the utterance in flight (screen off, app
/// change, a newer notification). An atomic rather than a parameter so any code
/// path can stop speech without threading a borrow — same idiom as
/// `mic_capture::RECORDING` and `audio_out::PLAYBACK_ACTIVE`.
///
/// A *tap* to interrupt comes through the `should_stop` callback instead, since
/// that needs the caller's touch driver.
pub static SPEAK_CANCEL: AtomicBool = AtomicBool::new(false);

/// Request that any in-flight utterance stop at the next chunk boundary.
pub fn cancel() {
    SPEAK_CANCEL.store(true, Ordering::Relaxed);
}

/// Why an utterance stopped — richer than a bare byte count so the caller can
/// distinguish "spoke it all" from "user walked away" without guessing.
///
/// No `Debug` derive on purpose: `{:?}` would pull a `core::fmt` instantiation
/// into a binary that is within a few KB of its ROM ceiling. Use [`label`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Spoken {
    /// The whole utterance reached the speaker.
    Complete { bytes: usize },
    /// Stopped early via [`cancel`].
    Cancelled { bytes: usize },
    /// The playback session was torn down underneath us (DMA re-arm).
    Interrupted { bytes: usize },
}

impl Spoken {
    pub fn bytes(self) -> usize {
        match self {
            Spoken::Complete { bytes } | Spoken::Cancelled { bytes } | Spoken::Interrupted { bytes } => bytes,
        }
    }
    /// Duration actually pushed to the ring, in ms.
    pub fn duration_ms(self) -> u32 {
        tts_proto::bytes_to_ms(self.bytes())
    }
    /// Log label — the `core::fmt`-free stand-in for `{:?}`.
    pub fn label(self) -> &'static str {
        match self {
            Spoken::Complete { .. } => "complete",
            Spoken::Cancelled { .. } => "cancelled",
            Spoken::Interrupted { .. } => "interrupted",
        }
    }
}

/// Clears `STREAM_LIVE` on EVERY exit from the streaming section.
///
/// `speak_text` leaves that section by many routes — socket error, stall, cancel,
/// interrupt, `Content-Length` reached, bridge close, byte cap — and one missed
/// path latches the flag forever. That is not a harmless leak:
/// `PlaybackFeeder::resync` keys off it, so a stuck flag silently gives every
/// LATER chime and SFX stream semantics on a DMA `Late` (skip the drain, resume)
/// when a finite clip wants drain-and-abort. A Drop guard makes the pairing
/// structural rather than something to remember.
struct StreamGuard;

impl Drop for StreamGuard {
    fn drop(&mut self) {
        audio_out::end_stream();
    }
}

/// Speak `text` through the bridge, streaming the reply into the speaker.
///
/// Runs **on the main loop** and parks it for the duration (precedent: the STT
/// push-to-talk flow). `amp`/`codec` are pumped through
/// [`audio_out::service_amp`] every chunk — load-bearing, see module docs.
///
/// `should_stop` is polled every [`CANCEL_POLL_CHUNKS`] chunks and aborts the
/// utterance when it returns true — the caller wires it to "finger is down".
/// **This is not optional polish:** speaking parks the main loop for seconds,
/// so without it a long notification is indistinguishable from the watch
/// freezing. A tap turns "it's frozen" into "I stopped it".
pub async fn speak_text<I: I2c>(
    stack: Stack<'static>,
    text: &str,
    amp: &mut Output<'static>,
    codec: &mut Es8311<I>,
    should_stop: &mut dyn FnMut() -> bool,
) -> Result<Spoken, Error> {
    speak_text_to(stack, default_bridge_ip(), BRIDGE_PORT, text, amp, codec, should_stop).await
}

/// Same as [`speak_text`] but with an explicit bridge address/port.
pub async fn speak_text_to<I: I2c>(
    stack: Stack<'static>,
    addr: Ipv4Address,
    port: u16,
    text: &str,
    amp: &mut Output<'static>,
    codec: &mut Es8311<I>,
    should_stop: &mut dyn FnMut() -> bool,
) -> Result<Spoken, Error> {
    // Don't spend a ~1.2 s Azure round-trip and an amp cycle on a card with no
    // words in it.
    if !tts_proto::is_speakable(text) {
        return Err("nothing to speak");
    }
    let body = tts_proto::encode_json_request(text).ok_or("text too long")?;

    // Fresh utterance: clear a stale cancel from a previous one.
    SPEAK_CANCEL.store(false, Ordering::Relaxed);

    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(SOCKET_TIMEOUT));

    socket.connect((addr, port)).await.map_err(|_| "connect failed")?;

    // --- request -------------------------------------------------------------
    // Content-Length, not chunked: unlike the STT upload (whose length is
    // unknown until the finger lifts) the whole body is in hand already.
    //
    // Built by hand rather than with `write!`/`format!`: this binary sits a few
    // KB under its 4 MiB ROM ceiling, and a `core::fmt` instantiation (Display
    // for Ipv4Address, integer formatting) costs kilobytes. `push_u32` below is
    // ~30 bytes of code and does the whole job.
    let mut head: heapless::String<160> = heapless::String::new();
    let o = addr.octets();
    let ok = (|| -> Option<()> {
        head.push_str("POST /tts HTTP/1.1\r\nHost: ").ok()?;
        for (i, b) in o.iter().enumerate() {
            if i > 0 {
                head.push('.').ok()?;
            }
            push_u32(&mut head, *b as u32)?;
        }
        head.push(':').ok()?;
        push_u32(&mut head, port as u32)?;
        head.push_str("\r\nContent-Type: application/json\r\nContent-Length: ").ok()?;
        push_u32(&mut head, body.len() as u32)?;
        head.push_str("\r\nConnection: close\r\n\r\n").ok()?;
        Some(())
    })();
    ok.ok_or("request head overflow")?;
    write_all(&mut socket, head.as_bytes()).await?;
    write_all(&mut socket, body.as_bytes()).await?;

    // --- response head -------------------------------------------------------
    // Drop the idle timeout: HEAD_TIMEOUT is now the single authoritative
    // deadline while the bridge synthesizes (see SOCKET_TIMEOUT's docs).
    socket.set_timeout(None);
    let mut buf = [0u8; HEAD_BUF];
    let (parsed, filled) = match with_timeout(HEAD_TIMEOUT, read_head(&mut socket, &mut buf)).await {
        Ok(r) => r?,
        Err(_) => return Err("tts response timeout"),
    };
    if parsed.status != 200 {
        // A 502 carries a JSON error body; treating it as PCM would fire ~10 ms
        // of noise at the speaker.
        return Err(status_message(parsed.status));
    }
    if let Some(len) = parsed.content_length {
        if len > MAX_UTTERANCE_BYTES {
            return Err("utterance too long");
        }
    }

    // --- stream the PCM into the speaker ------------------------------------
    // Arm the abort latch AFTER the (possibly long) synthesis wait, so a stale
    // abort from an earlier clip can't kill this utterance before it starts.
    audio_out::begin_stream();
    // Pairs with begin_stream on every exit path (see StreamGuard).
    let _stream_guard = StreamGuard;
    let mut played = 0usize;
    let mut outcome = Spoken::Complete { bytes: 0 };

    // Bytes of the body that already arrived alongside the head.
    let mut poll = 0u8;
    let initial = &buf[parsed.body_offset..filled];
    if !initial.is_empty() {
        match pump(initial, &mut played, &mut poll, amp, codec, should_stop).await {
            Pump::Continue => {}
            Pump::Stop(o) => {
                stop_playback(&mut socket, amp, codec);
                return Ok(o.with_bytes(played));
            }
        }
    }

    loop {
        if let Some(len) = parsed.content_length {
            if played >= len {
                break; // whole announced body delivered
            }
        }
        if played >= MAX_UTTERANCE_BYTES {
            break;
        }
        let n = match with_timeout(BODY_READ_TIMEOUT, socket.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return Err("socket read failed"),
            Err(_) => return Err("tts stream stalled"),
        };
        if n == 0 {
            break; // bridge closed — end of body (the no-Content-Length case)
        }
        match pump(&buf[..n], &mut played, &mut poll, amp, codec, should_stop).await {
            Pump::Continue => {}
            Pump::Stop(o) => {
                outcome = o;
                // Drop what is still queued so the speaker stops promptly
                // instead of playing out the 128 ms already buffered.
                audio_out::drain_queue();
                break;
            }
        }
    }

    socket.abort();

    // Leave the amp serviced on the way out so the feeder's tail can complete
    // and drop it cleanly rather than waiting for the next main-loop tick.
    audio_out::service_amp(amp, codec);

    Ok(outcome.with_bytes(played))
}

/// Outcome of feeding one buffer to the speaker.
enum Pump {
    Continue,
    Stop(Spoken),
}

/// Feed `pcm` to the playback queue, pumping the amp and honouring cancel.
///
/// `service_amp` runs BEFORE the (awaiting) push so `AMP_READY` is already up
/// when the feeder stages the very first chunk — otherwise chunk 0 waits out the
/// `AMP_WAIT_MS` failsafe into a muted DAC.
async fn pump<I: I2c>(
    pcm: &[u8],
    played: &mut usize,
    poll: &mut u8,
    amp: &mut Output<'static>,
    codec: &mut Es8311<I>,
    should_stop: &mut dyn FnMut() -> bool,
) -> Pump {
    for chunk in pcm.chunks(tts_proto::PLAY_CHUNK) {
        audio_out::service_amp(amp, codec);

        // Tap-to-stop, rate-limited (shares the codec's I2C bus).
        *poll = poll.wrapping_add(1);
        if *poll % CANCEL_POLL_CHUNKS == 0 && should_stop() {
            return Pump::Stop(Spoken::Cancelled { bytes: *played });
        }
        if SPEAK_CANCEL.load(Ordering::Relaxed) {
            return Pump::Stop(Spoken::Cancelled { bytes: *played });
        }
        if audio_out::stream_aborted() {
            // feeder.abort() tore the session down (DMA re-arm). Keep pushing
            // and we'd pour into a channel nobody is draining for this session.
            return Pump::Stop(Spoken::Interrupted { bytes: *played });
        }
        if !audio_out::push_chunk(chunk).await {
            // Queue never drained — the clock task is wedged. Bail rather than
            // park the main loop forever.
            return Pump::Stop(Spoken::Interrupted { bytes: *played });
        }
        *played += chunk.len();
    }
    Pump::Continue
}

impl Spoken {
    /// Re-stamp the byte count once the final total is known.
    fn with_bytes(self, bytes: usize) -> Spoken {
        match self {
            Spoken::Complete { .. } => Spoken::Complete { bytes },
            Spoken::Cancelled { .. } => Spoken::Cancelled { bytes },
            Spoken::Interrupted { .. } => Spoken::Interrupted { bytes },
        }
    }
}

/// Stop early: drop the socket and everything still queued, then service the
/// amp once so the feeder's tail can begin releasing it.
///
/// The queued audio is dropped but the amp is NOT hard-cut — `drain_queue`'s
/// docs explain why (the feeder's silence pad is the pop-free path).
fn stop_playback<I: I2c>(
    socket: &mut TcpSocket<'_>,
    amp: &mut Output<'static>,
    codec: &mut Es8311<I>,
) {
    audio_out::drain_queue();
    socket.abort();
    audio_out::service_amp(amp, codec);
}

/// Read until the response head is complete; return it plus how much of `buf`
/// is filled (the tail is the first body bytes).
async fn read_head(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8; HEAD_BUF],
) -> Result<(tts_proto::ResponseHead, usize), Error> {
    let mut len = 0;
    loop {
        match tts_proto::parse_response_head(&buf[..len]) {
            tts_proto::HeadParse::Ok(h) => return Ok((h, len)),
            tts_proto::HeadParse::Malformed => return Err("malformed tts response"),
            tts_proto::HeadParse::Incomplete => {}
        }
        if len == buf.len() {
            return Err("tts response head too large");
        }
        let n = socket.read(&mut buf[len..]).await.map_err(|_| "socket read failed")?;
        if n == 0 {
            return Err("connection closed in headers");
        }
        len += n;
    }
}

/// Map a non-200 bridge status to a short, screen-sized message.
fn status_message(status: u16) -> Error {
    match status {
        400 => "bridge rejected text",
        404 => "bridge has no /tts",
        502 => "azure TTS failed",
        503 => "bridge busy",
        _ => "bridge error",
    }
}

/// Append a decimal u32 — the `core::fmt`-free stand-in for `write!("{n}")`.
fn push_u32<const N: usize>(s: &mut heapless::String<N>, mut n: u32) -> Option<()> {
    let mut digits = [0u8; 10];
    let mut i = digits.len();
    if n == 0 {
        i -= 1;
        digits[i] = b'0';
    }
    while n > 0 {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    for &d in &digits[i..] {
        s.push(d as char).ok()?;
    }
    Some(())
}

/// Write the whole slice, looping over partial writes.
async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), Error> {
    while !data.is_empty() {
        let n = socket.write(data).await.map_err(|_| "socket write failed")?;
        if n == 0 {
            return Err("socket write returned 0");
        }
        data = &data[n..];
    }
    Ok(())
}
