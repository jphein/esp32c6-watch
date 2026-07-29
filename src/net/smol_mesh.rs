//! SMOLv1 mesh citizenship for the watch.
//!
//! Implements the hardware-verified core of the smol fleet protocol
//! (jphein/smol, docs/protocol.md — wire format mirrored from
//! rust/clock/src/net/mode.rs, which is the source of truth):
//!
//!   HELLO  "SMOLv1 HELLO NNN"                       broadcast, ~2s tick
//!   ACK    "SMOLv1 ACK NNN"                          unicast reply to HELLO
//!   TIME   "SMOLv1 TIME NNN UUUUUUUUUU SSSSSSSSSS"   broadcast, ~2s tick
//!
//! Time authority is loop-free: adopt a peer's time iff their `synced_at`
//! (Unix time of their last authoritative NTP sync) is strictly newer than
//! ours, and inherit their `synced_at` rather than stamping our own. The
//! watch does its own NTP, so it both adopts from and serves time to the
//! fleet. Frames the watch doesn't speak yet (SNK, FAM, ...) are counted
//! and ignored — hearing them still marks the peer as alive.
//!
//! Also ported (byte-accurate to mode.rs / net/wire.rs):
//!
//!   CFG      "SMOLv1 CFG NNN<KEY><value>"            gw→leaf, target 255 = all
//!   RELAY    "SMOLv1 RELAY NNN MMMMM F C " + chunk   leaf uplink, ~15s, ≤4 frags
//!   RELAYACK "SMOLv1 RELAYACK MMMMM BBB"             gw→leaf unicast frag bitmap
//!
//! Watch-originated additions (#35, flagged for smol upstreaming — #36):
//!
//!   PING     "SMOLv1 PING NNN SSSSS"                 broadcast greeting
//!   PINGACK  "SMOLv1 PINGACK NNN SSSSS"              unicast delivery confirm
//! fleet. FAM frames (the Mesh Familiar, #57) are decoded here and routed to
//! [`crate::net::familiar`] via [`MeshEvent::Fam`]. Frames the watch doesn't
//! speak yet (SNK, RELAY, CFG, ...) are counted and ignored — hearing them
//! still marks the peer as alive.

use alloc::vec::Vec;
use esp_radio::esp_now::{EspNow, EspNowWifiInterface, PeerInfo};
use esp_println::println;

use crate::net::familiar::{encode_fam, parse_fam, FamFrame, FAM_CALL, FAM_FRAME_LEN, FAM_PREFIX};

const HELLO_PREFIX: &[u8] = b"SMOLv1 HELLO ";
const ACK_PREFIX: &[u8] = b"SMOLv1 ACK ";
const TIME_PREFIX: &[u8] = b"SMOLv1 TIME ";
const SMOL_PREFIX: &[u8] = b"SMOLv1 ";
/// Keyed per-node config downlink (#56): "NNN" target + 1-byte KEY + value.
const CFG_PREFIX: &[u8] = b"SMOLv1 CFG ";
/// Leaf telemetry uplink fragment: + "NNN MMMMM F C " + chunk.
const RELAY_PREFIX: &[u8] = b"SMOLv1 RELAY ";
/// Gateway's unicast received-fragment bitmap: + "MMMMM BBB".
const RELAYACK_PREFIX: &[u8] = b"SMOLv1 RELAYACK ";

// ⚠️ COORDINATE-WITH-SMOL (#36): the two frame types below are WATCH-ORIGINATED
// additions to the SMOLv1 wire — they do not exist in smol's mode.rs/wire.rs
// yet. Flagged here for upstreaming; if smol adopts different prefixes, these
// two constants (and nothing else) change. Layout follows the fleet's ASCII
// conventions exactly (3-digit id via write_id, 5-digit seq via write_u5).
// Non-watch fleet members that don't speak them already count + ignore them
// via the SMOL_PREFIX fallthrough (proof of life preserved).
//
/// Watch-to-watch greeting (#35): "SMOLv1 PING NNN SSSSS" — NNN = the
/// SENDER's node id, SSSSS = the sender's seq. Broadcast (2-watch fleet).
const PING_PREFIX: &[u8] = b"SMOLv1 PING ";
/// Delivery confirmation (#35): "SMOLv1 PINGACK NNN SSSSS" — NNN = the
/// ACKER's node id, SSSSS echoes the ping's seq. Unicast back to the pinger.
const PINGACK_PREFIX: &[u8] = b"SMOLv1 PINGACK ";
/// Voice transcription pushed to the rest of the fleet: `"SMOLv1 SAY NNN <text>"`
/// where NNN is the SENDER's node id and the rest of the frame is UTF-8 text.
/// Broadcast, fire-and-forget (no ACK): a missed greeting is not worth a
/// retransmit protocol, and the shade card is a convenience, not a delivery
/// guarantee. Single frame — ESP-NOW caps a payload at 250 B, so the text is
/// clipped to [`SAY_TEXT_CAP`] on send (a spoken sentence fits comfortably).
const SAY_PREFIX: &[u8] = b"SMOLv1 SAY ";
/// Max transcription bytes on the wire. 250 B ESP-NOW limit − prefix(11) −
/// id(3) − space(1) leaves 235; clipped to the shade's body capacity since
/// that is all the receiver can render anyway.
pub const SAY_TEXT_CAP: usize = 96;

/// CFG broadcast target sentinel — a fleet-global config (e.g. units).
const CFG_TARGET_ALL: u8 = 255;
/// Max CFG value bytes (mode.rs CFG_VALUE_MAX).
const CFG_VALUE_MAX: usize = 64;

/// Max telemetry payload per RELAY fragment (wire.rs RELAY_CHUNK).
const RELAY_CHUNK: usize = 64;
/// Max fragments per message; keeps the ack bitmap in one u8 (mode.rs).
const RELAY_MAX_FRAGS: usize = 4;
/// Max staged telemetry = 256 B (mode.rs RELAY_MAX_MSG).
const RELAY_MAX_MSG: usize = RELAY_CHUNK * RELAY_MAX_FRAGS;
/// Leaf re-emits fresh telemetry this often (mode.rs RELAY_EMIT_INTERVAL_MS).
const RELAY_EMIT_INTERVAL_MS: u64 = 15_000;
/// Wait this long for a fuller RELAYACK before resending gaps (mode.rs).
const RELAY_RETX_MS: u64 = 2_000;
/// Retransmit ceiling per message — telemetry is loss-tolerant (mode.rs).
const RELAY_MAX_TRIES: u8 = 3;

/// The smol default TIME-SHARE mesh channel (mode.rs ESP_NOW_FIXED_CHANNEL).
pub const MESH_CHANNEL: u8 = 6;
/// Peer link decays after this much silence (protocol.md PEER_STALE_MS).
/// Pub since #35: the ping hero resolves its target from live roster rows.
pub const PEER_STALE_MS: u64 = 3000;
/// HELLO/TIME cadence.
pub const TICK_MS: u64 = 2000;

/// Send one ESP-NOW frame, waiting for completion with a BOUNDED, YIELDING wait.
///
/// ## The bug this replaces (#75) — measured, not theorised
///
/// Every send site here used `esp_now.send(..)` followed by `SendWaiter::wait()`.
/// That waiter is an unbounded, non-yielding busy spin, and so is its `Drop`, so
/// there was no way to escape it once started:
///
/// ```ignore
/// pub fn wait(self) -> Result<(), EspNowError> {
///     core::mem::forget(self);
///     while !ESP_NOW_SEND_CB_INVOKED.load(Ordering::Acquire) {}   // no timeout
/// }
/// impl Drop for SendWaiter<'_> {
///     fn drop(&mut self) { while !ESP_NOW_SEND_CB_INVOKED.load(..) {} }
/// }
/// ```
///
/// esp-radio's own doc says it "will block forever since the callback which
/// signals the completion of sending will never be invoked." `tick()` runs on the
/// Embassy main loop — the loop that renders the UI and reads touch — so one lost
/// TX completion froze the watch on its last drawn frame with no panic.
///
/// A within-device A/B on mythic-throne settled it. Identical tree, identical
/// trials, the ONLY difference being these waits, judged by whether the `[LOOP]`
/// heartbeat appeared at all:
///
/// | arm            | result                                                   |
/// |----------------|----------------------------------------------------------|
/// | waits INTACT   | **0/4 alive** — every trial's last line `[MESH] up as node id236` |
/// | waits REMOVED  | alive, heartbeat running                                 |
///
/// The death line is the print immediately before the first `tick()` after
/// `add_peer` succeeds, i.e. the first mesh HELLO of the boot.
///
/// An earlier CROSS-watch A/B appeared to refute this and was invalid: the other
/// watch runs BLE-on with a different mesh toggle, so it never entered the path
/// under test. A control must differ in one variable.
///
/// ## Why bounded-select rather than fire-and-forget
///
/// Fire-and-forget (`mem::forget` on the waiter) also removes the hang, but the
/// spin was incidentally PACING TX — dropping it lets `tick()` fire back-to-back
/// sends. `send_async` is a real `Future` with a waker (`ESP_NOW_TX_WAKER`), so
/// awaiting it yields the executor instead of burning the CPU, and racing it
/// against a timer makes the wait bounded. Dropping a `SendFuture` early is safe:
/// unlike `SendWaiter`, it has **no `Drop` impl**, so an abandoned wait costs
/// nothing (the next `send_async` resets the completion flag anyway).
///
/// Returns true if the frame completed within the deadline. Callers may ignore
/// it — these are broadcast beacons, and a dropped one is corrected by the next.
pub async fn send_bounded(
    esp_now: &mut EspNow<'_>,
    addr: &[u8; 6],
    data: &[u8],
) -> bool {
    use embassy_futures::select::{select, Either};
    match select(
        esp_now.send_async(addr, data),
        embassy_time::Timer::after(embassy_time::Duration::from_millis(TX_WAIT_MS)),
    )
    .await
    {
        Either::First(r) => r.is_ok(),
        Either::Second(_) => false, // deadline hit: abandon the frame, never hang
    }
}

/// Deadline for one ESP-NOW frame completion. A broadcast normally completes in
/// single-digit milliseconds; this only has to be generous enough not to abandon
/// healthy frames, and short enough that a stuck radio cannot stall the UI loop.
const TX_WAIT_MS: u64 = 30;


#[derive(Clone, Copy, PartialEq)]
pub enum LinkState {
    Idle,
    Detected,
    Connected,
}

struct Peer {
    mac: [u8; 6],
    id: Option<u8>,
    last_rx_ms: u64,
    /// EWMA of per-frame receive RSSI, dBm scaled x8 (alpha = 1/4). ESP-NOW
    /// carries a per-packet RSSI in the rx control info; smoothing it gives
    /// the Marauder's-Watch near/far signal without BLE.
    rssi_ewma_x8: Option<i32>,
}

/// Max roster rows the UI renders / the mesh push fills. Owns the constant now
/// that the embedded-graphics pages module (its former home) is gone.
pub const MESH_MAX_ROWS: usize = 7;

/// Hard ceiling on tracked peers. This is a SECURITY bound, not a memory tune (#75).
///
/// The roster was append-only: `upsert_peer` pushed a 24 B `Peer` for every distinct
/// source MAC and nothing in this file ever removed one. `PEER_STALE_MS` only filters
/// the roster *reads*; it never evicted.
///
/// The reachability is what makes this matter. The entry price is a 7-byte ASCII
/// prefix: the fallthrough at the end of `handle_frame` upserts on ANY frame starting
/// with `SMOL_PREFIX`, and a malformed FAM frame upserts too. No id parse, no
/// authentication — `ensure_unicast_peer` registers with `lmk: None, encrypt: false` —
/// and the source MAC is attacker-chosen. So any on-channel ESP-NOW broadcaster
/// (`MESH_CHANNEL` 6) could add 24 B per spoofed MAC without limit: ~340 entries
/// (~8 KB) is enough for the `Vec` doubling to become one of the mid-size contiguous
/// requests already failing on this pool, ~1,700 (~40 KB) exhausts it, and the OOM
/// path is `Vec::push` -> panic -> reboot. A single MAC-cycling neighbour does it by
/// accident; a hostile one does it on purpose.
///
/// Note the one bound that already existed and does NOT save us: `ensure_unicast_peer`
/// only fills the *blob's* peer table, which IDF caps and whose errors we swallow. The
/// blob side is bounded; this `Vec` was not.
///
/// 16 is chosen against two real ceilings, not for roundness:
/// - ESP-IDF's `ESP_NOW_MAX_TOTAL_PEER_NUM` is 20 (`esp-wifi-sys-*/src/include.rs`).
///   `ensure_unicast_peer` cannot register more than that with the driver, so tracking
///   past ~20 buys nothing — we could not unicast-ACK them anyway.
/// - It is 2x `MESH_MAX_ROWS`, so for any realistic fleet the roster never reaches the
///   cap at all and behaviour is bit-identical to before this change.
///
/// Worst case the backing Vec is 16 * 24 B = 384 B, reached via three growing pushes
/// (4 -> 8 -> 16) and then never reallocated.
pub const MESH_MAX_PEERS: usize = 16;

/// A read-only roster row for the UI: who, how long since we heard them, and
/// how near they sound (smoothed dBm).
#[derive(Clone, Copy, Default)]
pub struct PeerView {
    pub mac: [u8; 6],
    pub id: Option<u8>,
    pub age_ms: u64,
    pub rssi_dbm: Option<i8>,
}

pub enum MeshEvent {
    /// Adopted a fresher mesh time; payload is the new Unix time. The caller
    /// should set the RTC from it.
    TimeAdopted { unix: u32, from_id: u8 },
    /// CFG key `S` (#21/#56): default-screen command for us. `page` is the
    /// watchface page index, already clamped 0..=3 (fleet wire
    /// `<AppKind>:<page>`; the watch maps the page digit onto its own pages;
    /// empty value = clear → page 0). Apply live + persist.
    CfgScreen { page: u8 },
    /// CFG key `U` (#43, fleet-global): display units `<F|C>|<24|12>`.
    /// Already validated — a malformed value never yields an event (the
    /// caller keeps its current units). Store + persist.
    CfgUnits { temp_f: bool, clk_24h: bool },
    /// CFG key `R` (#52): remote reboot. Transient — never persisted; the
    /// caller should boot-debounce, then `esp_hal::system::software_reset()`.
    CfgReboot,
    /// A decoded SMOLv1 FAM frame (+ its RSSI, which weights the familiar's
    /// orphan-takeover stagger). Route to `FamState::ingest`.
    Fam { frame: FamFrame, rssi: i32 },
    /// A watch-to-watch PING for us (#35). The PINGACK reply has already been
    /// unicast by `handle_rx` (delivery confirmation is protocol-level, like
    /// HELLO→ACK); the caller owns the greeting choreography — pulse, chime,
    /// wake, dedup. `mac` is the sender's source address, for the sigil
    /// fallback when `from_id` is outside the known fleet.
    Ping { from_id: u8, seq: u16, mac: [u8; 6] },
    /// A PINGACK answering one of OUR pings (#35): `from_id` is the ACKER,
    /// `seq` echoes the ping it confirms ("delivered to <sigil>").
    PingAck { from_id: u8, seq: u16, mac: [u8; 6] },
    /// A voice transcription from another watch. The main loop turns it into a
    /// shade card ("<sigil> said"). Text is already clipped + sanitized.
    Say { from_id: u8, text: heapless::String<SAY_TEXT_CAP>, mac: [u8; 6] },
}

/// A leaf's single outstanding RELAY message (mode.rs RelayTx): retained so
/// the unacked fragments can be retransmitted. One at a time — a fresh emit
/// supersedes the previous.
struct RelayTx {
    active: bool,
    msgid: u16,
    count: u8,
    acked: u8,
    tries: u8,
    total_len: usize,
    last_ms: u64,
    buf: [u8; RELAY_MAX_MSG],
}

impl RelayTx {
    const fn new() -> Self {
        Self {
            active: false,
            msgid: 0,
            count: 0,
            acked: 0,
            tries: 0,
            total_len: 0,
            last_ms: 0,
            buf: [0; RELAY_MAX_MSG],
        }
    }
}

pub struct SmolMesh {
    id: u8,
    peers: Vec<Peer>,
    /// unix = uptime_secs + offset, once known.
    unix_offset: Option<i64>,
    /// Unix time of the last *authoritative* sync (ours = NTP; inherited on adopt).
    synced_at: u32,
    last_ack_for_us_ms: u64,
    last_tick_ms: u64,
    pub other_frames_heard: u32,
    // --- RELAY leaf uplink state (mode.rs Relay, leaf side only) ---
    next_msgid: u16,
    relay_tx: RelayTx,
    last_relay_emit_ms: u64,
    /// #75: latch so the "roster at cap" warning is printed once, not per frame.
    roster_cap_logged: bool,
}

fn parse_id(rest: &[u8]) -> Option<u8> {
    if rest.len() < 3 {
        return None;
    }
    let mut v: u16 = 0;
    for &b in &rest[..3] {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as u16;
    }
    u8::try_from(v).ok()
}

fn parse_u10(s: &[u8]) -> Option<u32> {
    if s.len() < 10 {
        return None;
    }
    let mut v: u64 = 0;
    for &b in &s[..10] {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as u64;
    }
    u32::try_from(v).ok()
}

fn write_id(id: u8, out: &mut [u8]) {
    out[0] = b'0' + (id / 100) % 10;
    out[1] = b'0' + (id / 10) % 10;
    out[2] = b'0' + id % 10;
}

fn write_u10(v: u32, out: &mut [u8]) {
    let mut v = v as u64;
    for i in (0..10).rev() {
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
}

/// Write a 5-digit zero-padded decimal (wire.rs write_u5); u16 msgids fit.
fn write_u5(v: u32, out: &mut [u8]) {
    let v = v % 100_000;
    out[0] = b'0' + ((v / 10_000) % 10) as u8;
    out[1] = b'0' + ((v / 1_000) % 10) as u8;
    out[2] = b'0' + ((v / 100) % 10) as u8;
    out[3] = b'0' + ((v / 10) % 10) as u8;
    out[4] = b'0' + (v % 10) as u8;
}

/// Parse exactly 5 ASCII digits (wire.rs parse_u5).
fn parse_u5(rest: &[u8]) -> Option<u32> {
    if rest.len() < 5 {
        return None;
    }
    let mut val: u32 = 0;
    for &b in &rest[..5] {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val * 10 + (b - b'0') as u32;
    }
    Some(val)
}

/// Encode one RELAY fragment "SMOLv1 RELAY NNN MMMMM F C " + chunk into
/// `out`; returns the total length (27-byte header + chunk ≤ 91). Byte-exact
/// port of wire.rs `encode_relay`.
fn encode_relay(src_id: u8, msgid: u16, frag: u8, count: u8, chunk: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    out[..RELAY_PREFIX.len()].copy_from_slice(RELAY_PREFIX);
    n += RELAY_PREFIX.len();
    write_id(src_id, &mut out[n..]);
    n += 3;
    out[n] = b' ';
    n += 1;
    write_u5(msgid as u32, &mut out[n..]);
    n += 5;
    out[n] = b' ';
    n += 1;
    out[n] = b'0' + frag;
    n += 1;
    out[n] = b' ';
    n += 1;
    out[n] = b'0' + count;
    n += 1;
    out[n] = b' ';
    n += 1;
    let len = chunk.len().min(RELAY_CHUNK);
    out[n..n + len].copy_from_slice(&chunk[..len]);
    n + len
}

/// Encode a "PREFIX NNN SSSSS" ping-family frame (#35) into `out`; returns the
/// total length (prefix + 3-digit id + ' ' + 5-digit seq). `out` must hold
/// `prefix.len() + 9` bytes — both call sites size for the longer PINGACK.
fn encode_ping_frame(prefix: &[u8], id: u8, seq: u16, out: &mut [u8]) -> usize {
    let mut n = prefix.len();
    out[..n].copy_from_slice(prefix);
    write_id(id, &mut out[n..]);
    n += 3;
    out[n] = b' ';
    n += 1;
    write_u5(seq as u32, &mut out[n..]);
    n + 5
}

/// Longest prefix of `s` that fits in `n` bytes, cut on a CHAR BOUNDARY.
/// Slicing mid-codepoint would emit invalid UTF-8 that the receiver's
/// `from_utf8` rejects, silently dropping the whole message.
fn clip_str(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Parse a ping-family "NNN SSSSS" tail into `(id, seq)` (#35).
fn parse_ping_rest(rest: &[u8]) -> Option<(u8, u16)> {
    if rest.len() < 9 {
        return None;
    }
    let id = parse_id(&rest[0..3])?;
    let seq = u16::try_from(parse_u5(&rest[4..9])?).ok()?;
    Some((id, seq))
}

/// Parse a RELAYACK "MMMMM BBB" tail into `(msgid, bitmap)` (wire.rs
/// `parse_relayack`, minus the strip_prefix done by the caller).
fn parse_relayack_rest(rest: &[u8]) -> Option<(u16, u8)> {
    if rest.len() < 9 {
        return None;
    }
    let msgid = u16::try_from(parse_u5(&rest[0..5])?).ok()?;
    let bitmap = parse_id(&rest[6..9])?;
    Some((msgid, bitmap))
}

/// Low `count` bits set — the "all fragments received" mask (mode.rs).
#[inline]
fn frag_mask(count: u8) -> u8 {
    if count >= 8 {
        0xFF
    } else {
        (1u8 << count) - 1
    }
}

/// CFG `S` value → watchface page. Fleet wire is `<AppKind>:<page>` (page =
/// one digit, out-of-range clamps; empty = clear → 0). The watch has no
/// AppKind tiers, so it takes the digit after the last ':' (or a bare
/// leading digit) as its own page index, clamped to the 5-page Slint shell
/// range 0..=4 (clock/sensors/system/power/mesh). Was 0..=3 in the 4-page
/// era — that stranded remote CFG-`S` one page short of the mesh page.
fn parse_screen_page(value: &[u8]) -> u8 {
    let digit = match value.iter().rposition(|&b| b == b':') {
        Some(i) => value.get(i + 1).copied(),
        None => value.first().copied(),
    };
    match digit {
        Some(d) if d.is_ascii_digit() => (d - b'0').min(4),
        _ => 0,
    }
}

/// CFG `U` value `<F|C>|<24|12>` → (temp_f, clk_24h). Any malformed token →
/// `None` so the caller keeps its current units (units.rs `from_wire`).
fn parse_units(value: &[u8]) -> Option<(bool, bool)> {
    let s = core::str::from_utf8(value).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    let mut it = s.split('|');
    let temp_f = match it.next().unwrap_or("").trim() {
        "F" => true,
        "C" => false,
        _ => return None,
    };
    let clk_24h = match it.next().unwrap_or("").trim() {
        "24" => true,
        "12" => false,
        _ => return None,
    };
    Some((temp_f, clk_24h))
}

impl SmolMesh {
    pub fn new(id: u8) -> Self {
        Self {
            id,
            peers: Vec::new(),
            unix_offset: None,
            synced_at: 0,
            last_ack_for_us_ms: 0,
            last_tick_ms: 0,
            other_frames_heard: 0,
            next_msgid: 0,
            relay_tx: RelayTx::new(),
            last_relay_emit_ms: 0,
            roster_cap_logged: false,
        }
    }

    /// Called after our own NTP sync: we become a time authority.
    pub fn set_time_authoritative(&mut self, unix: u32, uptime_secs: u64) {
        self.unix_offset = Some(unix as i64 - uptime_secs as i64);
        self.synced_at = unix;
    }

    pub fn unix_now(&self, uptime_secs: u64) -> Option<u32> {
        self.unix_offset
            .map(|off| (uptime_secs as i64 + off).max(0) as u32)
    }

    pub fn link_state(&self, now_ms: u64) -> LinkState {
        if now_ms.saturating_sub(self.last_ack_for_us_ms) < PEER_STALE_MS
            && self.last_ack_for_us_ms != 0
        {
            return LinkState::Connected;
        }
        if self
            .peers
            .iter()
            .any(|p| now_ms.saturating_sub(p.last_rx_ms) < PEER_STALE_MS)
        {
            return LinkState::Detected;
        }
        LinkState::Idle
    }

    pub fn peer_count(&self, now_ms: u64) -> usize {
        self.peers
            .iter()
            .filter(|p| now_ms.saturating_sub(p.last_rx_ms) < PEER_STALE_MS)
            .count()
    }

    /// Fill `out` with the ids of currently-live, id-known peers, returning
    /// the count. The familiar's stand-in for the fleet's RSSI-sorted roster
    /// (wander-destination candidates).
    pub fn live_peer_ids(&self, now_ms: u64, out: &mut [u8]) -> usize {
        let mut n = 0;
        for p in &self.peers {
            if n == out.len() {
                break;
            }
            if let Some(id) = p.id {
                if id != self.id && now_ms.saturating_sub(p.last_rx_ms) < PEER_STALE_MS {
                    out[n] = id;
                    n += 1;
                }
            }
        }
        n
    }

    fn ensure_unicast_peer(esp_now: &mut EspNow<'_>, mac: [u8; 6]) {
        if esp_now.peer_exists(&mac) {
            return;
        }
        let _ = esp_now.add_peer(PeerInfo {
            interface: EspNowWifiInterface::Station,
            peer_address: mac,
            lmk: None,
            channel: None,
            encrypt: false,
        });
    }

    fn upsert_peer(&mut self, mac: [u8; 6], id: Option<u8>, now_ms: u64, rssi: Option<i8>) -> bool {
        if let Some(p) = self.peers.iter_mut().find(|p| p.mac == mac) {
            p.last_rx_ms = now_ms;
            if id.is_some() {
                p.id = id;
            }
            if let Some(dbm) = rssi {
                let sample = (dbm as i32) << 3;
                p.rssi_ewma_x8 = Some(match p.rssi_ewma_x8 {
                    // EWMA alpha = 1/4: ewma += (sample - ewma) / 4
                    Some(e) => e + (sample - e) / 4,
                    None => sample,
                });
            }
            false
        } else {
            let fresh = Peer {
                mac,
                id,
                last_rx_ms: now_ms,
                rssi_ewma_x8: rssi.map(|dbm| (dbm as i32) << 3),
            };
            // #75 hardening: bounded insert. Below the cap this is the old `push`.
            if self.peers.len() < MESH_MAX_PEERS {
                self.peers.push(fresh);
                return true;
            }

            // At the cap. The newcomer is UNAUTHENTICATED (see `MESH_MAX_PEERS`), so it
            // may only take a slot we already believe is DEAD — stale beyond
            // `PEER_STALE_MS`, the same liveness notion the roster reads already use.
            // If every slot is live we refuse the newcomer rather than displace a real
            // peer: an attacker must not be able to evict the fleet by flooding MACs.
            // Among dead slots prefer anonymous ones (no id ever parsed from them),
            // then the stalest. `id` is a weak signal — a forged HELLO can claim one —
            // but it costs nothing and it correctly prioritises evicting the junk from
            // the `SMOL_PREFIX` fallthrough over a fleet member that went quiet.
            let victim = self
                .peers
                .iter_mut()
                .filter(|p| now_ms.saturating_sub(p.last_rx_ms) >= PEER_STALE_MS)
                .min_by_key(|p| (p.id.is_some(), p.last_rx_ms));

            // The only signal anyone will ever get that the cap is live. Latched, so a
            // flood cannot turn it into a log storm.
            if !self.roster_cap_logged {
                self.roster_cap_logged = true;
                println!(
                    "[MESH] roster at cap {MESH_MAX_PEERS} - evicting stale / refusing live-full (possible ESP-NOW MAC flood)"
                );
            }

            match victim {
                Some(v) => {
                    *v = fresh;
                    true
                }
                // Cap reached and every entry still live: drop the newcomer on the
                // floor. It is still ACKed/handled by the caller for this frame; it
                // just does not earn a roster slot.
                None => false,
            }
        }
    }

    /// Snapshot the roster into `out`, ordered by id (known ids first,
    /// ascending; anonymous MACs last, freshest first). Returns row count.
    pub fn peers(&self, now_ms: u64, out: &mut [PeerView]) -> usize {
        // SORT-then-truncate. This used to truncate-then-sort: the roster was
        // copied in INSERTION order, cut at `out.len()`, and only that prefix was
        // sorted — so an IDENTIFIED peer sitting past the cut was invisible behind
        // staler rows that merely arrived first. The roster holds up to
        // `2 * MESH_MAX_ROWS` entries against the DoS cap (a7e4b69) while the UI
        // shows 7, so the cut is reachable in normal use, not just in theory.
        //
        // Bounded top-k insertion: every roster entry is considered, the best
        // `out.len()` are kept, and they come out already sorted. No alloc, and at
        // 16 x 7 the comparison count is trivial.
        let key = |r: &PeerView| match r.id {
            Some(id) => (0u8, id as u64, 0u64),
            None => (1u8, 0, r.age_ms),
        };
        if out.is_empty() {
            return 0;
        }
        let mut n = 0;
        for p in &self.peers {
            let view = PeerView {
                mac: p.mac,
                id: p.id,
                age_ms: now_ms.saturating_sub(p.last_rx_ms),
                rssi_dbm: p.rssi_ewma_x8.map(|e| (e >> 3).clamp(-128, 127) as i8),
            };
            // Where does this row belong among the ones kept so far?
            let mut pos = n;
            while pos > 0 && key(&out[pos - 1]) > key(&view) {
                pos -= 1;
            }
            if n < out.len() {
                // Room left: shift the tail right and insert.
                let mut j = n;
                while j > pos {
                    out[j] = out[j - 1];
                    j -= 1;
                }
                out[pos] = view;
                n += 1;
            } else if pos < n {
                // Full, but this beats the current worst row — drop that one.
                let mut j = n - 1;
                while j > pos {
                    out[j] = out[j - 1];
                    j -= 1;
                }
                out[pos] = view;
            }
            // else: full and this sorts after everything kept — discard it.
        }
        n
    }

    /// The ~2s HELLO/TIME tick. Call from the main loop; no-ops until due.
    pub async fn tick(&mut self, esp_now: &mut EspNow<'_>, now_ms: u64, uptime_secs: u64) {
        if now_ms.saturating_sub(self.last_tick_ms) < TICK_MS {
            return;
        }
        self.last_tick_ms = now_ms;

        let mut hello = [0u8; 16];
        hello[..HELLO_PREFIX.len()].copy_from_slice(HELLO_PREFIX);
        write_id(self.id, &mut hello[HELLO_PREFIX.len()..]);
        send_bounded(esp_now, &esp_radio::esp_now::BROADCAST_ADDRESS, &hello).await;

        if let Some(unix) = self.unix_now(uptime_secs) {
            let mut time = [0u8; 37];
            time[..TIME_PREFIX.len()].copy_from_slice(TIME_PREFIX);
            let mut n = TIME_PREFIX.len();
            write_id(self.id, &mut time[n..]);
            n += 3;
            time[n] = b' ';
            n += 1;
            write_u10(unix, &mut time[n..]);
            n += 10;
            time[n] = b' ';
            n += 1;
            write_u10(self.synced_at, &mut time[n..]);
            send_bounded(esp_now, &esp_radio::esp_now::BROADCAST_ADDRESS, &time).await;
        }
    }

    /// Broadcast a DIAG record ("SMOLv1 DIAG NNN" + verbatim key=val record).
    /// The fleet gateway caches it and republishes retained to smol/<id>/diag,
    /// which is how the watch shows up in Home Assistant without MQTT.
    pub async fn broadcast_diag(&mut self, esp_now: &mut EspNow<'_>, record: &[u8]) {
        const DIAG_PREFIX: &[u8] = b"SMOLv1 DIAG ";
        let mut msg = [0u8; 250];
        msg[..DIAG_PREFIX.len()].copy_from_slice(DIAG_PREFIX);
        write_id(self.id, &mut msg[DIAG_PREFIX.len()..]);
        let base = DIAG_PREFIX.len() + 3;
        let n = record.len().min(250 - base);
        msg[base..base + n].copy_from_slice(&record[..n]);
        send_bounded(esp_now, &esp_radio::esp_now::BROADCAST_ADDRESS, &msg[..base + n]).await;
    }

    /// Handle one received ESP-NOW payload. `rssi` is the per-frame receive
    /// RSSI in dBm from the rx control info, if the radio reported one; it
    /// feeds the per-peer near/far EWMA.
    /// Leaf uplink: is it time to emit a fresh RELAY telemetry message?
    /// Gated on the mesh being up (a live peer) — a watch alone in a drawer
    /// shouldn't spend airtime on unhearable telemetry.
    pub fn relay_emit_due(&self, now_ms: u64) -> bool {
        self.link_state(now_ms) != LinkState::Idle
            && (self.last_relay_emit_ms == 0
                || now_ms.saturating_sub(self.last_relay_emit_ms) >= RELAY_EMIT_INTERVAL_MS)
    }

    /// Send one staged fragment as a broadcast RELAY frame.
    async fn relay_send_frag(&mut self, esp_now: &mut EspNow<'_>, frag: u8) {
        let off = frag as usize * RELAY_CHUNK;
        let end = (off + RELAY_CHUNK).min(self.relay_tx.total_len);
        let mut fb = [0u8; 96]; // ≤ 27-byte header + RELAY_CHUNK(64) = 91
        let len = encode_relay(
            self.id,
            self.relay_tx.msgid,
            frag,
            self.relay_tx.count,
            &self.relay_tx.buf[off..end],
            &mut fb,
        );
        send_bounded(esp_now, &esp_radio::esp_now::BROADCAST_ADDRESS, &fb[..len]).await;
    }

    /// Leaf uplink: fragment `telemetry` into RELAY frames and broadcast them
    /// all, staging the message for bounded retransmit (mode.rs `relay_emit`
    /// + `stage_tx`, single-hop only). A fresh emit supersedes the previous.
    pub async fn relay_emit(&mut self, esp_now: &mut EspNow<'_>, telemetry: &[u8], now_ms: u64) {
        let len = telemetry.len().min(RELAY_MAX_MSG);
        self.last_relay_emit_ms = now_ms;
        if len == 0 {
            return;
        }
        let count = len.div_ceil(RELAY_CHUNK) as u8; // 1..=RELAY_MAX_FRAGS
        self.relay_tx = RelayTx::new();
        self.relay_tx.active = true;
        self.relay_tx.msgid = self.next_msgid;
        self.next_msgid = self.next_msgid.wrapping_add(1);
        self.relay_tx.count = count;
        self.relay_tx.total_len = len;
        self.relay_tx.tries = 1;
        self.relay_tx.last_ms = now_ms;
        self.relay_tx.buf[..len].copy_from_slice(&telemetry[..len]);
        for frag in 0..count {
            self.relay_send_frag(esp_now, frag).await;
        }
        println!(
            "[MESH] relay emit msgid {} ({count} frag, {len} B)",
            self.relay_tx.msgid
        );
    }

    /// Leaf uplink: retransmit the fragments a RELAYACK hasn't confirmed,
    /// bounded to RELAY_MAX_TRIES with RELAY_RETX_MS between rounds (mode.rs
    /// `relay_retransmit`). No-op once fully acked / out of tries / too soon.
    pub async fn relay_retransmit(&mut self, esp_now: &mut EspNow<'_>, now_ms: u64) {
        if !self.relay_tx.active {
            return;
        }
        if self.relay_tx.acked & frag_mask(self.relay_tx.count) == frag_mask(self.relay_tx.count) {
            self.relay_tx.active = false;
            return;
        }
        if self.relay_tx.tries >= RELAY_MAX_TRIES {
            self.relay_tx.active = false; // give up — telemetry is loss-tolerant
            return;
        }
        if now_ms.saturating_sub(self.relay_tx.last_ms) < RELAY_RETX_MS {
            return; // give the gateway time to ACK before resending
        }
        for frag in 0..self.relay_tx.count {
            if self.relay_tx.acked & (1u8 << frag) != 0 {
                continue; // already confirmed
            }
            self.relay_send_frag(esp_now, frag).await;
        }
        self.relay_tx.tries += 1;
        self.relay_tx.last_ms = now_ms;
    }

    /// Broadcast a watch-to-watch PING (#35): "SMOLv1 PING NNN SSSSS" with OUR
    /// node id + the caller's seq. Broadcast is deliberate (2-watch fleet —
    /// and a fleet-wide greeting is the charming failure mode); delivery
    /// confirmation comes back as a unicast PINGACK ([`MeshEvent::PingAck`]).
    pub async fn send_ping(&mut self, esp_now: &mut EspNow<'_>, seq: u16) {
        let mut buf = [0u8; 24]; // PINGACK-sized; PING uses 21
        let n = encode_ping_frame(PING_PREFIX, self.id, seq, &mut buf);
        send_bounded(esp_now, &esp_radio::esp_now::BROADCAST_ADDRESS, &buf[..n]).await;
        println!("[MESH] ping sent (seq {seq})");
    }

    /// Broadcast a voice transcription to the fleet: `"SMOLv1 SAY NNN <text>"`.
    ///
    /// Fire-and-forget, single frame. `text` is clipped to [`SAY_TEXT_CAP`] on a
    /// CHAR BOUNDARY (a mid-codepoint cut would make the receiver's
    /// `from_utf8` reject the whole frame and silently drop the message).
    pub async fn send_say(&mut self, esp_now: &mut EspNow<'_>, text: &str) {
        let clipped = clip_str(text, SAY_TEXT_CAP);
        if clipped.is_empty() {
            return;
        }
        let mut buf = [0u8; SAY_PREFIX.len() + 4 + SAY_TEXT_CAP];
        let mut n = SAY_PREFIX.len();
        buf[..n].copy_from_slice(SAY_PREFIX);
        write_id(self.id, &mut buf[n..]);
        n += 3;
        buf[n] = b' ';
        n += 1;
        buf[n..n + clipped.len()].copy_from_slice(clipped.as_bytes());
        n += clipped.len();
        send_bounded(esp_now, &esp_radio::esp_now::BROADCAST_ADDRESS, &buf[..n]).await;
        println!("[MESH] say sent ({} B)", clipped.len());
    }

    /// Handle one received ESP-NOW payload.
    /// Broadcast a SMOLv1 FAM frame (heartbeat/handoff) for the familiar
    /// state machine. Fixed 29-byte binary frame, fleet wire format.
    pub async fn broadcast_fam(&mut self, esp_now: &mut EspNow<'_>, f: &FamFrame) {
        let mut buf = [0u8; FAM_FRAME_LEN];
        if let Some(len) = encode_fam(f, &mut buf) {
            send_bounded(esp_now, &esp_radio::esp_now::BROADCAST_ADDRESS, &buf[..len]).await;
        }
    }

    /// Handle one received ESP-NOW payload. `rssi` comes from the frame's RX
    /// control info and feeds the familiar's takeover weighting.
    pub async fn handle_rx(
        &mut self,
        esp_now: &mut EspNow<'_>,
        src: [u8; 6],
        data: &[u8],
        rssi: Option<i8>,
        now_ms: u64,
        uptime_secs: u64,
    ) -> Option<MeshEvent> {
        if let Some(rest) = data.strip_prefix(HELLO_PREFIX) {
            let id = parse_id(rest)?;
            let new = self.upsert_peer(src, Some(id), now_ms, rssi);
            if new {
                println!("[MESH] hello from id{id} {src:02x?}");
            }
            Self::ensure_unicast_peer(esp_now, src);
            // Reply with a unicast ACK echoing the sender's id.
            let mut ack = [0u8; 14];
            ack[..ACK_PREFIX.len()].copy_from_slice(ACK_PREFIX);
            write_id(id, &mut ack[ACK_PREFIX.len()..]);
            send_bounded(esp_now, &src, &ack).await;
            return None;
        }
        if let Some(rest) = data.strip_prefix(ACK_PREFIX) {
            let id = parse_id(rest)?;
            self.upsert_peer(src, None, now_ms, rssi);
            if id == self.id {
                if now_ms.saturating_sub(self.last_ack_for_us_ms) > PEER_STALE_MS {
                    println!("[MESH] link Connected (acked by {src:02x?})");
                }
                self.last_ack_for_us_ms = now_ms;
            }
            return None;
        }
        if let Some(rest) = data.strip_prefix(TIME_PREFIX) {
            if rest.len() < 25 {
                return None;
            }
            let id = parse_id(&rest[0..3])?;
            let unix = parse_u10(&rest[4..14])?;
            let synced_at = parse_u10(&rest[15..25])?;
            self.upsert_peer(src, Some(id), now_ms, rssi);
            // Loop-free adoption: strictly-newer authority wins; inherit
            // the origin's synced_at so freshness can't inflate in a cycle.
            if synced_at > self.synced_at {
                self.unix_offset = Some(unix as i64 - uptime_secs as i64);
                self.synced_at = synced_at;
                println!("[MESH] adopted time from id{id} (synced_at={synced_at})");
                return Some(MeshEvent::TimeAdopted { unix, from_id: id });
            }
            return None;
        }
        if let Some(rest) = data.strip_prefix(CFG_PREFIX) {
            // #21/#56 keyed config downlink: "NNN<KEY><value>". A key-less
            // frame (id only) is the pre-key wire = an empty-value clear on
            // the screen key `S` (mode.rs classify back-compat).
            self.upsert_peer(src, None, now_ms, rssi);
            let target = parse_id(rest)?;
            let (key, value): (u8, &[u8]) = match rest.get(3) {
                Some(&k) => (k, &rest[4..]),
                None => (b'S', &rest[3..]),
            };
            if target != self.id && target != CFG_TARGET_ALL {
                return None; // some other leaf's config
            }
            let value = &value[..value.len().min(CFG_VALUE_MAX)];
            return match key {
                b'S' => {
                    let page = parse_screen_page(value);
                    println!("[MESH] CFG S: default screen -> page {page}");
                    Some(MeshEvent::CfgScreen { page })
                }
                b'U' => parse_units(value).map(|(temp_f, clk_24h)| {
                    println!(
                        "[MESH] CFG U: units -> {}|{}",
                        if temp_f { "F" } else { "C" },
                        if clk_24h { "24" } else { "12" }
                    );
                    MeshEvent::CfgUnits { temp_f, clk_24h }
                }),
                b'R' => {
                    println!("[MESH] CFG R: remote reboot commanded");
                    Some(MeshEvent::CfgReboot)
                }
                // Forward-compat (#46 clamp): a key this firmware doesn't
                // apply (L/P/Y/B/O/W/G/g/...) is dropped silently.
                _ => None,
            };
        }
        if let Some(rest) = data.strip_prefix(RELAYACK_PREFIX) {
            // Gateway's unicast cumulative fragment bitmap for our uplink.
            self.upsert_peer(src, None, now_ms, rssi);
            if let Some((msgid, bitmap)) = parse_relayack_rest(rest) {
                if self.relay_tx.active && self.relay_tx.msgid == msgid {
                    self.relay_tx.acked |= bitmap;
                    if self.relay_tx.acked & frag_mask(self.relay_tx.count)
                        == frag_mask(self.relay_tx.count)
                    {
                        self.relay_tx.active = false; // fully delivered
                        println!("[MESH] relay msgid {msgid} fully acked");
                    }
                }
            }
            return None;
        }
        // #35 watch-to-watch ping. PINGACK first: "SMOLv1 PING " (trailing
        // space) can never prefix a PINGACK frame, but checking the more
        // specific type first keeps the pair robust against future editing.
        if let Some(rest) = data.strip_prefix(PINGACK_PREFIX) {
            let (from_id, seq) = parse_ping_rest(rest)?;
            self.upsert_peer(src, Some(from_id), now_ms, rssi);
            return Some(MeshEvent::PingAck { from_id, seq, mac: src });
        }
        if let Some(rest) = data.strip_prefix(PING_PREFIX) {
            let (from_id, seq) = parse_ping_rest(rest)?;
            self.upsert_peer(src, Some(from_id), now_ms, rssi);
            // Confirm delivery at the protocol level (the HELLO→ACK idiom):
            // unicast a PINGACK with OUR id echoing the ping's seq, even when
            // the caller ends up deduping the greeting choreography.
            Self::ensure_unicast_peer(esp_now, src);
            let mut ack = [0u8; 24];
            let n = encode_ping_frame(PINGACK_PREFIX, self.id, seq, &mut ack);
            send_bounded(esp_now, &src, &ack[..n]).await;
            println!("[MESH] ping from id{from_id} (seq {seq}) - acked");
            return Some(MeshEvent::Ping { from_id, seq, mac: src });
        }
        // Voice transcription from a peer watch: "SMOLv1 SAY NNN <text>".
        if let Some(rest) = data.strip_prefix(SAY_PREFIX) {
            if rest.len() < 4 {
                return None;
            }
            let from_id = parse_id(&rest[0..3])?;
            // Malformed UTF-8 (a mid-codepoint clip upstream) drops the frame
            // rather than rendering replacement characters in the shade.
            let text = core::str::from_utf8(&rest[4..]).ok()?;
            let mut s: heapless::String<SAY_TEXT_CAP> = heapless::String::new();
            let _ = s.push_str(clip_str(text, SAY_TEXT_CAP));
            self.upsert_peer(src, Some(from_id), now_ms, rssi);
            println!("[MESH] say from id{from_id} ({} B)", s.len());
            return Some(MeshEvent::Say { from_id, text: s, mac: src });
        }
        if data.starts_with(FAM_PREFIX) {
            if let Some(f) = parse_fam(data) {
                // The broadcaster is the holder for H/X frames, the caller
                // for C frames (mirrors the fleet's mode.rs routing).
                let sender_id = if f.kind == FAM_CALL { f.target } else { f.holder };
                self.upsert_peer(src, Some(sender_id), now_ms, rssi);
                let rssi_w = rssi.unwrap_or(-127) as i32;
                return Some(MeshEvent::Fam { frame: f, rssi: rssi_w });
            }
            // Malformed FAM: still proof of life.
            self.upsert_peer(src, None, now_ms, rssi);
            return None;
        }
        if data.starts_with(SMOL_PREFIX) {
            // A fleet frame we don't speak yet (SNK/RELAY/CFG/...):
            // still proof of life for the peer.
            self.upsert_peer(src, None, now_ms, rssi);
            self.other_frames_heard += 1;
        }
        None
    }
}
