//! Plain-HTTP climate + energy client for the native `esp32c6_watch` HA component.
//!
//! Replaces the MQTT session (`crate::net::mqtt_climate`, removed) 1:1 in public
//! surface, so `main.rs` / `climate_task` change only the module path. Instead of a
//! long-lived MQTT CONNECT + SUBSCRIBE against a broker, this polls a native Home
//! Assistant custom component over **plain HTTP** — the same transport the rest of
//! the watch net stack uses ([`crate::net::voice_stt`], [`crate::net::ota_http`],
//! [`crate::net::weather`]): dotted-quad IPv4, no TLS, no DNS.
//!
//! ## Why HTTP-direct (see docs/superpowers/specs/2026-07-21-ha-watch-component.md)
//! The watch is on WiFi `roam` (VLAN 11, 10.0.11.0/24), firewalled off the VLAN-6
//! server LAN. The old `MQTT_BROKER=10.0.6.11:1883` was on VLAN 6 — unreachable
//! from roam. HA is quad-homed with a VLAN-11 leg at 10.0.11.110 (same L2 as the
//! watch), and the `esp32c6_watch` component serves a plain-HTTP listener there, so
//! the watch reaches it same-subnet with no broker, no Node-RED, no proxy.
//!
//! ## Endpoints (contract owned by the HA component)
//!   - `GET  /watch/climate/state`         -> JSON array of climate entities (+`id`)
//!   - `POST /watch/climate/<obj>/set`     <- `{"set":72.0}` | `{"mode":"heat"}`
//!   - `GET  /watch/energy`                -> `{"battery_pct":..,"solar_w":..,..}`
//!
//! ## Reuse of `climate-model` (UNCHANGED)
//! Parsing/encoding is delegated to the pure `climate-model` crate exactly as the
//! MQTT path did:
//!   - `parse_state(&[u8]) -> Option<ClimateEntity>` (per array element; the extra
//!     `"id"` key is an unknown field the parser ignores)
//!   - `ClimateState::upsert(&mut self, obj: &str, entity: ClimateEntity)`
//!   - `encode_set_temp(f32)` / `encode_set_mode(HvacMode)` -> command bodies
//!
//! ## Untrusted input
//! Every response byte is untrusted network input. The array splitter is a bounded,
//! string-aware balanced-brace scan (never a raw index on attacker offsets), the
//! response is size-capped, and a malformed frame is skipped — never a panic.
//!
//! ## Lifecycle (unchanged from the MQTT path)
//! `run_climate_session` runs while the Climate OR Energy screen is open and returns
//! on `close` (or is simply cancelled). It never holds the radio across a return, so
//! `main.rs`/`climate_task` restore mesh unconditionally on close — the same
//! guarantee the MQTT session gave. HTTP is stateless, so transient GET/POST
//! failures do NOT end the session (there is no connection to "lose"); they leave
//! the last-known state in place and are retried on the next poll.

use alloc::{format, vec::Vec as AllocVec};

use climate_model;
use climate_model::{ClimateState, HvacMode};

use embassy_futures::select::{select3, Either3};
use embassy_net::{tcp::TcpSocket, Ipv4Address, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_println::println;
use heapless::String;

// --- public types the main.rs integrator allocates + matches (verbatim from the
//     MQTT module so the swap is module-path-only) ----------------------------

/// Errors are `&'static str` for parity with [`crate::net::ota_http`] / `voice_stt`.
pub type Error = &'static str;

/// Max object-id length (path component `watch/climate/<id>/set`). Matches
/// `climate_model::OBJ_ID_CAP` so a cloned model id fits a command unchanged.
pub const OBJ_ID_CAP: usize = 48;
/// Object id carried in a queued command (a bounded, heapless path component).
pub type ObjId = String<OBJ_ID_CAP>;

/// Command-queue depth (UI -> session). Debounced taps, so shallow is fine.
pub const CMD_QUEUE_DEPTH: usize = 4;

/// Shared climate state — session writes (rebuild/upsert), UI reads (build cards).
pub type ClimateStateMutex = Mutex<CriticalSectionRawMutex, ClimateState>;

/// UI -> session command channel and its endpoints.
pub type ClimateCmdChannel = Channel<CriticalSectionRawMutex, ClimateCmd, CMD_QUEUE_DEPTH>;
pub type ClimateCmdReceiver = Receiver<'static, CriticalSectionRawMutex, ClimateCmd, CMD_QUEUE_DEPTH>;
pub type ClimateCmdSender = Sender<'static, CriticalSectionRawMutex, ClimateCmd, CMD_QUEUE_DEPTH>;

/// Screen-close signal — fire it to end the session (clean return -> mesh restore).
pub type CloseSignal = Signal<CriticalSectionRawMutex, ()>;

/// Live HA energy snapshot consumed from `GET /watch/energy`. Small, `Copy`, behind
/// the same [`Mutex`] pattern as [`ClimateState`]. Numeric fields are `Option` so
/// the UI can distinguish "no data yet" from a real 0.
///
/// Contract preserved from the MQTT path: keys `battery_pct` / `solar_w` / `grid_w`
/// / `charging`; `grid_w` >0 import / <0 export. Unlike the MQTT path there is no
/// LWT — [`online`](Self::online) is set from GET success/failure by the poll loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnergyState {
    /// Home battery state of charge, 0..=100 %.
    pub battery_pct: Option<u8>,
    /// Solar/PV production, watts (>= 0).
    pub solar_w: Option<i32>,
    /// Grid flow, watts, **signed: + = importing, - = exporting**.
    pub grid_w: Option<i32>,
    /// Battery is charging.
    pub charging: bool,
    /// HA reachable — set true on a successful `GET /watch/energy`, false on
    /// failure. `false` -> the UI shows "HA unreachable" (conn-state = 2).
    pub online: bool,
}

impl EnergyState {
    pub const fn new() -> Self {
        Self {
            battery_pct: None,
            solar_w: None,
            grid_w: None,
            charging: false,
            online: false,
        }
    }

    /// True once at least one numeric field has been received (UI "connecting…"
    /// vs live gate).
    pub fn has_data(&self) -> bool {
        self.battery_pct.is_some() || self.solar_w.is_some() || self.grid_w.is_some()
    }
}

impl Default for EnergyState {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared energy snapshot — session updates it, the Energy screen reads it.
pub type EnergyStateMutex = Mutex<CriticalSectionRawMutex, EnergyState>;

/// A command the UI queues for the session to POST to `watch/climate/<obj>/set`.
/// `HvacMode` is `climate-model`'s own enum (carried straight through to
/// `encode_set_mode`).
pub enum ClimateCmd {
    SetTemp { obj: ObjId, temp: f32 },
    SetMode { obj: ObjId, mode: HvacMode },
}

impl ClimateCmd {
    fn obj(&self) -> &str {
        match self {
            ClimateCmd::SetTemp { obj, .. } | ClimateCmd::SetMode { obj, .. } => obj.as_str(),
        }
    }
}

// --- configuration (baked at build time; configurable like voice_stt) ---------

/// HA component endpoint as "ip:port". Set `HA_HTTP` at build time to override.
/// Default = HA's VLAN-11 (roam) leg + the component's dedicated plain-HTTP port,
/// reachable same-subnet from the watch (10.0.11.0/24).
pub const HA_HTTP: &str = match option_env!("HA_HTTP") {
    Some(s) => s,
    None => "10.0.11.110:8124",
};

/// Optional shared-secret sent as `X-Watch-Token` (baked via `WATCH_HA_TOKEN`).
/// `None` -> header omitted (the component only enforces it if a token is set there).
const HA_TOKEN: Option<&str> = option_env!("WATCH_HA_TOKEN");

/// Default HA component address — dotted-quad, no DNS. Mirrors
/// [`voice_stt::default_bridge_ip`](crate::net::voice_stt::default_bridge_ip): the
/// watch is on `roam` (VLAN 11), and HA carries VLAN 11 at 10.0.11.110, so the
/// component is reached same-subnet. Overridden by parsing [`HA_HTTP`].
pub fn default_ha_addr() -> Ipv4Address {
    Ipv4Address::new(10, 0, 11, 110)
}
/// Default HA component port (the component's plain-HTTP listener; `DEFAULT_PORT`).
pub const HA_PORT: u16 = 8124;

// --- session tunables ---------------------------------------------------------

/// How often to refresh climate + energy state while a HA screen is open. The
/// first poll fires immediately on session open (state on Climate-screen open).
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Hard wall-clock cap on a single request (connect + send + full response).
const REQ_TIMEOUT: Duration = Duration::from_secs(6);
/// Socket idle timeout for the connect/exchange phase.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// TCP receive-ring size passed to the socket (per request; heap, not `.bss`).
const RX_RING: usize = 1536;
/// TCP transmit-ring size (per request; heap, not `.bss`).
const TX_RING: usize = 768;
/// Response accumulation buffer: headers + body of the largest response (the
/// climate-state array for up to `MAX_ENTITIES` ~= 3 KB). Heap-allocated so it
/// never enters `.bss` — keeps the `climate_task` future small and the stack gap
/// wide (memory: c6-stack-geometry-and-esp-hal-guard; v0.5.1 flagged the MQTT
/// session's on-stack buffers as the `.bss` pressure this design removes).
const RESP_CAP: usize = 6144;

/// `"/watch/climate/" + max obj id + "/set"`, rounded up.
const PATH_CAP: usize = 96;

// --- public entry point -------------------------------------------------------

/// Run one climate+energy polling session until [`close`] fires (or the future is
/// cancelled). Returns `Ok(())` on a clean, close-driven shutdown. It never returns
/// with the radio held, so the caller (`climate_task`) restores mesh unconditionally.
///
/// One session feeds BOTH the Climate and Energy screens: each poll refreshes the
/// shared [`ClimateState`] (from `GET /watch/climate/state`) and the shared
/// [`EnergyState`] (from `GET /watch/energy`). Commands flow only from the Climate
/// UI via `cmd_rx` (`POST .../set`).
///
/// - `stack`   — the (already-associated) embassy-net stack; WiFi must be up.
/// - `state`   — shared climate roster; rebuilt from each successful state poll.
/// - `energy`  — shared [`EnergyState`]; refreshed from each energy poll.
/// - `cmd_rx`  — UI -> session command queue (setpoint/mode; Climate only).
/// - `close`   — fire to request shutdown.
pub async fn run_climate_session(
    stack: Stack<'static>,
    state: &'static ClimateStateMutex,
    energy: &'static EnergyStateMutex,
    cmd_rx: ClimateCmdReceiver,
    close: &'static CloseSignal,
) -> Result<(), Error> {
    let (addr, port) = parse_hostport(HA_HTTP).unwrap_or((default_ha_addr(), HA_PORT));

    // All per-request buffers live on the heap (not `.bss`) and are reused across
    // polls. A fresh TcpSocket borrows them for each request, dropped before the
    // next — stateless request/response, one buffer set (no churn).
    let mut rx = vec_zeros(RX_RING);
    let mut tx = vec_zeros(TX_RING);
    let mut resp = vec_zeros(RESP_CAP);

    println!("[CLIM] http session up ({addr}:{port})");

    // Poll immediately on open (state appears as soon as the screen is up).
    let mut next_poll = Instant::now();

    loop {
        match select3(cmd_rx.receive(), close.wait(), Timer::at(next_poll)).await {
            // --- outbound command from the Climate UI ---
            Either3::First(cmd) => {
                if let Err(e) = post_command(stack, addr, port, &cmd, &mut rx, &mut tx, &mut resp).await
                {
                    println!("[CLIM] set failed: {e}");
                }
                // Reconcile promptly: pull authoritative state on the next tick.
                next_poll = Instant::now();
            }

            // --- screen closed: clean return (caller restores mesh) ---
            Either3::Second(()) => {
                println!("[CLIM] http session closed");
                return Ok(());
            }

            // --- poll tick: refresh climate + energy ---
            Either3::Third(()) => {
                poll_climate(stack, addr, port, state, &mut rx, &mut tx, &mut resp).await;
                poll_energy(stack, addr, port, energy, &mut rx, &mut tx, &mut resp).await;
                next_poll = Instant::now() + POLL_INTERVAL;
            }
        }
    }
}

// --- climate state poll -------------------------------------------------------

/// `GET /watch/climate/state`, rebuild the shared [`ClimateState`] from the JSON
/// array. On any failure the last-known state is kept (transient, retried next tick).
async fn poll_climate(
    stack: Stack<'static>,
    addr: Ipv4Address,
    port: u16,
    state: &ClimateStateMutex,
    rx: &mut [u8],
    tx: &mut [u8],
    resp: &mut [u8],
) {
    let head = format!(
        "GET /watch/climate/state HTTP/1.1\r\n\
         Host: {addr}:{port}\r\n{token}\
         Connection: close\r\n\r\n",
        token = token_header(),
    );
    let range = match http_exchange(stack, addr, port, head.as_bytes(), None, rx, tx, resp).await {
        Ok(r) => r,
        Err(e) => {
            println!("[CLIM] state poll failed: {e}");
            return;
        }
    };
    let body = &resp[range.0..range.1];

    // Rebuild under a single lock: the UI reads the same lock, so it never observes
    // the intermediate empty state — this both refreshes existing entities AND
    // prunes any the component no longer reports (`[]` legitimately clears).
    let mut guard = state.lock().await;
    guard.entities.clear();
    let mut i = 0usize;
    while let Some((s, e, next)) = next_object(body, i) {
        let obj = &body[s..e];
        if let Some(id) = json_string_field(obj, "id") {
            if let Some(entity) = climate_model::parse_state(obj) {
                guard.upsert(id, entity);
            }
        }
        i = next;
    }
}

// --- energy poll --------------------------------------------------------------

/// `GET /watch/energy`. Sets [`EnergyState::online`] from success/failure and
/// refreshes the numeric fields when the body parses.
async fn poll_energy(
    stack: Stack<'static>,
    addr: Ipv4Address,
    port: u16,
    energy: &EnergyStateMutex,
    rx: &mut [u8],
    tx: &mut [u8],
    resp: &mut [u8],
) {
    let head = format!(
        "GET /watch/energy HTTP/1.1\r\n\
         Host: {addr}:{port}\r\n{token}\
         Connection: close\r\n\r\n",
        token = token_header(),
    );
    match http_exchange(stack, addr, port, head.as_bytes(), None, rx, tx, resp).await {
        Ok((bs, len)) => {
            let mut guard = energy.lock().await;
            guard.online = true; // reachable
            if let Some(next) = parse_energy(&resp[bs..len]) {
                // Preserve `online` (set above); take the numeric fields.
                guard.battery_pct = next.battery_pct;
                guard.solar_w = next.solar_w;
                guard.grid_w = next.grid_w;
                guard.charging = next.charging;
            }
            // parse_energy == None (all-null/malformed) -> keep last numeric values,
            // but online stays true so the UI shows "connecting", not "unreachable".
        }
        Err(_) => {
            energy.lock().await.online = false;
        }
    }
}

// --- command POST -------------------------------------------------------------

async fn post_command(
    stack: Stack<'static>,
    addr: Ipv4Address,
    port: u16,
    cmd: &ClimateCmd,
    rx: &mut [u8],
    tx: &mut [u8],
    resp: &mut [u8],
) -> Result<(), Error> {
    let mut path: String<PATH_CAP> = String::new();
    path.push_str("/watch/climate/").map_err(|_| "cmd path too long")?;
    path.push_str(cmd.obj()).map_err(|_| "cmd path too long")?;
    path.push_str("/set").map_err(|_| "cmd path too long")?;

    let payload = match cmd {
        ClimateCmd::SetTemp { temp, .. } => climate_model::encode_set_temp(*temp),
        ClimateCmd::SetMode { mode, .. } => climate_model::encode_set_mode(*mode),
    };

    let head = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {addr}:{port}\r\n{token}\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        token = token_header(),
        len = payload.len(),
    );
    // We only need the 200 (optimistic UI reconciles on the next state poll); the
    // response body is read to completion and discarded.
    http_exchange(stack, addr, port, head.as_bytes(), Some(payload.as_bytes()), rx, tx, resp)
        .await
        .map(|_| ())
}

// --- HTTP mechanics (mirrors ota_http / voice_stt) ----------------------------

/// Connect, send `head` (+ optional `body`), read the whole response into `resp`,
/// verify `200`, return the body byte-range `[start, end)` within `resp`. The
/// socket is aborted on return. Bounded by [`REQ_TIMEOUT`].
async fn http_exchange(
    stack: Stack<'static>,
    addr: Ipv4Address,
    port: u16,
    head: &[u8],
    body: Option<&[u8]>,
    rx: &mut [u8],
    tx: &mut [u8],
    resp: &mut [u8],
) -> Result<(usize, usize), Error> {
    match with_timeout(REQ_TIMEOUT, async {
        let mut socket = TcpSocket::new(stack, rx, tx);
        socket.set_timeout(Some(SOCKET_TIMEOUT));
        socket.connect((addr, port)).await.map_err(|_| "connect failed")?;
        write_all(&mut socket, head).await?;
        if let Some(b) = body {
            write_all(&mut socket, b).await?;
        }
        let range = recv_response(&mut socket, resp).await;
        socket.abort();
        range
    })
    .await
    {
        Ok(r) => r,
        Err(_) => Err("request timeout"),
    }
}

/// Read the full HTTP response into `resp` (headers via CRLFCRLF, then body until
/// the server closes the `Connection: close` socket). Returns the body range.
async fn recv_response(socket: &mut TcpSocket<'_>, resp: &mut [u8]) -> Result<(usize, usize), Error> {
    let mut len = 0usize;
    let body_start = loop {
        if len == resp.len() {
            return Err("response too large");
        }
        let n = socket.read(&mut resp[len..]).await.map_err(|_| "socket read failed")?;
        if n == 0 {
            return Err("connection closed in headers");
        }
        len += n;
        if let Some(pos) = find(&resp[..len], b"\r\n\r\n") {
            break pos + 4;
        }
    };
    check_status_200(&resp[..body_start])?;

    // Body: read until EOF (server closes) or the buffer fills.
    loop {
        if len == resp.len() {
            break;
        }
        let n = socket.read(&mut resp[len..]).await.map_err(|_| "socket read failed")?;
        if n == 0 {
            break;
        }
        len += n;
    }
    Ok((body_start, len))
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
        return Err("http status not 200");
    }
    Ok(())
}

/// `X-Watch-Token` header line (with trailing CRLF) or empty if no token is baked.
fn token_header() -> heapless::String<128> {
    let mut s: heapless::String<128> = heapless::String::new();
    if let Some(t) = HA_TOKEN {
        let _ = s.push_str("X-Watch-Token: ");
        let _ = s.push_str(t);
        let _ = s.push_str("\r\n");
    }
    s
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

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Heap-allocated zeroed buffer (avoids `.bss`; see [`RESP_CAP`] rationale).
fn vec_zeros(n: usize) -> alloc::vec::Vec<u8> {
    let mut v: AllocVec<u8> = AllocVec::new();
    v.resize(n, 0);
    v
}

// --- JSON array/object splitting (untrusted, bounded, panic-free) -------------

/// Find the next top-level `{...}` object in `body` starting at `from`. Returns
/// `(start, end, next)` where `body[start..end]` is the object (braces included)
/// and `next` is the index to resume from. String-aware (braces inside strings
/// don't count) and bounded by `body.len()` — never panics, never over-reads.
fn next_object(body: &[u8], from: usize) -> Option<(usize, usize, usize)> {
    let n = body.len();
    let mut i = from;
    // Seek the opening brace.
    while i < n && body[i] != b'{' {
        i += 1;
    }
    if i >= n {
        return None;
    }
    let start = i;
    let mut depth = 0u32;
    let mut in_str = false;
    let mut escaped = false;
    while i < n {
        let c = body[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((start, i + 1, i + 1));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None // unbalanced -> no more objects
}

/// Extract a top-level string value for `"<key>"` from a JSON object slice, as a
/// `&str` into `obj`. Only used for the flat `"id"` (HA object_id: `[a-z0-9_]`), so
/// no escape decoding is needed. Bounded, UTF-8 checked, panic-free.
fn json_string_field<'a>(obj: &'a [u8], key: &str) -> Option<&'a str> {
    let s = core::str::from_utf8(obj).ok()?;
    // Build the needle `"key"` on the stack (keys are short).
    let mut needle: String<24> = String::new();
    needle.push('"').ok()?;
    needle.push_str(key).ok()?;
    needle.push('"').ok()?;
    let mut rest = s;
    // Scan each occurrence; require a `:` (a quoted string) after it, so a value
    // that merely contains the key text isn't mistaken for the key.
    while let Some(idx) = rest.find(needle.as_str()) {
        let after = rest[idx + needle.len()..].trim_start();
        if let Some(after_colon) = after.strip_prefix(':') {
            let v = after_colon.trim_start();
            if let Some(open) = v.strip_prefix('"') {
                if let Some(endq) = open.find('"') {
                    return Some(&open[..endq]);
                }
            }
            return None; // key present but value isn't a plain string
        }
        rest = &rest[idx + needle.len()..];
    }
    None
}

// --- energy payload parsing (contract preserved from the MQTT path) -----------

/// Parse a `GET /watch/energy` JSON body into an [`EnergyState`] (numeric fields
/// only; `online` is owned by the caller). Bounded, panic-free, `null`-tolerant.
/// Returns `None` on empty / non-UTF-8 / no recognizable numeric field.
pub fn parse_energy(bytes: &[u8]) -> Option<EnergyState> {
    if bytes.is_empty() {
        return None;
    }
    let s = core::str::from_utf8(bytes).ok()?;

    let battery_pct = json_num(s, "battery_pct").map(|v| v.clamp(0.0, 100.0) as u8);
    let solar_w = json_num(s, "solar_w").map(|v| v as i32);
    let grid_w = json_num(s, "grid_w").map(|v| v as i32);
    let charging = json_bool(s, "charging").unwrap_or(false);

    if battery_pct.is_none() && solar_w.is_none() && grid_w.is_none() {
        return None;
    }

    Some(EnergyState {
        battery_pct,
        solar_w,
        grid_w,
        charging,
        online: false, // caller sets the real reachability flag
    })
}

/// Slice starting just after `"<key>":` (whitespace-trimmed). Bounded key buffer.
fn json_value_after<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let mut needle: String<40> = String::new();
    needle.push('"').ok()?;
    needle.push_str(key).ok()?;
    needle.push('"').ok()?;
    let idx = s.find(needle.as_str())?;
    let after_key = &s[idx + needle.len()..];
    let colon = after_key.find(':')?;
    Some(after_key[colon + 1..].trim_start())
}

/// Extract a numeric value for `"<key>":<number>` (int/float, signed, exponent).
/// Explicit JSON `null` -> `None`. Bounded, panic-free.
fn json_num(s: &str, key: &str) -> Option<f32> {
    let after = json_value_after(s, key)?;
    if after.starts_with("null") {
        return None;
    }
    let end = after
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
        .unwrap_or(after.len());
    after[..end].parse::<f32>().ok()
}

/// Extract a boolean value for `"<key>":true|false`. `null`/anything else -> `None`.
fn json_bool(s: &str, key: &str) -> Option<bool> {
    let after = json_value_after(s, key)?;
    if after.starts_with("true") {
        Some(true)
    } else if after.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

// --- host/port parse ("a.b.c.d:port") -----------------------------------------

/// Parse "a.b.c.d:port" -> (addr, port). IPv4 only, no DNS (parity with
/// `mqtt_ha::parse_broker` / `ota_http::parse_url`).
fn parse_hostport(s: &str) -> Option<(Ipv4Address, u16)> {
    let (ip_str, port_str) = s.split_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let mut octets = [0u8; 4];
    let mut parts = ip_str.split('.');
    for octet in &mut octets {
        *octet = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((
        Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]),
        port,
    ))
}
