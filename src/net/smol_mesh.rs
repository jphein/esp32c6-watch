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

/// The smol default TIME-SHARE mesh channel (mode.rs ESP_NOW_FIXED_CHANNEL).
pub const MESH_CHANNEL: u8 = 6;
/// Peer link decays after this much silence (protocol.md PEER_STALE_MS).
const PEER_STALE_MS: u64 = 3000;
/// HELLO/TIME cadence.
pub const TICK_MS: u64 = 2000;

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
}

pub enum MeshEvent {
    /// Adopted a fresher mesh time; payload is the new Unix time. The caller
    /// should set the RTC from it.
    TimeAdopted { unix: u32, from_id: u8 },
    /// A decoded SMOLv1 FAM frame (+ its RSSI, which weights the familiar's
    /// orphan-takeover stagger). Route to `FamState::ingest`.
    Fam { frame: FamFrame, rssi: i32 },
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

    fn upsert_peer(&mut self, mac: [u8; 6], id: Option<u8>, now_ms: u64) -> bool {
        if let Some(p) = self.peers.iter_mut().find(|p| p.mac == mac) {
            p.last_rx_ms = now_ms;
            if id.is_some() {
                p.id = id;
            }
            false
        } else {
            self.peers.push(Peer {
                mac,
                id,
                last_rx_ms: now_ms,
            });
            true
        }
    }

    /// The ~2s HELLO/TIME tick. Call from the main loop; no-ops until due.
    pub fn tick(&mut self, esp_now: &mut EspNow<'_>, now_ms: u64, uptime_secs: u64) {
        if now_ms.saturating_sub(self.last_tick_ms) < TICK_MS {
            return;
        }
        self.last_tick_ms = now_ms;

        let mut hello = [0u8; 16];
        hello[..HELLO_PREFIX.len()].copy_from_slice(HELLO_PREFIX);
        write_id(self.id, &mut hello[HELLO_PREFIX.len()..]);
        if let Ok(w) = esp_now.send(&esp_radio::esp_now::BROADCAST_ADDRESS, &hello) {
            let _ = w.wait();
        }

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
            if let Ok(w) = esp_now.send(&esp_radio::esp_now::BROADCAST_ADDRESS, &time) {
                let _ = w.wait();
            }
        }
    }

    /// Broadcast a DIAG record ("SMOLv1 DIAG NNN" + verbatim key=val record).
    /// The fleet gateway caches it and republishes retained to smol/<id>/diag,
    /// which is how the watch shows up in Home Assistant without MQTT.
    pub fn broadcast_diag(&mut self, esp_now: &mut EspNow<'_>, record: &[u8]) {
        const DIAG_PREFIX: &[u8] = b"SMOLv1 DIAG ";
        let mut msg = [0u8; 250];
        msg[..DIAG_PREFIX.len()].copy_from_slice(DIAG_PREFIX);
        write_id(self.id, &mut msg[DIAG_PREFIX.len()..]);
        let base = DIAG_PREFIX.len() + 3;
        let n = record.len().min(250 - base);
        msg[base..base + n].copy_from_slice(&record[..n]);
        if let Ok(w) = esp_now.send(&esp_radio::esp_now::BROADCAST_ADDRESS, &msg[..base + n]) {
            let _ = w.wait();
        }
    }

    /// Broadcast a SMOLv1 FAM frame (heartbeat/handoff) for the familiar
    /// state machine. Fixed 29-byte binary frame, fleet wire format.
    pub fn broadcast_fam(&mut self, esp_now: &mut EspNow<'_>, f: &FamFrame) {
        let mut buf = [0u8; FAM_FRAME_LEN];
        if let Some(len) = encode_fam(f, &mut buf) {
            if let Ok(w) = esp_now.send(&esp_radio::esp_now::BROADCAST_ADDRESS, &buf[..len]) {
                let _ = w.wait();
            }
        }
    }

    /// Handle one received ESP-NOW payload. `rssi` comes from the frame's RX
    /// control info and feeds the familiar's takeover weighting.
    pub fn handle_rx(
        &mut self,
        esp_now: &mut EspNow<'_>,
        src: [u8; 6],
        data: &[u8],
        rssi: i32,
        now_ms: u64,
        uptime_secs: u64,
    ) -> Option<MeshEvent> {
        if let Some(rest) = data.strip_prefix(HELLO_PREFIX) {
            let id = parse_id(rest)?;
            let new = self.upsert_peer(src, Some(id), now_ms);
            if new {
                println!("[MESH] hello from id{id} {src:02x?}");
            }
            Self::ensure_unicast_peer(esp_now, src);
            // Reply with a unicast ACK echoing the sender's id.
            let mut ack = [0u8; 14];
            ack[..ACK_PREFIX.len()].copy_from_slice(ACK_PREFIX);
            write_id(id, &mut ack[ACK_PREFIX.len()..]);
            if let Ok(w) = esp_now.send(&src, &ack) {
                let _ = w.wait();
            }
            return None;
        }
        if let Some(rest) = data.strip_prefix(ACK_PREFIX) {
            let id = parse_id(rest)?;
            self.upsert_peer(src, None, now_ms);
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
            self.upsert_peer(src, Some(id), now_ms);
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
        if data.starts_with(FAM_PREFIX) {
            if let Some(f) = parse_fam(data) {
                // The broadcaster is the holder for H/X frames, the caller
                // for C frames (mirrors the fleet's mode.rs routing).
                let sender_id = if f.kind == FAM_CALL { f.target } else { f.holder };
                self.upsert_peer(src, Some(sender_id), now_ms);
                return Some(MeshEvent::Fam { frame: f, rssi });
            }
            // Malformed FAM: still proof of life.
            self.upsert_peer(src, None, now_ms);
            return None;
        }
        if data.starts_with(SMOL_PREFIX) {
            // A fleet frame we don't speak yet (SNK/RELAY/CFG/...):
            // still proof of life for the peer.
            self.upsert_peer(src, None, now_ms);
            self.other_frames_heard += 1;
        }
        None
    }
}
