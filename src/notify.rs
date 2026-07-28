//! Notification center (#32): a bounded ring of on-wrist notifications behind
//! a blocking critical-section cell (the [`ota_http::PENDING_ANNOUNCE`]
//! pattern — single producer sites, single consumer, never held across an
//! await), so BOTH producers can push:
//!
//! - the main loop (system events: OTA failed, low battery, WiFi failed), and
//! - the MQTT climate-session task ([`handle_mqtt`] — `watch/notify` fleet +
//!   `watch/<sigil>/notify` per-device topics, payload `NOTIFY|<title>|<body>`).
//!
//! Newest-first, drop-oldest at [`CAP`]. Statics total ~1.3KB of .bss (8 ×
//! ~150B + cells) — accounted against the stack budget (stack = `_stack_start`
//! − `_bss_end`; gap stays well over the 46KB floor).
//!
//! Timestamps are wall-clock (PCF85063 day + seconds-of-day, fed by the main
//! loop's 1Hz RTC read via [`set_wall_clock`]) rather than `embassy_time` —
//! AOD light-sleep freezes embassy-time, which would silently stretch ages.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use esp_println::println;

/// Ring capacity (bounded, drop-oldest).
pub const CAP: usize = 8;
pub const TITLE_CAP: usize = 32;
pub const BODY_CAP: usize = 96;

/// Where a notification came from — drives the card glyph in the shade
/// (ui/slint/shade.slint `NotifGlyph`, same ids).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Generic system event. No Rust producer yet — reserved for the next
    /// println-only events worth surfacing (NTP failures, mesh peer loss);
    /// the shade's glyph 0 is already drawn for it.
    #[allow(dead_code)]
    System = 0,
    /// Battery (low-charge warning).
    Battery = 1,
    /// Firmware update (OTA failures).
    Ota = 2,
    /// WiFi (connect gave up after retries).
    Wifi = 3,
    /// Home Assistant / MQTT (`watch/notify`).
    Ha = 4,
}

/// One notification. `day`/`sod` are the PCF85063 stamp at arrival
/// (day-of-month + seconds-of-day) for age display.
#[derive(Clone)]
pub struct Notification {
    pub source: Source,
    pub title: heapless::String<TITLE_CAP>,
    pub body: heapless::String<BODY_CAP>,
    pub day: u8,
    pub sod: u32,
}

/// The ring: newest first (index 0 is the latest).
static RING: BlockingMutex<CriticalSectionRawMutex, RefCell<heapless::Vec<Notification, CAP>>> =
    BlockingMutex::new(RefCell::new(heapless::Vec::new()));

/// Latest arrival, for the screen-on toast: the title, plus the
/// `embassy_time` millis it was posted so a drain that happens much later
/// (e.g. after a game exits) can skip a stale toast. Overwritten by bursts —
/// the badge carries the real count.
static ARRIVAL: BlockingMutex<
    CriticalSectionRawMutex,
    RefCell<Option<(heapless::String<TITLE_CAP>, u64)>>,
> = BlockingMutex::new(RefCell::new(None));

/// Unread count: bumped per push, zeroed when the shade opens. Saturates at
/// 99 (the badge shows a number, not a ledger).
static UNREAD: AtomicU32 = AtomicU32::new(0);

/// Wall clock for arrival stamps, packed `day << 20 | sod` (sod < 86_400 fits
/// 17 bits). Fed by the main loop's RTC reads; 0 = RTC not read yet.
static WALL: AtomicU32 = AtomicU32::new(0);

/// Feed the wall clock (main loop, on every RTC read — both the 1Hz tick and
/// the AOD wake path).
pub fn set_wall_clock(day: u8, sod: u32) {
    WALL.store((day as u32) << 20 | (sod & 0xF_FFFF), Ordering::Relaxed);
}

/// Post a notification. Truncates to the caps (ASCII-sanitized: the shade's
/// embedded glyph sets are per-size and .slint-literal driven, so a stray
/// non-ASCII char from MQTT would render blank — replaced with '?').
/// Consecutive-duplicate suppression: a retained MQTT notify is re-delivered
/// on every session (re)subscribe, and it must not stack copies.
pub fn push(source: Source, title: &str, body: &str) {
    let title = sanitize::<TITLE_CAP>(title);
    let body = sanitize::<BODY_CAP>(body);
    let fresh = RING.lock(|cell| {
        let mut ring = cell.borrow_mut();
        if let Some(head) = ring.first() {
            if head.source == source && head.title == title && head.body == body {
                return false; // duplicate of the newest — drop quietly
            }
        }
        if ring.is_full() {
            ring.pop();
        }
        let wall = WALL.load(Ordering::Relaxed);
        let _ = ring.insert(
            0,
            Notification {
                source,
                title: title.clone(),
                body,
                day: (wall >> 20) as u8,
                sod: wall & 0xF_FFFF,
            },
        );
        true
    });
    if !fresh {
        return;
    }
    let _ = UNREAD.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some((n + 1).min(99))
    });
    ARRIVAL.lock(|cell| {
        cell.borrow_mut()
            .replace((title, embassy_time::Instant::now().as_millis()))
    });
    println!("[NOTIFY] posted ({source:?}, {} unread)", unread());
}

/// Parse + post an MQTT notify payload: `NOTIFY|<title>|<body>` (body may
/// contain further '|' — kept verbatim). Malformed frames are logged and
/// dropped, never a panic. Called from the climate session's PUBLISH dispatch.
pub fn handle_mqtt(payload: &[u8]) {
    let Ok(text) = core::str::from_utf8(payload) else {
        println!("[NOTIFY] rejected (not utf-8)");
        return;
    };
    // An empty retained-clear (`mosquitto_pub -r -n`) is not a notification.
    if text.is_empty() {
        return;
    }
    let mut parts = text.splitn(3, '|');
    let (Some("NOTIFY"), Some(title)) = (parts.next(), parts.next()) else {
        println!("[NOTIFY] rejected (malformed: {text})");
        return;
    };
    push(Source::Ha, title, parts.next().unwrap_or(""));
}

/// Take the latest arrival (clears it): `(title, posted_at_ms)`. The caller
/// toasts only if the screen is on AND the arrival is fresh.
pub fn take_arrival() -> Option<(heapless::String<TITLE_CAP>, u64)> {
    ARRIVAL.lock(|cell| cell.borrow_mut().take())
}

/// Clone the newest notification (index 0), or `None` when the ring is empty.
///
/// Read-aloud (#read-aloud) needs the BODY, which [`take_arrival`] doesn't
/// carry — and it must not compose speech while holding this lock. So: one
/// memcpy-sized clone under the critical section, all the string work outside.
/// (The chime regression in #58 was a copy LOOP left inside a critical section
/// starving the I2S DMA; a plain clone is the shape that was always fine.)
#[cfg(feature = "tts")]
pub fn newest() -> Option<Notification> {
    RING.lock(|cell| cell.borrow().first().cloned())
}

/// Human label for a source, used as the spoken prefix ("Home Assistant.") so
/// a listener gets the context a sighted user reads off the card glyph.
#[cfg(feature = "tts")]
pub fn source_label(source: Source) -> &'static str {
    match source {
        Source::System => "System",
        Source::Battery => "Battery",
        Source::Ota => "Firmware update",
        Source::Wifi => "Wi-Fi",
        Source::Ha => "Home Assistant",
    }
}

/// Unread count (badge). Zeroed by [`mark_read`] when the shade opens.
pub fn unread() -> u32 {
    UNREAD.load(Ordering::Relaxed)
}

pub fn mark_read() {
    UNREAD.store(0, Ordering::Relaxed);
}

/// Snapshot the ring (newest first) into `buf` for the shade rebuild.
pub fn snapshot(buf: &mut heapless::Vec<Notification, CAP>) {
    buf.clear();
    RING.lock(|cell| {
        for n in cell.borrow().iter() {
            let _ = buf.push(n.clone());
        }
    });
}

/// Dismiss one card (ring index, newest = 0). Out-of-range is a no-op (a
/// stale swipe against a just-rebuilt list must not panic).
pub fn dismiss(idx: usize) {
    RING.lock(|cell| {
        let mut ring = cell.borrow_mut();
        if idx < ring.len() {
            let _ = ring.remove(idx);
        }
    });
}

/// CLEAR ALL.
pub fn clear() {
    RING.lock(|cell| cell.borrow_mut().clear());
    mark_read();
}

/// True when the ring already holds an entry from `source` — dedup guard for
/// repeating system events (e.g. WiFi retry loops must not stack a card per
/// burst while the AP is down).
pub fn has_source(source: Source) -> bool {
    RING.lock(|cell| cell.borrow().iter().any(|n| n.source == source))
}

/// Age label for a card: "now", "Nm", "Nh", or "Nd" from the wall-clock stamp
/// vs now (same clock, so AOD sleep can't stretch it). Day wrap is handled
/// mod 31; a zero stamp (RTC never read) renders as "".
pub fn age_str(day: u8, sod: u32) -> heapless::String<8> {
    let mut s: heapless::String<8> = heapless::String::new();
    let wall = WALL.load(Ordering::Relaxed);
    if day == 0 && sod == 0 && wall == 0 {
        return s;
    }
    let (now_day, now_sod) = ((wall >> 20) as u8, wall & 0xF_FFFF);
    let days = (now_day as i32 - day as i32).rem_euclid(31) as u32;
    let secs = (days * 86_400 + now_sod).saturating_sub(sod);
    use core::fmt::Write;
    let _ = if secs < 60 {
        s.push_str("now").map_err(|_| core::fmt::Error)
    } else if secs < 3600 {
        write!(s, "{}m", secs / 60)
    } else if secs < 86_400 {
        write!(s, "{}h", secs / 3600)
    } else {
        write!(s, "{}d", secs / 86_400)
    };
    s
}

/// Copy `text` into a bounded string: printable ASCII kept, anything else
/// (incl. control chars) becomes '?'; truncated at the cap.
fn sanitize<const N: usize>(text: &str) -> heapless::String<N> {
    let mut s: heapless::String<N> = heapless::String::new();
    for c in text.chars() {
        let c = if (' '..='~').contains(&c) { c } else { '?' };
        if s.push(c).is_err() {
            break;
        }
    }
    s
}
