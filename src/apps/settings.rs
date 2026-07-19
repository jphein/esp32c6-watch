// Settings app - WiFi config with T9 keyboard input

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};
use embedded_graphics::geometry::Point as EgPoint;

use crate::peripherals::wifi::{WifiConfig, WifiState};
use crate::ui::t9_keyboard::T9Keyboard;

#[derive(Clone, Copy, PartialEq)]
enum SettingsField {
    Ssid,
    Password,
    Connect,
}

pub struct SettingsApp {
    pub wifi_config: WifiConfig,
    pub wifi_state: WifiState,
    pub keyboard: T9Keyboard,
    active_field: SettingsField,
    editing: bool,
}

impl SettingsApp {
    pub fn new() -> Self {
        Self {
            wifi_config: WifiConfig::new(),
            wifi_state: WifiState::Disconnected,
            keyboard: T9Keyboard::new(),
            active_field: SettingsField::Ssid,
            editing: false,
        }
    }

    /// Handle tap at screen position. Returns true if consumed.
    pub fn handle_tap(&mut self, x: u16, y: u16) -> bool {
        // Check if keyboard is active and handles it
        if self.keyboard.is_active() {
            if self.keyboard.handle_tap(x, y) {
                // Sync text to active field
                match self.active_field {
                    SettingsField::Ssid => self.wifi_config.set_ssid(self.keyboard.get_text()),
                    SettingsField::Password => self.wifi_config.set_password(self.keyboard.get_text()),
                    _ => {}
                }
                return true;
            }
            // Tap outside keyboard = close it
            if y < 200 {
                self.keyboard.hide();
                self.editing = false;
                return true;
            }
        }

        // Field selection (match the render positions: SSID=60-110, Pass=120-170, Connect=185-225)
        if y >= 60 && y < 115 {
            // SSID field tapped
            self.active_field = SettingsField::Ssid;
            self.keyboard.clear_text();
            self.keyboard.show();
            self.editing = true;
            return true;
        }
        if y >= 120 && y < 175 {
            // Password field
            self.active_field = SettingsField::Password;
            self.keyboard.clear_text();
            self.keyboard.show();
            self.editing = true;
            return true;
        }
        if y >= 185 && y < 230 {
            // Connect button
            if self.wifi_state == WifiState::Disconnected || self.wifi_state == WifiState::Error {
                self.wifi_state = WifiState::Connecting;
            }
            return true;
        }
        false
    }

    pub fn update(&mut self, dt_ms: u32) {
        self.keyboard.update(dt_ms);
    }

    pub fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        let _ = Rectangle::new(EgPoint::zero(), Size::new(410, 502))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::new(1, 2, 2)))
            .draw(d);

        let title = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
        let label = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_GRAY);
        let value = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);

        let _ = Text::with_alignment("SETTINGS", EgPoint::new(205, 35), title, Alignment::Center).draw(d);

        // SSID field
        let ssid_bg = if self.active_field == SettingsField::Ssid && self.editing { Rgb565::new(3, 6, 3) } else { Rgb565::new(2, 4, 2) };
        let _ = RoundedRectangle::with_equal_corners(
            Rectangle::new(EgPoint::new(15, 60), Size::new(380, 50)),
            Size::new(8, 8),
        ).into_styled(PrimitiveStyle::with_fill(ssid_bg)).draw(d);
        let _ = Text::new("WiFi SSID:", EgPoint::new(25, 78), label).draw(d);
        let ssid = self.wifi_config.ssid_str();
        let ssid_display = if ssid.is_empty() { "(tap to enter)" } else { ssid };
        let _ = Text::new(ssid_display, EgPoint::new(25, 98), value).draw(d);

        // Password field
        let pass_bg = if self.active_field == SettingsField::Password && self.editing { Rgb565::new(3, 6, 3) } else { Rgb565::new(2, 4, 2) };
        let _ = RoundedRectangle::with_equal_corners(
            Rectangle::new(EgPoint::new(15, 120), Size::new(380, 50)),
            Size::new(8, 8),
        ).into_styled(PrimitiveStyle::with_fill(pass_bg)).draw(d);
        let _ = Text::new("Password:", EgPoint::new(25, 138), label).draw(d);
        let pass_len = self.wifi_config.pass_len;
        let _ = Text::new(if pass_len > 0 { "********" } else { "(tap to enter)" }, EgPoint::new(25, 158), value).draw(d);

        // Connect button
        let btn_color = match self.wifi_state {
            WifiState::Disconnected => Rgb565::BLUE,
            WifiState::Connecting => Rgb565::YELLOW,
            WifiState::Connected => Rgb565::GREEN,
            WifiState::Error => Rgb565::RED,
        };
        let _ = RoundedRectangle::with_equal_corners(
            Rectangle::new(EgPoint::new(100, 185), Size::new(210, 40)),
            Size::new(10, 10),
        ).into_styled(PrimitiveStyle::with_fill(btn_color)).draw(d);
        let btn_text = match self.wifi_state {
            WifiState::Disconnected => "CONNECT",
            WifiState::Connecting => "CONNECTING...",
            WifiState::Connected => "CONNECTED",
            WifiState::Error => "RETRY",
        };
        let _ = Text::with_alignment(btn_text, EgPoint::new(205, 210), MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE), Alignment::Center).draw(d);

        // Draw keyboard overlay if active
        self.keyboard.render(d);
    }
}
