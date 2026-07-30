// Rust-side wrapper around the WatchShell Slint component: owns the window,
// the render strip, and the callback→loop request cells. main.rs talks to
// this module only; no Slint types cross its boundary except in render().

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;

use slint::platform::software_renderer::{MinimalSoftwareWindow, Rgb565Pixel};
use slint::platform::{PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::apps::AppState;
use crate::drivers::co5300::Co5300Display;
use crate::net::names;
// #58 climate: the real `climate-model` crate (oracle-t9 CONFIRMED-CLEAN @5c0d04c;
// stub swapped out). Provides ClimateState / ClimateEntity / HvacMode.
use climate_model;
use crate::net::smol_mesh::PeerView;
use crate::peripherals::rtc::DateTime;
use crate::peripherals::touch::{SwipeDirection, TouchPoint};
use crate::ui::slint_platform::{init_platform, TwoLineFlusher, WIDTH};

slint::include_modules!(); // WatchShell, PeerRow

/// Carousel page indices — MUST match the page order in ui/slint/shell.slint.
pub const PAGE_CLOCK: i32 = 0;
pub const PAGE_SENSORS: i32 = 1;
pub const PAGE_SYSTEM: i32 = 2;
pub const PAGE_POWER: i32 = 3;
pub const PAGE_MESH: i32 = 4;
pub const PAGE_COUNT: i32 = PAGE_MESH + 1;

const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

/// Map the UI slider fraction (0.0..1.0) onto the CO5300 brightness range,
/// with a floor so the slider can never black the panel out completely.
const BRIGHTNESS_MIN: u8 = 0x10;
pub fn brightness_raw(frac: f32) -> u8 {
    let frac = frac.clamp(0.0, 1.0);
    BRIGHTNESS_MIN + (frac * (0xFF - BRIGHTNESS_MIN) as f32) as u8
}

/// y-band of the brightness slider on the power page: horizontal swipes
/// starting here are slider drags, not page switches.
pub const SLIDER_BAND: core::ops::RangeInclusive<u16> = 330..=430;

// ==================== THE GESTURE MAP (#29 / #31 / #32) ====================
// Single source of truth for the edge-gesture shell. Zones are judged on
// `SwipeEvent.start_y` — the FT3168 driver already reports it (the slider
// exclusion above uses the same field). All edge gestures act on the
// WATCHFACE pages only: the launcher, the Settings hub, and every overlay
// own their gestures (they swallow nav swipes first), and framebuffer games
// never route touch through this module at all.
//
//   Bottom edge (start_y ≥ EDGE_BOTTOM_Y, ~85% of the 502px panel):
//     swipe UP        → app launcher (#29), from ANY watchface page
//     HOLD ≥ 500 ms   → app switcher (#31)
//   Top edge (start_y ≤ EDGE_TOP_Y, ~15%):
//     swipe DOWN      → notification shade (#32)
//   Mid-screen: unchanged — Left/Right page the carousel, Up on the clock
//     page still opens the launcher (the legacy affordance).
//
// On-face LONG-PRESS outside the bottom zone is RESERVED for the face
// manager (#45): the hold detector arms ONLY inside the bottom edge zone.
// Power-page corner case: the brightness slider band (330..=430) overlaps
// the bottom zone by 4px and its exclusion is checked first — a drag that
// close to the slider must never yank the launcher up.

/// Bottom edge zone floor: `start_y >= EDGE_BOTTOM_Y` is an edge gesture
/// (≈85% of the 502px panel height).
pub const EDGE_BOTTOM_Y: u16 = 427;

/// Top edge zone ceiling (#32): a swipe DOWN with `start_y <= EDGE_TOP_Y`
/// (≈15%) pulls the notification shade over any watchface page.
pub const EDGE_TOP_Y: u16 = 75;

/// Bottom-edge HOLD (#31): a press that stays inside the edge zone for this
/// long raises the app switcher.
const HOLD_MS: u64 = 500;
/// Finger drift that cancels a pending hold — past this it's swipe intent.
/// Kept under the touch driver's 36px swipe threshold so a cancelled hold can
/// still classify as the edge-swipe.
const HOLD_SLOP_PX: u16 = 24;

/// Switcher card geometry (#31) — MUST match `ui/slint/switcher.slint`:
/// slot i spans y `CARD_TOP + i*CARD_PITCH .. + CARD_H`. A kill-swipe (Up
/// starting on a card) maps back to its slot with [`switcher_slot`].
const SWITCHER_CARD_TOP: u16 = 110;
const SWITCHER_CARD_H: u16 = 84;
const SWITCHER_CARD_PITCH: u16 = 96;
/// Visible card slots (the suspension list may be longer; overlay shows "+N").
const SWITCHER_CARDS: usize = 4;

/// Shade card geometry (#32) — MUST match `ui/slint/shade.slint`: slot i
/// spans y `CARD_TOP + i*CARD_PITCH .. + CARD_H`. A dismiss-swipe (Left
/// starting on a card) maps back to its slot — which IS the ring index,
/// newest = 0 — with [`shade_slot`].
const SHADE_CARD_TOP: u16 = 76;
const SHADE_CARD_H: u16 = 84;
const SHADE_CARD_PITCH: u16 = 92;
/// Visible shade cards (the ring holds up to 8; overlay shows "+N").
const SHADE_CARDS: usize = 4;

/// Settings-hub section pages (ui/slint/settings.slint `titles` order).
pub const SETTINGS_PAGE_COUNT: i32 = 6;
/// The DISPLAY page's index — the one hosting the hub's brightness slider.
const HUB_PAGE_DISPLAY: i32 = 1;
/// y-band of the Settings hub's brightness slider (settings.slint DISPLAY
/// page: slider at absolute y 180..220, padded for finger slop): swipes
/// starting here are slider drags, not page flips / back-navigation.
const HUB_SLIDER_BAND: core::ops::RangeInclusive<u16> = 170..=240;

/// Wake gesture-hint choreography, in ms after [`ShellUi::hint_wake`] arms it.
/// The strips are created invisible with the wake frame (so that frame stays
/// cheap), bloom in at BLOOM, start fading at FADE, and are destroyed at KILL
/// (which must exceed FADE + the 480ms Slint opacity tween). Net: visible
/// ~0.15s → ~3.1s after the wake — "~3 seconds", never resident.
const HINT_BLOOM_MS: u64 = 150;
const HINT_FADE_MS: u64 = 2600;
const HINT_KILL_MS: u64 = 3200;

// The launcher launch-index → AppState mapping now lives in the app registry
// (src/apps/registry.rs): `REGISTRY[idx].state`, exposed via
// `registry::launch_state(idx)`. The launcher tiles are built from the same
// registry (see `build_launcher_pages`), so the idx→app contract is
// single-sourced instead of a hand-kept parallel array.

/// Full-screen Slint overlays that share the shell's Slint dispatch branch (no
/// framebuffer of their own). One table drives all three pieces of overlay
/// boilerplate: the open-flag mirror + post-touch reconcile (main loop, via
/// [`ShellUi::mirror_overlays`] / [`ShellUi::reconcile_overlay`]) and the
/// Right-swipe close cascade ([`ShellUi::handle_touch`]). Adding an overlay app
/// is one row here instead of a line in each of those three places. Order ==
/// the swipe-cascade check order. The Launcher is handled separately (it is not
/// a registry app).
struct Overlay {
    state: AppState,
    is_open: fn(&WatchShell) -> bool,
    set_open: fn(&WatchShell, bool),
    close: OverlayClose,
}

/// How a Right-swipe closes an overlay.
enum OverlayClose {
    /// Clear the open flag directly (stateless overlays).
    Flag,
    /// Fire a request cell instead — the main loop drains it to ALSO release a
    /// resource hold (WiFi for energy/climate). A direct flag-clear would strand
    /// the hold: the cell drain that frees the radio + restores mesh never runs.
    Cell(fn(&ShellRequests)),
}

const OVERLAYS: &[Overlay] = &[
    Overlay { state: AppState::Wled, is_open: WatchShell::get_wled_open, set_open: WatchShell::set_wled_open, close: OverlayClose::Flag },
    Overlay { state: AppState::Hunt, is_open: WatchShell::get_hunt_open, set_open: WatchShell::set_hunt_open, close: OverlayClose::Flag },
    Overlay { state: AppState::Energy, is_open: WatchShell::get_energy_open, set_open: WatchShell::set_energy_open, close: OverlayClose::Cell(|r| r.energy_close.set(true)) },
    Overlay { state: AppState::Climate, is_open: WatchShell::get_climate_open, set_open: WatchShell::set_climate_open, close: OverlayClose::Cell(|r| r.climate_closed.set(true)) },
    Overlay { state: AppState::Lights, is_open: WatchShell::get_lights_open, set_open: WatchShell::set_lights_open, close: OverlayClose::Cell(|r| r.lights_closed.set(true)) },
    Overlay { state: AppState::Ping, is_open: WatchShell::get_ping_open, set_open: WatchShell::set_ping_open, close: OverlayClose::Flag },
    Overlay { state: AppState::Voice, is_open: WatchShell::get_voice_open, set_open: WatchShell::set_voice_open, close: OverlayClose::Flag },
    Overlay { state: AppState::Sound, is_open: WatchShell::get_mic_open, set_open: WatchShell::set_mic_open, close: OverlayClose::Flag },
    // #story. `Flag` rather than `Cell`: the WiFi hold is edge-tracked off
    // `app_state == Story` in the loop, so clearing the flag drops it on the very
    // next tick with nothing to strand. Note a right-swipe cannot land *during*
    // playback — the loop is parked in the stream by design — which is exactly why
    // a tap stops playback (`PlaybackUi::should_stop`); that is the escape hatch,
    // not the swipe.
    Overlay { state: AppState::Story, is_open: WatchShell::get_story_open, set_open: WatchShell::set_story_open, close: OverlayClose::Flag },
    Overlay { state: AppState::Theme, is_open: WatchShell::get_theme_open, set_open: WatchShell::set_theme_open, close: OverlayClose::Flag },
    // Settings hub (v0.9.0): listed for the mirror/reconcile plumbing; its
    // Right-swipe never reaches this table's close arm — handle_touch routes
    // hub swipes (page flips + sub-view back) in a dedicated branch first.
    Overlay { state: AppState::Settings, is_open: WatchShell::get_settings_open, set_open: WatchShell::set_settings_open, close: OverlayClose::Flag },
];

#[derive(Default)]
pub struct ShellRequests {
    pub brightness: Cell<Option<u8>>, // raw CO5300 value
    pub launch: Cell<Option<AppState>>,
    pub wifi_toggle: Cell<bool>,
    pub ble_toggle: Cell<bool>,
    pub mesh_toggle: Cell<bool>,
    pub cpu_cycle: Cell<bool>,
    pub gyro_toggle: Cell<bool>,
    pub reboot: Cell<bool>,
    /// WLED remote: a tapped tile's action id (0..8), drained by the loop and
    /// mapped to a WiZmote broadcast. `wled_close` is the back-chevron/Right-swipe.
    pub wled_action: Cell<Option<i32>>,
    pub wled_close: Cell<bool>,
    /// Hunt: "next target" tap cycles the roster; `hunt_close` is back/Right-swipe.
    pub hunt_next: Cell<bool>,
    pub hunt_close: Cell<bool>,
    /// Energy overlay back/Right-swipe.
    pub energy_close: Cell<bool>,
    /// Climate (#58): UI → session commands + close. set-temp/set-mode carry
    /// (card-index, value); main.rs resolves the index → ObjId + queues the cmd.
    pub climate_set_temp: Cell<Option<(i32, f32)>>,
    pub climate_set_mode: Cell<Option<(i32, i32)>>,
    pub climate_closed: Cell<bool>,
    /// Lights (#39): a tapped command (0 toggle · 1 on · 2 off), drained by the
    /// loop into a `ClimateCmd::Lights` publish on the shared HA session;
    /// `lights_closed` is the back-chevron / right-swipe (cell, not flag, so
    /// the loop also releases the WiFi hold).
    pub lights_cmd: Cell<Option<i32>>,
    pub lights_closed: Cell<bool>,
    /// Ping (#35): hero tap → one PING broadcast (the loop gates seq/cooldown).
    pub ping_send: Cell<bool>,
    /// Ping receiver pulse: tap-to-dismiss (also set by any swipe while the
    /// pulse is up) — the loop drains it into [`ShellUi::ping_pulse_dismiss`]
    /// so the auto-dismiss clock is disarmed with the overlay.
    pub ping_pulse_tap: Cell<bool>,
    /// Voice PTT (#42): `pressed` is the finger-down that starts capture (drained
    /// by the loop when app_state == Voice). `released` is advisory — the loop's
    /// release watcher keys off the physical touch INT pin, since the Slint
    /// release callback can't fire while the loop is parked streaming the hold.
    pub voice_ptt_pressed: Cell<bool>,
    pub voice_ptt_released: Cell<bool>,
    pub mic_gain_up: Cell<bool>,
    pub mic_gain_down: Cell<bool>,
    /// Theme picker: new scheme index (0..3) chosen from the swatch grid. The
    /// tile sets `Theme.scheme` directly for instant preview; this cell is
    /// drained by the loop to persist the choice to flash (config.rs v3).
    pub theme: Cell<Option<i32>>,
    /// Settings hub (v0.9.0): touch-sound toggle (flip + persist).
    pub touch_sound_toggle: Cell<bool>,
    /// Settings hub: UPDATE FIRMWARE tap (the old fb Settings OTA request).
    pub settings_ota: Cell<bool>,
    /// NETWORK flow: trigger a scan (choose-network + rescan share it); the
    /// loop raises picker view 1, paints "Scanning…", then scans inline.
    pub wifi_scan: Cell<bool>,
    /// NETWORK flow: picker row index tapped (into the loop's scan list).
    pub wifi_pick: Cell<Option<i32>>,
    /// NETWORK flow: "Other network…" — manual SSID entry via the keyboard.
    pub wifi_manual: Cell<bool>,
    /// NETWORK flow: back out of a sub-view (chevron / right-swipe). The loop
    /// owns the view transitions (2→1→0) + keyboard-state cleanup.
    pub net_back: Cell<bool>,
    /// Keyboard: one character per tap (space = " "). Rust owns the buffer.
    pub kb_key: Cell<Option<SharedString>>,
    /// Keyboard: backspace finger-down/up edges (Rust deletes + auto-repeats).
    pub kb_bksp_down: Cell<bool>,
    pub kb_bksp_up: Cell<bool>,
    /// Keyboard: show/hide-password eye toggle.
    pub kb_eye: Cell<bool>,
    /// Keyboard: ✓ — commit the field (SSID stage → password stage → connect).
    pub kb_done: Cell<bool>,
    /// Buttons section (#59): a tapped mapping row's slot (0 boot-short ·
    /// 1 boot-long · 2 pwron-short · 3 pwron-long) — the loop cycles + persists
    /// that slot's [`ButtonAction`].
    pub button_cycle: Cell<Option<i32>>,
    /// SOUND-page volume steppers + mute toggle (#59).
    pub volume_down: Cell<bool>,
    pub volume_up: Cell<bool>,
    pub volume_mute: Cell<bool>,
    /// Volume HUD / SOUND-page slider drag → 0..1 (the loop maps to 0..15,
    /// clears mute, and resets the HUD's 2s auto-dismiss).
    pub volume_changed: Cell<Option<f32>>,
    /// Story (#story): tab tapped → page 0 list · 1 play · 2 stats · 3 character.
    pub story_nav: Cell<Option<i32>>,
    /// Story: a chapter tapped. Carries the CHAPTER NUMBER, not a row index, so
    /// paging the list cannot desynchronise it from what was tapped.
    pub story_pick: Cell<Option<i32>>,
    /// Story: STOP tapped — ends playback at the next chunk boundary.
    pub story_stop: Cell<bool>,
    pub story_pause: Cell<bool>,
    pub story_resume: Cell<bool>,
    /// Story: DIRECTOR NOTE tapped — hand off to the push-to-talk STT path.
    pub story_note: Cell<bool>,
    /// Story: list paging, -1 newer / +1 older.
    pub story_page_delta: Cell<Option<i32>>,
    /// Power menu (#48): SHUTDOWN row → the loop writes the AXP2101 poweroff
    /// bit (power.shutdown()). REBOOT reuses the `reboot` cell above.
    pub power_shutdown: Cell<bool>,
    /// App switcher (#31): open request — bottom-edge HOLD (handle_touch) or
    /// the status-cluster chip. A cell (not a direct property set) because the
    /// main loop must build the session cards BEFORE the overlay shows.
    pub open_switcher: Cell<bool>,
    /// App switcher: kill-swipe on a card — the registry idx to drop. The loop
    /// owns the session list; it kills + rebuilds the cards in place.
    pub switcher_kill: Cell<Option<i32>>,
    /// Notification shade (#32): open request — top-edge swipe-down
    /// (handle_touch) or the unread chip. A cell so the loop builds the
    /// cards (and zeroes the unread badge) BEFORE the overlay shows.
    pub open_shade: Cell<bool>,
    /// Shade: dismiss one card — the ring index (== visible slot, newest 0),
    /// from the card's X tap or a Left-swipe on it.
    pub notif_dismiss: Cell<Option<i32>>,
    /// Shade: CLEAR ALL pill.
    pub notif_clear: Cell<bool>,
}

pub struct ShellUi {
    window: Rc<MinimalSoftwareWindow>,
    /// The Slint scene. `None` while a game holds the framebuffer: the ~201KB
    /// RGB332 fb and the resident WatchShell scene can't both fit in the C6's
    /// SRAM, so the scene is dropped on game launch (freeing heap so
    /// `Framebuffer::try_new` fits) and recreated on return. The window +
    /// platform are set-once globals and stay put; only the component is
    /// droppable (the window holds a weak ref, so `= None` frees it).
    ui: Option<WatchShell>,
    /// True while a framebuffer game owns the panel. The scene stays ALIVE and
    /// simply stops rendering/receiving touch (#66).
    ///
    /// It used to be dropped instead (`ui = None`) to free heap for the fb —
    /// but tearing down the Slint component tree hit heap free-list corruption
    /// (`Freed node aliases existing hole! Bad free?` inside
    /// `PropertyHandle::drop`), crashing the watch 100 % of the time on game
    /// launch, including on shipped v0.12.1. The drop was already vestigial:
    /// the half-res fb is ~51KB and `Framebuffer::try_new` is fallible and can
    /// draw from the reclaimed pool, so the scene never needed to go. Not
    /// dropping it both dodges the corrupt teardown and makes game exit
    /// instant (no scene rebuild).
    suspended: bool,
    pub req: Rc<ShellRequests>,
    /// Long-lived roster model: set_mesh_rows swaps its contents in place
    /// instead of allocating a fresh ModelRc per push.
    mesh_model: Rc<VecModel<PeerRow>>,
    /// Climate (#58) card model: one ClimateCard per HA climate entity, swapped
    /// in place by set_climate (same long-lived pattern as mesh_model).
    climate_cards: Rc<VecModel<ClimateCard>>,
    /// Sound-app spectrum (#30): 12 log-spaced bands (level + peak-hold, dBFS),
    /// swapped in place by set_spectrum (same long-lived pattern).
    spectrum_model: Rc<VecModel<SpecBand>>,
    /// Settings-hub NETWORK picker rows, swapped in place by set_wifi_nets
    /// (same long-lived pattern as mesh_model).
    wifi_model: Rc<VecModel<WifiNet>>,
    /// App-switcher session cards (#31), swapped in place by
    /// set_switcher_cards (same long-lived pattern as mesh_model).
    switcher_model: Rc<VecModel<LauncherTile>>,
    /// Notification-shade cards (#32), swapped in place by set_shade_cards
    /// (same long-lived pattern as mesh_model).
    shade_model: Rc<VecModel<NotifCard>>,
    /// Story chapter-list rows (#story). Deliberately holds only the page the
    /// list draws, not the whole index: #75's OOM was a DRAWN-ITEM-COUNT problem,
    /// so paging costs a request rather than a scene full of hidden rows.
    story_chapters: Rc<VecModel<StoryChapter>>,
    /// Story inventory / equipment / appearance rows, all label+value pairs.
    story_equipment: Rc<VecModel<StorySlot>>,
    story_appearance: Rc<VecModel<StorySlot>>,
    /// Registry idx per visible switcher slot — maps a kill-swipe's start_y
    /// (→ slot via [`switcher_slot`]) back to the app it lands on.
    switcher_rows: heapless::Vec<i32, SWITCHER_CARDS>,
    line_buf: Vec<Rgb565Pixel>,
    scratch: Vec<u16>,
    touch_down: bool,
    last_pos: slint::LogicalPosition,
    last_second: u8,
    /// Current page, preserved across a suspend so the recreated scene returns
    /// to where the user was rather than snapping back to the clock.
    saved_page: i32,
    /// Active theme scheme (0 Midnight · 1 Paper · 2 Amber · 3 Violet). Stored
    /// here so a scene rebuild (game suspend/resume) re-applies it — a fresh
    /// WatchShell resets the Theme global to scheme 0 otherwise.
    scheme: i32,
    /// Gesture-hint (wake shimmer) sequencing. `hint_armed_at` is the wake
    /// instant [`hint_wake`] stamped (None = no hint window running);
    /// [`tick_hints`] (run from [`render`]) walks bloom → hold → fade →
    /// destroy against it, so main.rs carries no per-tick hint logic.
    /// `hint_lit` mirrors the Slint `hints-lit` property (edge-triggered
    /// sets only). The `seen` latches suppress a hint for the rest of the
    /// boot once its gesture has actually been used.
    hint_armed_at: Option<embassy_time::Instant>,
    hint_lit: bool,
    hint_seen_lr: bool,
    hint_seen_up: bool,
    hint_seen_down: bool,
    /// Bottom-edge HOLD tracking (#31). Armed on every press edge with the
    /// press origin; drifting past [`HOLD_SLOP_PX`] disarms it (swipe intent).
    /// When an armed press inside the bottom edge zone outlives [`HOLD_MS`]
    /// on a clean watchface, the switcher-open request fires and
    /// `hold_fired` latches so the eventual lift releases off-window and
    /// never classifies as a tap/swipe.
    hold_armed_at: Option<embassy_time::Instant>,
    hold_start: (u16, u16),
    hold_fired: bool,
    /// Ping receiver-pulse choreography (#35, the hint idiom): the arm
    /// instant [`ping_pulse_show`] stamped (None = no pulse up);
    /// [`tick_ping_pulse`] (run from [`render`]) breathes the rings —
    /// bloom → contract → bloom → auto-dismiss — against it, edge-triggered
    /// through `ping_pulse_stage` so steady phases set no properties.
    ping_pulse_armed_at: Option<embassy_time::Instant>,
    ping_pulse_stage: u8,
}

impl ShellUi {
    /// Call exactly once per boot (registers the Slint platform).
    pub fn new() -> Self {
        let window = init_platform();
        let req = Rc::new(ShellRequests::default());
        let mesh_model: Rc<VecModel<PeerRow>> = Rc::new(VecModel::default());
        let climate_cards: Rc<VecModel<ClimateCard>> = Rc::new(VecModel::default());
        // Prefill at the silence floor so the 12 columns render immediately on
        // first open (level 0.0 would paint full-scale bars).
        let spectrum_model: Rc<VecModel<SpecBand>> = Rc::new(VecModel::from(
            (0..mic_dsp::SPECTRUM_BANDS)
                .map(|_| SpecBand { level: mic_dsp::DBFS_FLOOR, peak: mic_dsp::DBFS_FLOOR })
                .collect::<Vec<_>>(),
        ));
        let wifi_model: Rc<VecModel<WifiNet>> = Rc::new(VecModel::default());
        let switcher_model: Rc<VecModel<LauncherTile>> = Rc::new(VecModel::default());
        let shade_model: Rc<VecModel<NotifCard>> = Rc::new(VecModel::default());
        let story_chapters: Rc<VecModel<StoryChapter>> = Rc::new(VecModel::default());
        let story_equipment: Rc<VecModel<StorySlot>> = Rc::new(VecModel::default());
        let story_appearance: Rc<VecModel<StorySlot>> = Rc::new(VecModel::default());
        let ui = build_scene(
            &req,
            &mesh_model,
            &climate_cards,
            &spectrum_model,
            &wifi_model,
            &switcher_model,
            &shade_model,
            &story_chapters,
            &story_equipment,
            &story_appearance,
        );
        // First frame under ReusedBuffer must be a full paint (the panel just
        // showed fill_screen(BLACK); the renderer has no prior frame to diff
        // against). Slint already dirties everything on first show, but request it
        // explicitly so the boot frame can never come up as a partial box.
        window.window().request_redraw();

        Self {
            window,
            ui: Some(ui),
            req,
            mesh_model,
            climate_cards,
            spectrum_model,
            wifi_model,
            switcher_model,
            shade_model,
            story_chapters,
            story_equipment,
            story_appearance,
            switcher_rows: heapless::Vec::new(),
            line_buf: alloc::vec![Rgb565Pixel(0); WIDTH * 2],
            scratch: alloc::vec![0u16; WIDTH * 2],
            touch_down: false,
            last_pos: slint::LogicalPosition::new(0.0, 0.0),
            last_second: 0xFF,
            suspended: false,
            saved_page: PAGE_CLOCK,
            scheme: 0,
            hint_armed_at: None,
            hint_lit: false,
            hint_seen_lr: false,
            hint_seen_up: false,
            hint_seen_down: false,
            hold_armed_at: None,
            hold_start: (0, 0),
            hold_fired: false,
            ping_pulse_armed_at: None,
            ping_pulse_stage: 0,
        }
    }

    /// Drop the Slint scene to free ~30-40KB of heap so a game's ~201KB
    /// framebuffer fits (the two can't coexist in the C6's SRAM). The window +
    /// platform are set-once globals and survive; the current page is saved for
    /// the recreate. Idempotent — safe to call when already suspended.
    ///
    /// #75: the teardown itself is gated behind
    /// [`Self::SCENE_DROP_ON_SUSPEND`] — read that const before changing it.
    pub fn suspend_scene(&mut self) {
        // Retire any running hint window: a stale armed-instant would other-
        // wise resume ticking while the game owns the panel.
        self.hints_cancel();
        // Same for a running ping pulse (#35): its choreography clock must not
        // keep ticking behind the game.
        self.ping_pulse_dismiss();
        if let Some(ui) = self.ui.as_ref() {
            self.saved_page = ui.get_current_page();
        }
        // #66: park the scene instead of dropping it. `ui = None` here is what
        // ran the Slint teardown that corrupts the heap free-list. Parked means
        // render() and handle_touch() bail, so the game owns the panel and the
        // input stream exactly as before — we simply stop touching Slint's
        // allocation graph.
        //
        // #75 forensics: flip `SCENE_DROP_ON_SUSPEND` to `true` to run the
        // teardown deliberately, under instrumentation.
        if Self::SCENE_DROP_ON_SUSPEND {
            self.ui = None;
        } else {
            self.suspended = true;
        }
    }

    /// #75 forensics gate: drop the Slint component tree on game launch instead
    /// of parking it. **OFF.** This deliberately re-arms the teardown that
    /// panicked with `Freed node aliases existing hole! Bad free?` inside
    /// `PropertyHandle::drop`, 100 % of the time on game launch, including on
    /// shipped v0.12.1 (#66).
    ///
    /// It exists because that panic was routed around, never diagnosed, and
    /// "free heap is high but the allocation failed" cannot be fully attributed
    /// to fragmentation while a live free-list corruption sits in the binary.
    ///
    /// # Running the experiment
    ///
    /// 1. Flip this to `true`.
    /// 2. Build `--features heap-forensics` (see `harvest_free` in src/main.rs).
    /// 3. Launch a framebuffer game. `log_heap` brackets this teardown, so the
    ///    run prints a `[HARVEST]` sweep immediately before and after it.
    ///
    /// # Reading the result
    ///
    /// - **Both sweeps `stop=budget`** → the teardown is NOT losing holes. The
    ///   corruption is a stale hazard, `free()` is honest, and the capacity work
    ///   is the right work. (The `assert!` may still fire — that is cause
    ///   (a)/(b), a double or wrong-size free caught *before* the list is
    ///   damaged, which is a different and more tractable bug.)
    /// - **`pre-drop` is `stop=budget` but `post-drop` is `stop=nomem` with a
    ///   large `left`** → the teardown lost holes. Confirmed live corruption,
    ///   localised to the Slint component drop, and it outranks every capacity
    ///   fix on #75.
    ///
    /// Safe to try: `esp-backtrace` is configured `custom-halt`, so a panic
    /// reboots (~2 s) instead of bricking the watch (#75, d43fa3c).
    /// `resume_scene` still has its full `build_scene` rebuild path, so a game
    /// exit recovers normally.
    ///
    /// Never ship `true`.
    const SCENE_DROP_ON_SUSPEND: bool = false;

    /// Recreate the scene after a game exits: fresh component, callbacks
    /// re-registered, mesh model re-bound, page restored. The caller re-pushes
    /// live data (battery/time/radios/fam/page-data) after this. Idempotent.
    pub fn resume_scene(&mut self) {
        // #66 fast path: the scene was parked, not dropped — just un-park it.
        // No rebuild, no callback re-registration, no allocation: game exit is
        // now instant instead of reconstructing the whole component tree.
        if self.suspended {
            self.suspended = false;
            if let Some(ui) = self.ui.as_ref() {
                ui.set_current_page(self.saved_page);
            }
            // The game painted straight to the panel, so the renderer's idea of
            // what is on-screen is stale — force a full repaint, and clear the
            // 1Hz clock gate so the next set_time lands.
            self.last_second = 0xFF;
            self.request_redraw();
            return;
        }
        if self.ui.is_some() {
            return;
        }
        let ui = build_scene(
            &self.req,
            &self.mesh_model,
            &self.climate_cards,
            &self.spectrum_model,
            &self.wifi_model,
            &self.switcher_model,
            &self.shade_model,
            &self.story_chapters,
            &self.story_equipment,
            &self.story_appearance,
        );
        ui.set_current_page(self.saved_page);
        // A fresh scene resets the Theme global to scheme 0; restore the active
        // scheme so a game exit doesn't snap the watch back to Midnight.
        ui.global::<Theme>().set_scheme(self.scheme);
        self.ui = Some(ui);
        // Fresh scene = time_text is back at its "--:--" default; clear the
        // 1Hz gate so the caller's next set_time repaints the clock even if the
        // second hasn't ticked since the game launched.
        self.last_second = 0xFF;
        // A game painted its framebuffer straight to the panel while suspended, so
        // the panel no longer shows what the (ReusedBuffer) renderer believes is
        // on-screen. Force a full repaint so the recreated scene paints the whole
        // screen, not just its first dirty box. (Callers also request_redraw, but
        // owning it here makes every resume path — game exit AND fb-alloc-fail —
        // correct without relying on each call site.)
        self.request_redraw();
    }

    // === input ===

    /// Feed one iteration's touch sample. `point` is Some while a finger is
    /// down (synthesizes press/move); None after it lifts (synthesizes
    /// release). Swipes drive page/launcher navigation.
    pub fn handle_touch(
        &mut self,
        point: Option<TouchPoint>,
        swipe: Option<SwipeDirection>,
        swipe_start_y: u16,
    ) {
        // The game owns the input stream while it holds the framebuffer. The
        // scene is parked (still allocated) rather than dropped since #66, so
        // the parked flag — not `ui.is_none()` — is what gates touch routing.
        if self.suspended {
            return;
        }
        let Some(ui) = self.ui.as_ref() else {
            return;
        };
        if let Some(tp) = point {
            let pos = slint::LogicalPosition::new(tp.x as f32, tp.y as f32);
            let event = if self.touch_down {
                WindowEvent::PointerMoved { position: pos }
            } else {
                // Press edge: arm the bottom-edge HOLD detector (#31).
                self.hold_armed_at = Some(embassy_time::Instant::now());
                self.hold_start = (tp.x, tp.y);
                self.hold_fired = false;
                WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Left }
            };
            self.touch_down = true;
            self.last_pos = pos;
            let _ = self.window.window().try_dispatch_event(event);
            // Bottom-edge HOLD (#31): drift past the slop disarms (that's a
            // swipe forming); a still press inside the edge zone that outlives
            // HOLD_MS on a clean watchface requests the app switcher. Fires at
            // most once per touch (`hold_fired` latch), and the finger is
            // still down when it fires — the lift is swallowed below.
            if let Some(t0) = self.hold_armed_at {
                let drift = (tp.x.abs_diff(self.hold_start.0))
                    .max(tp.y.abs_diff(self.hold_start.1));
                if drift > HOLD_SLOP_PX {
                    self.hold_armed_at = None;
                } else if t0.elapsed().as_millis() >= HOLD_MS {
                    self.hold_armed_at = None;
                    if self.hold_start.1 >= EDGE_BOTTOM_Y && shell_clean(ui) {
                        self.hold_fired = true;
                        self.req.open_switcher.set(true);
                    }
                }
            }
        } else if self.touch_down {
            self.touch_down = false;
            // touch.poll() reports the concluding swipe in the SAME iteration
            // as the finger-lift. Releasing at last_pos would also "click"
            // whatever TouchArea the swipe happened to end on (page dots,
            // cpu/gyro chips). For a swipe consumed as NAVIGATION, move the
            // pointer off-window first and release there — release-outside-
            // bounds suppresses `clicked` deterministically, regardless of
            // Slint's internal cancel semantics. Brightness-slider drags are
            // excluded: they travel far enough to classify as directional,
            // but the grabbed slider TouchArea must see the real final
            // position — an off-screen release would fire its moved handler
            // at x ≈ -1 and slam brightness to the floor. Taps and slider
            // drags keep the normal release at last_pos. The task-9 hardware
            // gate verifies this gesture behavior.
            // Power menu (#48) opacity guard: while it covers the screen there
            // is no slider — without this, a Right-swipe starting in a slider
            // band would keep the normal release at last_pos and could "click"
            // the menu row it ended on (SHUTDOWN sits inside SLIDER_BAND's y).
            let slider_drag = !ui.get_power_menu_open()
                && ((!ui.get_launcher_open()
                    && !ui.get_settings_open()
                    && ui.get_current_page() == PAGE_POWER
                    && SLIDER_BAND.contains(&swipe_start_y))
                    || hub_slider_drag(ui, swipe_start_y));
            // (The old launcher-scroll exclusion is gone with the Flickable: a
            // vertical swipe in the paged launcher is a page FLIP — navigation —
            // so the off-window release below correctly suppresses a stray tile
            // click at the lift point.)
            // A fired edge-hold (#31) also releases off-window: the switcher
            // just opened under the finger, and the lift must not click the
            // card that happens to sit at the hold point.
            let directional = (matches!(swipe, Some(d) if d != SwipeDirection::Tap)
                && !slider_drag)
                || self.hold_fired;
            self.hold_armed_at = None;
            let release_pos = if directional {
                let off = slint::LogicalPosition::new(-1.0, -1.0);
                let _ = self
                    .window
                    .window()
                    .try_dispatch_event(WindowEvent::PointerMoved { position: off });
                off
            } else {
                self.last_pos
            };
            let _ = self.window.window().try_dispatch_event(WindowEvent::PointerReleased {
                position: release_pos,
                button: PointerEventButton::Left,
            });
        }

        // A fired edge-hold consumed this whole touch (#31): the concluding
        // lift must not also classify as a tap/swipe against the switcher.
        if self.hold_fired && point.is_none() {
            self.hold_fired = false;
            return;
        }

        if let Some(direction) = swipe {
            // Ping receiver pulse (#35) first — it renders above everything
            // but AOD, so while it is up ANY swipe dismisses the greeting
            // (via the request cell: the loop owns the auto-dismiss clock).
            // The underlying screen's own gestures resume next touch.
            if ui.get_ping_pulse_open() {
                self.req.ping_pulse_tap.set(true);
                return;
            }
            // Power menu (#48) next — it stacks over EVERYTHING (launcher,
            // overlays, settings), so while it is up it swallows all nav
            // swipes and Right closes it (Flag idiom; main.rs owns nothing
            // here — the underlying app_state/WiFi holds are untouched).
            if ui.get_power_menu_open() {
                if direction == SwipeDirection::Right {
                    ui.set_power_menu_open(false);
                }
                return;
            }
            // Settings hub next (it is in OVERLAYS only for mirror/reconcile):
            // swipe up/down flips its section pages (clamped, launcher idiom);
            // Right backs out of a NETWORK sub-view via the net_back CELL (the
            // loop owns the view transitions + keyboard cleanup) or closes the
            // hub at view 0. Swipes starting on the DISPLAY page's brightness
            // slider are drags — excluded entirely.
            if ui.get_settings_open() {
                if !hub_slider_drag(ui, swipe_start_y) {
                    match direction {
                        SwipeDirection::Right => {
                            if ui.get_net_view() > 0 {
                                self.req.net_back.set(true);
                            } else {
                                ui.set_settings_open(false);
                            }
                        }
                        SwipeDirection::Up if ui.get_net_view() == 0 => {
                            ui.set_settings_page(
                                (ui.get_settings_page() + 1).min(SETTINGS_PAGE_COUNT - 1),
                            );
                        }
                        SwipeDirection::Down if ui.get_net_view() == 0 => {
                            ui.set_settings_page((ui.get_settings_page() - 1).max(0));
                        }
                        _ => {}
                    }
                }
                return;
            }
            // Overlays are full-screen over the scene, so whichever is open
            // swallows ALL nav swipes (no paging behind it) and a Right-swipe
            // closes it. Table-driven (`OVERLAYS`, in check order): stateless
            // overlays (WLED/Hunt/Voice/Sound) clear their flag directly;
            // energy/climate fire a close CELL instead so main.rs also releases
            // the WiFi hold — a direct flag-clear would strand WiFi because the
            // cell drain (which frees the radio + restores mesh) would never run.
            // (Taps still reach the tiles via the pointer events above.)
            for o in OVERLAYS {
                if (o.is_open)(ui) {
                    if direction == SwipeDirection::Right {
                        match o.close {
                            OverlayClose::Flag => (o.set_open)(ui, false),
                            OverlayClose::Cell(fire) => fire(&self.req),
                        }
                    }
                    return;
                }
            }
            // Notification shade (#32, not a registry app): swallows nav
            // swipes. Left starting ON a card dismisses it (the slot IS the
            // ring index); Up ("push it back up") or Right closes.
            if ui.get_shade_open() {
                match direction {
                    SwipeDirection::Right | SwipeDirection::Up => ui.set_shade_open(false),
                    SwipeDirection::Left => {
                        if let Some(slot) = shade_slot(swipe_start_y) {
                            self.req.notif_dismiss.set(Some(slot as i32));
                        }
                    }
                    _ => {}
                }
                return;
            }
            // App switcher (#31, not a registry app): swallows nav swipes.
            // Up starting ON a card kills that session — via a request cell,
            // the loop owns the session list and rebuilds the cards in place.
            // Right (the universal close) or Down ("push it back down") closes.
            if ui.get_switcher_open() {
                match direction {
                    SwipeDirection::Right | SwipeDirection::Down => {
                        ui.set_switcher_open(false)
                    }
                    SwipeDirection::Up => {
                        if let Some(slot) = switcher_slot(swipe_start_y) {
                            if let Some(&idx) = self.switcher_rows.get(slot) {
                                self.req.switcher_kill.set(Some(idx));
                            }
                        }
                    }
                    _ => {}
                }
                return;
            }
            // Launcher overlay next (not a registry app): it swallows nav swipes
            // wherever they start (including the power page's slider band).
            // PAGED: swipe up/down flips exactly one section page (clamped at
            // the ends — the dots show position); Right closes. One flip = one
            // hard-cut full-frame render, replacing the 6-10fps Flickable
            // scroll that dirtied the whole viewport per frame.
            if ui.get_launcher_open() {
                match direction {
                    SwipeDirection::Right => ui.set_launcher_open(false),
                    SwipeDirection::Up => {
                        let last = ui.get_launcher_page_count() - 1;
                        ui.set_launcher_page((ui.get_launcher_page() + 1).min(last.max(0)));
                    }
                    SwipeDirection::Down => {
                        ui.set_launcher_page((ui.get_launcher_page() - 1).max(0));
                    }
                    _ => {}
                }
                return;
            }
            // Horizontal swipes starting on the power page's brightness
            // slider are slider drags, not page switches.
            let on_slider =
                ui.get_current_page() == PAGE_POWER && SLIDER_BAND.contains(&swipe_start_y);
            if on_slider {
                return;
            }
            // "Seen it" latches: a nav gesture actually used retires its hint
            // for the rest of the boot (and drops the strip mid-window — no
            // point teaching a gesture that was just performed).
            match direction {
                SwipeDirection::Left => {
                    self.hint_seen_lr = true;
                    ui.set_hint_sides(false);
                    ui.set_current_page((ui.get_current_page() + 1).rem_euclid(PAGE_COUNT))
                }
                SwipeDirection::Right => {
                    self.hint_seen_lr = true;
                    ui.set_hint_sides(false);
                    ui.set_current_page(
                        (ui.get_current_page() + PAGE_COUNT - 1).rem_euclid(PAGE_COUNT),
                    )
                }
                // Launcher (#29): a bottom-EDGE swipe up opens it from ANY
                // watchface page (the standard wearable gesture); a mid-screen
                // swipe up keeps the legacy clock-page-only behavior.
                SwipeDirection::Up
                    if swipe_start_y >= EDGE_BOTTOM_Y
                        || ui.get_current_page() == PAGE_CLOCK =>
                {
                    self.hint_seen_up = true;
                    ui.set_hint_up(false);
                    ui.set_launcher_open(true)
                }
                // Notification shade (#32): a top-edge swipe down pulls it
                // over any watchface page. Mid-screen Down stays free.
                SwipeDirection::Down if swipe_start_y <= EDGE_TOP_Y => {
                    self.hint_seen_down = true;
                    ui.set_hint_down(false);
                    self.req.open_shade.set(true);
                }
                _ => {}
            }
        }
    }

    // Shell API surface awaiting its first caller (gesture polish, Task 12).
    #[allow(dead_code)]
    pub fn touch_is_down(&self) -> bool {
        self.touch_down
    }

    // === gesture hints (wake shimmer) ===

    /// Arm the wake gesture hints. Called by main.rs at every wake-to-bright
    /// seam (tap/button, wrist-raise, boot). Shows nothing yet — the strips
    /// are created invisible and [`tick_hints`] blooms them ~150ms later, so
    /// the wake frame itself stays hint-free. No-ops once both gestures have
    /// been used this boot, or while a game holds the panel. Armed on EVERY
    /// watchface page since #29: the bottom handle now means "edge-swipe up →
    /// launcher", which is honest everywhere (the sides always were — the
    /// carousel wraps).
    pub fn hint_wake(&mut self) {
        if self.hint_seen_lr && self.hint_seen_up && self.hint_seen_down {
            return;
        }
        // Waking straight into an open modal (screen timed out over the
        // shade/switcher): the strips would shimmer under its scrim.
        if self.modal_open() {
            return;
        }
        let Some(ui) = self.ui.as_ref() else {
            return;
        };
        self.hint_armed_at = Some(embassy_time::Instant::now());
        self.hint_lit = false;
        ui.set_hints_lit(false);
        ui.set_hint_sides(!self.hint_seen_lr);
        ui.set_hint_up(!self.hint_seen_up);
        ui.set_hint_down(!self.hint_seen_down);
    }

    /// True while a hint window is running. main.rs ORs this into its 33ms
    /// frame-pacing condition so the bloom/fade tweens get frames even when
    /// the idle clock page would otherwise tick at 1Hz (`draw_if_needed`
    /// no-ops through the hold phase, so the extra ticks stay cheap).
    pub fn hints_pending(&self) -> bool {
        self.hint_armed_at.is_some()
    }

    /// Tear the hints down immediately (window expired, an overlay/app took
    /// the screen, or the scene is being suspended). Idempotent.
    fn hints_cancel(&mut self) {
        self.hint_armed_at = None;
        self.hint_lit = false;
        if let Some(ui) = self.ui.as_ref() {
            ui.set_hint_sides(false);
            ui.set_hint_up(false);
            ui.set_hint_down(false);
            ui.set_hints_lit(false);
        }
    }

    /// Walk the hint choreography; called from [`render`] each shell tick.
    /// Everything is edge-triggered off `hint_lit`, so the hold phase sets no
    /// properties (no dirty regions, no repaints).
    fn tick_hints(&mut self) {
        let Some(t0) = self.hint_armed_at else {
            return;
        };
        let elapsed = t0.elapsed().as_millis();
        if elapsed >= HINT_KILL_MS {
            self.hints_cancel(); // fade finished — destroy the strips
            return;
        }
        let Some(ui) = self.ui.as_ref() else {
            return;
        };
        if elapsed >= HINT_FADE_MS {
            if self.hint_lit {
                self.hint_lit = false;
                ui.set_hints_lit(false); // → 480ms fade-out tween
            }
        } else if elapsed >= HINT_BLOOM_MS && !self.hint_lit {
            self.hint_lit = true;
            ui.set_hints_lit(true); // → 480ms bloom-in tween
        }
    }

    /// Mirror `app_state` into the launcher + overlay open-flags before feeding
    /// touch, so the scene shows the overlay the loop is in. Table-driven
    /// (`OVERLAYS`); no-op while the scene is suspended.
    pub fn mirror_overlays(&mut self, app_state: AppState) {
        // Anything on top of the watchface retires a running hint window —
        // the user is already navigating, and this keeps the strips out from
        // under the launcher scrim (the Slint gates cover the same frame).
        if app_state != AppState::Watchface && self.hint_armed_at.is_some() {
            self.hints_cancel();
        }
        let Some(ui) = self.ui.as_ref() else {
            return;
        };
        ui.set_launcher_open(app_state == AppState::Launcher);
        for o in OVERLAYS {
            (o.set_open)(ui, app_state == o.state);
        }
    }

    /// After [`handle_touch`], reconcile which launcher/overlay is still open
    /// back into an `AppState` (a Right-swipe may have closed one). Overlays that
    /// close via a request cell (energy/climate) still report open here until the
    /// main loop drains the cell. Table-driven (`OVERLAYS`); the launcher wins
    /// over overlays, matching the previous if-ladder order.
    pub fn reconcile_overlay(&self) -> AppState {
        let Some(ui) = self.ui.as_ref() else {
            return AppState::Watchface;
        };
        if ui.get_launcher_open() {
            return AppState::Launcher;
        }
        for o in OVERLAYS {
            if (o.is_open)(ui) {
                return o.state;
            }
        }
        AppState::Watchface
    }

    // === property push (call only when the source value changed) ===

    /// Returns true when the second ticked (caller may gate 1Hz work on it).
    pub fn set_time(&mut self, dt: &DateTime) -> bool {
        if dt.seconds == self.last_second {
            return false;
        }
        self.last_second = dt.seconds;
        let Some(ui) = self.ui.as_ref() else { return true; };
        ui.set_time_text(slint::format!("{:02}:{:02}", dt.hours, dt.minutes));
        ui.set_seconds_text(slint::format!("{:02}", dt.seconds));
        let weekday = WEEKDAYS[(dt.weekday % 7) as usize];
        let month = MONTHS[(dt.month.clamp(1, 12) - 1) as usize];
        ui.set_date_text(slint::format!(
            "{} {:02} {} 20{:02}", weekday, dt.day, month, dt.year
        ));
        ui.set_minute_progress(dt.seconds as f32 / 59.0);
        true
    }

    pub fn set_battery(&self, pct: u8, mv: u16, charging: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_battery_percent(pct.min(100) as i32);
        ui.set_charging(charging);
        let _ = mv; // chrome shows percent; power page (task 6) consumes mv
    }

    pub fn set_radios(&self, wifi: bool, ble: bool, mesh_peers: u8) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_wifi_on(wifi);
        ui.set_ble_on(ble);
        ui.set_mesh_peers(mesh_peers as i32);
    }

    pub fn set_steps(&self, steps: u32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_steps(steps as i32);
    }

    pub fn set_cpu_mhz(&self, mhz: u16) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_cpu_text(slint::format!("{} MHz", mhz));
    }

    pub fn set_gyro(&self, on: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_gyro_on(on);
    }

    pub fn set_sensors(&self, accel: (f32, f32, f32), gyro: (i16, i16, i16), temp_dc: i16) {
        // Sensors update at 100ms; skip the 3 SharedString allocs when the page
        // isn't showing rather than relying on caller discipline.
        let Some(ui) = self.ui.as_ref() else { return; };
        if ui.get_current_page() != PAGE_SENSORS { return; }
        ui.set_accel_text(slint::format!(
            "{:+.2} {:+.2} {:+.2} g", accel.0, accel.1, accel.2
        ));
        ui.set_gyro_text(slint::format!(
            "{:+.1} {:+.1} {:+.1} dps", gyro.0 as f32 / 10.0, gyro.1 as f32 / 10.0,
            gyro.2 as f32 / 10.0
        ));
        ui.set_imu_temp_text(slint::format!("{:.1} C", temp_dc as f32 / 10.0));
    }

    /// C6 on-die temperature (deci-degrees C) → sensors page (#54).
    pub fn set_die_temp(&self, dc: i16) {
        let Some(ui) = self.ui.as_ref() else { return; };
        if ui.get_current_page() != PAGE_SENSORS {
            return;
        }
        ui.set_die_temp_text(slint::format!("{:.1} C", dc as f32 / 10.0));
    }

    pub fn set_system(&self, heap_free: usize, batt_pct: u8, batt_mv: u16) {
        // System page refreshes at 2s; skip the SharedString allocs when the
        // page isn't showing rather than relying on caller discipline.
        let Some(ui) = self.ui.as_ref() else { return; };
        if ui.get_current_page() != PAGE_SYSTEM {
            return;
        }
        ui.set_heap_text(slint::format!("{}k free", heap_free / 1024));
        let s = embassy_time::Instant::now().as_secs();
        ui.set_uptime_text(slint::format!(
            "{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60
        ));
        ui.set_battery_text(slint::format!("{}% \u{00b7} {} mV", batt_pct, batt_mv));
    }

    pub fn set_power(&self, stats: &crate::peripherals::power_stats::PowerStats) {
        // Power page refreshes at 1s; skip the alloc churn when not showing.
        let Some(ui) = self.ui.as_ref() else { return; };
        if ui.get_current_page() != PAGE_POWER {
            return;
        }
        use crate::peripherals::power_stats::{on_off, BATTERY_CAPACITY_MAH};
        // Per-subsystem cells mirror the old "POWER MONITOR" read-out; all
        // labels and mA come from PowerStats (single source of truth, shared
        // with the legacy eg renderer until task 13 deletes it). SDCARD is
        // omitted: the C6 board has no SD slot and main.rs never sets sd_on.
        // cpu-text (clock chip) is untouched — the CPU cell has its own MHz.
        ui.set_cpu_cell(slint::format!(
            "{}MHz \u{00b7} {}mA", stats.cpu_mhz, stats.base_ma()
        ));
        ui.set_display_cell(slint::format!(
            "{} \u{00b7} {}mA", stats.display_label(), stats.display_ma()
        ));
        ui.set_wifi_cell(slint::format!(
            "{} \u{00b7} {}mA", stats.wifi_label(), stats.wifi_ma()
        ));
        ui.set_ble_cell(slint::format!(
            "{} \u{00b7} {}mA", on_off(stats.ble_on), stats.ble_ma()
        ));
        ui.set_imu_cell(slint::format!(
            "{} \u{00b7} {}mA", on_off(stats.imu_on), stats.imu_ma()
        ));
        ui.set_audio_cell(slint::format!(
            "{} \u{00b7} {}mA", on_off(stats.audio_on), stats.audio_ma()
        ));
        ui.set_total_ma(stats.total_ma() as i32);
        let full = stats.full_runtime_hours(BATTERY_CAPACITY_MAH);
        let left = stats.estimated_hours(BATTERY_CAPACITY_MAH);
        ui.set_left_hours(left as i32);
        let full_s: SharedString =
            if full >= 999 { "--".into() } else { slint::format!("{}h", full) };
        let left_s: SharedString =
            if left >= 999 { "--".into() } else { slint::format!("~{}h", left) };
        ui.set_runtime_text(slint::format!("100%: {} \u{00b7} left: {}", full_s, left_s));
    }

    /// Push the LP (low-power RISC-V) core status to the power page. Static for
    /// now: offload got a RED verdict (task #24), so this is an availability
    /// indicator, not a live workload. Formatted as "<state> \u{00b7} <mhz> MHz"
    /// to match the power page's read-out style; set once from main.rs (no page
    /// gate — the value never changes, so it persists until the page shows).
    pub fn set_lp_core(&self, state: &str, mhz: u16) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_lp_core_text(slint::format!("{} \u{00b7} {} MHz", state, mhz));
    }

    pub fn set_weather(&self, temp_f: Option<i16>, code: u8) {
        let Some(ui) = self.ui.as_ref() else { return; };
        match temp_f {
            Some(t) => {
                ui.set_weather_text(slint::format!("{}\u{00b0}F {}", t, weather_label(code)))
            }
            None => ui.set_weather_text(SharedString::new()),
        }
    }

    pub fn set_brightness_from_raw(&self, raw: u8) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_brightness((raw.saturating_sub(BRIGHTNESS_MIN)) as f32
            / (0xFF - BRIGHTNESS_MIN) as f32);
    }

    pub fn set_aod(&self, on: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_aod(on);
    }

    /// Push the Mesh Familiar snapshot to the clock nook (task 12).
    pub fn set_fam(&self, f: &crate::net::familiar::FamUi) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_fam_known(f.known);
        ui.set_fam_holding(f.holding);
        ui.set_fam_mood(f.mood as i32);
        ui.set_fam_hunger(f.hunger as i32);
        ui.set_fam_stage(f.stage as i32);
    }

    /// Feed scaled accel into the clock's parallax offsets, clamped to ±12px so
    /// the time/date never collide with the chrome. Fed only on the clock page
    /// with the gyro toy enabled. `par-x`/`par-y` are `length` in .slint; the
    /// generated setters take logical pixels as f32.
    pub fn set_parallax(&self, ax: f32, ay: f32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_par_x((ax * 12.0).clamp(-12.0, 12.0));
        ui.set_par_y((ay * 12.0).clamp(-12.0, 12.0));
    }

    pub fn set_toast(&self, text: &str) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_toast_text(SharedString::from(text));
    }

    pub fn set_launcher_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_launcher_open(open);
    }

    pub fn set_wled_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_wled_open(open);
    }

    /// Feedback line under the WLED tiles ("→ On", "Radio off …", "" = idle).
    pub fn set_wled_status(&self, text: &str) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_wled_status(SharedString::from(text));
    }

    pub fn set_hunt_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_hunt_open(open);
    }

    /// Push one hunt tick: maps `hunt::HuntView` onto the HuntPage props. The
    /// view→UI derivations (target noun, bar fraction, trend arrow/flags) live
    /// here in the UI layer so main.rs only owns the RSSI feed + game state.
    pub fn set_hunt(&self, v: &hunt::HuntView) {
        let Some(ui) = self.ui.as_ref() else { return; };
        let noun = v.target.map_or("--", |id| names::name_for_id(id).1);
        ui.set_hunt_seek(slint::format!("SEEK {noun}"));
        ui.set_hunt_hero(SharedString::from(v.trend.word()));
        let arrow = match v.trend {
            hunt::Trend::Warmer => "\u{2191}",
            hunt::Trend::Colder => "\u{2193}",
            _ => "",
        };
        ui.set_hunt_arrow(SharedString::from(arrow));
        let frac = (rssi::bar_px(v.smoothed_rssi, 1000) as f32 / 1000.0).clamp(0.0, 1.0);
        ui.set_hunt_bar(frac);
        if v.present {
            ui.set_hunt_rssi(slint::format!("{} dBm", v.smoothed_rssi));
        } else {
            ui.set_hunt_rssi(SharedString::from("--"));
        }
        ui.set_hunt_bucket(SharedString::from(rssi::label(v.proximity).trim_end()));
        ui.set_hunt_hot(matches!(
            v.proximity,
            rssi::Proximity::Here | rssi::Proximity::Near
        ));
        ui.set_hunt_found(v.trend == hunt::Trend::Found);
        ui.set_hunt_warmer(v.trend == hunt::Trend::Warmer);
        ui.set_hunt_colder(v.trend == hunt::Trend::Colder);
    }

    pub fn set_energy_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_energy_open(open);
    }

    /// Home energy figures (grid_w is SIGNED: >0 importing, <0 exporting).
    pub fn set_energy(&self, batt_pct: i32, solar_w: i32, grid_w: i32, charging: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_energy_batt(batt_pct);
        ui.set_energy_solar_w(solar_w);
        ui.set_energy_grid_w(grid_w);
        ui.set_energy_charging(charging);
    }

    /// Energy connection banner: 0 ready · 1 connecting · 2 HA unreachable (#58).
    pub fn set_energy_conn(&self, conn: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_energy_conn(conn);
    }

    pub fn set_climate_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_climate_open(open);
    }

    /// Push the climate roster → `ClimateCard` rows + the connection banner.
    /// `conn`: 0 disconnected · 1 connecting · 2 live. The ClimateState→UI
    /// mapping lives here in the UI layer (main.rs owns the session + commands).
    /// `mode`/`action` use `as i32` (not `as_ui()`) so this compiles identically
    /// on the stub (`repr(u8)`) and the real crate (`repr(i32)`) — swap-safe.
    pub fn set_climate(&self, state: &climate_model::ClimateState, conn: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        let cards: alloc::vec::Vec<ClimateCard> = state
            .entities
            .iter()
            .enumerate()
            .map(|(i, (_obj, e))| ClimateCard {
                id: i as i32,
                name: SharedString::from(e.name.as_str()),
                cur: match e.cur {
                    Some(c) => slint::format!("{:.0}", c),
                    None => SharedString::from("--"),
                },
                setpoint: e.set.unwrap_or(e.min),
                mode: e.mode as i32,
                action: e.action as i32,
                min: e.min,
                max: e.max,
                step: e.step,
                modes_mask: e.modes_mask() as i32,
                unit: SharedString::from("\u{00b0}F"),
            })
            .collect();
        self.climate_cards.set_vec(cards);
        ui.set_climate_conn(conn);
    }

    pub fn set_lights_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_lights_open(open);
    }

    /// Push the room-lights snapshot (#39) to the Lights overlay.
    /// `status`: 0 finding (no state yet / connecting) · 1 ok · 2 no_presence ·
    /// 3 error. `pending`: 0 idle · 1 sent (cmd in flight, awaiting HA's
    /// republish) · 2 no-reply hint. Only called while the screen is open.
    pub fn set_lights(&self, area: &str, on: u8, total: u8, status: i32, pending: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_lights_area(SharedString::from(area));
        ui.set_lights_on_count(on as i32);
        ui.set_lights_total(total as i32);
        ui.set_lights_status(status);
        ui.set_lights_pending(pending);
    }

    pub fn set_ping_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_ping_open(open);
    }

    /// Push the ping-plugin snapshot (#35). `state`: 0 idle · 1 sent ·
    /// 2 delivered · 3 no reply. `peer` is the resolved target sigil ("" =
    /// none heard yet → "PING THE FLEET"); `result` the ACKER's sigil for the
    /// delivered caption; `cooling` drives the 3s recharge sweep. Only called
    /// while the screen is open, gated on change by the loop.
    pub fn set_ping(&self, peer: &str, state: i32, result: &str, cooling: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_ping_peer(SharedString::from(peer));
        ui.set_ping_state(state);
        ui.set_ping_result(SharedString::from(result));
        ui.set_ping_cooling(cooling);
    }

    // === Ping receiver pulse (#35) ===

    /// Bloom the full-screen greeting pulse: `from` is the sender's sigil.
    /// Created dark (`lit:false` — the wake frame stays cheap, the hint
    /// idiom); [`tick_ping_pulse`] breathes the rings on the render clock and
    /// auto-dismisses after ~4s. Re-arming while up (a rapid re-ping the loop
    /// chose to surface) restarts the choreography with the new sender.
    pub fn ping_pulse_show(&mut self, from: &str) {
        // The strips would shimmer under the pulse's scrim — retire them.
        self.hints_cancel();
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_ping_pulse_from(SharedString::from(from));
        ui.set_ping_pulse_lit(false);
        ui.set_ping_pulse_open(true);
        self.ping_pulse_armed_at = Some(embassy_time::Instant::now());
        self.ping_pulse_stage = 0;
    }

    /// Tear the pulse down (tap/swipe dismiss, auto-dismiss, scene suspend).
    /// Idempotent.
    pub fn ping_pulse_dismiss(&mut self) {
        self.ping_pulse_armed_at = None;
        self.ping_pulse_stage = 0;
        if let Some(ui) = self.ui.as_ref() {
            ui.set_ping_pulse_open(false);
            ui.set_ping_pulse_lit(false);
        }
    }

    /// True while the greeting pulse is up. main.rs ORs this into its 33ms
    /// frame-pacing condition (the hints_pending pattern) so the ring tweens
    /// get frames even on a 1Hz clock page; it also absorbs a same-sender
    /// re-ping in the loop's dedup.
    pub fn ping_pulse_active(&self) -> bool {
        self.ping_pulse_armed_at.is_some()
    }

    /// Walk the pulse choreography; called from [`render`] each shell tick
    /// (the tick_hints pattern). Edge-triggered per stage, so the breathing
    /// phases between edges set no properties. Schedule: bloom at +150ms
    /// (1.1s ease-out tween), contract at +1.6s, bloom again at +2.75s,
    /// auto-dismiss at +4.2s.
    fn tick_ping_pulse(&mut self) {
        let Some(t0) = self.ping_pulse_armed_at else {
            return;
        };
        let elapsed = t0.elapsed().as_millis();
        if elapsed >= 4200 {
            self.ping_pulse_dismiss();
            return;
        }
        let Some(ui) = self.ui.as_ref() else {
            return;
        };
        let (stage, lit) = match elapsed {
            0..150 => (0, None),
            150..1600 => (1, Some(true)),
            1600..2750 => (2, Some(false)),
            _ => (3, Some(true)),
        };
        if stage != self.ping_pulse_stage {
            self.ping_pulse_stage = stage;
            if let Some(lit) = lit {
                ui.set_ping_pulse_lit(lit);
            }
        }
    }

    pub fn set_voice_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_voice_open(open);
    }

    /// Voice UI state: 0 idle · 1 listening · 2 sending · 3 result · 4 error · 5 connecting.
    pub fn set_voice_state(&self, state: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_voice_state(state);
    }

    /// Input level 0..1 (drives the listening pulse). Unused today: the loop is
    /// parked in the stream `.await` for the whole hold, so there's no frame to
    /// push a live level to — kept as the hook for the MC6 level-meter polish.
    #[allow(dead_code)]
    pub fn set_voice_level(&self, level: f32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_voice_level(level);
    }

    /// The recognised transcript (shown in state 3).
    pub fn set_voice_transcript(&self, text: &str) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_voice_transcript(SharedString::from(text));
    }

    /// Error message (shown in state 4; "" → the page's default "No speech").
    pub fn set_voice_error(&self, text: &str) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_voice_error(SharedString::from(text));
    }

    pub fn set_mic_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_mic_open(open);
    }

    // ================= Story (#story) =================
    // Every setter is a plain push from a `story_proto` model the loop already
    // holds; none of them allocates beyond the SharedStrings Slint needs, and
    // none of them holds prose (the parser never kept any).

    pub fn set_story_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_story_open(open);
    }

    /// 0 list · 1 play · 2 stats · 3 character.
    pub fn set_story_page(&self, page: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_story_page(page.clamp(0, 3));
    }

    pub fn story_page(&self) -> i32 {
        self.ui.as_ref().map_or(0, |ui| ui.get_story_page())
    }

    pub fn set_story_loading(&self, loading: bool, error: &str) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_story_loading(loading);
        ui.set_story_error(SharedString::from(error));
    }

    /// Push the visible chapter rows.
    ///
    /// Gated on `story` only because it names a `story_proto` type; the scene and
    /// every other Story setter are feature-INDEPENDENT, so a default build
    /// compiles byte-identical UI code (the same discipline the `tts` config
    /// record follows).
    #[cfg(feature = "story")]
    ///
    /// `more` is the count the retained window could not hold, so the list can
    /// say "+N more" instead of implying it showed everything — the same
    /// no-silent-caps rule the parser follows.
    pub fn set_story_chapters(
        &self,
        rows: &[story_proto::model::ChapterRow],
        current: u16,
        more: u16,
    ) {
        let Some(ui) = self.ui.as_ref() else { return; };
        // Only what the screen can draw — see VISIBLE_CHAPTERS.
        let items: Vec<StoryChapter> = rows
            .iter()
            .take(story_proto::model::VISIBLE_CHAPTERS)
            .map(|r| StoryChapter {
                number: r.number as i32,
                title: SharedString::from(r.title.as_str()),
                duration: SharedString::from(mmss(r.duration_ms).as_str()),
                playable: r.playable(),
                current: r.number == current,
            })
            .collect();
        let shown = items.len();
        self.story_chapters.set_vec(items);
        // "+N more" counts BOTH what the parse cap dropped and what this page did
        // not show, so the number is honest about the whole remainder.
        let unshown = rows.len().saturating_sub(shown) as u16;
        ui.set_story_more(more.saturating_add(unshown) as i32);
    }

    /// Playback state. Called on segment change and on a coarse progress tick —
    /// NEVER per frame: the paint has to fit inside the 48 ms DMA ring
    /// (`net::story_play::PAINT_BUDGET_MS`).
    #[allow(clippy::too_many_arguments)]
    /// Whether a paused chapter can be resumed. Drives PAUSE vs RESUME on the READ
    /// page, and suppresses the "tap a chapter in LIST to play" hint — which over a
    /// paused chapter reads as "your place was lost".
    pub fn set_story_paused(&self, paused: bool) {
        // Scene may be suspended (a framebuffer game took the display), so the UI is an
        // Option — matching every sibling setter.
        let Some(ui) = self.ui.as_ref() else {
            return;
        };
        ui.set_story_paused(paused);
    }

    pub fn set_story_playback(
        &self,
        title: &str,
        speaker: &str,
        kind: i32,
        position_ms: u32,
        duration_ms: u32,
        seg_index: i32,
        seg_count: i32,
        playing: bool,
    ) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_story_play_title(SharedString::from(title));
        ui.set_story_speaker(SharedString::from(speaker));
        ui.set_story_speaker_kind(kind);
        ui.set_story_progress(if duration_ms == 0 {
            0.0
        } else {
            (position_ms as f32 / duration_ms as f32).clamp(0.0, 1.0)
        });
        ui.set_story_elapsed(SharedString::from(mmss(position_ms).as_str()));
        ui.set_story_total(SharedString::from(mmss(duration_ms).as_str()));
        ui.set_story_seg_index(seg_index);
        ui.set_story_seg_count(seg_count);
        ui.set_story_playing(playing);
    }

    /// `no_manifest`: the segment index refused to drive highlighting (over the
    /// cap, non-contiguous, or a rate this hardware cannot play).
    /// `highlight_off`: the paint gate tripped mid-chapter to protect the audio.
    pub fn set_story_highlight_state(&self, no_manifest: bool, highlight_off: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_story_no_manifest(no_manifest);
        ui.set_story_highlight_off(highlight_off);
    }

    /// Push the stats and character pages from one `/api/character` response.
    #[cfg(feature = "story")]
    ///
    /// Absent values become an em-dash and, for HP, **suppress the bar entirely**
    /// rather than drawing it empty: on the live ledger `hp` is null against
    /// `max_hp` 110, and a zero-width fill would assert the protagonist is dead.
    pub fn set_story_character(&self, c: &story_proto::model::Character) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_story_subject(SharedString::from(c.subject.as_str()));
        ui.set_story_level(SharedString::from(opt_num(c.level.map(u32::from)).as_str()));
        ui.set_story_xp(SharedString::from(opt_num(c.xp).as_str()));
        ui.set_story_gold(SharedString::from(opt_num(c.gold).as_str()));
        ui.set_story_location(SharedString::from(
            c.location.as_ref().map_or("—", |s| s.as_str()),
        ));
        ui.set_story_status(SharedString::from(
            c.status.as_ref().map_or("", |s| s.as_str()),
        ));

        let hp_known = c.hp.is_some() && c.max_hp.is_some();
        ui.set_story_hp_known(hp_known);
        ui.set_story_hp_frac(c.hp_fraction().unwrap_or(0.0));
        let mut hp: heapless::String<24> = heapless::String::new();
        match (c.hp, c.max_hp) {
            (Some(h), Some(m)) => {
                let _ = story_proto::push_u32(&mut hp, h);
                let _ = hp.push_str(" / ");
                let _ = story_proto::push_u32(&mut hp, m);
            }
            // max_hp alone is still worth showing — it says the ledger knows the
            // ceiling but not the current value, which is different from "no HP".
            (None, Some(m)) => {
                let _ = hp.push_str("— / ");
                let _ = story_proto::push_u32(&mut hp, m);
            }
            _ => {
                let _ = hp.push_str("—");
            }
        }
        ui.set_story_hp_text(SharedString::from(hp.as_str()));

        // NOTE no inventory model is built here. `story-inventory` was declared,
        // bound, and repopulated on every character update — and referenced by no
        // element in any .slint file, so it rendered nowhere. Confirmed two ways: by
        // elimination (page 2's scene counts were flat at 170/160 for inventory
        // sizes 0, 2, 4 and 8, and page 3 measured 60/53 while the model held 8 rows
        // of 25-char labels — impossible if drawn) and by grepping every .slint for
        // the property. It cost 8 `SharedString` pairs per update, ~416-672 B
        // steady-state and ~830-1,340 B transient, since `set_vec` builds the new
        // generation before dropping the old.

        // FORCED-RUNG MEASUREMENT STUB (#75), `story-stub-slots`, never ship.
        //
        // The live ledger sends all 17 equipment/appearance slots null, and
        // `story.slint:517` renders an unknown slot as `"—"` — ONE glyph. So the
        // served CHAR page cannot reach the 512 scene-pool rung no matter how long
        // a real value would be, and the 512 cliff is unmeasurable against this
        // daemon. This stub substitutes a `MAX_SLOT_VAL`-length value with
        // `known: true` into every slot, which is the state the daemon WILL produce
        // once it sends equipment data (its naming style runs 22-28 chars).
        //
        // Forcing the value alone is not enough: without `known: true` the `"—"`
        // branch renders and the scene never grows. Both are required.
        //
        // Firmware-side rather than daemon-side deliberately: the daemon is a
        // separate project, this keeps the change in a tree we control, makes it a
        // build with its own sigil, and cannot leak into served data.
        #[cfg(feature = "story-stub-slots")]
        const STUB_SLOT_VAL: &str = "ABCDEFGHIJKLMNOPQRSTUVWX"; // 24 = MAX_SLOT_VAL

        // Deploy-time gate marker. `watchctl deploy` greps the image for
        // `NEVER-SHIP:` and refuses to flash it, so this feature cannot reach a
        // watch by accident. A marker rather than a comment because MEASURED:
        // this build REBOOTS on first paint of the CHAR page — a 14,336 B
        // `Vec<SceneTexture>` doubling fails in `draw_text_paragraph`. It does not
        // merely contain test code, it ships the crash regime.
        #[cfg(feature = "story-stub-slots")]
        #[used]
        static NEVER_SHIP: &str =
            "NEVER-SHIP: story-stub-slots (reboots on story CHAR page)\0";

        /// `(value, known)` for one slot. The stub ignores the payload entirely.
        #[cfg(feature = "story-stub-slots")]
        let slot = |_v: Option<&str>| (SharedString::from(STUB_SLOT_VAL), true);
        #[cfg(not(feature = "story-stub-slots"))]
        let slot = |v: Option<&str>| match v {
            Some(v) => (SharedString::from(v), true),
            None => (SharedString::default(), false),
        };

        let equip: Vec<StorySlot> = story_proto::model::EQUIP_LABELS
            .iter()
            .enumerate()
            .map(|(i, label)| {
                // Bind ONCE. `slot` allocates, so calling it for `.0` and again for
                // `.1` builds the SharedString twice and drops one — which would
                // silently undo 0729873's empty-string fix on every populated row.
                let (value, known) = slot(c.equip_at(i));
                StorySlot {
                    label: SharedString::from(*label),
                    value,
                    known,
                }
            })
            .collect();
        self.story_equipment.set_vec(equip);

        let appear: Vec<StorySlot> = story_proto::model::APPEAR_LABELS
            .iter()
            .enumerate()
            .map(|(i, label)| {
                // Bind ONCE. `slot` allocates, so calling it for `.0` and again for
                // `.1` builds the SharedString twice and drops one — which would
                // silently undo 0729873's empty-string fix on every populated row.
                let (value, known) = slot(c.appear_at(i));
                StorySlot {
                    label: SharedString::from(*label),
                    value,
                    known,
                }
            })
            .collect();
        self.story_appearance.set_vec(appear);

        ui.set_story_equipped_count(c.equipped_count() as i32);
        ui.set_story_appearance_count(c.appearance_count() as i32);
    }

    /// Apply a theme scheme (0 Midnight · 1 Paper · 2 Amber · 3 Violet) to the
    /// Slint Theme global — every screen repaints. Clamped to the valid range and
    /// stored so a scene rebuild (suspend/resume) re-applies it. Called at boot
    /// with the persisted choice and when the picker's choice is drained.
    pub fn set_scheme(&mut self, scheme: i32) {
        let scheme = scheme.clamp(0, 3);
        self.scheme = scheme;
        if let Some(ui) = self.ui.as_ref() {
            ui.global::<Theme>().set_scheme(scheme);
        }
    }

    /// Raise/lower the Theme picker overlay. Wired by the launcher (plugin
    /// registry) when the "Theme" tile is tapped.
    pub fn set_theme_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_theme_open(open);
    }

    // === Power menu (#48) ===
    // Raised by main.rs from the AXP2101 PWRON long-press poll; closed by
    // Slint (CANCEL/chevron), handle_touch (Right-swipe), or main.rs when the
    // screen sleeps. Not in OVERLAYS: it is not an AppState — it stacks over
    // whatever is open without touching app_state or the WiFi holds.
    pub fn set_power_menu_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_power_menu_open(open);
    }

    /// False while the scene is suspended (a game holds the framebuffer —
    /// the menu cannot be open then).
    pub fn power_menu_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.get_power_menu_open())
    }

    /// VBUS (USB power) presence — drives the menu's shutdown caption.
    pub fn set_vbus(&self, on: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_vbus_on(on);
    }

    // === Settings hub (v0.9.0, #49) ===

    /// Raise/lower the Settings hub. Opening resets to the first section page
    /// (the sub-view is reset by the loop via [`set_net_view`], which owns it).
    pub fn set_settings_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        if open {
            ui.set_settings_page(0);
        }
        ui.set_settings_open(open);
    }

    /// NETWORK sub-view: 0 hub pages · 1 scan picker · 2 keyboard. RUST-OWNED —
    /// the only writer; Slint reads it and emits back/pick/done intents.
    pub fn set_net_view(&self, view: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_net_view(view.clamp(0, 2));
    }

    /// Persisted every-touch tick gate (#49) shown on the SOUND page.
    pub fn set_touch_sound(&self, on: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_touch_sound_on(on);
    }

    /// Mesh TOGGLE state for the RADIOS page (distinct from the chrome dot,
    /// which lights on live peer count).
    pub fn set_mesh_enabled(&self, on: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_mesh_enabled(on);
    }

    /// Persisted WiFi INTENT (auto vs forced-off) for the RADIOS page; the
    /// live association state rides the existing `wifi-on` chrome property.
    pub fn set_wifi_intent(&self, auto: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_wifi_intent(auto);
    }

    /// Mesh node id for the SYSTEM page (sigil-arbitrated at boot).
    pub fn set_node_id(&self, id: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_node_id(id);
    }

    /// One-line OTA status under UPDATE FIRMWARE ("" hides it) — the port of
    /// the old fb Settings status line.
    pub fn set_ota_status(&self, text: &str) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_ota_status(SharedString::from(text));
    }

    /// Currently-configured SSID for the NETWORK page ("" = not configured).
    pub fn set_net_current(&self, ssid: &str) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_net_current(SharedString::from(ssid));
    }

    /// NETWORK connect feedback: 0 idle · 1 connecting · 2 connected · 3 failed.
    pub fn set_net_status(&self, status: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_net_status(status);
    }

    /// Picker scanning state ("Scanning…" vs the row list).
    pub fn set_net_scanning(&self, scanning: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_net_scanning(scanning);
    }

    /// Push the scanned-network rows (already dedup'd + strength-sorted by the
    /// loop; capped there to the picker's 6 visible rows). The RSSI→bars
    /// bucketing is UI-layer mapping, so it lives here like set_hunt's.
    pub fn set_wifi_nets(&self, rows: &[(heapless::String<32>, i8, bool)]) {
        if self.ui.is_none() {
            return;
        }
        let nets: Vec<WifiNet> = rows
            .iter()
            .map(|(ssid, rssi, secured)| WifiNet {
                ssid: SharedString::from(ssid.as_str()),
                bars: match *rssi {
                    r if r >= -50 => 4,
                    r if r >= -60 => 3,
                    r if r >= -70 => 2,
                    r if r >= -80 => 1,
                    _ => 0,
                },
                secured: *secured,
            })
            .collect();
        self.wifi_model.set_vec(nets);
    }

    /// Keyboard display state: Rust owns the buffer — this pushes the title
    /// ("PASSWORD" / "NETWORK NAME"), the context line (SSID), the ALREADY
    /// masked + tail-windowed display text, and the eye state.
    pub fn set_kb(&self, title: &str, context: &str, text: &str, plain: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_kb_title(SharedString::from(title));
        ui.set_kb_context(SharedString::from(context));
        ui.set_kb_text(SharedString::from(text));
        ui.set_kb_plain(plain);
    }

    // === Volume + buttons (#59) ===

    /// Push the volume step (0..15) + mute to the SOUND page + the HUD overlay,
    /// plus the slider fraction (muted → 0) so the HUD knob tracks the level.
    pub fn set_volume(&self, level: u8, muted: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        let level = level.min(15);
        ui.set_volume_level(level as i32);
        ui.set_volume_muted(muted);
        ui.set_volume_frac(if muted { 0.0 } else { level as f32 / 15.0 });
    }

    /// Raise/lower the ephemeral volume HUD (Rust owns the 2s auto-dismiss).
    pub fn set_volume_overlay_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_volume_overlay_open(open);
    }

    /// Push the four button-mapping action labels to the BUTTONS page.
    pub fn set_button_actions(
        &self,
        boot_short: &str,
        boot_long: &str,
        pwron_short: &str,
        pwron_long: &str,
    ) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_boot_short_action(SharedString::from(boot_short));
        ui.set_boot_long_action(SharedString::from(boot_long));
        ui.set_pwron_short_action(SharedString::from(pwron_short));
        ui.set_pwron_long_action(SharedString::from(pwron_long));
    }

    // === App switcher (#31) ===

    /// Raise/lower the app switcher. Opening retires a running hint window —
    /// the strips must not shimmer under the scrim (launcher idiom).
    pub fn set_switcher_open(&mut self, open: bool) {
        if open {
            self.hints_cancel();
        }
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_switcher_open(open);
    }

    /// True while a shell-level modal (the app switcher or the notification
    /// shade) is up. main.rs gates AOD entry on it: dimming into AOD over a
    /// modal would be dishonest — idle with a modal up goes dark like any
    /// non-clock page.
    pub fn modal_open(&self) -> bool {
        self.modal_kind() != 0
    }

    /// Which shell-level modal is up: 0 none · 1 switcher · 2 shade. Feeds
    /// the debug-console `state modal=` field (#54 swallow evidence — modals
    /// ride app_state == Watchface, so `app` alone can't show them).
    pub fn modal_kind(&self) -> u8 {
        let Some(ui) = self.ui.as_ref() else { return 0 };
        if ui.get_switcher_open() {
            1
        } else if ui.get_shade_open() {
            2
        } else {
            0
        }
    }

    /// Suspended-session count → the watchface status-cluster chip
    /// (0 hides it). Re-pushed by main.rs after a scene recreate.
    pub fn set_suspended_count(&self, n: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_suspended_count(n);
    }

    /// Push the switcher cards: `reg_indices` are app-registry positions,
    /// most recently suspended first (only the first [`SWITCHER_CARDS`] are
    /// shown); `total` is the full suspension count (the "+N more" line).
    /// The slot→idx map is kept for the kill-swipe's start_y lookup.
    pub fn set_switcher_cards(&mut self, reg_indices: &[i32], total: usize) {
        use crate::apps::registry::REGISTRY;
        self.switcher_rows.clear();
        let mut tiles: Vec<LauncherTile> = Vec::new();
        for &i in reg_indices.iter().take(SWITCHER_CARDS) {
            let Some(d) = REGISTRY.get(i as usize) else {
                continue;
            };
            let _ = self.switcher_rows.push(i);
            tiles.push(LauncherTile {
                name: SharedString::from(d.name),
                accent: color_from_rgb(d.accent),
                icon_id: d.icon_id as i32,
                idx: i,
                present: true,
            });
        }
        self.switcher_model.set_vec(tiles);
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_switcher_count(total as i32);
    }

    // === Notification shade (#32) ===

    /// Raise/lower the notification shade. Opening retires a running hint
    /// window (launcher idiom).
    pub fn set_shade_open(&mut self, open: bool) {
        if open {
            self.hints_cancel();
        }
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_shade_open(open);
    }

    /// True while the shade is up — the arrival drain routes a fresh
    /// notification straight into the open card list instead of toasting.
    pub fn shade_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.get_shade_open())
    }

    /// Unread count → the watchface status-cluster chip (0 hides it).
    /// Re-pushed by main.rs after a scene recreate.
    pub fn set_notif_unread(&self, n: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_notif_unread(n);
    }

    /// Push the shade cards from a ring snapshot (newest first; the first
    /// [`SHADE_CARDS`] are shown, the total drives the "+N" line). Ages are
    /// formatted here — UI-layer derivation, like set_hunt's — from the same
    /// wall clock that stamped the entries.
    pub fn set_shade_cards(&self, items: &[crate::notify::Notification]) {
        let cards: Vec<NotifCard> = items
            .iter()
            .take(SHADE_CARDS)
            .map(|n| NotifCard {
                source: n.source as i32,
                title: SharedString::from(n.title.as_str()),
                body: SharedString::from(n.body.as_str()),
                age: SharedString::from(crate::notify::age_str(n.day, n.sod).as_str()),
                present: true,
            })
            .collect();
        self.shade_model.set_vec(cards);
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_notif_total(items.len() as i32);
    }

    /// SoundLevel meter (#28): current dBFS + peak-hold, both in [-60, 0].
    pub fn set_mic_level(&self, dbfs: f32, peak: f32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_mic_dbfs(dbfs);
        ui.set_mic_peak(peak);
    }

    /// Sound-app mic gain readout (digital boost, dB). Set on boot + each step.
    pub fn set_mic_gain_db(&self, db: i32) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_mic_gain_db(db);
    }

    /// SoundLevel spectrum (#30): 12 per-band values (bar dBFS + peak-hold dBFS,
    /// both in [-60, 0], low band first). Swaps the model contents in place (no
    /// per-frame ModelRc alloc). Only meaningful while the Sound overlay is open.
    pub fn set_spectrum(&self, bars: &[f32], peaks: &[f32]) {
        if self.ui.is_none() {
            return;
        }
        let bands: Vec<SpecBand> = bars
            .iter()
            .zip(peaks.iter())
            .map(|(&level, &peak)| SpecBand { level, peak })
            .collect();
        self.spectrum_model.set_vec(bands);
    }

    pub fn page(&self) -> i32 {
        // While suspended, report the page we'll restore on resume.
        self.ui.as_ref().map_or(self.saved_page, |ui| ui.get_current_page())
    }

    /// Jump to a page (boot default_page, CFG `S` remote page-switch). Out-of-range
    /// values fall back to the clock so a bad downlink can't blank the shell. While
    /// the scene is suspended the target is stashed and applied on resume.
    pub fn set_page(&mut self, page: i32) {
        let p = if (0..PAGE_COUNT).contains(&page) {
            page
        } else {
            PAGE_CLOCK
        };
        self.saved_page = p;
        if let Some(ui) = self.ui.as_ref() {
            ui.set_current_page(p);
        }
    }

    /// Push the mesh roster. `age_ms` on a [`PeerView`] is already an age
    /// (ms since we last heard the peer — see `SmolMesh::peers`), so it is
    /// divided directly; no wall-clock parameter is needed.
    pub fn set_mesh_rows(&self, our_id: u8, rows: &[PeerView]) {
        // Mesh page refreshes at 1s; skip the row-string allocs when the
        // page isn't showing rather than relying on caller discipline.
        let Some(ui) = self.ui.as_ref() else { return; };
        if ui.get_current_page() != PAGE_MESH {
            return;
        }
        // The self banner is static per boot (node id never changes); the
        // property defaults to "", so format it on the first on-page push
        // only instead of re-allocating it every 1s refresh.
        if ui.get_mesh_self_text().is_empty() {
            let (adj, noun) = names::name_for_id(our_id);
            ui.set_mesh_self_text(slint::format!("#{:03} {} {}", our_id, adj, noun));
        }
        let model: Vec<PeerRow> = rows
            .iter()
            .take(crate::net::smol_mesh::MESH_MAX_ROWS)
            .map(|p| {
                let name = match p.id {
                    Some(id) => {
                        let (adj, noun) = names::name_for_id(id);
                        slint::format!("#{:03} {} {}", id, adj, noun)
                    }
                    None => slint::format!(
                        "{:02x}:{:02x}:{:02x}", p.mac[3], p.mac[4], p.mac[5]
                    ),
                };
                PeerRow {
                    name,
                    rssi: match p.rssi_dbm {
                        Some(r) => slint::format!("{} dBm", r),
                        None => SharedString::new(),
                    },
                    age: slint::format!("{}s", p.age_ms / 1000),
                }
            })
            .collect();
        self.mesh_model.set_vec(model);
    }

    // === render ===

    pub fn has_active_animations(&self) -> bool {
        self.window.has_active_animations()
    }

    /// Force a full repaint on the next [`render`]. Needed when something painted
    /// straight to the panel (a game's framebuffer flush, or a wake from a dim
    /// screen) and clobbered the frame Slint still believes is on-screen — its
    /// dirty tracking can't see writes that bypassed the scene.
    pub fn request_redraw(&self) {
        self.window.window().request_redraw();
    }

    /// Run timers/animations and repaint if the scene is dirty. No-op while the
    /// scene is suspended (a game owns the panel via the framebuffer).
    pub fn render(&mut self, display: &mut Co5300Display) {
        // `suspended` = a game owns the panel (#66). Previously this was implied
        // by `ui.is_none()`; the scene now stays alive, so check it explicitly
        // or the shell would repaint over the game's framebuffer.
        if self.ui.is_none() || self.suspended {
            return;
        }
        // Advance the wake gesture-hint choreography on the render clock
        // (no-op unless a hint window is armed).
        self.tick_hints();
        // Same clock for the ping receiver pulse (#35): breathe + auto-dismiss.
        self.tick_ping_pulse();
        slint::platform::update_timers_and_animations();
        self.window.draw_if_needed(|renderer| {
            let mut flusher =
                TwoLineFlusher::new(display, &mut self.line_buf, &mut self.scratch);
            renderer.render_by_line(&mut flusher);
            flusher.flush_pending();
        });
    }
}


/// `m:ss` / `h:mm:ss` from milliseconds, without `core::fmt`.
///
/// Chapters run to 18 minutes today and the buffer keeps growing, so the hour
/// case is real rather than defensive.
fn mmss(ms: u32) -> heapless::String<12> {
    let mut out: heapless::String<12> = heapless::String::new();
    let total = ms / 1000;
    let (h, m, sec) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        push_dec(&mut out, h);
        let _ = out.push(':');
        push_pad2(&mut out, m);
    } else {
        push_dec(&mut out, m);
    }
    let _ = out.push(':');
    push_pad2(&mut out, sec);
    out
}

/// A number, or an em-dash when the ledger has not proposed it yet. `None` and
/// `Some(0)` must never render the same — see `set_story_character`.
fn opt_num(v: Option<u32>) -> heapless::String<12> {
    let mut out: heapless::String<12> = heapless::String::new();
    match v {
        Some(n) => push_dec(&mut out, n),
        None => {
            let _ = out.push('\u{2014}');
        }
    }
    out
}

fn push_dec<const N: usize>(s: &mut heapless::String<N>, v: u32) {
    let mut buf = [0u8; 10];
    let mut n = 0usize;
    let mut v = v;
    loop {
        if let Some(slot) = buf.get_mut(n) {
            *slot = b'0' + (v % 10) as u8;
        }
        n += 1;
        v /= 10;
        if v == 0 || n >= buf.len() {
            break;
        }
    }
    for i in (0..n).rev() {
        if let Some(&c) = buf.get(i) {
            let _ = s.push(c as char);
        }
    }
}

fn push_pad2<const N: usize>(s: &mut heapless::String<N>, v: u32) {
    if v < 10 {
        let _ = s.push('0');
    }
    push_dec(s, v);
}

/// Build a fresh WatchShell: wire the callback→request cells, bind the mesh
/// model, stamp the firmware version, and show it on the (shared) window.
/// Used by `ShellUi::new` and by `resume_scene` after a suspend, so callback
/// registration lives in one place.
fn build_scene(
    req: &Rc<ShellRequests>,
    mesh_model: &Rc<VecModel<PeerRow>>,
    climate_cards: &Rc<VecModel<ClimateCard>>,
    spectrum_model: &Rc<VecModel<SpecBand>>,
    wifi_model: &Rc<VecModel<WifiNet>>,
    switcher_model: &Rc<VecModel<LauncherTile>>,
    shade_model: &Rc<VecModel<NotifCard>>,
    story_chapters: &Rc<VecModel<StoryChapter>>,
    story_equipment: &Rc<VecModel<StorySlot>>,
    story_appearance: &Rc<VecModel<StorySlot>>,
) -> WatchShell {
    let ui = WatchShell::new().expect("failed to create WatchShell");
    {
        let r = req.clone();
        ui.on_brightness_changed(move |frac| r.brightness.set(Some(brightness_raw(frac))));
    }
    {
        let r = req.clone();
        ui.on_wifi_tap(move || r.wifi_toggle.set(true));
    }
    // #story callbacks. Same Cell-request idiom as every other overlay: the
    // callback only records intent, and the main loop acts on it where it owns
    // the amp/codec/socket borrows.
    {
        let r = req.clone();
        ui.on_story_nav(move |p| r.story_nav.set(Some(p)));
    }
    {
        let r = req.clone();
        ui.on_story_pick(move |n| r.story_pick.set(Some(n)));
    }
    {
        let r = req.clone();
        ui.on_story_stop(move || r.story_stop.set(true));
        let r = req.clone();
        ui.on_story_pause(move || r.story_pause.set(true));
        let r = req.clone();
        ui.on_story_resume(move || r.story_resume.set(true));
    }
    {
        let r = req.clone();
        ui.on_story_note(move || r.story_note.set(true));
    }
    {
        let r = req.clone();
        ui.on_story_page_prev(move || r.story_page_delta.set(Some(-1)));
    }
    {
        let r = req.clone();
        ui.on_story_page_next(move || r.story_page_delta.set(Some(1)));
    }
    {
        let r = req.clone();
        ui.on_ble_tap(move || r.ble_toggle.set(true));
    }
    {
        let r = req.clone();
        ui.on_mesh_tap(move || r.mesh_toggle.set(true));
    }
    {
        let r = req.clone();
        ui.on_cpu_tap(move || r.cpu_cycle.set(true));
    }
    {
        let r = req.clone();
        ui.on_gyro_tap(move || r.gyro_toggle.set(true));
    }
    {
        let r = req.clone();
        ui.on_reboot_tap(move || r.reboot.set(true));
    }
    {
        let r = req.clone();
        ui.on_power_shutdown_tap(move || r.power_shutdown.set(true));
    }
    {
        let r = req.clone();
        ui.on_launch_app(move |idx| {
            if let Some(app) = crate::apps::registry::launch_state(idx as usize) {
                r.launch.set(Some(app));
            }
        });

        let r = req.clone();
        ui.on_wled_emit(move |act| r.wled_action.set(Some(act)));

        let r = req.clone();
        ui.on_wled_close(move || r.wled_close.set(true));

        let r = req.clone();
        ui.on_hunt_next(move || r.hunt_next.set(true));

        let r = req.clone();
        ui.on_hunt_close(move || r.hunt_close.set(true));

        let r = req.clone();
        ui.on_energy_close(move || r.energy_close.set(true));

        let r = req.clone();
        ui.on_climate_set_temp(move |id, temp| r.climate_set_temp.set(Some((id, temp))));

        let r = req.clone();
        ui.on_climate_set_mode(move |id, mode| r.climate_set_mode.set(Some((id, mode))));

        let r = req.clone();
        ui.on_climate_closed(move || r.climate_closed.set(true));

        let r = req.clone();
        ui.on_lights_cmd(move |a| r.lights_cmd.set(Some(a)));

        let r = req.clone();
        ui.on_lights_closed(move || r.lights_closed.set(true));

        let r = req.clone();
        ui.on_ping_send(move || r.ping_send.set(true));

        let r = req.clone();
        ui.on_ping_pulse_tap(move || r.ping_pulse_tap.set(true));

        let r = req.clone();
        ui.on_voice_ptt_pressed(move || r.voice_ptt_pressed.set(true));

        let r = req.clone();
        ui.on_voice_ptt_released(move || r.voice_ptt_released.set(true));

        let r = req.clone();
        ui.on_mic_gain_up(move || r.mic_gain_up.set(true));

        let r = req.clone();
        ui.on_mic_gain_down(move || r.mic_gain_down.set(true));

        // Settings hub (v0.9.0): toggles + OTA + the WiFi scan/creds flow.
        let r = req.clone();
        ui.on_touch_sound_tap(move || r.touch_sound_toggle.set(true));

        let r = req.clone();
        ui.on_settings_ota_tap(move || r.settings_ota.set(true));

        let r = req.clone();
        ui.on_net_open(move || r.wifi_scan.set(true));

        let r = req.clone();
        ui.on_net_pick(move |i| r.wifi_pick.set(Some(i)));

        let r = req.clone();
        ui.on_net_manual(move || r.wifi_manual.set(true));

        let r = req.clone();
        ui.on_net_back(move || r.net_back.set(true));

        let r = req.clone();
        ui.on_kb_key(move |k| r.kb_key.set(Some(k)));

        let r = req.clone();
        ui.on_kb_bksp_down(move || r.kb_bksp_down.set(true));

        let r = req.clone();
        ui.on_kb_bksp_up(move || r.kb_bksp_up.set(true));

        let r = req.clone();
        ui.on_kb_eye(move || r.kb_eye.set(true));

        let r = req.clone();
        ui.on_kb_done(move || r.kb_done.set(true));

        // Buttons + volume (#59).
        let r = req.clone();
        ui.on_button_cycle(move |slot| r.button_cycle.set(Some(slot)));

        let r = req.clone();
        ui.on_volume_down(move || r.volume_down.set(true));

        let r = req.clone();
        ui.on_volume_up(move || r.volume_up.set(true));

        let r = req.clone();
        ui.on_volume_mute_tap(move || r.volume_mute.set(true));

        let r = req.clone();
        ui.on_volume_changed(move |f| r.volume_changed.set(Some(f)));

        // App switcher (#31): the status-cluster chip opens it (same cell as
        // the edge-hold — the loop builds the cards first); a card tap resumes
        // through the SAME launch cell as a launcher tile, so the suspend-
        // aware launch path is the single dispatch spot.
        let r = req.clone();
        ui.on_open_switcher(move || r.open_switcher.set(true));

        let r = req.clone();
        ui.on_switcher_resume(move |idx| {
            if let Some(app) = crate::apps::registry::launch_state(idx as usize) {
                r.launch.set(Some(app));
            }
        });

        // Notification shade (#32): the unread chip opens it (cards + badge
        // reset happen in the loop before the overlay shows); per-card X and
        // CLEAR ALL flow back as cells — the loop owns the ring.
        let r = req.clone();
        ui.on_open_shade(move || r.open_shade.set(true));

        let r = req.clone();
        ui.on_notif_dismiss(move |i| r.notif_dismiss.set(Some(i)));

        let r = req.clone();
        ui.on_notif_clear(move || r.notif_clear.set(true));
    }
    {
        // Theme picker: the tile already set Theme.scheme (instant preview); this
        // hands the chosen index to the loop for flash persistence.
        let r = req.clone();
        ui.on_theme_changed(move |n| r.theme.set(Some(n)));
    }
    ui.set_mesh_rows(ModelRc::from(mesh_model.clone()));
    ui.set_climate_cards(ModelRc::from(climate_cards.clone()));
    ui.set_mic_spectrum(ModelRc::from(spectrum_model.clone()));
    ui.set_wifi_nets(ModelRc::from(wifi_model.clone()));
    // #story: four long-lived models, swapped in place like the rest.
    ui.set_story_chapters(ModelRc::from(story_chapters.clone()));
    ui.set_story_equipment(ModelRc::from(story_equipment.clone()));
    ui.set_story_appearance(ModelRc::from(story_appearance.clone()));
    ui.set_switcher_tiles(ModelRc::from(switcher_model.clone()));
    ui.set_notif_cards(ModelRc::from(shade_model.clone()));
    // Launcher pages are built once from the app registry (single source of
    // truth) — static per boot, so plain VecModels the scene owns are enough.
    // (The old Flickable + content-height plumbing is gone with the paged
    // launcher: page geometry is fixed 3x3, nothing to measure.)
    let (launcher_tiles, launcher_titles) = build_launcher_pages();
    ui.set_launcher_page_count(launcher_titles.len().max(1) as i32);
    ui.set_launcher_titles(ModelRc::from(Rc::new(VecModel::from(launcher_titles))));
    ui.set_launcher_tiles(ModelRc::from(Rc::new(VecModel::from(launcher_tiles))));
    // Firmware version + the git hash of the image that is actually running.
    // The Cargo version ALONE was the bug: `v0.12.1` is identical in every build
    // from this crate version, so the About page could not answer "did my OTA
    // land?" and on 2026-07-29 its unchanged value was read as proof that one had
    // NOT landed. `BUILD_HASH` comes from build.rs (see `stamp_build_sigil`) and
    // carries a trailing `*` when the tree was dirty.
    ui.set_fw_text(slint::format!(
        "v{} \u{b7} {}",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_HASH")
    ));
    // The same hash as a realm-sigil `forge` name — two words a human can match
    // against what the flash/OTA tooling reported, which seven hex characters at
    // 22 px are not. Deliberately a DIFFERENT realm from the device sigil below:
    // one names a build, the other names a board, and they appear in the same
    // sentence ("eldritch-lantern is running Glowing Wright").
    ui.set_build_text(SharedString::from(env!("BUILD_SIGIL")));
    // Per-device sigil (#34): a device constant (efuse MAC), stamped here like
    // fw-text so it survives suspend/resume scene rebuilds with no stored state.
    ui.set_sigil_text(SharedString::from(crate::net::sigil::get().sigil.as_str()));
    ui.show().expect("show failed");
    ui
}

/// Slots per launcher page — a fixed 3x3 grid. MUST match the `for slot in 9`
/// grid + geometry in `ui/slint/launcher.slint`.
const LAUNCHER_PAGE_SLOTS: usize = 9;

/// Build the PAGED launcher model from the app registry — the single source of
/// truth for tile metadata. One page per section (display order: Audio, Games,
/// System; a section that ever outgrows 9 apps chunks into repeated pages with
/// the same title). Returns the page-major tile list — each page padded to
/// exactly [`LAUNCHER_PAGE_SLOTS`] entries with `present:false` defaults so the
/// Slint grid can index `tiles[page*9 + slot]` — plus one title per page. The
/// tile `idx` is the app's registry position, so `launch_app(idx)` maps back
/// through `registry::launch_state(idx)`.
fn build_launcher_pages() -> (Vec<LauncherTile>, Vec<SharedString>) {
    use crate::apps::registry::{AppDescriptor, Section, REGISTRY};
    let tile = |idx: usize, d: &AppDescriptor| LauncherTile {
        name: SharedString::from(d.name),
        accent: color_from_rgb(d.accent),
        icon_id: d.icon_id as i32,
        idx: idx as i32,
        present: true,
    };
    let mut tiles: Vec<LauncherTile> = Vec::new();
    let mut titles: Vec<SharedString> = Vec::new();
    for sec in [Section::Audio, Section::Games, Section::System] {
        let apps: Vec<(usize, &AppDescriptor)> = REGISTRY
            .iter()
            .enumerate()
            .filter(|(_, d)| d.section == sec)
            .collect();
        for chunk in apps.chunks(LAUNCHER_PAGE_SLOTS) {
            titles.push(SharedString::from(sec.label()));
            for (i, d) in chunk {
                tiles.push(tile(*i, d));
            }
            for _ in chunk.len()..LAUNCHER_PAGE_SLOTS {
                tiles.push(LauncherTile::default()); // present:false pad
            }
        }
    }
    (tiles, titles)
}

/// 0xRRGGBB -> Slint opaque color.
fn color_from_rgb(rgb: u32) -> slint::Color {
    slint::Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// True when nothing is stacked over the watchface pages. The edge-hold
/// switcher gesture (#31) only arms here — the launcher, the Settings hub,
/// every registry overlay, and the switcher itself own their gestures.
fn shell_clean(ui: &WatchShell) -> bool {
    !ui.get_launcher_open()
        && !ui.get_settings_open()
        && !ui.get_switcher_open()
        && !ui.get_shade_open()
        && !ui.get_ping_pulse_open()
        && !OVERLAYS.iter().any(|o| (o.is_open)(ui))
}

/// Map a kill-swipe's `start_y` onto a switcher card slot (fixed geometry —
/// see the SWITCHER_CARD_* constants and ui/slint/switcher.slint). `None`
/// when the swipe started in a gutter or off the card stack.
fn switcher_slot(start_y: u16) -> Option<usize> {
    let rel = start_y.checked_sub(SWITCHER_CARD_TOP)?;
    let slot = (rel / SWITCHER_CARD_PITCH) as usize;
    (rel % SWITCHER_CARD_PITCH < SWITCHER_CARD_H && slot < SWITCHER_CARDS).then_some(slot)
}

/// Map a dismiss-swipe's `start_y` onto a shade card slot (== ring index,
/// newest = 0; fixed geometry — see SHADE_CARD_* and ui/slint/shade.slint).
fn shade_slot(start_y: u16) -> Option<usize> {
    let rel = start_y.checked_sub(SHADE_CARD_TOP)?;
    let slot = (rel / SHADE_CARD_PITCH) as usize;
    (rel % SHADE_CARD_PITCH < SHADE_CARD_H && slot < SHADE_CARDS).then_some(slot)
}

/// True when a swipe starting at `start_y` grabbed the Settings hub's
/// brightness slider (DISPLAY page, hub view 0): it must be treated as a
/// slider DRAG — no page flip / back-nav, and the release stays on-window so
/// the grabbed TouchArea sees the real final position (same rationale as the
/// power page's SLIDER_BAND).
fn hub_slider_drag(ui: &WatchShell, start_y: u16) -> bool {
    ui.get_settings_open()
        && ui.get_net_view() == 0
        && ui.get_settings_page() == HUB_PAGE_DISPLAY
        && HUB_SLIDER_BAND.contains(&start_y)
}

fn weather_label(code: u8) -> &'static str {
    match code {
        0 => "CLEAR",
        1..=3 => "CLOUDS",
        45 | 48 => "FOG",
        51..=67 => "RAIN",
        71..=77 => "SNOW",
        80..=82 => "SHOWERS",
        85 | 86 => "SNOW",
        95..=99 => "STORM",
        _ => "",
    }
}
