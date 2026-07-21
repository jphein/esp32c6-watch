//! Voice-to-text upload: stream mic PCM to the LAN STT bridge, get a transcript.
//!
//! Companion to [`crate::net::ota_http`] but in the UPLOAD direction. Opens a
//! plain-HTTP `TcpSocket` to the STT bridge (`watch_bridge.py` on familiar),
//! `POST /stt` with `Transfer-Encoding: chunked`, streams the raw **mono
//! 16 kHz 16-bit-LE PCM** body as it is captured (push-to-talk), then reads the
//! `200 {"text":"..."}` reply and returns the transcript.
//!
//! Plain HTTP only — no TLS, no DNS (dotted-quad IPv4), exactly like `ota_http`.
//! The bridge holds the Azure key and does the HTTPS hop to Azure; the watch
//! never sees a secret. Bridge contract (verified end-to-end):
//!   POST /stt  body: raw mono 16 kHz 16-bit-LE PCM (headerless)  -> 200 {"text": "..."}
//!
//! Wiring (MC5, later, in main.rs): capture I2S RX -> extract one channel to
//! mono (see [`stereo_to_mono_le`]) -> feed chunks through a [`PcmSource`] into
//! [`stream_utterance`]; show the returned string on the Slint Voice page.

use alloc::format;

use embassy_net::{tcp::TcpSocket, Ipv4Address, Stack};
use embassy_time::{with_timeout, Duration};
use heapless::String;

/// STT bridge default address — ubox0's VLAN-11 IP, reachable from the roam
/// (VLAN 11) network the watch is on; the old 10.0.6.107 sat on VLAN 6, which
/// roam is firewalled off from. Dotted-quad, no DNS.
/// MC5 can pass a different address to [`stream_utterance_to`] (e.g. from config).
pub fn default_bridge_ip() -> Ipv4Address {
    Ipv4Address::new(10, 0, 11, 11)
}
/// STT bridge port (matches `watch_bridge.py` default / systemd `BRIDGE_PORT`).
pub const BRIDGE_PORT: u16 = 8090;

/// Max transcript length returned to the caller (bounded, heapless).
pub const MAX_TRANSCRIPT: usize = 256;
/// Idle timeout while waiting for the bridge's response (Azure round-trip).
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// Socket idle timeout for the CONNECT + STREAM phase only. It is cleared before
/// `read_response` so it can't pre-empt the deliberate [`RESPONSE_TIMEOUT`] while
/// waiting on Azure (a slow round-trip past this would otherwise idle-abort the
/// read and spuriously fail an otherwise-good transcription).
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);

/// Source of captured audio: yields successive chunks of **mono 16 kHz
/// 16-bit-LE PCM**. Implemented by the caller (MC5) over the I2S RX ring or an
/// embassy channel the capture task feeds.
///
/// [`next_chunk`](PcmSource::next_chunk) fills `buf` and returns the number of
/// bytes written; **returning 0 signals end-of-utterance** (push-to-talk
/// released), which flushes the chunked body terminator. It may `.await`
/// (e.g. on the ring/channel) while the finger is held and no samples are ready.
pub trait PcmSource {
    async fn next_chunk(&mut self, buf: &mut [u8]) -> usize;
}

/// Errors are `&'static str` for parity with [`crate::net::ota_http`].
pub type Error = &'static str;

/// Stream one push-to-talk utterance to the bridge and return the transcript.
///
/// `source` yields mono 16 kHz PCM until it returns 0 (finger released). The
/// response body (`{"text": "..."}`) is parsed and the transcript returned as a
/// bounded [`String`]. An empty string means the bridge recognized no speech.
pub async fn stream_utterance<S: PcmSource>(
    stack: Stack<'static>,
    source: &mut S,
) -> Result<String<MAX_TRANSCRIPT>, Error> {
    stream_utterance_to(stack, default_bridge_ip(), BRIDGE_PORT, source).await
}

/// Same as [`stream_utterance`] but with an explicit bridge address/port.
pub async fn stream_utterance_to<S: PcmSource>(
    stack: Stack<'static>,
    addr: Ipv4Address,
    port: u16,
    source: &mut S,
) -> Result<String<MAX_TRANSCRIPT>, Error> {
    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(SOCKET_TIMEOUT));

    socket
        .connect((addr, port))
        .await
        .map_err(|_| "connect failed")?;

    // --- request head: chunked POST -----------------------------------------
    let head = format!(
        "POST /stt HTTP/1.1\r\n\
         Host: {addr}:{port}\r\n\
         Content-Type: application/octet-stream\r\n\
         Transfer-Encoding: chunked\r\n\
         Connection: close\r\n\r\n",
    );
    write_all(&mut socket, head.as_bytes()).await?;

    // --- stream the PCM body as HTTP chunks until the source ends -----------
    // One capture chunk per HTTP chunk. 512 B = 256 mono samples ≈ 16 ms @16kHz.
    let mut pcm = [0u8; 512];
    loop {
        let n = source.next_chunk(&mut pcm).await;
        if n == 0 {
            break; // push-to-talk released
        }
        // chunk framing: "<hex-len>\r\n" <data> "\r\n"
        let mut hdr: String<16> = String::new();
        write_hex_len(&mut hdr, n);
        write_all(&mut socket, hdr.as_bytes()).await?;
        write_all(&mut socket, &pcm[..n]).await?;
        write_all(&mut socket, b"\r\n").await?;
    }
    // terminating zero-length chunk
    write_all(&mut socket, b"0\r\n\r\n").await?;

    // --- response (bounded, Azure round-trip) -------------------------------
    // Drop the connect/stream idle timeout: `with_timeout(RESPONSE_TIMEOUT)` is
    // now the single authoritative deadline. Without this, SOCKET_TIMEOUT (15s)
    // would idle-abort `socket.read()` before the 30s response budget and fail a
    // slow-but-valid Azure transcription with "socket read failed".
    socket.set_timeout(None);
    let text = match with_timeout(RESPONSE_TIMEOUT, read_response(&mut socket)).await {
        Ok(r) => r?,
        Err(_) => return Err("response timeout"),
    };
    socket.abort();
    Ok(text)
}

/// Read + parse the `200 {"text":"..."}` response; return the transcript.
async fn read_response(socket: &mut TcpSocket<'_>) -> Result<String<MAX_TRANSCRIPT>, Error> {
    // Read headers until CRLFCRLF, then the body. The bridge sends a small
    // JSON body with Content-Length, so a single fixed buffer suffices.
    let mut buf = [0u8; 1024];
    let mut len = 0;
    let body_start = loop {
        if len == buf.len() {
            return Err("response too large");
        }
        let n = socket.read(&mut buf[len..]).await.map_err(|_| "socket read failed")?;
        if n == 0 {
            return Err("connection closed in headers");
        }
        len += n;
        if let Some(pos) = find(&buf[..len], b"\r\n\r\n") {
            break pos + 4;
        }
    };
    check_status_200(&buf[..body_start])?;

    // Ensure we have the whole body: read until the socket closes or the JSON
    // closes. Content-Length is small; keep reading opportunistically.
    loop {
        if find(&buf[..len], b"}").is_some() {
            break;
        }
        if len == buf.len() {
            break;
        }
        let n = socket.read(&mut buf[len..]).await.map_err(|_| "socket read failed")?;
        if n == 0 {
            break;
        }
        len += n;
    }
    parse_text_field(&buf[body_start..len])
}

/// Verify the status line is `HTTP/1.x 200`.
fn check_status_200(header: &[u8]) -> Result<(), Error> {
    let first = header.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let code = first
        .split(|&b| b == b' ')
        .nth(1)
        .and_then(|c| core::str::from_utf8(c).ok())
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or("malformed status line")?;
    if code != 200 {
        return Err("bridge status not 200");
    }
    Ok(())
}

/// Extract the `"text"` string value from `{"text": "..."}` (minimal unescape
/// for `\"`, `\\`, `\n`, `\t`). Returns "" if the key/value is absent.
fn parse_text_field(body: &[u8]) -> Result<String<MAX_TRANSCRIPT>, Error> {
    let s = core::str::from_utf8(body).map_err(|_| "non-utf8 response")?;
    let mut out: String<MAX_TRANSCRIPT> = String::new();
    let Some(key) = s.find("\"text\"") else {
        return Ok(out); // no field -> empty transcript
    };
    // find the opening quote of the value after the colon
    let after = &s[key + 6..];
    let Some(colon) = after.find(':') else { return Ok(out) };
    let rest = &after[colon + 1..];
    let Some(open) = rest.find('"') else { return Ok(out) };
    let val = &rest[open + 1..];
    let mut chars = val.chars();
    while let Some( c) = chars.next() {
        match c {
            '"' => break, // closing quote
            '\\' => {
                let e = chars.next().unwrap_or('\\');
                let decoded = match e {
                    'n' => '\n',
                    't' => '\t',
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    other => other,
                };
                let _ = out.push(decoded); // ignore overflow past MAX_TRANSCRIPT
            }
            other => {
                let _ = out.push(other);
            }
        }
    }
    Ok(out)
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

/// Append the lowercase hex length + CRLF (chunk-size line) to `hdr`.
fn write_hex_len(hdr: &mut String<16>, mut n: usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [0u8; 8];
    let mut i = digits.len();
    if n == 0 {
        i -= 1;
        digits[i] = b'0';
    }
    while n > 0 {
        i -= 1;
        digits[i] = HEX[n & 0xf];
        n >>= 4;
    }
    for &d in &digits[i..] {
        let _ = hdr.push(d as char);
    }
    let _ = hdr.push_str("\r\n");
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extract one channel from interleaved stereo 16-bit-LE PCM into `out`, giving
/// **mono** PCM (what the bridge/Azure want). `right` selects R (else L).
/// Returns the number of mono bytes written. Helper for MC5's capture drain.
pub fn stereo_to_mono_le(stereo: &[u8], out: &mut [u8], right: bool) -> usize {
    let off = if right { 2 } else { 0 };
    let mut w = 0;
    let mut i = off;
    // one 16-bit sample per 4-byte stereo frame
    while i + 2 <= stereo.len() && w + 2 <= out.len() {
        out[w] = stereo[i];
        out[w + 1] = stereo[i + 1];
        w += 2;
        i += 4;
    }
    w
}
