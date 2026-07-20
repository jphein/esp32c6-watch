// Watchface - renders to any DrawTarget (framebuffer or display)

use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, Line, PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Text};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

use crate::board;

/// Big clock digits: Logisoso 92px, numeric-only ("HH:MM" is 257x92 px).
const TIME_FONT: FontRenderer = FontRenderer::new::<fonts::u8g2_font_logisoso92_tn>();
/// Medium numeric font: seconds + date line (Logisoso 24px, has ':' and '/').
const MED_FONT: FontRenderer = FontRenderer::new::<fonts::u8g2_font_logisoso24_tn>();
/// Bold text font for battery percentage etc. (Helvetica Bold 18px, full charset).
const TEXT_FONT: FontRenderer = FontRenderer::new::<fonts::u8g2_font_helvB18_tf>();

/// Unwrap a u8g2 render error into the underlying display error.
/// We only render with `FontColor::Transparent` and ASCII the fonts contain,
/// so the only reachable variant is `DisplayError`.
fn font_err<E>(e: u8g2_fonts::Error<E>) -> E {
    match e {
        u8g2_fonts::Error::DisplayError(e) => e,
        _ => unreachable!(),
    }
}

const SCREEN_CX: i32 = board::LCD_WIDTH as i32 / 2;
const TIME_Y: i32 = 44; // top of the big digits
const TIME_H: i32 = 92; // logisoso92 glyph height
const SECONDS_H: i32 = 24; // logisoso24 glyph height
// Region covers the widest "HH:MM" (257px centered) plus the small seconds
// hanging off the right edge (10px gap + 27px digits).
const TIME_REGION_X: i32 = SCREEN_CX - 134;
const TIME_REGION_W: i32 = 134 + 134 + 10 + 27 + 5;
const TIME_PAD: i32 = 4;
const BATTERY_Y: i32 = 175;
const BATTERY_PAD_Y: i32 = 4;
const BATTERY_REGION_W: i32 = 240;
const BATTERY_REGION_H: i32 = 52;

// Mesh status indicator (top-right, mirrors the wifi/ble icons top-left)
const MESH_X: i32 = 300;
const MESH_Y: i32 = 10;
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
    pub mesh_region: Option<FlushRegion>,
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
    /// Number of SMOLv1 mesh peers currently heard. 0 = greyed-out indicator.
    pub mesh_peers: u8,
    /// Last peer count actually drawn, for dirty tracking.
    mesh_drawn: u8,
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
            mesh_peers: 0,
            mesh_drawn: 0,
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
        self.full_redraw
            || self.time_changed
            || self.battery_changed
            || self.gyro_changed
            || self.mesh_peers != self.mesh_drawn
    }

    /// Draw the big HH:MM (logisoso92) with small seconds (logisoso24)
    /// bottom-aligned to the right of the digits.
    fn draw_time_block<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) -> Result<(), D::Error> {
        let mut buf = [0u8; 5];
        let hhmm = fmt_hhmm(&mut buf, self.hours, self.minutes);
        let bb = TIME_FONT
            .render_aligned(
                hhmm,
                Point::new(SCREEN_CX, TIME_Y),
                VerticalPosition::Top,
                HorizontalAlignment::Center,
                FontColor::Transparent(Rgb565::WHITE),
                d,
            )
            .map_err(font_err)?;
        // Seconds tucked against the right edge of the big digits.
        let right = bb
            .map(|r| r.top_left.x + r.size.width as i32)
            .unwrap_or(SCREEN_CX + 129);
        let mut sbuf = [0u8; 2];
        let ss = fmt_ss(&mut sbuf, self.seconds);
        MED_FONT
            .render_aligned(
                ss,
                Point::new(right + 10, TIME_Y + TIME_H - SECONDS_H),
                VerticalPosition::Top,
                HorizontalAlignment::Left,
                FontColor::Transparent(Rgb565::CSS_GRAY),
                d,
            )
            .map_err(font_err)?;
        Ok(())
    }

    /// Small mesh network glyph (3 linked nodes) + peer count, top-right.
    /// Greyed out when no peers, green when the mesh is alive.
    fn draw_mesh_indicator<D: DrawTarget<Color = Rgb565>>(
        d: &mut D,
        peers: u8,
    ) -> Result<(), D::Error> {
        let color = if peers > 0 {
            Rgb565::GREEN
        } else {
            Rgb565::new(8, 16, 8) // flat mid-grey, RGB332-safe
        };
        let x = MESH_X;
        let y = MESH_Y;
        let node = |dx: i32, dy: i32| {
            Rectangle::new(Point::new(x + dx, y + dy), Size::new(4, 4))
                .into_styled(PrimitiveStyle::with_fill(color))
        };
        let link = |x0: i32, y0: i32, x1: i32, y1: i32| {
            Line::new(Point::new(x + x0, y + y0), Point::new(x + x1, y + y1))
                .into_styled(PrimitiveStyle::with_stroke(color, 1))
        };
        // Links between node centers first, nodes drawn on top.
        link(8, 2, 2, 13).draw(d)?;
        link(8, 2, 14, 13).draw(d)?;
        link(2, 13, 14, 13).draw(d)?;
        node(6, 0).draw(d)?;
        node(0, 11).draw(d)?;
        node(12, 11).draw(d)?;
        // Peer count
        let mut buf = [0u8; 3];
        let s = fmt_u8(&mut buf, peers);
        let st = MonoTextStyle::new(&FONT_10X20, color);
        Text::new(s, Point::new(x + 22, y + 14), st).draw(d)?;
        Ok(())
    }

    pub fn mesh_region() -> FlushRegion {
        FlushRegion::new(MESH_X - 4, MESH_Y - 8, 70, 30)
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
        let cy = h / 2 - TIME_H / 2 + shift_y;

        // HH:MM only (no seconds, no extra widgets).
        // We use a slightly dimmed white (~50% gray) to further reduce power
        // because each AMOLED sub-pixel scales current with luminance.
        let dim_white = Rgb565::new(20, 40, 20);

        let mut buf = [0u8; 5];
        let hhmm = fmt_hhmm(&mut buf, self.hours, self.minutes);
        TIME_FONT
            .render_aligned(
                hhmm,
                Point::new(cx, cy),
                VerticalPosition::Top,
                HorizontalAlignment::Center,
                FontColor::Transparent(dim_white),
                d,
            )
            .map_err(font_err)?;

        // Tiny battery indicator at the bottom (4 chars max: "100%")
        let mut bbuf = [0u8; 4];
        let s = fmt_bat_short(&mut bbuf, self.battery_percent);
        TEXT_FONT
            .render_aligned(
                s,
                Point::new(cx, cy + TIME_H + 18),
                VerticalPosition::Top,
                HorizontalAlignment::Center,
                FontColor::Transparent(Rgb565::new(8, 16, 8)),
                d,
            )
            .map_err(font_err)?;

        // Reset dirty flags so the normal renderer does a full redraw on wake.
        self.full_redraw = true;
        self.time_changed = false;
        self.battery_changed = false;
        self.gyro_changed = false;
        Ok(())
    }

    pub fn render<D: DrawTarget<Color = Rgb565>>(&mut self, d: &mut D) -> Result<RenderOutcome, D::Error> {
        if !self.needs_render() {
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
            // Mesh indicator (top-right): always drawn, greyed out when 0 peers
            Self::draw_mesh_indicator(d, self.mesh_peers)?;
            self.mesh_drawn = self.mesh_peers;

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

            // Time: big HH:MM (y=44..136) + small seconds bottom-right
            self.draw_time_block(d)?;

            // Date under time (logisoso24, digits + '/')
            let mut date_buf = [0u8; 12];
            let ds = fmt_date_fr(&mut date_buf, self.day, self.month, self.year);
            MED_FONT
                .render_aligned(
                    ds,
                    Point::new(cx, 144),
                    VerticalPosition::Top,
                    HorizontalAlignment::Center,
                    FontColor::Transparent(Rgb565::CSS_GRAY),
                    d,
                )
                .map_err(font_err)?;

            // Battery bar + percentage (more space below date)
            self.draw_battery(d, cx, BATTERY_Y)?;

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
            self.draw_time_block(d)?;
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

        if self.mesh_peers != self.mesh_drawn {
            Rectangle::new(
                Point::new(Self::mesh_region().x as i32, Self::mesh_region().y as i32),
                Size::new(Self::mesh_region().w as u32, Self::mesh_region().h as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d)?;
            Self::draw_mesh_indicator(d, self.mesh_peers)?;
            self.mesh_drawn = self.mesh_peers;
            outcome.mesh_region = Some(Self::mesh_region());
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
        let color = if self.is_charging { Rgb565::GREEN } else { Rgb565::WHITE };
        TEXT_FONT
            .render_aligned(
                s,
                Point::new(cx, y + bh + 8),
                VerticalPosition::Top,
                HorizontalAlignment::Center,
                FontColor::Transparent(color),
                d,
            )
            .map_err(font_err)?;
        Ok(())
    }

    pub fn time_region() -> FlushRegion {
        FlushRegion::new(
            TIME_REGION_X - TIME_PAD,
            TIME_Y - TIME_PAD,
            TIME_REGION_W + TIME_PAD * 2,
            TIME_H + TIME_PAD * 2,
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

fn fmt_hhmm<'a>(buf: &'a mut [u8; 5], h: u8, m: u8) -> &'a str {
    buf[0] = b'0' + h / 10;
    buf[1] = b'0' + h % 10;
    buf[2] = b':';
    buf[3] = b'0' + m / 10;
    buf[4] = b'0' + m % 10;
    core::str::from_utf8(buf).unwrap_or("??:??")
}

fn fmt_ss<'a>(buf: &'a mut [u8; 2], s: u8) -> &'a str {
    buf[0] = b'0' + s / 10;
    buf[1] = b'0' + s % 10;
    core::str::from_utf8(buf).unwrap_or("??")
}

fn fmt_u8<'a>(buf: &'a mut [u8; 3], v: u8) -> &'a str {
    let mut p = 0;
    if v >= 100 { buf[p] = b'0' + v / 100; p += 1; }
    if v >= 10 { buf[p] = b'0' + (v / 10) % 10; p += 1; }
    buf[p] = b'0' + v % 10; p += 1;
    core::str::from_utf8(&buf[..p]).unwrap_or("?")
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

