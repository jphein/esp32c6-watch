//! Bidirectional MQTT 3.1.1 session for Home Assistant climate + energy +
//! room lights (#39).
//!
//! Companion to [`crate::net::mqtt_ha`] (which stays a fire-and-forget publish
//! burst for telemetry). This module holds an **open, long-lived** session for
//! as long as the Climate or Energy screen is up: it SUBSCRIBEs to the bridge's
//! climate state + roster topics AND the retained `watch/energy/state` snapshot,
//! reacts to inbound PUBLISHes by upserting a shared [`ClimateState`] /
//! replacing a shared [`EnergyState`], PUBLISHes setpoint/mode commands the
//! Climate UI queues, and keeps the link alive with PINGREQ. One CONNECT +
//! keepalive feeds both screens (the "same bidirectional channel"). Energy is
//! consume-only. Hand-rolled for the same reasons as `mqtt_ha` (no crate wants
//! the watch's short-radio-window model).
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
//! Parsing/encoding is delegated to the pure `climate-model` crate (now merged;
//! the earlier `climate_model_stub` stand-in has been removed). The exact
//! surface this module calls:
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
//! static ENERGY_STATE:  StaticCell<EnergyStateMutex>  = StaticCell::new();
//! static CLIMATE_CMDS:  StaticCell<ClimateCmdChannel> = StaticCell::new();
//! static CLIMATE_CLOSE: StaticCell<CloseSignal>       = StaticCell::new();
//! // ... init, then (one session feeds both the Climate and Energy screens):
//! let res = run_climate_session(stack, state, energy, cmds.receiver(), close).await;
//! // on return (Ok or Err) main.rs restores RadioMode -> mesh (never stranded).
//! ```

// Real crate on integration: `use climate_model;` (see module docs / stub).
use climate_model;
use crate::net::mqtt_ha::{
    build_connect, parse_broker, publish, push, push_remaining_len, push_str, read_exact,
    write_all, BROKER, PKT_CAP,
};
use climate_model::{ClimateState, HvacMode};
use embassy_sync::lazy_lock::LazyLock;

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

/// Fires after every ACCEPTED inbound state update (climate upsert, energy
/// replace, avail flip, lights frame). The main loop selects on it so a state
/// arrival repaints on the NEXT executor pass instead of waiting out the idle
/// tick (up to 1s on the Lights/Climate screens) — the press→render round trip
/// loses its biggest firmware-side term. Coalescing by design (Signal, not a
/// channel): a burst of retained frames on subscribe wakes one repaint.
pub static STATE_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Session phase for UI gating: 0 = down (incl. reconnect backoff) · 1 = TCP/
/// handshake in flight · 2 = up (SUBACK'd). Main gates command *acceptance* on
/// this so a press during a broker outage is rejected with visible feedback
/// instead of silently queueing and replaying a stale toggle at the next
/// reconnect (the "lights flip on their own seconds later" bug class).
pub static SESSION_PHASE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

pub const PHASE_DOWN: u8 = 0;
pub const PHASE_CONNECTING: u8 = 1;
pub const PHASE_UP: u8 = 2;

/// Live HA energy snapshot consumed from retained `watch/energy/state` (v0.4.1).
/// Small, `Copy`, behind the same [`Mutex`] pattern as [`ClimateState`]. Numeric
/// fields are `Option` so the UI can distinguish "no data yet" from a real 0.
///
/// Contract confirmed against luna-website's `feat/energy-live @ 05a8be1`:
/// keys `battery_pct` / `solar_w` / `grid_w` / `charging`, full retained state
/// per frame (not deltas → wholesale replace), `grid_w` >0 import / <0 export.
/// `online` is driven by the separate retained LWT `watch/energy/avail`
/// (`online`|`offline`) and is **preserved across state-frame replaces** — the
/// UI uses `!online` to show "HA unreachable" (conn-state = 2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnergyState {
    /// Home battery state of charge, 0..=100 %.
    pub battery_pct: Option<u8>,
    /// Solar/PV production, watts (≥ 0).
    pub solar_w: Option<i32>,
    /// Grid flow, watts, **signed: + = importing, − = exporting**.
    pub grid_w: Option<i32>,
    /// Battery is charging.
    pub charging: bool,
    /// HA/bridge reachable per the `watch/energy/avail` LWT. `false` → the UI
    /// shows "HA unreachable" (conn-state = 2) over the last-known values.
    pub online: bool,
    /// True once ANY `watch/energy/avail` frame has been received this boot.
    /// Distinguishes "the bridge said offline" (show *HA unreachable*) from
    /// "no avail topic exists at all" (bridge never deployed / LWT retained
    /// message lost) — in the latter case live data should still render
    /// instead of a false-negative "HA unreachable" (#energy-conn-gate).
    pub avail_seen: bool,
}

impl EnergyState {
    pub const fn new() -> Self {
        Self {
            battery_pct: None,
            solar_w: None,
            grid_w: None,
            charging: false,
            online: false,
            avail_seen: false,
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

/// Shared energy snapshot — session replaces it, the Energy screen reads it.
pub type EnergyStateMutex = Mutex<CriticalSectionRawMutex, EnergyState>;

// --- Lights (#39): room-aware light control --------------------------------

/// Max area-name length carried in a lights state frame ("Living Room" etc.).
/// Anything longer is truncated at a char boundary — display-only data.
pub const LIGHTS_AREA_CAP: usize = 32;

/// HA-reported room-resolution status from the retained
/// `watch/<sigil>/lights/state` payload (4th field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightsStatus {
    /// Presence resolved to an area; counts are live.
    Ok,
    /// No room presence — HA falls back to its default behavior on `toggle`.
    NoPresence,
    /// HA-side error (also the defensive mapping for an unknown status word).
    Error,
}

/// Live room-lights snapshot consumed from the RETAINED
/// `watch/<sigil>/lights/state` topic. Payload contract (HA side, #39):
/// `AREA|<area name>|<lights on>/<total>|<ok|no_presence|error>`
/// e.g. `AREA|Living Room|3/5|ok`. Same Mutex pattern as [`EnergyState`].
#[derive(Clone, Debug, PartialEq)]
pub struct LightsState {
    /// Resolved area name ("" until the first frame).
    pub area: String<LIGHTS_AREA_CAP>,
    /// Lights currently on in the area.
    pub on: u8,
    /// Total lights in the area.
    pub total: u8,
    /// Room-resolution status.
    pub status: LightsStatus,
    /// Bumped on EVERY accepted state frame (even an identical payload — HA
    /// republishes after acting on a command). The Lights UI's optimistic
    /// "sent" flash clears when this moves past the value seen at send time.
    /// 0 = nothing received yet.
    pub seq: u32,
}

impl LightsState {
    pub const fn new() -> Self {
        Self {
            area: String::new(),
            on: 0,
            total: 0,
            status: LightsStatus::Error,
            seq: 0,
        }
    }

    /// True once at least one state frame has landed (UI "finding your room…"
    /// vs live gate).
    pub fn has_data(&self) -> bool {
        self.seq > 0
    }
}

impl Default for LightsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared lights snapshot — session replaces it, the Lights screen reads it.
pub type LightsStateMutex = Mutex<CriticalSectionRawMutex, LightsState>;

/// A lights command the UI queues; published to `watch/<sigil>/lights/cmd`
/// (NOT retained). HA resolves which room's lights it applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightsAction {
    Toggle,
    On,
    Off,
}

impl LightsAction {
    pub const fn payload(self) -> &'static str {
        match self {
            LightsAction::Toggle => "toggle",
            LightsAction::On => "on",
            LightsAction::Off => "off",
        }
    }
}

/// A command the UI queues for the session to PUBLISH. Climate setpoint/mode
/// go to `watch/climate/<obj>/set` (`HvacMode` is `climate-model`'s own enum —
/// no parallel type); lights actions (#39) go to `watch/<sigil>/lights/cmd`
/// riding the SAME session/channel, so the Lights screen reuses the whole
/// connect/keepalive/backoff machinery.
pub enum ClimateCmd {
    SetTemp { obj: ObjId, temp: f32 },
    SetMode { obj: ObjId, mode: HvacMode },
    Lights(LightsAction),
}

// --- session tunables -------------------------------------------------------

/// Suffix appended to `mqtt_ha`'s per-device client id (`smolwatch-<sigil>`)
/// so the broker never kicks the telemetry connection if the two ever briefly
/// overlap (MQTT: same client id = the newer connection evicts the older).
/// Per-device (#34): the old fleet-shared "smolwatch042-clim" meant two
/// watches holding climate sessions evicted each other.
const CLIMATE_CLIENT_ID_SUFFIX: &str = "-clim";

/// Send PINGREQ this often while idle (< the 30s keepalive `mqtt_ha` bakes into
/// the CONNECT). Reset on every packet we *send* (command or ping), so a busy
/// command stream never adds redundant pings, and a receive-only period still
/// pings on schedule (keepalive is about *our* outbound traffic).
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// If nothing is received (incl. PINGRESP) within this window, declare the
/// broker dead and end the session (caller shows "reconnecting…", keeps mesh).
const DEAD_TIMEOUT: Duration = Duration::from_secs(35);
/// Idle timeout for the CONNACK + SUBSCRIBE + SUBACK handshake (post-connect).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// TCP-connect timeout, bounded tighter than the handshake. If the broker is on a
/// subnet the watch can't reach (roam VLAN firewalled off the broker's VLAN), the
/// SYN is silently dropped, so `connect` would otherwise block for the full
/// handshake window on the single-threaded executor every retry — a tight
/// reconnect storm that thrashes the radio. Fast-fail so the caller's backoff paces
/// retries and frees the radio for the mesh in between.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Once a frame's type byte arrives, its remainder must land within this — a
/// broker that dribbles half a frame can't stall the session.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Persistent-phase writes (PINGREQ, command publish, DISCONNECT) run after the
/// socket idle timeout is cleared — they must not be able to block forever. A
/// broker that completes the handshake then stops reading (TCP zero-window)
/// would otherwise wedge a write inside a `select4` arm, so we never return to
/// re-check `DEAD_TIMEOUT` and the WiFi radio is held → mesh stranded. Bounding
/// each write makes a stuck write error out → session returns → caller restores
/// mesh. (oracle-t10-review)
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest inbound frame body we buffer. A state payload is ~150 B and the
/// roster ~200 B; anything larger is treated as a protocol error (bounded,
/// no over-read) and ends the session.
const INBOUND_CAP: usize = 1024;

// Topics (kept as plain consts — the bridge contract from spec §A/§B).
const STATE_WILDCARD: &str = "watch/climate/+/state";
const ROSTER_TOPIC: &str = "watch/climate/roster";
/// Retained energy snapshot published by luna-website's HA energy bridge
/// (v0.4.1). Consume-only (no watch→HA energy commands).
const ENERGY_TOPIC: &str = "watch/energy/state";
/// Retained LWT availability for the energy bridge: `online` | `offline`.
/// Drives [`EnergyState::online`] → UI "HA unreachable" (conn-state = 2).
const ENERGY_AVAIL_TOPIC: &str = "watch/energy/avail";
const STATE_PREFIX: &str = "watch/climate/";
const STATE_SUFFIX: &str = "/state";
const SET_PREFIX: &str = "watch/climate/";
const SET_SUFFIX: &str = "/set";
/// `SET_PREFIX` + max obj id + `SET_SUFFIX`, rounded up.
const TOPIC_CAP: usize = 96;

// Lights (#39) topics are per-device (`watch/<sigil>/lights/...`, like the
// per-watch OTA topic) so two watches never shadow each other's rooms.
// Built ONCE (LazyLock over the cached sigil identity, same pattern as
// `sigil::IDENTITY`) — `classify_topic` runs per inbound PUBLISH and must not
// re-assemble strings.
/// `watch/` (6) + sigil (≤20) + `/lights/state` (13) = ≤39.
const LIGHTS_TOPIC_CAP: usize = 48;

struct LightsTopics {
    /// `watch/<sigil>/lights/cmd` — command publishes (NOT retained).
    cmd: String<LIGHTS_TOPIC_CAP>,
    /// `watch/<sigil>/lights/state` — retained state, subscribed.
    state: String<LIGHTS_TOPIC_CAP>,
}

static LIGHTS_TOPICS: LazyLock<LightsTopics> = LazyLock::new(|| {
    let build = |suffix: &str| {
        let mut t: String<LIGHTS_TOPIC_CAP> = String::new();
        // Infallible by the cap math above; bounded regardless.
        let _ = t.push_str("watch/");
        let _ = t.push_str(crate::net::sigil::get().sigil.as_str());
        let _ = t.push_str(suffix);
        t
    };
    LightsTopics {
        cmd: build("/lights/cmd"),
        state: build("/lights/state"),
    }
});

// Notifications (#32): fleet topic + a per-device one (the OTA announce /
// `watch/<sigil>/ota` pairing) so HA automations can target one wrist or all.
// Payload contract: `NOTIFY|<title>|<body>` — parsed in `notify::handle_mqtt`.
/// Fleet-wide notify topic.
const NOTIFY_TOPIC: &str = "watch/notify";
/// `watch/` (6) + sigil (≤20) + `/notify` (7) = ≤33.
const NOTIFY_TOPIC_CAP: usize = 40;
/// Notify topic pair (fleet, per-device) for subscribers: this session AND
/// the boot-burst window (`mqtt_ha::check_ota_announce`), so a RETAINED
/// notify is picked up on the next hourly NTP burst, not only when an HA
/// screen opens a session.
pub(crate) fn notify_topics() -> (&'static str, &'static str) {
    (NOTIFY_TOPIC, NOTIFY_DEVICE_TOPIC.get().as_str())
}

/// `watch/<sigil>/notify` — per-device, built once (classify_topic runs per
/// inbound PUBLISH and must not re-assemble strings).
static NOTIFY_DEVICE_TOPIC: LazyLock<String<NOTIFY_TOPIC_CAP>> = LazyLock::new(|| {
    let mut t: String<NOTIFY_TOPIC_CAP> = String::new();
    let _ = t.push_str("watch/");
    let _ = t.push_str(crate::net::sigil::get().sigil.as_str());
    let _ = t.push_str("/notify");
    t
});

// --- public entry point -----------------------------------------------------

/// Run one bidirectional climate session until [`close`] fires or an error /
/// broker drop occurs. Returns `Ok(())` on a clean, close-driven DISCONNECT;
/// `Err` on any connect/protocol/link failure. Either way the socket is closed
/// on return, so the caller is free to restore the radio to mesh — the mesh is
/// never stranded by this function (structural guarantee: no early return
/// leaves the radio held).
///
/// This is the unified HA consume+command session: it subscribes the climate
/// topics AND `watch/energy/state`, so whichever screen (Climate or Energy) is
/// open, both shared states stay live off one CONNECT/keepalive (the "same
/// bidirectional channel" the integrator was promised). Energy is consume-only
/// (no commands); commands only ever flow from the Climate UI via `cmd_rx`.
///
/// - `stack`   — the (already-associated) embassy-net stack; WiFi must be up.
/// - `state`   — shared climate roster; upserted as climate state PUBLISHes arrive.
/// - `energy`  — shared [`EnergyState`]; replaced as `watch/energy/state` arrives.
/// - `lights`  — shared [`LightsState`] (#39); replaced (seq-bumped) as the
///   retained `watch/<sigil>/lights/state` arrives.
/// - `cmd_rx`  — UI → session command queue (climate setpoint/mode + lights).
/// - `close`   — fire to request a clean session shutdown.
pub async fn run_climate_session(
    stack: Stack<'static>,
    state: &'static ClimateStateMutex,
    energy: &'static EnergyStateMutex,
    lights: &'static LightsStateMutex,
    cmd_rx: ClimateCmdReceiver,
    close: &'static CloseSignal,
) -> Result<(), Error> {
    let (ip, port) = parse_broker(BROKER).ok_or("bad MQTT_BROKER (want ip:port)")?;
    let t_start = Instant::now();
    SESSION_PHASE.store(PHASE_CONNECTING, core::sync::atomic::Ordering::Relaxed);
    // Drop commands queued during a PREVIOUS failed attempt / backoff window:
    // replaying a stale `toggle` seconds later flips lights the user isn't
    // expecting. Main gates sends on SESSION_PHASE, so the only survivors here
    // are the ones from the race window around a session death — drop them.
    while cmd_rx.try_receive().is_ok() {}

    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    // Bound the whole handshake; cleared before the idle loop (see below).
    socket.set_timeout(Some(HANDSHAKE_TIMEOUT));

    // Fast-fail the connect (CONNECT_TIMEOUT < HANDSHAKE_TIMEOUT) so an unreachable
    // broker aborts in ~2s instead of blocking the executor for the full handshake.
    match with_timeout(CONNECT_TIMEOUT, socket.connect((ip, port))).await {
        Ok(Ok(())) => {}
        _ => return Err("tcp connect"),
    }

    // CONNECT (clean session, keepalive 30s) -> CONNACK. Reuses mqtt_ha's
    // builder with a climate-specific per-device client id
    // ("smolwatch-<sigil>-clim", #34).
    let mut client_id: heapless::String<{ crate::net::mqtt_ha::CLIENT_ID_CAP }> =
        heapless::String::new();
    let _ = client_id.push_str(crate::net::mqtt_ha::CLIENT_ID_PREFIX);
    let _ = client_id.push_str(crate::net::sigil::get().sigil.as_str());
    let _ = client_id.push_str(CLIMATE_CLIENT_ID_SUFFIX);
    let mut pkt: Vec<u8, PKT_CAP> = Vec::new();
    build_connect(&mut pkt, client_id.as_str())?;
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
    SESSION_PHASE.store(PHASE_UP, core::sync::atomic::Ordering::Relaxed);
    println!(
        "[CLIM] session up (subscribed) [LAT] connect+handshake={}ms",
        (Instant::now() - t_start).as_millis()
    );

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
                    0x30 => handle_publish(type_byte, &inbuf[..n], state, energy, lights).await,
                    0xD0 => {} // PINGRESP — last_rx already refreshed above
                    _ => {}    // unexpected control packet — ignore
                }
            }

            // --- outbound command from the UI ---
            // Bounded write: a broker that stops reading (zero-window) must not
            // wedge this and strand the radio (oracle-t10-review).
            Either4::Second(cmd) => {
                match with_timeout(WRITE_TIMEOUT, send_command(&mut socket, &cmd)).await {
                    Ok(r) => r?,
                    Err(_) => return Err("command write timeout (broker not reading)"),
                }
                next_ping = Instant::now() + PING_INTERVAL; // we just sent
            }

            // --- screen closed: clean DISCONNECT (best-effort, never blocking) ---
            Either4::Third(()) => {
                let _ = with_timeout(WRITE_TIMEOUT, async {
                    let _ = write_all(&mut socket, &[0xE0, 0x00]).await; // DISCONNECT
                    let _ = socket.flush().await;
                })
                .await;
                socket.close();
                println!("[CLIM] session closed");
                return Ok(());
            }

            // --- keepalive tick ---
            Either4::Fourth(()) => {
                if Instant::now() - last_rx > DEAD_TIMEOUT {
                    return Err("keepalive timeout (broker silent)");
                }
                // Bounded PINGREQ write (see WRITE_TIMEOUT / oracle-t10-review).
                match with_timeout(WRITE_TIMEOUT, write_all(&mut socket, &[0xC0, 0x00])).await {
                    Ok(r) => r?,
                    Err(_) => return Err("PINGREQ write timeout (broker not reading)"),
                }
                next_ping = Instant::now() + PING_INTERVAL;
            }
        }
    }
}

// --- SUBSCRIBE + SUBACK -----------------------------------------------------

async fn subscribe(socket: &mut TcpSocket<'_>) -> Result<(), Error> {
    // Push-OTA rides along: the retained `watch/ota/announce` (fleet) and
    // `watch/<sigil>/ota` (per-watch, #34) are delivered on every
    // (re)subscribe, so any climate/energy session doubles as a push-OTA
    // window (gate + dispatch live in `ota_http::handle_announce`).
    let topics = [
        STATE_WILDCARD,
        ROSTER_TOPIC,
        ENERGY_TOPIC,
        ENERGY_AVAIL_TOPIC,
        crate::net::ota_http::ANNOUNCE_TOPIC,
        crate::net::sigil::get().ota_topic.as_str(),
        // Lights (#39): the retained per-device room-lights snapshot, delivered
        // on every (re)subscribe like the OTA announce above.
        LIGHTS_TOPICS.get().state.as_str(),
        // Notifications (#32): fleet + per-device. Retained notifies re-deliver
        // on every (re)subscribe; the ring's duplicate-of-newest guard
        // (notify::push) keeps them from stacking.
        NOTIFY_TOPIC,
        NOTIFY_DEVICE_TOPIC.get().as_str(),
    ];

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
async fn handle_publish(
    type_byte: u8,
    body: &[u8],
    state: &ClimateStateMutex,
    energy: &EnergyStateMutex,
    lights: &LightsStateMutex,
) {
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
                drop(guard);
                STATE_WAKE.signal(());
            }
            // parse_state == None (malformed / empty retained-clear) -> skip.
        }
        Some(TopicKind::Energy) => {
            if let Some(mut next) = parse_energy(payload) {
                let mut guard = energy.lock().await;
                // Availability is owned by the LWT topic, not the state frame —
                // carry it across the wholesale replace so a fresh state frame
                // can't spuriously clear "HA unreachable".
                next.online = guard.online;
                next.avail_seen = guard.avail_seen;
                *guard = next;
                drop(guard);
                STATE_WAKE.signal(());
            }
            // parse_energy == None (malformed / empty) -> keep last-known state.
        }
        Some(TopicKind::EnergyAvail) => {
            if let Some(online) = parse_avail(payload) {
                let mut guard = energy.lock().await;
                guard.online = online;
                guard.avail_seen = true;
                drop(guard);
                STATE_WAKE.signal(());
            }
            // Unrecognized avail payload -> leave the flag unchanged.
        }
        Some(TopicKind::Roster) => {
            // Belt-and-suspenders per spec §A: the wildcard state subscription
            // is authoritative for what renders, so the roster is informational
            // in v1. Kept as an explicit branch (subscribed + drained, never
            // choked on) for a future roster-diff prune.
        }
        Some(TopicKind::OtaAnnounce) => {
            // Push-OTA: gate (BUILD_EPOCH monotonicity) + post for main.rs.
            crate::net::ota_http::handle_announce(payload);
        }
        Some(TopicKind::Notify) => {
            // Notifications (#32): parse `NOTIFY|<title>|<body>` into the
            // bounded ring; the main loop badges/toasts it. Wake the loop so
            // the badge lands on the next executor pass, not the next tick.
            crate::notify::handle_mqtt(payload);
            STATE_WAKE.signal(());
        }
        Some(TopicKind::LightsState) => {
            // Lights (#39): wholesale replace, seq bumped on EVERY accepted
            // frame — HA republishes after acting on a command, and even an
            // identical payload must clear the UI's optimistic "sent" flash.
            if let Some(mut next) = parse_lights(payload) {
                let mut guard = lights.lock().await;
                next.seq = guard.seq.wrapping_add(1).max(1);
                let seq = next.seq;
                *guard = next;
                drop(guard);
                println!(
                    "[LAT] lights state rx seq={} t={}ms",
                    seq,
                    Instant::now().as_millis()
                );
                STATE_WAKE.signal(());
            }
            // parse_lights == None (malformed / empty retained-clear) -> keep
            // the last-known snapshot.
        }
        None => {} // not one of our topics — ignore
    }
}

enum TopicKind<'a> {
    State(&'a str),
    Energy,
    EnergyAvail,
    Roster,
    OtaAnnounce,
    LightsState,
    Notify,
}

/// Classify an inbound topic. Bounded, UTF-8 checked, no panic.
fn classify_topic(topic: &[u8]) -> Option<TopicKind<'_>> {
    let t = core::str::from_utf8(topic).ok()?;
    if t == ROSTER_TOPIC {
        return Some(TopicKind::Roster);
    }
    if t == crate::net::ota_http::ANNOUNCE_TOPIC
        || t == crate::net::sigil::get().ota_topic.as_str()
    {
        return Some(TopicKind::OtaAnnounce);
    }
    if t == ENERGY_TOPIC {
        return Some(TopicKind::Energy);
    }
    if t == ENERGY_AVAIL_TOPIC {
        return Some(TopicKind::EnergyAvail);
    }
    if t == LIGHTS_TOPICS.get().state.as_str() {
        return Some(TopicKind::LightsState);
    }
    if t == NOTIFY_TOPIC || t == NOTIFY_DEVICE_TOPIC.get().as_str() {
        return Some(TopicKind::Notify);
    }
    let mid = t.strip_prefix(STATE_PREFIX)?.strip_suffix(STATE_SUFFIX)?;
    if mid.is_empty() || mid.contains('/') {
        return None; // "+" matches exactly one level
    }
    Some(TopicKind::State(mid))
}

// --- outbound command PUBLISH ----------------------------------------------

async fn send_command(socket: &mut TcpSocket<'_>, cmd: &ClimateCmd) -> Result<(), Error> {
    match cmd {
        ClimateCmd::SetTemp { obj, temp } => {
            let topic = climate_set_topic(obj)?;
            let payload = climate_model::encode_set_temp(*temp);
            publish(socket, &topic, payload.as_bytes(), false).await
        }
        ClimateCmd::SetMode { obj, mode } => {
            let topic = climate_set_topic(obj)?;
            let payload = climate_model::encode_set_mode(*mode);
            publish(socket, &topic, payload.as_bytes(), false).await
        }
        // Lights (#39): `toggle`|`on`|`off` to the per-device cmd topic,
        // NOT retained (a command is an event, not state).
        ClimateCmd::Lights(action) => {
            let topic = &LIGHTS_TOPICS.get().cmd;
            let res = publish(socket, topic, action.payload().as_bytes(), false).await;
            println!(
                "[LAT] lights cmd '{}' published t={}ms",
                action.payload(),
                Instant::now().as_millis()
            );
            res
        }
    }
}

/// `watch/climate/<obj>/set` — the climate command topic for one entity.
fn climate_set_topic(obj: &str) -> Result<String<TOPIC_CAP>, Error> {
    let mut topic: String<TOPIC_CAP> = String::new();
    topic.push_str(SET_PREFIX).map_err(|_| "cmd topic too long")?;
    topic.push_str(obj).map_err(|_| "cmd topic too long")?;
    topic.push_str(SET_SUFFIX).map_err(|_| "cmd topic too long")?;
    Ok(topic)
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
/// read is cancellable. `pub(crate)` — reused by [`crate::net::mqtt_ha`]'s
/// boot-burst push-OTA announce wait (SUBACK + retained PUBLISH).
pub(crate) async fn read_frame(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<(u8, usize), Error> {
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

// --- energy payload parsing (v0.4.1) ---------------------------------------

/// Parse a `watch/energy/state` retained JSON payload into an [`EnergyState`].
///
/// Same untrusted-input discipline as the climate parse: bounded, panic-free,
/// checked slicing only. Returns `None` on empty / non-UTF-8 / no recognizable
/// numeric field (caller keeps the last-known state). Number parsing is
/// float-tolerant (`87` or `87.0` — luna publishes `battery_pct` as int, but a
/// future `86.7` template sensor is handled) and **JSON `null`-tolerant**: a key
/// emitted as explicit `null` before its sensor's first reading parses to `None`
/// (never a panic). Unknown extra fields are ignored.
///
/// Contract (confirmed, luna-website `feat/energy-live @ 05a8be1`): keys
/// `battery_pct` / `solar_w` / `grid_w` / `charging`; `grid_w` >0 import /
/// <0 export; full retained state per frame. `online` is NOT in this payload —
/// it comes from the `watch/energy/avail` LWT and is merged by the caller, so
/// it is set to `false` here and overwritten with the preserved flag on upsert.
pub fn parse_energy(bytes: &[u8]) -> Option<EnergyState> {
    if bytes.is_empty() {
        return None;
    }
    let s = core::str::from_utf8(bytes).ok()?;

    let battery_pct = json_num(s, "battery_pct").map(|v| v.clamp(0.0, 100.0) as u8);
    let solar_w = json_num(s, "solar_w").map(|v| v as i32);
    let grid_w = json_num(s, "grid_w").map(|v| v as i32);
    let charging = json_bool(s, "charging").unwrap_or(false);

    // Require at least one numeric field so a stray/empty/all-null object doesn't
    // wipe a good last-known snapshot.
    if battery_pct.is_none() && solar_w.is_none() && grid_w.is_none() {
        return None;
    }

    Some(EnergyState {
        battery_pct,
        solar_w,
        grid_w,
        charging,
        online: false,     // caller preserves the real value from the avail LWT
        avail_seen: false, // caller preserves this too (owned by the avail arm)
    })
}

// --- lights payload parsing (#39) -------------------------------------------

/// Parse a retained `watch/<sigil>/lights/state` payload into a [`LightsState`]
/// (with `seq` left at 0 — the caller owns the bump).
///
/// Contract (agreed with the HA side, #39):
/// `AREA|<area name>|<lights on>/<total>|<ok|no_presence|error>`
/// e.g. `AREA|Living Room|3/5|ok`.
///
/// Untrusted network input, same discipline as [`parse_energy`]: bounded,
/// panic-free, checked operations only. Returns `None` (caller keeps the
/// last-known snapshot) on empty / non-UTF-8 / wrong tag / missing fields /
/// unparseable counts. A too-long area name is truncated at a char boundary
/// (display-only); an unknown status word maps to [`LightsStatus::Error`]
/// (defensive — the UI subdues the screen rather than lying "ok").
pub fn parse_lights(bytes: &[u8]) -> Option<LightsState> {
    if bytes.is_empty() {
        return None;
    }
    let s = core::str::from_utf8(bytes).ok()?;
    let mut fields = s.split('|');
    if fields.next()? != "AREA" {
        return None;
    }
    let area_raw = fields.next()?.trim();
    let counts = fields.next()?.trim();
    let status_word = fields.next()?.trim();

    let (on_s, total_s) = counts.split_once('/')?;
    let on: u8 = on_s.trim().parse().ok()?;
    let total: u8 = total_s.trim().parse().ok()?;

    let status = match status_word {
        "ok" => LightsStatus::Ok,
        "no_presence" => LightsStatus::NoPresence,
        _ => LightsStatus::Error, // includes the contract's "error"
    };

    // Truncating, char-boundary-safe copy into the bounded name.
    let mut area: String<LIGHTS_AREA_CAP> = String::new();
    for c in area_raw.chars() {
        if area.push(c).is_err() {
            break;
        }
    }

    Some(LightsState {
        area,
        // Defensive clamp: never display "6/5 lights on".
        on: on.min(total),
        total,
        status,
        seq: 0, // caller bumps from the previous snapshot
    })
}

/// Parse the `watch/energy/avail` LWT payload → `Some(true)` for `online`,
/// `Some(false)` for `offline`, `None` for anything else. Bounded, panic-free.
fn parse_avail(payload: &[u8]) -> Option<bool> {
    match core::str::from_utf8(payload).ok()?.trim() {
        "online" => Some(true),
        "offline" => Some(false),
        _ => None,
    }
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

/// Extract a numeric value for `"<key>":<number>` (accepts int/float, signed,
/// exponent). Explicit JSON `null` → `None` (contract: null === absent). Bounded,
/// panic-free: the value slice is scanned only over numeric chars, so a `null`
/// (or any non-numeric) value yields an empty slice that parses to `None`.
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

/// Extract a boolean value for `"<key>":true|false`. `null`/anything else → `None`.
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
