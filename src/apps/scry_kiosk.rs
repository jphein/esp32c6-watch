//! smol #540: the scry station's kiosk — the state machine that makes the
//! board a tap-a-card terminal wearing server-rendered faces.
//!
//! Lifecycle (the spike-proven shape, contract v4):
//! link up → paint `/screen-idle` (the resting face: "TAP A DEVICE CARD
//! HERE") → on tap `POST /tap` → paint `/screen/<host>` (or
//! `/screen-unbound/<uid>`) and refresh every [`FRAME_MS`] for
//! [`STATUS_MS`] → back to the idle face. A BOOT-button press parks the
//! kiosk and hands the glass back to the normal watch UI for
//! [`SUSPEND_MS`]; a tap while parked re-enters immediately.
//!
//! ## Who owns what
//! The kiosk owns nothing visual — every face is server-rendered (no local
//! artwork at all), and every paint is a streamed strip blit straight to the
//! panel. Scene park/unpark is REQUESTED via [`KioskAction`] and performed by
//! `main`, which also owns the post-resume chrome resets (`prev_page` etc.)
//! this module cannot reach — the same division the launch machinery uses.
//!
//! ## Blocking honesty
//! A status paint awaits ~3 s inline in the main loop (measured: 2,961 ms
//! full frame, 812 ms idle face). During those seconds the mesh tick and
//! touch handling stall — the same order of stall the fleet flavor's WiFi
//! flush already imposes (~15 s) and the ping-melody precedent accepts. A
//! kiosk that is also a perfect mesh citizen wants the fetch in its own
//! task + a shared panel lock; that is a follow-up, stated not implied.

use embassy_net::Stack;
use esp_println::println;

use crate::drivers::ActivePanel;
use crate::net::scry_client::{self, TapOutcome, FRAME_H, FRAME_W, HOST_CAP};
use crate::peripherals::rc522::{Tap, UID_STR_CAP};

/// Status-face refresh cadence. A frame costs ~3 s of the 5 s, per the spike.
const FRAME_MS: u64 = 5_000;
/// How long a tap holds the status face before the idle face returns.
const STATUS_MS: u64 = 60_000;
/// How long a BOOT press lends the glass back to the normal watch UI.
const SUSPEND_MS: u64 = 60_000;
/// Idle-face repaint cadence. 10 s (not per-minute) so the server's transient
/// idle prompts reach the glass promptly — the `/imbue` rite (labels) arms a
/// 180 s pending bind and `/screen-idle` then shows "PRESENT A BLANK CARD" /
/// "CARD IMBUED"; a slow repaint would strand that prompt. The station is
/// mains-powered, so a 10 s HTTP poll costs nothing worth saving.
const IDLE_REPAINT_MS: u64 = 10_000;
/// Retry cadence for a failed paint (server down, link flap).
const RETRY_MS: u64 = 10_000;

/// What `main` must do on the kiosk's behalf this tick.
pub enum KioskAction {
    /// Suspend the Slint scene — the kiosk just took the panel.
    ParkScene,
    /// Resume the Slint scene + run the post-resume chrome resets.
    UnparkScene,
}

enum Mode {
    /// No link yet (or the first idle paint keeps failing): nothing on the
    /// glass is ours yet; the scene still runs.
    WaitLink,
    /// The resting face is up.
    Idle,
    /// A tapped card's status face is up (host = None → the unbound face,
    /// painted once, no refresh).
    Status {
        host: Option<heapless::String<HOST_CAP>>,
        until_ms: u64,
    },
    /// BOOT press: normal watch UI until the deadline (or the next tap).
    Suspended { until_ms: u64 },
}

pub struct Kiosk {
    mode: Mode,
    /// The UID of the card driving the current/last status face.
    uid: heapless::String<UID_STR_CAP>,
    /// Next paint due (frame refresh, idle repaint, or retry).
    next_paint_ms: u64,
}

impl Kiosk {
    pub const fn new() -> Self {
        Self {
            mode: Mode::WaitLink,
            uid: heapless::String::new(),
            next_paint_ms: 0,
        }
    }

    /// True while the kiosk owns the panel (main must not let the scene draw).
    pub fn owns_panel(&self) -> bool {
        matches!(self.mode, Mode::Idle | Mode::Status { .. })
    }

    /// One kiosk step. `tap` is this tick's debounced reader event,
    /// `boot_pressed` a BOOT short-press, `connected` the net snapshot's
    /// DHCP-lease state (sockets work). Returns what `main` must do to the scene.
    ///
    /// Borrow shape: the mode is taken BY VALUE at the top and reassigned in
    /// every arm — no arm holds a `&self.mode` across the `paint` awaits.
    pub async fn tick(
        &mut self,
        stack: Stack<'static>,
        connected: bool,
        tap: Option<&Tap>,
        boot_pressed: bool,
        display: &mut ActivePanel<'static>,
        now_ms: u64,
    ) -> Option<KioskAction> {
        let mode = core::mem::replace(&mut self.mode, Mode::WaitLink);
        match mode {
            Mode::WaitLink => {
                if !connected || now_ms < self.next_paint_ms {
                    self.mode = Mode::WaitLink;
                    return None;
                }
                if self.paint(stack, Face::Idle, display).await {
                    self.mode = Mode::Idle;
                    self.next_paint_ms = now_ms + IDLE_REPAINT_MS;
                    return Some(KioskAction::ParkScene);
                }
                self.mode = Mode::WaitLink;
                self.next_paint_ms = now_ms + RETRY_MS;
                None
            }
            Mode::Idle => {
                if boot_pressed {
                    self.mode = Mode::Suspended {
                        until_ms: now_ms + SUSPEND_MS,
                    };
                    return Some(KioskAction::UnparkScene);
                }
                if let Some(tap) = tap {
                    // Already owned the panel: no scene action needed.
                    self.enter_status(stack, tap, display, now_ms).await;
                    return None;
                }
                if now_ms >= self.next_paint_ms {
                    // Idle repaint (footer clock) — a failure keeps the stale
                    // face and retries sooner; never falls back to WaitLink
                    // (the glass already shows a meaningful frame).
                    let ok = self.paint(stack, Face::Idle, display).await;
                    self.next_paint_ms = now_ms + if ok { IDLE_REPAINT_MS } else { RETRY_MS };
                }
                self.mode = Mode::Idle;
                None
            }
            Mode::Status { host, until_ms } => {
                if boot_pressed {
                    self.mode = Mode::Suspended {
                        until_ms: now_ms + SUSPEND_MS,
                    };
                    return Some(KioskAction::UnparkScene);
                }
                if let Some(tap) = tap {
                    // A new card (or the same card re-armed) restarts the flow.
                    self.enter_status(stack, tap, display, now_ms).await;
                    return None;
                }
                if now_ms >= until_ms {
                    let ok = self.paint(stack, Face::Idle, display).await;
                    self.mode = Mode::Idle;
                    self.next_paint_ms = now_ms + if ok { IDLE_REPAINT_MS } else { RETRY_MS };
                    return None;
                }
                if now_ms >= self.next_paint_ms {
                    if let Some(h) = &host {
                        let h: heapless::String<HOST_CAP> = h.clone();
                        let _ = self.paint(stack, Face::Host(h.as_str()), display).await;
                    }
                    // (The unbound face is static — painted at entry, waits
                    // out its deadline with no refresh.)
                    self.next_paint_ms = now_ms + FRAME_MS;
                }
                self.mode = Mode::Status { host, until_ms };
                None
            }
            Mode::Suspended { until_ms } => {
                if let Some(tap) = tap {
                    // A tap re-enters the kiosk immediately — the card wins.
                    self.enter_status(stack, tap, display, now_ms).await;
                    return Some(KioskAction::ParkScene);
                }
                if now_ms >= until_ms {
                    if self.paint(stack, Face::Idle, display).await {
                        self.mode = Mode::Idle;
                        self.next_paint_ms = now_ms + IDLE_REPAINT_MS;
                        return Some(KioskAction::ParkScene);
                    }
                    self.mode = Mode::Suspended {
                        until_ms: now_ms + RETRY_MS,
                    };
                    return None;
                }
                self.mode = Mode::Suspended { until_ms };
                None
            }
        }
    }

    /// POST the tap, then paint whichever status face the answer names, and
    /// set the mode accordingly. Always leaves the kiosk owning the panel.
    async fn enter_status(
        &mut self,
        stack: Stack<'static>,
        tap: &Tap,
        display: &mut ActivePanel<'static>,
        now_ms: u64,
    ) {
        self.uid.clear();
        let _ = self.uid.push_str(tap.uid.as_str());
        // Local copy so `Face` never borrows `self` across `paint(&mut self)`.
        let uid: heapless::String<UID_STR_CAP> = self.uid.clone();
        let host = match scry_client::post_tap(stack, uid.as_str()).await {
            TapOutcome::Bound(h) => {
                println!("[SCRY] {} -> {} (summoned)", uid, h.as_str());
                Some(h)
            }
            TapOutcome::Unbound => {
                println!("[SCRY] {uid} unbound");
                None
            }
            TapOutcome::Failed(e) => {
                println!("[SCRY] tap failed: {e}");
                // A failed POST must not strand a mid-transition kiosk with a
                // stale face — fall back to idle and retry-paint soon.
                let _ = self.paint(stack, Face::Idle, display).await;
                self.mode = Mode::Idle;
                self.next_paint_ms = now_ms + RETRY_MS;
                return;
            }
        };
        let face = match &host {
            Some(h) => Face::Host(h.as_str()),
            None => Face::Unbound(uid.as_str()),
        };
        let _ = self.paint(stack, face, display).await;
        self.mode = Mode::Status {
            host,
            until_ms: now_ms + STATUS_MS,
        };
        self.next_paint_ms = now_ms + FRAME_MS;
    }

    /// Stream one face onto the panel. One address window + one RAMWR run for
    /// the whole frame (the bus latches RAMWR_CONT between strips — the
    /// measured-fast path); a mid-frame error leaves a torn frame the next
    /// paint repairs, which is the honest failure for a kiosk.
    async fn paint(
        &mut self,
        stack: Stack<'static>,
        face: Face<'_>,
        display: &mut ActivePanel<'static>,
    ) -> bool {
        display.set_addr_window(0, 0, FRAME_W as u16, FRAME_H as u16);
        display.bus_mut().begin_pixels();
        let res = {
            // The closure holds the bus borrow for the whole fetch (across
            // its awaits) — the panel is untouchable meanwhile, by design.
            let bus = display.bus_mut();
            let mut blit = |_y0: u16, rows: u16, bytes: &[u8]| {
                // BE wire bytes -> u16 pixels, one panel row at a time
                // (320 px = 640 B on the stack — deliberately not a
                // strip-sized buffer; #438: stack is not headroom).
                let mut row = [0u16; FRAME_W];
                for r in 0..rows as usize {
                    let base = r * FRAME_W * 2;
                    for (i, px) in row.iter_mut().enumerate() {
                        *px = u16::from_be_bytes([
                            bytes[base + 2 * i],
                            bytes[base + 2 * i + 1],
                        ]);
                    }
                    bus.stream_pixels(&row);
                }
            };
            match face {
                Face::Idle => scry_client::fetch_idle(stack, &mut blit).await,
                Face::Host(h) => scry_client::fetch_screen(stack, Some(h), "", &mut blit).await,
                Face::Unbound(uid) => {
                    scry_client::fetch_screen(stack, None, uid, &mut blit).await
                }
            }
        };
        display.bus_mut().end_pixels();
        match res {
            Ok(()) => true,
            Err(e) => {
                println!("[SCRY] paint failed: {e}");
                false
            }
        }
    }
}

enum Face<'a> {
    Idle,
    Host(&'a str),
    Unbound(&'a str),
}
