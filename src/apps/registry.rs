//! Single source of truth for every launchable app: its metadata (name / icon /
//! accent / section) and its dispatch routing (framebuffer vs overlay + flags).
//!
//! Before this module the same facts were spread across four places — the
//! `AppState` enum, `LAUNCHER_APPS` in `slint_shell.rs`, the hand-authored tiles
//! in `launcher.slint`, and the two dispatch match arms in `main.rs`. Adding an
//! app meant editing all of them with the metadata duplicated. Now: append one
//! [`AppDescriptor`] row here and wire its behavior in ONE dispatch spot.
//!
//! Deliberately a plain `static` const array, **not** a `linkme`-style
//! distributed slice: the C6's flip-link + custom `linkall.x` linker setup makes
//! link-section auto-registration fragile and non-deterministically ordered. A
//! const array is simpler, ordering-stable, zero-alloc, and still "add in one
//! place".
//!
//! Consumed incrementally across the plugin-system migration (P2 = framebuffer
//! dispatch, P4 = data-driven launcher, P5 = overlay table), so some accessors
//! have no caller yet.
#![allow(dead_code)]

use crate::apps::AppState;

/// Launcher grouping. The launcher's section headers and display order derive
/// from this; the launch *index* is the app's position in [`REGISTRY`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Audio,
    Games,
    System,
}

impl Section {
    /// Header label shown above the section in the launcher.
    pub const fn label(self) -> &'static str {
        match self {
            Section::Audio => "AUDIO",
            Section::Games => "GAMES",
            Section::System => "SYSTEM",
        }
    }
}

/// Which dispatch family an app belongs to — routes the main-loop match.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppKind {
    /// Paints straight to the [`Framebuffer`](crate::drivers::framebuffer::Framebuffer);
    /// launch suspends the Slint scene to free heap for the ~51KB fb.
    Framebuffer,
    /// Renders *through* the resident Slint scene by pushing properties — no
    /// framebuffer, no scene suspend. Peripheral service stays hand-written.
    Overlay,
}

/// Per-app behavior flags (a small bitset so future apps can combine them).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AppFlags(u8);

impl AppFlags {
    pub const NONE: AppFlags = AppFlags(0);
    /// Holds a WiFi association up while the app is open (climate/energy/voice).
    pub const WIFI: AppFlags = AppFlags(1 << 0);
    /// Drops the Slint scene on launch to free heap for the framebuffer.
    pub const SUSPEND: AppFlags = AppFlags(1 << 1);

    pub const fn or(self, other: AppFlags) -> AppFlags {
        AppFlags(self.0 | other.0)
    }
    pub const fn has(self, f: AppFlags) -> bool {
        self.0 & f.0 == f.0
    }
    /// Holds WiFi up for its lifetime.
    pub const fn holds_wifi(self) -> bool {
        self.has(AppFlags::WIFI)
    }
    /// Suspends the Slint scene on launch (framebuffer apps).
    pub const fn suspends_scene(self) -> bool {
        self.has(AppFlags::SUSPEND)
    }
}

/// One launchable app. `'static`, `Copy`, zero-alloc.
#[derive(Clone, Copy)]
pub struct AppDescriptor {
    /// Stable loop-mode discriminant + launcher launch payload.
    pub state: AppState,
    /// Launcher label.
    pub name: &'static str,
    /// Vector-glyph id — matches `AppIcon.id` in `ui/slint/launcher.slint`.
    pub icon_id: u16,
    /// Accent tint as `0xRRGGBB`.
    pub accent: u32,
    /// Launcher grouping.
    pub section: Section,
    /// Dispatch family.
    pub kind: AppKind,
    /// Behavior flags.
    pub flags: AppFlags,
}

use AppKind::{Framebuffer, Overlay};
use Section::{Audio, Games, System};

/// The single source of truth for launchable apps.
///
/// **Order == launcher launch index**: `REGISTRY[idx].state` is the app raised by
/// `launch_app(idx)` — this replaces the old `LAUNCHER_APPS: [AppState; 13]` array
/// (same order, same indices). Non-launchable loop modes (Watchface, Launcher,
/// Mp3Player, SmartHome) are intentionally absent — they are not tiles.
///
/// Accents/icons/sections mirror the current `launcher.slint` tiles exactly, so
/// this is metadata-preserving (Theme.warm = `#ffd166` for WLED).
pub static REGISTRY: &[AppDescriptor] = &[
    // --- GAMES (framebuffer) ------------------------------------------------
    AppDescriptor { state: AppState::Snake,      name: "Snake",       icon_id: 0,  accent: 0x35e0b0, section: Games,  kind: Framebuffer, flags: AppFlags::SUSPEND }, // idx 0
    AppDescriptor { state: AppState::WorldSnake, name: "World Snake", icon_id: 1,  accent: 0x00ff80, section: Games,  kind: Framebuffer, flags: AppFlags::SUSPEND }, // idx 1
    AppDescriptor { state: AppState::Game2048,   name: "2048",        icon_id: 2,  accent: 0xf0d000, section: Games,  kind: Framebuffer, flags: AppFlags::SUSPEND }, // idx 2
    AppDescriptor { state: AppState::Tetris,     name: "Tetris",      icon_id: 3,  accent: 0x00d0f0, section: Games,  kind: Framebuffer, flags: AppFlags::SUSPEND }, // idx 3
    AppDescriptor { state: AppState::Flappy,     name: "Flappy Bird", icon_id: 4,  accent: 0xffffff, section: Games,  kind: Framebuffer, flags: AppFlags::SUSPEND }, // idx 4
    AppDescriptor { state: AppState::Maze,       name: "Maze (Tilt)", icon_id: 5,  accent: 0x8090ff, section: Games,  kind: Framebuffer, flags: AppFlags::SUSPEND }, // idx 5
    // --- SYSTEM Settings hub (v0.9.0: scene-resident overlay, was fb) -------
    AppDescriptor { state: AppState::Settings,   name: "Settings",    icon_id: 6,  accent: 0xc0ffc0, section: System, kind: Overlay,     flags: AppFlags::NONE },    // idx 6
    // --- Overlays -----------------------------------------------------------
    AppDescriptor { state: AppState::Wled,       name: "WLED",        icon_id: 9,  accent: 0xffd166, section: System, kind: Overlay,     flags: AppFlags::NONE },    // idx 7
    AppDescriptor { state: AppState::Hunt,       name: "Hunt",        icon_id: 10, accent: 0xff7a3d, section: Games,  kind: Overlay,     flags: AppFlags::NONE },    // idx 8
    AppDescriptor { state: AppState::Energy,     name: "Energy",      icon_id: 11, accent: 0x35e0b0, section: System, kind: Overlay,     flags: AppFlags::WIFI },    // idx 9
    AppDescriptor { state: AppState::Climate,    name: "Climate",     icon_id: 12, accent: 0xff9d5c, section: System, kind: Overlay,     flags: AppFlags::WIFI },    // idx 10
    AppDescriptor { state: AppState::Voice,      name: "Voice",       icon_id: 7,  accent: 0xa78bfa, section: Audio,  kind: Overlay,     flags: AppFlags::WIFI },    // idx 11
    AppDescriptor { state: AppState::Sound,      name: "Sound",       icon_id: 8,  accent: 0x4fd6ff, section: Audio,  kind: Overlay,     flags: AppFlags::NONE },    // idx 12
    AppDescriptor { state: AppState::Theme,      name: "Theme",       icon_id: 13, accent: 0xa78bfa, section: System, kind: Overlay,     flags: AppFlags::NONE },    // idx 13
    AppDescriptor { state: AppState::Lights,     name: "Lights",      icon_id: 14, accent: 0xffb454, section: System, kind: Overlay,     flags: AppFlags::WIFI },    // idx 14
    AppDescriptor { state: AppState::Ping,       name: "Ping",        icon_id: 15, accent: 0xffd166, section: System, kind: Overlay,     flags: AppFlags::NONE },    // idx 15
    // APPENDED, not inserted: order == launch index, so adding anywhere else
    // would silently re-point every launcher tile after it.
    //
    // Gated with the feature so the shipped default build does not show a tile
    // that opens a screen with no client behind it. A dead launcher entry is
    // worse than an absent one — the whole point of shipping `story` off is that
    // JP's watch behaves exactly as it does today.
    #[cfg(feature = "story")]
    AppDescriptor { state: AppState::Story,      name: "Story",       icon_id: 16, accent: 0xa78bfa, section: Audio,  kind: Overlay,     flags: AppFlags::WIFI },    // idx 16
];

impl AppDescriptor {
    /// True when this board carries the hardware the app needs. The launcher
    /// filters on this; the row itself is NEVER deleted or cfg'd out — idx is
    /// a persistence contract (suspended-session records, persisted mappings,
    /// an OTA'd device holding stale state), and removing a row shifts every
    /// later app onto the wrong index — the switcher-map silent-wrong-index
    /// class. Same doctrine as the story tile's gate above: an absent tile
    /// beats a dead one, and a shifted one is worst of all.
    pub fn hardware_present(&self) -> bool {
        match self.state {
            AppState::Maze => cfg!(feature = "has-imu"), // tilt-driven
            AppState::Voice | AppState::Sound => cfg!(feature = "has-audio"),
            _ => true,
        }
    }
}

/// Look up a launchable app's descriptor by state (linear scan, ≤15 entries).
/// `None` for non-launchable states (Watchface / Launcher / Mp3Player / SmartHome).
pub fn descriptor(state: AppState) -> Option<&'static AppDescriptor> {
    REGISTRY.iter().find(|d| d.state == state)
}

/// The app raised by launcher index `idx` (replaces `LAUNCHER_APPS[idx]`).
pub fn launch_state(idx: usize) -> Option<AppState> {
    REGISTRY.get(idx).map(|d| d.state)
}

/// True when `state` is a framebuffer app (paints to the fb, suspends the scene).
pub fn is_framebuffer(state: AppState) -> bool {
    descriptor(state).is_some_and(|d| d.kind == AppKind::Framebuffer)
}

/// True when `state` is a Slint-overlay app (renders through the scene).
pub fn is_overlay(state: AppState) -> bool {
    descriptor(state).is_some_and(|d| d.kind == AppKind::Overlay)
}
