//! smol #490: the CFG `B`/`O` network overrides at runtime — one small
//! blackboard between main (which owns the CFG apply + flash persistence),
//! the MQTT session tasks (which dial the broker), and the HTTP OTA fetch
//! (which gates the image host).
//!
//! Main seeds it from the loaded [`WatchConfig`](crate::peripherals::config::WatchConfig)
//! at boot and updates it on a CFG apply; everyone else only reads (plus the
//! MQTT tasks' session verdicts, which feed the fallback ratchet). All state
//! is `Copy` behind one blocking mutex — the same shape as `net_task`'s
//! snapshot.
//!
//! ## The fallback ratchet (fleet `NetCfg::broker_fallback`, mirrored)
//! A broker override must never be able to brick the MQTT link: after
//! [`BROKER_FALLBACK_AFTER`] consecutive failed sessions on the override leg,
//! [`broker`] starts answering `None` (→ the baked broker) and the trip is
//! surfaced to main via [`take_fallback_tripped`] so it can persist the flag.
//! The override VALUE is kept — a re-sent identical CFG-`B` stays a no-op and
//! a changed one still edge-triggers, exactly the fleet's contract.

use core::cell::Cell;
use embassy_sync::blocking_mutex::{raw::CriticalSectionRawMutex, Mutex as BlockingMutex};

/// Consecutive failed MQTT sessions on the override leg before falling back
/// to the baked broker. Matches Settings' own "failed at >= 3" convention
/// (`NetSnapshot::connect_fails`).
const BROKER_FALLBACK_AFTER: u8 = 3;

#[derive(Clone, Copy, Default)]
struct Ovr {
    broker: Option<([u8; 4], u16)>,
    fallback: bool,
    /// Fallback just tripped; main collects it via `take_fallback_tripped`
    /// to persist the flag (the tasks can't touch flash — main owns it).
    fallback_tripped: bool,
    fails: u8,
    ota_host: Option<[u8; 4]>,
}

static OVR: BlockingMutex<CriticalSectionRawMutex, Cell<Ovr>> =
    BlockingMutex::new(Cell::new(Ovr {
        broker: None,
        fallback: false,
        fallback_tripped: false,
        fails: 0,
        ota_host: None,
    }));

fn with<R>(f: impl FnOnce(&mut Ovr) -> R) -> R {
    OVR.lock(|c| {
        let mut v = c.get();
        let r = f(&mut v);
        c.set(v);
        r
    })
}

/// Boot seed / CFG-`B` apply. Setting a NEW value (or clearing) resets the
/// fallback ratchet — the operator just expressed fresh intent.
pub fn set_broker(addr: Option<([u8; 4], u16)>) {
    with(|o| {
        o.broker = addr;
        o.fallback = false;
        o.fallback_tripped = false;
        o.fails = 0;
    });
}

/// Boot re-seed of a PERSISTED tripped fallback (keeps the value + the trip,
/// without re-arming `fallback_tripped`).
pub fn seed(broker: Option<([u8; 4], u16)>, fallback: bool, ota_host: Option<[u8; 4]>) {
    with(|o| {
        o.broker = broker;
        o.fallback = fallback;
        o.ota_host = ota_host;
    });
}

/// The broker leg the NEXT session should dial: the override, unless none is
/// set or the fallback ratchet tripped (→ `None` = the baked `MQTT_BROKER`).
pub fn broker() -> Option<([u8; 4], u16)> {
    with(|o| if o.fallback { None } else { o.broker })
}

/// The stored override + fallback flag, for edge-trigger comparison and logs
/// (unlike [`broker`], this shows the value even while fallen back).
pub fn broker_stored() -> (Option<([u8; 4], u16)>, bool) {
    with(|o| (o.broker, o.fallback))
}

/// MQTT session verdict from the session tasks. Only sessions that DIALED
/// THE OVERRIDE count toward the ratchet (`used_override` = what the task
/// actually connected to, so a baked-leg failure can never trip it).
pub fn note_mqtt_session(used_override: bool, ok: bool) {
    with(|o| {
        if !used_override || o.broker.is_none() || o.fallback {
            return;
        }
        if ok {
            o.fails = 0;
        } else {
            o.fails = o.fails.saturating_add(1);
            if o.fails >= BROKER_FALLBACK_AFTER {
                o.fallback = true;
                o.fallback_tripped = true;
            }
        }
    })
}

/// One-shot: the fallback ratchet tripped since the last call. Main persists
/// the flag (config v8) and logs — the readback half of the fleet contract.
pub fn take_fallback_tripped() -> bool {
    with(|o| core::mem::take(&mut o.fallback_tripped))
}

/// CFG-`O` apply / boot seed of the extra allowed OTA image-host.
pub fn set_ota_host(host: Option<[u8; 4]>) {
    with(|o| o.ota_host = host);
}

/// The OTA fetch gate (fleet `ota_allowed`, GUI shape): the watch has no
/// baked `ota_hosts` allowlist — its announce URL is the source of truth and
/// the image is ed25519-manifest-gated regardless (#489) — so the host gate
/// is the fleet's on-LAN guard: RFC1918, or the one CFG-`O` override host.
/// (The override is itself RFC1918-gated at parse today, so its real effect
/// begins if this blanket ever narrows — stated in #490 rather than implied.)
pub fn ota_host_allowed(ip: [u8; 4]) -> bool {
    crate::net::smol_mesh::is_rfc1918(ip) || with(|o| o.ota_host == Some(ip))
}
