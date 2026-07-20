// App framework - common types and trait for all apps/games

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::DrawTarget;

use crate::peripherals::touch::{SwipeDirection, TouchPoint};

pub mod snake;
pub mod world_snake;
pub mod game2048;
pub mod tetris;
pub mod flappy;
pub mod maze;
pub mod settings;

/// Input state passed to apps each frame
pub struct AppInput {
    pub touch: Option<TouchPoint>,
    pub swipe: Option<SwipeDirection>,
    pub tap: bool,
    pub accel: (f32, f32, f32),
    pub dt_ms: u32, // milliseconds since last frame
}

/// Result of an app update
pub enum AppResult {
    Continue,
    Exit, // Return to launcher/watchface
}

/// Common trait for all apps/games
pub trait App {
    fn name(&self) -> &str;
    fn setup(&mut self);
    fn update(&mut self, input: &AppInput) -> AppResult;
    fn render<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D);
}

/// All available app states
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AppState {
    Watchface,
    Launcher,
    Snake,
    WorldSnake,
    Game2048,
    Tetris,
    Flappy,
    Maze,
    Mp3Player,
    SmartHome,
    Settings,
    /// WLED WiZmote remote — a Slint overlay (not a framebuffer app): renders
    /// through the resident scene, taps broadcast ESP-NOW WiZmote frames.
    Wled,
    /// RSSI treasure-hunt (warmer/colder) — a Slint overlay driven live from the
    /// mesh roster's smoothed RSSI. Also scene-resident (no framebuffer).
    Hunt,
}
