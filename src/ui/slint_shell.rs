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
pub const LAUNCHER_APPS: [AppState; 7] = [
    AppState::Snake,
    AppState::WorldSnake,
    AppState::Game2048,
    AppState::Tetris,
    AppState::Flappy,
    AppState::Maze,
    AppState::Settings,
];

#[derive(Default)]
pub struct ShellRequests {
    pub brightness: Cell<Option<u8>>, // raw CO5300 value
    pub launch: Cell<Option<AppState>>,
    pub wifi_toggle: Cell<bool>,
    pub ble_toggle: Cell<bool>,
    pub cpu_cycle: Cell<bool>,
    pub gyro_toggle: Cell<bool>,
    pub reboot: Cell<bool>,
}

pub struct ShellUi {
    window: Rc<MinimalSoftwareWindow>,
    ui: WatchShell,
    pub req: Rc<ShellRequests>,
    /// Long-lived roster model: set_mesh_rows swaps its contents in place
    /// instead of allocating a fresh ModelRc per push.
    mesh_model: Rc<VecModel<PeerRow>>,
    line_buf: Vec<Rgb565Pixel>,
    scratch: Vec<u16>,
    touch_down: bool,
    last_pos: slint::LogicalPosition,
    last_second: u8,
}

impl ShellUi {
    /// Call exactly once per boot (registers the Slint platform).
    pub fn new() -> Self {
        let window = init_platform();
        let ui = WatchShell::new().expect("failed to create WatchShell");
        let req = Rc::new(ShellRequests::default());

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
        }

        let mesh_model: Rc<VecModel<PeerRow>> = Rc::new(VecModel::default());
        ui.set_mesh_rows(ModelRc::from(mesh_model.clone()));

        // Firmware version is a compile-time constant; set it once so the
        // system page shows the real Cargo version (single source of truth)
        // instead of a hardcoded string that drifts.
        ui.set_fw_text(slint::format!("v{}", env!("CARGO_PKG_VERSION")));

        ui.show().expect("show failed");

        Self {
            window,
            ui,
            req,
            mesh_model,
            line_buf: alloc::vec![Rgb565Pixel(0); WIDTH * 2],
            scratch: alloc::vec![0u16; WIDTH * 2],
            touch_down: false,
            last_pos: slint::LogicalPosition::new(0.0, 0.0),
            last_second: 0xFF,
        }
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
            let slider_drag = !self.ui.get_launcher_open()
                && self.ui.get_current_page() == PAGE_POWER
                && SLIDER_BAND.contains(&swipe_start_y);
            // Vertical swipes while the launcher is open belong to the Flickable's
            // own scroll/fling; releasing off-screen would kill its momentum. Keep
            // the natural release — Flickable's drag-capture suppresses stray item
            // clicks on a real scroll. (slider_drag already excludes launcher-open,
            // so the two are mutually exclusive.)
            let launcher_scroll = self.ui.get_launcher_open()
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
            // Launcher overlay first: it swallows nav swipes wherever they
            // start (including the power page's slider band); Right closes.
            if self.ui.get_launcher_open() {
                if direction == SwipeDirection::Right {
                    self.ui.set_launcher_open(false);
                }
                return;
            }
            // Horizontal swipes starting on the power page's brightness
            // slider are slider drags, not page switches.
            let on_slider =
                self.ui.get_current_page() == PAGE_POWER && SLIDER_BAND.contains(&swipe_start_y);
            if on_slider {
                return;
            }
            match direction {
                SwipeDirection::Left => {
                    self.ui.set_current_page((self.ui.get_current_page() + 1).rem_euclid(PAGE_COUNT))
                }
                SwipeDirection::Right => {
                    self.ui.set_current_page(
                        (self.ui.get_current_page() + PAGE_COUNT - 1).rem_euclid(PAGE_COUNT),
                    )
                }
                SwipeDirection::Up if self.ui.get_current_page() == PAGE_CLOCK => {
                    self.ui.set_launcher_open(true)
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
        self.ui.set_time_text(slint::format!("{:02}:{:02}", dt.hours, dt.minutes));
        self.ui.set_seconds_text(slint::format!("{:02}", dt.seconds));
        let weekday = WEEKDAYS[(dt.weekday % 7) as usize];
        let month = MONTHS[(dt.month.clamp(1, 12) - 1) as usize];
        self.ui.set_date_text(slint::format!(
            "{} {:02} {} 20{:02}", weekday, dt.day, month, dt.year
        ));
        self.ui.set_minute_progress(dt.seconds as f32 / 59.0);
        true
    }

    pub fn set_battery(&self, pct: u8, mv: u16, charging: bool) {
        self.ui.set_battery_percent(pct.min(100) as i32);
        self.ui.set_charging(charging);
        let _ = mv; // chrome shows percent; power page (task 6) consumes mv
    }

    pub fn set_radios(&self, wifi: bool, ble: bool, mesh_peers: u8) {
        self.ui.set_wifi_on(wifi);
        self.ui.set_ble_on(ble);
        self.ui.set_mesh_peers(mesh_peers as i32);
    }

    pub fn set_steps(&self, steps: u32) {
        self.ui.set_steps(steps as i32);
    }

    pub fn set_cpu_mhz(&self, mhz: u16) {
        self.ui.set_cpu_text(slint::format!("{} MHz", mhz));
    }

    pub fn set_gyro(&self, on: bool) {
        self.ui.set_gyro_on(on);
    }

    pub fn set_sensors(&self, accel: (f32, f32, f32), gyro: (i16, i16, i16), temp_dc: i16) {
        // Sensors update at 100ms; skip the 3 SharedString allocs when the page
        // isn't showing rather than relying on caller discipline.
        if self.ui.get_current_page() != PAGE_SENSORS { return; }
        self.ui.set_accel_text(slint::format!(
            "{:+.2} {:+.2} {:+.2} g", accel.0, accel.1, accel.2
        ));
        self.ui.set_gyro_text(slint::format!(
            "{:+.1} {:+.1} {:+.1} dps", gyro.0 as f32 / 10.0, gyro.1 as f32 / 10.0,
            gyro.2 as f32 / 10.0
        ));
        self.ui.set_imu_temp_text(slint::format!("{:.1} C", temp_dc as f32 / 10.0));
    }

    pub fn set_system(&self, heap_free: usize, batt_pct: u8, batt_mv: u16) {
        // System page refreshes at 2s; skip the SharedString allocs when the
        // page isn't showing rather than relying on caller discipline.
        if self.ui.get_current_page() != PAGE_SYSTEM {
            return;
        }
        self.ui.set_heap_text(slint::format!("{}k free", heap_free / 1024));
        let s = embassy_time::Instant::now().as_secs();
        self.ui.set_uptime_text(slint::format!(
            "{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60
        ));
        self.ui.set_battery_text(slint::format!("{}% \u{00b7} {} mV", batt_pct, batt_mv));
    }

    pub fn set_power(&self, stats: &crate::peripherals::power_stats::PowerStats) {
        // Power page refreshes at 1s; skip the alloc churn when not showing.
        if self.ui.get_current_page() != PAGE_POWER {
            return;
        }
        use crate::peripherals::power_stats::{on_off, BATTERY_CAPACITY_MAH};
        // Per-subsystem cells mirror the old "POWER MONITOR" read-out; all
        // labels and mA come from PowerStats (single source of truth, shared
        // with the legacy eg renderer until task 13 deletes it). SDCARD is
        // omitted: the C6 board has no SD slot and main.rs never sets sd_on.
        // cpu-text (clock chip) is untouched — the CPU cell has its own MHz.
        self.ui.set_cpu_cell(slint::format!(
            "{}MHz \u{00b7} {}mA", stats.cpu_mhz, stats.base_ma()
        ));
        self.ui.set_display_cell(slint::format!(
            "{} \u{00b7} {}mA", stats.display_label(), stats.display_ma()
        ));
        self.ui.set_wifi_cell(slint::format!(
            "{} \u{00b7} {}mA", stats.wifi_label(), stats.wifi_ma()
        ));
        self.ui.set_ble_cell(slint::format!(
            "{} \u{00b7} {}mA", on_off(stats.ble_on), stats.ble_ma()
        ));
        self.ui.set_imu_cell(slint::format!(
            "{} \u{00b7} {}mA", on_off(stats.imu_on), stats.imu_ma()
        ));
        self.ui.set_audio_cell(slint::format!(
            "{} \u{00b7} {}mA", on_off(stats.audio_on), stats.audio_ma()
        ));
        self.ui.set_total_ma(stats.total_ma() as i32);
        let full = stats.full_runtime_hours(BATTERY_CAPACITY_MAH);
        let left = stats.estimated_hours(BATTERY_CAPACITY_MAH);
        self.ui.set_left_hours(left as i32);
        let full_s: SharedString =
            if full >= 999 { "--".into() } else { slint::format!("{}h", full) };
        let left_s: SharedString =
            if left >= 999 { "--".into() } else { slint::format!("~{}h", left) };
        self.ui
            .set_runtime_text(slint::format!("100%: {} \u{00b7} left: {}", full_s, left_s));
    }

    pub fn set_weather(&self, temp_f: Option<i16>, code: u8) {
        match temp_f {
            Some(t) => self
                .ui
                .set_weather_text(slint::format!("{}\u{00b0}F {}", t, weather_label(code))),
            None => self.ui.set_weather_text(SharedString::new()),
        }
    }

    pub fn set_brightness_from_raw(&self, raw: u8) {
        self.ui
            .set_brightness((raw.saturating_sub(BRIGHTNESS_MIN)) as f32
                / (0xFF - BRIGHTNESS_MIN) as f32);
    }

    pub fn set_aod(&self, on: bool) {
        self.ui.set_aod(on);
    }

    /// Push the Mesh Familiar snapshot to the clock nook (task 12).
    pub fn set_fam(&self, f: &crate::net::familiar::FamUi) {
        self.ui.set_fam_known(f.known);
        self.ui.set_fam_holding(f.holding);
        self.ui.set_fam_mood(f.mood as i32);
        self.ui.set_fam_hunger(f.hunger as i32);
        self.ui.set_fam_stage(f.stage as i32);
    }

    /// Feed scaled accel into the clock's parallax offsets, clamped to ±12px so
    /// the time/date never collide with the chrome. Fed only on the clock page
    /// with the gyro toy enabled. `par-x`/`par-y` are `length` in .slint; the
    /// generated setters take logical pixels as f32.
    pub fn set_parallax(&self, ax: f32, ay: f32) {
        self.ui.set_par_x((ax * 12.0).clamp(-12.0, 12.0));
        self.ui.set_par_y((ay * 12.0).clamp(-12.0, 12.0));
    }

    pub fn set_toast(&self, text: &str) {
        self.ui.set_toast_text(SharedString::from(text));
    }

    pub fn set_launcher_open(&self, open: bool) {
        self.ui.set_launcher_open(open);
    }

    pub fn launcher_open(&self) -> bool {
        self.ui.get_launcher_open()
    }

    pub fn page(&self) -> i32 {
        self.ui.get_current_page()
    }

    /// Jump to a page (boot default_page, CFG `S` remote page-switch). Out-of-range
    /// values fall back to the clock so a bad downlink can't blank the shell.
    pub fn set_page(&self, page: i32) {
        let p = if (0..PAGE_COUNT).contains(&page) {
            page
        } else {
            PAGE_CLOCK
        };
        self.ui.set_current_page(p);
    }

    /// Push the mesh roster. `age_ms` on a [`PeerView`] is already an age
    /// (ms since we last heard the peer — see `SmolMesh::peers`), so it is
    /// divided directly; no wall-clock parameter is needed.
    pub fn set_mesh_rows(&self, our_id: u8, rows: &[PeerView]) {
        // Mesh page refreshes at 1s; skip the row-string allocs when the
        // page isn't showing rather than relying on caller discipline.
        if self.ui.get_current_page() != PAGE_MESH {
            return;
        }
        // The self banner is static per boot (node id never changes); the
        // property defaults to "", so format it on the first on-page push
        // only instead of re-allocating it every 1s refresh.
        if self.ui.get_mesh_self_text().is_empty() {
            let (adj, noun) = names::name_for_id(our_id);
            self.ui
                .set_mesh_self_text(slint::format!("#{:03} {} {}", our_id, adj, noun));
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

    /// Run timers/animations and repaint if the scene is dirty.
    pub fn render(&mut self, display: &mut Co5300Display) {
        slint::platform::update_timers_and_animations();
        self.window.draw_if_needed(|renderer| {
            let mut flusher =
                TwoLineFlusher::new(display, &mut self.line_buf, &mut self.scratch);
            renderer.render_by_line(&mut flusher);
            flusher.flush_pending();
        });
    }
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
