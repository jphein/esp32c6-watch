//! Session manager (#31): which apps are *suspended* — exited with their state
//! kept — and in what order, driving the bottom-edge-hold app switcher and the
//! watchface status-cluster badge.
//!
//! The model is deliberately tiny: presence in the recency-ordered list IS the
//! `Suspended` state; the currently-dispatched `app_state` IS `Running`; and
//! everything else is fresh (next open runs `setup()`). No parallel per-app
//! enum to keep in sync with the registry.
//!
//! Framebuffer apps are the real clients: their state structs (snake_game,
//! tetris_game, …) already persist as main-loop locals, so "suspend" is simply
//! *exit without running `setup()` on the next entry* — the ~51KB framebuffer
//! is freed on exit and re-allocated on resume through the existing
//! `Framebuffer::try_new` + RAM-toast path (no-PSRAM constraint: we keep logic
//! state, never pixels). Every fb exit suspends: no game ever returns
//! `AppResult::Exit` (game-over screens self-reset on tap in-app), so the boot
//! button is the one exit path and it always means "put it away", not "reset".
//! KILL (switcher card swipe-up) removes the entry, so the next open is fresh.
//!
//! Overlay apps are scene-resident — their per-boot state (hunt target, mic
//! gain, …) persists trivially whether or not they're listed here, and closing
//! one (right-swipe / chevron) reads as *dismiss*, so no overlay call-site
//! suspends in v1. The API stays state-agnostic: `suspend()` accepts any
//! registry app if an overlay ever earns a background mode.

use crate::apps::AppState;

/// Suspension list capacity — must cover the registry (15 apps today).
const CAP: usize = 16;

/// Recency-ordered suspended-app list (most recently suspended first).
pub struct Sessions {
    order: heapless::Vec<AppState, CAP>,
}

impl Sessions {
    pub const fn new() -> Self {
        Self {
            order: heapless::Vec::new(),
        }
    }

    /// Mark `state` suspended (moves it to the front on a re-suspend).
    pub fn suspend(&mut self, state: AppState) {
        self.kill(state);
        // Capacity math: CAP ≥ registry size, and kill() just freed any dup.
        let _ = self.order.insert(0, state);
    }

    /// Drop `state`'s session (switcher kill): next open runs `setup()`.
    pub fn kill(&mut self, state: AppState) {
        if let Some(pos) = self.order.iter().position(|s| *s == state) {
            let _ = self.order.remove(pos);
        }
    }

    /// Consume a suspension for `state`: returns true when it was suspended
    /// (caller skips `setup()` — the resume path) and removes it, since the
    /// app is now running again.
    pub fn take_resume(&mut self, state: AppState) -> bool {
        let suspended = self.order.contains(&state);
        if suspended {
            self.kill(state);
        }
        suspended
    }

    /// Number of suspended apps (the badge count).
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Suspended apps, most recently suspended first (the switcher order).
    pub fn iter(&self) -> impl Iterator<Item = AppState> + '_ {
        self.order.iter().copied()
    }
}
