//! Chapter playback: `Range`-window `GET /media/{n}.pcm` straight into the
//! speaker.
//!
//! The audio half of the Story app. Structurally a copy of
//! [`crate::net::voice_tts`]'s streaming loop — deliberately, because that loop
//! encodes several hard-won lessons this one must not re-learn (see
//! [`self::amp_gate`] below). What is genuinely new here is three things:
//! **Range windowing**, **exact resume**, and a **measured paint budget**.
//!
//! Design specs:
//! * `docs/superpowers/specs/2026-07-27-tts-notify-readaloud-design.md` §6.2 (the amp gate)
//! * `~/Projects/endlesslitrpg/docs/superpowers/specs/2026-07-29-endless-litrpg-design.md` §9.4
//!
//! # Why windows instead of one long request
//!
//! A 13-minute chapter is ~25 MB. One open-ended `Range` request would work —
//! `push_chunk` awaits queue space, so the TCP receive window paces the download
//! from the speaker's own clock — right up until anything goes wrong, at which
//! point the whole chapter is lost with nothing to retry *from*. So playback
//! walks 60-second windows ([`story_proto::WINDOW_BYTES`]). A failure costs one
//! window, and resuming is just the next byte offset because
//! `byte = ms x 32` is exact by construction (design §8.1). No re-sync, no seek
//! index, no frame table.
//!
//! # Nothing is buffered
//!
//! 25 MB flows through one 512-byte staging buffer plus the existing 8-slot
//! playback channel. There is no decoder, no resampler and no cache: 16 MB of
//! flash is already committed to two 6 MB OTA slots, and 512 KB of SRAM with no
//! PSRAM has nowhere to put a chapter. Streaming is the only option, not an
//! optimisation.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_net::{tcp::TcpSocket, Ipv4Address, Stack};
use embassy_time::{with_timeout, Duration, Instant};
use embedded_hal::i2c::I2c;
use esp_hal::gpio::Output;

use crate::net::story_api::{status_message, DAEMON_PORT};
use crate::peripherals::audio::Es8311;
use crate::peripherals::audio_out;
use story_proto::{HeadParse, Method, Route};

pub type Error = &'static str;

/// Same compile-time tie as `story_api`: the protocol crate's chunk size and
/// sample rate must equal the playback queue's, or every offset is wrong.
const _: () = assert!(story_proto::PLAY_CHUNK == audio_out::PLAY_CHUNK);
const _: () = assert!(story_proto::SAMPLE_RATE == audio_out::PLAY_SAMPLE_RATE);

/// Socket idle timeout for connect + request only, cleared before the body so a
/// slow-but-valid response cannot idle-abort mid-chapter.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Response-head deadline for one window.
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-read deadline once PCM is flowing. The daemon reads from a local file, so
/// a stall this long means the window is dead — drop it and re-range.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(6);

/// Head scratch, also the PCM staging window: one [`audio_out::PLAY_CHUNK`].
const BUF: usize = 512;

/// The retry decision now lives in `story_proto::RetryBudget`, which is
/// host-tested. It was here, where `esp-hal` makes host tests impossible — and it
/// was wrong: only a fully-delivered window cleared the strike count, so a link
/// delivering ~4.6 s per attempt abandoned a chapter that was still progressing.
/// That was found by reading a production access log. Untestable logic is how.
pub use story_proto::{MAX_TOTAL_RETRIES, MAX_WINDOW_RETRIES, MIN_PROGRESS_BYTES};

/// Chunks between `should_stop` polls. The poll reads the touch controller over
/// the I2C bus the codec shares, so polling all 62 chunks/second would triple
/// bus traffic for no perceptible gain. Every 4th chunk is a 64 ms worst-case
/// reaction — inside the window where a tap still feels instant.
const CANCEL_POLL_CHUNKS: u8 = 4;

/// **The paint budget while audio is streaming, in milliseconds.**
///
/// This constant is the whole reason live highlighting is possible at all, so it
/// is worth stating precisely why it is 30 and not, say, 120.
///
/// The read-aloud spec's §6.4 says never to paint during playback because a
/// paint "starves the audio DMA behind the 128 ms queue". The 128 ms playback
/// channel is **not** the relevant buffer: it is drained by `silent_clock_task`,
/// an Embassy task on the *same single-threaded executor* that a paint blocks.
/// The only buffer the DMA can consume while the executor is blocked is
/// `audio_out::TX_RING_LEN` — **48 ms**. Deepening the channel would not add a
/// microsecond of runway, and deepening the ring costs 16 KB of `.bss` against
/// ~9 KB of headroom (already tried twice and reverted — see `audio_out.rs`).
///
/// Full-frame Slint paints are **90–170 ms** (`main.rs:1726`, measured), which
/// is why the ping chime defers its visual until the clip finishes. But partial
/// rendering is enabled (`RepaintBufferType::ReusedBuffer`,
/// `slint_platform.rs:77`) and that figure is the *whole-scene* cost. The
/// playback screen's changing region is small and its geometry never moves, so
/// its paint should be single-digit milliseconds.
///
/// "Should be" is not good enough for something that decides whether audio comes
/// out chopped, and it cannot be measured without flashing. So 30 ms is a budget
/// that is **enforced, not assumed**: see [`PaintGate`].
pub const PAINT_BUDGET_MS: u32 = 30;

/// Cancellation latch, so any code path can stop playback without threading a
/// borrow. Same idiom as `voice_tts::SPEAK_CANCEL` and `audio_out::PLAYBACK_ACTIVE`.
pub static PLAY_CANCEL: AtomicBool = AtomicBool::new(false);

/// Pause request. Distinct from [`PLAY_CANCEL`] because the OUTCOME differs, not the
/// mechanism: both exit `play_chapter` promptly, but cancel discards the position and
/// pause hands it back to be fed straight in again.
///
/// # Why pause is a clean EXIT and not a held stream
///
/// The tempting implementation — stop pushing chunks and spin until resumed — is
/// precisely the failure the paint gate exists to prevent, and this module already
/// documents it: starving the feeder drains the DMA ring, the tail expires,
/// `PLAYBACK_ACTIVE`/`AMP_REQUEST` fall, the amp powers down, and the next chunk opens a
/// fresh session that waits on `AMP_READY`. So "hold the stream" cannot be held; the
/// hardware tears it down underneath you and the resume is audibly worse than a
/// re-entry.
///
/// Feeding silence instead would keep the session alive at the cost of streaming zeros
/// over a link that is already the bottleneck (`retries` exists because it is flaky),
/// and would burn battery on an idle amp for as long as the user is away.
///
/// The architecture already answers this. `play_chapter` takes `start_byte` and returns
/// `position` — "feed straight back in to resume" in its own words — because a chapter's
/// PCM (up to 25.7 MB) exceeds the entire 16 MB flash and Range-streaming was never
/// optional. Byte offset is `ms * 32` exactly, with no frame table, so a resume is
/// sample-accurate rather than approximate. Pause therefore costs one HTTP range request
/// on resume and nothing at all while paused.
pub static PLAY_PAUSE: AtomicBool = AtomicBool::new(false);

/// Request that playback stop at the next chunk boundary.
pub fn pause() {
    PLAY_PAUSE.store(true, Ordering::Relaxed);
}

/// Clear the pause latch. Called by the resume path before re-entering
/// `play_chapter`, so a stale request cannot pause the resumed stream instantly.
pub fn clear_pause() {
    PLAY_PAUSE.store(false, Ordering::Relaxed);
}

pub fn cancel() {
    PLAY_CANCEL.store(true, Ordering::Relaxed);
}

/// What the screen must provide for playback to drive it.
///
/// A trait rather than inline paint calls so the *player* owns the timing (it
/// alone knows where the audio pipeline is) while the *screen* owns what appears
/// — which is also what keeps this module independent of how highlighting is
/// eventually rendered.
pub trait PlaybackUi {
    /// True if the screen would like to repaint at `position_ms`.
    ///
    /// Called at a chunk boundary, so implementations must be cheap and must NOT
    /// paint here. Return true only when something visible actually changed —
    /// a segment boundary crossed, or a coarse progress tick — never per frame.
    fn wants_paint(&mut self, position_ms: u32) -> bool;

    /// Repaint now. Must dirty a small, fixed-geometry region; a full-frame
    /// repaint here will exceed [`PAINT_BUDGET_MS`] and be switched off.
    fn paint(&mut self);

    /// True to stop playback (finger down, app change, screen off).
    fn should_stop(&mut self) -> bool;

    /// Poll for a volume change requested during playback, as `(level, muted)`.
    ///
    /// WHY THIS EXISTS (#75 follow-up). `play_chapter` is `await`ed for a whole
    /// chapter and holds `&mut codec` for its duration, so the main loop's
    /// per-tick volume block cannot run — and both button sources (BOOT GPIO and
    /// the PMIC PWRON poll) are drained *by that loop*. Result before this hook:
    /// pressing volume during playback did nothing, and because the PMIC LATCHES
    /// its key event the press then applied after the chapter ended, which reads
    /// as "the buttons are dead" rather than "the buttons are deferred".
    ///
    /// The codec borrow is why the fix has to be shaped this way: only
    /// `play_chapter` can touch the codec here, so the implementer reports the
    /// desired level and playback applies it. Called at chunk boundaries, so it
    /// must be cheap and must not block.
    fn poll_volume(&mut self) -> Option<(u8, bool)> {
        None
    }

    /// Sample any *sampled* (non-latching) button EVERY chunk and latch its edge.
    ///
    /// WHY THIS IS SEPARATE FROM [`poll_volume`]. The two button sources are not
    /// equivalent: the PMIC LATCHES its key event, so a press is still there
    /// whenever firmware next looks, while the BOOT GPIO is a level that must be
    /// caught while it is held. `poll_volume` runs on the
    /// [`CANCEL_POLL_CHUNKS`]-chunk boundary (64 ms) because it shares that tick
    /// with `should_stop`, whose touch read costs I2C traffic — and a press that
    /// begins and ends inside one 64 ms window produces no observable edge at all.
    ///
    /// The result was an asymmetry that reads as a hardware fault: Volume− (PMIC,
    /// latched) 100 % reliable, Volume+ (BOOT, sampled) intermittently dead. Which
    /// is the same "the buttons don't work" complaint the volume hook was added to
    /// cure, just rarer and therefore harder to believe. The canonical button state
    /// machine in the main loop samples every tick and is built around presses
    /// ≥40 ms; 64 ms sampling is coarser than the thing being measured.
    ///
    /// Called on EVERY chunk (~16 ms), so it must touch nothing but GPIO — no I2C,
    /// no allocation, no logging. Tripling the I2C cadence by lowering
    /// `CANCEL_POLL_CHUNKS` instead would have paid for this in bus traffic during
    /// audio playback, which is the one place that bus is busiest.
    fn poll_button_edge(&mut self) {}
}

/// Enforces [`PAINT_BUDGET_MS`] and gives up permanently rather than risk audio.
///
/// The failure this exists to prevent is the one this project keeps meeting: a
/// path that reports complete success while the room hears something wrong. An
/// over-budget paint does not merely drop a frame — it drains the DMA ring, the
/// feeder's tail expires, `PLAYBACK_ACTIVE`/`AMP_REQUEST` fall, the amp powers
/// down, and the next chunk opens a fresh session that waits on `AMP_READY`
/// again: chopped audio with a pop at every seam, and a log that says every byte
/// streamed.
///
/// So the first breach disables highlighting for the rest of the chapter and
/// records the number. Worst case becomes "highlighting stopped, and here is the
/// measurement that says why" instead of a chapter that plays badly.
pub struct PaintGate {
    enabled: bool,
    worst_us: u32,
    paints: u32,
    /// Set when a paint blew the budget, so the caller can surface it.
    tripped: bool,
}

impl Default for PaintGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PaintGate {
    pub const fn new() -> Self {
        Self { enabled: true, worst_us: 0, paints: 0, tripped: false }
    }

    /// Worst paint observed, in microseconds. The figure to report — it is what
    /// turns "should be single-digit milliseconds" into a fact.
    pub fn worst_us(&self) -> u32 {
        self.worst_us
    }

    pub fn paints(&self) -> u32 {
        self.paints
    }

    /// True when highlighting was switched off mid-chapter to protect audio.
    pub fn tripped(&self) -> bool {
        self.tripped
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Paint if still allowed, timing it and latching off on a breach.
    fn try_paint(&mut self, ui: &mut dyn PlaybackUi) {
        if !self.enabled {
            return;
        }
        let t0 = Instant::now();
        ui.paint();
        let us = (Instant::now() - t0).as_micros().min(u32::MAX as u64) as u32;
        self.paints = self.paints.saturating_add(1);
        if us > self.worst_us {
            self.worst_us = us;
        }
        if us > PAINT_BUDGET_MS.saturating_mul(1000) {
            self.enabled = false;
            self.tripped = true;
        }
    }
}

/// How playback ended.
///
/// No `Debug` derive: `{:?}` would pull in a `core::fmt` instantiation. Use
/// [`label`](Played::label).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Played {
    /// Reached the end of the chapter.
    Complete,
    /// Stopped by [`cancel`] or `should_stop`.
    Cancelled,
    /// The playback session was torn down underneath us (DMA re-arm).
    Interrupted,
    /// Paused by [`pause`]. The distinction from `Cancelled` is the whole point: the
    /// caller must KEEP `Session::position` and offer resume, where a cancel drops it
    /// and returns to the list. Same exit path, opposite meaning to the user.
    Paused,
}

impl Played {
    pub fn label(self) -> &'static str {
        match self {
            Played::Complete => "complete",
            Played::Cancelled => "cancelled",
            Played::Interrupted => "interrupted",
            Played::Paused => "paused",
        }
    }
}

/// Result of playing (part of) a chapter.
pub struct Session {
    pub outcome: Played,
    /// Absolute byte offset reached — feed straight back in to resume.
    pub position: u32,
    pub gate: PaintGate,
    /// Windows that had to be retried, so a flaky link is visible rather than
    /// merely felt.
    pub retries: u16,
}

impl Session {
    /// Position in milliseconds. Exact, not estimated.
    pub fn position_ms(&self) -> u32 {
        story_proto::bytes_to_ms(self.position)
    }
}

/// Clears `STREAM_LIVE` on every exit path.
///
/// `play_chapter` leaves the streaming section by many routes — socket error,
/// stall, cancel, interrupt, end of chapter, retry exhaustion — and one missed
/// path latches the flag forever. Not a harmless leak: `PlaybackFeeder::resync`
/// keys off it, so a stuck flag gives every LATER chime and SFX stream semantics
/// on a DMA `Late` (skip the drain, resume) when a finite clip wants
/// drain-and-abort. A `Drop` guard makes the pairing structural.
struct StreamGuard;

impl Drop for StreamGuard {
    fn drop(&mut self) {
        audio_out::end_stream();
    }
}

/// Play `chapter` from `start_byte` to the end, or until stopped.
///
/// # Runs on the main loop, and must
///
/// `amp`/`codec` are pumped through [`audio_out::service_amp`] on every chunk.
/// This is load-bearing, not defensive: `PlaybackFeeder::gate_open` withholds
/// every sample until `AMP_READY`, which **only** `service_amp` sets, and that
/// needs the amp GPIO and the codec's I2C — both owned by the main loop. Drive
/// this anywhere that cannot pump it and every chunk waits out the 1,000 ms
/// `AMP_WAIT_MS` failsafe and then drains into a **muted DAC**: silent in the
/// room, fully successful in the log. That exact signature already burned the
/// ping chime once (read-aloud spec §6.2).
///
/// `start_byte` should come from `ms x 32` (or a previous [`Session::position`]).
/// It is exact by construction, so resume needs no index and no re-sync.
#[allow(clippy::too_many_arguments)]
pub async fn play_chapter<I: I2c>(
    stack: Stack<'static>,
    addr: Ipv4Address,
    chapter: u16,
    total_bytes: u32,
    start_byte: u32,
    amp: &mut Output<'static>,
    codec: &mut Es8311<I>,
    ui: &mut dyn PlaybackUi,
) -> Result<Session, Error> {
    // Fresh playback: clear a stale cancel or pause from a previous chapter. Without
    // the pause clear, tapping PAUSE and then picking a different chapter would pause
    // the new one on its first chunk.
    PLAY_CANCEL.store(false, Ordering::Relaxed);
    PLAY_PAUSE.store(false, Ordering::Relaxed);

    let mut pos = story_proto::align_sample(start_byte.min(total_bytes));
    let mut gate = PaintGate::new();
    let mut poll = 0u8;
    // Odd trailing byte carried between socket reads — see `feed_pcm`.
    let mut carry: Option<u8> = None;

    // Arm the abort latch and pair it with the guard before any audio moves.
    audio_out::begin_stream();
    let _guard = StreamGuard;

    let mut outcome = Played::Complete;

    // The window is re-derived from `pos` on EVERY attempt, including retries.
    //
    // That is the whole reason this is one loop rather than a window loop with a
    // retry loop inside it. `pos` advances as chunks are queued, so a window that
    // fails halfway has already played its first half; re-requesting the range
    // the window *started* at would push those bytes to the speaker a second time
    // — audible as a stutter that repeats a few seconds of narration, with the
    // progress bar still moving forward. Deriving from `pos` makes a retry resume
    // exactly where the audio stopped, which costs nothing because `ms x 32` is
    // exact.
    let mut budget = story_proto::RetryBudget::new();
    loop {
        let Some((first, last)) = story_proto::window_at(pos, total_bytes) else {
            break; // reached the end of the chapter
        };
        let before = pos;
        match stream_window(
            stack, addr, chapter, first, last, &mut pos, &mut carry, &mut poll, amp, codec,
            ui, &mut gate,
        )
        .await
        {
            Ok(Some(stop)) => {
                outcome = stop;
                // Drop what is still queued so the speaker stops promptly rather
                // than playing out the buffered 128 ms.
                audio_out::drain_queue();
                break;
            }
            // Window delivered: the link is healthy, clear the strikes.
            Ok(None) => budget.delivered(),
            Err(_e) => {
                // `failed` owns the whole decision — progress clears the strike
                // count, and the total cap guarantees termination. Host-tested in
                // `story-proto`'s tests/retry.rs.
                if budget.failed(pos.saturating_sub(before)) {
                    // Give up on the chapter but keep the position, so the caller
                    // can report progress and resume exactly here.
                    outcome = Played::Interrupted;
                    break;
                }
                // A partial chunk in flight is meaningless once the socket dies.
                pos = story_proto::align_sample(pos);
                carry = None;
            }
        }
    }

    // Leave the amp serviced on the way out so the feeder's tail can complete
    // and release it cleanly instead of waiting for the next main-loop tick.
    audio_out::service_amp(amp, codec);
    // Final catch for a press that landed in the last chunk, after the loop's
    // last poll. The mid-chapter case is handled per chunk in `feed_pcm` — this
    // alone is NOT sufficient and was the original bug.
    if let Some((level, muted)) = ui.poll_volume() {
        audio_out::set_master_volume(codec, level, muted);
    }

    Ok(Session { outcome, position: pos, gate, retries: budget.total() })
}

/// Stream one `Range` window. `Ok(None)` = delivered; `Ok(Some(_))` = stop.
#[allow(clippy::too_many_arguments)]
async fn stream_window<I: I2c>(
    stack: Stack<'static>,
    addr: Ipv4Address,
    chapter: u16,
    first: u32,
    last: u32,
    pos: &mut u32,
    carry: &mut Option<u8>,
    poll: &mut u8,
    amp: &mut Output<'static>,
    codec: &mut Es8311<I>,
    ui: &mut dyn PlaybackUi,
    gate: &mut PaintGate,
) -> Result<Option<Played>, Error> {
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(CONNECT_TIMEOUT));

    let head = story_proto::request(
        Method::Get,
        Route::Media { n: chapter },
        addr.octets(),
        DAEMON_PORT,
        Some((first, last)),
        None,
    )
    .ok_or("request head overflow")?;

    socket.connect((addr, DAEMON_PORT)).await.map_err(|_| "connect failed")?;
    let mut data = head.as_bytes();
    while !data.is_empty() {
        match socket.write(data).await {
            Ok(0) => return Err("socket write closed"),
            Ok(n) => data = data.get(n..).unwrap_or(&[]),
            Err(_) => return Err("socket write failed"),
        }
    }
    socket.set_timeout(None);

    // --- response head ---------------------------------------------------
    let mut buf = [0u8; BUF];
    let mut filled = 0usize;
    let parsed = loop {
        let Some(dst) = buf.get_mut(filled..) else { return Err("head too large") };
        if dst.is_empty() {
            return Err("head too large");
        }
        let n = match with_timeout(HEAD_TIMEOUT, socket.read(dst)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return Err("socket read failed"),
            Err(_) => return Err("media response timeout"),
        };
        if n == 0 {
            return Err("closed before head");
        }
        filled = filled.saturating_add(n);
        match story_proto::parse_head(buf.get(..filled).unwrap_or(&[])) {
            HeadParse::Ok(h) => break h,
            HeadParse::Incomplete => continue,
            HeadParse::Malformed => return Err("malformed media head"),
        }
    };
    if !parsed.ok() {
        // A 404 or 416 body would be JSON; queueing it as PCM fires noise at
        // the speaker.
        return Err(status_message(parsed.status));
    }

    // Where the body we are about to read actually starts in the FILE.
    //
    // Not necessarily where we asked: a server that ignored `Range` answers 200
    // with the body at byte 0. Treating that as the resume point would play the
    // chapter from the beginning while the progress bar claimed otherwise, so
    // the difference is skipped rather than assumed away.
    let body_at = parsed.body_starts_at(first);
    let mut skip = first.saturating_sub(body_at);

    let mut delivered = 0u32; // body bytes consumed this window
    let announced = parsed.content_length;

    // Body bytes that arrived alongside the head.
    if let Some(initial) = buf.get(parsed.body_offset..filled) {
        delivered = delivered.saturating_add(initial.len() as u32);
        if let Some(stop) =
            feed_pcm(initial, &mut skip, pos, carry, poll, amp, codec, ui, gate).await
        {
            return Ok(Some(stop));
        }
    }

    loop {
        if let Some(len) = announced {
            if delivered >= len {
                return Ok(None); // whole window delivered
            }
        }
        if *pos > last {
            return Ok(None);
        }
        let n = match with_timeout(BODY_READ_TIMEOUT, socket.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return Err("socket read failed"),
            Err(_) => return Err("media stream stalled"),
        };
        if n == 0 {
            // Closed. With no Content-Length that is a clean end of window.
            return if announced.is_none() { Ok(None) } else { Err("window truncated") };
        }
        let Some(piece) = buf.get(..n) else { return Ok(None) };
        delivered = delivered.saturating_add(n as u32);
        if let Some(stop) = feed_pcm(piece, &mut skip, pos, carry, poll, amp, codec, ui, gate).await
        {
            return Ok(Some(stop));
        }
    }
}

/// Queue `pcm` into the speaker, pumping the amp and honouring cancel.
///
/// `service_amp` runs BEFORE the awaiting `push_chunk` so `AMP_READY` is already
/// up when the feeder stages the very first chunk — otherwise chunk 0 waits out
/// `AMP_WAIT_MS` into a muted DAC.
///
/// # The odd-byte carry
///
/// A socket read can end mid-sample: PCM is 16-bit, and TCP knows nothing about
/// that. Pushing an odd-length chunk would shift every following byte by one
/// inside `fill_stereo`'s mono-to-stereo expansion and turn the rest of the
/// chapter into noise — a corruption that sounds like a hardware fault rather
/// than an off-by-one. So a trailing odd byte is carried into the next read.
#[allow(clippy::too_many_arguments)]
async fn feed_pcm<I: I2c>(
    pcm: &[u8],
    skip: &mut u32,
    pos: &mut u32,
    carry: &mut Option<u8>,
    poll: &mut u8,
    amp: &mut Output<'static>,
    codec: &mut Es8311<I>,
    ui: &mut dyn PlaybackUi,
    gate: &mut PaintGate,
) -> Option<Played> {
    // Discard bytes preceding our resume point (the 200-instead-of-206 case).
    let mut pcm = pcm;
    if *skip > 0 {
        let drop = (*skip as usize).min(pcm.len());
        *skip -= drop as u32;
        pcm = pcm.get(drop..).unwrap_or(&[]);
        if pcm.is_empty() {
            return None;
        }
    }

    // Re-attach a carried odd byte so chunk boundaries stay sample-aligned.
    //
    // Built in two steps so the mutable borrow of `joined` ends before `body`
    // takes an immutable one, and so a failed copy RESTORES the carry rather
    // than dropping it — silently losing one byte here would shift every
    // remaining sample in the chapter.
    let mut joined = [0u8; BUF + 1];
    let mut joined_len = 0usize;
    if let Some(c) = carry.take() {
        let n = pcm.len().min(BUF);
        let mut ok = false;
        if let Some((head, tail)) = joined.split_first_mut() {
            *head = c;
            if let (Some(dst), Some(src)) = (tail.get_mut(..n), pcm.get(..n)) {
                dst.copy_from_slice(src);
                ok = true;
            }
        }
        if ok {
            joined_len = n.saturating_add(1);
        } else {
            *carry = Some(c); // unreachable in practice; never lose the byte
            return None;
        }
    }
    let mut body: &[u8] = if joined_len > 0 {
        joined.get(..joined_len).unwrap_or(&[])
    } else {
        pcm
    };

    // Hold back a trailing odd byte for the next read.
    if body.len() % 2 == 1 {
        *carry = body.last().copied();
        body = body.get(..body.len().saturating_sub(1)).unwrap_or(&[]);
    }

    for chunk in body.chunks(audio_out::PLAY_CHUNK) {
        audio_out::service_amp(amp, codec);

        // EVERY chunk (~16 ms): GPIO only, no bus traffic. The 4-chunk boundary
        // below is gated on an I2C touch read, which is why the cheap sampled
        // button cannot simply share it — see `poll_button_edge`.
        ui.poll_button_edge();

        *poll = poll.wrapping_add(1);
        if *poll % CANCEL_POLL_CHUNKS == 0 {
            if ui.should_stop() {
                return Some(Played::Cancelled);
            }
            // Volume requested mid-chapter. THIS is the call site the
            // `poll_volume` doc comment describes ("called at chunk
            // boundaries") — a single call after the streaming loop, which is
            // where this started out, applies the press only once the chapter
            // ENDS and so reproduces the very symptom it was added to fix.
            // `feed_pcm` already holds `&mut codec` for `service_amp`, so the
            // codec borrow that forced the hook's shape is satisfied here.
            if let Some((level, muted)) = ui.poll_volume() {
                audio_out::set_master_volume(codec, level, muted);
            }
        }
        if PLAY_CANCEL.load(Ordering::Relaxed) {
            return Some(Played::Cancelled);
        }
        // Checked HERE, at the same chunk boundary as cancel and BEFORE the push, so the
        // pause lands on a whole 32-byte-aligned chunk. `*pos` has not yet advanced past
        // it, so the position handed back is exactly the first unplayed sample — resume
        // is sample-accurate, not "near enough". Both latches share this boundary
        // deliberately: a pause that took a different exit path could leave the amp in a
        // state cancel never produces, and there would be no test that ever covered it.
        if PLAY_PAUSE.load(Ordering::Relaxed) {
            return Some(Played::Paused);
        }
        if audio_out::stream_aborted() {
            // feeder.abort() tore the session down (DMA re-arm). Keep pushing
            // and we would pour into a channel nobody is draining.
            return Some(Played::Interrupted);
        }
        if !audio_out::push_chunk(chunk).await {
            // The queue never drained — the clock task is wedged. Bail rather
            // than park the main loop forever.
            return Some(Played::Interrupted);
        }
        *pos = pos.saturating_add(chunk.len() as u32);

        // Paint AFTER a successful push: the queue is as full as it will get,
        // so this is the moment with the most runway behind it. Gated and timed
        // — see PaintGate.
        let ms = story_proto::bytes_to_ms(*pos);
        if gate.enabled() && ui.wants_paint(ms) {
            gate.try_paint(ui);
        }
    }
    None
}
