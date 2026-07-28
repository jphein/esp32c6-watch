//! `net_task` — the network owner (#53, the realtime-UI restructure).
//!
//! This task **exclusively owns `wifi_controller`**: the connect state
//! machine (attempts, reconnect backoff), the STA-PHY start, WiFi scanning,
//! the boot burst (NTP → MQTT burst → weather) and the OTA download. The main
//! loop NEVER calls connect/scan/blocking-net again — that inversion is the
//! fix for "the watch is often unresponsive": every 1–15 s association stall,
//! every scan, and the whole OTA download used to park render+touch.
//!
//! ## Command in, state out
//!
//! Main (and only main) drives this task over a small bounded channel
//! ([`NetCmd`], depth [`CMD_DEPTH`], reject-when-full with a log — see
//! [`send`]), and reads results from a published snapshot:
//!
//! - [`NetCmd::Raise`]/[`NetCmd::Drop`] manage a **hold mask** ([`Hold`]) —
//!   the clean replacement for the old `wifi_on_request` +
//!   `session_holds_wifi` + per-tick `wifi_want` re-raising. Association is
//!   wanted while any of User/Burst/Session/Voice/Ota is up (and creds
//!   exist); `Hold::Phy` (mesh) starts the radio without associating.
//! - [`NetCmd::Scan`] runs a **streaming per-channel sweep**: rows land in
//!   the published list channel by channel (`scan_seq` bumps), so the picker
//!   renders live results under its scanning animation instead of freezing.
//! - [`NetCmd::SetCreds`] persists nothing (main owns flash config); it
//!   re-applies the station config, resets the backoff ("user action"), and
//!   re-arms the boot burst so the new network gets NTP/MQTT/weather.
//! - [`NetCmd::Ota`] queues a download with the old executor's semantics:
//!   45 s WiFi window, 3 attempts with re-arm between them — and because the
//!   connect machine lives HERE, WiFi really can reconnect between attempts
//!   mid-update (the #25 limitation).
//!
//! State flows back through [`snapshot`] (a `Copy` struct behind a blocking
//! mutex: [`WifiPhase`], scan seq, [`OtaPhase`] progress, the mesh-pin
//! verdict) plus one-shot handoffs ([`take_ntp_unix`], [`take_weather`] —
//! main owns the RTC/mesh/shell, so applying results stays there). Every
//! publish signals [`NET_WAKE`], the same coalescing-Signal pattern as
//! v0.8.8's `STATE_WAKE`: the render loop wakes on arrival instead of
//! polling.
//!
//! ## Reconnect policy (the dead-AP acceptance test)
//!
//! Failed connects back off exponentially — 2 s → 10 s → 60 s → 300 s
//! ([`BACKOFF_SECS`]) — instead of hammering; the counter resets on success
//! and any fresh user intent (a new hold, `SetCreds`) retries immediately.
//! Under a dead AP the watch keeps a bounded, radio-only retry duty-cycle and
//! the UI never feels it. A burst that can't complete gives up after
//! [`BURST_GIVEUP`] so a credentialed watch on a dead AP eventually returns
//! the radio to the mesh.
//!
//! ## Radio arbitration (v0.9.1 semantics, decided here)
//!
//! The ESP-NOW mesh channel pin **yields to any WiFi intent** (a5a4c27). The
//! *decision* now lives in this task — `mesh_pin_ok` is true only when the
//! radio is up, unassociated, not connecting/scanning, with no association
//! holds and no OTA pending — while the mechanical `esp_now.set_channel`
//! stays in main (the mesh owns the `esp_now` handle). Bonus over v0.9.1:
//! after a scan sweep the pin verdict returns to true, so main re-pins ch6
//! instead of leaving the radio wherever the scan stopped.
//!
//! Commands arriving while the task is parked in a bounded await (one 15 s
//! connect attempt, one scan channel, an OTA read) queue up and apply right
//! after — the old code blocked the whole UI in those same waits.
//!
//! ## Firmware roaming (#57 — esp-radio has no 802.11r FT)
//!
//! The C6 radio can't do seamless Fast Transition, and the whole house is one
//! roaming SSID with a dozen APs. esp-radio's default connect uses
//! `WIFI_FAST_SCAN`, which associates to the FIRST SSID-matching AP it hears
//! (sort_method is ignored in fast-scan) — routinely a DISTANT BSSID, whose
//! weak-link handshake times out as `AuthenticationExpired` even while a
//! strong AP sits beside you. So we roam in firmware:
//!
//! - **Best-BSSID association**: [`attempt_connect`] runs a targeted candidate
//!   scan ([`scan_candidates`]) and pins the STRONGEST BSSID for the SSID via
//!   `StationConfig::with_bssid` + `with_channel` (`bssid_set=true` →
//!   esp-radio targets exactly it). A pinned BSSID that fails
//!   [`PIN_MAX_FAILS`] times falls back to one `ScanMethod::AllChannels`
//!   driver-select attempt, then re-scans — so a vanished BSSID can't wedge.
//! - **RSSI-triggered roam** ([`maybe_roam`]): while connected, sample
//!   `rssi()`; sustained below [`ROAM_RSSI_THRESH`] AND a candidate ≥
//!   [`ROAM_MARGIN_DB`] stronger (excluding the current BSSID) → disconnect,
//!   pin the stronger one, reconnect (backoff reset). [`ROAM_COOLDOWN`] +
//!   the margin hysteresis prevent ping-pong. Not seamless (~1–3 s drop) but
//!   it follows you room to room.
//! - **SSID-agnostic**: it pins strongest-of-whatever-`ssid`-is, so it works
//!   for `roam`, the `jplovescl` stopgap, or any future network unchanged.
//! - esp-radio's `wifi_sta_config_t` has NO FT/MDE fields — it always
//!   negotiates plain WPA2-PSK (00-0F-AC:2), never FT-PSK. All watch-side; no
//!   AP/infra config is touched.

use core::cell::RefCell;
use core::sync::atomic::Ordering;

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::{raw::CriticalSectionRawMutex, Mutex as BlockingMutex};
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, WifiController};

use crate::net::ota_http;

/// SSID buffer, sized to the config record's field.
pub type SsidBuf = heapless::String<32>;
/// Password buffer, sized to the config record's field.
pub type PassBuf = heapless::String<64>;
/// OTA image-URL override buffer (push announces).
pub type UrlBuf = heapless::String<{ ota_http::ANNOUNCE_URL_CAP }>;

// ============================================================================
// Commands (main -> net_task)
// ============================================================================

/// One reason to keep the radio up. `Raise`/`Drop` toggle bits in a hold
/// mask — levels, not counts, so a duplicate Raise is idempotent and a lost
/// screen-close can be re-dropped safely.
#[derive(Clone, Copy, PartialEq)]
pub enum Hold {
    /// The watchface WIFI toggle (and the boot auto-connect intent).
    User,
    /// The boot burst (NTP/MQTT/weather). Raised internally; completing the
    /// burst drops `Burst` AND `User` — the old "wifi_on_request = false,
    /// burst complete" auto-off. Dropped by `Drop(User)` too (a user OFF
    /// overrides a pending burst).
    Burst,
    /// The shared HA MQTT session (Climate / Energy / Lights screens).
    Session,
    /// The Voice screen (STT upload needs the stack, not the session).
    Voice,
    /// An OTA download pending/running. Raised/dropped internally by the
    /// OTA job; listed here so the mask reads whole in logs.
    Ota,
    /// PHY-only: start the STA radio for ESP-NOW (mesh) without associating.
    /// Never dropped in practice — esp-radio 0.18 has no controller stop,
    /// and mesh-off is a tick-level pause, not a teardown.
    Phy,
    /// PHY-only: a push-to-talk voice session (#71) needs the radio up for
    /// ESP-NOW, but **must not** associate — see [`ASSOC_HOLDS`].
    ///
    /// It gets its own bit rather than reusing [`Hold::Phy`] so the two
    /// reference-count independently: ending a PTT session must not take the
    /// radio away from a running mesh, and mesh-off must not cut a live
    /// transmission.
    Vox,
}

impl Hold {
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
    fn name(self) -> &'static str {
        match self {
            Hold::User => "user",
            Hold::Burst => "burst",
            Hold::Session => "session",
            Hold::Voice => "voice",
            Hold::Ota => "ota",
            Hold::Phy => "phy",
            Hold::Vox => "vox",
        }
    }
}

/// Holds that want an *association* (PHY + connect); `Phy` wants the radio
/// started only.
/// Holds that want the radio STARTED but deliberately NOT associated —
/// ESP-NOW rides the PHY. Associating for a PTT session would make it wait on
/// DHCP and contend with WiFi traffic for the air, which is exactly backwards
/// for realtime audio.
const PHY_HOLDS: u8 = Hold::Phy.bit() | Hold::Vox.bit();

const ASSOC_HOLDS: u8 = Hold::User.bit()
    | Hold::Burst.bit()
    | Hold::Session.bit()
    | Hold::Voice.bit()
    | Hold::Ota.bit();

/// A command for the network owner. See the module docs for semantics.
pub enum NetCmd {
    /// Raise a hold (idempotent).
    Raise(Hold),
    /// Drop a hold (idempotent). `Drop(User)` also clears `Burst`.
    Drop(Hold),
    /// Run a streaming scan sweep; results land in the published rows.
    Scan,
    /// Swap the station credentials (main already persisted them), reconnect,
    /// and re-arm the boot burst. Resets the backoff — this is a user action.
    SetCreds { ssid: SsidBuf, pass: PassBuf },
    /// Queue an OTA download; `None` = the baked `OTA_URL`. Ignored (logged)
    /// while an update is already pending.
    Ota { url: Option<UrlBuf> },
}

impl NetCmd {
    fn tag(&self) -> &'static str {
        match self {
            NetCmd::Raise(_) => "raise",
            NetCmd::Drop(_) => "drop",
            NetCmd::Scan => "scan",
            NetCmd::SetCreds { .. } => "set-creds",
            NetCmd::Ota { .. } => "ota",
        }
    }
}

/// Command-queue depth. Small on purpose: main sends edge events (a hold
/// flip, a scan tap), not streams. The longest producer stall is one OTA
/// download; hold edges are level-derived in main and re-sent next tick when
/// a send is rejected, so nothing is lost — worst case a log line.
pub const CMD_DEPTH: usize = 8;

static CMD_CH: Channel<CriticalSectionRawMutex, NetCmd, CMD_DEPTH> = Channel::new();

/// Queue a command for the net task. Returns `false` (with a log) when the
/// queue is full — callers deriving edges from level state simply retry on
/// the next tick.
pub fn send(cmd: NetCmd) -> bool {
    let tag = cmd.tag();
    if CMD_CH.try_send(cmd).is_ok() {
        true
    } else {
        println!("[NET] cmd queue full - {tag} dropped (caller retries)");
        false
    }
}

// ============================================================================
// Published state (net_task -> main)
// ============================================================================

/// Where the WiFi association machine is. `Up { ip: None }` = associated,
/// DHCP still settling; `ready()` (ip present) is the gate for anything that
/// opens a socket (the old `wifi_connected && stack.config_v4().is_some()`).
#[derive(Clone, Copy, PartialEq)]
pub enum WifiPhase {
    /// PHY down (never started).
    Off,
    /// PHY up, no association wanted (mesh-only steady state).
    Idle,
    /// A connect attempt is in flight (1-based attempt number).
    Connecting { attempt: u8 },
    /// Associated. `ip` is the DHCP address once the lease lands.
    Up { ip: Option<[u8; 4]> },
    /// Waiting out the reconnect backoff after `attempt` consecutive fails.
    Backoff { attempt: u8 },
}

impl WifiPhase {
    /// Associated (the old `wifi_connected`).
    pub fn connected(self) -> bool {
        matches!(self, WifiPhase::Up { .. })
    }
    /// Associated with a DHCP lease — sockets will work.
    pub fn ready(self) -> bool {
        matches!(self, WifiPhase::Up { ip: Some(_) })
    }
}

/// OTA job progress, rendered by main (status line + toast).
#[derive(Clone, Copy, PartialEq)]
pub enum OtaPhase {
    /// No job (also after main consumed a terminal state).
    Idle,
    /// Queued; the connect machine is bringing WiFi up (45 s window).
    WaitingWifi,
    /// Download in flight.
    Downloading { pct: u8 },
    /// Attempt failed; re-armed for the next one (WiFi reconnects under it).
    Retrying { attempt: u8 },
    /// Image staged — reboot to apply (main owns the reset).
    Staged,
    /// Gave up; `msg` is the last error (ota_http's static strings).
    Failed { msg: &'static str },
}

impl OtaPhase {
    /// A job is queued or running (used to refuse a second one).
    pub fn active(self) -> bool {
        matches!(
            self,
            OtaPhase::WaitingWifi | OtaPhase::Downloading { .. } | OtaPhase::Retrying { .. }
        )
    }
}

/// The per-tick view main reads (all `Copy`; one blocking-mutex lock).
#[derive(Clone, Copy, PartialEq)]
pub struct NetSnapshot {
    pub phase: WifiPhase,
    /// STA PHY started (the old `radio_started`) — gates the mesh block.
    pub radio_started: bool,
    /// Association currently wanted (holds + creds) — the old
    /// `wifi_on_request`, for the power page and the idle backstop.
    pub wanted: bool,
    /// v0.9.1 arbitration verdict: main may pin ESP-NOW to the mesh channel
    /// iff this is true (radio up, unassociated, nothing wanting WiFi).
    pub mesh_pin_ok: bool,
    /// A scan sweep is in flight (picker animation).
    pub scanning: bool,
    /// Bumped whenever the scan rows change; main re-pulls rows on a bump.
    pub scan_seq: u32,
    /// Consecutive connect failures (Settings shows "failed" at >= 3).
    pub connect_fails: u8,
    pub ota: OtaPhase,
}

impl NetSnapshot {
    const fn boot() -> Self {
        NetSnapshot {
            phase: WifiPhase::Off,
            radio_started: false,
            wanted: false,
            mesh_pin_ok: false,
            scanning: false,
            scan_seq: 0,
            connect_fails: 0,
            ota: OtaPhase::Idle,
        }
    }
}

/// One scan-result row: (ssid, best RSSI, secured).
pub type ScanRow = (SsidBuf, i8, bool);
/// Published scan-row capacity (the picker shows 6; keep a few spares so
/// dedup across channels has room to pick the strongest).
pub const SCAN_ROWS_CAP: usize = 12;

struct Published {
    snap: NetSnapshot,
    rows: heapless::Vec<ScanRow, SCAN_ROWS_CAP>,
    /// One-shot: a fresh NTP unix time for main to apply (RTC + mesh
    /// authority live there).
    ntp_unix: Option<u32>,
    /// One-shot: a fresh weather sample (temp_f, code) for the shell.
    weather: Option<(i16, u8)>,
}

static STATE: BlockingMutex<CriticalSectionRawMutex, RefCell<Published>> =
    BlockingMutex::new(RefCell::new(Published {
        snap: NetSnapshot::boot(),
        rows: heapless::Vec::new(),
        ntp_unix: None,
        weather: None,
    }));

/// Coalescing wake for the render loop (the STATE_WAKE pattern, v0.8.8):
/// signalled on every published change — phase flips, scan rows, OTA
/// progress, NTP/weather handoffs. Main selects on it next to STATE_WAKE.
pub static NET_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Current snapshot (cheap: one critical section, all `Copy`).
pub fn snapshot() -> NetSnapshot {
    STATE.lock(|s| s.borrow().snap)
}

/// Take the pending NTP result, if any (one-shot).
pub fn take_ntp_unix() -> Option<u32> {
    STATE.lock(|s| s.borrow_mut().ntp_unix.take())
}

/// Take the pending weather sample, if any (one-shot).
pub fn take_weather() -> Option<(i16, u8)> {
    STATE.lock(|s| s.borrow_mut().weather.take())
}

/// Read the current scan rows under the lock (used on a `scan_seq` bump).
pub fn with_scan_rows<R>(f: impl FnOnce(&[ScanRow]) -> R) -> R {
    STATE.lock(|s| f(&s.borrow().rows))
}

fn publish(snap: NetSnapshot) {
    let changed = STATE.lock(|s| {
        let mut p = s.borrow_mut();
        if p.snap != snap {
            p.snap = snap;
            true
        } else {
            false
        }
    });
    if changed {
        NET_WAKE.signal(());
    }
}

fn post_ntp(unix: u32) {
    STATE.lock(|s| s.borrow_mut().ntp_unix = Some(unix));
    NET_WAKE.signal(());
}

fn post_weather(w: (i16, u8)) {
    STATE.lock(|s| s.borrow_mut().weather = Some(w));
    NET_WAKE.signal(());
}

fn post_scan_rows(rows: &heapless::Vec<ScanRow, SCAN_ROWS_CAP>, seq: u32, scanning: bool) {
    STATE.lock(|s| {
        let mut p = s.borrow_mut();
        p.rows.clear();
        for r in rows.iter() {
            let _ = p.rows.push(r.clone());
        }
        p.snap.scan_seq = seq;
        p.snap.scanning = scanning;
    });
    NET_WAKE.signal(());
}

/// OTA progress hook (fn pointer handed to `ota_http::ota_update`): publish
/// `Downloading { pct }` on whole-percent changes only, so a 4 MB image is
/// ~100 wakes, not ~1000.
fn ota_progress(got: u32, total: u32) {
    let pct = if total > 0 {
        ((got as u64 * 100) / total as u64) as u8
    } else {
        0
    };
    let changed = STATE.lock(|s| {
        let mut p = s.borrow_mut();
        let next = OtaPhase::Downloading { pct };
        if p.snap.ota != next {
            p.snap.ota = next;
            true
        } else {
            false
        }
    });
    if changed {
        NET_WAKE.signal(());
    }
}

// ============================================================================
// Policy constants
// ============================================================================

/// One association attempt's budget (the old inline machine's 15 s).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Reconnect backoff schedule under consecutive failures (#53 contract):
/// fail 1 → 2 s, 2 → 10 s, 3 → 60 s, 4+ → 300 s. Reset on success and on
/// user action (fresh hold / SetCreds).
const BACKOFF_SECS: [u64; 4] = [2, 10, 60, 300];
/// A boot/creds burst that can't complete (dead AP, NTP unreachable) drops
/// its intent after this, returning the radio to the mesh — the power/mesh
/// backstop the old 3-attempts-then-give-up (~45 s) provided. 180 s spans
/// the 2 s/10 s/60 s backoff steps (~4 attempts) before the ch6 pin returns.
const BURST_GIVEUP: Duration = Duration::from_secs(180);
/// OTA: how long a queued job waits for WiFi before failing (old executor).
const OTA_WIFI_WINDOW: Duration = Duration::from_secs(45);
/// OTA: download attempts before giving up (old executor's re-arm loop).
const OTA_MAX_ATTEMPTS: u8 = 3;
/// NTP retry cadence while the burst window is open (old inline machine).
const NTP_RETRY: Duration = Duration::from_secs(10);
/// Pace between retries of a failing `set_config` (PHY start). Rare — the
/// call is infallible in practice — but a bare retry loop would busy-spin
/// the executor if the radio glue ever wedges (review F6).
const SET_CONFIG_RETRY: Duration = Duration::from_millis(500);

// --- #57 firmware roaming policy -------------------------------------------
/// A pinned BSSID that fails this many times in a row is dropped in favour of
/// ONE driver-select (AllChannels + sort-by-signal) fallback attempt — so a
/// BSSID that moved/vanished since the candidate scan can't wedge the connect.
const PIN_MAX_FAILS: u8 = 2;
/// Cadence for sampling the connected link RSSI (the roam trigger). Matches
/// the connected idle poll so it costs no extra wakes.
const RSSI_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// Sustained-weak threshold: connected RSSI at/below this for
/// [`ROAM_LOW_SAMPLES`] consecutive samples arms a roam candidate scan.
const ROAM_RSSI_THRESH: i32 = -75;
/// Consecutive sub-threshold samples before a roam scan (≈ N×interval of
/// sustained weak signal — rejects a single fade dip). 4×2 s = 8 s.
const ROAM_LOW_SAMPLES: u8 = 4;
/// Hysteresis: a candidate BSSID must beat the current link by at least this
/// many dB to justify the ~1–3 s reassociation drop (anti-ping-pong).
const ROAM_MARGIN_DB: i32 = 12;
/// Minimum spacing between roams (anti-flap): even sustained-weak won't
/// reassociate more than once per this window.
const ROAM_COOLDOWN: Duration = Duration::from_secs(45);
/// Targeted candidate-scan passes (per-BSSID, filtered to the connect SSID).
/// Same multi-pass rationale as #56's picker sweep — a short dwell misses a
/// present AP — but SSID-filtered so it's fast (only matching APs returned).
const ROAM_SCAN_PASSES: u8 = 2;

fn backoff_for(fails: u8) -> Duration {
    let idx = (fails.saturating_sub(1) as usize).min(BACKOFF_SECS.len() - 1);
    Duration::from_secs(BACKOFF_SECS[idx])
}

// ============================================================================
// The task
// ============================================================================

struct OtaJob {
    url: Option<UrlBuf>,
    attempts: u8,
    queued_at: Instant,
}

struct St {
    holds: u8,
    ssid: SsidBuf,
    pass: PassBuf,
    /// Station config must be (re)applied before the next connect.
    creds_dirty: bool,
    radio_started: bool,
    connected: bool,
    connecting: bool,
    ntp_synced: bool,
    next_ntp: Instant,
    burst_since: Option<Instant>,
    consec_fails: u8,
    backoff_until: Option<Instant>,
    scan_pending: bool,
    scan_seq: u32,
    scanning: bool,
    ota: Option<OtaJob>,
    ota_phase: OtaPhase,
    // #57 firmware roaming: the BSSID+channel to TARGET on the next connect.
    // Filled by a best-BSSID candidate scan (or a roam decision); pinning it
    // makes esp-radio associate to THAT specific AP instead of the first one
    // WIFI_FAST_SCAN happens to hear (the AuthExpired-at-good-RSSI root cause).
    // Cleared on success (re-picked fresh each reconnect — BSSIDs move).
    pinned: Option<([u8; 6], u8)>,
    /// Consecutive failures while a BSSID was pinned; at PIN_MAX_FAILS the pin
    /// is dropped and ONE driver-select (AllChannels+sort-by-signal) fallback
    /// attempt runs before re-scanning — so a vanished/moved BSSID can't wedge.
    pin_fails: u8,
    /// One deliberate SSID-only / AllChannels attempt (the pin-fail fallback).
    force_ssid_only: bool,
    /// Consecutive connected RSSI reads below ROAM_RSSI_THRESH (roam trigger).
    rssi_low: u8,
    /// Next time to sample the connected link RSSI (paces the roam check).
    next_rssi: Instant,
    /// Roam cooldown: no reassociation before this (anti-flap).
    roam_after: Instant,
}

impl St {
    fn has_creds(&self) -> bool {
        !self.ssid.is_empty()
    }
    fn assoc_want(&self) -> bool {
        self.has_creds() && (self.holds & ASSOC_HOLDS) != 0
    }
    fn phy_want(&self) -> bool {
        self.assoc_want() || (self.holds & PHY_HOLDS) != 0 || self.scan_pending
    }
}

fn make_snap(st: &St, stack: Stack<'static>) -> NetSnapshot {
    let phase = if st.connected {
        WifiPhase::Up {
            ip: stack.config_v4().map(|c| c.address.address().octets()),
        }
    } else if st.connecting {
        WifiPhase::Connecting {
            attempt: st.consec_fails.saturating_add(1),
        }
    } else if st.assoc_want() && st.backoff_until.is_some() {
        WifiPhase::Backoff {
            attempt: st.consec_fails,
        }
    } else if st.radio_started {
        WifiPhase::Idle
    } else {
        WifiPhase::Off
    };
    NetSnapshot {
        phase,
        radio_started: st.radio_started,
        wanted: st.assoc_want(),
        // v0.9.1 arbitration: the pin yields to ANY WiFi intent — association,
        // an in-flight connect/scan, a pending OTA. Scanning included (the
        // sweep walks channels); once it ends the verdict returns to true and
        // main re-pins ch6.
        mesh_pin_ok: st.radio_started
            && !st.connected
            && !st.connecting
            && !st.scanning
            && !st.scan_pending
            && !st.assoc_want()
            && st.ota.is_none(),
        scanning: st.scanning,
        scan_seq: st.scan_seq,
        connect_fails: st.consec_fails,
        ota: st.ota_phase,
    }
}

/// The network owner. `boot_connect` mirrors the old boot intent
/// (`wifi_has_creds && !watch_cfg.wifi_off`): raise User+Burst at spawn so
/// the watch auto-connects and time-syncs, then drops the association.
#[embassy_executor::task]
pub async fn net_task(
    mut controller: WifiController<'static>,
    stack: Stack<'static>,
    flash: &'static crate::FlashMutex,
    ssid: SsidBuf,
    pass: PassBuf,
    boot_connect: bool,
) -> ! {
    let now = Instant::now();
    let mut st = St {
        holds: if boot_connect {
            Hold::User.bit() | Hold::Burst.bit()
        } else {
            0
        },
        ssid,
        pass,
        creds_dirty: false,
        radio_started: false,
        connected: false,
        connecting: false,
        ntp_synced: false,
        next_ntp: now,
        burst_since: if boot_connect { Some(now) } else { None },
        consec_fails: 0,
        backoff_until: None,
        scan_pending: false,
        scan_seq: 0,
        scanning: false,
        ota: None,
        ota_phase: OtaPhase::Idle,
        pinned: None,
        pin_fails: 0,
        force_ssid_only: false,
        rssi_low: 0,
        next_rssi: now,
        roam_after: now,
    };
    println!(
        "[NET] task up (holds {:#04x}, creds={})",
        st.holds,
        st.has_creds()
    );

    let mut pending_cmd: Option<NetCmd> = None;
    loop {
        // --- Apply commands (never blocks) --------------------------------
        if let Some(cmd) = pending_cmd.take() {
            apply_cmd(&mut st, cmd);
        }
        while let Ok(cmd) = CMD_CH.try_receive() {
            apply_cmd(&mut st, cmd);
        }

        // --- Reconcile radio ground truth ----------------------------------
        st.connected = st.radio_started && controller.is_connected();

        // Burst give-up: a dead AP must not hold the radio hostage forever.
        if let Some(t0) = st.burst_since {
            if Instant::now().duration_since(t0) > BURST_GIVEUP {
                println!(
                    "[NET] burst gave up ({}s) - releasing wifi intent",
                    BURST_GIVEUP.as_secs()
                );
                st.holds &= !(Hold::Burst.bit() | Hold::User.bit());
                st.burst_since = None;
            }
        }
        // OTA WiFi window: queued but the link never came up.
        if let Some(job) = st.ota.as_ref() {
            let net_ready = st.connected && stack.config_v4().is_some();
            if !net_ready && Instant::now().duration_since(job.queued_at) > OTA_WIFI_WINDOW {
                println!("[OTA] WiFi didn't come up within 45s - giving up");
                st.ota = None;
                st.holds &= !Hold::Ota.bit();
                st.ota_phase = OtaPhase::Failed {
                    msg: "WiFi failed \u{2014} tap to retry",
                };
            }
        }

        publish(make_snap(&st, stack));

        // --- One action per iteration --------------------------------------
        if st.creds_dirty {
            if st.connected {
                let _ = controller.disconnect_async().await;
                st.connected = false;
                println!("[WIFI] disconnected (new credentials)");
            }
            st.creds_dirty = !apply_station_config(&mut controller, &mut st);
            if st.creds_dirty {
                // set_config refused (radio glue wedged): pace the retry —
                // an unconditional `continue` here would busy-spin the
                // executor (review F6).
                Timer::after(SET_CONFIG_RETRY).await;
            }
            continue;
        }
        if st.phy_want() && !st.radio_started {
            if !apply_station_config(&mut controller, &mut st) {
                if st.scan_pending {
                    // Radio refused to start: fail the scan visibly instead
                    // of leaving the picker spinning.
                    st.scan_pending = false;
                    let empty = heapless::Vec::new();
                    st.scan_seq = st.scan_seq.wrapping_add(1);
                    post_scan_rows(&empty, st.scan_seq, false);
                }
                // Pace the retry (review F6): phy_want stays up, so a bare
                // `continue` would spin on a persistently failing set_config.
                Timer::after(SET_CONFIG_RETRY).await;
            }
            continue;
        }
        if st.scan_pending && st.radio_started {
            run_scan(&mut controller, &mut st).await;
            continue;
        }
        if st.assoc_want() && !st.connected {
            if let Some(until) = st.backoff_until {
                let now = Instant::now();
                if now < until {
                    pending_cmd = wait_cmd_or(until.duration_since(now), st.ota.is_some()).await;
                    continue;
                }
                st.backoff_until = None;
            }
            attempt_connect(&mut controller, &mut st, stack).await;
            continue;
        }
        if !st.assoc_want() && st.connected {
            let _ = controller.disconnect_async().await;
            println!("[WIFI] disconnected");
            st.connected = false;
            continue;
        }
        if st.connected && stack.config_v4().is_some() {
            // OTA owns the window (old executor ran it ahead of the mesh
            // re-pin for the same reason).
            if st.ota.is_some() {
                run_ota(stack, flash, &mut st).await;
                continue;
            }
            // Burst on ANY association while time is unsynced (old semantics:
            // `wifi_connected && !ntp_synced` — a manual toggle after a
            // failed boot burst still syncs the clock), not just under the
            // Burst hold; completion drops Burst+User either way.
            if !st.ntp_synced && Instant::now() >= st.next_ntp {
                run_burst(stack, &mut st).await;
                continue;
            }
            // #57 roam: sample the link; a sustained-weak association with a
            // meaningfully stronger same-SSID BSSID available → reassociate.
            // Runs only in the steady state (after burst/OTA) so it never
            // fights the boot window; the reassoc pins the target and the
            // top-of-loop connect machine lands it.
            if maybe_roam(&mut controller, &mut st, stack).await {
                continue;
            }
        }

        // --- Nothing to do: wait for a command / link event / poll tick ----
        pending_cmd = idle_wait(&controller, &st).await;
    }
}

fn apply_cmd(st: &mut St, cmd: NetCmd) {
    match cmd {
        NetCmd::Raise(h) => {
            let fresh = st.holds & h.bit() == 0;
            st.holds |= h.bit();
            if fresh {
                println!("[NET] hold +{} (mask {:#04x})", h.name(), st.holds);
                // A fresh want is a user action: retry NOW, not at the tail
                // of a 300 s backoff (the counter itself only resets on
                // success/User/SetCreds, so a dead AP stays bounded).
                st.backoff_until = None;
                if h == Hold::User {
                    st.consec_fails = 0;
                }
            }
        }
        NetCmd::Drop(h) => {
            let had = st.holds & h.bit() != 0;
            st.holds &= !h.bit();
            if h == Hold::User {
                // A user OFF overrides a pending burst (old toggle-off
                // semantics: it killed the whole association intent).
                st.holds &= !Hold::Burst.bit();
                st.burst_since = None;
            }
            if had {
                println!("[NET] hold -{} (mask {:#04x})", h.name(), st.holds);
            }
        }
        NetCmd::Scan => {
            st.scan_pending = true;
        }
        NetCmd::SetCreds { ssid, pass } => {
            st.ssid = ssid;
            st.pass = pass;
            st.creds_dirty = true;
            // Fresh creds re-arm the burst (NTP/MQTT/weather on the new
            // network) and reset the failure history — user action.
            st.ntp_synced = false;
            st.next_ntp = Instant::now();
            st.consec_fails = 0;
            st.backoff_until = None;
            // A new SSID invalidates any BSSID pin from the old network (#57).
            st.pinned = None;
            st.pin_fails = 0;
            st.force_ssid_only = false;
            st.holds |= Hold::User.bit() | Hold::Burst.bit();
            st.burst_since = Some(Instant::now());
            println!("[NET] credentials set (ssid={:?})", st.ssid.as_str());
        }
        NetCmd::Ota { url } => {
            if st.ota.is_some() || st.ota_phase.active() {
                println!("[OTA] request ignored (update already pending)");
            } else {
                st.ota = Some(OtaJob {
                    url,
                    attempts: 0,
                    queued_at: Instant::now(),
                });
                st.ota_phase = OtaPhase::WaitingWifi;
                st.holds |= Hold::Ota.bit();
                println!("[OTA] queued (hold mask {:#04x})", st.holds);
            }
        }
    }
}

/// Build + apply a station config. `pin = Some((bssid, ch))` targets exactly
/// that AP (bssid_set=true → esp-radio associates to it, NOT the first one
/// WIFI_FAST_SCAN hears — the #57 fix). `all_channels` selects
/// `ScanMethod::AllChannels` so the driver full-scans every channel and
/// sort-by-signal picks the strongest itself — the documented fallback when a
/// pin fails or no candidate was found. `auth_method` stays the default
/// Wpa2Personal (threshold floor WPA2-PSK; esp-radio has no FT surface, so
/// this is always plain 00-0F-AC:2 — never FT-PSK).
fn set_sta_config(
    controller: &mut WifiController<'static>,
    ssid: &str,
    pass: &str,
    pin: Option<([u8; 6], u8)>,
    all_channels: bool,
) -> bool {
    let mut cfg = StationConfig::default()
        .with_ssid(esp_radio::wifi::Ssid::from(ssid))
        .with_password(pass.into());
    if let Some((bssid, ch)) = pin {
        cfg = cfg.with_bssid(bssid).with_channel(ch);
    } else if all_channels {
        cfg = cfg.with_scan_method(esp_radio::wifi::sta::ScanMethod::AllChannels);
    }
    match controller.set_config(&esp_radio::wifi::Config::Station(cfg)) {
        Ok(()) => {
            let _ = controller.set_power_saving(esp_radio::wifi::PowerSaveMode::Minimum);
            true
        }
        Err(e) => {
            println!("[NET] set_config failed: {e:?}");
            false
        }
    }
}

/// (Re)apply the station config for PHY start / creds change — SSID-only, no
/// pin (starting the PHY or swapping creds doesn't need a target; the pin is
/// chosen per-attempt in `attempt_connect`). This is what starts the PHY in
/// esp-radio 0.18.
fn apply_station_config(controller: &mut WifiController<'static>, st: &mut St) -> bool {
    let ok = set_sta_config(controller, st.ssid.as_str(), st.pass.as_str(), None, false);
    if ok {
        if !st.radio_started {
            println!("[NET] STA radio started");
        }
        st.radio_started = true;
    }
    ok
}

/// Targeted candidate scan: SSID-filtered (only matching APs returned → fast),
/// multi-pass merged (strongest RSSI per BSSID — same short-dwell rationale as
/// #56's picker sweep). Returns the strongest (bssid, channel, rssi) for the
/// connect SSID, or None if none was heard. SSID-agnostic: whatever `ssid` is
/// (`roam`, `jplovescl`, …), it picks the strongest BSSID advertising it.
async fn scan_candidates(
    controller: &mut WifiController<'static>,
    ssid: &str,
) -> Option<([u8; 6], u8, i8)> {
    let mut best: Option<([u8; 6], u8, i8)> = None;
    for _ in 0..ROAM_SCAN_PASSES {
        let cfg = esp_radio::wifi::scan::ScanConfig::default()
            .with_ssid(esp_radio::wifi::Ssid::from(ssid));
        match controller.scan_async(&cfg).await {
            Ok(aps) => {
                for ap in aps.iter() {
                    if ap.ssid.as_str() != ssid {
                        continue; // defensive: filter is best-effort in the driver
                    }
                    if best.map(|(_, _, r)| ap.signal_strength > r).unwrap_or(true) {
                        best = Some((ap.bssid, ap.channel, ap.signal_strength));
                    }
                }
            }
            Err(e) => println!("[NET] candidate scan failed: {e:?}"),
        }
    }
    best
}

/// One bounded association attempt + backoff bookkeeping, with #57 best-BSSID
/// targeting: unless a pin is already set (by a prior scan or a roam
/// decision) or we're in the one-shot driver-select fallback, run a candidate
/// scan and pin the STRONGEST BSSID for our SSID — so esp-radio associates to
/// the near strong AP instead of whatever WIFI_FAST_SCAN hears first.
async fn attempt_connect(
    controller: &mut WifiController<'static>,
    st: &mut St,
    stack: Stack<'static>,
) {
    // Choose the target. force_ssid_only = the deliberate driver-select
    // fallback after a pin kept failing; otherwise pin the strongest BSSID.
    let all_channels = st.force_ssid_only;
    if !st.force_ssid_only && st.pinned.is_none() {
        if let Some((bssid, ch, rssi)) = scan_candidates(controller, st.ssid.as_str()).await {
            println!(
                "[NET] best BSSID {} ch{} rssi{} (pinning)",
                fmt_bssid(&bssid),
                ch,
                rssi
            );
            st.pinned = Some((bssid, ch));
        } else {
            // Nothing heard on a targeted scan — let the driver full-scan +
            // sort-by-signal itself rather than fast-scan-first-hit.
            println!("[NET] no candidate BSSID heard - driver AllChannels select");
            st.force_ssid_only = true;
        }
    }
    let pin = st.pinned;
    let use_all_channels = all_channels || st.force_ssid_only;
    if !set_sta_config(
        controller,
        st.ssid.as_str(),
        st.pass.as_str(),
        pin,
        use_all_channels,
    ) {
        // Config apply failed — treat as a normal attempt failure (backoff).
        st.consec_fails = st.consec_fails.saturating_add(1);
        st.backoff_until = Some(Instant::now() + backoff_for(st.consec_fails));
        return;
    }

    st.connecting = true;
    publish(make_snap(st, stack)); // show Connecting before the 15 s wait
    let result = with_timeout(CONNECT_TIMEOUT, controller.connect_async()).await;
    st.connecting = false;
    match result {
        Ok(Ok(_)) => {
            match pin {
                Some((b, ch)) => println!("[WIFI] connected (BSSID {} ch{ch})", fmt_bssid(&b)),
                None => println!("[WIFI] connected (driver select)"),
            }
            st.connected = true;
            st.consec_fails = 0;
            st.pin_fails = 0;
            st.backoff_until = None;
            // Re-pick fresh next reconnect (BSSIDs move); clear the fallback.
            st.pinned = None;
            st.force_ssid_only = false;
            // Arm the roam sampler fresh (don't roam the instant we land).
            st.rssi_low = 0;
            st.next_rssi = Instant::now() + RSSI_SAMPLE_INTERVAL;
            st.roam_after = Instant::now() + ROAM_COOLDOWN;
        }
        other => {
            st.consec_fails = st.consec_fails.saturating_add(1);
            match other {
                Ok(Err(e)) => println!(
                    "[WIFI] connect error (attempt {}): {e:?}",
                    st.consec_fails
                ),
                _ => println!("[WIFI] connect timeout (attempt {})", st.consec_fails),
            }
            // Pin bookkeeping: a pinned BSSID that keeps failing (moved,
            // congested, gone) is dropped for ONE driver-select fallback,
            // then we re-scan fresh.
            if pin.is_some() {
                st.pin_fails = st.pin_fails.saturating_add(1);
                if st.pin_fails >= PIN_MAX_FAILS {
                    println!(
                        "[NET] pinned BSSID failed {}x - falling back to driver select",
                        st.pin_fails
                    );
                    st.pinned = None;
                    st.pin_fails = 0;
                    st.force_ssid_only = true;
                }
            } else {
                // The driver-select fallback itself failed: clear it so the
                // next attempt re-scans for a fresh candidate.
                st.force_ssid_only = false;
            }
            let b = backoff_for(st.consec_fails);
            println!(
                "[WIFI] backoff {}s (consecutive fails: {})",
                b.as_secs(),
                st.consec_fails
            );
            st.backoff_until = Some(Instant::now() + b);
        }
    }
}

/// Format a BSSID for logs (`a4:2b:b0:b7:93:2e`).
fn fmt_bssid(b: &[u8; 6]) -> heapless::String<17> {
    use core::fmt::Write as _;
    let mut s = heapless::String::new();
    let _ = write!(
        s,
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5]
    );
    s
}

/// #57 RSSI-triggered roam: sample the connected link; if it stays weak, scan
/// for a same-SSID BSSID that's meaningfully stronger and reassociate to it.
/// Returns true if it initiated a reassociation (caller should `continue` so
/// the connect machine re-associates to the freshly pinned BSSID). Bounded by
/// [`ROAM_COOLDOWN`] and gated by hysteresis ([`ROAM_MARGIN_DB`]) so it never
/// ping-pongs. For a radio with no 802.11r this IS roaming (a brief drop).
async fn maybe_roam(
    controller: &mut WifiController<'static>,
    st: &mut St,
    stack: Stack<'static>,
) -> bool {
    let now = Instant::now();
    if now < st.next_rssi {
        return false;
    }
    st.next_rssi = now + RSSI_SAMPLE_INTERVAL;

    let cur_rssi = match controller.rssi() {
        Ok(r) => r,
        Err(_) => return false,
    };
    if cur_rssi > ROAM_RSSI_THRESH {
        st.rssi_low = 0;
        return false;
    }
    st.rssi_low = st.rssi_low.saturating_add(1);
    if st.rssi_low < ROAM_LOW_SAMPLES || now < st.roam_after {
        return false; // not sustained yet, or still cooling down
    }

    // Sustained weak: is there a better AP for our SSID? Current BSSID (from
    // the last beacon) excludes roaming to ourselves.
    let cur_bssid = controller.ap_info().ok().map(|i| i.bssid);
    st.rssi_low = 0; // consume the trigger regardless of outcome
    let Some((bssid, ch, cand_rssi)) = scan_candidates(controller, st.ssid.as_str()).await else {
        st.roam_after = now + ROAM_COOLDOWN;
        return false;
    };
    let same = cur_bssid == Some(bssid);
    if same || (cand_rssi as i32) < cur_rssi + ROAM_MARGIN_DB {
        // No candidate worth the ~1-3 s drop — hold and cool down.
        st.roam_after = now + ROAM_COOLDOWN;
        return false;
    }

    println!(
        "[ROAM] {}→{} switching {}→{} ch{}",
        cur_rssi,
        cand_rssi,
        cur_bssid.map(|b| fmt_bssid(&b)).unwrap_or_else(|| heapless::String::new()),
        fmt_bssid(&bssid),
        ch
    );
    let _ = controller.disconnect_async().await;
    st.connected = false;
    st.pinned = Some((bssid, ch)); // attempt_connect targets it (no re-scan)
    st.force_ssid_only = false;
    st.consec_fails = 0; // a roam-triggered reconnect resets the backoff
    st.backoff_until = None;
    st.roam_after = now + ROAM_COOLDOWN;
    true
}

/// Streaming scan sweep: one `scan_async` per 2.4 GHz channel, rows published
/// (dedup'd by SSID keeping the strongest, RSSI-sorted) after every channel —
/// the picker fills in live under its scanning animation.
async fn run_scan(controller: &mut WifiController<'static>, st: &mut St) {
    println!("[NET] scan sweep starting");
    st.scan_pending = false;
    st.scanning = true;
    let mut rows: heapless::Vec<ScanRow, SCAN_ROWS_CAP> = heapless::Vec::new();
    st.scan_seq = st.scan_seq.wrapping_add(1);
    post_scan_rows(&rows, st.scan_seq, true);
    // #56: esp-radio 0.18's default active dwell is 10-20ms/channel — shorter
    // than a ~100ms beacon interval, so a single sweep routinely MISSES a
    // present AP (mythic-throne dropped "roam" from most single-pass snapshots
    // while it was demonstrably reachable). scan_type is pub(crate) with no
    // builder, so we can't lengthen the dwell directly; instead sweep each
    // channel SCAN_PASSES times and merge (strongest RSSI wins) — effectively
    // multiplying beacon-catch odds. 2 passes ≈ doubles per-AP visibility for
    // a ~2x sweep-time cost (still a few seconds, off the hot path).
    const SCAN_PASSES: u8 = 2;
    for pass in 0..SCAN_PASSES {
    for ch in 1..=13u8 {
        let cfg = esp_radio::wifi::scan::ScanConfig::default().with_channel(ch);
        match controller.scan_async(&cfg).await {
            Ok(aps) => {
                let mut changed = false;
                for ap in aps.iter() {
                    let ssid = ap.ssid.as_str();
                    if ssid.is_empty() {
                        continue; // hidden — the manual entry row covers these
                    }
                    let secured =
                        ap.auth_method != Some(esp_radio::wifi::AuthenticationMethod::None);
                    if let Some(row) = rows.iter_mut().find(|r| r.0.as_str() == ssid) {
                        if ap.signal_strength > row.1 {
                            row.1 = ap.signal_strength;
                            row.2 = secured;
                            changed = true;
                        }
                    } else if !rows.is_full() {
                        let mut s = SsidBuf::new();
                        let _ = s.push_str(ssid);
                        let _ = rows.push((s, ap.signal_strength, secured));
                        changed = true;
                    }
                }
                if changed {
                    rows.sort_unstable_by(|a, b| b.1.cmp(&a.1));
                    st.scan_seq = st.scan_seq.wrapping_add(1);
                    post_scan_rows(&rows, st.scan_seq, true);
                }
            }
            Err(e) => println!("[NET] scan ch{ch} failed: {e:?}"),
        }
    }
    let _ = pass;
    }
    st.scanning = false;
    st.scan_seq = st.scan_seq.wrapping_add(1);
    post_scan_rows(&rows, st.scan_seq, false);
    println!("[NET] scan sweep done: {} networks", rows.len());
}

/// One-shot SNTP query (the socket half of the old `ntp_sync`; applying the
/// result to the RTC + mesh authority stays in main, which owns both).
async fn ntp_query(stack: Stack<'static>) -> Result<u32, ()> {
    use embassy_net::udp::{PacketMetadata, UdpSocket};

    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx_buf = [0u8; 256];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buf = [0u8; 256];

    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    socket.bind(12345).map_err(|_| ())?;

    let mut ntp_request = [0u8; 48];
    ntp_request[0] = 0x1B; // LI=0, VN=3, Mode=3 (client)

    // time.google.com anycast (no DNS needed)
    let ntp_addr = embassy_net::Ipv4Address::new(216, 239, 35, 0);
    socket
        .send_to(&ntp_request, (ntp_addr, 123))
        .await
        .map_err(|_| ())?;

    let mut response = [0u8; 48];
    match with_timeout(Duration::from_secs(5), socket.recv_from(&mut response)).await {
        Ok(Ok((len, _addr))) if len >= 48 => {
            let ntp_secs =
                u32::from_be_bytes([response[40], response[41], response[42], response[43]]);
            Ok(ntp_secs.wrapping_sub(2_208_988_800))
        }
        _ => Err(()),
    }
}

/// The boot burst: NTP (posted to main for RTC + mesh authority), the HA MQTT
/// burst, and the weather fetch — all in one WiFi window, exactly the old
/// inline sequence. Completing it releases the Burst AND User holds (the old
/// `wifi_on_request = false` auto-off); an NTP miss retries in 10 s.
async fn run_burst(stack: Stack<'static>, st: &mut St) {
    match ntp_query(stack).await {
        Ok(unix) => {
            st.ntp_synced = true;
            st.burst_since = None;
            post_ntp(unix);
            println!("[NTP] unix={unix} - posted to main (RTC + mesh authority)");
            // MQTT burst to Home Assistant while the window is open.
            // Fire-and-forget, internally bounded (~5 s worst case).
            let batt = crate::peripherals::ble::BATTERY_PERCENT.load(Ordering::Relaxed);
            crate::net::mqtt_ha::publish_burst(stack, batt).await;
            // Weather in the same window (bounded at 8 s; logs and moves on).
            if let Some(wx) = crate::net::weather::fetch(stack).await {
                post_weather((wx.temp_f, wx.code));
            }
            st.holds &= !(Hold::Burst.bit() | Hold::User.bit());
            println!("[NET] burst complete - wifi intent released");
        }
        Err(()) => {
            println!("[NTP] failed, retrying in 10s");
            st.next_ntp = Instant::now() + NTP_RETRY;
        }
    }
}

/// One OTA download attempt with the old executor's re-arm semantics.
async fn run_ota(stack: Stack<'static>, flash: &'static crate::FlashMutex, st: &mut St) {
    let (url, attempt) = match st.ota.as_ref() {
        Some(job) => (job.url.clone(), job.attempts + 1),
        None => return,
    };
    println!("[OTA] download starting (attempt {attempt}/{OTA_MAX_ATTEMPTS})");
    st.ota_phase = OtaPhase::Downloading { pct: 0 };
    publish(make_snap(st, stack));
    match ota_http::ota_update(stack, flash, url.as_deref(), ota_progress).await {
        Ok(()) => {
            println!("[OTA] staged - main reboots to apply");
            st.ota = None;
            st.holds &= !Hold::Ota.bit();
            st.ota_phase = OtaPhase::Staged;
        }
        Err(e) => {
            let give_up = {
                let job = st.ota.as_mut().expect("ota job present");
                job.attempts += 1;
                println!(
                    "[OTA] attempt {}/{} failed: {e}",
                    job.attempts, OTA_MAX_ATTEMPTS
                );
                if job.attempts < OTA_MAX_ATTEMPTS {
                    // Re-arm: keep the Ota hold so the connect machine
                    // reconnects under the pending job (the mid-download WiFi
                    // recovery the old blocking executor couldn't do), with a
                    // fresh 45 s window.
                    job.queued_at = Instant::now();
                    false
                } else {
                    true
                }
            };
            if give_up {
                st.ota = None;
                st.holds &= !Hold::Ota.bit();
                st.ota_phase = OtaPhase::Failed { msg: e };
            } else {
                let attempts = st.ota.as_ref().map(|j| j.attempts).unwrap_or(0);
                st.ota_phase = OtaPhase::Retrying { attempt: attempts };
            }
        }
    }
}

/// Wait out (part of) a backoff, interruptible by a command. Capped at 5 s
/// while an OTA is queued so its 45 s WiFi window stays accurate.
async fn wait_cmd_or(d: Duration, ota_pending: bool) -> Option<NetCmd> {
    let d = if ota_pending {
        d.min(Duration::from_secs(5))
    } else {
        d
    };
    match select(CMD_CH.receive(), Timer::after(d)).await {
        Either::First(cmd) => Some(cmd),
        Either::Second(()) => None,
    }
}

/// Nothing actionable: park on the command queue, plus a link-loss wake while
/// associated (1 s poll backstop) or a slow tick while an OTA waits on WiFi.
/// Fully idle (radio parked, no jobs) waits on commands alone — zero wakes.
async fn idle_wait(controller: &WifiController<'static>, st: &St) -> Option<NetCmd> {
    if st.connected {
        match select3(
            CMD_CH.receive(),
            controller.wait_for_disconnect_async(),
            Timer::after(Duration::from_secs(1)),
        )
        .await
        {
            Either3::First(cmd) => Some(cmd),
            Either3::Second(_) => {
                println!("[WIFI] link lost - will reconnect");
                None
            }
            Either3::Third(()) => None,
        }
    } else {
        let tick = if st.ota.is_some() {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(60)
        };
        match select(CMD_CH.receive(), Timer::after(tick)).await {
            Either::First(cmd) => Some(cmd),
            Either::Second(()) => None,
        }
    }
}
