//! Endless LitRPG daemon client — the JSON half.
//!
//! Talks to `litrpg-daemon` over plain HTTP with a literal IPv4 address: **no
//! TLS and no DNS**, both hard constraints of this device. The daemon sits on
//! the admin VLAN alongside the watch, so no firewall rule is involved.
//!
//! Design spec: `~/Projects/endlesslitrpg/docs/superpowers/specs/2026-07-29-endless-litrpg-design.md`
//! (§9.1 routes, §9.4 the watch app).
//!
//! Deliberately parallel to [`crate::net::voice_stt`] / [`crate::net::voice_tts`]:
//! same `&'static str` error style, same dotted-quad plain HTTP, same
//! `Connection: close`, same hand-rolled request head (no `core::fmt`).
//!
//! # Nothing is buffered whole
//!
//! Response bodies are read in 512-byte pieces straight into a
//! [`story_proto::model::Reader`], which retains only bounded fields. The
//! chapter payload is ~18 KB — an 8.3 KB `text_md` plus segment texts up to
//! 3.6 KB each — and none of it is ever resident. That is the whole reason the
//! parser is a streaming scanner rather than a deserializer.
//!
//! # Audio is NOT here
//!
//! `GET /media/{n}.pcm` streams through [`crate::net::story_play`], which has to
//! pump the amp gate between chunks. Keeping the JSON client free of that
//! concern is why they are two modules.

use embassy_net::{dns::DnsQueryType, tcp::TcpSocket, Ipv4Address, Stack};
use embassy_time::{with_timeout, Duration};
use story_proto::model::{EventSink, Reader};
use story_proto::{HeadParse, Method, ResponseHead, Route};

/// Errors are `&'static str` for parity with the voice modules.
pub type Error = &'static str;

/// The chunk size the protocol crate does its byte-to-ms math against MUST equal
/// the one the playback queue actually uses. They live in different crates
/// (`story-proto` is host-testable and cannot see `audio_out`), so nothing but
/// this assert stops them drifting — and a silent drift would mis-time every
/// offset in the streaming loop. Same guard `voice_tts` uses for `tts_proto`.
const _: () = assert!(story_proto::PLAY_CHUNK == crate::peripherals::audio_out::PLAY_CHUNK);
const _: () =
    assert!(story_proto::SAMPLE_RATE == crate::peripherals::audio_out::PLAY_SAMPLE_RATE);

/// The daemon's hostname.
///
/// **The design spec's §9.1 claim that the watch has "no TLS and no DNS" is half
/// wrong.** No TLS is correct. But DNS has worked on this device since the
/// weather feature — `embassy-net`'s `dns` feature is enabled and
/// `src/net/weather.rs:62` already resolves a name. So the pinned static DHCP
/// lease §9.1 justifies on "no DNS" grounds is belt-and-braces, not load-bearing.
///
/// One line to change if the daemon moves host.
///
/// **`katana`, not `familiar`** — verified empirically rather than from the spec,
/// which still says `familiar` / `10.0.6.107` (design §9.1). Measured
/// 2026-07-29: `katana.lan:8093/healthz` answers `ok`, `familiar.lan:8093` does
/// not, and both `litrpg-daemon` and `litrpg-engine` run on katana. `gatekeeper`
/// holds static MAC reservations with `dns='1'` for both hosts, so the name
/// resolves LAN-wide with no server-side work.
const DAEMON_HOST: &str = "katana.lan";

/// Literal fallback, used when resolution fails.
///
/// Not redundancy for its own sake: a chapter streams for up to 18 minutes, and
/// losing playback because a resolver blipped would be a worse failure than
/// having a stale address. `gatekeeper` holds a static MAC reservation for
/// katana, so the literal is stable by configuration.
///
/// Must match [`DAEMON_HOST`]: `katana` is `10.0.6.129`. The design spec's
/// `10.0.6.107` is `familiar`, which is **not** serving the daemon — verified,
/// not assumed.
const FALLBACK_IP: Ipv4Address = Ipv4Address::new(10, 0, 6, 129);

/// The daemon's address: resolve the name, fall back to the literal.
///
/// Costs one DNS round trip per request. That is invisible against a JSON fetch
/// and irrelevant against a 60-second PCM window, and it means the address is not
/// frozen into the firmware image.
pub async fn resolve(stack: Stack<'static>) -> Ipv4Address {
    match stack.dns_query(DAEMON_HOST, DnsQueryType::A).await {
        Ok(addrs) => match addrs.first() {
            Some(embassy_net::IpAddress::Ipv4(v4)) => *v4,
            _ => FALLBACK_IP,
        },
        Err(_) => FALLBACK_IP,
    }
}

/// The literal address, for callers that cannot await a resolution.
pub fn default_ip() -> Ipv4Address {
    FALLBACK_IP
}

/// The daemon's port. Verified free on `familiar` before it was chosen.
pub const DAEMON_PORT: u16 = 8093;

/// Response-head scratch, doubling as the body read window.
///
/// 512 B is both a comfortable margin over the daemon's ~200-byte heads and
/// exactly one `PLAY_CHUNK`, so the JSON path and the audio path read in the
/// same size and there is one number to reason about.
const BUF: usize = 512;

/// Connect + request deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Response-head deadline. Generous: the daemon may be mid-render, and a slow
/// but valid answer must not fail as a socket error (the trap `voice_tts`
/// documents).
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-read deadline once the body is flowing. These payloads are small and
/// already in the daemon's memory, so a stall this long means it died.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(8);

/// Hard cap on a JSON response, so a runaway body cannot park the main loop.
///
/// The largest real payload is ~18 KB (a chapter with its manifest). 128 KB is
/// ample headroom for a much longer chapter while still bounding the damage —
/// and because nothing is retained, the cost of reading it is time, not memory.
const MAX_JSON_BYTES: u32 = 128 * 1024;

/// `GET` a JSON route, streaming the body through `sink`.
///
/// A body that is malformed, or that ends mid-document, is an error rather than a
/// partially-populated model: a half-read chapter must not be presented as a
/// whole one.
///
/// # Why `&mut dyn` and not a generic
///
/// **This is a memory decision, not a style one.** Generic over the sink, this
/// function is monomorphised once per model — and because it is awaited from the
/// main loop, each copy's 2 KB of socket buffers lands in the main task's future,
/// which lives in `.bss` and steals directly from the stack. Three models
/// (chapter list, segment index, character) cost three sets.
///
/// Measured, not theorised: the generic version put `--features story` at a
/// **69,304 B** stack gap against a 71,680 B floor — 2,376 B *below* the boot
/// assert, i.e. a watch that panics on startup. One `dyn` instantiation is the
/// same trade `voice_tts::speak_text` makes for `should_stop`, and for the same
/// reason.
pub async fn get_json(
    stack: Stack<'static>,
    route: Route,
    sink: &mut dyn EventSink,
) -> Result<(), Error> {
    let addr = resolve(stack).await;
    request_json(stack, addr, DAEMON_PORT, Method::Get, route, None, Some(sink)).await
}

/// The ONE HTTP call in this module.
///
/// `GET`-and-parse and `PUT`/`POST`-a-body differ by two `Option`s, so they share
/// one function — and therefore **one** set of socket buffers in the main task's
/// future rather than two. That is worth ~1.5 KB of `.bss`, which on this device
/// comes straight out of the stack: see [`get_json`]'s note on why the generic
/// version of this failed the boot assert outright.
///
/// `body` is sent when present; `sink` is fed the response body when present
/// (the mutating routes answer with echoes the watch has no use for, so it
/// checks only their status).
#[allow(clippy::too_many_arguments)]
async fn request_json(
    stack: Stack<'static>,
    addr: Ipv4Address,
    port: u16,
    method: Method,
    route: Route,
    body: Option<&str>,
    sink: Option<&mut dyn EventSink>,
) -> Result<(), Error> {
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(CONNECT_TIMEOUT));

    let head = story_proto::request(
        method,
        route,
        addr.octets(),
        port,
        None,
        body.map(|b| b.len()),
    )
    .ok_or("request head overflow")?;

    connect_and_send(&mut socket, addr, port, head.as_bytes(), body.map(|b| b.as_bytes()))
        .await?;

    let mut buf = [0u8; BUF];
    let (parsed, filled) = read_head(&mut socket, &mut buf).await?;
    if !parsed.ok() {
        return Err(status_message(parsed.status));
    }

    // Mutating routes: the status was the whole answer.
    let Some(sink) = sink else {
        socket.abort();
        return Ok(());
    };

    let mut reader = Reader::new(sink);
    let mut read = 0u32;

    // Body bytes that arrived alongside the head.
    if let Some(initial) = buf.get(parsed.body_offset..filled) {
        read = read.saturating_add(initial.len() as u32);
        reader.feed(initial);
    }

    loop {
        if let Some(len) = parsed.content_length {
            if read >= len {
                break;
            }
        }
        if read >= MAX_JSON_BYTES {
            return Err("json body too large");
        }
        let n = match with_timeout(BODY_READ_TIMEOUT, socket.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return Err("socket read failed"),
            Err(_) => return Err("json stream stalled"),
        };
        if n == 0 {
            break; // daemon closed — end of body
        }
        let Some(piece) = buf.get(..n) else { break };
        read = read.saturating_add(n as u32);
        reader.feed(piece);
        // Stop early on malformed input rather than reading a hostile body to
        // its end; the scanner has already latched and emits nothing more.
        if reader.error() {
            return Err("malformed json");
        }
    }

    socket.abort();

    if reader.error() {
        return Err("malformed json");
    }
    if !reader.complete() {
        // The document ended mid-structure: the socket died or the daemon
        // truncated. Either way this is not a chapter we can trust.
        return Err("incomplete json");
    }
    Ok(())
}

/// `PUT /api/progress` — report how far JP has actually listened.
///
/// Not a convenience: `consumed_through` is what stops chapter generation
/// running away, so the watch reporting it is load-bearing for the whole system
/// (design §9.4).
pub async fn put_progress(
    stack: Stack<'static>,
    consumed_through: u16,
) -> Result<(), Error> {
    let body = story_proto::encode_progress(consumed_through).ok_or("body overflow")?;
    let addr = resolve(stack).await;
    request_json(
        stack,
        addr,
        DAEMON_PORT,
        Method::Put,
        Route::Progress,
        Some(&body),
        None,
    )
    .await
}

/// `POST /api/notes` — a director note dictated through the STT gateway.
pub async fn post_note(stack: Stack<'static>, text: &str) -> Result<(), Error> {
    // A misfired push-to-talk must not become a note that steers the story.
    if !story_proto::is_notable(text) {
        return Err("nothing to say");
    }
    let body = story_proto::encode_note(text).ok_or("body overflow")?;
    let addr = resolve(stack).await;
    request_json(
        stack,
        addr,
        DAEMON_PORT,
        Method::Post,
        Route::Notes,
        Some(&body),
        None,
    )
    .await
}

/// Connect, write the head and optional body, then hand the socket back with
/// its idle timeout cleared so [`HEAD_TIMEOUT`] is the single deadline while the
/// daemon works.
async fn connect_and_send(
    socket: &mut TcpSocket<'_>,
    addr: Ipv4Address,
    port: u16,
    head: &[u8],
    body: Option<&[u8]>,
) -> Result<(), Error> {
    socket.connect((addr, port)).await.map_err(|_| "connect failed")?;
    write_all(socket, head).await?;
    if let Some(b) = body {
        write_all(socket, b).await?;
    }
    socket.set_timeout(None);
    Ok(())
}

/// Read until the response head is complete.
///
/// Returns the parsed head and how many bytes of `buf` are filled, so the caller
/// can feed the body bytes that arrived in the same segment.
async fn read_head(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8; BUF],
) -> Result<(ResponseHead, usize), Error> {
    let mut filled = 0usize;
    loop {
        let Some(dst) = buf.get_mut(filled..) else {
            return Err("response head too large");
        };
        if dst.is_empty() {
            return Err("response head too large");
        }
        let n = match with_timeout(HEAD_TIMEOUT, socket.read(dst)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return Err("socket read failed"),
            Err(_) => return Err("response timeout"),
        };
        if n == 0 {
            return Err("closed before head");
        }
        filled = filled.saturating_add(n);
        match story_proto::parse_head(buf.get(..filled).unwrap_or(&[])) {
            HeadParse::Ok(h) => return Ok((h, filled)),
            HeadParse::Incomplete => continue,
            HeadParse::Malformed => return Err("malformed response head"),
        }
    }
}

/// Write a whole buffer, tolerating short writes.
async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), Error> {
    while !data.is_empty() {
        match socket.write(data).await {
            Ok(0) => return Err("socket write closed"),
            Ok(n) => data = data.get(n..).unwrap_or(&[]),
            Err(_) => return Err("socket write failed"),
        }
    }
    Ok(())
}

/// A `&'static str` per status, because `{}`-formatting a code would link
/// `core::fmt`.
pub fn status_message(status: u16) -> Error {
    match status {
        400 => "bad request",
        404 => "not found",
        416 => "range not satisfiable",
        500 => "daemon error",
        502 | 503 => "daemon unavailable",
        _ => "unexpected status",
    }
}
