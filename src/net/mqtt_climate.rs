//! Bidirectional MQTT 3.1.1 session for Home Assistant climate control.
//!
//! Companion to [`crate::net::mqtt_ha`] (which stays a fire-and-forget publish
//! burst for telemetry). This module holds an **open, long-lived** session for
//! as long as the Climate screen is up: it SUBSCRIBEs to the bridge's state +
//! roster topics, reacts to inbound retained/state PUBLISHes by upserting a
//! shared [`ClimateState`], PUBLISHes setpoint/mode commands the UI queues, and
//! keeps the link alive with PINGREQ. Hand-rolled for the same reasons as
//! `mqtt_ha` (no crate wants the watch's short-radio-window model).
//!
//! ## Reuse
//! The low-level MQTT framing primitives live in [`mqtt_ha`] and are reused
//! `pub(crate)` here — [`mqtt_ha::build_connect`] (parameterised with a client
//! id), [`mqtt_ha::publish`] (QoS-0 encoder), [`mqtt_ha::push`],
//! [`mqtt_ha::push_str`], [`mqtt_ha::push_remaining_len`],
//! [`mqtt_ha::write_all`], [`mqtt_ha::read_exact`], [`mqtt_ha::parse_broker`],
//! and the broker/creds consts. Only the **inbound** direction (remaining-length
//! varint *decode*, PUBLISH topic/payload split, SUBACK/PINGRESP handling) and
//! the async session lifecycle are new.
//!
//! ## Untrusted input
//! Every inbound broker frame is treated as untrusted network input: the
//! remaining-length varint is bounded to 4 bytes, the body is bounded to
//! [`INBOUND_CAP`], and PUBLISH topic/payload splits are checked-slice only —
//! a malformed frame ends the session cleanly (caller restores mesh); it never
//! panics or over-reads.
//!
//! ## Dependency on `climate-model` (spec §B′)
//! Parsing/encoding is delegated to the pure `climate-model` crate. Until it
//! merges, [`crate::net::climate_model_stub`] stands in via the alias below.
//! The exact surface this module calls:
//!   - `parse_state(&[u8]) -> Option<ClimateEntity>`
//!   - `ClimateState::upsert(&mut self, obj: &str, entity: ClimateEntity)`
//!   - `encode_set_temp(f32) -> heapless::String<_>`
//!   - `encode_set_mode(HvacMode) -> heapless::String<_>`
//!   - `HvacMode` (Copy enum, carried in [`ClimateCmd`])
//! If the real crate's signatures drift from these, coordinate via team-lead.
//!
//! ## Integration (main.rs, sequenced later — NOT wired here)
//! The integrator allocates three `'static`s (StaticCell) and spawns the
//! session as an embassy task or drives it from the Climate-screen branch:
//! ```ignore
//! static CLIMATE_STATE: StaticCell<ClimateStateMutex> = StaticCell::new();
//! static CLIMATE_CMDS:  StaticCell<ClimateCmdChannel> = StaticCell::new();
//! static CLIMATE_CLOSE: StaticCell<CloseSignal>       = StaticCell::new();
//! // ... init, then:
//! let res = run_climate_session(stack, state, cmds.receiver(), close).await;
//! // on return (Ok or Err) main.rs restores RadioMode -> mesh (never stranded).
//! ```

// Real crate on integration: `use climate_model;` (see module docs / stub).
use crate::net::climate_model_stub as climate_model;
use crate::net::mqtt_ha::{
    build_connect, parse_broker, publish, push, push_remaining_len, push_str, read_exact,
    write_all, BROKER, PKT_CAP,
};
use climate_model::{ClimateState, HvacMode};

use embassy_futures::select::{select4, Either4};
use embassy_net::{tcp::TcpSocket, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_println::println;
use heapless::{String, Vec};

// --- public types the main.rs integrator allocates + matches ----------------

/// Errors are `&'static str` for parity with [`mqtt_ha`] / [`crate::net::ota_http`].
pub type Error = &'static str;

/// Max object-id length (topic component `watch/climate/<id>/...`).
pub const OBJ_ID_CAP: usize = 48;
/// Object id carried in a queued command (a bounded, heapless topic component).
pub type ObjId = String<OBJ_ID_CAP>;

/// Command-queue depth (UI → session). Debounced taps, so shallow is fine.
pub const CMD_QUEUE_DEPTH: usize = 4;

/// Shared climate state — session writes (upsert), UI reads (build cards).
/// Async [`Mutex`]: both sides `.lock().await`. Raw kind matches the rest of
/// the firmware ([`CriticalSectionRawMutex`], see `mic_capture`).
pub type ClimateStateMutex = Mutex<CriticalSectionRawMutex, ClimateState>;

/// UI → session command channel and its endpoints.
pub type ClimateCmdChannel = Channel<CriticalSectionRawMutex, ClimateCmd, CMD_QUEUE_DEPTH>;
pub type ClimateCmdReceiver = Receiver<'static, CriticalSectionRawMutex, ClimateCmd, CMD_QUEUE_DEPTH>;
pub type ClimateCmdSender = Sender<'static, CriticalSectionRawMutex, ClimateCmd, CMD_QUEUE_DEPTH>;

/// Screen-close signal — fire it to end the session with a clean DISCONNECT.
pub type CloseSignal = Signal<CriticalSectionRawMutex, ()>;

/// A command the UI queues for the session to PUBLISH to
/// `watch/climate/<obj>/set`. `HvacMode` is `climate-model`'s own enum (no
/// parallel type — carried straight through to `encode_set_mode`).
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

// --- session tunables -------------------------------------------------------

/// Distinct from `mqtt_ha`'s telemetry client id so the broker never kicks the
/// telemetry connection if the two ever briefly overlap (MQTT: same client id =
/// the newer connection evicts the older).
const CLIMATE_CLIENT_ID: &str = "smolwatch042-clim";

/// Send PINGREQ this often while idle (< the 30s keepalive `mqtt_ha` bakes into
/// the CONNECT). Reset on every packet we *send* (command or ping), so a busy
/// command stream never adds redundant pings, and a receive-only period still
/// pings on schedule (keepalive is about *our* outbound traffic).
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// If nothing is received (incl. PINGRESP) within this window, declare the
/// broker dead and end the session (caller shows "reconnecting…", keeps mesh).
const DEAD_TIMEOUT: Duration = Duration::from_secs(35);
/// Idle timeout for the connect + CONNACK + SUBSCRIBE + SUBACK handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Once a frame's type byte arrives, its remainder must land within this — a
/// broker that dribbles half a frame can't stall the session.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest inbound frame body we buffer. A state payload is ~150 B and the
/// roster ~200 B; anything larger is treated as a protocol error (bounded,
/// no over-read) and ends the session.
const INBOUND_CAP: usize = 1024;

// Topics (kept as plain consts — the bridge contract from spec §A/§B).
const STATE_WILDCARD: &str = "watch/climate/+/state";
const ROSTER_TOPIC: &str = "watch/climate/roster";
const STATE_PREFIX: &str = "watch/climate/";
const STATE_SUFFIX: &str = "/state";
const SET_PREFIX: &str = "watch/climate/";
const SET_SUFFIX: &str = "/set";
/// `SET_PREFIX` + max obj id + `SET_SUFFIX`, rounded up.
const TOPIC_CAP: usize = 96;

// --- public entry point -----------------------------------------------------

/// Run one bidirectional climate session until [`close`] fires or an error /
/// broker drop occurs. Returns `Ok(())` on a clean, close-driven DISCONNECT;
/// `Err` on any connect/protocol/link failure. Either way the socket is closed
/// on return, so the caller is free to restore the radio to mesh — the mesh is
/// never stranded by this function (structural guarantee: no early return
/// leaves the radio held).
///
/// - `stack`   — the (already-associated) embassy-net stack; WiFi must be up.
/// - `state`   — shared roster the session upserts as state PUBLISHes arrive.
/// - `cmd_rx`  — UI → session command queue (setpoint/mode).
/// - `close`   — fire to request a clean session shutdown.
pub async fn run_climate_session(
    stack: Stack<'static>,
    state: &'static ClimateStateMutex,
    cmd_rx: ClimateCmdReceiver,
    close: &'static CloseSignal,
) -> Result<(), Error> {
    let (ip, port) = parse_broker(BROKER).ok_or("bad MQTT_BROKER (want ip:port)")?;

    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    // Bound the whole handshake; cleared before the idle loop (see below).
    socket.set_timeout(Some(HANDSHAKE_TIMEOUT));

    socket.connect((ip, port)).await.map_err(|_| "tcp connect")?;

    // CONNECT (clean session, keepalive 30s) -> CONNACK. Reuses mqtt_ha's
    // builder with a climate-specific client id.
    let mut pkt: Vec<u8, PKT_CAP> = Vec::new();
    build_connect(&mut pkt, CLIMATE_CLIENT_ID)?;
    write_all(&mut socket, &pkt).await?;

    let mut ack = [0u8; 4];
    read_exact(&mut socket, &mut ack).await?;
    if ack[0] != 0x20 || ack[1] != 0x02 {
        return Err("bad CONNACK");
    }
    if ack[3] != 0x00 {
        return Err("broker refused connection (check MQTT_USER/MQTT_PASS)");
    }

    // SUBSCRIBE watch/climate/+/state + watch/climate/roster (QoS 0) -> SUBACK.
    subscribe(&mut socket).await?;
    println!("[CLIM] session up (subscribed)");

    // Persistent phase: drop the handshake idle timeout so idle awaits (waiting
    // for the next state change) don't abort. From here, deadlines are explicit
    // per-phase (FRAME_TIMEOUT on a mid-frame, DEAD_TIMEOUT via keepalive).
    socket.set_timeout(None);

    let mut inbuf = [0u8; INBOUND_CAP];
    let mut last_rx = Instant::now();
    let mut next_ping = Instant::now() + PING_INTERVAL;

    loop {
        match select4(
            read_type_byte(&mut socket),
            cmd_rx.receive(),
            close.wait(),
            Timer::at(next_ping),
        )
        .await
        {
            // --- inbound frame: only the 1-byte type read is cancellable here.
            // TcpSocket::read is cancel-safe (drop before Ready consumes nothing),
            // and the rest of the frame is read to completion outside the select,
            // so a cancelled arm can never leave a half-consumed frame. ---
            Either4::First(res) => {
                let type_byte = res?;
                last_rx = Instant::now();
                let n = match with_timeout(FRAME_TIMEOUT, read_frame_body(&mut socket, &mut inbuf))
                    .await
                {
                    Ok(r) => r?,
                    Err(_) => return Err("frame read timeout"),
                };
                match type_byte & 0xF0 {
                    0x30 => handle_publish(type_byte, &inbuf[..n], state).await,
                    0xD0 => {} // PINGRESP — last_rx already refreshed above
                    _ => {}    // unexpected control packet — ignore
                }
            }

            // --- outbound command from the UI ---
            Either4::Second(cmd) => {
                send_command(&mut socket, &cmd).await?;
                next_ping = Instant::now() + PING_INTERVAL; // we just sent
            }

            // --- screen closed: clean DISCONNECT ---
            Either4::Third(()) => {
                let _ = write_all(&mut socket, &[0xE0, 0x00]).await; // DISCONNECT
                let _ = socket.flush().await;
                socket.close();
                println!("[CLIM] session closed");
                return Ok(());
            }

            // --- keepalive tick ---
            Either4::Fourth(()) => {
                if Instant::now() - last_rx > DEAD_TIMEOUT {
                    return Err("keepalive timeout (broker silent)");
                }
                write_all(&mut socket, &[0xC0, 0x00]).await?; // PINGREQ
                next_ping = Instant::now() + PING_INTERVAL;
            }
        }
    }
}

// --- SUBSCRIBE + SUBACK -----------------------------------------------------

async fn subscribe(socket: &mut TcpSocket<'_>) -> Result<(), Error> {
    let topics = [STATE_WILDCARD, ROSTER_TOPIC];

    // remaining length = 2 (packet id) + sum(2-byte len + topic + 1-byte QoS)
    let mut remaining = 2usize;
    for t in topics {
        remaining += 2 + t.len() + 1;
    }

    let mut pkt: Vec<u8, PKT_CAP> = Vec::new();
    push(&mut pkt, &[0x82])?; // SUBSCRIBE, reserved flags 0b0010
    push_remaining_len(&mut pkt, remaining)?;
    push(&mut pkt, &[0x00, 0x01])?; // packet identifier = 1
    for t in topics {
        push_str(&mut pkt, t)?;
        push(&mut pkt, &[0x00])?; // requested QoS 0
    }
    write_all(socket, &pkt).await?;

    // SUBACK: 0x90 | rem-len | [packet id:2][return code per topic]
    let mut buf = [0u8; 16];
    let (ty, n) = read_frame(socket, &mut buf).await?;
    if ty & 0xF0 != 0x90 {
        return Err("bad SUBACK");
    }
    if n < 2 + topics.len() {
        return Err("short SUBACK");
    }
    for &rc in &buf[2..n] {
        if rc == 0x80 {
            return Err("subscribe rejected by broker");
        }
    }
    Ok(())
}

// --- inbound PUBLISH handling ----------------------------------------------

/// Split a PUBLISH body into topic + payload and route it. `type_byte` carries
/// the QoS bits (we subscribe QoS 0, but a QoS>0 delivery is handled defensively
/// by skipping its 2-byte packet id). All slicing is checked — a malformed frame
/// is silently skipped, never a panic.
async fn handle_publish(type_byte: u8, body: &[u8], state: &ClimateStateMutex) {
    if body.len() < 2 {
        return;
    }
    let topic_len = ((body[0] as usize) << 8) | body[1] as usize;
    let mut idx = 2 + topic_len;
    if idx > body.len() {
        return; // topic overruns frame — malformed, skip
    }
    let topic = &body[2..idx];

    let qos = (type_byte >> 1) & 0x03;
    if qos > 0 {
        if idx + 2 > body.len() {
            return; // no room for the packet id — malformed, skip
        }
        idx += 2; // skip packet identifier
    }
    let payload = &body[idx..];

    match classify_topic(topic) {
        Some(TopicKind::State(obj)) => {
            if let Some(entity) = climate_model::parse_state(payload) {
                let mut guard = state.lock().await;
                guard.upsert(obj, entity);
            }
            // parse_state == None (malformed / empty retained-clear) -> skip.
        }
        Some(TopicKind::Roster) => {
            // Belt-and-suspenders per spec §A: the wildcard state subscription
            // is authoritative for what renders, so the roster is informational
            // in v1. Kept as an explicit branch (subscribed + drained, never
            // choked on) for a future roster-diff prune.
        }
        None => {} // not one of our topics — ignore
    }
}

enum TopicKind<'a> {
    State(&'a str),
    Roster,
}

/// Classify an inbound topic. Bounded, UTF-8 checked, no panic.
fn classify_topic(topic: &[u8]) -> Option<TopicKind<'_>> {
    let t = core::str::from_utf8(topic).ok()?;
    if t == ROSTER_TOPIC {
        return Some(TopicKind::Roster);
    }
    let mid = t.strip_prefix(STATE_PREFIX)?.strip_suffix(STATE_SUFFIX)?;
    if mid.is_empty() || mid.contains('/') {
        return None; // "+" matches exactly one level
    }
    Some(TopicKind::State(mid))
}

// --- outbound command PUBLISH ----------------------------------------------

async fn send_command(socket: &mut TcpSocket<'_>, cmd: &ClimateCmd) -> Result<(), Error> {
    let mut topic: String<TOPIC_CAP> = String::new();
    topic.push_str(SET_PREFIX).map_err(|_| "cmd topic too long")?;
    topic.push_str(cmd.obj()).map_err(|_| "cmd topic too long")?;
    topic.push_str(SET_SUFFIX).map_err(|_| "cmd topic too long")?;

    match cmd {
        ClimateCmd::SetTemp { temp, .. } => {
            let payload = climate_model::encode_set_temp(*temp);
            publish(socket, &topic, payload.as_bytes(), false).await
        }
        ClimateCmd::SetMode { mode, .. } => {
            let payload = climate_model::encode_set_mode(*mode);
            publish(socket, &topic, payload.as_bytes(), false).await
        }
    }
}

// --- inbound frame reading (new; mqtt_ha has no decode path) ----------------

/// Read exactly one MQTT fixed-header type byte. A single `socket.read` →
/// cancel-safe as a `select` arm (drop before Ready consumes nothing).
async fn read_type_byte(socket: &mut TcpSocket<'_>) -> Result<u8, Error> {
    let mut b = [0u8; 1];
    read_exact(socket, &mut b).await?;
    Ok(b[0])
}

/// Read the remaining-length varint + body (type byte already consumed). Bounded
/// to `buf.len()`; an over-large frame is a protocol error, not an over-read.
async fn read_frame_body(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, Error> {
    let rem = read_remaining_len(socket).await?;
    if rem > buf.len() {
        return Err("inbound frame too large");
    }
    read_exact(socket, &mut buf[..rem]).await?;
    Ok(rem)
}

/// Read a whole frame (type byte + remaining-length + body). Used for the
/// handshake replies; the main loop splits type-byte / body so only the 1-byte
/// read is cancellable.
async fn read_frame(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(u8, usize), Error> {
    let ty = read_type_byte(socket).await?;
    let n = read_frame_body(socket, buf).await?;
    Ok((ty, n))
}

/// Decode the MQTT "remaining length" varint (1..=4 bytes, MSB = continue).
/// Bounded to 4 bytes so a malformed stream can't spin — untrusted input.
async fn read_remaining_len(socket: &mut TcpSocket<'_>) -> Result<usize, Error> {
    let mut value: usize = 0;
    let mut mult: usize = 1;
    for _ in 0..4 {
        let mut b = [0u8; 1];
        read_exact(socket, &mut b).await?;
        value += (b[0] & 0x7F) as usize * mult;
        if b[0] & 0x80 == 0 {
            return Ok(value);
        }
        mult *= 128;
    }
    Err("malformed remaining length")
}
