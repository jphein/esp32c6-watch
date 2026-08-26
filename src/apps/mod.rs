// App framework - common types and trait for all apps/games

use crate::drivers::framebuffer::Framebuffer;
use crate::peripherals::touch::{SwipeDirection, TouchPoint};

pub mod snake;
pub mod world_snake;
pub mod game2048;
pub mod tetris;
pub mod flappy;
pub mod maze;
pub mod registry;
pub mod session;
#[cfg(feature = "bard")]
pub mod bard;

/// Input state passed to apps each frame
pub struct AppInput {
    pub touch: Option<TouchPoint>,
    pub swipe: Option<SwipeDirection>,
    pub tap: bool,
    /// True while a finger is physically on the glass THIS frame (live
    /// touch-controller sample). Distinct from `touch`, which some apps receive
    /// as last-known coords even after lift (Settings) or as a synthesized hold
    /// (Flappy). Lets apps draw pressed-state feedback on finger-DOWN instead of
    /// only reacting to the tap that fires on lift (touch overhaul).
    pub down: bool,
    pub accel: (f32, f32, f32),
    pub dt_ms: u32, // milliseconds since last frame
}

/// Result of an app update
pub enum AppResult {
    Continue,
    Exit, // Return to launcher/watchface
}

/// A one-shot sound effect an app can queue during `update`, drained by the
/// generic framebuffer runner and played on the shared I2S path.
pub enum Sfx {
    /// Short blip (e.g. Snake eating food).
    Beep,
}

/// Common trait for all apps/games.
///
/// `render` is monomorphized to the one concrete [`Framebuffer`] (rather than a
/// generic `D: DrawTarget`) so the trait is **object-safe** — that is what lets
/// the main loop dispatch every framebuffer app through a single `&mut dyn App`
/// runner (`run_fb_app`) instead of a per-game match arm.
pub trait App {
    fn name(&self) -> &str;
    fn setup(&mut self);
    fn update(&mut self, input: &AppInput) -> AppResult;
    fn render(&self, fb: &mut Framebuffer);

    /// Whether the last `update` produced a frame worth flushing. Default: always
    /// (cadence-driven apps). Event-driven apps override to gate on their own
    /// change signal (Snake on a step, 2048 on a swipe).
    fn dirty(&self) -> bool {
        true
    }

    /// Minimum milliseconds between flushes (cadence throttle). `0` = flush
    /// whenever `dirty` (event-driven); `33` ≈ 30fps for continuous animation.
    fn min_flush_ms(&self) -> u32 {
        0
    }

    /// Drain a one-shot sound effect the last `update` queued. Default: none.
    fn take_sfx(&mut self) -> Option<Sfx> {
        None
    }
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
    /// Home energy screen (house battery/solar/grid) — a display-only Slint
    /// overlay. Placeholder data until the HA/ESP-NOW energy feed lands.
    Energy,
    /// HA climate control (#58) — a Slint overlay holding an open MQTT session
    /// (WiFi held while the screen is up, released on close) to view + command
    /// Nest / minisplit setpoints & modes via the Node-RED bridge.
    Climate,
    /// Voice-to-text (push-to-talk, #42) — a Slint overlay: hold streams mic PCM
    /// over HTTP to the LAN STT bridge, release shows the transcript. Scene-
    /// resident (no framebuffer); capture runs in the shared `mic_capture_task`,
    /// streamed by `voice_stt::stream_utterance` while the button is held.
    Voice,
    /// Sound-level meter (#28) — a Slint overlay (SoundLevel) showing live dBFS +
    /// peak-hold. Subscribes to the SAME shared `mic_capture_task`/MIC_CH as Voice
    /// (METER gate), draining chunks through `mic_dsp::rms_dbfs`. No WiFi (local).
    Sound,
    /// Theme picker — a Slint overlay (ThemeOverlay): a 2×2 swatch grid to pick
    /// the color scheme (Midnight/Paper/Amber/Violet). Scene-resident (no fb, no
    /// WiFi); the tap sets `Theme.scheme` for instant preview and emits
    /// `theme-changed` for flash persistence. Right-swipe closes.
    Theme,
    /// Room lights (#39) — a Slint overlay: one press lights the room you're in.
    /// Publishes `toggle`/`on`/`off` to `watch/<sigil>/lights/cmd`; HA resolves
    /// the room (the watch stays floor-plan-dumb) and republishes the retained
    /// `watch/<sigil>/lights/state` (`AREA|<name>|<on>/<total>|<status>`).
    /// Rides the shared HA MQTT session like Climate/Energy (WiFi held while
    /// open, released on close).
    Lights,
    /// Endless LitRPG reader (#story) — a Slint overlay with four pages (chapter
    /// list, playback, stats, character). Streams raw 16 kHz PCM from
    /// `litrpg-daemon` via HTTP `Range` windows straight into `audio_out`, with no
    /// decoder and nothing buffered whole (a chapter is ~25 MB against 512 KB of
    /// SRAM). Holds WiFi while open; playback parks the main loop by design and
    /// pumps `service_amp` per chunk (read-aloud spec §6.2).
    Story,
    /// Watch-to-watch ping (#35) — a Slint overlay: a hero button broadcasts a
    /// SMOLv1 PING over ESP-NOW; the peer answers with a PINGACK ("delivered
    /// to <sigil>") and blooms a full-screen greeting pulse + two-tone chime.
    /// Mesh-dependent (no WiFi); rides the always-on ESP-NOW radio like WLED.
    Ping,
}
