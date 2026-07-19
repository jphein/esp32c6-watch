// Watchface - renders to any DrawTarget (framebuffer or display)

use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Text};

use crate::board;
use crate::ui::segments;

const SCREEN_CX: i32 = board::LCD_WIDTH as i32 / 2;
const TIME_Y: i32 = 60;
const TIME_DW: i32 = 36;
const TIME_DH: i32 = 64;
const TIME_GAP: i32 = 6;
const TIME_CW: i32 = 14;
const TIME_TOTAL_W: i32 = 6 * TIME_DW + 2 * TIME_CW + 7 * TIME_GAP;
const TIME_PAD: i32 = 4;
const BATTERY_Y: i32 = 175;
const BATTERY_PAD_Y: i32 = 4;
const BATTERY_REGION_W: i32 = 240;
const BATTERY_REGION_H: i32 = 92;
const GYRO_CX: i32 = 205;
const GYRO_CY: i32 = 370;
const GYRO_R: i32 = 50;
const BALL_R: i32 = 8;
const GYRO_FLUSH_PAD: i32 = 2;

// BLE toggle switch geometry (above WiFi)
const BLE_TOGGLE_X: i32 = 50;
const BLE_TOGGLE_Y: i32 = 245;
const BLE_TOGGLE_W: i32 = 56;
const BLE_TOGGLE_H: i32 = 28;

// WiFi toggle switch geometry (iOS-style pill)
const WIFI_TOGGLE_X: i32 = 50;
const WIFI_TOGGLE_Y: i32 = 290;
const WIFI_TOGGLE_W: i32 = 56;
const WIFI_TOGGLE_H: i32 = 28;
const WIFI_KNOB_R: i32 = 10;

// Brightness slider geometry
const BRI_SLIDER_X: i32 = 160;
const BRI_SLIDER_Y: i32 = 278;
const BRI_SLIDER_W: i32 = 180;
const BRI_SLIDER_H: i32 = 22;

// CPU freq button geometry (below WiFi toggle)
const CPU_BTN_X: i32 = 42;
const CPU_BTN_Y: i32 = 327;
const CPU_BTN_W: i32 = 72;
const CPU_BTN_H: i32 = 28;

// Apps button geometry (bottom center, replaces "100% Rust" footer)
const APPS_BTN_X: i32 = 130;
const APPS_BTN_Y: i32 = 450;
const APPS_BTN_W: i32 = 140;
const APPS_BTN_H: i32 = 32;

#[derive(Clone, Copy, Debug)]
pub struct FlushRegion {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl FlushRegion {
    const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            x: x as u16,
            y: y as u16,
            w: w as u16,
            h: h as u16,
        }
    }

    fn union(self, other: Self) -> Self {
        let x1 = (self.x as i32).min(other.x as i32);
        let y1 = (self.y as i32).min(other.y as i32);
        let x2 = (self.x as i32 + self.w as i32).max(other.x as i32 + other.w as i32);
        let y2 = (self.y as i32 + self.h as i32).max(other.y as i32 + other.h as i32);
        Self::new(x1, y1, x2 - x1, y2 - y1)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderOutcome {
    pub full_redraw: bool,
    pub time_region: Option<FlushRegion>,
    pub battery_region: Option<FlushRegion>,
    pub gyro_region: Option<FlushRegion>,
}

pub struct WatchFace {
    hours: u8, minutes: u8, seconds: u8,
    battery_percent: u8, battery_voltage: u16, is_charging: bool,
    accel_x: i16, accel_y: i16, accel_z: i16,
    prev_ball_x: i32, prev_ball_y: i32,
    day: u8, month: u8, year: u8,
    full_redraw: bool, time_changed: bool, battery_changed: bool, gyro_changed: bool,
    pub wifi_connected: bool,
    pub ble_on: bool,
    pub gyro_enabled: bool,
    /// Display brightness 0..255, controlled by the slider on the watchface.
    pub brightness: u8,
    /// CPU frequency in MHz. Cycles through 80/160/240 on tap.
    /// Only takes effect on next reboot (esp-hal doesn't expose runtime DVFS).
    pub cpu_mhz: u16,
}

impl WatchFace {
    pub fn new() -> Self {
        Self {
            hours: 0, minutes: 0, seconds: 0,
            battery_percent: 0, battery_voltage: 0, is_charging: false,
            accel_x: 0, accel_y: 0, accel_z: 0,
            prev_ball_x: GYRO_CX, prev_ball_y: GYRO_CY,
            day: 6, month: 4, year: 26,
            full_redraw: true, time_changed: false, battery_changed: false, gyro_changed: false,
            wifi_connected: false,
            ble_on: false,
            gyro_enabled: false, // off by default to save battery
            brightness: 0xA0,   // default ~63%
            cpu_mhz: 160,
        }
    }

    pub fn update_time(&mut self, h: u8, m: u8, s: u8) {
        if self.hours != h || self.minutes != m || self.seconds != s {
            self.hours = h; self.minutes = m; self.seconds = s;
            self.time_changed = true;
        }
    }

    pub fn update_date(&mut self, day: u8, month: u8, year: u8) {
        self.day = day; self.month = month; self.year = year;
    }

    pub fn update_battery(&mut self, pct: u8, mv: u16, chg: bool) {
        if self.battery_percent != pct || self.battery_voltage != mv || self.is_charging != chg {
            self.battery_percent = pct;
            self.battery_voltage = mv;
            self.is_charging = chg;
            self.battery_changed = true;
        }
    }

    pub fn update_accel(&mut self, x: f32, y: f32, z: f32) {
        self.accel_x = (x * 100.0) as i16;
        self.accel_y = (y * 100.0) as i16;
        self.accel_z = (z * 100.0) as i16;
        let (nx, ny) = Self::projected_ball_position(self.accel_x, self.accel_y);
        if (nx - self.prev_ball_x).unsigned_abs() >= 2 || (ny - self.prev_ball_y).unsigned_abs() >= 2 {
            self.gyro_changed = true;
        }
    }

    pub fn force_redraw(&mut self) { self.full_redraw = true; }

    /// Toggle gyroscope display. Returns new state.
    pub fn toggle_gyro(&mut self) -> bool {
        self.gyro_enabled = !self.gyro_enabled;
        self.full_redraw = true;
        self.gyro_enabled
    }

    /// Check if tap is in gyro zone
    pub fn is_gyro_zone(y: u16) -> bool {
        y as i32 >= GYRO_CY - GYRO_R - 20 && (y as i32) <= GYRO_CY + GYRO_R + 20
    }

    /// Hit-test for the WiFi toggle switch.
    pub fn is_wifi_zone(x: u16, y: u16) -> bool {
        let xi = x as i32;
        let yi = y as i32;
        xi >= WIFI_TOGGLE_X - 10
            && xi <= WIFI_TOGGLE_X + WIFI_TOGGLE_W + 10
            && yi >= WIFI_TOGGLE_Y - 10
            && yi <= WIFI_TOGGLE_Y + WIFI_TOGGLE_H + 10
    }

    /// Hit-test for the brightness slider. Returns Some(brightness 0..255)
    /// based on horizontal position, or None if tap is outside.
    pub fn brightness_from_tap(x: u16, y: u16) -> Option<u8> {
        let xi = x as i32;
        let yi = y as i32;
        if yi >= BRI_SLIDER_Y - 12
            && yi <= BRI_SLIDER_Y + BRI_SLIDER_H + 12
            && xi >= BRI_SLIDER_X - 10
            && xi <= BRI_SLIDER_X + BRI_SLIDER_W + 10
        {
            let clamped = (xi - BRI_SLIDER_X).clamp(0, BRI_SLIDER_W) as u32;
            // Map 0..BRI_SLIDER_W → 0x10..0xFF (never fully off via slider)
            let val = 0x10 + (clamped * (0xFF - 0x10) as u32 / BRI_SLIDER_W as u32);
            Some(val as u8)
        } else {
            None
        }
    }

    /// Draw a WiFi icon (3 concentric arcs + dot) using rectangles.
    /// The icon is ~16x14 pixels, top-left at (x, y).
    fn draw_wifi_icon<D: DrawTarget<Color = Rgb565>>(
        d: &mut D,
        x: i32,
        y: i32,
        color: Rgb565,
    ) -> Result<(), D::Error> {
        let px = |dx: i32, dy: i32, w: u32, h: u32| {
            Rectangle::new(Point::new(x + dx, y + dy), Size::new(w, h))
                .into_styled(PrimitiveStyle::with_fill(color))
        };
        // Dot (center bottom)
        px(7, 12, 2, 2).draw(d)?;
        // Arc 1 (smallest)
        px(5, 9, 6, 1).draw(d)?;
        px(4, 8, 1, 1).draw(d)?;
        px(11, 8, 1, 1).draw(d)?;
        // Arc 2 (middle)
        px(3, 5, 10, 1).draw(d)?;
        px(2, 4, 1, 1).draw(d)?;
        px(13, 4, 1, 1).draw(d)?;
        // Arc 3 (largest)
        px(1, 1, 14, 1).draw(d)?;
        px(0, 0, 1, 1).draw(d)?;
        px(15, 0, 1, 1).draw(d)?;
        Ok(())
    }

    /// Draw a Bluetooth rune icon (~10x16 pixels) at (x, y).
    fn draw_ble_icon<D: DrawTarget<Color = Rgb565>>(
        d: &mut D,
        x: i32,
        y: i32,
        color: Rgb565,
    ) -> Result<(), D::Error> {
        let px = |dx: i32, dy: i32, w: u32, h: u32| {
            Rectangle::new(Point::new(x + dx, y + dy), Size::new(w, h))
                .into_styled(PrimitiveStyle::with_fill(color))
        };
        // Vertical line (center)
        px(5, 0, 1, 16).draw(d)?;
        // Top arrow (pointing right-up): line going from center-top to right
        px(6, 1, 1, 1).draw(d)?;
        px(7, 2, 1, 1).draw(d)?;
        px(8, 3, 1, 1).draw(d)?;
        // Arrow comes back to center at ~y+5
        px(7, 4, 1, 1).draw(d)?;
        px(6, 5, 1, 1).draw(d)?;
        // Cross line top-left to mid-right
        px(1, 4, 1, 1).draw(d)?;
        px(2, 5, 1, 1).draw(d)?;
        px(3, 6, 1, 1).draw(d)?;
        px(4, 7, 1, 1).draw(d)?;
        // Cross line bottom-left to mid-right
        px(4, 8, 1, 1).draw(d)?;
        px(3, 9, 1, 1).draw(d)?;
        px(2, 10, 1, 1).draw(d)?;
        px(1, 11, 1, 1).draw(d)?;
        // Bottom arrow
        px(6, 10, 1, 1).draw(d)?;
        px(7, 11, 1, 1).draw(d)?;
        px(8, 12, 1, 1).draw(d)?;
        px(7, 13, 1, 1).draw(d)?;
        px(6, 14, 1, 1).draw(d)?;
        Ok(())
    }

    /// Draw the iOS-style BLE toggle pill (above WiFi).
    fn draw_ble_toggle<D: DrawTarget<Color = Rgb565>>(
        d: &mut D,
        on: bool,
    ) -> Result<(), D::Error> {
        let x = BLE_TOGGLE_X;
        let y = BLE_TOGGLE_Y;
        let w = BLE_TOGGLE_W;
        let h = BLE_TOGGLE_H;
        let r = h / 2;
        let kr = 10i32;

        let track_color = if on { Rgb565::new(0, 16, 31) } else { Rgb565::new(6, 12, 6) }; // blue when on
        RoundedRectangle::with_equal_corners(
            Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32)),
            Size::new(r as u32, r as u32),
        ).into_styled(PrimitiveStyle::with_fill(track_color)).draw(d)?;

        let knob_cx = if on { x + w - r } else { x + r };
        let knob_cy = y + h / 2;
        Circle::new(
            Point::new(knob_cx - kr, knob_cy - kr),
            (kr * 2) as u32,
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE)).draw(d)?;

        let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);
        Text::new("BLE", Point::new(x + 4, y - 4), dim).draw(d)?;

        Ok(())
    }

    /// Draw the iOS-style WiFi toggle pill.
    fn draw_wifi_toggle<D: DrawTarget<Color = Rgb565>>(
        d: &mut D,
        connected: bool,
    ) -> Result<(), D::Error> {
        let x = WIFI_TOGGLE_X;
        let y = WIFI_TOGGLE_Y;
        let w = WIFI_TOGGLE_W;
        let h = WIFI_TOGGLE_H;
        let r = h / 2;

        // Track (pill shape = rounded rectangle with half-height corners)
        let track_color = if connected { Rgb565::GREEN } else { Rgb565::new(6, 12, 6) };
        RoundedRectangle::with_equal_corners(
            Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32)),
            Size::new(r as u32, r as u32),
        ).into_styled(PrimitiveStyle::with_fill(track_color)).draw(d)?;

        // Knob (white circle, left when off, right when on)
        let knob_cx = if connected {
            x + w - r
        } else {
            x + r
        };
        let knob_cy = y + h / 2;
        Circle::new(
            Point::new(knob_cx - WIFI_KNOB_R, knob_cy - WIFI_KNOB_R),
            (WIFI_KNOB_R * 2) as u32,
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE)).draw(d)?;

        // Label
        let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);
        Text::new("WiFi", Point::new(x + 2, y - 4), dim).draw(d)?;

        Ok(())
    }

    /// Draw the CPU frequency button with "CPU" label underneath.
    fn draw_cpu_button<D: DrawTarget<Color = Rgb565>>(
        d: &mut D,
        mhz: u16,
    ) -> Result<(), D::Error> {
        // Rounded pill button
        let color = match mhz {
            80 => Rgb565::new(0, 12, 4),   // greenish = eco
            240 => Rgb565::new(15, 6, 0),  // orange = performance
            _ => Rgb565::new(4, 8, 12),    // blue = balanced
        };
        RoundedRectangle::with_equal_corners(
            Rectangle::new(
                Point::new(CPU_BTN_X, CPU_BTN_Y),
                Size::new(CPU_BTN_W as u32, CPU_BTN_H as u32),
            ),
            Size::new(8, 8),
        ).into_styled(PrimitiveStyle::with_fill(color)).draw(d)?;

        // Text: "80M" / "160M" / "240M"
        let mut buf = [0u8; 5];
        let s = fmt_mhz_short(&mut buf, mhz);
        let ts = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
        Text::with_alignment(
            s,
            Point::new(CPU_BTN_X + CPU_BTN_W / 2, CPU_BTN_Y + 20),
            ts,
            Alignment::Center,
        ).draw(d)?;

        // "CPU" label below button
        let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);
        Text::with_alignment(
            "CPU",
            Point::new(CPU_BTN_X + CPU_BTN_W / 2, CPU_BTN_Y + CPU_BTN_H + 16),
            dim,
            Alignment::Center,
        ).draw(d)?;
        Ok(())
    }

    /// Draw the Apps launcher button (bottom center).
    fn draw_apps_button<D: DrawTarget<Color = Rgb565>>(d: &mut D) -> Result<(), D::Error> {
        RoundedRectangle::with_equal_corners(
            Rectangle::new(
                Point::new(APPS_BTN_X, APPS_BTN_Y),
                Size::new(APPS_BTN_W as u32, APPS_BTN_H as u32),
            ),
            Size::new(12, 12),
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::new(4, 8, 14))).draw(d)?;

        let ts = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
        Text::with_alignment(
            "APPS",
            Point::new(APPS_BTN_X + APPS_BTN_W / 2, APPS_BTN_Y + 24),
            ts,
            Alignment::Center,
        ).draw(d)?;
        Ok(())
    }

    /// Hit-test for the BLE toggle switch.
    pub fn is_ble_zone(x: u16, y: u16) -> bool {
        let xi = x as i32;
        let yi = y as i32;
        xi >= BLE_TOGGLE_X - 10 && xi <= BLE_TOGGLE_X + BLE_TOGGLE_W + 10
            && yi >= BLE_TOGGLE_Y - 10 && yi <= BLE_TOGGLE_Y + BLE_TOGGLE_H + 10
    }

    /// Hit-test for the CPU frequency button.
    pub fn is_cpu_zone(x: u16, y: u16) -> bool {
        let xi = x as i32;
        let yi = y as i32;
        xi >= CPU_BTN_X - 8 && xi <= CPU_BTN_X + CPU_BTN_W + 8
            && yi >= CPU_BTN_Y - 8 && yi <= CPU_BTN_Y + CPU_BTN_H + 8
    }

    /// Hit-test for the Apps button.
    pub fn is_apps_zone(x: u16, y: u16) -> bool {
        let xi = x as i32;
        let yi = y as i32;
        xi >= APPS_BTN_X - 8 && xi <= APPS_BTN_X + APPS_BTN_W + 8
            && yi >= APPS_BTN_Y - 8 && yi <= APPS_BTN_Y + APPS_BTN_H + 8
    }

    /// Cycle CPU frequency: 80 → 160 → 240 → 80.
    pub fn cycle_cpu(&mut self) -> u16 {
        self.cpu_mhz = match self.cpu_mhz {
            80 => 160,
            160 => 240,
            _ => 80,
        };
        self.full_redraw = true;
        self.cpu_mhz
    }

    /// Draw the horizontal brightness slider.
    fn draw_brightness_slider<D: DrawTarget<Color = Rgb565>>(
        d: &mut D,
        brightness: u8,
    ) -> Result<(), D::Error> {
        let x = BRI_SLIDER_X;
        let y = BRI_SLIDER_Y;
        let w = BRI_SLIDER_W;
        let h = BRI_SLIDER_H;

        // Label
        let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);
        Text::new("Bri", Point::new(x - 2, y - 4), dim).draw(d)?;

        // Track background (dark gray pill)
        RoundedRectangle::with_equal_corners(
            Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32)),
            Size::new((h / 2) as u32, (h / 2) as u32),
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::new(3, 6, 3))).draw(d)?;

        // Filled portion (proportional to brightness)
        let fill_w = ((brightness as i32 - 0x10).max(0) * w / (0xFF - 0x10)) as u32;
        if fill_w > 0 {
            let fill_color = if brightness > 180 {
                Rgb565::YELLOW
            } else {
                Rgb565::new(16, 32, 16) // soft green
            };
            RoundedRectangle::with_equal_corners(
                Rectangle::new(Point::new(x, y), Size::new(fill_w.min(w as u32), h as u32)),
                Size::new((h / 2) as u32, (h / 2) as u32),
            ).into_styled(PrimitiveStyle::with_fill(fill_color)).draw(d)?;
        }

        // Thumb knob
        let knob_x = x + fill_w as i32;
        let knob_cy = y + h / 2;
        let kr = h / 2 + 2;
        Circle::new(
            Point::new(knob_x - kr, knob_cy - kr),
            (kr * 2) as u32,
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE)).draw(d)?;

        Ok(())
    }

    pub fn needs_render(&self) -> bool {
        self.full_redraw || self.time_changed || self.battery_changed || self.gyro_changed
    }

    /// Always-On-Display renderer.
    /// Strategy:
    ///   * Pure black background → on AMOLED these pixels are physically OFF (zero current).
    ///   * Only HH:MM is drawn (no seconds), in dim white using the same 7-segment font.
    ///   * Tiny battery percentage in the corner.
    ///   * Vertical position is shifted by `(minutes % 8) - 4` pixels to avoid pixel
    ///     burn-in over months of always-on use, mimicking what Apple Watch does.
    pub fn render_aod<D: DrawTarget<Color = Rgb565>>(&mut self, d: &mut D) -> Result<(), D::Error> {
        let w = board::LCD_WIDTH as i32;
        let h = board::LCD_HEIGHT as i32;

        // Full clear to black — this is the cheapest possible AMOLED state.
        Rectangle::new(Point::zero(), Size::new(w as u32, h as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d)?;

        // Anti burn-in: shift the time block by a few pixels based on the current minute.
        let shift_x = ((self.minutes as i32) % 9) - 4;
        let shift_y = ((self.minutes as i32 / 9) % 9) - 4;

        let cx = SCREEN_CX + shift_x;
        let cy = h / 2 - 32 + shift_y;

        // HH:MM only (no seconds, no extra widgets).
        // We use a slightly dimmed white (CSS_LIGHT_GRAY = ~0.8 brightness) to further reduce power
        // because each AMOLED sub-pixel scales current with luminance.
        let dim_white = Rgb565::new(20, 40, 20); // ~50% gray, looks white-ish on AMOLED but uses ~half the current

        // Draw HH:MM using the segment renderer. Pass 99 for seconds to indicate "skip seconds".
        // The segments::draw_time function draws all 8 chars; we'll use a custom call.
        segments::draw_hhmm(d, cx, cy, self.hours, self.minutes, dim_white, Rgb565::BLACK)?;

        // Tiny battery indicator at the bottom (3 chars max: "99%")
        let mut buf = [0u8; 4];
        let s = fmt_bat_short(&mut buf, self.battery_percent);
        let style = MonoTextStyle::new(&FONT_10X20, Rgb565::new(8, 16, 8));
        Text::with_alignment(s, Point::new(cx, cy + 110), style, Alignment::Center).draw(d)?;

        // Reset dirty flags so the normal renderer does a full redraw on wake.
        self.full_redraw = true;
        self.time_changed = false;
        self.battery_changed = false;
        self.gyro_changed = false;
        Ok(())
    }

    pub fn render<D: DrawTarget<Color = Rgb565>>(&mut self, d: &mut D) -> Result<RenderOutcome, D::Error> {
        if !self.full_redraw && !self.time_changed && !self.battery_changed && !self.gyro_changed {
            return Ok(RenderOutcome::default());
        }

        let w = board::LCD_WIDTH as i32;
        let h = board::LCD_HEIGHT as i32;
        let cx = SCREEN_CX;

        let cyan = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
        let dim = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);

        if self.full_redraw {
            // Clear
            Rectangle::new(Point::zero(), Size::new(w as u32, h as u32))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
                .draw(d)?;

            // Status icons top-left (inside rounded area)
            // WiFi icon: 3 arcs + dot (pixel-drawn)
            if self.wifi_connected {
                Self::draw_wifi_icon(d, 72, 10, Rgb565::GREEN)?;
            }
            // BLE icon: rune-style "B" shape
            if self.ble_on {
                Self::draw_ble_icon(d, 96, 10, Rgb565::new(0, 16, 31))?;
            }

            // === BLE toggle (above WiFi) ===
            Self::draw_ble_toggle(d, self.ble_on)?;

            // === WiFi toggle switch (iOS-style pill) ===
            Self::draw_wifi_toggle(d, self.wifi_connected)?;

            // === CPU freq button (below WiFi toggle) ===
            Self::draw_cpu_button(d, self.cpu_mhz)?;

            // === Brightness slider (horizontal bar) ===
            Self::draw_brightness_slider(d, self.brightness)?;

            // Title
            Text::with_alignment("RUST WATCH", Point::new(cx, 38), cyan, Alignment::Center).draw(d)?;

            // Time (y=60, 64px tall, ends at y=124)
            segments::draw_time(d, cx, TIME_Y, self.hours, self.minutes, self.seconds,
                Rgb565::WHITE, Rgb565::BLACK)?;

            // Date FR under time
            let mut date_buf = [0u8; 12];
            let ds = fmt_date_fr(&mut date_buf, self.day, self.month, self.year);
            Text::with_alignment(ds, Point::new(cx, 150), dim, Alignment::Center).draw(d)?;

            // Battery bar + percentage (more space below date)
            self.draw_battery(d, cx, 175)?;

            // Gyro section (only when enabled)
            if self.gyro_enabled {
                Circle::new(Point::new(GYRO_CX - GYRO_R, GYRO_CY - GYRO_R), (GYRO_R * 2) as u32)
                    .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_DARK_GRAY, 2))
                    .draw(d)?;
                Text::with_alignment("GYRO", Point::new(GYRO_CX, GYRO_CY + GYRO_R + 20), dim, Alignment::Center).draw(d)?;
                self.draw_gyro_ball(d)?;
            } else {
                Text::with_alignment("TAP FOR GYRO", Point::new(GYRO_CX, GYRO_CY + GYRO_R + 20), dim, Alignment::Center).draw(d)?;
            }

            // Apps button (bottom center)
            Self::draw_apps_button(d)?;

            self.full_redraw = false;
            self.time_changed = false;
            self.battery_changed = false;
            self.gyro_changed = false;
            return Ok(RenderOutcome {
                full_redraw: true,
                ..RenderOutcome::default()
            });
        }

        let mut outcome = RenderOutcome::default();

        if self.time_changed {
            Rectangle::new(
                Point::new(Self::time_region().x as i32, Self::time_region().y as i32),
                Size::new(Self::time_region().w as u32, Self::time_region().h as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d)?;
            segments::draw_time(d, cx, TIME_Y, self.hours, self.minutes, self.seconds,
                Rgb565::WHITE, Rgb565::BLACK)?;
            self.time_changed = false;
            outcome.time_region = Some(Self::time_region());
        }

        if self.battery_changed {
            Rectangle::new(
                Point::new(Self::battery_region().x as i32, Self::battery_region().y as i32),
                Size::new(Self::battery_region().w as u32, Self::battery_region().h as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d)?;
            self.draw_battery(d, cx, BATTERY_Y)?;
            self.battery_changed = false;
            outcome.battery_region = Some(Self::battery_region());
        }

        if self.gyro_changed && self.gyro_enabled {
            outcome.gyro_region = self.draw_gyro_ball(d)?;
            self.gyro_changed = false;
        }

        Ok(outcome)
    }

    fn draw_gyro_ball<D: DrawTarget<Color = Rgb565>>(&mut self, d: &mut D) -> Result<Option<FlushRegion>, D::Error> {
        let (nx, ny) = Self::projected_ball_position(self.accel_x, self.accel_y);

        if (nx - self.prev_ball_x).unsigned_abs() < 2 && (ny - self.prev_ball_y).unsigned_abs() < 2 {
            return Ok(None);
        }

        // Erase old
        Rectangle::new(
            Point::new(self.prev_ball_x - BALL_R, self.prev_ball_y - BALL_R),
            Size::new(BALL_R as u32 * 2, BALL_R as u32 * 2),
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(d)?;

        // Draw new
        Rectangle::new(
            Point::new(nx - BALL_R, ny - BALL_R),
            Size::new(BALL_R as u32 * 2, BALL_R as u32 * 2),
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::GREEN)).draw(d)?;

        let dirty = Self::ball_region(self.prev_ball_x, self.prev_ball_y)
            .union(Self::ball_region(nx, ny));
        self.prev_ball_x = nx;
        self.prev_ball_y = ny;
        Ok(Some(dirty))
    }

    fn draw_battery<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D, cx: i32, y: i32) -> Result<(), D::Error> {
        let bw = 200i32; let bh = 20i32; let bx = cx - bw/2;

        RoundedRectangle::with_equal_corners(
            Rectangle::new(Point::new(bx, y), Size::new(bw as u32, bh as u32)),
            Size::new(4, 4),
        ).into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 2)).draw(d)?;

        let fw = ((self.battery_percent as i32).min(100) * (bw - 6)) / 100;
        let fc = if self.battery_percent > 50 { Rgb565::GREEN }
            else if self.battery_percent > 20 { Rgb565::YELLOW }
            else { Rgb565::RED };

        if fw > 0 {
            Rectangle::new(Point::new(bx+3, y+3), Size::new(fw as u32, (bh-6) as u32))
                .into_styled(PrimitiveStyle::with_fill(fc)).draw(d)?;
        }

        let mut buf = [0u8; 16];
        let s = fmt_batt(&mut buf, self.battery_percent, self.is_charging);
        let st = if self.is_charging {
            MonoTextStyle::new(&FONT_10X20, Rgb565::GREEN)
        } else {
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE)
        };
        Text::with_alignment(s, Point::new(cx, y + bh + 25), st, Alignment::Center).draw(d)?;
        Ok(())
    }

    pub fn time_region() -> FlushRegion {
        FlushRegion::new(
            SCREEN_CX - TIME_TOTAL_W / 2 - TIME_PAD,
            TIME_Y - TIME_PAD,
            TIME_TOTAL_W + TIME_PAD * 2,
            TIME_DH + TIME_PAD * 2,
        )
    }

    pub fn battery_region() -> FlushRegion {
        FlushRegion::new(
            SCREEN_CX - BATTERY_REGION_W / 2,
            BATTERY_Y - BATTERY_PAD_Y,
            BATTERY_REGION_W,
            BATTERY_REGION_H + BATTERY_PAD_Y * 2,
        )
    }

    fn ball_region(x: i32, y: i32) -> FlushRegion {
        FlushRegion::new(
            x - BALL_R - GYRO_FLUSH_PAD,
            y - BALL_R - GYRO_FLUSH_PAD,
            BALL_R * 2 + GYRO_FLUSH_PAD * 2,
            BALL_R * 2 + GYRO_FLUSH_PAD * 2,
        )
    }

    fn projected_ball_position(accel_x: i16, accel_y: i16) -> (i32, i32) {
        let max_off = GYRO_R - BALL_R - 4;
        let bx = (-(accel_y as i32) * max_off / 100).clamp(-max_off, max_off);
        let by = ((accel_x as i32) * max_off / 100).clamp(-max_off, max_off);
        (GYRO_CX + bx, GYRO_CY + by)
    }
}

fn fmt_mhz_short<'a>(buf: &'a mut [u8; 5], mhz: u16) -> &'a str {
    let mut p = 0;
    if mhz >= 100 {
        buf[p] = b'0' + (mhz / 100) as u8; p += 1;
    }
    buf[p] = b'0' + ((mhz / 10) % 10) as u8; p += 1;
    buf[p] = b'0' + (mhz % 10) as u8; p += 1;
    buf[p] = b'M'; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("?M")
}

fn fmt_date_fr<'a>(buf: &'a mut [u8; 12], d: u8, m: u8, y: u8) -> &'a str {
    // Format: "DD/MM/20YY"
    let mut p = 0;
    buf[p] = b'0' + d / 10; p += 1;
    buf[p] = b'0' + d % 10; p += 1;
    buf[p] = b'/'; p += 1;
    buf[p] = b'0' + m / 10; p += 1;
    buf[p] = b'0' + m % 10; p += 1;
    buf[p] = b'/'; p += 1;
    buf[p] = b'2'; p += 1;
    buf[p] = b'0'; p += 1;
    buf[p] = b'0' + y / 10; p += 1;
    buf[p] = b'0' + y % 10; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("??/??/????")
}

fn fmt_batt<'a>(buf: &'a mut [u8; 16], pct: u8, chg: bool) -> &'a str {
    let mut p = 0;
    if pct >= 100 { buf[p]=b'1'; p+=1; buf[p]=b'0'; p+=1; buf[p]=b'0'; p+=1; }
    else if pct >= 10 { buf[p]=b'0'+pct/10; p+=1; buf[p]=b'0'+pct%10; p+=1; }
    else { buf[p]=b'0'+pct; p+=1; }
    buf[p]=b'%'; p+=1;
    if chg { for &c in b" CHG" { buf[p]=c; p+=1; } }
    core::str::from_utf8(&buf[..p]).unwrap_or("?%")
}

fn fmt_bat_short<'a>(buf: &'a mut [u8; 4], pct: u8) -> &'a str {
    let mut p = 0;
    if pct >= 100 { buf[p]=b'1'; p+=1; buf[p]=b'0'; p+=1; buf[p]=b'0'; p+=1; }
    else if pct >= 10 { buf[p]=b'0'+pct/10; p+=1; buf[p]=b'0'+pct%10; p+=1; }
    else { buf[p]=b'0'+pct; p+=1; }
    buf[p]=b'%'; p+=1;
    core::str::from_utf8(&buf[..p]).unwrap_or("?%")
}

fn fmt_mv<'a>(buf: &'a mut [u8; 12], mv: u16) -> &'a str {
    let mut p = 0;
    if mv >= 1000 { buf[p]=b'0'+(mv/1000) as u8; p+=1; }
    buf[p]=b'0'+((mv/100)%10) as u8; p+=1;
    buf[p]=b'0'+((mv/10)%10) as u8; p+=1;
    buf[p]=b'0'+(mv%10) as u8; p+=1;
    for &c in b"mV" { buf[p]=c; p+=1; }
    core::str::from_utf8(&buf[..p]).unwrap_or("????mV")
}
