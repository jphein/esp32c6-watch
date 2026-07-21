// Rust-side wrapper around the WatchShell Slint component: owns the window,
// the render strip, and the callback→loop request cells. main.rs talks to
// this module only; no Slint types cross its boundary except in render().

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;

use slint::platform::software_renderer::{MinimalSoftwareWindow, Rgb565Pixel};
use slint::platform::{PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ModelRc, SharedString, VecModel};

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

/// Launcher item order — MUST match the `for` list in ui/slint/launcher.slint
/// (which lands in plan task 8).
pub const LAUNCHER_APPS: [AppState; 12] = [
    AppState::Snake,
    AppState::WorldSnake,
    AppState::Game2048,
    AppState::Tetris,
    AppState::Flappy,
    AppState::Maze,
    AppState::Settings,
    AppState::Wled,   // idx 7 — SYSTEM-section WLED tile (icon-id 9)
    AppState::Hunt,   // idx 8 — GAMES-section HUNT tile (icon-id 10)
    AppState::Energy,  // idx 9 — SYSTEM-section ENERGY tile (icon-id 11)
    AppState::Climate, // idx 10 — SYSTEM-section CLIMATE tile (icon-id 12)
    AppState::Voice,   // idx 11 — SYSTEM-section VOICE tile (icon-id 7)
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
    /// Voice PTT (#42): `pressed` is the finger-down that starts capture (drained
    /// by the loop when app_state == Voice). `released` is advisory — the loop's
    /// release watcher keys off the physical touch INT pin, since the Slint
    /// release callback can't fire while the loop is parked streaming the hold.
    pub voice_ptt_pressed: Cell<bool>,
    pub voice_ptt_released: Cell<bool>,
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
    pub req: Rc<ShellRequests>,
    /// Long-lived roster model: set_mesh_rows swaps its contents in place
    /// instead of allocating a fresh ModelRc per push.
    mesh_model: Rc<VecModel<PeerRow>>,
    /// Climate (#58) card model: one ClimateCard per HA climate entity, swapped
    /// in place by set_climate (same long-lived pattern as mesh_model).
    climate_cards: Rc<VecModel<ClimateCard>>,
    line_buf: Vec<Rgb565Pixel>,
    scratch: Vec<u16>,
    touch_down: bool,
    last_pos: slint::LogicalPosition,
    last_second: u8,
    /// Current page, preserved across a suspend so the recreated scene returns
    /// to where the user was rather than snapping back to the clock.
    saved_page: i32,
}

impl ShellUi {
    /// Call exactly once per boot (registers the Slint platform).
    pub fn new() -> Self {
        let window = init_platform();
        let req = Rc::new(ShellRequests::default());
        let mesh_model: Rc<VecModel<PeerRow>> = Rc::new(VecModel::default());
        let climate_cards: Rc<VecModel<ClimateCard>> = Rc::new(VecModel::default());
        let ui = build_scene(&req, &mesh_model, &climate_cards);

        Self {
            window,
            ui: Some(ui),
            req,
            mesh_model,
            climate_cards,
            line_buf: alloc::vec![Rgb565Pixel(0); WIDTH * 2],
            scratch: alloc::vec![0u16; WIDTH * 2],
            touch_down: false,
            last_pos: slint::LogicalPosition::new(0.0, 0.0),
            last_second: 0xFF,
            saved_page: PAGE_CLOCK,
        }
    }

    /// Drop the Slint scene to free ~30-40KB of heap so a game's ~201KB
    /// framebuffer fits (the two can't coexist in the C6's SRAM). The window +
    /// platform are set-once globals and survive; the current page is saved for
    /// the recreate. Idempotent — safe to call when already suspended.
    pub fn suspend_scene(&mut self) {
        if let Some(ui) = self.ui.as_ref() {
            self.saved_page = ui.get_current_page();
            let _ = ui.hide();
        }
        self.ui = None;
    }

    /// Recreate the scene after a game exits: fresh component, callbacks
    /// re-registered, mesh model re-bound, page restored. The caller re-pushes
    /// live data (battery/time/radios/fam/page-data) after this. Idempotent.
    pub fn resume_scene(&mut self) {
        if self.ui.is_some() {
            return;
        }
        let ui = build_scene(&self.req, &self.mesh_model, &self.climate_cards);
        ui.set_current_page(self.saved_page);
        self.ui = Some(ui);
        // Fresh scene = time_text is back at its "--:--" default; clear the
        // 1Hz gate so the caller's next set_time repaints the clock even if the
        // second hasn't ticked since the game launched.
        self.last_second = 0xFF;
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
        // No scene to route touches to while a game holds the framebuffer.
        let Some(ui) = self.ui.as_ref() else {
            return;
        };
        if let Some(tp) = point {
            let pos = slint::LogicalPosition::new(tp.x as f32, tp.y as f32);
            let event = if self.touch_down {
                WindowEvent::PointerMoved { position: pos }
            } else {
                WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Left }
            };
            self.touch_down = true;
            self.last_pos = pos;
            let _ = self.window.window().try_dispatch_event(event);
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
            let slider_drag = !ui.get_launcher_open()
                && ui.get_current_page() == PAGE_POWER
                && SLIDER_BAND.contains(&swipe_start_y);
            // Vertical swipes while the launcher is open belong to the Flickable's
            // own scroll/fling; releasing off-screen would kill its momentum. Keep
            // the natural release — Flickable's drag-capture suppresses stray item
            // clicks on a real scroll. (slider_drag already excludes launcher-open,
            // so the two are mutually exclusive.)
            let launcher_scroll = ui.get_launcher_open()
                && matches!(swipe, Some(SwipeDirection::Up) | Some(SwipeDirection::Down));
            let directional = matches!(swipe, Some(d) if d != SwipeDirection::Tap)
                && !slider_drag
                && !launcher_scroll;
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

        if let Some(direction) = swipe {
            // WLED overlay first: it's full-screen over the scene, so swallow all
            // nav swipes (no paging behind it); Right closes, mirroring the
            // launcher. Taps still reach the tiles via the pointer events above.
            if ui.get_wled_open() {
                if direction == SwipeDirection::Right {
                    ui.set_wled_open(false);
                }
                return;
            }
            // Hunt overlay: same contract — swallow nav swipes, Right closes.
            if ui.get_hunt_open() {
                if direction == SwipeDirection::Right {
                    ui.set_hunt_open(false);
                }
                return;
            }
            // Energy overlay: swallow nav swipes; Right routes through the close
            // CELL (not a direct set_energy_open) so main.rs clears energy_active +
            // releases the WiFi hold. A direct set would strand WiFi (luna-uifix's
            // finding on the oracle-t10 finding-b bug — the cell drain never fires).
            if ui.get_energy_open() {
                if direction == SwipeDirection::Right {
                    self.req.energy_close.set(true);
                }
                return;
            }
            // Climate overlay: swallow nav swipes; Right fires the close cell (a
            // swipe never reaches the chevron TouchArea, so a bare return would
            // strand WiFi). main.rs clears climate_active + releases the hold.
            if ui.get_climate_open() {
                if direction == SwipeDirection::Right {
                    self.req.climate_closed.set(true);
                }
                return;
            }
            // Voice overlay: same contract — swallow nav swipes, Right closes.
            // (No back-chevron; Right-swipe is the only close. A swipe only lands
            // here when the button isn't held — the loop is parked during a hold.)
            if ui.get_voice_open() {
                if direction == SwipeDirection::Right {
                    ui.set_voice_open(false);
                }
                return;
            }
            // Launcher overlay next: it swallows nav swipes wherever they
            // start (including the power page's slider band); Right closes.
            if ui.get_launcher_open() {
                if direction == SwipeDirection::Right {
                    ui.set_launcher_open(false);
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
            match direction {
                SwipeDirection::Left => {
                    ui.set_current_page((ui.get_current_page() + 1).rem_euclid(PAGE_COUNT))
                }
                SwipeDirection::Right => {
                    ui.set_current_page(
                        (ui.get_current_page() + PAGE_COUNT - 1).rem_euclid(PAGE_COUNT),
                    )
                }
                SwipeDirection::Up if ui.get_current_page() == PAGE_CLOCK => {
                    ui.set_launcher_open(true)
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

    pub fn launcher_open(&self) -> bool {
        // No launcher while a game holds the framebuffer (scene suspended).
        self.ui.as_ref().is_some_and(|ui| ui.get_launcher_open())
    }

    pub fn set_wled_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_wled_open(open);
    }

    pub fn wled_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.get_wled_open())
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

    pub fn hunt_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.get_hunt_open())
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

    pub fn energy_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.get_energy_open())
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

    pub fn climate_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.get_climate_open())
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

    pub fn set_voice_open(&self, open: bool) {
        let Some(ui) = self.ui.as_ref() else { return; };
        ui.set_voice_open(open);
    }

    pub fn voice_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.get_voice_open())
    }

    /// Voice UI state: 0 idle · 1 listening · 2 sending · 3 result · 4 error.
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
        if self.ui.is_none() {
            return;
        }
        slint::platform::update_timers_and_animations();
        self.window.draw_if_needed(|renderer| {
            let mut flusher =
                TwoLineFlusher::new(display, &mut self.line_buf, &mut self.scratch);
            renderer.render_by_line(&mut flusher);
            flusher.flush_pending();
        });
    }
}

/// Build a fresh WatchShell: wire the callback→request cells, bind the mesh
/// model, stamp the firmware version, and show it on the (shared) window.
/// Used by `ShellUi::new` and by `resume_scene` after a suspend, so callback
/// registration lives in one place.
fn build_scene(
    req: &Rc<ShellRequests>,
    mesh_model: &Rc<VecModel<PeerRow>>,
    climate_cards: &Rc<VecModel<ClimateCard>>,
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
        ui.on_launch_app(move |idx| {
            if let Some(app) = LAUNCHER_APPS.get(idx as usize) {
                r.launch.set(Some(*app));
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
        ui.on_voice_ptt_pressed(move || r.voice_ptt_pressed.set(true));

        let r = req.clone();
        ui.on_voice_ptt_released(move || r.voice_ptt_released.set(true));
    }
    ui.set_mesh_rows(ModelRc::from(mesh_model.clone()));
    ui.set_climate_cards(ModelRc::from(climate_cards.clone()));
    // Firmware version is a compile-time constant; set it once so the system
    // page shows the real Cargo version instead of a string that drifts.
    ui.set_fw_text(slint::format!("v{}", env!("CARGO_PKG_VERSION")));
    ui.show().expect("show failed");
    ui
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
