// Settings app - WiFi config with T9 keyboard input

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::text::{Alignment, Text};
use embedded_graphics::geometry::Point as EgPoint;

use crate::apps::{App, AppInput, AppResult};
use crate::drivers::framebuffer::Framebuffer;
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
    /// Set true when the user taps "Update firmware". main.rs takes it (one-tick
    /// handshake, same as the Connect flow), gates on WiFi + a baked-in OTA_URL,
    /// runs the OTA download, and reboots on success.
    pub ota_requested: bool,
    /// One-line OTA status shown under the button (`&'static` so the error string
    /// from `ota_http::ota_update` drops straight in). "" hides the line.
    pub ota_status: &'static str,
}

impl SettingsApp {
    pub fn new() -> Self {
        Self {
            wifi_config: WifiConfig::new(),
            wifi_state: WifiState::Disconnected,
            keyboard: T9Keyboard::new(),
            active_field: SettingsField::Ssid,
            editing: false,
            ota_requested: false,
            ota_status: "",
        }
    }

    /// Take a pending OTA request (clears it). One-shot handshake for main.rs.
    pub fn take_ota_request(&mut self) -> bool {
        core::mem::take(&mut self.ota_requested)
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
        // Update-firmware button (250-295). Guarded on !keyboard so a stray tap
        // while typing SSID/pass can't kick off an OTA. main.rs does the WiFi gate.
        if y >= 250 && y < 295 && !self.keyboard.is_active() {
            self.ota_requested = true;
            self.ota_status = "Requested\u{2026}";
            return true;
        }
        false
    }

}

impl App for SettingsApp {
    fn name(&self) -> &str {
        "Settings"
    }

    // Launch does no per-entry reset (the old launcher setup was a no-op for
    // Settings — WiFi creds/state persist across opens).
    fn setup(&mut self) {}

    fn update(&mut self, input: &AppInput) -> AppResult {
        self.keyboard.update(input.dt_ms);
        // Tap targets use the last-known touch coords (the tap frame's point may
        // already be None on finger-lift); the runner passes them via input.touch.
        if input.tap {
            if let Some(tp) = input.touch {
                self.handle_tap(tp.x, tp.y);
            }
        }
        AppResult::Continue
    }

    // Fairly static screen; repaint on a 50ms cadence (matches the old arm's
    // `next_flush + 50ms` gate). `dirty` stays the default `true`.
    fn min_flush_ms(&self) -> u32 {
        50
    }

    fn render(&self, d: &mut Framebuffer) {
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

        // Update-firmware button (OTA). Tap requests an OTA; main.rs gates on WiFi.
        let _ = RoundedRectangle::with_equal_corners(
            Rectangle::new(EgPoint::new(60, 250), Size::new(290, 45)),
            Size::new(10, 10),
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::new(6, 12, 20))).draw(d);
        let _ = Text::with_alignment(
            "UPDATE FIRMWARE",
            EgPoint::new(205, 278),
            MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_LIGHT_BLUE),
            Alignment::Center,
        ).draw(d);
        // OTA status line (download progress / staged / error), hidden when empty.
        if !self.ota_status.is_empty() {
            let _ = Text::with_alignment(
                self.ota_status,
                EgPoint::new(205, 320),
                MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_ORANGE),
                Alignment::Center,
            ).draw(d);
        }

        // Draw keyboard overlay if active
        self.keyboard.render(d);
    }
}
