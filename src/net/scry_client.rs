//! smol #540: the scry station's HTTP client — `POST /tap` + the streamed
//! `GET /screen` frame, against scry-glass's station API (labels `ae91b2d`).
//!
//! Contract (measured live by the labels spike, first paint 2026-09-01):
//! - Base: plain HTTP, dotted-quad only (the watch stack has no DNS) — baked
//!   via `SCRY_HOST` ("ip:port", default the ubox0 admin leg). The station
//!   joins the ADMIN network: ubox0 has no VLAN8 leg and iot is firewalled
//!   away from it (labels, measured — not assumed).
//! - Auth: every route takes `?k=<token>`, token = HMAC(secret,"station-<id>")
//!   [:12], baked via `SCRY_TOKEN` (the secret itself never leaves katana).
//! - `POST /tap/<UID>` resolves the uid→host binding server-side, fires the
//!   HA summon webhook + chronicle note, answers `{"host":"gatekeeper"}` or
//!   `{"host":null,...}` for an unbound sigil.
//! - `GET /screen/<host>` / `GET /screen-unbound/<UID>`: exactly 153,600 B of
//!   RGB565 **big-endian**, row-major, top-left, 320×240 landscape as mounted
//!   — display-ready, no rotation (server bakes any future case rotation).
//!
//! The frame is STREAMED in strips (8 rows = 5,120 B) straight to the caller's
//! blit — a full-frame buffer never exists (the spike measured 2,961 ms per
//! fetch+blit this way; the wire is the cost, the SPI half is noise).

use alloc::vec;

use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::{with_timeout, Duration};
use esp_println::println;

/// Station API endpoint, "ip:port". ubox0's ADMIN leg — see the module doc.
pub const HOST: &str = match option_env!("SCRY_HOST") {
    Some(h) => h,
    None => "10.0.6.11:7787",
};
/// Station capability token (`?k=`). Empty = unauthenticated build; the
/// server refuses, which is the honest failure.
pub const TOKEN: &str = match option_env!("SCRY_TOKEN") {
    Some(t) => t,
    None => "",
};

/// Panel geometry — the contract's, restated where the strip math lives.
pub const FRAME_W: usize = 320;
pub const FRAME_H: usize = 240;
/// Rows per streamed strip. 8 rows = 5,120 B — the spike's proven granularity.
pub const STRIP_ROWS: usize = 8;
const STRIP_BYTES: usize = FRAME_W * STRIP_ROWS * 2;
const FRAME_BYTES: usize = FRAME_W * FRAME_H * 2;

/// Longest host name we accept back from `/tap` (server names are short).
pub const HOST_CAP: usize = 24;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// Per-read inactivity budget. The spike's whole frame is ~3 s; a socket
/// quiet for this long is dead, not slow.
const STALL_TIMEOUT: Duration = Duration::from_secs(8);

pub enum TapOutcome {
    /// The sigil is bound: paint `/screen/<host>`.
    Bound(heapless::String<HOST_CAP>),
    /// Unbound sigil: paint `/screen-unbound/<uid>` once.
    Unbound,
    Failed(&'static str),
}

fn endpoint() -> Option<(embassy_net::Ipv4Address, u16)> {
    crate::net::mqtt_ha::parse_broker(HOST)
}

/// One short request/response exchange (the `/tap` POST). Returns the raw
/// response (headers + body) up to `buf`'s length.
async fn exchange<'b>(
    stack: Stack<'static>,
    request: &str,
    buf: &'b mut [u8],
) -> Result<&'b [u8], &'static str> {
    let (ip, port) = endpoint().ok_or("bad SCRY_HOST (want ip:port)")?;
    let mut rx = vec![0u8; 1024];
    let mut tx = vec![0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(STALL_TIMEOUT));
    match with_timeout(CONNECT_TIMEOUT, socket.connect((ip, port))).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err("tap: connect refused"),
        Err(_) => return Err("tap: connect timeout"),
    }
    socket
        .write(request.as_bytes())
        .await
        .map_err(|_| "tap: write failed")?;
    let mut n = 0usize;
    loop {
        if n == buf.len() {
            break;
        }
        match socket.read(&mut buf[n..]).await {
            Ok(0) => break,
            Ok(m) => n += m,
            Err(_) => break, // whatever arrived is what we parse
        }
    }
    socket.abort();
    Ok(&buf[..n])
}

/// POST the tap. The server does the uid→host mapping, the HA summon and the
/// chronicle note — firmware stays dumb (#540's ruling, HTTP-as-the-path).
pub async fn post_tap(stack: Stack<'static>, uid: &str) -> TapOutcome {
    let mut req: heapless::String<192> = heapless::String::new();
    {
        use core::fmt::Write as _;
        if write!(
            req,
            "POST /tap/{uid}?k={TOKEN} HTTP/1.0\r\nHost: {HOST}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .is_err()
        {
            return TapOutcome::Failed("tap: request too long");
        }
    }
    let mut buf = [0u8; 1024];
    let resp = match exchange(stack, req.as_str(), &mut buf).await {
        Ok(r) => r,
        Err(e) => return TapOutcome::Failed(e),
    };
    let Ok(text) = core::str::from_utf8(resp) else {
        return TapOutcome::Failed("tap: non-utf8 response");
    };
    if !text.starts_with("HTTP/1.0 200") && !text.starts_with("HTTP/1.1 200") {
        println!("[SCRY] tap refused: {}", text.lines().next().unwrap_or("?"));
        return TapOutcome::Failed("tap: non-200 (token? binding route?)");
    }
    // Minimal JSON peel: `"host":null` or `"host":"name"`. The body is the
    // server's own compact shape; a full parser earns nothing here.
    let Some(at) = text.find("\"host\"") else {
        return TapOutcome::Failed("tap: no host field");
    };
    let rest = text[at + 6..].trim_start_matches([':', ' ']);
    if rest.starts_with("null") {
        return TapOutcome::Unbound;
    }
    let Some(name) = rest.strip_prefix('"').and_then(|r| r.split('"').next()) else {
        return TapOutcome::Failed("tap: malformed host");
    };
    let mut host: heapless::String<HOST_CAP> = heapless::String::new();
    if host.push_str(name).is_err() {
        return TapOutcome::Failed("tap: host name too long");
    }
    TapOutcome::Bound(host)
}

/// Stream the station's resting face (`/screen-idle`, contract v4: gradient
/// void + orb + "TAP A DEVICE CARD HERE" + clock footer — 812 ms measured).
/// Same wire shape as [`fetch_screen`]; separate route, same streaming.
pub async fn fetch_idle(
    stack: Stack<'static>,
    blit: &mut dyn FnMut(u16, u16, &[u8]),
) -> Result<(), &'static str> {
    fetch_frame(stack, FramePath::Idle, blit).await
}

/// Stream one status frame, handing each 8-row strip (big-endian RGB565
/// bytes, display-ready) to `blit(y0, rows, bytes)`. `host` = `Some(name)`
/// for `/screen/<name>`, `None` for `/screen-unbound/<uid>`.
///
/// The body loop AWAITS the socket — under embassy that yield is what keeps
/// the esp-rtos WiFi task fed (the spike's hand-polled smoltcp loop starved
/// it and read 32 rows in 8 s; lesson inherited, not relearned).
pub async fn fetch_screen(
    stack: Stack<'static>,
    host: Option<&str>,
    uid: &str,
    blit: &mut dyn FnMut(u16, u16, &[u8]),
) -> Result<(), &'static str> {
    let path = match host {
        Some(h) => FramePath::Host(h),
        None => FramePath::Unbound(uid),
    };
    fetch_frame(stack, path, blit).await
}

enum FramePath<'a> {
    Idle,
    Host(&'a str),
    Unbound(&'a str),
}

async fn fetch_frame(
    stack: Stack<'static>,
    path: FramePath<'_>,
    blit: &mut dyn FnMut(u16, u16, &[u8]),
) -> Result<(), &'static str> {
    let (ip, port) = endpoint().ok_or("bad SCRY_HOST (want ip:port)")?;
    let mut req: heapless::String<192> = heapless::String::new();
    {
        use core::fmt::Write as _;
        let r = match path {
            FramePath::Idle => write!(
                req,
                "GET /screen-idle?k={TOKEN} HTTP/1.0\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"
            ),
            FramePath::Host(h) => write!(
                req,
                "GET /screen/{h}?k={TOKEN} HTTP/1.0\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"
            ),
            FramePath::Unbound(uid) => write!(
                req,
                "GET /screen-unbound/{uid}?k={TOKEN} HTTP/1.0\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"
            ),
        };
        if r.is_err() {
            return Err("screen: request too long");
        }
    }
    // Heap, not stack: this future is awaited INLINE in main's loop, so a
    // stack buffer here counts against main's #59 stack floor — and 8 KiB RX
    // + a strip blew it (68,604 < 71,680, boot-loop). The watch has a heap;
    // these are transient per-fetch. 8 KiB RX: the spike measured per-strip
    // stalls with a small window.
    let mut rx = vec![0u8; 8192];
    let mut tx = vec![0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(STALL_TIMEOUT));
    match with_timeout(CONNECT_TIMEOUT, socket.connect((ip, port))).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err("screen: connect refused"),
        Err(_) => return Err("screen: connect timeout"),
    }
    socket
        .write(req.as_bytes())
        .await
        .map_err(|_| "screen: write failed")?;

    // --- Skip the response head (status + headers), keeping body remainder --
    let mut head = [0u8; 1024];
    let mut head_n = 0usize;
    let body_start;
    loop {
        if head_n == head.len() {
            return Err("screen: header too large");
        }
        let n = match socket.read(&mut head[head_n..]).await {
            Ok(0) => return Err("screen: closed in header"),
            Ok(n) => n,
            Err(_) => return Err("screen: read failed in header"),
        };
        head_n += n;
        if let Some(pos) = head[..head_n].windows(4).position(|w| w == b"\r\n\r\n") {
            body_start = pos + 4;
            break;
        }
    }
    {
        let status = core::str::from_utf8(&head[..head_n.min(32)]).unwrap_or("");
        if !status.starts_with("HTTP/1.0 200") && !status.starts_with("HTTP/1.1 200") {
            return Err("screen: non-200 (token? unknown host?)");
        }
    }

    // --- Stream exactly FRAME_BYTES, strip by strip ------------------------
    let mut strip = vec![0u8; STRIP_BYTES]; // heap — see the rx note above
    let mut strip_n = 0usize;
    let mut total = 0usize;
    let mut y: u16 = 0;
    // Body bytes that arrived glued to the header.
    let mut carry = &head[body_start..head_n];
    while total < FRAME_BYTES {
        let n = if !carry.is_empty() {
            let n = carry.len().min(STRIP_BYTES - strip_n);
            strip[strip_n..strip_n + n].copy_from_slice(&carry[..n]);
            carry = &carry[n..];
            n
        } else {
            match socket.read(&mut strip[strip_n..]).await {
                Ok(0) => return Err("screen: closed mid-frame"),
                Ok(n) => n,
                Err(_) => return Err("screen: stalled mid-frame"),
            }
        };
        strip_n += n;
        total += n;
        if strip_n == STRIP_BYTES {
            blit(y, STRIP_ROWS as u16, &strip);
            y += STRIP_ROWS as u16;
            strip_n = 0;
        }
    }
    socket.abort();
    // FRAME_BYTES is an exact multiple of STRIP_BYTES (240 / 8 = 30), so a
    // leftover partial strip means the math above broke, not the server.
    debug_assert_eq!(strip_n, 0);
    Ok(())
}
